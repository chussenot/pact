//! The other half of "optional twice over": in a build without the `mcp`
//! feature, `pact mcp serve` must not exist at all.
//!
//! Gated `not(feature = "mcp")` so it runs in exactly the build it describes —
//! the default one, which is what `cargo install pact` produces and what the
//! zero-dependency promise is about. tests/mcp.rs is gated the other way.
#![cfg(not(feature = "mcp"))]

use std::process::Command;

fn pact(args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_pact"))
        .args(args)
        .output()
        .expect("failed to run pact binary")
}

/// The `features:` line of `pact --version`, from the binary this test will
/// actually spawn.
///
/// Needed because of a sharp edge in how integration tests reach the binary:
/// `CARGO_BIN_EXE_pact` is `target/debug/pact`, **one path shared by every
/// feature set**. A concurrent `cargo test --features …` overwrites it, so a test
/// compiled without `mcp` can end up spawning a binary that has it. That is not
/// hypothetical — it turned this file red the first time `mise run check` built
/// more than one feature set, because `depends` runs tasks in parallel.
///
/// Every other integration test in this repo is immune by luck rather than
/// design: they assert behaviour that does not vary with features. This one
/// asserts an *absence*, which is exactly what a swapped artifact destroys.
fn artifact_features() -> String {
    let version = pact(&["--version"]);
    let text = String::from_utf8_lossy(&version.stdout);
    text.lines()
        .find_map(|l| l.strip_prefix("features:"))
        .map(|f| f.trim().to_string())
        .unwrap_or_default()
}

#[test]
fn the_default_build_has_no_mcp_subcommand() {
    // Check what we are about to test before testing it. `mise run check` is
    // serialised so this should always hold; a bare parallel `cargo test`
    // invocation is the case it exists for, and skipping loudly beats a failure
    // that blames the code for the harness.
    let features = artifact_features();
    if features.contains("mcp") {
        eprintln!(
            "SKIP: target/debug/pact reports `features: {features}`, so another cargo \
             invocation replaced the artifact this test was built against. Run \
             `cargo test` alone, or `mise run check` (which serialises the legs)."
        );
        return;
    }

    let help = pact(&["--help"]);
    assert!(help.status.success());
    let text = String::from_utf8_lossy(&help.stdout);
    assert!(
        !text.contains("mcp"),
        "a default build must not advertise mcp (artifact features: {features:?}):\n{text}"
    );

    // Exit 5, not 2: a subcommand this build does not have is a usage error, and
    // exit 2 means only "another agent holds the lease" (see docs/cli.md). An
    // orchestrator probing for MCP support must not read the probe as a lease
    // conflict.
    let served = pact(&["mcp", "serve"]);
    assert_eq!(served.status.code(), Some(5));
}
