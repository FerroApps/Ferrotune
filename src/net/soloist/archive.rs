//! The official Soloist archive and `soloist --version` output.
//!
//! * [`extract_binary`] reads a `.tar.gz` and copies out only the regular
//!   file named `soloist` (top level or one directory deep), with size caps
//!   on the compressed stream, the total unpacked bytes and the binary.
//!   Links, devices and every other entry are skipped, never written.
//! * [`parse_version`] reads the version and the build timestamp from the
//!   `--version` output (the format is not documented, so the parser is
//!   lenient); builds expire [`BUILD_LIFETIME_DAYS`] days after that date.
//!
//! A ~100-line ustar reader instead of the `tar` crate: we only need one
//! file out of one archive, and every byte read is bounded here.

use std::io::{self, Read, Write};

/// "Spotify Soloist builds expire 90 days after their build date."
pub const BUILD_LIFETIME_DAYS: u64 = 90;
pub const DAY_SECS: u64 = 86_400;

/// Caps. Soloist 1.3.10 for x86_64: 13.8 MB archive, 36 MB binary, flat
/// layout (`CHANGELOG.md`, `THIRD_PARTY_LICENSES.txt`, `soloist`).
#[derive(Debug, Clone, Copy)]
pub struct Caps {
    /// Bytes unpacked from the gzip stream, all entries together.
    pub max_unpacked: u64,
    /// Size of the `soloist` executable.
    pub max_binary: u64,
}

