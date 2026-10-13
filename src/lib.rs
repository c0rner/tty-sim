//! Synchronous terminal automation over a pseudo-terminal.
//!
//! Create a session with [`Engine::builder`], execute [`Command`] values or the
//! result of [`parse_script`], then explicitly consume the engine with
//! [`Engine::finish`], [`Engine::wait_for_exit`], or [`Engine::terminate`]. Do not
//! rely on `Drop`: it performs only best-effort cleanup and cannot report errors.
//!
//! ```no_run
//! use tty_sim::{Engine, SpawnSpec, parse_script};
//!
//! # fn run() -> Result<(), Box<dyn std::error::Error>> {
//! let script = parse_script("steps:\n  - send: \"exit\\n\"\n")?;
//! let commands = script.commands();
//! let mut engine = Engine::builder(SpawnSpec::new("sh")).build()?;
//! engine.execute(&commands)?;
//! let completion = engine.finish()?;
//! assert!(completion.status().success());
//! # Ok(())
//! # }
//! ```

#[cfg(target_os = "windows")]
compile_error!("tty-sim does not support Windows; use a POSIX-style target");

#[cfg(unix)]
pub(crate) mod terminal;

pub mod automation;
pub mod cmd;
pub mod presentation;
pub mod script;

use std::io::{self, Read, Write};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use thiserror::Error;

#[cfg(test)]
use crate::automation::RecordingSleeper;
use crate::automation::terminal::{
    ChildHandle, RunningTerminal, TerminalBackend, TerminalControl, TerminalError,
};
use crate::automation::{
    ActiveTerminal, DEFAULT_NATURAL_EXIT_GRACE, DEFAULT_TERMINATION_GRACE, PresentationApi,
    RandomPolicy, Session, SleepPolicy, spawn_reader,
};
use crate::presentation::{
    DEFAULT_HISTORY_CAPACITY, ExpectationError, HistoryCapacity, OutputCoordinator, OutputSink,
    PresentationError, SinkAdapter,
};
use crate::terminal::{PtyBackend, PtyChild, PtyControl, PtyInput, PtyReader};
#[cfg(test)]
use crate::terminal::{
    ScriptedChild, ScriptedControl, ScriptedInput, ScriptedOutput, ScriptedTerminal,
};

pub use automation::terminal::{ChildExitStatus, SpawnSpec, TerminalSize};
pub use automation::{
    CleanupError, Command, Completion, ExecutionError, TerminationOutcome, TypingRange,
};
pub use presentation::{CallbackSink, MaskRule, OutputFailure, StdoutSink};
pub use script::{ParseError, parse_script};

#[derive(Debug, Error)]
pub enum EngineBuildError {
    #[error(transparent)]
    Configuration(#[from] crate::presentation::ConfigurationError),
    #[error(transparent)]
    Terminal(#[from] TerminalError),
}

enum BackendAdapter {
    Pty(PtyBackend),
    #[cfg(test)]
    Scripted(ScriptedTerminal),
}

enum TerminalInput {
    Pty(PtyInput),
    #[cfg(test)]
    Scripted(ScriptedInput),
}

impl Write for TerminalInput {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        match self {
            Self::Pty(input) => input.write(buffer),
            #[cfg(test)]
            Self::Scripted(input) => input.write(buffer),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match self {
            Self::Pty(input) => input.flush(),
            #[cfg(test)]
            Self::Scripted(input) => input.flush(),
        }
    }
}

enum TerminalOutput {
    Pty(PtyReader),
    #[cfg(test)]
    Scripted(ScriptedOutput),
}

impl Read for TerminalOutput {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        match self {
            Self::Pty(output) => output.read(buffer),
            #[cfg(test)]
            Self::Scripted(output) => output.read(buffer),
        }
    }
}

enum ChildProcess {
    Pty(PtyChild),
    #[cfg(test)]
    Scripted(ScriptedChild),
}

impl ChildHandle for ChildProcess {
    fn try_wait(&mut self) -> io::Result<Option<ChildExitStatus>> {
        match self {
            Self::Pty(child) => child.try_wait(),
            #[cfg(test)]
            Self::Scripted(child) => child.try_wait(),
        }
    }

    fn wait(&mut self) -> io::Result<ChildExitStatus> {
        match self {
            Self::Pty(child) => child.wait(),
            #[cfg(test)]
            Self::Scripted(child) => child.wait(),
        }
    }

    fn process_group_is_alive(&self) -> io::Result<bool> {
        match self {
            Self::Pty(child) => child.process_group_is_alive(),
            #[cfg(test)]
            Self::Scripted(child) => child.process_group_is_alive(),
        }
    }

