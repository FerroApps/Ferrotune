//! "Restart now": run the new binary with the same arguments (plus
//! `--minimized` when the window is hidden), after the single-instance lock
//! was released, then quit through the normal quit path.

use std::path::Path;

/// Arguments for the new process: the current ones without any
/// `--minimized`, plus `--minimized` when hidden to the tray.
pub fn restart_args(current: &[String], hidden: bool) -> Vec<String> {
    let mut args: Vec<String> = current
        .iter()
        .filter(|a| a.as_str() != "--minimized")
        .cloned()
        .collect();
    if hidden {
        args.push("--minimized".into());
    }
    args
}

/// Spawn `bin` detached (its own process group, no inherited stdin).
pub fn spawn(bin: &Path, args: &[String]) -> std::io::Result<()> {
    let mut cmd = std::process::Command::new(bin);
    cmd.args(args).stdin(std::process::Stdio::null());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    cmd.spawn().map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn args_follow_the_window_state() {
        let a = |v: &[&str]| v.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>();
        assert_eq!(restart_args(&a(&[]), false), a(&[]));
        assert_eq!(restart_args(&a(&[]), true), a(&["--minimized"]));
        assert_eq!(restart_args(&a(&["--minimized"]), false), a(&[]));
        assert_eq!(
            restart_args(&a(&["--minimized", "--x"]), true),
            a(&["--x", "--minimized"])
        );
    }
}
