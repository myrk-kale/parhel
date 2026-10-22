//! I-70: command-line contract of the binary.

mod common;

use common::*;
use rstest::rstest;

fn run(args: &[&str]) -> std::process::Output {
    let sb = Sandbox::new();
    parhel(&sb.env, &sb.root, args)
}

// I-70a
#[rstest]
#[case(&["--help"])]
#[case(&["-h"])]
#[case(&["help"])]
fn i70a_help_lists_commands(#[case] args: &[&str]) {
    let out = run(args);
    assert_success(&out);
    let text = stdout(&out);
    assert!(text.contains("git parhel"), "{text}");
    assert!(text.contains("add") && text.contains("doctor"), "{text}");
}

// I-70b
#[test]
fn i70b_add_help_documents_every_flag() {
    let out = run(&["add", "--help"]);
    assert_success(&out);
    let text = stdout(&out);
    for flag in [
        "--from",
        "--branch",
        "--new-branch",
        "--detach",
        "--allow-dirty",
        "--fallback",
        "--submodules",
    ] {
        assert!(text.contains(flag), "missing {flag}:\n{text}");
    }
    assert!(
        text.contains("<destination>") || text.contains("<DESTINATION>"),
        "{text}"
    );
}

// I-70c
#[rstest]
#[case(&["--version"])]
#[case(&["-V"])]
fn i70c_version(#[case] args: &[&str]) {
    let out = run(args);
    assert_success(&out);
    assert!(
        stdout(&out).contains(env!("CARGO_PKG_VERSION")),
        "{}",
        stdout(&out)
    );
}

// I-70d: usage errors exit 2 with the usage code; nothing is created.
#[rstest]
#[case::no_command(&[])]
#[case::unknown_command(&["remove", "x"])]
#[case::missing_destination(&["add"])]
#[case::conflicting_branch_flags(&["add", "d", "--branch", "a", "--new-branch", "b"])]
#[case::branch_and_detach(&["add", "d", "--branch", "a", "--detach"])]
#[case::branch_and_ref(&["add", "d", "v1", "--branch", "a"])]
#[case::bad_policy(&["add", "d", "--submodules=always"])]
#[case::removed_flag(&["add", "d", "--allow-mismatched-ref"])]
fn i70d_usage_errors(#[case] args: &[&str]) {
    let sb = Sandbox::new();
    let out = parhel(&sb.env, &sb.root, args);
    assert_error(&out, "usage");
    assert!(!sb.path("d").exists());
}

// I-70e: errors go to stderr, never stdout.
#[test]
fn i70e_errors_on_stderr_only() {
    let out = run(&["add"]);
    assert_error(&out, "usage");
    assert!(
        stdout(&out).is_empty(),
        "stdout must stay clean: {}",
        stdout(&out)
    );
}
