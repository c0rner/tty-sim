pub(crate) mod coordinator;
pub(crate) mod masker;
pub(crate) mod protocol;
pub(crate) mod sink;

use std::fmt;
use std::io;
use std::sync::Arc;

use thiserror::Error;

pub(crate) use coordinator::OutputCoordinator;
pub use sink::{CallbackSink, StdoutSink};
pub(crate) use sink::{OutputSink, SinkAdapter};

pub const DEFAULT_HISTORY_CAPACITY: usize = 64 * 1024;
pub const MAX_HISTORY_CAPACITY: usize = 16 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ConfigurationError {
    #[error("mask search value must not be empty")]
    EmptyMaskPattern,
    #[error("history capacity {capacity} exceeds maximum {maximum}")]
    HistoryCapacityTooLarge { capacity: usize, maximum: usize },
    #[error("expect pattern must not be empty")]
    EmptyExpectPattern,
    #[error("expect pattern length {pattern} exceeds history capacity {capacity}")]
    ExpectPatternTooLarge { pattern: usize, capacity: usize },
}

#[derive(Clone, PartialEq, Eq)]
pub struct MaskRule {
    from: Vec<u8>,
    to: Vec<u8>,
}

impl MaskRule {
    /// Creates a presentation mask rule.
    ///
    /// # Errors
    ///
    /// Returns an error when `from` is empty.
    pub fn new(from: Vec<u8>, to: Vec<u8>) -> Result<Self, ConfigurationError> {
        if from.is_empty() {
            return Err(ConfigurationError::EmptyMaskPattern);
        }
        Ok(Self { from, to })
    }

    /// Creates a rule whose replacement contains one `*` per source byte.
    ///
    /// # Errors
    ///
    /// Returns an error when `from` is empty.
    pub fn automatic(from: Vec<u8>) -> Result<Self, ConfigurationError> {
        let to = vec![b'*'; from.len()];
        Self::new(from, to)
    }

    pub fn from(&self) -> &[u8] {
        &self.from
    }

    pub fn to(&self) -> &[u8] {
        &self.to
    }
}

impl fmt::Debug for MaskRule {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("MaskRule")
            .field(
                "from",
                &format_args!("<redacted: {} bytes>", self.from.len()),
            )
            .field("to", &format_args!("<redacted: {} bytes>", self.to.len()))
            .finish()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HistoryCapacity(usize);

impl HistoryCapacity {
    /// Creates a bounded output-history capacity.
    ///
    /// # Errors
    ///
    /// Returns an error when `capacity` exceeds [`MAX_HISTORY_CAPACITY`].
    pub const fn new(capacity: usize) -> Result<Self, ConfigurationError> {
        if capacity > MAX_HISTORY_CAPACITY {
            return Err(ConfigurationError::HistoryCapacityTooLarge {
                capacity,
                maximum: MAX_HISTORY_CAPACITY,
            });
        }
        Ok(Self(capacity))
    }

    pub const fn get(self) -> usize {
        self.0
    }
}

impl Default for HistoryCapacity {
    fn default() -> Self {
        Self(DEFAULT_HISTORY_CAPACITY)
    }
}

#[derive(Debug, Error)]
pub enum OutputFailure {
    #[error("PTY output reader failed")]
    Reader(#[source] io::Error),
    #[error(transparent)]
    Sink(#[from] SinkError),
}

#[derive(Debug, Error)]
#[error("presentation sink failed")]
pub struct SinkError {
    #[source]
    pub source: io::Error,
}

impl From<io::Error> for SinkError {
    fn from(source: io::Error) -> Self {
        Self { source }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
#[error("synchronization state `{resource}` is poisoned")]
pub struct SynchronizationError {
    pub resource: &'static str,
}

#[derive(Debug, Error)]
pub enum PresentationError {
    #[error(transparent)]
    Configuration(#[from] ConfigurationError),
    #[error("output failed")]
    Output {
        #[source]
        source: Arc<OutputFailure>,
    },
    #[error("sink failed and its failure could not be recorded: {failure}")]
    FailureRecording {
        failure: Arc<OutputFailure>,
        #[source]
        state: Box<Self>,
    },
    #[error("presentation output is already finished")]
    Finished,
    #[error("raw output stream offset overflowed")]
    OffsetOverflow,
    #[error(transparent)]
    Synchronization(#[from] SynchronizationError),
}

#[derive(Debug, Error)]
pub enum ExpectationError {
    #[error(transparent)]
    Configuration(#[from] ConfigurationError),
    #[error("another expectation is already active")]
    AlreadyActive,
    #[error("expectation deadline cannot be represented")]
    DeadlineOverflow,
    #[error("expectation timed out")]
    Timeout,
    #[error("output ended before the expectation matched")]
    Eof,
    #[error("output failed before the expectation matched")]
    Output {
        #[source]
        source: Arc<OutputFailure>,
    },
    #[error("raw output stream offset overflowed")]
    OffsetOverflow,
    #[error(transparent)]
    Synchronization(#[from] SynchronizationError),
}

#[cfg(test)]
mod tests {
    use super::{ConfigurationError, HistoryCapacity, MAX_HISTORY_CAPACITY, MaskRule};

    #[test]
    fn mask_rule_empty_search_rejected() {
        assert_eq!(
            MaskRule::new(vec![], vec![b'*']),
            Err(ConfigurationError::EmptyMaskPattern)
        );
        assert_eq!(
            MaskRule::automatic(vec![]),
            Err(ConfigurationError::EmptyMaskPattern)
        );
    }

    #[test]
    fn mask_rule_automatic_replacement_has_matching_length() {
        let rule = MaskRule::automatic(b"secret".to_vec()).unwrap();
        assert_eq!(rule.from(), b"secret");
        assert_eq!(rule.to(), b"******");
    }

    #[test]
    fn mask_rule_debug_redacts_patterns() {
        let rule = MaskRule::new(b"secret".to_vec(), b"hidden".to_vec()).unwrap();
        let formatted = format!("{rule:?}");
        assert!(!formatted.contains("secret"));
        assert!(!formatted.contains("hidden"));
        assert!(formatted.contains("<redacted: 6 bytes>"));
    }

    #[test]
    fn history_capacity_boundary_validation() {
        assert_eq!(
            HistoryCapacity::new(MAX_HISTORY_CAPACITY),
            Ok(HistoryCapacity(MAX_HISTORY_CAPACITY))
        );
        assert_eq!(
            HistoryCapacity::new(MAX_HISTORY_CAPACITY + 1),
            Err(ConfigurationError::HistoryCapacityTooLarge {
                capacity: MAX_HISTORY_CAPACITY + 1,
                maximum: MAX_HISTORY_CAPACITY,
            })
        );
    }
}
