use super::{ConfigurationError, MaskRule as ConfiguredMaskRule};

#[derive(Debug, Clone, PartialEq, Eq)]
struct MaskRule {
    search: Vec<u8>,
    replacement: Vec<u8>,
}

#[derive(Debug, Default)]
pub struct StreamingMasker {
    rules: Vec<MaskRule>,
    pending: Vec<u8>,
}

impl StreamingMasker {
    #[cfg(test)]
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_rules(rules: &[ConfiguredMaskRule]) -> Self {
        Self {
            rules: rules
                .iter()
                .map(|rule| MaskRule {
                    search: rule.from().to_vec(),
                    replacement: rule.to().to_vec(),
                })
                .collect(),
            pending: Vec::new(),
        }
    }

    pub fn process(&mut self, input: &[u8]) -> Vec<u8> {
        self.pending.extend_from_slice(input);
        self.resolve(false)
    }

    /// Installs a new mask rule, flushing pending bytes under prior rules.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigurationError::EmptyMaskPattern`] if `search` is empty.
    pub fn install(
        &mut self,
        search: Vec<u8>,
        replacement: Vec<u8>,
    ) -> Result<Vec<u8>, ConfigurationError> {
        if search.is_empty() {
            return Err(ConfigurationError::EmptyMaskPattern);
        }

        let flushed = self.resolve(true);
        self.rules.push(MaskRule {
            search,
            replacement,
        });
        Ok(flushed)
    }

    pub fn finish(&mut self) -> Vec<u8> {
        self.resolve(true)
    }

    fn resolve(&mut self, final_input: bool) -> Vec<u8> {
        let mut output = Vec::new();
        let mut consumed = 0;
        while consumed < self.pending.len() {
            let remaining = &self.pending[consumed..];
            let mut longest_match = None;
            let mut needs_more_input = false;

            for (index, rule) in self.rules.iter().enumerate() {
                if remaining.starts_with(&rule.search) {
                    match longest_match {
                        Some((_, length)) if length >= rule.search.len() => {}
                        _ => longest_match = Some((index, rule.search.len())),
                    }
                } else if rule.search.starts_with(remaining) {
                    needs_more_input = true;
                }
            }

            if needs_more_input && !final_input {
                break;
            }

            #[allow(clippy::single_match_else)]
            match longest_match {
                Some((index, length)) => {
                    output.extend_from_slice(&self.rules[index].replacement);
                    consumed += length;
                }
                None => {
                    output.push(remaining[0]);
                    consumed += 1;
                }
            }
        }
        self.pending.drain(..consumed);
        output
    }
}

#[cfg(test)]
mod tests {
    use super::{ConfigurationError, StreamingMasker};

    #[test]
    fn contained_matches_are_replaced_without_reprocessing_replacements() {
        let mut masker = StreamingMasker::new();
        assert_eq!(
            masker.install(b"secret".to_vec(), b"hidden".to_vec()),
            Ok(Vec::new())
        );
        assert_eq!(
            masker.install(b"hidden".to_vec(), b"again".to_vec()),
            Ok(Vec::new())
        );

        let mut output = masker.process(b"a secret value");
        output.extend(masker.finish());

        assert_eq!(output, b"a hidden value");
    }

    #[test]
    fn a_match_split_at_every_byte_boundary_is_identical_to_one_chunk() {
        let input = b"before needle after";
        let expected = b"before *** after";

        for split in 0..=input.len() {
            let mut masker = masker_with_rules(&[(b"needle", b"***")]);
            let mut output = masker.process(&input[..split]);
            output.extend(masker.process(&input[split..]));
            output.extend(masker.finish());
            assert_eq!(output, expected, "split at {split}");
        }
    }

    #[test]
    fn overlapping_prefix_rules_use_leftmost_longest_and_registration_order() {
        let mut masker = masker_with_rules(&[
            (b"ab", b"short"),
            (b"aba", b"long"),
            (b"ba", b"tail"),
            (b"aba", b"later"),
        ]);

        let mut output = masker.process(b"ababa");
        output.extend(masker.finish());

        assert_eq!(output, b"longtail");
    }

    #[test]
    fn multi_byte_utf8_search_values_match_across_every_byte_boundary() {
        let input = "a🙂b".as_bytes();

        for split in 1..input.len() {
            let mut masker = masker_with_rules(&[("🙂".as_bytes(), b"[face]")]);
            let mut output = masker.process(&input[..split]);
            output.extend(masker.process(&input[split..]));
            output.extend(masker.finish());
            assert_eq!(output, b"a[face]b", "split at {split}");
        }
    }

    #[test]
    fn installing_a_rule_flushes_pending_bytes_under_the_old_rules() {
        let mut masker = masker_with_rules(&[(b"secret", b"old")]);
        assert_eq!(masker.process(b"sec"), b"");

        let flushed = masker.install(b"sec".to_vec(), b"new".to_vec()).unwrap();
        let mut later = masker.process(b"sec!");
        later.extend(masker.finish());

        assert_eq!(flushed, b"sec");
        assert_eq!(later, b"new!");
    }

    #[test]
    fn final_flush_emits_an_unmatched_partial_prefix() {
        let mut masker = masker_with_rules(&[(b"password", b"***")]);

        assert_eq!(masker.process(b"pass"), b"");
        assert_eq!(masker.finish(), b"pass");
        assert_eq!(masker.finish(), b"");
    }

    #[test]
    fn output_is_equivalent_for_every_chunk_partition() {
        let input = b"zabacaba";
        let expected = process_partition(input, 0);

        for boundaries in 0..(1_usize << (input.len() - 1)) {
            assert_eq!(
                process_partition(input, boundaries),
                expected,
                "partition mask {boundaries:#b}"
            );
        }
    }

    #[test]
    fn empty_search_values_are_rejected_without_changing_pending_state() {
        let mut masker = masker_with_rules(&[(b"abc", b"x")]);
        assert_eq!(masker.process(b"ab"), b"");

        assert_eq!(
            masker.install(Vec::new(), b"invalid".to_vec()),
            Err(ConfigurationError::EmptyMaskPattern)
        );
        assert_eq!(masker.finish(), b"ab");
    }

    fn masker_with_rules(rules: &[(&[u8], &[u8])]) -> StreamingMasker {
        let mut masker = StreamingMasker::new();
        for (search, replacement) in rules {
            assert_eq!(
                masker.install(search.to_vec(), replacement.to_vec()),
                Ok(Vec::new())
            );
        }
        masker
    }

    fn process_partition(input: &[u8], boundaries: usize) -> Vec<u8> {
        let mut masker = masker_with_rules(&[
            (b"aba", b"X"),
            (b"ab", b"Y"),
            (b"cab", b"Z"),
            (b"z", b"aba"),
        ]);
        let mut output = Vec::new();
        let mut chunk_start = 0;
        for boundary in 1..input.len() {
            if boundaries & (1 << (boundary - 1)) != 0 {
                output.extend(masker.process(&input[chunk_start..boundary]));
                chunk_start = boundary;
            }
        }
        output.extend(masker.process(&input[chunk_start..]));
        output.extend(masker.finish());
        output
    }
}
