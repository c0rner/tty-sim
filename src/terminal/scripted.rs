use std::collections::VecDeque;
use std::io::{self, Read, Write};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};

use thiserror::Error;

use crate::automation::terminal::{
    ChildExitStatus, ChildHandle, RunningTerminal, SpawnSpec, TerminalBackend, TerminalControl,
    TerminalSize,
};

#[derive(Debug, Clone)]
pub struct ScriptedStep {
    pub expected_input: Vec<u8>,
    pub output_chunks: Vec<Vec<u8>>,
    pub exit_after: Option<ChildExitStatus>,
}

#[derive(Debug, Clone)]
pub enum ScriptedEnd {
    Pending,
    Eof,
    ReaderFailure(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScriptedSignal {
    Term,
    Kill,
}

#[derive(Debug, Clone, Default)]
pub struct ScriptedObservations {
    pub input: Arc<Mutex<Vec<u8>>>,
    pub flushes: Arc<Mutex<usize>>,
    pub sizes: Arc<Mutex<Vec<TerminalSize>>>,
    pub signals: Arc<Mutex<Vec<ScriptedSignal>>>,
    pub closes: Arc<Mutex<usize>>,
}

pub struct ScriptedTerminal {
    steps: Vec<ScriptedStep>,
    end: ScriptedEnd,
    terminate_on_term: bool,
    keep_group_alive_after_exit: bool,
    wait_failure: Option<String>,
    observations: ScriptedObservations,
}

impl ScriptedTerminal {
    pub fn new(steps: Vec<ScriptedStep>, end: ScriptedEnd) -> Self {
        Self {
            steps,
            end,
            terminate_on_term: true,
            keep_group_alive_after_exit: false,
            wait_failure: None,
            observations: ScriptedObservations::default(),
        }
    }

    pub fn ignore_term(mut self) -> Self {
        self.terminate_on_term = false;
        self
    }

    pub fn keep_group_alive_after_exit(mut self) -> Self {
        self.keep_group_alive_after_exit = true;
        self
    }

    pub fn fail_wait(mut self, message: impl Into<String>) -> Self {
        self.wait_failure = Some(message.into());
        self
    }

    pub fn observations(&self) -> ScriptedObservations {
        self.observations.clone()
    }
}

impl TerminalBackend for ScriptedTerminal {
    type Input = ScriptedInput;
    type Output = ScriptedOutput;
    type Child = ScriptedChild;
    type Control = ScriptedControl;

    fn spawn(
        &self,
        _spec: &SpawnSpec,
    ) -> io::Result<RunningTerminal<Self::Input, Self::Output, Self::Child, Self::Control>> {
        let (events, output) = mpsc::channel();
        let lifecycle = Arc::new(Lifecycle {
            state: Mutex::new(LifecycleState {
                status: None,
                group_alive: true,
                terminate_on_term: self.terminate_on_term,
                keep_group_alive_after_exit: self.keep_group_alive_after_exit,
                wait_failure: self.wait_failure.clone(),
            }),
            changed: Condvar::new(),
        });
        let shared = Arc::new(Mutex::new(ScriptState {
            steps: self.steps.clone().into(),
            step_number: 1,
            step_offset: 0,
            end: self.end.clone(),
            ended: false,
        }));
        let input = ScriptedInput {
            shared: Arc::clone(&shared),
            events: events.clone(),
            lifecycle: Arc::clone(&lifecycle),
            observations: self.observations.clone(),
        };
        input.release_completed_steps()?;

        Ok(RunningTerminal {
            input,
            output: ScriptedOutput {
                events: output,
                pending: Vec::new(),
            },
            child: ScriptedChild {
                lifecycle: Arc::clone(&lifecycle),
                observations: self.observations.clone(),
            },
            control: ScriptedControl {
                events,
                lifecycle,
                observations: self.observations.clone(),
            },
        })
    }
}

enum OutputEvent {
    Chunk(Vec<u8>),
    Eof,
    Failure(String),
}

struct ScriptState {
    steps: VecDeque<ScriptedStep>,
    step_number: usize,
    step_offset: usize,
    end: ScriptedEnd,
    ended: bool,
}

#[derive(Debug, Error)]
enum ScriptedInputError {
    #[error(
        "scripted input mismatch at step {step}, byte offset {offset}: expected {expected:#04x}, received {actual:#04x}"
    )]
    Mismatch {
        step: usize,
        offset: usize,
        expected: u8,
        actual: u8,
    },
    #[error("unexpected scripted input at step {step}, byte offset 0: received {actual:#04x}")]
    Unexpected { step: usize, actual: u8 },
    #[error("scripted terminal state is poisoned")]
    Poisoned,
    #[error("scripted output reader is no longer available")]
    ReaderGone,
}

pub struct ScriptedInput {
    shared: Arc<Mutex<ScriptState>>,
    events: Sender<OutputEvent>,
    lifecycle: Arc<Lifecycle>,
    observations: ScriptedObservations,
}