    fn terminate_group(&mut self) -> io::Result<()> {
        match self {
            Self::Pty(child) => child.terminate_group(),
            #[cfg(test)]
            Self::Scripted(child) => child.terminate_group(),
        }
    }

    fn kill_group(&mut self) -> io::Result<()> {
        match self {
            Self::Pty(child) => child.kill_group(),
            #[cfg(test)]
            Self::Scripted(child) => child.kill_group(),
        }
    }
}

enum TerminalController {
    Pty(PtyControl),
    #[cfg(test)]
    Scripted(ScriptedControl),
}

impl TerminalControl for TerminalController {
    fn resize(&self, size: TerminalSize) -> io::Result<()> {
        match self {
            Self::Pty(control) => control.resize(size),
            #[cfg(test)]
            Self::Scripted(control) => control.resize(size),
        }
    }

    fn close(&self) -> io::Result<()> {
        match self {
            Self::Pty(control) => control.close(),
            #[cfg(test)]
            Self::Scripted(control) => control.close(),
        }
    }
}

impl TerminalBackend for BackendAdapter {
    type Input = TerminalInput;
    type Output = TerminalOutput;
    type Child = ChildProcess;
    type Control = TerminalController;

    fn spawn(
        &self,
        spec: &SpawnSpec,
    ) -> io::Result<RunningTerminal<Self::Input, Self::Output, Self::Child, Self::Control>> {
        match self {
            Self::Pty(backend) => backend.spawn(spec).map(|running| RunningTerminal {
                input: TerminalInput::Pty(running.input),
                output: TerminalOutput::Pty(running.output),
                child: ChildProcess::Pty(running.child),
                control: TerminalController::Pty(running.control),
            }),
            #[cfg(test)]
            Self::Scripted(backend) => backend.spawn(spec).map(|running| RunningTerminal {
                input: TerminalInput::Scripted(running.input),
                output: TerminalOutput::Scripted(running.output),
                child: ChildProcess::Scripted(running.child),
                control: TerminalController::Scripted(running.control),
            }),
        }
    }
}

impl<S: OutputSink> PresentationApi for OutputCoordinator<S> {
    fn accept_child_output(&self, bytes: &[u8]) -> Result<Vec<u8>, PresentationError> {
        self.accept_child_output(bytes)
    }

    fn emit_presentation(&self, bytes: &[u8]) -> Result<(), PresentationError> {
        self.emit_presentation(bytes)
    }

    fn install_mask(&self, rule: &MaskRule) -> Result<(), PresentationError> {
        self.install_mask(rule)
    }

    fn wait_for_expectation(
        &self,
        pattern: &[u8],
        timeout: Duration,
    ) -> Result<(), ExpectationError> {
        self.wait_for_expectation(pattern, timeout)
    }

    fn failure(&self) -> Result<Option<Arc<OutputFailure>>, PresentationError> {
        self.failure()
    }

    fn record_reader_failure(
        &self,
        source: io::Error,
    ) -> Result<Arc<OutputFailure>, PresentationError> {
        self.record_reader_failure(source)
    }

    fn finish(&self) -> Result<(), PresentationError> {
        self.finish()
    }
}

type EngineTerminal = ActiveTerminal<TerminalInput, ChildProcess, TerminalController>;
type EngineSession =
    Session<EngineTerminal, Arc<OutputCoordinator<SinkAdapter>>, SleepPolicy, RandomPolicy>;

/// A synchronous terminal-automation session.
///
/// Command execution does not imply child completion. Consume every session with
/// [`Self::finish`], [`Self::wait_for_exit`], or [`Self::terminate`] so lifecycle
/// and cleanup failures remain observable. Dropping an unfinished session performs
/// only best-effort cleanup.
pub struct Engine {
    session: EngineSession,
}

impl Engine {
    pub const fn builder(spec: SpawnSpec) -> EngineBuilder {
        EngineBuilder {
            spec,
            history_capacity: DEFAULT_HISTORY_CAPACITY,
            seed: None,
            natural_exit_grace: DEFAULT_NATURAL_EXIT_GRACE,
            termination_grace: DEFAULT_TERMINATION_GRACE,
            sink: SinkAdapter::Stdout(StdoutSink),
            backend: BackendAdapter::Pty(PtyBackend),
            sleeper: SleepPolicy::Thread,
            initial_masks: Vec::new(),
        }
    }

    /// Executes direct library-created commands in source order.
    ///
    /// # Errors
    ///
    /// Returns the first validation, child, input, output, or expectation failure.
    pub fn execute(&mut self, commands: &[Command]) -> Result<(), ExecutionError> {
        self.session.execute(commands)
    }

    /// Resizes the running terminal.
    ///
    /// # Errors
    ///
    /// Returns a terminal-control error while preserving its I/O source.
    pub fn resize(&self, size: TerminalSize) -> Result<(), TerminalError> {
        self.session.resize(size)
    }