impl Default for Caps {
    fn default() -> Self {
        Self {
            max_unpacked: 256 * 1024 * 1024,
            max_binary: 128 * 1024 * 1024,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ArchiveError {
    #[error("not a gzip archive")]
    NotGzip,
    #[error("the archive is corrupt: {0}")]
    Corrupt(&'static str),
    #[error("the archive is larger than allowed")]
    TooLarge,
    #[error("the archive has no `soloist` executable")]
    Missing,
    #[error("i/o: {0}")]
    Io(#[from] io::Error),
}

/// Copy the `soloist` binary out of `gz` (a `.tar.gz` stream) into `out`.
/// Returns its size.
pub fn extract_binary<R: Read, W: Write>(
    mut gz: R,
    out: &mut W,
    caps: Caps,
) -> Result<u64, ArchiveError> {
    let mut magic = [0u8; 2];
    gz.read_exact(&mut magic)
        .map_err(|_| ArchiveError::NotGzip)?;
    if magic != [0x1f, 0x8b] {
        return Err(ArchiveError::NotGzip);
    }
    let chained = io::Cursor::new(magic).chain(gz);
    // One more byte than allowed, so "exactly at the cap" and "over" differ.
    let mut tar = flate2::read::GzDecoder::new(chained).take(caps.max_unpacked + 1);
    let mut header = [0u8; 512];
    let mut long_name: Option<String> = None;
    loop {
        if !read_block(&mut tar, &mut header)? {
            return Err(ArchiveError::Missing);
        }
        if header.iter().all(|b| *b == 0) {
            // End-of-archive marker.
            return Err(ArchiveError::Missing);
        }
        if !checksum_ok(&header) {
            return Err(ArchiveError::Corrupt("bad header checksum"));
        }
        let size = octal(&header[124..136]).ok_or(ArchiveError::Corrupt("bad size"))?;
        let kind = header[156];
        let name = long_name.take().unwrap_or_else(|| header_name(&header));
        let padded = size.div_ceil(512) * 512;
        match kind {
            // GNU long name: the data is the next entry's name.
            b'L' => {
                if size > 4096 {
                    return Err(ArchiveError::Corrupt("long name too long"));
                }
                let mut buf = vec![0u8; padded as usize];
                read_exact_or_corrupt(&mut tar, &mut buf)?;
                buf.truncate(size as usize);
                let n = String::from_utf8_lossy(&buf);
                long_name = Some(n.trim_end_matches('\0').to_owned());
            }
            b'0' | b'\0' if is_soloist(&name) => {
                if size > caps.max_binary {
                    return Err(ArchiveError::TooLarge);
                }
                let copied = io::copy(&mut (&mut tar).take(size), out)?;
                if copied != size {
                    return Err(unpacked_error(&tar));
                }
                return Ok(size);
            }
            // Directories, pax headers, links, other files: skip the data.
            _ => {
                let skipped = io::copy(&mut (&mut tar).take(padded), &mut io::sink())?;
                if skipped != padded {
                    return Err(unpacked_error(&tar));
                }
            }
        }
        if tar.limit() == 0 {
            return Err(ArchiveError::TooLarge);
        }
    }
}

/// A short read is either the cap (the `Take` ran out) or truncation.
pub(crate) fn unpacked_error<R>(tar: &io::Take<R>) -> ArchiveError {
    if tar.limit() == 0 {
        ArchiveError::TooLarge
    } else {
        ArchiveError::Corrupt("truncated")
    }
}

pub(crate) fn read_exact_or_corrupt<R: Read>(
    r: &mut io::Take<R>,
    buf: &mut [u8],
) -> Result<(), ArchiveError> {
    match r.read_exact(buf) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => Err(unpacked_error(r)),
        Err(e) => Err(e.into()),
    }
}

/// Read one 512-byte block; `false` at a clean end of stream.
pub(crate) fn read_block<R: Read>(
    r: &mut io::Take<R>,
    buf: &mut [u8; 512],
) -> Result<bool, ArchiveError> {
    let mut filled = 0;
    while filled < buf.len() {
        match r.read(&mut buf[filled..]) {
            Ok(0) if filled == 0 => {
                return if r.limit() == 0 {
                    Err(ArchiveError::TooLarge)
                } else {
                    Ok(false)
                };
            }
            Ok(0) => return Err(unpacked_error(r)),
            Ok(n) => filled += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(ArchiveError::Io(e)),
        }
    }
    Ok(true)
}

pub(crate) fn header_name(h: &[u8; 512]) -> String {
    let field = |b: &[u8]| {
        let end = b.iter().position(|c| *c == 0).unwrap_or(b.len());
        String::from_utf8_lossy(&b[..end]).into_owned()
    };
    let name = field(&h[0..100]);
    // ustar: a non-empty prefix is the leading path.
    if &h[257..262] == b"ustar" {
        let prefix = field(&h[345..500]);
        if !prefix.is_empty() {
            return format!("{prefix}/{name}");
        }
    }
    name
}

/// `soloist`, `./soloist` or `<dir>/soloist` (one level), no `..`.
fn is_soloist(name: &str) -> bool {
    let parts: Vec<&str> = name
        .split('/')
        .filter(|p| !p.is_empty() && *p != ".")
        .collect();
    !parts.contains(&"..") && matches!(parts.as_slice(), ["soloist"] | [_, "soloist"])
}

pub(crate) fn octal(field: &[u8]) -> Option<u64> {
    // GNU base-256 for very large sizes: refuse (the caps are far lower).
    if field.first().is_some_and(|b| b & 0x80 != 0) {
        return None;
    }
    let s: String = field
        .iter()
        .take_while(|b| **b != 0)
        .map(|b| *b as char)
        .collect();
    let s = s.trim();
    if s.is_empty() {
        return Some(0);
    }
    u64::from_str_radix(s, 8).ok()
}

pub(crate) fn checksum_ok(h: &[u8; 512]) -> bool {
    let Some(want) = octal(&h[148..156]) else {
        return false;
    };
    let sum: u64 = h
        .iter()
        .enumerate()
        .map(|(i, b)| {
            if (148..156).contains(&i) {
                32
            } else {
                u64::from(*b)
            }
        })
        .sum();
    sum == want
}

/// What `soloist --version` told us.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BuildInfo {
    pub version: Option<String>,
    /// Build time, Unix seconds.
    pub built_unix: Option<u64>,
}

impl BuildInfo {
    /// Expiry (Unix seconds): build date + 90 days.
    pub fn expires_unix(&self) -> Option<u64> {
        self.built_unix
            .map(|b| b.saturating_add(BUILD_LIFETIME_DAYS * DAY_SECS))
    }
}

/// Parse `--version` output. Soloist 1.3.10 prints one line:
///
/// `soloist 1.3.10.87 build 1795111339 (20261119) (g19412df6ea) (linux/x86_64)`
///
/// The build date is the 8-digit `(YYYYMMDD)`. The layout is not
/// documented, so this stays lenient: the first dotted version number, and
/// for the date `(YYYYMMDD)`, else an ISO-8601 date, else a Unix timestamp
/// on a line that mentions "build" or "time".
pub fn parse_version(out: &str) -> BuildInfo {
    let mut info = BuildInfo::default();
    for line in out.lines().take(64) {
        let line = line.trim();
        if info.version.is_none() {
            info.version = find_version(line);
        }
        if info.built_unix.is_none() {
            info.built_unix = find_compact_date(line)
                .or_else(|| find_iso_datetime(line))
                .or_else(|| {
                    let l = line.to_ascii_lowercase();
                    (l.contains("build") || l.contains("time"))
                        .then(|| find_epoch(line))
                        .flatten()
                });
        }
    }
    info
}

fn find_version(line: &str) -> Option<String> {
    for raw in line.split(|c: char| c.is_whitespace() || c == ',' || c == '(' || c == ')') {
        let tok = raw.trim_start_matches(['v', 'V']);
        let core: String = tok
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '+' | '_'))
            .collect();
        let mut parts = core.split('.');
        let numeric = |p: Option<&str>| {
            p.is_some_and(|s| {
                !s.is_empty() && s.chars().take_while(char::is_ascii_digit).count() > 0
            })
        };
        // Looks like N.N…, and is not a date (2025-01-02) or a time (12:30).
        if numeric(parts.next())
            && numeric(parts.next())
            && core.chars().next().is_some_and(|c| c.is_ascii_digit())
            && !core.contains(':')
            && !looks_like_date(&core)
        {
            return Some(core.chars().take(48).collect());
        }
    }
    None
}

/// `(YYYYMMDD)`, midnight UTC.
fn find_compact_date(line: &str) -> Option<u64> {
    line.split('(').skip(1).find_map(|part| {
        let d = part.split(')').next()?.trim();
        if d.len() != 8 || !d.bytes().all(|c| c.is_ascii_digit()) {
            return None;
        }
        let y: i64 = d[0..4].parse().ok()?;
        let m: u32 = d[4..6].parse().ok()?;
        let day: u32 = d[6..8].parse().ok()?;
        if y < 2000 || !(1..=12).contains(&m) || !(1..=31).contains(&day) {
            return None;
        }
        Some(days_from_civil(y, m, day).max(0) as u64 * DAY_SECS)
    })
}

fn looks_like_date(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() >= 10 && b[4] == b'-' && b[7] == b'-'
}

/// `YYYY-MM-DD`, optionally followed by `[T ]HH:MM[:SS]` (UTC assumed).
fn find_iso_datetime(line: &str) -> Option<u64> {
    let b = line.as_bytes();
    if b.len() < 10 {
        return None;
    }
    for i in 0..=b.len() - 10 {
        let d = &b[i..i + 10];
        let digits = |r: std::ops::Range<usize>| d[r].iter().all(u8::is_ascii_digit);
        if !(digits(0..4) && d[4] == b'-' && digits(5..7) && d[7] == b'-' && digits(8..10)) {
            continue;
        }
        if i > 0 && b[i - 1].is_ascii_digit() {
            continue;
        }
        let num = |s: &[u8]| -> u64 { s.iter().fold(0, |a, c| a * 10 + u64::from(c - b'0')) };
        let (y, m, day) = (num(&d[0..4]), num(&d[5..7]), num(&d[8..10]));
        if !(1..=12).contains(&m) || !(1..=31).contains(&day) || y < 2000 {
            continue;
        }
        let mut secs = days_from_civil(y as i64, m as u32, day as u32).max(0) as u64 * DAY_SECS;
        let rest = &b[i + 10..];
        if rest.len() >= 6
            && (rest[0] == b'T' || rest[0] == b' ')
            && rest[1..3].iter().all(u8::is_ascii_digit)
            && rest[3] == b':'
            && rest[4..6].iter().all(u8::is_ascii_digit)
        {
            let h = num(&rest[1..3]).min(23);
            let mi = num(&rest[4..6]).min(59);
            secs += h * 3600 + mi * 60;
            if rest.len() >= 9 && rest[6] == b':' && rest[7..9].iter().all(u8::is_ascii_digit) {
                secs += num(&rest[7..9]).min(59);
            }
        }
        return Some(secs);
    }
    None
}

/// A 10-digit (seconds) or 13-digit (ms) Unix timestamp after 2020.
fn find_epoch(line: &str) -> Option<u64> {
    line.split(|c: char| !c.is_ascii_digit())
        .filter_map(|t| match t.len() {
            10 => t.parse::<u64>().ok(),
            13 => t.parse::<u64>().ok().map(|ms| ms / 1000),
            _ => None,
        })
        .find(|s| *s > 1_577_836_800)
}

/// Days since 1970-01-01 (Howard Hinnant's algorithm).
pub fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let m = i64::from(m);
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + i64::from(d) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// `YYYY-MM-DD` of a Unix timestamp (UTC).
pub fn format_date(unix: u64) -> String {
    let z = (unix / DAY_SECS) as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02}")
}

