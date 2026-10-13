use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, RawFd};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use filedescriptor::{FileDescriptor, POLLIN, Pipe, poll, pollfd};
use nix::errno::Errno;
use nix::sys::signal::{Signal, killpg};
use nix::unistd::Pid;
use portable_pty::{CommandBuilder, MasterPty, PtySize, native_pty_system};
use thiserror::Error;

use crate::automation::terminal::{
    ChildExitStatus, ChildHandle, EnvironmentChange, RunningTerminal, SpawnSpec, TerminalBackend,
    TerminalControl, TerminalSize,
};

#[derive(Debug, Default)]
pub struct PtyBackend;

pub struct PtyInput(Box<dyn Write + Send>);

impl Write for PtyInput {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        self.0.write(buffer)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.0.flush()
    }
}

pub struct PtyChild {
    child: SharedChild,
    group: ProcessGroup,
}

impl ChildHandle for PtyChild {
    fn try_wait(&mut self) -> io::Result<Option<ChildExitStatus>> {
        self.child.try_wait()
    }

    fn wait(&mut self) -> io::Result<ChildExitStatus> {
        self.child.wait()
    }

    fn process_group_is_alive(&self) -> io::Result<bool> {
        self.group.is_alive().map_err(io::Error::other)
    }

    fn terminate_group(&mut self) -> io::Result<()> {
        self.group.terminate().map_err(io::Error::other)
    }

    fn kill_group(&mut self) -> io::Result<()> {
        self.group.kill().map_err(io::Error::other)
    }
}

type SharedMaster = Arc<Mutex<Option<Box<dyn MasterPty + Send>>>>;

#[derive(Clone)]
struct SharedChild(Arc<Mutex<ChildState>>);

impl std::fmt::Debug for SharedChild {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("SharedChild")
    }
}

struct ChildState {
    child: Box<dyn portable_pty::Child + Send + Sync>,
    status: Option<ChildExitStatus>,
}

impl SharedChild {
    fn new(child: Box<dyn portable_pty::Child + Send + Sync>) -> Self {
        Self(Arc::new(Mutex::new(ChildState {
            child,
            status: None,
        })))
    }

    fn try_wait(&self) -> io::Result<Option<ChildExitStatus>> {
        let mut state = self
            .0
            .lock()
            .map_err(|_| io::Error::other("PTY child state is poisoned"))?;
        if let Some(status) = &state.status {
            return Ok(Some(status.clone()));
        }
        let status = state.child.try_wait()?.as_ref().map(convert_status);
        state.status.clone_from(&status);
        drop(state);
        Ok(status)
    }

    fn wait(&self) -> io::Result<ChildExitStatus> {
        let mut state = self
            .0
            .lock()
            .map_err(|_| io::Error::other("PTY child state is poisoned"))?;
        if let Some(status) = &state.status {
            return Ok(status.clone());
        }
        let status = convert_status(&state.child.wait()?);
        state.status = Some(status.clone());
        drop(state);
        Ok(status)
    }
}

pub struct PtyControl {
    master: SharedMaster,
    reader: ReaderControl,
}

impl TerminalControl for PtyControl {
    fn resize(&self, size: TerminalSize) -> io::Result<()> {
        let master_guard = self
            .master
            .lock()
            .map_err(|_| io::Error::other("PTY master state is poisoned"))?;
        let master = master_guard
            .as_ref()
            .ok_or_else(|| io::Error::other("PTY master is closed"))?;
        let result = master.resize(pty_size(size)).map_err(io::Error::other);
        drop(master_guard);
        result
    }

    fn close(&self) -> io::Result<()> {
        let reader_result = self.reader.close();
        let master_result = self
            .master
            .lock()
            .map_err(|_| io::Error::other("PTY master state is poisoned"))
            .map(|mut master| {
                *master = None;
            });
        reader_result.and(master_result)
    }
}

impl TerminalBackend for PtyBackend {
    type Input = PtyInput;
    type Output = PtyReader;
    type Child = PtyChild;
    type Control = PtyControl;

