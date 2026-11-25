//! Strict semver comparison of release tags against the running version.

use semver::Version;

/// `v1.2.3` / `1.2.3` → a release version. Prereleases (`1.2.3-rc1`) and
/// anything that is not strict semver are refused.
pub fn parse_tag(tag: &str) -> Option<Version> {
    let t = tag.trim();
    let t = t.strip_prefix('v').unwrap_or(t);
    let v = Version::parse(t).ok()?;
    v.pre.is_empty().then_some(v)
}

/// True when `tag` is a release strictly newer than `current`.
pub fn is_newer(current: &str, tag: &str) -> bool {
    match (Version::parse(current.trim()), parse_tag(tag)) {
        (Ok(cur), Some(new)) => new.cmp_precedence(&cur).is_gt(),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compares_strict_semver() {
        assert!(is_newer("0.8.0", "v0.8.1"));
        assert!(is_newer("0.8.0", "0.9.0"));
        assert!(is_newer("0.8.9", "v0.10.0"), "numeric, not lexical");
        assert!(is_newer("0.8.0", "v1.0.0"));
        assert!(!is_newer("0.8.0", "v0.8.0"));
        assert!(!is_newer("0.8.1", "v0.8.0"));
        assert!(!is_newer("0.8.0", "v0.9.0-rc.1"), "prereleases are ignored");
        assert!(!is_newer("0.8.0", "v0.9"), "not strict semver");
        assert!(!is_newer("0.8.0", "latest"));
        assert!(!is_newer("garbage", "v9.9.9"));
        // Build metadata does not make a release newer.
        assert!(!is_newer("0.8.0", "v0.8.0+build.5"));
        assert_eq!(parse_tag(" v1.2.3 "), Some(Version::new(1, 2, 3)));
    }
}
