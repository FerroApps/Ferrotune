//! Self-update of an `install.sh` install: the same files, paths and
//! steps as `install.sh`, done from inside the app.
//!
//! 1. Download the tarball and its `.sha256` into a temp dir inside the bin
//!    dir (same file system, so the final step is a rename).
//! 2. Verify the SHA-256; abort on mismatch (nothing touched yet).
//! 3. Extract with [`super::extract`] (no links, no `..`, capped).
//! 4. Stage every file next to its target (`.<name>.new.<pid>`, binaries
//!    0755), then rename each over its target. The old file is kept as a
//!    hard link (`.<name>.old.<pid>`) until all renames succeeded; on a
//!    failure every replaced file is put back. Renaming over a running ELF
//!    is fine on Linux: the running process keeps the old inode.
//! 5. Desktop entry (`Exec=` = absolute binary path, field codes kept),
//!    icon and `installed-version`; the caller then runs
//!    [`refresh_desktop_caches`].

use std::path::{Path, PathBuf};

use super::download::{self, MAX_DOWNLOAD};
use super::extract::{self, Caps};
use super::method::Layout;
use super::release::{Release, select_assets};
use super::{BINARIES, CRATE, Endpoints, UpdateError};
use crate::config::APP_ID;

/// Where an update is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    Downloading { received: u64, total: Option<u64> },
    Verifying,
    Installing,
}

/// A finished install.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Installed {
    pub version: String,
    /// The new main binary (what "Restart now" runs).
    pub binary: PathBuf,
}

/// A temp directory removed on drop (also when the download is cancelled:
/// aborting the task drops the future and with it this guard).
struct TempDir(PathBuf);

impl TempDir {
    fn new_in(dir: &Path) -> Result<Self, UpdateError> {
        let path = dir.join(format!(".{CRATE}-update.{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700))?;
        }
        Ok(Self(path))
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Download, verify and install `release` into `layout`.
pub async fn run(
    ends: &Endpoints,
    release: &Release,
    layout: &Layout,
    progress: &(dyn Fn(Stage) + Send + Sync),
) -> Result<Installed, UpdateError> {
    let assets = select_assets(release, CRATE, std::env::consts::OS, std::env::consts::ARCH)
        .ok_or_else(|| {
            UpdateError::Unsupported("This release has no build for this system.".into())
        })?;
    if assets.archive.size > MAX_DOWNLOAD {
        return Err(UpdateError::TooLarge);
    }
    std::fs::create_dir_all(&layout.bin_dir)?;
    let tmp = TempDir::new_in(&layout.bin_dir)?;
    let http = download::client(ends)?;
    let sum = download::fetch_small(&http, &assets.checksum.url, ends.allow_loopback).await?;
    let expected = download::parse_checksum(&String::from_utf8_lossy(&sum), &assets.archive.name)
        .ok_or_else(|| UpdateError::Release("no checksum for the tarball".into()))?;
    let archive = tmp.0.join("download.tar.gz");
    let digest = download::fetch_to_file(
        &http,
        &assets.archive.url,
        ends.allow_loopback,
        &archive,
        MAX_DOWNLOAD,
        &|received, total| progress(Stage::Downloading { received, total }),
    )
    .await?;
    progress(Stage::Verifying);
    if digest != expected {
        tracing::warn!("update: checksum mismatch, nothing installed");
        return Err(UpdateError::Checksum);
    }
    progress(Stage::Installing);
    let (owned, version) = (layout.clone(), release.version.clone());
    let installed = tokio::task::spawn_blocking(move || {
        let r = install_archive(&archive, &tmp.0, &owned, &version);
        drop(tmp);
        r
    })
    .await
    .map_err(|_| UpdateError::Cancelled)??;
    tracing::info!(version = %installed.version, "update: installed");
    Ok(installed)
}

/// Files in the tarball, relative to its top directory.
fn wanted() -> Vec<String> {
    let mut w: Vec<String> = BINARIES.iter().map(|b| (*b).to_owned()).collect();
    w.push(format!("packaging/{APP_ID}.desktop"));
    w.push(format!("packaging/{APP_ID}.svg"));
    w
}

/// Steps 3-5 (blocking): extract the verified `archive` (inside `tmp`)
/// and swap the files in.
pub fn install_archive(
    archive: &Path,
    tmp: &Path,
    layout: &Layout,
    version: &str,
) -> Result<Installed, UpdateError> {
    let x = tmp.join("x");
    std::fs::create_dir_all(&x)?;
    let wanted = wanted();
    let wanted_refs: Vec<&str> = wanted.iter().map(String::as_str).collect();
    let file = std::io::BufReader::new(std::fs::File::open(archive)?);
    let files = extract::extract(file, &x, &wanted_refs, Caps::default())?;
    let get = |k: &str| {
        files
            .get(k)
            .cloned()
            .ok_or_else(|| UpdateError::Archive(format!("missing {k}")))
    };

    let pid = std::process::id();
    let staged_name = |target: &Path| -> Result<PathBuf, UpdateError> {
        let dir = target
            .parent()
            .ok_or_else(|| UpdateError::Io("bad target path".into()))?;
        let name = target
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        std::fs::create_dir_all(dir)?;
        Ok(dir.join(format!(".{name}.new.{pid}")))
    };
    let mut plan: Vec<(PathBuf, PathBuf)> = Vec::new();
    let result = (|| -> Result<(), UpdateError> {
        for bin in BINARIES {
            let target = layout.binary(bin);
            let staged = staged_name(&target)?;
            // Same file system as the temp dir: a rename, no copy.
            let src = get(bin)?;
            if std::fs::rename(&src, &staged).is_err() {
                std::fs::copy(&src, &staged)?;
            }
            plan.push((staged.clone(), target));
            set_executable(&staged)?;
        }
        let shipped = std::fs::read_to_string(get(&format!("packaging/{APP_ID}.desktop"))?)?;
        let entry = rewrite_desktop_entry(&shipped, &layout.binary(CRATE));
        let target = layout.desktop_entry();
        let staged = staged_name(&target)?;
        plan.push((staged.clone(), target));
        std::fs::write(&staged, entry)?;

        let target = layout.icon();
        let staged = staged_name(&target)?;
        plan.push((staged.clone(), target));
        std::fs::copy(get(&format!("packaging/{APP_ID}.svg"))?, &staged)?;

        let target = layout.version_file();
        let staged = staged_name(&target)?;
        plan.push((staged.clone(), target));
        std::fs::write(&staged, format!("{version}\n"))?;
        Ok(())
    })();
    if let Err(e) = result {
        for (staged, _) in &plan {
            let _ = std::fs::remove_file(staged);
        }
        return Err(e);
    }
    swap_all(&plan)?;
    Ok(Installed {
        version: version.to_owned(),
        binary: layout.binary(CRATE),
    })
}

fn set_executable(p: &Path) -> Result<(), UpdateError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o755))?;
    }
    let _ = p;
    Ok(())
}

