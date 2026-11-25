//! How this copy of Ferrotune was installed, which decides what "update"
//! can do. [`detect`] is pure (paths and probes are passed in);
//! [`Probe::from_process`] gathers the real values.

use std::path::{Path, PathBuf};

use super::CRATE;

/// Why Ferrotune only points at the release page.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Unmanaged {
    /// `/usr/bin` and the like, or a distro package.
    SystemPath,
    Flatpak,
    AppImage,
    /// Running from a cargo `target/` directory.
    Development,
    /// Looks like `install.sh`, but the folder is not writable.
    ReadOnly,
    /// No release build for this OS/architecture.
    Platform,
    /// The executable path could not be read.
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InstallMethod {
    /// Installed by `install.sh`: self-update supported.
    InstallScript(Layout),
    /// `cargo install` (`~/.cargo/bin` or `$CARGO_HOME/bin`).
    Cargo,
    Unmanaged(Unmanaged),
}

/// The paths `install.sh` uses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Layout {
    /// `${XDG_BIN_HOME:-$HOME/.local/bin}`.
    pub bin_dir: PathBuf,
    /// `${XDG_DATA_HOME:-$HOME/.local/share}`.
    pub data_dir: PathBuf,
}

impl Layout {
    pub fn from_env(home: &Path, xdg_bin: Option<&Path>, xdg_data: Option<&Path>) -> Self {
        Self {
            bin_dir: xdg_bin.map_or_else(|| home.join(".local/bin"), Path::to_path_buf),
            data_dir: xdg_data.map_or_else(|| home.join(".local/share"), Path::to_path_buf),
        }
    }

    pub fn binary(&self, name: &str) -> PathBuf {
        self.bin_dir.join(name)
    }

    pub fn state_dir(&self) -> PathBuf {
        self.data_dir.join(CRATE)
    }

    pub fn version_file(&self) -> PathBuf {
        self.state_dir().join("installed-version")
    }

    pub fn desktop_entry(&self) -> PathBuf {
        self.data_dir
            .join("applications")
            .join(format!("{}.desktop", crate::config::APP_ID))
    }

    pub fn icon(&self) -> PathBuf {
        self.data_dir
            .join("icons/hicolor/scalable/apps")
            .join(format!("{}.svg", crate::config::APP_ID))
    }
}

impl InstallMethod {
    pub fn can_self_update(&self) -> bool {
        matches!(self, Self::InstallScript(_))
    }

    /// Shown in Settings → Updates.
    pub fn label(&self) -> &'static str {
        match self {
            Self::InstallScript(_) => "install.sh (updates itself)",
            Self::Cargo => "cargo install",
            Self::Unmanaged(Unmanaged::SystemPath) => "System package or system folder",
            Self::Unmanaged(Unmanaged::Flatpak) => "Flatpak",
            Self::Unmanaged(Unmanaged::AppImage) => "AppImage",
            Self::Unmanaged(Unmanaged::Development) => "Development build",
            Self::Unmanaged(Unmanaged::ReadOnly) => "install.sh (folder not writable)",
            Self::Unmanaged(Unmanaged::Platform) => "Not available for this system",
            Self::Unmanaged(Unmanaged::Unknown) => "Unknown",
        }
    }
}

/// Everything detection looks at.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Probe {
    /// `current_exe()`, canonicalized (`None` when unreadable).
    pub exe: Option<PathBuf>,
    pub home: Option<PathBuf>,
    pub xdg_bin_home: Option<PathBuf>,
    pub xdg_data_home: Option<PathBuf>,
    pub cargo_home: Option<PathBuf>,
    /// `FLATPAK_ID` set or `/.flatpak-info` present.
    pub flatpak: bool,
    /// `APPIMAGE` / `APPDIR` set.
    pub appimage: bool,
    /// A release exists for this OS/architecture.
    pub platform_supported: bool,
}

fn canon(p: &Path) -> PathBuf {
    std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf())
}

fn abs_env(k: &str) -> Option<PathBuf> {
    std::env::var_os(k)
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
}