#[cfg(test)]
pub mod tests {
    use super::*;

    /// Build a ustar entry (header + padded data).
    pub fn tar_entry(name: &str, kind: u8, data: &[u8]) -> Vec<u8> {
        let mut h = [0u8; 512];
        h[..name.len()].copy_from_slice(name.as_bytes());
        h[100..108].copy_from_slice(b"0000755\0");
        h[124..136].copy_from_slice(format!("{:011o}\0", data.len()).as_bytes());
        h[136..148].copy_from_slice(b"00000000000\0");
        h[156] = kind;
        h[257..263].copy_from_slice(b"ustar\0");
        h[263..265].copy_from_slice(b"00");
        h[148..156].copy_from_slice(b"        ");
        let sum: u32 = h.iter().map(|b| u32::from(*b)).sum();
        h[148..156].copy_from_slice(format!("{sum:06o}\0 ").as_bytes());
        let mut v = h.to_vec();
        v.extend_from_slice(data);
        v.resize(v.len().div_ceil(512) * 512, 0);
        v
    }

    pub fn targz(entries: &[Vec<u8>]) -> Vec<u8> {
        let mut tar: Vec<u8> = entries.concat();
        tar.extend_from_slice(&[0u8; 1024]);
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        gz.write_all(&tar).unwrap();
        gz.finish().unwrap()
    }