impl ScriptedInput {
    fn release_completed_steps(&self) -> io::Result<()> {
        let mut state = lock(&self.shared)?;
        loop {
            let complete = state
                .steps
                .front()
                .is_some_and(|step| state.step_offset == step.expected_input.len());
            if !complete {
                break;
            }
            let step = state.steps.pop_front().ok_or_else(poisoned)?;
            state.step_offset = 0;
            state.step_number += 1;
            for chunk in step.output_chunks {
                self.events
                    .send(OutputEvent::Chunk(chunk))
                    .map_err(|_| io::Error::other(ScriptedInputError::ReaderGone))?;
            }
            if let Some(status) = step.exit_after {
                set_status(&self.lifecycle, status)?;
            }
        }
        if state.steps.is_empty() && !state.ended {
            match &state.end {
                ScriptedEnd::Pending => {}
                ScriptedEnd::Eof => self
                    .events
                    .send(OutputEvent::Eof)
                    .map_err(|_| io::Error::other(ScriptedInputError::ReaderGone))?,
                ScriptedEnd::ReaderFailure(message) => self
                    .events
                    .send(OutputEvent::Failure(message.clone()))
                    .map_err(|_| io::Error::other(ScriptedInputError::ReaderGone))?,
            }
            state.ended = !matches!(state.end, ScriptedEnd::Pending);
        }
        drop(state);
        Ok(())
    }
}

impl Write for ScriptedInput {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        for &actual in buffer {
            let mut state = lock(&self.shared)?;
            let Some(step) = state.steps.front() else {
                return Err(io::Error::other(ScriptedInputError::Unexpected {
                    step: state.step_number,
                    actual,
                }));
            };
            let expected = step.expected_input[state.step_offset];
            if actual != expected {
                return Err(io::Error::other(ScriptedInputError::Mismatch {
                    step: state.step_number,
                    offset: state.step_offset,
                    expected,
                    actual,
                }));
            }
            state.step_offset += 1;
            drop(state);
            self.release_completed_steps()?;
        }
        lock(&self.observations.input)?.extend_from_slice(buffer);
        Ok(buffer.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        let mut flushes = lock(&self.observations.flushes)?;
        *flushes += 1;
        drop(flushes);
        Ok(())
    }
}

pub struct ScriptedOutput {
    events: Receiver<OutputEvent>,
    pending: Vec<u8>,
}

impl Read for ScriptedOutput {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if buffer.is_empty() {
            return Ok(0);
        }
        if self.pending.is_empty() {
            match self.events.recv() {
                Ok(OutputEvent::Chunk(chunk)) => self.pending = chunk,
                Ok(OutputEvent::Eof) | Err(_) => return Ok(0),
                Ok(OutputEvent::Failure(message)) => return Err(io::Error::other(message)),
            }
        }
        let copied = buffer.len().min(self.pending.len());
        buffer[..copied].copy_from_slice(&self.pending[..copied]);
        self.pending.drain(..copied);
        Ok(copied)
    }
}

struct Lifecycle {
    state: Mutex<LifecycleState>,
    changed: Condvar,
}

struct LifecycleState {
    status: Option<ChildExitStatus>,
    group_alive: bool,
    terminate_on_term: bool,
    keep_group_alive_after_exit: bool,
    wait_failure: Option<String>,
}

pub struct ScriptedChild {
    lifecycle: Arc<Lifecycle>,
    observations: ScriptedObservations,
}

impl ChildHandle for ScriptedChild {
    fn try_wait(&mut self) -> io::Result<Option<ChildExitStatus>> {
        let state = lock(&self.lifecycle.state)?;
        state.wait_failure.as_ref().map_or_else(
            || Ok(state.status.clone()),
            |message| Err(io::Error::other(message.clone())),
        )
    }

    fn wait(&mut self) -> io::Result<ChildExitStatus> {
        let mut state = lock(&self.lifecycle.state)?;
        if let Some(message) = &state.wait_failure {
            return Err(io::Error::other(message.clone()));
        }
        while state.status.is_none() {
            state = self.lifecycle.changed.wait(state).map_err(|_| poisoned())?;
        }
        state.status.clone().ok_or_else(poisoned)
    }

    fn process_group_is_alive(&self) -> io::Result<bool> {
        Ok(lock(&self.lifecycle.state)?.group_alive)
    }

    fn terminate_group(&mut self) -> io::Result<()> {
        lock(&self.observations.signals)?.push(ScriptedSignal::Term);
        let terminate_on_term = lock(&self.lifecycle.state)?.terminate_on_term;
        if terminate_on_term {
            set_stopped(&self.lifecycle, ChildExitStatus::signaled("SIGTERM"))?;
        }
        Ok(())
    }

    fn kill_group(&mut self) -> io::Result<()> {
        lock(&self.observations.signals)?.push(ScriptedSignal::Kill);
        set_stopped(&self.lifecycle, ChildExitStatus::signaled("SIGKILL"))
    }
}

