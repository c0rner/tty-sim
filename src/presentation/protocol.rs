use tracing::debug;

const MAX_SEQUENCE_BYTES: usize = 64 * 1024;

#[derive(Debug, Default, Clone, Copy)]
enum State {
    #[default]
    Ground,
    Escape {
        intermediate: bool,
    },
    Csi,
    String {
        osc: bool,
        escape: bool,
    },
}

/// Filters reply-producing queries without interpreting screen contents.
#[derive(Debug, Default)]
pub struct TerminalProtocol {
    state: State,
    pending: Vec<u8>,
    oversized: bool,
}

#[derive(Debug, Default, Eq, PartialEq)]
pub struct ProtocolOutput {
    pub presentation: Vec<u8>,
    pub responses: Vec<u8>,
}

impl TerminalProtocol {
    pub fn process(&mut self, bytes: &[u8]) -> ProtocolOutput {
        let mut output = ProtocolOutput::default();
        for &byte in bytes {
            if matches!(self.state, State::Ground) {
                if byte == 0x1b {
                    self.state = State::Escape {
                        intermediate: false,
                    };
                    self.pending.push(byte);
                } else {
                    output.presentation.push(byte);
                }
                continue;
            }
            if self.pending.len() == MAX_SEQUENCE_BYTES {
                self.pending.clear();
                self.oversized = true;
                debug!("suppressing oversized terminal control sequence");
            }
            if !self.oversized {
                self.pending.push(byte);
            }
            match self.state {
                State::Ground => unreachable!(),
                State::Escape { intermediate } => match byte {
                    0x1b => self.restart(),
                    0x18 | 0x1a => self.discard(),
                    b'[' if !intermediate => self.state = State::Csi,
                    b']' | b'P' | b'_' | b'^' | b'X' if !intermediate => {
                        self.state = State::String {
                            osc: byte == b']',
                            escape: false,
                        };
                    }
                    0x20..=0x2f => self.state = State::Escape { intermediate: true },
                    _ => self.complete(&mut output),
                },
                State::Csi => {
                    if (0x40..=0x7e).contains(&byte) {
                        self.complete(&mut output);
                    } else if byte == 0x1b {
                        self.restart();
                    } else if matches!(byte, 0x18 | 0x1a) {
                        self.discard();
                    }
                }
                State::String { osc, escape } => {
                    if (escape && byte == b'\\') || (osc && byte == 7) {
                        self.complete(&mut output);
                    } else if matches!(byte, 0x18 | 0x1a) {
                        self.discard();
                    } else {
                        self.state = State::String {
                            osc,
                            escape: byte == 0x1b,
                        };
                    }
                }
            }
        }
        output
    }

    /// Incomplete controls must not leave the viewer's parser awaiting a terminator.
    pub fn finish(&mut self) -> Vec<u8> {
        self.discard();
        Vec::new()
    }

    fn discard(&mut self) {
        self.pending.clear();
        self.oversized = false;
        self.state = State::Ground;
    }

    fn restart(&mut self) {
        self.discard();
        self.pending.push(0x1b);
        self.state = State::Escape {
            intermediate: false,
        };
    }

    fn complete(&mut self, output: &mut ProtocolOutput) {
        if !self.oversized {
            match classify(&self.pending) {
                Action::Forward => output.presentation.extend_from_slice(&self.pending),
                Action::Replace(bytes) => output.presentation.extend(bytes),
                Action::Respond(response) => output.responses.extend_from_slice(response),
                Action::Suppress => debug!("suppressing unsupported terminal query"),
            }
        }
        self.discard();
    }
}

