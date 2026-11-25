//! Single-instance lock. The first process owns a Unix socket; a second
//! launch connects, sends `show` and exits, so there is never a second tray
//! icon and launching again brings the window back.
//!
//! Windows / macOS: no lock yet (every process is "primary").

use std::path::PathBuf;

/// Outcome of trying to become the one running instance.
pub enum Role {
    Primary(Primary),
    /// Another instance is running; it has been asked to show its window.
    Secondary,
}

/// Held by the running instance; `serve` starts answering second launches.
pub struct Primary {
    #[cfg(unix)]
    listener: Option<std::os::unix::net::UnixListener>,
}

/// The socket this process owns (removed by [`release`]).
static OWNED_SOCKET: std::sync::Mutex<Option<PathBuf>> = std::sync::Mutex::new(None);

/// Give up the lock before starting a replacement process (restart after
/// an update): the socket file is removed, so the new process binds a fresh
/// one instead of finding us and exiting as a "second launch".
pub fn release() {
    let path = OWNED_SOCKET.lock().ok().and_then(|mut g| g.take());
    if let Some(p) = path {
        let _ = std::fs::remove_file(p);
    }
}

fn remember_socket(path: &std::path::Path) {
    if let Ok(mut g) = OWNED_SOCKET.lock() {
        *g = Some(path.to_path_buf());
    }
}

/// Wakes the app when a second launch asked for the window.
static SHOW_REQUESTED: tokio::sync::Notify = tokio::sync::Notify::const_new();

/// A stream with one item per "show the window" request from a second
/// launch (requests made before the first poll are not lost).
pub fn show_requests() -> impl futures::Stream<Item = ()> + Send + 'static {
    futures::stream::unfold((), |()| async {
        SHOW_REQUESTED.notified().await;
        Some(((), ()))
    })
}

/// Socket location: `$XDG_RUNTIME_DIR/<name>.sock` (a private 0700
/// directory), else a per-user name in the temp directory.
pub fn socket_path(name: &str, runtime_dir: Option<std::ffi::OsString>, user: &str) -> PathBuf {
    match runtime_dir
        .map(PathBuf::from)
        .filter(|p| p.is_absolute() && p.is_dir())
    {
        Some(dir) => dir.join(format!("{name}.sock")),
        None => std::env::temp_dir().join(format!("{name}-{user}.sock")),
    }
}

#[cfg(unix)]
mod imp {
    use std::io::{BufRead, BufReader, Read, Write};
    use std::os::unix::fs::PermissionsExt;
    use std::os::unix::net::{UnixListener, UnixStream};
    use std::path::Path;
    use std::time::Duration;

    use super::{Primary, Role, SHOW_REQUESTED};

    const IO_TIMEOUT: Duration = Duration::from_secs(1);

    /// Tries to become the primary instance on `path`.
    pub fn acquire_at(path: &Path, ask_to_show: bool) -> std::io::Result<Role> {
        match UnixListener::bind(path) {
            Ok(l) => return primary(path, l),
            Err(e) if e.kind() != std::io::ErrorKind::AddrInUse => return Err(e),
            Err(_) => {}
        }
        match UnixStream::connect(path) {
            Ok(mut s) => {
                let _ = s.set_write_timeout(Some(IO_TIMEOUT));
                if ask_to_show {
                    let _ = s.write_all(b"show\n");
                }
                Ok(Role::Secondary)
            }
            Err(_) => {
                // Nobody answers: a stale socket from a crashed instance.
                let _ = std::fs::remove_file(path);
                primary(path, UnixListener::bind(path)?)
            }
        }
    }

    fn primary(path: &Path, listener: UnixListener) -> std::io::Result<Role> {
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
        super::remember_socket(path);
        Ok(Role::Primary(Primary {
            listener: Some(listener),
        }))
    }

