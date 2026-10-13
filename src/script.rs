mod grammar;
mod yaml;

pub(crate) use grammar::{parse_duration_value, parse_key_value};
pub use yaml::parse_script;

use std::fmt;
use std::path::{Path, PathBuf};

use thiserror::Error;

use crate::automation::terminal::{EnvironmentChange, TerminalSizeError};
use crate::automation::{Command, ConfigurationError as AutomationConfigError};
use crate::presentation::{ConfigurationError as PresentationConfigError, MaskRule};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SourceLocation {
    pub line: usize,
    pub column: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ScriptValidationError {
    #[error(transparent)]
    Terminal(#[from] TerminalSizeError),
    #[error(transparent)]
    Automation(#[from] AutomationConfigError),
    #[error(transparent)]
    Presentation(#[from] PresentationConfigError),
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("{message}")]
pub struct YamlError {
    message: String,
}

impl YamlError {
    pub(crate) fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }

    pub fn message(&self) -> &str {
        &self.message
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error(
    "script error at line {}, column {}: {kind}",
    .location.line,
    .location.column
)]
pub struct ParseError {
    pub location: SourceLocation,
    #[source]
    pub kind: ParseErrorKind,
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ParseErrorKind {
    #[error("invalid YAML or script schema")]
    Yaml(#[source] YamlError),
    #[error("invalid duration `{0}`")]
    InvalidDuration(String),
    #[error("invalid key `{0}`")]
    InvalidKey(String),
    #[error("key list must not be empty")]
    EmptyKeyList,
    #[error("hex byte values require an even number of ASCII hexadecimal digits")]
    InvalidHex,
    #[error("{0}")]
    UnsupportedFeature(&'static str),
    #[error(transparent)]
    Validation(#[from] ScriptValidationError),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocatedCommand {
    pub command: Command,
    pub location: SourceLocation,
}

#[derive(Clone, PartialEq, Eq)]
pub struct Script {
    cwd: Option<PathBuf>,
    env: Vec<EnvironmentChange>,
    masks: Vec<MaskRule>,
    steps: Vec<LocatedCommand>,
}

impl Script {
    pub(crate) const fn new(
        cwd: Option<PathBuf>,
        env: Vec<EnvironmentChange>,
        masks: Vec<MaskRule>,
        steps: Vec<LocatedCommand>,
    ) -> Self {
        Self {
            cwd,
            env,
            masks,
            steps,
        }
    }

    pub fn cwd(&self) -> Option<&Path> {
        self.cwd.as_deref()
    }

    pub fn env(&self) -> &[EnvironmentChange] {
        &self.env
    }

    pub fn masks(&self) -> &[MaskRule] {
        &self.masks
    }

    pub fn steps(&self) -> &[LocatedCommand] {
        &self.steps
    }

    pub fn commands(&self) -> Vec<Command> {
        self.steps.iter().map(|step| step.command.clone()).collect()
    }

    /// Returns the source location of the command at the given 1-based command index
    /// reported by an execution error.
    pub fn command_location(&self, index: usize) -> Option<SourceLocation> {
        index
            .checked_sub(1)
            .and_then(|idx| self.steps.get(idx).map(|step| step.location))
    }
}

impl fmt::Debug for Script {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Script")
            .field("cwd", &self.cwd)
            .field("env", &self.env)
            .field("masks", &self.masks)
            .field("steps", &self.steps)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::{
        AutomationConfigError, ParseError, ParseErrorKind, PresentationConfigError,
        ScriptValidationError, SourceLocation, TerminalSizeError,
    };

    #[test]
    fn source_location_creation() {
        let location = SourceLocation {
            line: 12,
            column: 34,
        };
        assert_eq!(location.line, 12);
        assert_eq!(location.column, 34);
    }

    #[test]
    fn parse_error_formatting() {
        let error = ParseError {
            location: SourceLocation {
                line: 5,
                column: 10,
            },
            kind: ParseErrorKind::InvalidDuration("abc".into()),
        };
        let message = error.to_string();
        assert!(message.contains("line 5"));
        assert!(message.contains("column 10"));
        assert!(message.contains("invalid duration"));
    }

    #[test]
    fn script_validation_error_conversion() {
        let terminal_err: ScriptValidationError = TerminalSizeError::ZeroRows.into();
        assert!(matches!(
            terminal_err,
            ScriptValidationError::Terminal(TerminalSizeError::ZeroRows)
        ));

        let auto_err: ScriptValidationError = AutomationConfigError::InvalidTypingDelayRange.into();
        assert!(matches!(
            auto_err,
            ScriptValidationError::Automation(AutomationConfigError::InvalidTypingDelayRange)
        ));

        let pres_err: ScriptValidationError = PresentationConfigError::EmptyMaskPattern.into();
        assert!(matches!(
            pres_err,
            ScriptValidationError::Presentation(PresentationConfigError::EmptyMaskPattern)
        ));
    }

    #[test]
    fn yaml_error_is_project_owned_and_does_not_leak_serde_types() {
        use super::YamlError;
        let yaml_error = YamlError::new("unknown field `foo`");
        assert_eq!(yaml_error.message(), "unknown field `foo`");
        assert_eq!(yaml_error.to_string(), "unknown field `foo`");

        let parse_error = ParseError {
            location: SourceLocation { line: 1, column: 1 },
            kind: ParseErrorKind::Yaml(yaml_error.clone()),
        };
        assert!(parse_error.to_string().contains("line 1, column 1"));
        assert!(matches!(parse_error.kind, ParseErrorKind::Yaml(ref err) if err == &yaml_error));
    }

    #[test]
    fn script_provides_commands_and_location_mapping() {
        use super::parse_script;
        use crate::automation::Command;

        let script =
            parse_script("steps:\n  - send: 'hello'\n  - wait: 100ms\n  - type: 'done'\n").unwrap();

        let commands = script.commands();
        assert_eq!(commands.len(), 3);
        assert_eq!(commands[0], Command::Send(b"hello".to_vec()));
        assert_eq!(commands[2], Command::Type("done".to_string()));

        // 1-based command indexes match CommandContext::Index
        assert_eq!(
            script.command_location(1),
            Some(SourceLocation { line: 2, column: 5 })
        );
        assert_eq!(
            script.command_location(2),
            Some(SourceLocation { line: 3, column: 5 })
        );
        assert_eq!(
            script.command_location(3),
            Some(SourceLocation { line: 4, column: 5 })
        );
        assert_eq!(script.command_location(0), None);
        assert_eq!(script.command_location(4), None);
    }

    #[test]
    fn grammar_parsing_in_script_feature() {
        use super::{parse_duration_value, parse_key_value};
        use std::time::Duration;

        assert_eq!(
            parse_duration_value("250ms"),
            Some(Duration::from_millis(250))
        );
        assert_eq!(
            parse_duration_value("1.5s"),
            Some(Duration::from_millis(1500))
        );
        assert_eq!(parse_duration_value("invalid"), None);

        assert_eq!(parse_key_value("enter"), Some(b"\r".to_vec()));
        assert_eq!(parse_key_value("Ctrl+C"), Some(b"\x03".to_vec()));
        assert_eq!(parse_key_value("invalid"), None);
    }
}
