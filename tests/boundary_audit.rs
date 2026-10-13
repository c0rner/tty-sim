#![cfg(unix)]

use std::collections::HashSet;
use std::error::Error;
use std::fs;
use std::path::{Path, PathBuf};

const fn assert_error<E: Error + Send + Sync + 'static>() {}

#[test]
fn obsolete_architectural_files_and_modules_are_removed() {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let src_dir = manifest_dir.join("src");

    let expected_entries: HashSet<&'static str> = HashSet::from([
        "automation",
        "cmd",
        "lib.rs",
        "main.rs",
        "presentation",
        "script",
        "terminal",
    ]);

    let actual_entries: HashSet<String> = fs::read_dir(&src_dir)
        .expect("read src dir")
        .filter_map(|entry| {
            let entry = entry.ok()?;
            Some(entry.file_name().to_string_lossy().into_owned())
        })
        .collect();

    assert_eq!(
        actual_entries,
        expected_entries
            .into_iter()
            .map(String::from)
            .collect::<HashSet<_>>(),
        "src directory must contain only the target feature modules and root files"
    );

    let forbidden = [
        "src/engine.rs",
        "src/domain",
        "src/ports",
        "src/adapters",
        "src/output",
        "src/policy",
    ];

    for path in &forbidden {
        assert!(
            !manifest_dir.join(path).exists(),
            "obsolete path {path} must not exist"
        );
    }
}

#[test]
fn feature_dependency_isolation_and_no_engine_references() {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let src_dir = manifest_dir.join("src");

    let mut rs_files = Vec::new();
    collect_rs_files(&src_dir, &mut rs_files);

    for file_path in &rs_files {
        let relative = file_path
            .strip_prefix(&manifest_dir)
            .unwrap()
            .to_string_lossy();
        let content = fs::read_to_string(file_path).unwrap();
        check_file_boundaries(&relative, &content);
    }
}

fn check_file_boundaries(relative: &str, content: &str) {
    assert!(
        !content.contains("crate::engine"),
        "file {relative} must not reference crate::engine"
    );

    if !relative.starts_with("src/script/") {
        assert!(
            !content.contains("serde_saphyr"),
            "file {relative} must not reference serde_saphyr"
        );
    }

    if !relative.starts_with("src/terminal/") {
        assert!(
            !content.contains("portable_pty"),
            "file {relative} must not reference portable_pty"
        );
        let non_test = content
            .lines()
            .take_while(|line| !line.trim().starts_with("#[cfg(test)]"))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            !non_test.contains("nix::"),
            "production code in {relative} must not reference nix"
        );
    }

    if !relative.starts_with("src/cmd/") {
        assert!(
            !content.contains("argh"),
            "file {relative} must not reference argh"
        );
    }

    check_cross_feature_rules(relative, content);
}

fn check_cross_feature_rules(relative: &str, content: &str) {
    if relative.starts_with("src/automation/") {
        assert!(
            !content.contains("crate::terminal"),
            "{relative} must not import terminal"
        );
        assert!(
            !content.contains("crate::script"),
            "{relative} must not import script"
        );
        assert!(
            !content.contains("crate::cmd"),
            "{relative} must not import cmd"
        );
    }

    if relative.starts_with("src/presentation/") {
        assert!(
            !content.contains("crate::automation"),
            "{relative} must not import automation"
        );
        assert!(
            !content.contains("crate::terminal"),
            "{relative} must not import terminal"
        );
        assert!(
            !content.contains("crate::script"),
            "{relative} must not import script"
        );
        assert!(
            !content.contains("crate::cmd"),
            "{relative} must not import cmd"
        );
    }

    assert!(
        !content.contains("presentation::coordinator"),
        "{relative} must not import presentation::coordinator"
    );
    assert!(
        !content.contains("presentation::masker"),
        "{relative} must not import presentation::masker"
    );
    assert!(
        !content.contains("presentation::protocol"),
        "{relative} must not import presentation::protocol"
    );
    assert!(
        !content.contains("presentation::sink"),
        "{relative} must not import presentation::sink"
    );
    assert!(
        !content.contains("script::grammar"),
        "{relative} must not import script::grammar"
    );
    assert!(
        !content.contains("script::yaml"),
        "{relative} must not import script::yaml"
    );
    assert!(
        !content.contains("terminal::pty"),
        "{relative} must not import terminal::pty"
    );
    assert!(
        !content.contains("terminal::scripted"),
        "{relative} must not import terminal::scripted"
    );
}

#[test]
fn public_api_types_and_error_independence() {
    use tty_sim::{
        CallbackSink, ChildExitStatus, CleanupError, Command, Completion, Engine, EngineBuildError,
        EngineBuilder, ExecutionError, MaskRule, OutputFailure, ParseError, SpawnSpec, StdoutSink,
        TerminalSize, TerminationOutcome, TypingRange, parse_script,
    };

    let _ = std::mem::size_of::<Engine>();
    let _ = std::mem::size_of::<EngineBuilder>();
    let _ = std::mem::size_of::<EngineBuildError>();
    let _ = std::mem::size_of::<ExecutionError>();
    let _ = std::mem::size_of::<CleanupError>();
    let _ = std::mem::size_of::<ParseError>();
    let _ = std::mem::size_of::<OutputFailure>();
    let _ = std::mem::size_of::<StdoutSink>();
    let _ = std::mem::size_of::<CallbackSink>();
    let _ = std::mem::size_of::<MaskRule>();
    let _ = std::mem::size_of::<Command>();
    let _ = std::mem::size_of::<Completion>();
    let _ = std::mem::size_of::<TerminationOutcome>();
    let _ = std::mem::size_of::<TypingRange>();
    let _ = std::mem::size_of::<SpawnSpec>();
    let _ = std::mem::size_of::<TerminalSize>();
    let _ = std::mem::size_of::<ChildExitStatus>();

    assert!(parse_script("steps:\n  - send: \"exit\\n\"\n").is_ok());

    assert_error::<EngineBuildError>();
    assert_error::<ExecutionError>();
    assert_error::<CleanupError>();
    assert_error::<ParseError>();
    assert_error::<OutputFailure>();
}

fn collect_rs_files(dir: &Path, files: &mut Vec<PathBuf>) {
    for entry in fs::read_dir(dir).unwrap() {
        let entry = entry.unwrap();
        let path = entry.path();
        if path.is_dir() {
            collect_rs_files(&path, files);
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            files.push(path);
        }
    }
}
