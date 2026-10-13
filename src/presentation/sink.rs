use std::fmt;
use std::io::{self, Write};

use super::SinkError;

pub trait OutputSink: Send + Sync {
    /// Emits presentation bytes to the underlying sink.
    ///
    /// # Errors
    ///
    /// Returns a [`SinkError`] if writing to the sink fails.
    fn emit(&self, bytes: &[u8]) -> Result<(), SinkError>;

    /// Flushes buffered presentation bytes.
    ///
    /// # Errors
    ///
    /// Returns a [`SinkError`] if flushing fails.
    fn flush(&self) -> Result<(), SinkError>;
}

#[derive(Debug, Default, Clone, Copy)]
pub struct StdoutSink;

impl OutputSink for StdoutSink {
    fn emit(&self, bytes: &[u8]) -> Result<(), SinkError> {
        io::stdout()
            .lock()
            .write_all(bytes)
            .map_err(SinkError::from)
    }

    fn flush(&self) -> Result<(), SinkError> {
        io::stdout().lock().flush().map_err(SinkError::from)
    }
}

type OutputCallback = dyn Fn(&[u8]) -> Result<(), SinkError> + Send + Sync + 'static;

pub struct CallbackSink {
    callback: Box<OutputCallback>,
}

impl CallbackSink {
    pub fn new<F>(callback: F) -> Self
    where
        F: Fn(&[u8]) -> Result<(), SinkError> + Send + Sync + 'static,
    {
        Self {
            callback: Box::new(callback),
        }
    }

    pub fn from_io<F>(callback: F) -> Self
    where
        F: Fn(&[u8]) -> io::Result<()> + Send + Sync + 'static,
    {
        Self::new(move |bytes| callback(bytes).map_err(SinkError::from))
    }
}

impl fmt::Debug for CallbackSink {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CallbackSink")
            .finish_non_exhaustive()
    }
}

impl OutputSink for CallbackSink {
    fn emit(&self, bytes: &[u8]) -> Result<(), SinkError> {
        (self.callback)(bytes)
    }

    fn flush(&self) -> Result<(), SinkError> {
        Ok(())
    }
}

#[derive(Debug)]
pub enum SinkAdapter {
    Stdout(StdoutSink),
    Callback(CallbackSink),
}

impl OutputSink for SinkAdapter {
    fn emit(&self, bytes: &[u8]) -> Result<(), SinkError> {
        match self {
            Self::Stdout(sink) => sink.emit(bytes),
            Self::Callback(sink) => sink.emit(bytes),
        }
    }

    fn flush(&self) -> Result<(), SinkError> {
        match self {
            Self::Stdout(sink) => sink.flush(),
            Self::Callback(sink) => sink.flush(),
        }
    }
}
