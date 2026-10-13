pub(crate) mod session;
pub mod terminal;
pub(crate) mod timing;

pub(crate) use session::{ActiveTerminal, PresentationApi, Session, spawn_reader};

#[cfg(test)]
pub(crate) use session::TerminalSession;
#[cfg(test)]
pub(crate) use timing::RecordingSleeper;
pub(crate) use timing::{RandomPolicy, SleepPolicy};

use std::fmt;
use std::io;
use std::time::Duration;

use thiserror::Error;

use crate::presentation::MaskRule;
pub use terminal::{
    ChildExitStatus, DEFAULT_TERMINAL_SIZE, EnvironmentChange, SpawnSpec, TerminalError,
    TerminalSize, TerminalSizeError,
};

pub const DEFAULT_EXPECT_TIMEOUT: Duration = Duration::from_secs(5);
pub const DEFAULT_TYPE_MIN_DELAY: Duration = Duration::from_millis(10);
pub const DEFAULT_TYPE_MAX_DELAY: Duration = Duration::from_millis(200);
pub const DEFAULT_NATURAL_EXIT_GRACE: Duration = Duration::from_secs(2);
pub const DEFAULT_TERMINATION_GRACE: Duration = Duration::from_secs(2);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum ConfigurationError {
    #[error("minimum typing delay exceeds maximum typing delay")]
    InvalidTypingDelayRange,
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum CommandConfigurationError {
    #[error(transparent)]
    Automation(#[from] ConfigurationError),
    #[error(transparent)]
    Presentation(#[from] crate::presentation::ConfigurationError),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TypingRange {
    minimum: Duration,
    maximum: Duration,
}

impl TypingRange {
    /// Creates an inclusive range of per-character typing delays.
    ///
    /// # Errors
    ///
    /// Returns an error when `minimum` exceeds `maximum`.
    pub fn new(minimum: Duration, maximum: Duration) -> Result<Self, ConfigurationError> {
        if minimum > maximum {
            return Err(ConfigurationError::InvalidTypingDelayRange);
        }
        Ok(Self { minimum, maximum })
    }

    pub const fn minimum(self) -> Duration {
        self.minimum
    }

    pub const fn maximum(self) -> Duration {
        self.maximum
    }
}

impl Default for TypingRange {
    fn default() -> Self {
        Self {
            minimum: DEFAULT_TYPE_MIN_DELAY,
            maximum: DEFAULT_TYPE_MAX_DELAY,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandContext {
    Index(usize),
}

impl CommandContext {
    pub const fn index(self) -> usize {
        match self {
            Self::Index(index) => index,
        }
    }
}

impl From<usize> for CommandContext {
    fn from(index: usize) -> Self {
        Self::Index(index)
    }
}

#[derive(Clone, PartialEq, Eq)]
pub enum Command {
    Type(String),
    Typing(TypingRange),
    Key(Vec<u8>),
    Send(Vec<u8>),
    Show(Vec<u8>),
    Wait(Duration),
    Expect { pattern: Vec<u8>, timeout: Duration },
    Mask(MaskRule),
    Simulate { command: String, output: Vec<u8> },
}

impl fmt::Debug for Command {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Type(text) => formatter
                .debug_tuple("Type")
                .field(&format_args!("<redacted: {} bytes>", text.len()))
                .finish(),
            Self::Typing(range) => formatter.debug_tuple("Typing").field(range).finish(),
            Self::Key(bytes) => formatter.debug_tuple("Key").field(bytes).finish(),
            Self::Send(bytes) => redacted_bytes(formatter, "Send", bytes),
            Self::Show(bytes) => redacted_bytes(formatter, "Show", bytes),
            Self::Wait(duration) => formatter.debug_tuple("Wait").field(duration).finish(),
            Self::Expect { pattern, timeout } => formatter
                .debug_struct("Expect")
                .field(
                    "pattern",
                    &format_args!("<redacted: {} bytes>", pattern.len()),
                )
                .field("timeout", timeout)
                .finish(),
            Self::Mask(rule) => formatter.debug_tuple("Mask").field(rule).finish(),
            Self::Simulate { command, output } => formatter
                .debug_struct("Simulate")
                .field(
                    "command",
                    &format_args!("<redacted: {} bytes>", command.len()),
                )
                .field(
                    "output",
                    &format_args!("<redacted: {} bytes>", output.len()),
                )
                .finish(),
        }
    }
}

fn redacted_bytes(formatter: &mut fmt::Formatter<'_>, name: &str, bytes: &[u8]) -> fmt::Result {
    formatter
        .debug_tuple(name)
        .field(&format_args!("<redacted: {} bytes>", bytes.len()))
        .finish()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TerminationOutcome {
    Natural,
    Sigterm,
    Sigkill,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Completion {
    status: ChildExitStatus,
    termination: TerminationOutcome,
}

impl Completion {
    pub const fn new(status: ChildExitStatus, termination: TerminationOutcome) -> Self {
        Self {
            status,
            termination,
        }
    }

    pub const fn status(&self) -> &ChildExitStatus {
        &self.status
    }

    pub const fn termination(&self) -> TerminationOutcome {
        self.termination
    }

    pub const fn required_sigterm(&self) -> bool {
        matches!(
            self.termination,
            TerminationOutcome::Sigterm | TerminationOutcome::Sigkill
        )
    }

    pub const fn required_sigkill(&self) -> bool {
        matches!(self.termination, TerminationOutcome::Sigkill)
    }
}

#[derive(Debug, Error)]
pub enum ExecutionError {
    #[error("invalid command configuration at command {index}", index = .context.index())]
    Configuration {
        context: CommandContext,
        #[source]
        source: CommandConfigurationError,
    },
    #[error("input write failed while executing command {index}", index = .context.index())]
    Input {
        context: CommandContext,
        #[source]
        source: io::Error,
    },
    #[error("output failed while executing command {index}", index = .context.index())]
    Output {
        context: CommandContext,
        #[source]
        source: std::sync::Arc<crate::presentation::OutputFailure>,
    },
    #[error("synchronization failed while executing command {index}", index = .context.index())]
    Synchronization {
        context: CommandContext,
        #[source]
        source: SynchronizationError,
    },
    #[error("expectation timed out while executing command {index}", index = .context.index())]
    ExpectTimeout { context: CommandContext },
    #[error("output ended before the expectation matched while executing command {index}", index = .context.index())]
    ExpectEof { context: CommandContext },
    #[error("child exited before command execution completed at command {index}", index = .context.index())]
    EarlyChildExit {
        context: CommandContext,
        status: ChildExitStatus,
    },
}

impl ExecutionError {
    pub const fn context(&self) -> CommandContext {
        match self {
            Self::Configuration { context, .. }
            | Self::Input { context, .. }
            | Self::Output { context, .. }
            | Self::Synchronization { context, .. }
            | Self::ExpectTimeout { context }
            | Self::ExpectEof { context }
            | Self::EarlyChildExit { context, .. } => *context,
        }
    }
}

#[derive(Debug, Error)]
#[error("synchronization state `{resource}` is poisoned")]
pub struct SynchronizationError {
    pub resource: &'static str,
}

#[derive(Debug, Error)]
#[error("terminal cleanup failed in {} operation(s)", .failures.len())]
pub struct CleanupError {
    failures: Vec<CleanupFailure>,
}

impl CleanupError {
    pub const fn new(failures: Vec<CleanupFailure>) -> Self {
        Self { failures }
    }

    pub fn failures(&self) -> &[CleanupFailure] {
        &self.failures
    }
}

#[derive(Debug, Error)]
pub enum CleanupFailure {
    #[error("cleanup operation `{operation}` failed")]
    Operation {
        operation: &'static str,
        #[source]
        source: io::Error,
    },
    #[error("cleanup deadline for `{operation}` cannot be represented")]
    DeadlineOverflow { operation: &'static str },
    #[error("output failed during cleanup")]
    Output {
        #[source]
        source: std::sync::Arc<crate::presentation::OutputFailure>,
    },
    #[error("synchronization failed during cleanup")]
    Synchronization(#[source] SynchronizationError),
    #[error("reader thread panicked during cleanup")]
    ReaderPanicked,
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::{
        ChildExitStatus, Command, CommandContext, Completion, ConfigurationError,
        TerminationOutcome, TypingRange,
    };

    #[test]
    fn typing_delay_range_invariant() {
        assert_eq!(
            TypingRange::new(Duration::from_millis(50), Duration::from_millis(20)),
            Err(ConfigurationError::InvalidTypingDelayRange)
        );
        let valid = TypingRange::new(Duration::from_millis(10), Duration::from_millis(20)).unwrap();
        assert_eq!(valid.minimum(), Duration::from_millis(10));
        assert_eq!(valid.maximum(), Duration::from_millis(20));
    }

    #[test]
    fn command_debug_redacts_sensitive_payloads() {
        let type_cmd = Command::Type("password123".to_string());
        let send_cmd = Command::Send(b"token_bytes".to_vec());
        let show_cmd = Command::Show(b"visible_bytes".to_vec());
        let expect_cmd = Command::Expect {
            pattern: b"secret_prompt".to_vec(),
            timeout: Duration::from_secs(1),
        };
        let simulate_cmd = Command::Simulate {
            command: "sudo su".to_string(),
            output: b"root# ".to_vec(),
        };

        let type_debug = format!("{type_cmd:?}");
        assert!(!type_debug.contains("password123"));
        assert!(type_debug.contains("<redacted: 11 bytes>"));

        let send_debug = format!("{send_cmd:?}");
        assert!(!send_debug.contains("token_bytes"));
        assert!(send_debug.contains("<redacted: 11 bytes>"));

        let show_debug = format!("{show_cmd:?}");
        assert!(!show_debug.contains("visible_bytes"));
        assert!(show_debug.contains("<redacted: 13 bytes>"));

        let expect_debug = format!("{expect_cmd:?}");
        assert!(!expect_debug.contains("secret_prompt"));
        assert!(expect_debug.contains("<redacted: 13 bytes>"));

        let simulate_debug = format!("{simulate_cmd:?}");
        assert!(!simulate_debug.contains("sudo su"));
        assert!(!simulate_debug.contains("root#"));
        assert!(simulate_debug.contains("<redacted: 7 bytes>"));
        assert!(simulate_debug.contains("<redacted: 6 bytes>"));
    }

    #[test]
    fn completion_reports_each_termination_level() {
        let natural = Completion::new(ChildExitStatus::exited(0), TerminationOutcome::Natural);
        let terminated = Completion::new(ChildExitStatus::exited(143), TerminationOutcome::Sigterm);
        let killed = Completion::new(
            ChildExitStatus::signaled("SIGKILL"),
            TerminationOutcome::Sigkill,
        );

        assert!(!natural.required_sigterm());
        assert!(!natural.required_sigkill());
        assert!(terminated.required_sigterm());
        assert!(!terminated.required_sigkill());
        assert!(killed.required_sigterm());
        assert!(killed.required_sigkill());
    }

    #[test]
    fn command_context_stores_index() {
        let context = CommandContext::Index(42);
        assert_eq!(context.index(), 42);
    }
}
