//! What a build of orrery is (docs/design/2026-11-22-self-update.md, "Releases").
//!
//! The version is Cargo's, with a `v`: `v0.2.0`. CI builds a release with `ORRERY_RELEASE=1`;
//! any other build is a development build, which doesn't update itself.

mod version;

pub use version::archive_target;

/// Whether CI built this as a release, for a target that has a release archive.
pub const RELEASE: bool = !env!("ORRERY_BUILD_RELEASE").is_empty();

/// The Rust target this was built for, such as `aarch64-unknown-linux-musl`.
pub const TARGET: &str = env!("ORRERY_BUILD_TARGET");

/// The repository a release came from, as `owner/name`. Empty for a development build.
pub const REPO: &str = env!("ORRERY_BUILD_REPO");

/// orrery's version, as `v0.2.0`, or `v0.2.0-rc.1` for a prerelease.
pub const VERSION: &str = env!("ORRERY_BUILD_VERSION");

/// The version with the kind of build: `v0.2.0 (aarch64)` for a release, and
/// `v0.2.0 (development build)` otherwise.
pub const LONG_VERSION: &str = env!("ORRERY_BUILD_LONG_VERSION");

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_facts() {
        assert_eq!(archive_target("aarch64-unknown-linux-musl"), Some("aarch64"));
        assert_eq!(archive_target("arm-unknown-linux-musleabihf"), None);
        assert_eq!(VERSION, concat!("v", env!("CARGO_PKG_VERSION")));
        assert!(LONG_VERSION.ends_with("(development build)") || RELEASE);
    }
}