    /// Waits without a timeout for the direct child and output stream to finish.
    ///
    /// # Errors
    ///
    /// Returns every failure observed while draining, joining, closing, and reaping.
    pub fn wait_for_exit(self) -> Result<Completion, CleanupError> {
        self.session.wait_for_exit()
    }

    /// Allows the configured natural-exit grace before bounded TERM/KILL cleanup.
    ///
    /// # Errors
    ///
    /// Returns every lifecycle or cleanup failure observed during completion.
    pub fn finish(self) -> Result<Completion, CleanupError> {
        self.session.finish()
    }

    /// Immediately starts bounded TERM/KILL cleanup.
    ///
    /// # Errors
    ///
    /// Returns every lifecycle or cleanup failure observed during termination.
    pub fn terminate(self) -> Result<Completion, CleanupError> {
        self.session.terminate()
    }

    #[cfg(test)]
    const fn session_mut(&mut self) -> &mut EngineSession {
        &mut self.session
    }
}

/// Fluent configuration returned by [`Engine::builder`].
#[must_use]
pub struct EngineBuilder {
    spec: SpawnSpec,
    history_capacity: usize,
    seed: Option<u64>,
    natural_exit_grace: Duration,
    termination_grace: Duration,
    sink: SinkAdapter,
    backend: BackendAdapter,
    sleeper: SleepPolicy,
    initial_masks: Vec<MaskRule>,
}

impl EngineBuilder {
    pub const fn terminal_size(mut self, size: TerminalSize) -> Self {
        self.spec.size = size;
        self
    }

    pub const fn history_capacity(mut self, capacity: usize) -> Self {
        self.history_capacity = capacity;
        self
    }

    pub const fn seed(mut self, seed: u64) -> Self {
        self.seed = Some(seed);
        self
    }

    pub const fn natural_exit_grace(mut self, grace: Duration) -> Self {
        self.natural_exit_grace = grace;
        self
    }

    pub const fn termination_grace(mut self, grace: Duration) -> Self {
        self.termination_grace = grace;
        self
    }

    pub fn initial_masks(mut self, masks: Vec<MaskRule>) -> Self {
        self.initial_masks = masks;
        self
    }

    /// Replaces stdout presentation with a serialized byte callback.
    ///
    /// Returning an I/O error records a sink failure and stops output processing.
    /// A subsequent or active command observes that failure as an
    /// [`ExecutionError::Output`].
    pub fn output_callback<F>(mut self, callback: F) -> Self
    where
        F: Fn(&[u8]) -> io::Result<()> + Send + Sync + 'static,
    {
        self.sink = SinkAdapter::Callback(CallbackSink::from_io(callback));
        self
    }

    /// Spawns the configured terminal session and its single output reader.
    ///
    /// # Errors
    ///
    /// Returns an invalid history-capacity or terminal-spawn error.
    pub fn build(self) -> Result<Engine, EngineBuildError> {
        let history_capacity = HistoryCapacity::new(self.history_capacity)?;
        let coordinator = Arc::new(OutputCoordinator::new(
            history_capacity,
            self.sink,
            &self.initial_masks,
        ));
        let RunningTerminal {
            input,
            output,
            child,
            control,
        } = self
            .backend
            .spawn(&self.spec)
            .map_err(TerminalError::Spawn)?;
        let input = Arc::new(Mutex::new(input));
        let reader = spawn_reader(output, Arc::clone(&input), Arc::clone(&coordinator));
        let random = self
            .seed
            .map_or_else(RandomPolicy::from_entropy, RandomPolicy::seeded);
        let terminal = ActiveTerminal::new(input, child, control, reader);
        let session = Session::new(
            terminal,
            coordinator,
            self.sleeper,
            random,
            history_capacity.get(),
            self.natural_exit_grace,
            self.termination_grace,
        );

        Ok(Engine { session })
    }

    #[cfg(test)]
    fn scripted_terminal(mut self, terminal: ScriptedTerminal) -> Self {
        self.backend = BackendAdapter::Scripted(terminal);
        self
    }

    #[cfg(test)]
    fn recording_sleeper(mut self, sleeper: RecordingSleeper) -> Self {
        self.sleeper = SleepPolicy::Recording(sleeper);
        self
    }
}

#[cfg(test)]
mod tests {
    use std::io;
    use std::sync::{Arc, Mutex};
    use std::thread;
    use std::time::{Duration, Instant};

