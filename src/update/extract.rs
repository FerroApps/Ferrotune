//! Safe extraction of the release tarball.
//!
//! The tarball (`release.yml`) holds one top directory
//! `ferrotune-v<ver>-linux-x86_64/` with the binary, `packaging/` (desktop
//! entry + icon), README, LICENSE and `licenses/`. Only the files
//! `install.sh` installs are written, flattened into `dest`. Any absolute
//! path, `..`, symlink, hardlink or special file aborts the whole
//! extraction; total unpacked bytes and each file are capped. The ustar
//! block reader is shared with the Soloist archive reader.

use std::collections::HashMap;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

use super::UpdateError;
use crate::net::soloist::archive::{
    ArchiveError, checksum_ok, header_name, octal, read_block, read_exact_or_corrupt,
    unpacked_error,
};

#[derive(Debug, Clone, Copy)]
pub struct Caps {
    pub max_unpacked: u64,
    pub max_file: u64,
}

impl Default for Caps {
    fn default() -> Self {
        Self {
            max_unpacked: 400 * 1024 * 1024,
            max_file: 300 * 1024 * 1024,
        }
    }
}

fn bad(m: impl Into<String>) -> UpdateError {
    UpdateError::Archive(m.into())
}

impl From<ArchiveError> for UpdateError {
    fn from(e: ArchiveError) -> Self {
        match e {
            ArchiveError::TooLarge => UpdateError::TooLarge,
            ArchiveError::Io(e) => UpdateError::Io(e.to_string()),
            other => UpdateError::Archive(other.to_string()),
        }
    }
}

/// Split an entry name into components after checking it is safe.
fn safe_parts(name: &str) -> Result<Vec<&str>, UpdateError> {
    if name.starts_with('/') || name.contains('\\') || name.contains('\0') {
        return Err(bad("absolute or odd path"));
    }
    let parts: Vec<&str> = name
        .split('/')
        .filter(|p| !p.is_empty() && *p != ".")
        .collect();
    if parts.contains(&"..") {
        return Err(bad("path with .."));
    }
    Ok(parts)
}

