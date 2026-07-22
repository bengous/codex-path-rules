//! CLI entry point: reads the hook payload from stdin and prints the hook
//! output, failing open on any error (see the crate-level failure model in
//! `lib.rs`).

use std::env;
use std::io::{self, Read};
use std::process;

fn main() {
    let mut args = env::args().skip(1);
    let first = args.next();
    let extra = args.next();

    match (first.as_deref(), extra) {
        (Some("--self-test"), None) => {
            match codex_path_rules::run_self_test() {
                Ok(()) => println!("path-rules self-test passed"),
                Err(error) => {
                    eprintln!("[path-rules] self-test failed: {error}");
                    process::exit(1);
                }
            }
            return;
        }
        (Some("--version" | "-V"), None) => {
            println!("codex-path-rules {}", env!("CARGO_PKG_VERSION"));
            return;
        }
        (Some("--help" | "-h"), None) => {
            println!(
                "codex-path-rules {}\n\nUsage: codex-path-rules [--help | --version | --self-test]\n\nReads a Codex hook payload from stdin and writes hook output to stdout.",
                env!("CARGO_PKG_VERSION")
            );
            return;
        }
        (None, None) => {}
        _ => {
            eprintln!("codex-path-rules: invalid arguments; use --help");
            process::exit(2);
        }
    }

    // Fail open: a context-injection hook must never block the developer's
    // tool call, so a runtime error is reported on stderr and the process
    // still exits 0. CLI validation and self-test failures may exit nonzero.
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
