use std::io::Write;
use std::process::{Command, Output, Stdio};

fn mat(args: &[&str], input: &str) -> Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_mat"))
        .args(args)
        .env_remove("NO_COLOR")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("start mat");
    child
        .stdin
        .take()
        .expect("stdin")
        .write_all(input.as_bytes())
        .expect("write input");
    child.wait_with_output().expect("wait for mat")
}

#[test]
fn stdin_renders_markdown_without_escapes_when_output_is_piped() {
    let output = mat(&[], "# Heading\n\nA **bold** statement.\n");
    assert!(output.status.success(), "{:?}", output);
    let text = String::from_utf8(output.stdout).expect("UTF-8");
    assert!(text.contains("Heading"));
    assert!(text.contains("A bold statement."));
    assert!(!text.contains('\x1b'));
}

#[test]
fn file_input_and_explicit_stdin_produce_the_same_output() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("sample.md");
    let markdown = "# Sample\n\n- one\n- two\n";
    std::fs::write(&path, markdown).expect("write sample");
    let from_file = mat(&[path.to_str().expect("path")], "");
    let from_stdin = mat(&["-"], markdown);
    assert!(from_file.status.success(), "{:?}", from_file);
    assert_eq!(from_file.stdout, from_stdin.stdout);
    assert!(!from_file.stdout.is_empty());
}

#[test]
fn forced_color_keeps_styles_but_removes_input_terminal_controls() {
    let output = mat(
        &["--color", "always"],
        "# Heading\n\nSafe \x1b]52;c;bad-secret\x07text\x1b[2J.\n",
    );
    assert!(output.status.success(), "{:?}", output);
    let text = String::from_utf8(output.stdout).expect("UTF-8");
    assert!(text.contains("\x1b["));
    assert!(!text.contains("\x1b]52"));
    assert!(!text.contains("bad-secret"));
    assert!(!text.contains("\x1b[2J"));
    assert!(text.contains("Safe text."));
}

#[test]
fn image_is_a_readable_fallback_without_fetching_when_output_is_piped() {
    let output = mat(
        &[],
        "Before\n\n![Diagram](http://127.0.0.1:1/never-fetch.png)\n\nAfter\n",
    );
    assert!(output.status.success(), "{:?}", output);
    let text = String::from_utf8(output.stdout).expect("UTF-8");
    assert!(text.contains("Before"));
    assert!(text.contains("Diagram"));
    assert!(text.contains("http://127.0.0.1:1/never-fetch.png"));
    assert!(text.contains("After"));
    assert!(output.stderr.is_empty(), "{:?}", output.stderr);
    assert!(!text.contains('\x1b'));
}

#[test]
fn unreadable_input_has_nonzero_exit_and_no_document_output() {
    let dir = tempfile::tempdir().expect("tempdir");
    let missing = dir.path().join("missing.md");
    let output = mat(&[missing.to_str().expect("path")], "");
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    assert!(!output.stderr.is_empty());
}

#[test]
fn zero_width_is_rejected() {
    let output = mat(&["--width", "0"], "text");
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    assert!(!output.stderr.is_empty());
}

#[test]
fn requested_width_wraps_unicode_text_by_terminal_cells() {
    let output = mat(&["--width", "12"], "日本語の文章を狭い端末でも折り返す。\n");
    assert!(output.status.success(), "{:?}", output);
    let text = String::from_utf8(output.stdout).expect("UTF-8");
    assert!(text.lines().count() > 1);
    for line in text.lines() {
        assert!(unicode_width::UnicodeWidthStr::width(line) <= 12, "{line}");
    }
}

#[test]
fn broken_stdout_pipe_exits_without_diagnostics() {
    let mut child = Command::new(env!("CARGO_BIN_EXE_mat"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("start mat");
    drop(child.stdout.take());
    child
        .stdin
        .take()
        .expect("stdin")
        .write_all(b"# Heading\n\nText that must be written.\n")
        .expect("write input");
    let output = child.wait_with_output().expect("wait for mat");
    assert!(output.status.success(), "{:?}", output);
    assert!(output.stderr.is_empty(), "{:?}", output.stderr);
}
