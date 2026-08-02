//! Stamps build provenance into the binary for `pact --version`.
//!
//! Deliberately emits no `cargo:rerun-if-changed`: that keeps cargo's default
//! (rerun when any file in the package changes), so the stamp goes stale only
//! on a commit that touches nothing — which is the point, since a stale `pact`
//! on PATH has silently rewritten `AGENTS.md` from an old build before.

use std::process::Command;

fn git(args: &[&str]) -> Option<String> {
    let out = Command::new("git").args(args).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8(out.stdout).ok()?.trim().to_string();
    (!s.is_empty()).then_some(s)
}

fn main() {
    // Tarball builds have no .git; "unknown" is honest and never fails the build.
    let sha = git(&["rev-parse", "--short=12", "HEAD"]).unwrap_or_else(|| "unknown".into());
    let dirty = match git(&["status", "--porcelain", "--untracked-files=no"]) {
        Some(_) => "-dirty",
        None => "",
    };
    let rustc = std::env::var("RUSTC").unwrap_or_else(|_| "rustc".into());
    let rustc = Command::new(rustc)
        .arg("--version")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|| "unknown".into());

    // CARGO_FEATURE_<NAME>=1 is set for each enabled feature.
    let mut features: Vec<String> = std::env::vars()
        .filter_map(|(k, _)| k.strip_prefix("CARGO_FEATURE_").map(str::to_lowercase))
        .collect();
    features.sort();
    let features = if features.is_empty() {
        "none".to_string()
    } else {
        features.join(",")
    };

    println!(
        "cargo:rustc-env=PACT_BUILD_TIME={}",
        chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
    );
    println!("cargo:rustc-env=PACT_GIT_SHA={sha}{dirty}");
    println!("cargo:rustc-env=PACT_RUSTC={rustc}");
    println!("cargo:rustc-env=PACT_FEATURES={features}");
    println!(
        "cargo:rustc-env=PACT_TARGET={}",
        std::env::var("TARGET").unwrap_or_else(|_| "unknown".into())
    );
    println!(
        "cargo:rustc-env=PACT_PROFILE={}",
        std::env::var("PROFILE").unwrap_or_else(|_| "unknown".into())
    );
}
