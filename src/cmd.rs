//! Command-line driving adapter and executable composition.
//!
//! Owns argument parsing via `argh`, logging setup, working directory
//! inspection, production engine composition, diagnostic reporting, and
//! CLI exit code policy.

use std::error::Error;
use std::ffi::OsString;
use std::fmt;
use std::fs;
use std::io;
use std::io::Write as _;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use argh::FromArgs;
use rand::random;
use tracing::debug;
use tracing_subscriber::EnvFilter;

use crate::automation::terminal::{
    ChildExitStatus, DEFAULT_TERMINAL_SIZE, SpawnSpec, TerminalError, TerminalSize,
    TerminalSizeError,
};
use crate::automation::{
    CleanupError, Completion, DEFAULT_NATURAL_EXIT_GRACE, DEFAULT_TERMINATION_GRACE,
    ExecutionError, TerminationOutcome,
};
use crate::presentation::OutputFailure;
use crate::script::{ParseError, SourceLocation, parse_duration_value, parse_script};
use crate::{Engine, EngineBuildError};

const EXIT_SUCCESS: u8 = 0;
const EXIT_INVOCATION: u8 = 1;
const EXIT_SCRIPT: u8 = 2;
const EXIT_SPAWN: u8 = 3;
const EXIT_EXECUTION: u8 = 4;
const EXIT_READER: u8 = 5;
const EXIT_SINK: u8 = 6;
const EXIT_CLEANUP: u8 = 7;
const EXIT_SIGTERM: u8 = 8;
const EXIT_SIGKILL: u8 = 9;
const EXIT_UNREPRESENTABLE_CHILD: u8 = 10;
const CHILD_EXIT_OFFSET: u32 = 100;
const MAX_MAPPABLE_CHILD_STATUS: u32 = 155;

/// Drive a program through a pseudo-terminal using a tty-sim script.
#[derive(Debug, FromArgs)]
pub(crate) struct Arguments {
    /// UTF-8 tty-sim script path
    #[argh(option)]
    script: PathBuf,

    /// program to execute directly, without a shell
    #[argh(option)]
    command: String,

    /// terminal rows (default: 24)
    #[argh(option, default = "DEFAULT_TERMINAL_SIZE.rows()")]
    rows: u16,

    /// terminal columns (default: 80)
    #[argh(option, default = "DEFAULT_TERMINAL_SIZE.cols()")]
    cols: u16,

    /// random typing seed
    #[argh(option)]
    seed: Option<u64>,

    /// natural-exit grace, as milliseconds or seconds (default: 2s)
    #[argh(
        option,
        default = "DEFAULT_NATURAL_EXIT_GRACE",
        from_str_fn(parse_cli_duration)
    )]
    grace: Duration,

    /// SIGTERM grace, as milliseconds or seconds (default: 2s)
    #[argh(
        option,
        default = "DEFAULT_TERMINATION_GRACE",
        from_str_fn(parse_cli_duration)
    )]
    termination_grace: Duration,

    /// arguments passed literally to the program; precede them with `--`
    #[argh(positional, greedy, arg_name = "ARG")]
    child_args: Vec<String>,
}

fn parse_cli_duration(value: &str) -> Result<Duration, String> {
    parse_duration_value(value)
        .ok_or_else(|| "expected a duration such as `250ms` or `1.5s`".into())
}

/// Runs the process CLI and returns its stable exit status.
pub fn main_entry() -> ExitCode {
    let raw = std::env::args_os().collect::<Vec<_>>();
    let executable = raw
        .first()
        .and_then(|value| value.to_str())
        .unwrap_or("tty-sim")
        .to_owned();
    let raw_arguments = match utf8_arguments(raw.into_iter().skip(1)) {
        Ok(arguments) => arguments,
        Err(failure) => {
            failure.report();
            return ExitCode::from(failure.code());
        }
    };
    let arguments = match Arguments::from_args(
        &[&executable],
        &raw_arguments.iter().map(String::as_str).collect::<Vec<_>>(),
    ) {
        Ok(arguments) => arguments,
        Err(early) => {
            if early.status.is_ok() {
                write_stdout(format_args!("{}", early.output));
                return ExitCode::SUCCESS;
            }
            write_stderr(format_args!("{}", early.output));
            return ExitCode::from(EXIT_INVOCATION);
        }
    };

    match initialize_logging().and_then(|()| run(arguments)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(failure) => {
            failure.report();
            ExitCode::from(failure.code())
        }
    }
}

fn utf8_arguments(arguments: impl Iterator<Item = OsString>) -> Result<Vec<String>, CliFailure> {
    arguments
        .map(|argument| {
            argument
                .into_string()
                .map_err(|_| CliFailure::Invocation(InvocationError::NonUtf8Arguments))
        })
        .collect()
}

