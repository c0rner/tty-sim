const OSC_BACKGROUND_QUERY_BEL: &[u8] = b"\x1b]11;?\x07";
const OSC_BACKGROUND_QUERY_ST: &[u8] = b"\x1b]11;?\x1b\\";
const CURSOR_POSITION_QUERY: &[u8] = b"\x1b[6n";

const OSC_BACKGROUND_RESPONSE: &[u8] = b"\x1b]11;rgb:0000/0000/0000\x1b\\";
const CURSOR_POSITION_RESPONSE: &[u8] = b"\x1b[1;1R";

const QUERIES: [(&[u8], &[u8]); 3] = [
    (OSC_BACKGROUND_QUERY_BEL, OSC_BACKGROUND_RESPONSE),
    (OSC_BACKGROUND_QUERY_ST, OSC_BACKGROUND_RESPONSE),
    (CURSOR_POSITION_QUERY, CURSOR_POSITION_RESPONSE),
];

#[derive(Debug, Default)]
pub struct TerminalProtocol {
    pending: Vec<u8>,
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
            if self.pending.is_empty() && byte != b'\x1b' {
                output.presentation.push(byte);
                continue;
            }
            self.pending.push(byte);
            self.resolve_pending(&mut output);
        }

        output
    }

    pub fn finish(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.pending)
    }

    fn resolve_pending(&mut self, output: &mut ProtocolOutput) {
        loop {
            if let Some((_, response)) = QUERIES
                .iter()
                .find(|(query, _)| *query == self.pending.as_slice())
            {
                output.responses.extend_from_slice(response);
                self.pending.clear();
                return;
            }

            if QUERIES
                .iter()
                .any(|(query, _)| query.starts_with(&self.pending))
            {
                return;
            }

            if self.pending.is_empty() {
                return;
            }

            output.presentation.push(self.pending.remove(0));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{ProtocolOutput, TerminalProtocol};

    #[test]
    fn terminal_protocol_queries_answered_and_withheld() {
        let mut protocol = TerminalProtocol::default();

        let bel_output = protocol.process(b"\x1b]11;?\x07");
        assert_eq!(
            bel_output,
            ProtocolOutput {
                presentation: vec![],
                responses: b"\x1b]11;rgb:0000/0000/0000\x1b\\".to_vec(),
            }
        );

        let csi_output = protocol.process(b"\x1b[6n");
        assert_eq!(
            csi_output,
            ProtocolOutput {
                presentation: vec![],
                responses: b"\x1b[1;1R".to_vec(),
            }
        );
    }

    #[test]
    fn terminal_protocol_ordinary_bytes_preserved() {
        let mut protocol = TerminalProtocol::default();
        let output = protocol.process(b"hello\x1b[31mworld");
        assert_eq!(output.presentation, b"hello\x1b[31mworld");
        assert!(output.responses.is_empty());
    }
}