enum Action {
    Forward,
    Replace(Vec<u8>),
    Respond(&'static [u8]),
    Suppress,
}

fn classify(sequence: &[u8]) -> Action {
    if let Some(filtered) = filter_focus_modes(sequence) {
        return Action::Replace(filtered);
    }
    match sequence {
        b"\x1b[6n" => Action::Respond(b"\x1b[1;1R"),
        // A conservative VT100 identity without optional capabilities.
        b"\x1b[c" | b"\x1b[0c" | b"\x1bZ" => Action::Respond(b"\x1b[?1;0c"),
        b"\x1b[>c" | b"\x1b[>0c" => Action::Respond(b"\x1b[>0;0;0c"),
        _ => classify_query(sequence),
    }
}

/// Focus events belong to the outer terminal, whose input is not routed to the
/// child. Filter set/reset and save/restore so the child cannot enable reporting
/// indirectly through a saved mode. Preserve the other parameters verbatim.
fn filter_focus_modes(sequence: &[u8]) -> Option<Vec<u8>> {
    let body = sequence.strip_prefix(b"\x1b[?")?;
    let (&final_byte, parameters) = body.split_last()?;
    if !matches!(final_byte, b'h' | b'l' | b's' | b'r')
        || !parameters
            .iter()
            .all(|byte| byte.is_ascii_digit() || *byte == b';')
        || !parameters.split(|&byte| byte == b';').any(is_focus_mode)
    {
        return None;
    }
    let mut filtered = Vec::new();
    for mode in parameters
        .split(|&byte| byte == b';')
        .filter(|mode| !is_focus_mode(mode))
    {
        if filtered.is_empty() {
            filtered.extend_from_slice(b"\x1b[?");
        } else {
            filtered.push(b';');
        }
        filtered.extend_from_slice(mode);
    }
    if !filtered.is_empty() {
        filtered.push(final_byte);
    }
    debug!("suppressing child focus-reporting mode change");
    Some(filtered)
}

fn is_focus_mode(mode: &[u8]) -> bool {
    mode.iter()
        .position(|&byte| byte != b'0')
        .is_some_and(|start| &mode[start..] == b"1004")
}

fn classify_query(sequence: &[u8]) -> Action {
    if let Some(body) = sequence.strip_prefix(b"\x1b]") {
        let body = body
            .strip_suffix(b"\x07")
            .or_else(|| body.strip_suffix(b"\x1b\\"))
            .unwrap_or(body);
        return match body {
            b"10;?" => Action::Respond(b"\x1b]10;rgb:ffff/ffff/ffff\x1b\\"),
            b"11;?" => Action::Respond(b"\x1b]11;rgb:0000/0000/0000\x1b\\"),
            // Color, selection and other OSC query forms use a '?' field.
            _ if osc_query(body) => Action::Suppress,
            _ => Action::Forward,
        };
    }
    if let Some(body) = sequence.strip_prefix(b"\x1b[") {
        // Device attributes/status, mode reports, terminal parameters,
        // window reports, resource reports and version queries.
        let query = matches!(body.last(), Some(b'c' | b'n' | b'x'))
            || body.ends_with(b"$p")
            || body.ends_with(b"$w")
            || body.ends_with(b"*y")
            || body.ends_with(b"&u")
            || body == b">q"
            || body == b">0q"
            || (body.ends_with(b"m") && body.starts_with(b"?"))
            || (body.ends_with(b"t") && window_query(&body[..body.len() - 1]));
        if query {
            return Action::Suppress;
        }
    }
    if sequence.starts_with(b"\x1bP$q") || sequence.starts_with(b"\x1bP+q") {
        return Action::Suppress;
    }
    Action::Forward
}

fn osc_query(body: &[u8]) -> bool {
    let mut fields = body.split(|&byte| byte == b';');
    matches!(
        fields.next(),
        Some(
            b"4" | b"5"
                | b"10"
                | b"11"
                | b"12"
                | b"13"
                | b"14"
                | b"15"
                | b"16"
                | b"17"
                | b"18"
                | b"19"
                | b"52"
                | b"60"
                | b"61"
                | b"62"
        )
    ) && fields.any(|field| field == b"?")
}

fn window_query(parameters: &[u8]) -> bool {
    matches!(
        parameters.split(|&byte| byte == b';').next(),
        Some(b"11" | b"13" | b"14" | b"15" | b"16" | b"18" | b"19" | b"20" | b"21")
    )
}

#[cfg(test)]
mod tests {
    use super::{MAX_SEQUENCE_BYTES, TerminalProtocol};