/// Extract `wanted` (paths relative to the top directory, e.g.
/// `ferrotune` or `packaging/x.desktop`) from the `.tar.gz` stream into
/// `dest`. Returns wanted path → written file; every wanted file must be
/// present.
pub fn extract(
    gz: impl Read,
    dest: &Path,
    wanted: &[&str],
    caps: Caps,
) -> Result<HashMap<String, PathBuf>, UpdateError> {
    let mut gz = gz;
    let mut magic = [0u8; 2];
    gz.read_exact(&mut magic)
        .map_err(|_| bad("not a gzip archive"))?;
    if magic != [0x1f, 0x8b] {
        return Err(bad("not a gzip archive"));
    }
    let chained = io::Cursor::new(magic).chain(gz);
    let mut r = flate2::read::GzDecoder::new(chained).take(caps.max_unpacked + 1);
    let mut header = [0u8; 512];
    let mut long_name: Option<String> = None;
    let mut top: Option<String> = None;
    let mut out: HashMap<String, PathBuf> = HashMap::new();
    loop {
        if !read_block(&mut r, &mut header)? || header.iter().all(|b| *b == 0) {
            break;
        }
        if !checksum_ok(&header) {
            return Err(bad("bad header checksum"));
        }
        let size = octal(&header[124..136]).ok_or_else(|| bad("bad size"))?;
        let padded = size.div_ceil(512) * 512;
        let kind = header[156];
        let skip = |r: &mut io::Take<_>| -> Result<(), UpdateError> {
            let n = io::copy(&mut r.take(padded), &mut io::sink())?;
            if n != padded {
                return Err(unpacked_error(r).into());
            }
            Ok(())
        };
        match kind {
            // GNU long name / pax headers: metadata for the next entry.
            b'L' => {
                if size > 4096 {
                    return Err(bad("long name too long"));
                }
                let mut buf = vec![0u8; padded as usize];
                read_exact_or_corrupt(&mut r, &mut buf)?;
                buf.truncate(size as usize);
                long_name = Some(
                    String::from_utf8_lossy(&buf)
                        .trim_end_matches('\0')
                        .to_owned(),
                );
                continue;
            }
            // A pax header can rename the next entry: refuse rather than
            // guess (GNU tar's default format, used by release.yml, has none).
            b'x' => return Err(bad("pax headers are not supported")),
            b'g' => {
                skip(&mut r)?;
                continue;
            }
            _ => {}
        }
        let name = long_name.take().unwrap_or_else(|| header_name(&header));
        let parts = safe_parts(&name)?;
        let Some(first) = parts.first() else {
            return Err(bad("empty name"));
        };
        match &top {
            None => top = Some((*first).to_owned()),
            Some(t) if t == first => {}
            Some(_) => return Err(bad("more than one top directory")),
        }
        let rel = parts[1..].join("/");
        match kind {
            b'5' => skip(&mut r)?,
            b'0' | b'\0' | b'7' => {
                if size > caps.max_file {
                    return Err(UpdateError::TooLarge);
                }
                if let Some(w) = wanted.iter().find(|w| **w == rel) {
                    if out.contains_key(*w) {
                        return Err(bad("duplicate entry"));
                    }
                    let file_name = rel.replace('/', "__");
                    let path = dest.join(file_name);
                    let mut f = std::fs::File::create(&path)?;
                    let copied = io::copy(&mut (&mut r).take(size), &mut f)?;
                    if copied != size {
                        return Err(unpacked_error(&r).into());
                    }
                    f.flush()?;
                    let rest = padded - size;
                    let n = io::copy(&mut (&mut r).take(rest), &mut io::sink())?;
                    if n != rest {
                        return Err(unpacked_error(&r).into());
                    }
                    out.insert((*w).to_owned(), path);
                } else {
                    skip(&mut r)?;
                }
            }
            b'1' => return Err(bad("hardlink in archive")),
            b'2' => return Err(bad("symlink in archive")),
            _ => return Err(bad("special file in archive")),
        }
        if r.limit() == 0 {
            return Err(UpdateError::TooLarge);
        }
    }
    if let Some(missing) = wanted.iter().find(|w| !out.contains_key(**w)) {
        return Err(bad(format!("missing {missing}")));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::net::soloist::archive::tests::{tar_entry, targz};

    const WANT: &[&str] = &["ferrotune", "packaging/a.desktop"];

    fn good() -> Vec<u8> {
        targz(&[
            tar_entry("ferrotune-v1.0.0-linux-x86_64/", b'5', b""),
            tar_entry("ferrotune-v1.0.0-linux-x86_64/README.md", b'0', b"readme"),
            tar_entry("ferrotune-v1.0.0-linux-x86_64/ferrotune", b'0', b"ELF-new"),
            tar_entry("ferrotune-v1.0.0-linux-x86_64/packaging/", b'5', b""),
            tar_entry(
                "ferrotune-v1.0.0-linux-x86_64/packaging/a.desktop",
                b'0',
                b"[Desktop Entry]\n",
            ),
        ])
    }

    fn run(gz: &[u8], caps: Caps) -> Result<HashMap<String, Vec<u8>>, UpdateError> {
        let tmp = tempfile::tempdir().unwrap();
        let m = extract(gz, tmp.path(), WANT, caps)?;
        Ok(m.into_iter()
            .map(|(k, p)| (k, std::fs::read(p).unwrap()))
            .collect())
    }

    #[test]
    fn extracts_the_wanted_files() {
        let m = run(&good(), Caps::default()).unwrap();
        assert_eq!(m["ferrotune"], b"ELF-new");
        assert_eq!(m["packaging/a.desktop"], b"[Desktop Entry]\n");
        assert_eq!(m.len(), 2);
    }

    fn rejects(entries: &[Vec<u8>]) -> UpdateError {
        run(&targz(entries), Caps::default()).unwrap_err()
    }

    #[test]
    fn unsafe_entries_abort() {
        let ok = tar_entry("top/ferrotune", b'0', b"x");
        assert!(matches!(
            rejects(&[tar_entry("/etc/passwd", b'0', b"x"), ok.clone()]),
            UpdateError::Archive(_)
        ));
        assert!(matches!(
            rejects(&[tar_entry("top/../../.bashrc", b'0', b"x")]),
            UpdateError::Archive(_)
        ));
        assert!(matches!(
            rejects(&[tar_entry("top/link", b'2', b""), ok.clone()]),
            UpdateError::Archive(m) if m.contains("symlink")
        ));
        assert!(matches!(
            rejects(&[tar_entry("top/hard", b'1', b""), ok.clone()]),
            UpdateError::Archive(m) if m.contains("hardlink")
        ));
        assert!(matches!(
            rejects(&[tar_entry("top/dev", b'3', b"")]),
            UpdateError::Archive(_)
        ));
        assert!(matches!(
            rejects(&[ok.clone(), tar_entry("other/x", b'0', b"x")]),
            UpdateError::Archive(m) if m.contains("top directory")
        ));
        // A symlink named like the binary is never followed or written.
        assert!(matches!(
            rejects(&[tar_entry("top/ferrotune", b'2', b"")]),
            UpdateError::Archive(_)
        ));
        // Missing desktop entry.
        assert!(matches!(
            rejects(&[ok]),
            UpdateError::Archive(m) if m.contains("missing")
        ));
    }

    #[test]
    fn sizes_are_capped() {
        let caps = Caps {
            max_unpacked: 8192,
            max_file: 1000,
        };
        let big = targz(&[tar_entry("top/ferrotune", b'0', &[1u8; 2000])]);
        assert_eq!(run(&big, caps).unwrap_err(), UpdateError::TooLarge);
        let bomb = targz(&[
            tar_entry("top/filler", b'0', &[0u8; 900]),
            tar_entry("top/filler2", b'0', &[0u8; 900]),
            tar_entry("top/filler3", b'0', &[0u8; 900]),
            tar_entry("top/filler4", b'0', &[0u8; 900]),
            tar_entry("top/filler5", b'0', &[0u8; 900]),
            tar_entry("top/filler6", b'0', &[0u8; 900]),
        ]);
        assert_eq!(run(&bomb, caps).unwrap_err(), UpdateError::TooLarge);
        assert!(run(b"PK\x03\x04", Caps::default()).is_err());
        let g = good();
        assert!(run(&g[..g.len() / 2], Caps::default()).is_err());
    }
}