pub struct ScriptedControl {
    events: Sender<OutputEvent>,
    lifecycle: Arc<Lifecycle>,
    observations: ScriptedObservations,
}

impl TerminalControl for ScriptedControl {
    fn resize(&self, size: TerminalSize) -> io::Result<()> {
        lock(&self.observations.sizes)?.push(size);
        Ok(())
    }

    fn close(&self) -> io::Result<()> {
        let mut closes = lock(&self.observations.closes)?;
        *closes += 1;
        drop(closes);
        let _ = self.events.send(OutputEvent::Eof);
        let mut state = lock(&self.lifecycle.state)?;
        state.group_alive = false;
        drop(state);
        Ok(())
    }
}

fn set_status(lifecycle: &Lifecycle, status: ChildExitStatus) -> io::Result<()> {
    let mut state = lock(&lifecycle.state)?;
    state.status = Some(status);
    if !state.keep_group_alive_after_exit {
        state.group_alive = false;
    }
    drop(state);
    lifecycle.changed.notify_all();
    Ok(())
}

fn set_stopped(lifecycle: &Lifecycle, status: ChildExitStatus) -> io::Result<()> {
    let mut state = lock(&lifecycle.state)?;
    state.status = Some(status);
    state.group_alive = false;
    drop(state);
    lifecycle.changed.notify_all();
    Ok(())
}

fn lock<T>(mutex: &Mutex<T>) -> io::Result<MutexGuard<'_, T>> {
    mutex.lock().map_err(|_| poisoned())
}

fn poisoned() -> io::Error {
    io::Error::other(ScriptedInputError::Poisoned)
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};

    use super::{ScriptedEnd, ScriptedSignal, ScriptedStep, ScriptedTerminal};
    use crate::automation::terminal::{
        ChildExitStatus, ChildHandle, SpawnSpec, TerminalBackend, TerminalControl, TerminalSize,
    };

    #[test]
    fn partial_and_multiple_writes_release_exact_chunks_in_order() {
        let terminal = ScriptedTerminal::new(
            vec![
                step(b"abc", vec![vec![0xff], b"two".to_vec()]),
                step(b"d", vec![b"last".to_vec()]),
            ],
            ScriptedEnd::Eof,
        );
        let mut running = terminal.spawn(&SpawnSpec::new("unused")).unwrap();

        running.input.write_all(b"a").unwrap();
        running.input.write_all(b"bcd").unwrap();
        let mut first = [0; 1];
        let mut second = [0; 3];
        running.output.read_exact(&mut first).unwrap();
        running.output.read_exact(&mut second).unwrap();
        let mut last = [0; 4];
        running.output.read_exact(&mut last).unwrap();

        assert_eq!(first, [0xff]);
        assert_eq!(&second, b"two");
        assert_eq!(&last, b"last");
        assert_eq!(running.output.read(&mut [0; 1]).unwrap(), 0);
    }

    #[test]
    fn mismatch_reports_the_step_and_byte_offset() {
        let terminal = ScriptedTerminal::new(vec![step(b"abc", vec![])], ScriptedEnd::Pending);
        let mut running = terminal.spawn(&SpawnSpec::new("unused")).unwrap();

        running.input.write_all(b"a").unwrap();
        let error = running.input.write_all(b"x").unwrap_err();

        assert!(error.to_string().contains("step 1, byte offset 1"));
        assert!(error.to_string().contains("0x62"));
        assert!(error.to_string().contains("0x78"));
    }

    #[test]
    fn reader_failure_exit_resize_and_signals_are_explicit() {
        let terminal = ScriptedTerminal::new(
            vec![ScriptedStep {
                expected_input: vec![],
                output_chunks: vec![],
                exit_after: Some(ChildExitStatus::exited(3)),
            }],
            ScriptedEnd::ReaderFailure("read probe".into()),
        )
        .ignore_term()
        .keep_group_alive_after_exit();
        let observations = terminal.observations();
        let mut running = terminal.spawn(&SpawnSpec::new("unused")).unwrap();

        assert_eq!(
            running.child.try_wait().unwrap(),
            Some(ChildExitStatus::exited(3))
        );
        assert_eq!(
            running.output.read(&mut [0; 1]).unwrap_err().to_string(),
            "read probe"
        );
        let size = TerminalSize::new(40, 100, 0, 0).unwrap();
        running.control.resize(size).unwrap();
        running.child.terminate_group().unwrap();
        assert!(running.child.process_group_is_alive().unwrap());
        running.child.kill_group().unwrap();
        running.control.close().unwrap();

        assert_eq!(*observations.sizes.lock().unwrap(), [size]);
        assert_eq!(
            *observations.signals.lock().unwrap(),
            [ScriptedSignal::Term, ScriptedSignal::Kill]
        );
        assert!(!running.child.process_group_is_alive().unwrap());
    }

    fn step(expected_input: &[u8], output_chunks: Vec<Vec<u8>>) -> ScriptedStep {
        ScriptedStep {
            expected_input: expected_input.to_vec(),
            output_chunks,
            exit_after: None,
        }
    }
}