    use super::Engine;
    use crate::automation::RecordingSleeper;
    use crate::automation::terminal::{
        ChildExitStatus, EnvironmentChange, SpawnSpec, TerminalSize,
    };
    use crate::automation::{
        CleanupFailure, Command, CommandConfigurationError, CommandContext, ExecutionError,
        TerminalSession, TerminationOutcome, TypingRange,
    };
    use crate::presentation::ConfigurationError as PresentationConfigError;
    use crate::terminal::{ScriptedEnd, ScriptedSignal, ScriptedStep, ScriptedTerminal};

    #[test]
    fn parsed_walkthrough_splits_utf8_expectations_and_masks_across_output_chunks() {
        let terminal = ScriptedTerminal::new(
            vec![
                step("é".as_bytes(), vec![]),
                step(b"\x1b[A", vec![]),
                step(
                    b"start",
                    vec![b"rea".to_vec(), vec![b'd', b'y', 0xc3], vec![0xa9]],
                ),
                step(
                    b"again",
                    vec![
                        b"raw-se".to_vec(),
                        vec![b'c', b'r', b'e', b't', 0xc3],
                        vec![0xa9, b'-'],
                        b"done".to_vec(),
                    ],
                ),
            ],
            ScriptedEnd::Pending,
        );
        let observations = terminal.observations();
        let presentation = Arc::new(Mutex::new(Vec::new()));
        let sleeper = RecordingSleeper::default();
        let sleeps = sleeper.observations();
        let mut engine = test_builder(terminal, Arc::clone(&presentation))
            .seed(17)
            .recording_sleeper(sleeper)
            .build()
            .unwrap();
        let script = crate::parse_script(
            r#"
steps:
  - type: é
  - key: Up
  - send: start
  - expect: { pattern: readyé, timeout: 1s }
  - mask: { from: secreté, to: "***" }
  - send: again
  - expect: { pattern: raw-secreté-done, timeout: 1s }
  - show: " narration "
  - wait: 8ms
  - simulate: { command: demo, output: result }
"#,
        )
        .unwrap();

        let commands = script
            .steps()
            .iter()
            .map(|step| step.command.clone())
            .collect::<Vec<_>>();
        engine.execute(&commands).unwrap();

        assert_eq!(
            *observations.input.lock().unwrap(),
            "é\x1b[Astartagain".as_bytes()
        );
        assert_eq!(*observations.flushes.lock().unwrap(), 4);
        let sleeps = sleeps.lock().unwrap();
        assert_eq!(sleeps.len(), 6);
        assert!(
            (crate::automation::DEFAULT_TYPE_MIN_DELAY..=crate::automation::DEFAULT_TYPE_MAX_DELAY)
                .contains(&sleeps[0])
        );
        assert_eq!(sleeps[1], Duration::from_millis(8));
        assert!(sleeps[2..].iter().all(|delay| {
            (crate::automation::DEFAULT_TYPE_MIN_DELAY..=crate::automation::DEFAULT_TYPE_MAX_DELAY)
                .contains(delay)
        }));
        drop(sleeps);
        let visible = presentation.lock().unwrap().concat();
        assert!(visible.starts_with("readyé".as_bytes()));
        assert!(
            visible
                .windows(b"raw-***-done".len())
                .any(|part| part == b"raw-***-done")
        );
        assert!(
            !visible
                .windows("secreté".len())
                .any(|part| part == "secreté".as_bytes())
        );
        assert!(visible.ends_with(b" narration demoresult"));
    }

    #[test]
    fn type_and_simulate_emit_exact_bytes_without_implicit_line_endings() {
        let terminal = ScriptedTerminal::new(vec![step(b"go", vec![])], ScriptedEnd::Pending);
        let observations = terminal.observations();
        let presentation = Arc::new(Mutex::new(Vec::new()));
        let sleeper = RecordingSleeper::default();
        let sleeps = sleeper.observations();
        let mut engine = test_builder(terminal, Arc::clone(&presentation))
            .seed(23)
            .recording_sleeper(sleeper)
            .build()
            .unwrap();
        let commands = [
            Command::Typing(
                TypingRange::new(Duration::from_millis(2), Duration::from_millis(2)).unwrap(),
            ),
            Command::Type("go".into()),
            Command::Simulate {
                command: "éx".into(),
                output: b"done".to_vec(),
            },
        ];

        engine.execute(&commands).unwrap();

        assert_eq!(*observations.input.lock().unwrap(), b"go");
        assert_eq!(*observations.flushes.lock().unwrap(), 2);
        assert_eq!(
            *presentation.lock().unwrap(),
            Vec::from(["é".as_bytes().to_vec(), b"x".to_vec(), b"done".to_vec()])
        );
        let sleeps = sleeps.lock().unwrap();
        assert_eq!(sleeps.len(), 4);
        assert_eq!(sleeps[..2], [Duration::from_millis(2); 2]);
        assert_eq!(sleeps.as_slice(), [Duration::from_millis(2); 4]);
        drop(sleeps);
    }