    fn spawn(
        &self,
        spec: &SpawnSpec,
    ) -> io::Result<RunningTerminal<Self::Input, Self::Output, Self::Child, Self::Control>> {
        let directory = match &spec.current_dir {
            Some(directory) => directory.clone(),
            None => std::env::current_dir()?,
        };
        if !std::fs::metadata(&directory)?.is_dir() {
            return Err(io::Error::new(
                io::ErrorKind::NotADirectory,
                format!(
                    "child working directory is not a directory: {}",
                    directory.display()
                ),
            ));
        }
        let pair = native_pty_system()
            .openpty(pty_size(spec.size))
            .map_err(io::Error::other)?;
        let mut command = CommandBuilder::new(&spec.program);
        command.args(&spec.args);
        for change in &spec.env {
            match change {
                EnvironmentChange::Set { name, value } => command.env(name, value),
                EnvironmentChange::Remove { name } => command.env_remove(name),
            }
        }
        command.cwd(&directory);
        let mut child = pair
            .slave
            .spawn_command(command)
            .map_err(io::Error::other)?;
        drop(pair.slave);
        let group =
            match ProcessGroup::from_pty(child.process_id(), pair.master.process_group_leader()) {
                Ok(group) => group,
                Err(source) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(io::Error::other(source));
                }
            };
        let input = match pair.master.take_writer() {
            Ok(input) => input,
            Err(source) => {
                cleanup_failed_spawn(&mut child, group);
                return Err(io::Error::other(source));
            }
        };
        let child = SharedChild::new(child);
        let (output, reader) = match PtyReader::new(pair.master.as_ref(), child.clone()) {
            Ok(reader) => reader,
            Err(source) => {
                let _ = group.kill();
                let _ = child.wait();
                return Err(io::Error::other(source));
            }
        };
        let master = Arc::new(Mutex::new(Some(pair.master)));

        Ok(RunningTerminal {
            input: PtyInput(input),
            output,
            child: PtyChild { child, group },
            control: PtyControl { master, reader },
        })
    }
}

fn cleanup_failed_spawn(
    child: &mut Box<dyn portable_pty::Child + Send + Sync>,
    group: ProcessGroup,
) {
    let _ = group.kill();
    let _ = child.kill();
    let _ = child.wait();
}

const fn pty_size(size: TerminalSize) -> PtySize {
    PtySize {
        rows: size.rows(),
        cols: size.cols(),
        pixel_width: size.pixel_width(),
        pixel_height: size.pixel_height(),
    }
}

fn convert_status(status: &portable_pty::ExitStatus) -> ChildExitStatus {
    status.signal().map_or_else(
        || ChildExitStatus::exited(status.exit_code()),
        ChildExitStatus::signaled,
    )
}

