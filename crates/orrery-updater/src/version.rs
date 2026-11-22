//! Versions from `git describe --tags --long`, such as `v0.12.0-300-g991e1823-dirty`, read as
//! Moonraker's `GitVersion` reads them (`moonraker/utils/versions.py`, `GIT_VERSION_PATTERN`).

use std::cmp::Ordering;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitVersion {
    /// The text it came from.
    pub full: String,
    /// The tag, such as `v0.12.0`; empty when there's none.
    pub tag: String,
    release: Vec<u64>,
    /// `(rank, number)` for a pre-release: alpha 1, beta 2, release candidate 3. A final
    /// release is `(4, 0)`, so it sorts after its pre-releases.
    pre: (u8, u64),
    pub dev_count: u64,
    pub dirty: bool,
    /// Whether the text had a version at all, not only a commit hash.
    pub valid: bool,
}

impl GitVersion {
    pub fn parse(text: &str) -> Self {
        let full = text.trim().to_owned();
        let mut v = Self {
            full: full.clone(),
            tag: String::new(),
            release: Vec::new(),
            pre: (4, 0),
            dev_count: 0,
            dirty: false,
            valid: false,
        };
        let mut rest = full.as_str();
        for suffix in ["-inferred", "-shallow"] {
            rest = rest.strip_suffix(suffix).unwrap_or(rest);
        }
        if let Some(r) = rest.strip_suffix("-dirty") {
            v.dirty = true;
            rest = r;
        }
        // `-<count>-g<hash>`, or `-<count>`, after the tag.
        let digits = |s: &str| !s.is_empty() && s.chars().all(|c| c.is_ascii_digit());
        let hex = |s: &str| !s.is_empty() && s.chars().all(|c| c.is_ascii_hexdigit());
        if let Some((head, hash)) = rest.rsplit_once("-g")
            && hex(hash)
            && let Some((tag, count)) = head.rsplit_once('-')
            && digits(count)
        {
            v.dev_count = count.parse().unwrap_or(0);
            rest = tag;
        } else if let Some((tag, count)) = rest.rsplit_once('-')
            && digits(count)
        {
            v.dev_count = count.parse().unwrap_or(0);
            rest = tag;
        }
        let Some((release, pre)) = parse_tag(rest) else { return v };
        v.tag = rest.to_owned();
        v.release = release;
        v.pre = pre;
        v.valid = true;
        v
    }

    /// Moonraker's `short_version`: the tag and the count of commits after it, or `?`.
    pub fn short(&self) -> String {
        if self.valid { format!("{}-{}", self.tag, self.dev_count) } else { "?".into() }
    }

    pub fn is_final_release(&self) -> bool {
        self.valid && self.pre.0 == 4
    }

    pub fn is_alpha(&self) -> bool {
        self.valid && self.pre.0 == 1
    }
}

impl PartialOrd for GitVersion {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        if !self.valid || !other.valid {
            return None;
        }
        let len = self.release.len().max(other.release.len());
        let pad = |r: &[u64]| (0..len).map(|i| r.get(i).copied().unwrap_or(0)).collect::<Vec<_>>();
        Some((pad(&self.release), self.pre, self.dev_count).cmp(&(pad(&other.release), other.pre, other.dev_count)))
    }
}

/// `v1.2.3`, `1.2rc1`, `v2.0.0-beta.2`: the release numbers and the pre-release part.
fn parse_tag(tag: &str) -> Option<(Vec<u64>, (u8, u64))> {
    let s = tag.strip_prefix(['v', 'V']).unwrap_or(tag);
    let end = s.find(|c: char| !c.is_ascii_digit() && c != '.').unwrap_or(s.len());
    let (numbers, pre) = s.split_at(end);
    if numbers.is_empty() || numbers.starts_with('.') || numbers.ends_with('.') || numbers.contains("..") {
        return None;
    }
    let release = numbers.split('.').map(|n| n.parse().ok()).collect::<Option<Vec<u64>>>()?;
    if pre.is_empty() {
        return Some((release, (4, 0)));
    }
    let p = pre.strip_prefix(['-', '_', '.']).unwrap_or(pre).to_ascii_lowercase();
    let label_end = p.find(|c: char| !c.is_ascii_alphabetic()).unwrap_or(p.len());
    let (label, n) = p.split_at(label_end);
    let rank = match label {
        "a" | "alpha" => 1,
        "b" | "beta" => 2,
        "c" | "rc" | "pre" | "preview" => 3,
        _ => return None,
    };
    let n = n.strip_prefix(['-', '_', '.']).unwrap_or(n);
    let n = if n.is_empty() { 0 } else { n.parse().ok()? };
    Some((release, (rank, n)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_describe_output() {
        let v = GitVersion::parse("v2026.11.00-30-g991e1823-dirty");
        assert!(v.valid && v.dirty);
        assert_eq!((v.tag.as_str(), v.dev_count, v.short()), ("v2026.11.00", 30, "v2026.11.00-30".into()));
        let v = GitVersion::parse("v6.0.0-0-gabcdef12");
        assert!(v.is_final_release() && !v.dirty);
        assert_eq!(v.short(), "v6.0.0-0");
        // A repo without tags describes as a bare hash.
        let v = GitVersion::parse("991e1823");
        assert!(!v.valid);
        assert_eq!(v.short(), "?");
        let v = GitVersion::parse("991e1823-dirty");
        assert!(!v.valid && v.dirty);
    }

    #[test]
    fn pre_releases() {
        assert!(GitVersion::parse("v1.0.0-beta.2-0-gabc").valid);
        assert!(GitVersion::parse("v1.0.0a1-3-gabc").is_alpha());
        assert!(!GitVersion::parse("v1.0.0rc1-0-gabc").is_final_release());
        assert!(!GitVersion::parse("release-candidate-0-gabc").valid);
    }

    #[test]
    fn orders_like_moonraker() {
        let v = |s: &str| GitVersion::parse(s);
        assert!(v("v1.2.0-0-ga") < v("v1.2.0-1-gb"));
        assert!(v("v1.2.0rc1-5-ga") < v("v1.2.0-0-gb"));
        assert_eq!(v("v1.2-0-ga").partial_cmp(&v("v1.2.0-0-ga")), Some(Ordering::Equal));
        assert!(v("v1.10.0-0-ga") > v("v1.9.9-9-gb"));
        assert!(v("abc").partial_cmp(&v("v1.0.0-0-ga")).is_none());
    }
}