impl Probe {
    pub fn from_process() -> Self {
        Self {
            exe: std::env::current_exe().ok().map(|p| canon(&p)),
            home: directories::BaseDirs::new().map(|b| b.home_dir().to_path_buf()),
            xdg_bin_home: abs_env("XDG_BIN_HOME"),
            xdg_data_home: abs_env("XDG_DATA_HOME"),
            cargo_home: abs_env("CARGO_HOME"),
            flatpak: std::env::var_os("FLATPAK_ID").is_some()
                || Path::new("/.flatpak-info").exists(),
            appimage: std::env::var_os("APPIMAGE").is_some()
                || std::env::var_os("APPDIR").is_some(),
            platform_supported: super::release::archive_name(
                CRATE,
                "0.0.0",
                std::env::consts::OS,
                std::env::consts::ARCH,
            )
            .is_some(),
        }
    }
}

/// Detect the install method. `exists` and `writable` are the only file
/// system probes (injected for tests); expected paths are compared after
/// canonicalizing with `canon` (identity in tests).
pub fn detect(
    p: &Probe,
    exists: impl Fn(&Path) -> bool,
    writable: impl Fn(&Path) -> bool,
    canon: impl Fn(&Path) -> PathBuf,
) -> InstallMethod {
    let Some(exe) = p.exe.as_deref() else {
        return InstallMethod::Unmanaged(Unmanaged::Unknown);
    };
    if p.flatpak {
        return InstallMethod::Unmanaged(Unmanaged::Flatpak);
    }
    if p.appimage {
        return InstallMethod::Unmanaged(Unmanaged::AppImage);
    }
    // `.../target/debug/ferrotune`, `.../target/release/ferrotune`, ...
    let parts: Vec<_> = exe.components().collect();
    if parts.len() >= 3 && parts[parts.len() - 3].as_os_str() == "target" {
        return InstallMethod::Unmanaged(Unmanaged::Development);
    }
    if let Some(home) = p.home.as_deref() {
        let layout = Layout::from_env(home, p.xdg_bin_home.as_deref(), p.xdg_data_home.as_deref());
        if canon(&layout.binary(CRATE)) == exe && exists(&layout.version_file()) {
            if !p.platform_supported {
                return InstallMethod::Unmanaged(Unmanaged::Platform);
            }
            if !writable(&layout.bin_dir) {
                return InstallMethod::Unmanaged(Unmanaged::ReadOnly);
            }
            return InstallMethod::InstallScript(layout);
        }
        let cargo_bin = p
            .cargo_home
            .clone()
            .unwrap_or_else(|| home.join(".cargo"))
            .join("bin");
        if exe.parent() == Some(canon(&cargo_bin).as_path()) {
            return InstallMethod::Cargo;
        }
    }
    if !p.platform_supported {
        return InstallMethod::Unmanaged(Unmanaged::Platform);
    }
    InstallMethod::Unmanaged(Unmanaged::SystemPath)
}

/// Can we create files in `dir`? (A real probe: permissions alone miss
/// read-only mounts.)
pub fn dir_writable(dir: &Path) -> bool {
    let probe = dir.join(format!(".{CRATE}.write-test.{}", std::process::id()));
    match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&probe)
    {
        Ok(_) => {
            let _ = std::fs::remove_file(&probe);
            true
        }
        Err(_) => false,
    }
}

