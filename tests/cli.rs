use std::process::Command;

#[test]
fn invalid_arguments_exit_with_a_usage_error() {
    let output = Command::new(env!("CARGO_BIN_EXE_codex-path-rules"))
        .arg("--self-tset")
        .output()
        .expect("run codex-path-rules");

    assert_eq!(output.status.code(), Some(2));
    assert_eq!(
        String::from_utf8_lossy(&output.stderr),
        "codex-path-rules: invalid arguments; use --help\n"
    );
}
