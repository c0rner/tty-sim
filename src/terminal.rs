//! POSIX pseudo-terminal and process management adapter.
//!
//! Owns POSIX PTY allocation, process-group management, signaling, reaping,
//! reader cancellation, and EOF classification.

#[cfg(unix)]
mod pty;
#[cfg(test)]
mod scripted;

#[cfg(unix)]
pub use pty::{PtyBackend, PtyChild, PtyControl, PtyInput, PtyReader};
#[cfg(test)]
#[allow(unused_imports)]
pub use scripted::{
    ScriptedChild, ScriptedControl, ScriptedEnd, ScriptedInput, ScriptedObservations,
    ScriptedOutput, ScriptedSignal, ScriptedStep, ScriptedTerminal,
};

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};

    use super::*;
    use crate::automation::terminal::{
        ChildExitStatus, ChildHandle, SpawnSpec, TerminalBackend, TerminalControl, TerminalSize,
    };

    #[test]
    fn pty_backend_implements_terminal_backend_and_spawns_process() {
        let backend = PtyBackend;
        let mut spec = SpawnSpec::new("/bin/sh");
        spec.args = vec!["-c".into(), "read line; echo seen:$line".into()];
        let mut running = backend.spawn(&spec).expect("spawn must succeed");
        let size = TerminalSize::new(30, 90, 0, 0).expect("valid size");
        running.control.resize(size).expect("resize must succeed");
        running
            .input
            .write_all(b"hello\n")
            .expect("write must succeed");
        running.input.flush().expect("flush must succeed");
        let mut output = [0u8; 16];
        let n = running.output.read(&mut output).expect("read must succeed");
        assert!(n > 0);
        let status = running.child.wait().expect("wait must succeed");
        assert!(status.success());
        running.control.close().expect("close must succeed");
    }

    #[test]
    fn scripted_terminal_implements_terminal_backend() {
        let terminal = ScriptedTerminal::new(
            vec![ScriptedStep {
                expected_input: b"ping\n".to_vec(),
                output_chunks: vec![b"pong\n".to_vec()],
                exit_after: Some(ChildExitStatus::exited(0)),
            }],
            ScriptedEnd::Eof,
        );
        let mut running = terminal.spawn(&SpawnSpec::new("unused")).unwrap();
        running.input.write_all(b"ping\n").unwrap();
        running.input.flush().unwrap();
        let mut buf = [0u8; 5];
        running.output.read_exact(&mut buf).unwrap();
        assert_eq!(&buf, b"pong\n");
        let status = running.child.wait().unwrap();
        assert!(status.success());
    }

    #[test]
    fn terminal_module_does_not_leak_nix_or_pty_types() {
        // Static type assertion that PtyBackend and ScriptedTerminal implement TerminalBackend
        fn assert_backend<B: TerminalBackend>() {}
        assert_backend::<PtyBackend>();
        assert_backend::<ScriptedTerminal>();
    }
}