    fn extract(gz: &[u8], caps: Caps) -> Result<Vec<u8>, ArchiveError> {
        let mut out = Vec::new();
        extract_binary(gz, &mut out, caps).map(|_| out)
    }

    #[test]
    fn extracts_only_the_binary() {
        let gz = targz(&[
            tar_entry("LICENSES.txt", b'0', b"licences"),
            tar_entry("soloist-link", b'2', b""),
            tar_entry("./soloist", b'0', b"#!/bin/sh\necho hi\n"),
        ]);
        assert_eq!(
            extract(&gz, Caps::default()).unwrap(),
            b"#!/bin/sh\necho hi\n"
        );
        // One directory deep is fine too.
        let gz = targz(&[
            tar_entry("soloist_release/", b'5', b""),
            tar_entry("soloist_release/soloist", b'0', b"bin"),
        ]);
        assert_eq!(extract(&gz, Caps::default()).unwrap(), b"bin");
    }

    #[test]
    fn gnu_long_names_are_followed() {
        let long = format!("a/{}/soloist", "d".repeat(120));
        let gz = targz(&[
            tar_entry("././@LongLink", b'L', long.as_bytes()),
            tar_entry("truncated-name", b'0', b"x"),
        ]);
        // Two levels deep: rejected (only top level or one directory).
        assert!(matches!(
            extract(&gz, Caps::default()),
            Err(ArchiveError::Missing)
        ));
        let gz = targz(&[
            tar_entry("././@LongLink", b'L', b"soloist"),
            tar_entry("whatever", b'0', b"x"),
        ]);
        assert_eq!(extract(&gz, Caps::default()).unwrap(), b"x");
    }

