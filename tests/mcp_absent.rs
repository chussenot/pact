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

#[test]
fn the_default_build_has_no_mcp_subcommand() {
    let help = pact(&["--help"]);
    assert!(help.status.success());
    let text = String::from_utf8_lossy(&help.stdout);
    assert!(
        !text.contains("mcp"),
        "a default build must not advertise mcp:\n{text}"
    );

    // Exit 5, not 2: a subcommand this build does not have is a usage error, and
    // exit 2 means only "another agent holds the lease" (see docs/cli.md). An
    // orchestrator probing for MCP support must not read the probe as a lease
    // conflict.
    let served = pact(&["mcp", "serve"]);
    assert_eq!(served.status.code(), Some(5));
}