    #[test]
    fn terminal_queries_are_answered_inside_the_pty_and_withheld_from_presentation() {
        let background_query = b"\x1b]11;?\x1b\\";
        let background_response = b"\x1b]11;rgb:0000/0000/0000\x1b\\";
        let cursor_query = b"\x1b[6n";
        let cursor_response = b"\x1b[1;1R";
        let terminal = ScriptedTerminal::new(
            vec![
                step(
                    b"probe",
                    vec![b"visible\x1b]11;?".to_vec(), b"\x1b\\".to_vec()],
                ),
                step(background_response, vec![b"\x1b[".to_vec(), b"6n".to_vec()]),
                step(cursor_response, vec![b"done".to_vec()]),
            ],
            ScriptedEnd::Pending,
        );
        let observations = terminal.observations();
        let presentation = Arc::new(Mutex::new(Vec::new()));
        let mut engine = test_builder(terminal, Arc::clone(&presentation))
            .build()
            .unwrap();
        let commands = [
            Command::Send(b"probe".to_vec()),
            Command::Expect {
                pattern: background_query.to_vec(),
                timeout: Duration::from_secs(1),
            },
            Command::Expect {
                pattern: cursor_query.to_vec(),
                timeout: Duration::from_secs(1),
            },
            Command::Expect {
                pattern: b"done".to_vec(),
                timeout: Duration::from_secs(1),
            },
            Command::Show(b"barrier".to_vec()),
        ];

        engine.execute(&commands).unwrap();

        assert_eq!(
            *observations.input.lock().unwrap(),
            [b"probe".as_slice(), background_response, cursor_response].concat()
        );
        assert_eq!(presentation.lock().unwrap().concat(), b"visibledonebarrier");
    }

    #[test]
    fn engine_seed_reproduces_ranged_typing_delays() {
        let first = typing_delays(91);
        let second = typing_delays(91);

        assert_eq!(first, second);
        assert!(first.iter().all(|delay| {
            (Duration::from_millis(2)..=Duration::from_millis(5)).contains(delay)
        }));
    }

    #[test]
    fn validation_finishes_before_any_command_writes() {
        let terminal =
            ScriptedTerminal::new(vec![step(b"must-not-write", vec![])], ScriptedEnd::Pending);
        let observations = terminal.observations();
        let mut engine = test_builder(terminal, Arc::new(Mutex::new(Vec::new())))
            .history_capacity(3)
            .build()
            .unwrap();
        let commands = [
            Command::Send(b"must-not-write".to_vec()),
            Command::Expect {
                pattern: b"long".to_vec(),
                timeout: Duration::ZERO,
            },
        ];

        let error = engine.execute(&commands).unwrap_err();

        assert!(matches!(
            error,
            ExecutionError::Configuration {
                context: CommandContext::Index(2),
                source: CommandConfigurationError::Presentation(
                    PresentationConfigError::ExpectPatternTooLarge { .. }
                )
            }
        ));
        assert!(observations.input.lock().unwrap().is_empty());
    }

    #[test]
    fn scripted_mismatch_and_early_exit_keep_command_context() {
        let mismatch = ScriptedTerminal::new(vec![step(b"right", vec![])], ScriptedEnd::Pending);
        let mut mismatch_engine = test_builder(mismatch, Arc::new(Mutex::new(Vec::new())))
            .build()
            .unwrap();
        assert!(matches!(
            mismatch_engine.execute(&[Command::Send(b"wrong".to_vec())]),
            Err(ExecutionError::Input {
                context: CommandContext::Index(1),
                ..
            })
        ));

        let exited = ScriptedTerminal::new(
            vec![ScriptedStep {
                expected_input: b"go".to_vec(),
                output_chunks: vec![],
                exit_after: Some(ChildExitStatus::exited(0)),
            }],
            ScriptedEnd::Pending,
        );
        let mut exited_engine = test_builder(exited, Arc::new(Mutex::new(Vec::new())))
            .build()
            .unwrap();
        assert!(matches!(
            exited_engine.execute(&[Command::Send(b"go".to_vec())]),
            Err(ExecutionError::EarlyChildExit {
                context: CommandContext::Index(1),
                status
            }) if status == ChildExitStatus::exited(0)
        ));
    }

    #[test]
    fn parsed_command_failures_retain_source_location() {
        let terminal = ScriptedTerminal::new(vec![step(b"right", vec![])], ScriptedEnd::Pending);
        let mut engine = test_builder(terminal, Arc::new(Mutex::new(Vec::new())))
            .build()
            .unwrap();
        let script = crate::parse_script("steps:\n  - send: wrong\n").unwrap();
        let commands = script.commands();

        let error = engine.execute(&commands).unwrap_err();
        assert!(matches!(
            error,
            ExecutionError::Input {
                context: CommandContext::Index(1),
                ..
            }
        ));
        let failed_index = error.context().index();
        assert_eq!(script.steps()[failed_index - 1].location.line, 2);
        assert_eq!(script.command_location(failed_index).unwrap().line, 2);
    }