    #[test]
    fn hostile_archives_are_refused() {
        let caps = Caps {
            max_unpacked: 4096,
            max_binary: 100,
        };
        assert!(matches!(
            extract(b"PK\x03\x04zip", caps),
            Err(ArchiveError::NotGzip)
        ));
        assert!(matches!(extract(b"", caps), Err(ArchiveError::NotGzip)));
        // Symlink named soloist: never written.
        let gz = targz(&[tar_entry("soloist", b'2', b"")]);
        assert!(matches!(extract(&gz, caps), Err(ArchiveError::Missing)));
        // Path traversal.
        let gz = targz(&[tar_entry("../soloist", b'0', b"x")]);
        assert!(matches!(extract(&gz, caps), Err(ArchiveError::Missing)));
        // Binary over the cap.
        let gz = targz(&[tar_entry("soloist", b'0', &[7u8; 200])]);
        assert!(matches!(extract(&gz, caps), Err(ArchiveError::TooLarge)));
        // Too much unpacked data before the binary (a gzip bomb).
        let gz = targz(&[
            tar_entry("filler", b'0', &vec![0u8; 8192]),
            tar_entry("soloist", b'0', b"x"),
        ]);
        assert!(matches!(extract(&gz, caps), Err(ArchiveError::TooLarge)));
        // Corrupt header checksum.
        let mut e = tar_entry("soloist", b'0', b"x");
        e[0] = b'S';
        assert!(matches!(
            extract(&targz(&[e]), caps),
            Err(ArchiveError::Corrupt(_))
        ));
        // Truncated gzip.
        let gz = targz(&[tar_entry("soloist", b'0', &[1u8; 90])]);
        assert!(extract(&gz[..gz.len() / 2], caps).is_err());
    }

    /// `soloist --version` of the real 1.3.10 Linux x86_64 build.
    const REAL_VERSION: &str =
        "soloist 1.3.10.87 build 1795111339 (20261119) (g19412df6ea) (linux/x86_64)\n";

    #[test]
    fn real_version_output() {
        let b = parse_version(REAL_VERSION);
        assert_eq!(b.version.as_deref(), Some("1.3.10.87"));
        let built = b.built_unix.unwrap();
        assert_eq!(format_date(built), "2026-11-19");
        assert_eq!(built % DAY_SECS, 0, "the date, not the build number");
        assert_eq!(format_date(b.expires_unix().unwrap()), "2027-02-17");
        // Bad compact dates are ignored.
        assert_eq!(parse_version("soloist 1.0 (20261399)").built_unix, None);
    }

    #[test]
    fn version_output_is_parsed_leniently() {
        let out = "Spotify Soloist 1.4.2 (build 1f2e3d4)\nbuilt: 2026-10-01T12:34:56Z\nplatform: linux x86_64\n";
        let b = parse_version(out);
        assert_eq!(b.version.as_deref(), Some("1.4.2"));
        let built = b.built_unix.unwrap();
        assert_eq!(format_date(built), "2026-10-01");
        assert_eq!(built % DAY_SECS, 12 * 3600 + 34 * 60 + 56);
        assert_eq!(format_date(b.expires_unix().unwrap()), "2026-12-30");

        let b = parse_version("soloist v0.9.0-rc1+abc\nbuild timestamp 1767225600\n");
        assert_eq!(b.version.as_deref(), Some("0.9.0-rc1+abc"));
        assert_eq!(format_date(b.built_unix.unwrap()), "2026-01-01");

        let b = parse_version("soloist 2026-01-02 build_time=1767225600123");
        assert_eq!(b.version, None, "a date is not a version");
        assert_eq!(format_date(b.built_unix.unwrap()), "2026-01-02");

        assert_eq!(parse_version(""), BuildInfo::default());
        assert_eq!(parse_version("garbage 1999-99-99 123").built_unix, None);
    }

    #[test]
    fn civil_dates_roundtrip() {
        assert_eq!(days_from_civil(1970, 1, 1), 0);
        assert_eq!(format_date(0), "1970-01-01");
        assert_eq!(format_date(951_782_400), "2000-02-29");
        let d = days_from_civil(2026, 11, 19) as u64 * DAY_SECS;
        assert_eq!(format_date(d), "2026-11-19");
    }
}