#[derive(Debug, Error)]
enum ProcessGroupError {
    #[error("the PTY child has no process identity")]
    MissingChildIdentity,
    #[error("the PTY has no foreground process-group identity")]
    MissingProcessGroupIdentity,
    #[error("child process ID {0} cannot be represented by the POSIX API")]
    ChildIdentityOutOfRange(u32),
    #[error("PTY child {child} is not its process-group leader {group}")]
    UnexpectedProcessGroup { child: i32, group: i32 },
    #[error("failed to signal process group {group}")]
    Signal {
        group: i32,
        #[source]
        source: Errno,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ProcessGroup(Pid);

impl ProcessGroup {
    fn from_pty(
        child_identity: Option<u32>,
        group_identity: Option<i32>,
    ) -> Result<Self, ProcessGroupError> {
        let child_identity = child_identity.ok_or(ProcessGroupError::MissingChildIdentity)?;
        let child = i32::try_from(child_identity)
            .map_err(|_| ProcessGroupError::ChildIdentityOutOfRange(child_identity))?;
        let group = group_identity.ok_or(ProcessGroupError::MissingProcessGroupIdentity)?;

        if child != group {
            return Err(ProcessGroupError::UnexpectedProcessGroup { child, group });
        }

        Ok(Self(Pid::from_raw(group)))
    }

    fn is_alive(self) -> Result<bool, ProcessGroupError> {
        match killpg(self.0, None) {
            Ok(()) | Err(Errno::EPERM) => Ok(true),
            Err(Errno::ESRCH) => Ok(false),
            Err(source) => Err(ProcessGroupError::Signal {
                group: self.0.as_raw(),
                source,
            }),
        }
    }

    fn terminate(self) -> Result<(), ProcessGroupError> {
        self.signal(Signal::SIGTERM)
    }

    fn kill(self) -> Result<(), ProcessGroupError> {
        self.signal(Signal::SIGKILL)
    }

    fn signal(self, signal: Signal) -> Result<(), ProcessGroupError> {
        killpg(self.0, signal).map_err(|source| ProcessGroupError::Signal {
            group: self.0.as_raw(),
            source,
        })
    }
}

#[derive(Debug, Error)]
enum ReaderSetupError {
    #[error("the PTY master has no POSIX file descriptor")]
    MissingMasterDescriptor,
    #[error("failed to duplicate the PTY master descriptor")]
    DuplicateMaster {
        #[source]
        source: filedescriptor::Error,
    },
    #[error("failed to create the PTY reader cancellation channel")]
    CancellationChannel {
        #[source]
        source: filedescriptor::Error,
    },
}

#[derive(Debug)]
struct ReaderControl {
    cancel: FileDescriptor,
    closed: AtomicBool,
}

impl ReaderControl {
    fn close(&self) -> io::Result<()> {
        if self.closed.load(Ordering::Acquire) {
            return Ok(());
        }

        match nix::unistd::write(&self.cancel, &[1]) {
            Ok(_) | Err(Errno::EPIPE) => {
                self.closed.store(true, Ordering::Release);
                Ok(())
            }
            Err(error) => Err(io::Error::from_raw_os_error(error as i32)),
        }
    }
}

#[derive(Debug)]
pub struct PtyReader {
    master: FileDescriptor,
    cancel: FileDescriptor,
    child: SharedChild,
}

impl PtyReader {
    fn new(
        master: &dyn MasterPty,
        child: SharedChild,
    ) -> Result<(Self, ReaderControl), ReaderSetupError> {
        let raw = master
            .as_raw_fd()
            .ok_or(ReaderSetupError::MissingMasterDescriptor)?;
        // `filedescriptor` creates both the duplicate and pipe with CLOEXEC.
        // The reader exclusively owns this master duplicate; `PtyControl` owns
        // the original master, and `PtyInput` owns the dependency's writer.
        let master = FileDescriptor::dup(&RawDescriptor(raw))
            .map_err(|source| ReaderSetupError::DuplicateMaster { source })?;
        let Pipe {
            read: cancel_reader,
            write: cancel,
        } = Pipe::new().map_err(|source| ReaderSetupError::CancellationChannel { source })?;

        Ok((
            Self {
                master,
                cancel: cancel_reader,
                child,
            },
            ReaderControl {
                cancel,
                closed: AtomicBool::new(false),
            },
        ))
    }
}

impl Read for PtyReader {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        let mut descriptors = [
            pollfd {
                fd: self.cancel.as_raw_fd(),
                events: POLLIN,
                revents: 0,
            },
            pollfd {
                fd: self.master.as_raw_fd(),
                events: POLLIN,
                revents: 0,
            },
        ];

        poll(&mut descriptors, None).map_err(io::Error::other)?;
        if descriptors[0].revents != 0 {
            return Ok(0);
        }

        classify_pty_read(self.master.read(buffer), &self.child)
    }
}

fn classify_pty_read(result: io::Result<usize>, child: &SharedChild) -> io::Result<usize> {
    match result {
        Err(source) if is_post_exit_pty_error(&source) => {
            for _ in 0..20 {
                match child.try_wait() {
                    Ok(Some(_)) => return Ok(0),
                    Ok(None) => std::thread::sleep(std::time::Duration::from_millis(1)),
                    Err(_) => return Err(source),
                }
            }
            Err(source)
        }
        result => result,
    }
}

fn is_post_exit_pty_error(error: &io::Error) -> bool {
    cfg!(any(target_os = "linux", target_os = "macos"))
        && error.raw_os_error() == Some(Errno::EIO as i32)
}

struct RawDescriptor(RawFd);

impl AsRawFd for RawDescriptor {
    fn as_raw_fd(&self) -> RawFd {
        self.0
    }
}

#[cfg(test)]
mod tests {
    use std::io::{self, Read, Write};
    use std::sync::mpsc;
    use std::thread;
    use std::time::{Duration, Instant};

    use nix::errno::Errno;
    use portable_pty::{CommandBuilder, PtySize, native_pty_system};