fn backup_name(target: &Path) -> PathBuf {
    let name = target
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    target.with_file_name(format!(".{name}.old.{}", std::process::id()))
}

/// Rename every staged file over its target, all or nothing.
pub fn swap_all(plan: &[(PathBuf, PathBuf)]) -> Result<(), UpdateError> {
    let mut done: Vec<(&Path, Option<PathBuf>)> = Vec::new();
    for (i, (staged, target)) in plan.iter().enumerate() {
        let backup = if target.is_file() {
            let b = backup_name(target);
            let _ = std::fs::remove_file(&b);
            match std::fs::hard_link(target, &b).or_else(|_| std::fs::copy(target, &b).map(|_| ()))
            {
                Ok(()) => Some(b),
                Err(e) => {
                    rollback(&done);
                    cleanup_staged(&plan[i..]);
                    return Err(e.into());
                }
            }
        } else {
            None
        };
        if let Err(e) = std::fs::rename(staged, target) {
            tracing::warn!(error = %e, "update: rename failed, rolling back");
            if let Some(b) = &backup {
                let _ = std::fs::remove_file(b);
            }
            rollback(&done);
            cleanup_staged(&plan[i..]);
            return Err(e.into());
        }
        done.push((target, backup));
    }
    for (_, b) in done {
        if let Some(b) = b {
            let _ = std::fs::remove_file(b);
        }
    }
    Ok(())
}

fn rollback(done: &[(&Path, Option<PathBuf>)]) {
    for (target, backup) in done.iter().rev() {
        match backup {
            Some(b) => {
                if let Err(e) = std::fs::rename(b, target) {
                    tracing::error!(error = %e, "update: could not restore a file");
                }
            }
            None => {
                let _ = std::fs::remove_file(target);
            }
        }
    }
}

fn cleanup_staged(rest: &[(PathBuf, PathBuf)]) {
    for (staged, _) in rest {
        let _ = std::fs::remove_file(staged);
    }
}

