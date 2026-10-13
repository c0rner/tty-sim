use std::fmt;
use std::io::{self, Read, Write};
use std::path::PathBuf;

use thiserror::Error;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum TerminalSizeError {
    #[error("terminal rows must be non-zero")]
    ZeroRows,
    #[error("terminal columns must be non-zero")]
    ZeroColumns,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TerminalSize {
    rows: u16,
    cols: u16,
    pixel_width: u16,
    pixel_height: u16,
}

impl TerminalSize {
    /// Creates a terminal size.
    ///
    /// # Errors
    ///
    /// Returns an error when either the row or column count is zero.
    pub const fn new(
        rows: u16,
        cols: u16,
        pixel_width: u16,
        pixel_height: u16,
    ) -> Result<Self, TerminalSizeError> {
        if rows == 0 {
            return Err(TerminalSizeError::ZeroRows);
        }
        if cols == 0 {
            return Err(TerminalSizeError::ZeroColumns);
        }

        Ok(Self {
            rows,
            cols,
            pixel_width,
            pixel_height,
        })
    }

    pub const fn rows(self) -> u16 {
        self.rows
    }

    pub const fn cols(self) -> u16 {
        self.cols
    }

    pub const fn pixel_width(self) -> u16 {
        self.pixel_width
    }

    pub const fn pixel_height(self) -> u16 {
        self.pixel_height
    }
}

pub const DEFAULT_TERMINAL_SIZE: TerminalSize = TerminalSize {
    rows: 24,
    cols: 80,
    pixel_width: 0,
    pixel_height: 0,
};

impl Default for TerminalSize {
    fn default() -> Self {
        DEFAULT_TERMINAL_SIZE
    }
}

#[derive(Clone, PartialEq, Eq)]
pub enum EnvironmentChange {
    Set { name: String, value: String },
    Remove { name: String },
}

impl fmt::Debug for EnvironmentChange {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Set { name, value } => formatter
                .debug_struct("Set")
                .field("name", name)
                .field("value", &format_args!("<redacted: {} bytes>", value.len()))
                .finish(),
            Self::Remove { name } => formatter
                .debug_struct("Remove")
                .field("name", name)
                .finish(),
        }
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct SpawnSpec {
    pub program: String,
    pub args: Vec<String>,
    pub env: Vec<EnvironmentChange>,
    /// Child working directory; `None` inherits tty-sim's current working directory.
    pub current_dir: Option<PathBuf>,
    pub size: TerminalSize,
}

impl fmt::Debug for SpawnSpec {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SpawnSpec")
            .field("program", &self.program)
            .field("args", &self.args)
            .field("env", &self.env)
            .field("current_dir", &self.current_dir)
            .field("size", &self.size)
            .finish()
    }
}