    #[test]
    fn reader_and_sink_failures_and_resize_remain_distinct() {
        let reader = ScriptedTerminal::new(vec![], ScriptedEnd::ReaderFailure("reader".into()));
        let mut reader_engine = test_builder(reader, Arc::new(Mutex::new(Vec::new())))
            .build()
            .unwrap();
        assert!(matches!(
            reader_engine.execute(&[Command::Expect {
                pattern: b"missing".to_vec(),
                timeout: Duration::from_secs(1),
            }]),
            Err(ExecutionError::Output { source, .. })
                if matches!(&*source, crate::OutputFailure::Reader(_))
        ));

        let sink_terminal = ScriptedTerminal::new(vec![], ScriptedEnd::Pending);
        let mut sink_engine = Engine::builder(SpawnSpec::new("unused"))
            .scripted_terminal(sink_terminal)
            .output_callback(|_| Err(io::Error::other("sink")))
            .build()
            .unwrap();
        assert!(matches!(
            sink_engine.execute(&[Command::Show(b"visible".to_vec())]),
            Err(ExecutionError::Output { source, .. })
                if matches!(&*source, crate::OutputFailure::Sink(_))
        ));

        let resize_terminal = ScriptedTerminal::new(vec![], ScriptedEnd::Pending);
        let observations = resize_terminal.observations();
        let resize_engine = test_builder(resize_terminal, Arc::new(Mutex::new(Vec::new())))
            .build()
            .unwrap();
        let size = TerminalSize::new(50, 120, 0, 0).unwrap();
        resize_engine.resize(size).unwrap();
        assert_eq!(*observations.sizes.lock().unwrap(), [size]);
    }

    #[test]
    fn wait_for_exit_drains_output_joins_reader_and_closes_terminal() {
        let terminal = ScriptedTerminal::new(
            vec![ScriptedStep {
                expected_input: vec![],
                output_chunks: vec![b"final output".to_vec()],
                exit_after: Some(ChildExitStatus::exited(7)),
            }],
            ScriptedEnd::Eof,
        );
        let observations = terminal.observations();
        let presentation = Arc::new(Mutex::new(Vec::new()));
        let engine = test_builder(terminal, Arc::clone(&presentation))
            .build()
            .unwrap();

        let completion = engine.wait_for_exit().unwrap();

        assert_eq!(completion.status(), &ChildExitStatus::exited(7));
        assert_eq!(completion.termination(), TerminationOutcome::Natural);
        assert_eq!(presentation.lock().unwrap().concat(), b"final output");
        assert_eq!(*observations.closes.lock().unwrap(), 1);
        assert!(observations.signals.lock().unwrap().is_empty());
    }

    #[test]
    fn finish_reports_term_and_kill_escalation() {
        let term_terminal = ScriptedTerminal::new(vec![], ScriptedEnd::Pending);
        let term_observations = term_terminal.observations();
        let term_engine = test_builder(term_terminal, Arc::new(Mutex::new(Vec::new())))
            .natural_exit_grace(Duration::ZERO)
            .termination_grace(Duration::ZERO)
            .build()
            .unwrap();

        let term = term_engine.finish().unwrap();

        assert_eq!(term.termination(), TerminationOutcome::Sigterm);
        assert_eq!(
            *term_observations.signals.lock().unwrap(),
            [ScriptedSignal::Term]
        );
        assert_eq!(*term_observations.closes.lock().unwrap(), 1);

        let kill_terminal = ScriptedTerminal::new(vec![], ScriptedEnd::Pending).ignore_term();
        let kill_observations = kill_terminal.observations();
        let kill_engine = test_builder(kill_terminal, Arc::new(Mutex::new(Vec::new())))
            .natural_exit_grace(Duration::ZERO)
            .termination_grace(Duration::ZERO)
            .build()
            .unwrap();

        let kill = kill_engine.finish().unwrap();

        assert_eq!(kill.termination(), TerminationOutcome::Sigkill);
        assert_eq!(
            *kill_observations.signals.lock().unwrap(),
            [ScriptedSignal::Term, ScriptedSignal::Kill]
        );
        assert_eq!(*kill_observations.closes.lock().unwrap(), 1);
    }

