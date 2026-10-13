#![cfg(unix)]

use std::sync::{Arc, Mutex};
use std::time::Duration;
use tty_sim::{
    CallbackSink, ChildExitStatus, CleanupError, Command, Completion, Engine, EngineBuildError,
    EngineBuilder, ExecutionError, OutputFailure, ParseError, SpawnSpec, StdoutSink, TerminalSize,
    TerminationOutcome, TypingRange, parse_script,
};

#[test]
fn library_facade_provides_documented_entry_points_and_types() {
    // 0. Ensure error and sink types are present and inspectable
    let _ = std::mem::size_of::<EngineBuilder>();
    let _ = std::mem::size_of::<EngineBuildError>();
    let _ = std::mem::size_of::<ExecutionError>();
    let _ = std::mem::size_of::<CleanupError>();
    let _ = std::mem::size_of::<ParseError>();
    let _ = std::mem::size_of::<OutputFailure>();
    let _ = std::mem::size_of::<StdoutSink>();
    let _ = std::mem::size_of::<CallbackSink>();
    // 1. Script parsing
    let script = parse_script("steps:\n  - type: 'hello'\n").expect("valid script");
    assert_eq!(script.commands().len(), 1);

    // 2. Terminal size
    let size = TerminalSize::new(24, 80, 0, 0).expect("valid size");
    assert_eq!(size.rows(), 24);
    assert_eq!(size.cols(), 80);

    // 3. Typing range
    let range = TypingRange::new(Duration::from_millis(10), Duration::from_millis(50))
        .expect("valid range");
    assert_eq!(range.minimum(), Duration::from_millis(10));
    assert_eq!(range.maximum(), Duration::from_millis(50));

    // 4. Commands
    assert_eq!(Command::Typing(range), Command::Typing(range));

    // 5. EngineBuilder & Engine execution with CallbackSink
    let output_received = Arc::new(Mutex::new(Vec::new()));
    let output_clone = Arc::clone(&output_received);
    let mut spec = SpawnSpec::new("/bin/sh");
    spec.args = vec![
        "-c".into(),
        "printf 'ready:'; IFS= read -r line; printf 'got:%s\\n' \"$line\"; sleep 1".into(),
    ];
    spec.size = size;

    let mut engine: Engine = Engine::builder(spec)
        .output_callback(move |bytes| {
            output_clone.lock().unwrap().extend_from_slice(bytes);
            Ok(())
        })
        .build()
        .expect("engine builds successfully");

    engine
        .execute(&[
            Command::Expect {
                pattern: b"ready:".to_vec(),
                timeout: Duration::from_secs(2),
            },
            Command::Send(b"input-data\n".to_vec()),
            Command::Expect {
                pattern: b"got:input-data".to_vec(),
                timeout: Duration::from_secs(2),
            },
        ])
        .expect("commands execute successfully");

    let completion: Completion = engine.wait_for_exit().expect("process exits cleanly");
    assert_eq!(completion.status(), &ChildExitStatus::exited(0));
    assert_eq!(completion.termination(), TerminationOutcome::Natural);

    let output = output_received.lock().unwrap();
    let has_output = output
        .windows(b"got:input-data".len())
        .any(|w| w == b"got:input-data");
    drop(output);
    assert!(has_output);
}

#[test]
fn feature_namespaces_are_accessible() {
    use tty_sim::automation;
    use tty_sim::cmd;
    use tty_sim::presentation;
    use tty_sim::script;

    // automation models
    let _ = automation::CommandContext::Index(0);

    // presentation models
    let mask = presentation::MaskRule::new(b"foo".to_vec(), b"bar".to_vec()).unwrap();
    assert_eq!(mask.from(), b"foo");

    // script models
    let loc = script::SourceLocation { line: 1, column: 1 };
    assert_eq!(loc.line, 1);

    // cmd entry point
    assert!(std::ptr::fn_addr_eq(
        cmd::main_entry as fn() -> std::process::ExitCode,
        cmd::main_entry as fn() -> std::process::ExitCode
    ));
}