    impl Primary {
        /// Answers second launches on a background thread; `on_show` runs
        /// once per `show` request.
        pub fn serve(self, on_show: impl Fn() + Send + 'static) {
            let Some(listener) = self.listener else {
                return;
            };
            let spawned = std::thread::Builder::new()
                .name("single-instance".into())
                .spawn(move || {
                    for conn in listener.incoming() {
                        let Ok(conn) = conn else { continue };
                        let _ = conn.set_read_timeout(Some(IO_TIMEOUT));
                        let mut line = String::new();
                        // A request is one short line; cap what we read.
                        let n = BufReader::new(conn.take(16)).read_line(&mut line);
                        if n.is_ok() && line.trim() == "show" {
                            on_show();
                        }
                    }
                });
            if let Err(e) = spawned {
                tracing::warn!(error = %e, "single-instance listener not started");
            }
        }
    }

    pub fn notify_show() {
        SHOW_REQUESTED.notify_one();
    }
}

#[cfg(not(unix))]
mod imp {
    use super::{Primary, Role, SHOW_REQUESTED};

    impl Primary {
        pub fn serve(self, _on_show: impl Fn() + Send + 'static) {}
    }

    pub fn notify_show() {
        SHOW_REQUESTED.notify_one();
    }
}

#[cfg(unix)]
pub use imp::acquire_at;

/// Becomes the primary instance, or asks the running one to show its window
/// (`ask_to_show`; false for `--minimized` launches). Failures to use the
/// socket are logged and treated as "primary": better two instances than
/// none.
pub fn acquire(name: &str, ask_to_show: bool) -> Role {
    #[cfg(unix)]
    {
        let user = std::env::var("USER").unwrap_or_else(|_| "user".into());
        let path = socket_path(name, std::env::var_os("XDG_RUNTIME_DIR"), &user);
        match acquire_at(&path, ask_to_show) {
            Ok(role) => role,
            Err(e) => {
                tracing::warn!(error = %e, "single-instance lock unavailable");
                Role::Primary(Primary { listener: None })
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = (name, ask_to_show);
        Role::Primary(Primary {})
    }
}

/// Called by the accept thread of the running instance.
pub fn request_show_locally() {
    imp::notify_show();
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::time::Duration;

    /// `acquire_at` records the owned socket in a global: serialize.
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn socket_path_prefers_the_runtime_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let p = socket_path("dev.ferro.x", Some(tmp.path().into()), "bob");
        assert_eq!(p, tmp.path().join("dev.ferro.x.sock"));
        let p = socket_path("dev.ferro.x", Some("/definitely/not/here".into()), "bob");
        assert_eq!(p, std::env::temp_dir().join("dev.ferro.x-bob.sock"));
        let p = socket_path("dev.ferro.x", None, "bob");
        assert_eq!(p, std::env::temp_dir().join("dev.ferro.x-bob.sock"));
    }

    #[test]
    fn second_launch_asks_the_first_to_show() {
        let _g = LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("a.sock");
        let Role::Primary(first) = acquire_at(&path, true).unwrap() else {
            panic!("first launch must be primary");
        };
        let (tx, rx) = mpsc::channel();
        first.serve(move || {
            let _ = tx.send(());
        });
        assert!(matches!(acquire_at(&path, true).unwrap(), Role::Secondary));
        rx.recv_timeout(Duration::from_secs(5)).unwrap();
        // `--minimized` launches do not ask for the window.
        assert!(matches!(acquire_at(&path, false).unwrap(), Role::Secondary));
        assert!(rx.recv_timeout(Duration::from_millis(300)).is_err());
    }

    #[test]
    fn released_lock_lets_a_new_process_become_primary() {
        let _g = LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("r.sock");
        let Role::Primary(first) = acquire_at(&path, true).unwrap() else {
            panic!("first launch must be primary");
        };
        first.serve(|| {});
        release();
        assert!(!path.exists());
        // The old listener still runs, but the restarted process owns the lock.
        assert!(matches!(acquire_at(&path, true).unwrap(), Role::Primary(_)));
    }

    #[test]
    fn stale_socket_is_replaced() {
        let _g = LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("b.sock");
        // A crashed instance leaves the socket file behind.
        drop(std::os::unix::net::UnixListener::bind(&path).unwrap());
        assert!(path.exists());
        assert!(matches!(acquire_at(&path, true).unwrap(), Role::Primary(_)));
    }
}