    use super::{ProcessGroup, PtyBackend, PtyReader, SharedChild, classify_pty_read};
    use crate::automation::terminal::{
        ChildHandle, EnvironmentChange, SpawnSpec, TerminalBackend, TerminalControl,
    };

    const PROBE_TIMEOUT: Duration = Duration::from_secs(2);

    #[test]
    fn child_inherits_the_current_directory_unless_one_is_supplied() {
        let inherited = std::env::current_dir().unwrap().canonicalize().unwrap();
        let alternate = if inherited == std::path::Path::new("/") {
            std::env::temp_dir().canonicalize().unwrap()
        } else {
            std::path::PathBuf::from("/")
        };
        for directory in [None, Some(alternate.clone()), Some(".".into())] {
            let expected = directory
                .as_ref()
                .unwrap_or(&inherited)
                .canonicalize()
                .unwrap();
            let mut spec = SpawnSpec::new("/bin/sh");
            spec.args = Vec::from(["-c".into(), "IFS= read -r release; pwd -P".into()]);
            spec.env = Vec::from([EnvironmentChange::Set {
                name: "HOME".into(),
                value: alternate.to_string_lossy().into_owned(),
            }]);
            spec.current_dir = directory;
            let mut running = PtyBackend.spawn(&spec).unwrap();
            running.input.write_all(b"\n").unwrap();
            running.input.flush().unwrap();
            let mut output = String::new();
            running.output.read_to_string(&mut output).unwrap();
            assert!(running.child.wait().unwrap().success());
            running.control.close().unwrap();
            assert_eq!(output.trim(), expected.to_str().unwrap());
        }
    }

    #[test]
    fn invalid_child_directory_fails_before_spawning() {
        for (directory, kind) in [
            (
                std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml"),
                io::ErrorKind::NotADirectory,
            ),
            (
                std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml/absent"),
                io::ErrorKind::NotADirectory,
            ),
        ] {
            let mut spec = SpawnSpec::new("/not/a/tty-sim-program");
            spec.current_dir = Some(directory);
            assert_eq!(PtyBackend.spawn(&spec).err().unwrap().kind(), kind);
        }
    }

    #[test]
    fn portable_pty_creates_an_owned_group_that_sigterm_can_end() {
        let (child, group, master) = spawn_shell("sleep 60 & wait");

        group.terminate().unwrap();
        let status = child.wait().unwrap();

        assert!(!status.success());
        wait_until_group_exits(group);
        drop(master);
    }

    #[test]
    fn owned_group_can_escalate_from_sigterm_to_sigkill() {
        let script = "trap '' TERM; (trap '' TERM; while :; do sleep 1; done) & wait";
        let (child, group, master) = spawn_shell(script);

        group.terminate().unwrap();
        thread::sleep(Duration::from_millis(50));
        assert!(group.is_alive().unwrap());
        group.kill().unwrap();
        let status = child.wait().unwrap();

        assert!(!status.success());
        wait_until_group_exits(group);
        drop(master);
    }

    #[test]
    fn group_identity_remains_usable_after_direct_child_exit() {
        let script = "trap '' HUP; (trap '' HUP; sleep 60) &";
        let (child, group, master) = spawn_shell(script);

        let status = child.wait().unwrap();

        assert!(status.success());
        assert!(group.is_alive().unwrap());
        group.kill().unwrap();
        wait_until_group_exits(group);
        drop(master);
    }

    #[test]
    fn reader_control_unblocks_a_blocking_raw_pty_read() {
        let (child, group, master) = spawn_shell("sleep 60");
        let (mut reader, control) = PtyReader::new(master.as_ref(), child.clone()).unwrap();
        let (result_sender, result_receiver) = mpsc::channel();
        let reader_thread = thread::spawn(move || {
            loop {
                match reader.read(&mut [0; 32]) {
                    Ok(0) => {
                        result_sender.send(Ok(0)).unwrap();
                        break;
                    }
                    Ok(_) => {}
                    Err(error) => {
                        result_sender.send(Err(error)).unwrap();
                        break;
                    }
                }
            }
        });

        control.close().unwrap();
        control.close().unwrap();
        let read_result = result_receiver.recv_timeout(PROBE_TIMEOUT).unwrap();

        assert_eq!(read_result.unwrap(), 0);
        reader_thread.join().unwrap();
        group.kill().unwrap();
        child.wait().unwrap();
        wait_until_group_exits(group);
        drop(master);
    }