/// `install.sh`'s desktop entry: every `Exec=` line becomes the absolute
/// binary path (quoted when it has a space) followed by the arguments of
/// the shipped first `Exec=` line.
pub fn rewrite_desktop_entry(shipped: &str, bin: &Path) -> String {
    let args = shipped
        .lines()
        .find_map(|l| l.strip_prefix("Exec="))
        .map(|rest| match rest.find(' ') {
            Some(i) => &rest[i..],
            None => "",
        })
        .unwrap_or("");
    let bin = bin.to_string_lossy();
    let exec_bin = if bin.contains(' ') {
        format!("\"{bin}\"")
    } else {
        bin.into_owned()
    };
    let mut out = String::with_capacity(shipped.len() + 64);
    for l in shipped.lines() {
        if l.starts_with("Exec=") {
            out.push_str("Exec=");
            out.push_str(&exec_bin);
            out.push_str(args);
        } else {
            out.push_str(l);
        }
        out.push('\n');
    }
    out
}

/// `install.sh`'s `refresh_caches` for `layout`: best effort, on its own
/// thread (never blocks the app or its quit).
pub fn refresh_desktop_caches(layout: &Layout) {
    let apps = layout.data_dir.join("applications");
    let icons = layout.data_dir.join("icons/hicolor");
    let spawned = std::thread::Builder::new()
        .name("update-caches".into())
        .spawn(move || {
            let run = |cmd: &str, args: &[&std::ffi::OsStr]| {
                std::process::Command::new(cmd)
                    .args(args)
                    .stdin(std::process::Stdio::null())
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .status()
                    .is_ok()
            };
            run("update-desktop-database", &[apps.as_os_str()]);
            run(
                "gtk-update-icon-cache",
                &["-f".as_ref(), "-t".as_ref(), icons.as_os_str()],
            );
            if !run("kbuildsycoca6", &[]) {
                run("kbuildsycoca5", &[]);
            }
        });
    if let Err(e) = spawned {
        tracing::debug!(error = %e, "update: cache refresh not started");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn desktop_entry_matches_install_sh() {
        let shipped = crate::desktop::DESKTOP_ENTRY;
        let out = rewrite_desktop_entry(shipped, Path::new("/home/u/.local/bin/ferrotune"));
        assert!(out.contains("\nExec=/home/u/.local/bin/ferrotune\n"));
        assert!(out.contains("Icon=dev.ferro.ferrotune\n"));
        let out = rewrite_desktop_entry(
            "[Desktop Entry]\nExec=ferrotune %U\nX=1\nExec=ferrotune --other\n",
            Path::new("/home/my user/bin/ferrotune"),
        );
        assert_eq!(
            out,
            "[Desktop Entry]\nExec=\"/home/my user/bin/ferrotune\" %U\nX=1\nExec=\"/home/my user/bin/ferrotune\" %U\n"
        );
    }

    #[test]
    fn swap_rolls_back_when_a_rename_fails() {
        let tmp = tempfile::tempdir().unwrap();
        let d = tmp.path();
        let (a, b) = (d.join("a"), d.join("b"));
        std::fs::write(&a, "old-a").unwrap();
        // `b` is a non-empty directory: renaming a file over it fails.
        std::fs::create_dir_all(b.join("x")).unwrap();
        let (sa, sb) = (d.join(".a.new"), d.join(".b.new"));
        std::fs::write(&sa, "new-a").unwrap();
        std::fs::write(&sb, "new-b").unwrap();
        assert!(swap_all(&[(sa.clone(), a.clone()), (sb.clone(), b.clone())]).is_err());
        assert_eq!(std::fs::read_to_string(&a).unwrap(), "old-a", "restored");
        assert!(!sa.exists() && !sb.exists(), "staged files cleaned up");
        let leftovers: Vec<_> = std::fs::read_dir(d)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.contains(".old."))
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
    }

    #[test]
    fn swap_replaces_and_creates() {
        let tmp = tempfile::tempdir().unwrap();
        let d = tmp.path();
        std::fs::write(d.join("a"), "old").unwrap();
        std::fs::write(d.join(".a.new"), "new").unwrap();
        std::fs::write(d.join(".c.new"), "c").unwrap();
        swap_all(&[
            (d.join(".a.new"), d.join("a")),
            (d.join(".c.new"), d.join("c")),
        ])
        .unwrap();
        assert_eq!(std::fs::read_to_string(d.join("a")).unwrap(), "new");
        assert_eq!(std::fs::read_to_string(d.join("c")).unwrap(), "c");
        assert_eq!(std::fs::read_dir(d).unwrap().count(), 2);
    }
}