/// Detection with the real file system (blocking: run off the UI thread).
pub fn detect_current() -> InstallMethod {
    detect(&Probe::from_process(), Path::exists, dir_writable, canon)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    fn probe(exe: &str) -> Probe {
        Probe {
            exe: Some(exe.into()),
            home: Some("/home/u".into()),
            platform_supported: true,
            ..Probe::default()
        }
    }

    fn run(p: &Probe, files: &[&str], writable: bool) -> InstallMethod {
        let files: HashSet<PathBuf> = files.iter().map(PathBuf::from).collect();
        detect(p, |f| files.contains(f), |_| writable, Path::to_path_buf)
    }

    const MARKER: &str = "/home/u/.local/share/ferrotune/installed-version";

    #[test]
    fn install_sh_layout_is_self_updating() {
        let m = run(&probe("/home/u/.local/bin/ferrotune"), &[MARKER], true);
        assert_eq!(
            m,
            InstallMethod::InstallScript(Layout {
                bin_dir: "/home/u/.local/bin".into(),
                data_dir: "/home/u/.local/share".into(),
            })
        );
        assert!(m.can_self_update());
        // Without the marker it is not ours to replace.
        assert_eq!(
            run(&probe("/home/u/.local/bin/ferrotune"), &[], true),
            InstallMethod::Unmanaged(Unmanaged::SystemPath)
        );
        // Not writable: notify only.
        assert_eq!(
            run(&probe("/home/u/.local/bin/ferrotune"), &[MARKER], false),
            InstallMethod::Unmanaged(Unmanaged::ReadOnly)
        );
    }

    #[test]
    fn xdg_overrides_are_followed() {
        let mut p = probe("/opt/me/bin/ferrotune");
        p.xdg_bin_home = Some("/opt/me/bin".into());
        p.xdg_data_home = Some("/opt/me/data".into());
        let m = run(&p, &["/opt/me/data/ferrotune/installed-version"], true);
        let InstallMethod::InstallScript(l) = m else {
            panic!("expected install.sh, got {m:?}");
        };
        assert_eq!(
            l.version_file(),
            PathBuf::from("/opt/me/data/ferrotune/installed-version")
        );
        assert_eq!(
            l.desktop_entry(),
            PathBuf::from("/opt/me/data/applications/dev.ferro.ferrotune.desktop")
        );
        assert_eq!(
            l.icon(),
            PathBuf::from("/opt/me/data/icons/hicolor/scalable/apps/dev.ferro.ferrotune.svg")
        );
    }

    #[test]
    fn cargo_install_is_detected() {
        assert_eq!(
            run(&probe("/home/u/.cargo/bin/ferrotune"), &[], true),
            InstallMethod::Cargo
        );
        let mut p = probe("/data/cargo/bin/ferrotune");
        p.cargo_home = Some("/data/cargo".into());
        assert_eq!(run(&p, &[], true), InstallMethod::Cargo);
        assert!(!InstallMethod::Cargo.can_self_update());
    }

    #[test]
    fn everything_else_only_notifies() {
        let u = |m| InstallMethod::Unmanaged(m);
        assert_eq!(
            run(&probe("/usr/bin/ferrotune"), &[MARKER], true),
            u(Unmanaged::SystemPath)
        );
        assert_eq!(
            run(
                &probe("/home/u/src/Ferrotune/target/debug/ferrotune"),
                &[],
                true
            ),
            u(Unmanaged::Development)
        );
        assert_eq!(
            run(
                &probe("/home/u/src/Ferrotune/target/release/ferrotune"),
                &[],
                true
            ),
            u(Unmanaged::Development)
        );
        let mut p = probe("/home/u/.local/bin/ferrotune");
        p.flatpak = true;
        assert_eq!(run(&p, &[MARKER], true), u(Unmanaged::Flatpak));
        let mut p = probe("/tmp/.mount_x/usr/bin/ferrotune");
        p.appimage = true;
        assert_eq!(run(&p, &[], true), u(Unmanaged::AppImage));
        let mut p = probe("/home/u/.local/bin/ferrotune");
        p.platform_supported = false;
        assert_eq!(run(&p, &[MARKER], true), u(Unmanaged::Platform));
        let p = Probe::default();
        assert_eq!(run(&p, &[], true), u(Unmanaged::Unknown));
    }

    #[test]
    fn writable_probe_uses_the_file_system() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(dir_writable(tmp.path()));
        assert!(!dir_writable(&tmp.path().join("missing")));
        assert_eq!(
            std::fs::read_dir(tmp.path()).unwrap().count(),
            0,
            "probe removed"
        );
    }
}
