//! CLI entry point: reads the hook payload from stdin and prints the hook
//! output, failing open on any error (see the crate-level failure model in
//! `lib.rs`).

use std::env;
use std::io::{self, Read};
use std::process;

fn main() {
    if env::args().any(|arg| arg == "--self-test") {
        match codex_path_rules::run_self_test() {
            Ok(()) => {
                println!("path-rules self-test passed");
            }
            Err(error) => {
                eprintln!("[path-rules] self-test failed: {error}");
                process::exit(1);
            }
        }
        return;
    }

    // Fail open: a context-injection hook must never block the developer's
    // tool call, so a runtime error is reported on stderr and the process
    // still exits 0. The `--self-test` branch above is the only path allowed
    // to exit nonzero.
    if let Err(error) = run_cli() {
        eprintln!("[path-rules] {error}");
    }
}

/// Read the hook payload from stdin, run the hook, and print any output JSON.
fn run_cli() -> Result<(), String> {
    let mut text = String::new();
    io::stdin()
        .read_to_string(&mut text)
        .map_err(|error| format!("failed to read stdin: {error}"))?;

    if let Some(rendered) = codex_path_rules::run(&text)? {
        println!("{rendered}");
    }

    Ok(())
}
