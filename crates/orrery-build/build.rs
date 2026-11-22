//! Bakes the build's facts into constants: CI sets `ORRERY_RELEASE=1` for a release, and
//! GitHub sets `GITHUB_REPOSITORY` to the repository CI runs in
//! (docs/design/2026-11-22-self-update.md, "Releases").

include!("src/version.rs");

fn main() {
    for var in ["ORRERY_RELEASE", "GITHUB_REPOSITORY"] {
        println!("cargo:rerun-if-env-changed={var}");
    }
    let target = std::env::var("TARGET").unwrap_or_default();
    // A release needs a target that has a release archive.
    let release = std::env::var("ORRERY_RELEASE").is_ok_and(|v| v == "1") && archive_target(&target).is_some();
    let repo = if release { std::env::var("GITHUB_REPOSITORY").unwrap_or_default() } else { String::new() };
    let version = format!("v{}", std::env::var("CARGO_PKG_VERSION").unwrap_or_default());
    let long = match archive_target(&target).filter(|_| release) {
        Some(arch) => format!("{version} ({arch})"),
        None => format!("{version} (development build)"),
    };
    println!("cargo:rustc-env=ORRERY_BUILD_RELEASE={}", if release { "1" } else { "" });
    println!("cargo:rustc-env=ORRERY_BUILD_TARGET={target}");
    println!("cargo:rustc-env=ORRERY_BUILD_REPO={repo}");
    println!("cargo:rustc-env=ORRERY_BUILD_VERSION={version}");
    println!("cargo:rustc-env=ORRERY_BUILD_LONG_VERSION={long}");
}