fn initialize_logging() -> Result<(), CliFailure> {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("warn"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .try_init()
        .map_err(|source| CliFailure::Invocation(InvocationError::Logging(source)))
}

fn run(arguments: Arguments) -> Result<(), CliFailure> {
    let size = TerminalSize::new(arguments.rows, arguments.cols, 0, 0)
        .map_err(|source| CliFailure::Invocation(InvocationError::TerminalSize(source)))?;
    let source =
        fs::read_to_string(&arguments.script).map_err(|source| CliFailure::ScriptRead {
            path: arguments.script.clone(),
            source,
        })?;
    let script = parse_script(&source).map_err(|source| CliFailure::ScriptParse {
        path: arguments.script.clone(),
        source,
    })?;
    debug!(
        steps = script.steps().len(),
        environment_changes = script.env().len(),
        initial_masks = script.masks().len(),
        configured_working_directory = script.cwd().is_some(),
        "parsed YAML script"
    );
    let current_dir = resolve_working_directory(script.cwd()).map_err(|source| {
        CliFailure::ScriptConfiguration {
            path: arguments.script,
            source,
        }
    })?;

    let mut spec = SpawnSpec::new(arguments.command);
    spec.args = arguments.child_args;
    spec.env = script.env().to_vec();
    spec.current_dir = current_dir;
    spec.size = size;
    let seed = arguments.seed.unwrap_or_else(random);
    debug!(seed, "selected random typing seed");
    let mut engine = Engine::builder(spec)
        .initial_masks(script.masks().to_vec())
        .terminal_size(size)
        .seed(seed)
        .natural_exit_grace(arguments.grace)
        .termination_grace(arguments.termination_grace)
        .build()
        .map_err(map_build_error)?;

    let commands = script.commands();
    if let Err(primary) = engine.execute(&commands) {
        let cleanup = engine.terminate().err();
        let location = script.command_location(primary.context().index());
        return Err(CliFailure::Execution {
            primary,
            location,
            cleanup,
        });
    }

    engine
        .finish()
        .map_err(CliFailure::Cleanup)
        .and_then(classify_completion)
}

fn resolve_working_directory(
    directory: Option<&std::path::Path>,
) -> Result<Option<PathBuf>, ScriptConfigurationError> {
    let Some(directory) = directory else {
        return Ok(None);
    };
    let resolved = if directory.is_absolute() {
        directory.to_path_buf()
    } else {
        std::env::current_dir()
            .map_err(ScriptConfigurationError::ResolveBase)?
            .join(directory)
    };
    let metadata = fs::metadata(&resolved).map_err(|source| ScriptConfigurationError::Inspect {
        path: resolved.clone(),
        source,
    })?;
    if !metadata.is_dir() {
        return Err(ScriptConfigurationError::NotDirectory(resolved));
    }
    Ok(Some(resolved))
}

fn map_build_error(error: EngineBuildError) -> CliFailure {
    match error {
        EngineBuildError::Configuration(source) => {
            CliFailure::Invocation(InvocationError::EngineConfiguration(source))
        }
        EngineBuildError::Terminal(source) => CliFailure::Spawn(source),
    }
}

fn classify_completion(completion: Completion) -> Result<(), CliFailure> {
    match completion.termination() {
        TerminationOutcome::Sigterm | TerminationOutcome::Sigkill => {
            Err(CliFailure::ForcedTermination(completion))
        }
        TerminationOutcome::Natural if completion.status().success() => Ok(()),
        TerminationOutcome::Natural => Err(CliFailure::Child(completion.status().clone())),
    }
}

enum CliFailure {
    Invocation(InvocationError),
    ScriptRead {
        path: PathBuf,
        source: io::Error,
    },
    ScriptParse {
        path: PathBuf,
        source: ParseError,
    },
    ScriptConfiguration {
        path: PathBuf,
        source: ScriptConfigurationError,
    },
    Spawn(TerminalError),
    Execution {
        primary: ExecutionError,
        location: Option<SourceLocation>,
        cleanup: Option<CleanupError>,
    },
    Cleanup(CleanupError),
    ForcedTermination(Completion),
    Child(ChildExitStatus),
}

#[derive(Debug, thiserror::Error)]
enum InvocationError {
    #[error("command-line arguments must be valid UTF-8")]
    NonUtf8Arguments,
    #[error("invalid terminal size")]
    TerminalSize(#[source] TerminalSizeError),
    #[error("invalid engine configuration")]
    EngineConfiguration(#[source] crate::presentation::ConfigurationError),
    #[error("could not initialize diagnostics")]
    Logging(#[source] Box<dyn Error + Send + Sync>),
}

#[derive(Debug, thiserror::Error)]
enum ScriptConfigurationError {
    #[error("could not determine the current execution directory")]
    ResolveBase(#[source] io::Error),
    #[error("could not inspect configured working directory {}", .path.display())]
    Inspect {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("configured working directory {} is not a directory", .0.display())]
    NotDirectory(PathBuf),
}

impl CliFailure {
    fn code(&self) -> u8 {
        match self {
            Self::Invocation(_) => EXIT_INVOCATION,
            Self::ScriptRead { .. }
            | Self::ScriptParse { .. }
            | Self::ScriptConfiguration { .. } => EXIT_SCRIPT,
            Self::Spawn(_) => EXIT_SPAWN,
            Self::Execution { primary, .. } => execution_code(primary),
            Self::Cleanup(_) => EXIT_CLEANUP,
            Self::ForcedTermination(completion) => match completion.termination() {
                TerminationOutcome::Sigterm => EXIT_SIGTERM,
                TerminationOutcome::Sigkill => EXIT_SIGKILL,
                TerminationOutcome::Natural => EXIT_CLEANUP,
            },
            Self::Child(status) => child_status_code(status),
        }
    }

    fn report(&self) {
        match self {
            Self::Invocation(error) => report_error("invocation error", error),
            Self::ScriptRead { path, source } => {
                write_stderr(format_args!(
                    "tty-sim: script error for {}: {source}\n",
                    path.display()
                ));
                report_sources(source);
            }
            Self::ScriptParse { path, source } => {
                write_stderr(format_args!(
                    "tty-sim: script error for {}: {source}\n",
                    path.display()
                ));
                report_sources(source);
            }
            Self::ScriptConfiguration { path, source } => {
                write_stderr(format_args!(
                    "tty-sim: script error for {}: {source}\n",
                    path.display()
                ));
                report_sources(source);
            }
            Self::Spawn(error) => report_error("spawn error", error),
            Self::Execution {
                primary,
                location,
                cleanup,
            } => {
                if let Some(location) = location {
                    write_stderr(format_args!(
                        "tty-sim: execution error at line {}, column {}: {primary}\n",
                        location.line, location.column
                    ));
                } else {
                    report_error("execution error", primary);
                }
                report_sources(primary);
                if let Some(cleanup) = cleanup {
                    report_cleanup("cleanup after execution error", cleanup);
                }
            }
            Self::Cleanup(error) => report_cleanup("cleanup error", error),
            Self::ForcedTermination(completion) => write_stderr(format_args!(
                "tty-sim: forced termination: {}; child {}\n",
                termination_name(completion.termination()),
                DisplayStatus(completion.status())
            )),
            Self::Child(status) => write_stderr(format_args!(
                "tty-sim: child failure: {}\n",
                DisplayStatus(status)
            )),
        }
    }
}

fn execution_code(error: &ExecutionError) -> u8 {
    match error {
        ExecutionError::Output { source, .. } => match source.as_ref() {
            OutputFailure::Reader(_) => EXIT_READER,
            OutputFailure::Sink(_) => EXIT_SINK,
        },
        ExecutionError::EarlyChildExit { status, .. } if !status.success() => {
            child_status_code(status)
        }
        ExecutionError::Configuration { .. }
        | ExecutionError::Input { .. }
        | ExecutionError::Synchronization { .. }
        | ExecutionError::ExpectTimeout { .. }
        | ExecutionError::ExpectEof { .. }
        | ExecutionError::EarlyChildExit { .. } => EXIT_EXECUTION,
    }
}

fn child_status_code(status: &ChildExitStatus) -> u8 {
    match status.code() {
        Some(0) => EXIT_SUCCESS,
        Some(code @ 1..=MAX_MAPPABLE_CHILD_STATUS) => {
            u8::try_from(CHILD_EXIT_OFFSET + code).unwrap_or(EXIT_UNREPRESENTABLE_CHILD)
        }
        Some(_) | None => EXIT_UNREPRESENTABLE_CHILD,
    }
}

const fn termination_name(outcome: TerminationOutcome) -> &'static str {
    match outcome {
        TerminationOutcome::Natural => "natural exit",
        TerminationOutcome::Sigterm => "SIGTERM",
        TerminationOutcome::Sigkill => "SIGKILL",
    }
}

struct DisplayStatus<'a>(&'a ChildExitStatus);

impl fmt::Display for DisplayStatus<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match (self.0.code(), self.0.signal()) {
            (Some(code), signal) => write!(
                formatter,
                "exited with status {code}{}",
                signal.map_or_else(String::new, |signal| format!(" ({signal})"))
            ),
            (None, Some(signal)) => write!(formatter, "terminated by {signal}"),
            (None, None) => formatter.write_str("terminated without a numeric exit status"),
        }
    }
}

fn report_error(category: &str, error: &dyn Error) {
    write_stderr(format_args!("tty-sim: {category}: {error}\n"));
    report_sources(error);
}

fn report_cleanup(category: &str, error: &CleanupError) {
    write_stderr(format_args!("tty-sim: {category}: {error}\n"));
    for failure in error.failures() {
        write_stderr(format_args!("tty-sim: cleanup detail: {failure}\n"));
        report_sources(failure);
    }
}

fn report_sources(error: &dyn Error) {
    let mut source = error.source();
    while let Some(next) = source {
        write_stderr(format_args!("tty-sim: caused by: {next}\n"));
        source = next.source();
    }
}

fn write_stdout(arguments: fmt::Arguments<'_>) {
    let _ = io::stdout().lock().write_fmt(arguments);
}

fn write_stderr(arguments: fmt::Arguments<'_>) {
    let _ = io::stderr().lock().write_fmt(arguments);
}

#[cfg(test)]
mod tests {
    use std::io;

    use super::*;
    use crate::automation::{CleanupFailure, CommandContext};
    use crate::presentation::SinkError;

    fn parse(arguments: &[&str]) -> Result<Arguments, argh::EarlyExit> {
        Arguments::from_args(&["tty-sim"], arguments)
    }

    #[test]
    fn required_arguments_defaults_and_literal_program_arguments_are_stable() {
        assert!(parse(&[]).is_err());
        assert!(parse(&["--script", "demo.yaml"]).is_err());

        let parsed = parse(&[
            "--script",
            "demo.yaml",
            "--command",
            "printf",
            "--",
            "$HOME",
            "a;b",
        ])
        .unwrap();

        assert_eq!(parsed.rows, 24);
        assert_eq!(parsed.cols, 80);
        assert_eq!(parsed.grace, Duration::from_secs(2));
        assert_eq!(parsed.termination_grace, Duration::from_secs(2));
        assert_eq!(parsed.child_args, ["$HOME", "a;b"]);
    }

    #[test]
    fn malformed_cli_values_are_rejected() {
        for arguments in [
            vec!["--script", "x", "--command", "x", "--rows", "no"],
            vec!["--script", "x", "--command", "x", "--seed", "-1"],
            vec!["--script", "x", "--command", "x", "--grace", "1e3s"],
        ] {
            assert!(parse(&arguments).is_err());
        }
        assert!(TerminalSize::new(0, 80, 0, 0).is_err());
        assert!(TerminalSize::new(24, 0, 0, 0).is_err());
    }

    #[test]
    fn child_exit_mapping_covers_every_boundary() {
        assert!(
            classify_completion(Completion::new(
                ChildExitStatus::exited(0),
                TerminationOutcome::Natural
            ))
            .is_ok()
        );
        assert_eq!(child_status_code(&ChildExitStatus::exited(0)), 0);
        assert_eq!(child_status_code(&ChildExitStatus::exited(1)), 101);
        assert_eq!(child_status_code(&ChildExitStatus::exited(155)), 255);
        assert_eq!(child_status_code(&ChildExitStatus::exited(156)), 10);
        assert_eq!(child_status_code(&ChildExitStatus::exited(255)), 10);
        assert_eq!(child_status_code(&ChildExitStatus::signaled("SIGSEGV")), 10);
    }

    #[test]
    fn application_categories_and_precedence_are_stable() {
        let context = CommandContext::Index(1);
        let reader = ExecutionError::Output {
            context,
            source: std::sync::Arc::new(OutputFailure::Reader(io::Error::other("read"))),
        };
        let sink = ExecutionError::Output {
            context,
            source: std::sync::Arc::new(OutputFailure::Sink(SinkError {
                source: io::Error::other("sink"),
            })),
        };
        let timeout = ExecutionError::ExpectTimeout { context };
        let child = ExecutionError::EarlyChildExit {
            context,
            status: ChildExitStatus::exited(2),
        };

        assert_eq!(execution_code(&reader), 5);
        assert_eq!(execution_code(&sink), 6);
        assert_eq!(execution_code(&timeout), 4);
        assert_eq!(execution_code(&child), 102);
        assert_eq!(
            CliFailure::Execution {
                primary: timeout,
                location: None,
                cleanup: Some(CleanupError::new(vec![CleanupFailure::ReaderPanicked])),
            }
            .code(),
            4
        );
        assert_eq!(
            CliFailure::Cleanup(CleanupError::new(vec![CleanupFailure::ReaderPanicked])).code(),
            7
        );
    }

    #[test]
    fn forced_termination_mapping_is_stable() {
        let term = Completion::new(ChildExitStatus::exited(143), TerminationOutcome::Sigterm);
        let kill = Completion::new(
            ChildExitStatus::signaled("SIGKILL"),
            TerminationOutcome::Sigkill,
        );

        assert_eq!(CliFailure::ForcedTermination(term).code(), 8);
        assert_eq!(CliFailure::ForcedTermination(kill).code(), 9);
    }
}