    #[test]
    fn finish_retains_group_identity_after_direct_child_exit() {
        let terminal = ScriptedTerminal::new(
            vec![ScriptedStep {
                expected_input: vec![],
                output_chunks: vec![],
                exit_after: Some(ChildExitStatus::exited(0)),
            }],
            ScriptedEnd::Pending,
        )
        .keep_group_alive_after_exit();
        let observations = terminal.observations();
        let engine = test_builder(terminal, Arc::new(Mutex::new(Vec::new())))
            .natural_exit_grace(Duration::ZERO)
            .termination_grace(Duration::ZERO)
            .build()
            .unwrap();

        let completion = engine.finish().unwrap();

        assert_eq!(completion.status(), &ChildExitStatus::exited(0));
        assert_eq!(completion.termination(), TerminationOutcome::Sigterm);
        assert_eq!(
            *observations.signals.lock().unwrap(),
            [ScriptedSignal::Term]
        );
    }

    #[test]
    fn cleanup_accumulates_reader_join_and_reap_failures() {
        let terminal = ScriptedTerminal::new(vec![], ScriptedEnd::Pending).fail_wait("reap probe");
        let observations = terminal.observations();
        let mut engine = test_builder(terminal, Arc::new(Mutex::new(Vec::new())))
            .termination_grace(Duration::ZERO)
            .build()
            .unwrap();
        if let Some(term) = engine.session_mut().terminal_mut() {
            let _ = term.close();
            term.replace_reader_for_test(thread::spawn(|| panic!("join probe")));
        }

        let error = engine.terminate().unwrap_err();

        assert!(
            error
                .failures()
                .iter()
                .any(|failure| matches!(failure, CleanupFailure::ReaderPanicked))
        );
        assert!(error.failures().iter().any(|failure| {
            matches!(
                failure,
                CleanupFailure::Operation {
                    operation: "reap child",
                    ..
                }
            )
        }));
        assert_eq!(*observations.closes.lock().unwrap(), 2);
    }

    #[test]
    fn execution_error_remains_available_when_explicit_cleanup_also_fails() {
        let terminal = ScriptedTerminal::new(vec![step(b"right", vec![])], ScriptedEnd::Pending)
            .fail_wait("cleanup probe");
        let mut engine = test_builder(terminal, Arc::new(Mutex::new(Vec::new())))
            .termination_grace(Duration::ZERO)
            .build()
            .unwrap();

        let execution = engine.execute(&[Command::Send(b"wrong".to_vec())]);
        let cleanup = engine.terminate();

        assert!(matches!(execution, Err(ExecutionError::Input { .. })));
        assert!(cleanup.is_err());
    }

    #[test]
    fn dropping_an_unfinished_engine_terminates_and_releases_resources() {
        let terminal = ScriptedTerminal::new(vec![], ScriptedEnd::Pending);
        let observations = terminal.observations();
        let engine = test_builder(terminal, Arc::new(Mutex::new(Vec::new())))
            .termination_grace(Duration::ZERO)
            .build()
            .unwrap();

        drop(engine);

        assert_eq!(
            *observations.signals.lock().unwrap(),
            [ScriptedSignal::Term]
        );
        assert_eq!(*observations.closes.lock().unwrap(), 1);
    }

    #[test]
    fn production_pty_round_trips_spawn_configuration_io_resize_and_status() {
        let presentation = Arc::new(Mutex::new(Vec::new()));
        let mut spec = SpawnSpec::new("/bin/sh");
        spec.args = Vec::from([
            "-c".into(),
            concat!(
                "printf 'ready:%s:%s:' \"$TTY_SIM_OVERRIDE\" \"$1\"; ",
                "test -n \"$PATH\" && printf 'inherited:'; ",
                "pwd; IFS= read -r value; stty size; printf 'seen:%s\\n' \"$value\"; sleep 1"
            )
            .into(),
            "tty-sim-test".into(),
            "literal;$(not-executed)".into(),
        ]);
        spec.env = Vec::from([
            EnvironmentChange::Set {
                name: "TTY_SIM_OVERRIDE".into(),
                value: "first".into(),
            },
            EnvironmentChange::Set {
                name: "TTY_SIM_OVERRIDE".into(),
                value: "last".into(),
            },
        ]);
        spec.current_dir = Some(std::env::temp_dir());
        let mut engine = production_builder(spec, Arc::clone(&presentation))
            .build()
            .unwrap();
        let resized = TerminalSize::new(41, 101, 0, 0).unwrap();

        engine.resize(resized).unwrap();
        engine
            .execute(&[
                Command::Expect {
                    pattern: b"ready:last:literal;$(not-executed):inherited:".to_vec(),
                    timeout: Duration::from_secs(2),
                },
                Command::Send(b"value\n".to_vec()),
                Command::Expect {
                    pattern: b"seen:value".to_vec(),
                    timeout: Duration::from_secs(2),
                },
            ])
            .unwrap();
        let completion = engine.wait_for_exit().unwrap();
        let visible = presentation.lock().unwrap().concat();
        let working_directory = std::env::temp_dir().to_string_lossy().into_owned();

        assert_eq!(completion.status(), &ChildExitStatus::exited(0));
        assert_eq!(completion.termination(), TerminationOutcome::Natural);
        assert!(
            visible
                .windows(working_directory.len())
                .any(|bytes| { bytes == working_directory.as_bytes() })
        );
        assert!(
            visible
                .windows(b"41 101".len())
                .any(|bytes| bytes == b"41 101")
        );
    }

