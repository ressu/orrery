// Shared by build.rs and the library, through `include!`, so the rules are in one place.

/// The name of the release archive for a Rust target: `aarch64` in
/// `orrery-v0.2.0-aarch64.tar.gz`. `None` for a target orrery doesn't release.
pub fn archive_target(target: &str) -> Option<&'static str> {
    match target {
        "aarch64-unknown-linux-musl" => Some("aarch64"),
        "armv7-unknown-linux-musleabihf" => Some("armv7"),
        "x86_64-unknown-linux-musl" => Some("x86_64"),
        _ => None,
    }
}
