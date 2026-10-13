#![cfg(unix)]

use std::fs;
use std::path::PathBuf;
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT_SCRIPT: AtomicU64 = AtomicU64::new(0);

struct ScriptFile(PathBuf);

impl ScriptFile {
    fn new(contents: &str) -> Self {
        let sequence = NEXT_SCRIPT.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "tty-sim-cli-{}-{sequence}.yaml",
            std::process::id()
        ));
        fs::write(&path, contents).unwrap();
        Self(path)
    }
}

impl Drop for ScriptFile {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

fn run(arguments: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_tty-sim"))
        .args(arguments)
        .output()
        .unwrap()
}

fn run_with_script(script: &ScriptFile, arguments: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_tty-sim"))
        .arg("--script")
        .arg(&script.0)
        .args(arguments)
        .output()
        .unwrap()
}

#[test]
fn invocation_script_and_spawn_failures_have_distinct_codes() {
    assert_eq!(run(&[]).status.code(), Some(1));

    let absent_script = run(&["--script", "/not/a/tty-sim-script", "--command", "true"]);
    assert_eq!(absent_script.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&absent_script.stderr).contains("script error"));

    let script = ScriptFile::new("steps: []\n");
    let spawn = run_with_script(&script, &["--command", "/not/a/tty-sim-program"]);
    assert_eq!(spawn.status.code(), Some(3));
    assert!(String::from_utf8_lossy(&spawn.stderr).contains("spawn error"));
}

#[test]
fn invalid_size_and_parse_errors_fail_before_spawn() {
    let valid = ScriptFile::new("steps: []\n");
    let size = run_with_script(&valid, &["--command", "/not/a/program", "--rows", "0"]);
    assert_eq!(size.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&size.stderr).contains("invocation error"));

    let invalid = ScriptFile::new("unknown\n");
    let parse = run_with_script(&invalid, &["--command", "/not/a/program"]);
    assert_eq!(parse.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&parse.stderr).contains("script error"));
}

#[test]
fn arguments_after_separator_are_forwarded_as_literal_data() {
    let script = ScriptFile::new("steps:\n  - expect: { pattern: '$HOME;a&b', timeout: 1s }\n");
    let output = run_with_script(
        &script,
        &[
            "--command",
            "/bin/sh",
            "--grace",
            "0ms",
            "--termination-grace",
            "0ms",
            "--",
            "-c",
            "trap '' TERM; printf %s \"$1\"; while :; do sleep 1; done",
            "tty-sim-test",
            "$HOME;a&b",
        ],
    );

    assert_eq!(output.status.code(), Some(9));
    assert!(
        output
            .stdout
            .windows(b"$HOME;a&b".len())
            .any(|bytes| bytes == b"$HOME;a&b")
    );
    assert!(!output.stdout.windows(5).any(|bytes| bytes == b"/home"));
}

#[test]
fn diagnostics_and_debug_seed_do_not_contaminate_stdout() {
    let script = ScriptFile::new("steps:\n  - expect: { pattern: payload, timeout: 1s }\n");
    let output = Command::new(env!("CARGO_BIN_EXE_tty-sim"))
        .arg("--script")
        .arg(&script.0)
        .args([
            "--command",
            "/bin/sh",
            "--seed",
            "42",
            "--grace",
            "0ms",
            "--termination-grace",
            "0ms",
            "--",
            "-c",
            "trap '' TERM; printf payload; while :; do sleep 1; done",
        ])
        .env("RUST_LOG", "debug")
        .output()
        .unwrap();

    assert_eq!(
        output.status.code(),
        Some(9),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stdout.windows(7).any(|bytes| bytes == b"payload"));
    assert!(!String::from_utf8_lossy(&output.stdout).contains("selected random typing seed"));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("selected random typing seed"));
    assert!(stderr.contains("42"));
}

#[test]
fn script_spawn_configuration_and_initial_masks_apply_before_output() {
    let script = ScriptFile::new(
        "cwd: .\nenv:\n  TTY_SIM_SET: script\n  TTY_SIM_UNSET: null\nmasks:\n  - startup-secret\nsteps:\n  - expect: { pattern: 'startup-secret:script:missing', timeout: 1s }\n",
    );
    let output = Command::new(env!("CARGO_BIN_EXE_tty-sim"))
        .arg("--script")
        .arg(&script.0)
        .args([
            "--command",
            "/bin/sh",
            "--grace",
            "0ms",
            "--termination-grace",
            "0ms",
            "--",
            "-c",
            "trap '' TERM; printf 'startup-secret:%s:%s' \"$TTY_SIM_SET\" \"${TTY_SIM_UNSET-missing}\"; while :; do sleep 1; done",
        ])
        .env("TTY_SIM_UNSET", "inherited")
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(9));
    assert!(
        output
            .stdout
            .windows(14)
            .any(|bytes| bytes == b"**************")
    );
    assert!(
        !output
            .stdout
            .windows(14)
            .any(|bytes| bytes == b"startup-secret")
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains(":script:missing"));
}

#[test]
fn invalid_script_working_directory_fails_before_spawn() {
    let script = ScriptFile::new("cwd: /not/a/tty-sim-directory\nsteps: []\n");
    let output = run_with_script(&script, &["--command", "/not/a/program"]);

    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&output.stderr).contains("working directory"));
}

#[test]
fn execution_failure_reports_source_location_in_diagnostics() {
    let script = ScriptFile::new(
        "steps:\n  - wait: 1ms\n  - expect: { pattern: 'never-seen', timeout: 20ms }\n",
    );
    let output = run_with_script(
        &script,
        &[
            "--command",
            "/bin/sh",
            "--grace",
            "0ms",
            "--termination-grace",
            "0ms",
            "--",
            "-c",
            "while :; do sleep 1; done",
        ],
    );

    assert_eq!(output.status.code(), Some(4));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("line 3, column 5"),
        "expected line 3, column 5 in stderr, got: {stderr}"
    );
}

#[test]
fn successful_script_execution_exits_with_code_zero() {
    let script = ScriptFile::new("steps:\n  - wait: 10ms\n");
    let output = run_with_script(
        &script,
        &["--command", "/bin/sh", "--", "-c", "sleep 0.05; exit 0"],
    );
    assert_eq!(
        output.status.code(),
        Some(0),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn child_exit_statuses_map_to_offset_or_unrepresentable_code() {
    let script = ScriptFile::new("steps:\n  - wait: 10ms\n");
    let output_42 = run_with_script(
        &script,
        &["--command", "/bin/sh", "--", "-c", "sleep 0.05; exit 42"],
    );
    assert_eq!(
        output_42.status.code(),
        Some(142),
        "stderr: {}",
        String::from_utf8_lossy(&output_42.stderr)
    );

    let output_200 = run_with_script(
        &script,
        &["--command", "/bin/sh", "--", "-c", "sleep 0.05; exit 200"],
    );
    assert_eq!(
        output_200.status.code(),
        Some(10),
        "stderr: {}",
        String::from_utf8_lossy(&output_200.stderr)
    );
}

#[test]
fn sigterm_escalation_exits_with_code_eight() {
    let script = ScriptFile::new("steps: []\n");
    let output = run_with_script(
        &script,
        &[
            "--command",
            "/bin/sh",
            "--grace",
            "0ms",
            "--termination-grace",
            "2s",
            "--",
            "-c",
            "trap 'exit 0' TERM; while :; do sleep 1; done",
        ],
    );
    assert_eq!(
        output.status.code(),
        Some(8),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}