    #[test]
    fn master_supports_separate_io_handles_and_resize() {
        let pair = native_pty_system().openpty(PtySize::default()).unwrap();
        let mut command = CommandBuilder::new("sh");
        command.args(["-c", "read value; printf 'seen:%s\\n' \"$value\""]);
        let child = pair.slave.spawn_command(command).unwrap();
        let child = SharedChild::new(child);
        drop(pair.slave);
        let mut writer = pair.master.take_writer().unwrap();
        let (mut reader, control) = PtyReader::new(pair.master.as_ref(), child.clone()).unwrap();
        let resized = PtySize {
            rows: 40,
            cols: 100,
            pixel_width: 0,
            pixel_height: 0,
        };

        pair.master.resize(resized).unwrap();
        writer.write_all(b"value\n").unwrap();
        let output = read_until(&mut reader, b"seen:value");
        let status = child.wait().unwrap();

        assert!(status.success());
        assert!(
            output
                .windows(b"seen:value".len())
                .any(|part| part == b"seen:value")
        );
        assert_eq!(pair.master.get_size().unwrap(), resized);
        control.close().unwrap();
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn reader_reaches_eof_after_proving_child_exit() {
        let (child, group, master) = spawn_shell("exit 0");
        let (mut reader, _control) = PtyReader::new(master.as_ref(), child.clone()).unwrap();

        loop {
            match reader.read(&mut [0; 64]) {
                Ok(0) => break,
                Ok(_) => {}
                Err(error) => panic!("post-exit PTY error was not classified as EOF: {error}"),
            }
        }
        let status = child.wait().unwrap();

        assert!(status.success());
        assert!(!group.is_alive().unwrap());
        drop(master);
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn eio_requires_child_exit_proof_and_other_errors_remain_failures() {
        let (child, group, master) = spawn_shell("sleep 60");
        let eio = || io::Error::from_raw_os_error(Errno::EIO as i32);

        let running_error = classify_pty_read(Err(eio()), &child).unwrap_err();
        assert_eq!(running_error.raw_os_error(), Some(Errno::EIO as i32));

        group.kill().unwrap();
        child.wait().unwrap();
        assert_eq!(classify_pty_read(Err(eio()), &child).unwrap(), 0);
        let other = classify_pty_read(
            Err(io::Error::from_raw_os_error(Errno::EBADF as i32)),
            &child,
        )
        .unwrap_err();
        assert_eq!(other.raw_os_error(), Some(Errno::EBADF as i32));
        wait_until_group_exits(group);
        drop(master);
    }

    fn spawn_shell(
        script: &str,
    ) -> (
        SharedChild,
        ProcessGroup,
        Box<dyn portable_pty::MasterPty + Send>,
    ) {
        let pair = native_pty_system().openpty(PtySize::default()).unwrap();
        let mut command = CommandBuilder::new("sh");
        command.args(["-c", &format!("read _tty_sim_start; {script}")]);
        let child = pair.slave.spawn_command(command).unwrap();
        drop(pair.slave);
        let group =
            ProcessGroup::from_pty(child.process_id(), pair.master.process_group_leader()).unwrap();
        let child = SharedChild::new(child);
        let mut writer = pair.master.take_writer().unwrap();
        writer.write_all(b"start\n").unwrap();
        writer.flush().unwrap();
        drop(writer);

        (child, group, pair.master)
    }

    fn wait_until_group_exits(group: ProcessGroup) {
        let deadline = Instant::now() + PROBE_TIMEOUT;
        while group.is_alive().unwrap() {
            assert!(Instant::now() < deadline, "process group did not exit");
            thread::sleep(Duration::from_millis(10));
        }
    }

    fn read_until(reader: &mut PtyReader, marker: &[u8]) -> Vec<u8> {
        let mut output = Vec::new();
        let mut chunk = [0; 64];
        while !output.windows(marker.len()).any(|part| part == marker) {
            let read = reader.read(&mut chunk).unwrap();
            assert_ne!(read, 0, "PTY reached EOF before marker");
            output.extend_from_slice(&chunk[..read]);
        }
        output
    }
}