    #[test]
    fn supported_queries_are_answered_locally_at_every_chunk_boundary() {
        for (query, response) in [
            (b"\x1b[6n".as_slice(), b"\x1b[1;1R".as_slice()),
            (b"\x1b[>c", b"\x1b[>0;0;0c"),
            (b"\x1b[>0c", b"\x1b[>0;0;0c"),
            (b"\x1b[c", b"\x1b[?1;0c"),
            (b"\x1b[0c", b"\x1b[?1;0c"),
            (b"\x1bZ", b"\x1b[?1;0c"),
            (b"\x1b]10;?\x07", b"\x1b]10;rgb:ffff/ffff/ffff\x1b\\"),
            (b"\x1b]10;?\x1b\\", b"\x1b]10;rgb:ffff/ffff/ffff\x1b\\"),
            (b"\x1b]11;?\x07", b"\x1b]11;rgb:0000/0000/0000\x1b\\"),
            (b"\x1b]11;?\x1b\\", b"\x1b]11;rgb:0000/0000/0000\x1b\\"),
        ] {
            for split in 0..=query.len() {
                let mut protocol = TerminalProtocol::default();
                let mut first = protocol.process(&query[..split]);
                let second = protocol.process(&query[split..]);
                first.presentation.extend(second.presentation);
                first.responses.extend(second.responses);
                assert!(
                    first.presentation.is_empty(),
                    "query {query:?}, split {split}"
                );
                assert_eq!(first.responses, response);
            }
        }
    }

    #[test]
    fn unsupported_query_families_never_reach_the_viewer() {
        for query in [
            b"\x1b[5n".as_slice(),
            b"\x1b[?6n",
            b"\x1b[=c",
            b"\x1b[18t",
            b"\x1b[14;2t",
            b"\x1b[?2004$p",
            b"\x1b[>q",
            b"\x1b[?m",
            b"\x1b[0x",
            b"\x1b[1$w",
            b"\x1b[1*y",
            b"\x1b[&u",
            b"\x1b]4;1;?\x07",
            b"\x1b]52;c;?\x1b\\",
            b"\x1bP$qm\x1b\\",
            b"\x1bP+q544e\x1b\\",
        ] {
            let mut protocol = TerminalProtocol::default();
            for byte in query {
                let output = protocol.process(&[*byte]);
                assert!(output.presentation.is_empty(), "{query:?}");
                assert!(output.responses.is_empty());
            }
            assert_eq!(protocol.process(b"after").presentation, b"after");
        }
    }

    #[test]
    fn display_controls_and_string_contents_are_preserved() {
        let bytes = b"hello\x1b[31m\x1b[?47h\x1b[>4;2m\x1b[22;2t\x1b[H\x1b[2J\x1b(B\x1b]0;?\x07\x1b]10;red\x1b\\\x1bPzz\x1b[6n\x1b\\world";
        let mut protocol = TerminalProtocol::default();
        let mut displayed = Vec::new();
        for byte in bytes {
            let output = protocol.process(&[*byte]);
            displayed.extend(output.presentation);
            assert!(output.responses.is_empty());
        }
        assert_eq!(displayed, bytes);
    }