    #[test]
    fn production_pty_terminates_the_owned_group_with_term_and_kill() {
        let term = finish_production_shell("trap 'exit 0' TERM");
        assert_eq!(term.termination(), TerminationOutcome::Sigterm);

        let kill = finish_production_shell("trap '' TERM");
        assert_eq!(kill.termination(), TerminationOutcome::Sigkill);
    }

    fn test_builder(
        terminal: ScriptedTerminal,
        presentation: Arc<Mutex<Vec<Vec<u8>>>>,
    ) -> super::EngineBuilder {
        Engine::builder(SpawnSpec::new("unused"))
            .scripted_terminal(terminal)
            .output_callback(move |bytes| {
                presentation
                    .lock()
                    .map_err(|_| io::Error::other("presentation state poisoned"))?
                    .push(bytes.to_vec());
                Ok(())
            })
    }

    fn production_builder(
        spec: SpawnSpec,
        presentation: Arc<Mutex<Vec<Vec<u8>>>>,
    ) -> super::EngineBuilder {
        Engine::builder(spec).output_callback(move |bytes| {
            presentation
                .lock()
                .map_err(|_| io::Error::other("presentation state poisoned"))?
                .push(bytes.to_vec());
            Ok(())
        })
    }

    fn finish_production_shell(signal_setup: &str) -> crate::Completion {
        let presentation = Arc::new(Mutex::new(Vec::new()));
        let mut spec = SpawnSpec::new("/bin/sh");
        spec.args = Vec::from([
            "-c".into(),
            format!("{signal_setup}; printf 'group:%s:ready' \"$$\"; while :; do sleep 1; done"),
        ]);
        let mut engine = production_builder(spec, Arc::clone(&presentation))
            .natural_exit_grace(Duration::ZERO)
            .termination_grace(Duration::from_secs(2))
            .build()
            .unwrap();
        engine
            .execute(&[
                Command::Expect {
                    pattern: b"ready".to_vec(),
                    timeout: Duration::from_secs(2),
                },
                Command::Show(Vec::new()),
            ])
            .unwrap();
        let group = process_group_from_presentation(&presentation.lock().unwrap().concat());
        let completion = engine.finish().unwrap();

        wait_for_process_group_exit(group);
        assert_eq!(Arc::strong_count(&presentation), 1);
        completion
    }

    fn wait_for_process_group_exit(group: i32) {
        use nix::errno::Errno;
        use nix::sys::signal::killpg;
        use nix::unistd::Pid;

        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            match killpg(Pid::from_raw(group), None) {
                Err(Errno::ESRCH) => return,
                Ok(()) | Err(Errno::EPERM) if Instant::now() < deadline => {
                    thread::sleep(Duration::from_millis(10));
                }
                Ok(()) | Err(Errno::EPERM) => {
                    panic!("process group {group} remained after engine completion");
                }
                Err(error) => panic!("failed to inspect process group {group}: {error}"),
            }
        }
    }

    fn process_group_from_presentation(presentation: &[u8]) -> i32 {
        let text = String::from_utf8_lossy(presentation);
        let group = text
            .split("group:")
            .nth(1)
            .and_then(|suffix| suffix.split(':').next())
            .unwrap();
        group.trim().parse().unwrap()
    }

    fn step(expected_input: &[u8], output_chunks: Vec<Vec<u8>>) -> ScriptedStep {
        ScriptedStep {
            expected_input: expected_input.to_vec(),
            output_chunks,
            exit_after: None,
        }
    }

    fn typing_delays(seed: u64) -> Vec<Duration> {
        let terminal = ScriptedTerminal::new(vec![step(b"abcdef", vec![])], ScriptedEnd::Pending);
        let sleeper = RecordingSleeper::default();
        let sleeps = sleeper.observations();
        let mut engine = test_builder(terminal, Arc::new(Mutex::new(Vec::new())))
            .seed(seed)
            .recording_sleeper(sleeper)
            .build()
            .unwrap();
        engine
            .execute(&[
                Command::Typing(
                    TypingRange::new(Duration::from_millis(2), Duration::from_millis(5)).unwrap(),
                ),
                Command::Type("abcdef".into()),
            ])
            .unwrap();
        sleeps.lock().unwrap().clone()
    }
}