impl SpawnSpec {
    pub fn new(program: impl Into<String>) -> Self {
        Self {
            program: program.into(),
            args: Vec::new(),
            env: Vec::new(),
            current_dir: None,
            size: DEFAULT_TERMINAL_SIZE,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChildExitStatus {
    code: Option<u32>,
    signal: Option<String>,
}

impl ChildExitStatus {
    pub const fn exited(code: u32) -> Self {
        Self {
            code: Some(code),
            signal: None,
        }
    }

    pub fn signaled(signal: impl Into<String>) -> Self {
        Self {
            code: None,
            signal: Some(signal.into()),
        }
    }

    pub const fn code(&self) -> Option<u32> {
        self.code
    }

    pub fn signal(&self) -> Option<&str> {
        self.signal.as_deref()
    }

    pub const fn success(&self) -> bool {
        matches!(self.code, Some(0))
    }
}

#[derive(Debug, Error)]
pub enum TerminalError {
    #[error("failed to spawn terminal child")]
    Spawn(#[source] io::Error),
    #[error("terminal operation `{operation}` failed")]
    Control {
        operation: &'static str,
        #[source]
        source: io::Error,
    },
}

/// Contract for spawning pseudo-terminal sessions.
pub(crate) trait TerminalBackend: Send + Sync {
    type Input: Write + Send;
    type Output: Read + Send;
    type Child: ChildHandle;
    type Control: TerminalControl;

    /// Spawns a new terminal session configured by `spec`.
    ///
    /// # Errors
    ///
    /// Returns an [`io::Error`] if process creation or pseudo-terminal setup fails.
    #[allow(clippy::type_complexity)]
    fn spawn(
        &self,
        spec: &SpawnSpec,
    ) -> io::Result<RunningTerminal<Self::Input, Self::Output, Self::Child, Self::Control>>;
}

/// Connected I/O handles and process controls for an active terminal session.
pub(crate) struct RunningTerminal<I, O, C, T> {
    pub input: I,
    pub output: O,
    pub child: C,
    pub control: T,
}

/// Handle for querying and terminating a running child process group.
pub(crate) trait ChildHandle: Send {
    /// Queries the child process exit status without blocking.
    ///
    /// # Errors
    ///
    /// Returns an [`io::Error`] if querying the child process state fails.
    fn try_wait(&mut self) -> io::Result<Option<ChildExitStatus>>;

    /// Waits for the child process to exit.
    ///
    /// # Errors
    ///
    /// Returns an [`io::Error`] if waiting on the child process fails.
    fn wait(&mut self) -> io::Result<ChildExitStatus>;

    /// Checks if any processes in the child's process group remain alive.
    ///
    /// # Errors
    ///
    /// Returns an [`io::Error`] if inspecting the process group fails.
    fn process_group_is_alive(&self) -> io::Result<bool>;

    /// Sends a termination signal to the child process group.
    ///
    /// # Errors
    ///
    /// Returns an [`io::Error`] if signaling the process group fails.
    fn terminate_group(&mut self) -> io::Result<()>;

    /// Sends a kill signal to the child process group.
    ///
    /// # Errors
    ///
    /// Returns an [`io::Error`] if signaling the process group fails.
    fn kill_group(&mut self) -> io::Result<()>;
}

/// Control handle for resizing and closing a running terminal.
pub(crate) trait TerminalControl: Send + Sync {
    /// Resizes the pseudo-terminal window to `size`.
    ///
    /// # Errors
    ///
    /// Returns an [`io::Error`] if the window size ioctl fails.
    fn resize(&self, size: TerminalSize) -> io::Result<()>;

    /// Closes the master terminal handle.
    ///
    /// # Errors
    ///
    /// Returns an [`io::Error`] if closing the handle fails.
    fn close(&self) -> io::Result<()>;
}

#[cfg(test)]
mod tests {
    use std::error::Error as _;

    use super::{
        ChildExitStatus, EnvironmentChange, SpawnSpec, TerminalError, TerminalSize,
        TerminalSizeError,
    };

    #[test]
    fn terminal_rows_and_columns_must_be_non_zero() {
        assert_eq!(
            TerminalSize::new(0, 80, 0, 0),
            Err(TerminalSizeError::ZeroRows)
        );
        assert_eq!(
            TerminalSize::new(24, 0, 0, 0),
            Err(TerminalSizeError::ZeroColumns)
        );
    }

    #[test]
    fn zero_pixel_dimensions_are_valid() {
        let size = TerminalSize::new(24, 80, 0, 0).unwrap();

        assert_eq!(size.pixel_width(), 0);
        assert_eq!(size.pixel_height(), 0);
    }

    #[test]
    fn env_change_redacts_secret_value() {
        let change = EnvironmentChange::Set {
            name: "TOKEN".to_string(),
            value: "super_secret".to_string(),
        };
        let formatted = format!("{change:?}");

        assert!(formatted.contains("TOKEN"));
        assert!(!formatted.contains("super_secret"));
        assert!(formatted.contains("<redacted: 12 bytes>"));
    }

    #[test]
    fn spawn_spec_defaults() {
        let spec = SpawnSpec::new("/bin/sh");

        assert_eq!(spec.program, "/bin/sh");
        assert!(spec.args.is_empty());
        assert!(spec.env.is_empty());
        assert_eq!(spec.current_dir, None);
        assert_eq!(spec.size, super::DEFAULT_TERMINAL_SIZE);
    }

    #[test]
    fn child_exit_status_success_invariant() {
        let success = ChildExitStatus::exited(0);
        let failure = ChildExitStatus::exited(1);
        let signaled = ChildExitStatus::signaled("SIGTERM");

        assert!(success.success());
        assert!(!failure.success());
        assert!(!signaled.success());
        assert_eq!(success.code(), Some(0));
        assert_eq!(failure.code(), Some(1));
        assert_eq!(signaled.signal(), Some("SIGTERM"));
    }

    #[test]
    fn terminal_errors_retain_their_io_source() {
        let error = TerminalError::Spawn(std::io::Error::other("probe"));

        assert_eq!(error.source().unwrap().to_string(), "probe");
    }
}