    #[test]
    fn focus_reporting_is_filtered_without_changing_other_modes_at_every_split() {
        for (control, expected) in [
            (b"\x1b[?1004h".as_slice(), b"".as_slice()),
            (b"\x1b[?1004l", b""),
            (b"\x1b[?1004s", b""),
            (b"\x1b[?1004r", b""),
            (b"\x1b[?001004h", b""),
            (b"\x1b[?1004;1004h", b""),
            (b"\x1b[?1004;2004h", b"\x1b[?2004h"),
            (b"\x1b[?1;1004;2004l", b"\x1b[?1;2004l"),
            (b"\x1b[?1;1004s", b"\x1b[?1s"),
            (b"\x1b[?1;1004r", b"\x1b[?1r"),
            (b"\x1b[?2004;1004h", b"\x1b[?2004h"),
            (b"\x1b[?10040h", b"\x1b[?10040h"),
            (b"\x1b[1004h", b"\x1b[1004h"),
            (b"\x1b[?1;2004h", b"\x1b[?1;2004h"),
            (b"\x1b]0;1004h\x07", b"\x1b]0;1004h\x07"),
        ] {
            let bytes = [b"before".as_slice(), control, b"after"].concat();
            let expected = [b"before".as_slice(), expected, b"after"].concat();
            for split in 0..=bytes.len() {
                let mut protocol = TerminalProtocol::default();
                let mut first = protocol.process(&bytes[..split]);
                let second = protocol.process(&bytes[split..]);
                first.presentation.extend(second.presentation);
                first.responses.extend(second.responses);
                assert_eq!(first.presentation, expected, "{control:?}, split {split}");
                assert!(first.responses.is_empty());
                assert!(protocol.finish().is_empty());
            }
        }
    }

    #[test]
    fn escape_intermediate_finals_preserve_following_text_at_every_split() {
        for final_byte in [b'[', b']', b'P', b'_', b'^', b'X'] {
            let mut bytes = b"\x1b(".to_vec();
            bytes.push(final_byte);
            bytes.extend_from_slice(b"plain text");
            for split in 0..=bytes.len() {
                let mut protocol = TerminalProtocol::default();
                let mut first = protocol.process(&bytes[..split]);
                let second = protocol.process(&bytes[split..]);
                first.presentation.extend(second.presentation);
                first.responses.extend(second.responses);
                first.presentation.extend(protocol.finish());
                assert_eq!(first.presentation, bytes, "split {split}");
                assert!(first.responses.is_empty());
            }
        }
    }

    #[test]
    fn escape_restart_and_cancellation_do_not_leak_queries_at_every_split() {
        for (bytes, displayed, response) in [
            (
                b"\x1b\x1b[6nafter".as_slice(),
                b"after".as_slice(),
                b"\x1b[1;1R".as_slice(),
            ),
            (b"\x1b(\x1b[6nafter", b"after", b"\x1b[1;1R"),
            (b"\x1b\x18after", b"after", b""),
            (b"\x1b\x1aafter", b"after", b""),
            (b"\x1b(\x18after", b"after", b""),
            (b"\x1b(\x1aafter", b"after", b""),
        ] {
            for split in 0..=bytes.len() {
                let mut protocol = TerminalProtocol::default();
                let mut first = protocol.process(&bytes[..split]);
                let second = protocol.process(&bytes[split..]);
                first.presentation.extend(second.presentation);
                first.responses.extend(second.responses);
                assert_eq!(first.presentation, displayed, "{bytes:?}, split {split}");
                assert_eq!(first.responses, response, "{bytes:?}, split {split}");
            }
        }
    }

    #[test]
    fn incomplete_cancelled_and_oversized_controls_do_not_escape() {
        for prefix in [b"\x1b".as_slice(), b"\x1b[>", b"\x1b]10;?", b"\x1bP$q"] {
            let mut protocol = TerminalProtocol::default();
            assert!(protocol.process(prefix).presentation.is_empty());
            assert!(protocol.finish().is_empty());
            assert_eq!(protocol.process(b"next").presentation, b"next");
        }
        let mut protocol = TerminalProtocol::default();
        assert!(protocol.process(b"\x1b]10;?\x18").presentation.is_empty());
        assert!(protocol.process(b"\x1b[>\x1b[6n").presentation.is_empty());
        let mut oversized = b"\x1b]0;".to_vec();
        oversized.extend(std::iter::repeat_n(b'x', MAX_SEQUENCE_BYTES));
        oversized.extend_from_slice(b"\x1b\\after");
        assert_eq!(protocol.process(&oversized).presentation, b"after");
        assert!(protocol.pending.is_empty());
    }
}
