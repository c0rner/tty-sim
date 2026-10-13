use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

use serde::Deserialize;
use serde_saphyr::Spanned;
use serde_saphyr::options::{DuplicateKeyPolicy, MergeKeyPolicy};

use super::{
    LocatedCommand, ParseError, ParseErrorKind, Script, SourceLocation, YamlError,
    parse_duration_value, parse_key_value,
};
use crate::automation::terminal::EnvironmentChange;
use crate::automation::{Command, DEFAULT_EXPECT_TIMEOUT, TypingRange};
use crate::presentation::MaskRule;

/// Parses one strict YAML tty-sim script.
///
/// # Errors
///
/// Returns the first YAML schema or intrinsic command validation error with a
/// one-based source location.
pub fn parse_script(source: &str) -> Result<Script, ParseError> {
    reject_composition_features(source)?;
    let options = serde_saphyr::options! {
        budget: serde_saphyr::budget! {
            max_documents: 1,
            max_aliases: 0,
            max_anchors: 0,
        },
        alias_limits: serde_saphyr::alias_limits! {
            max_total_replayed_events: 0,
            max_replay_stack_depth: 0,
            max_alias_expansions_per_anchor: 0,
        },
        duplicate_keys: DuplicateKeyPolicy::Error,
        merge_keys: MergeKeyPolicy::Error,
        reject_unsupported_tags: true,
        emit_comments: false,
        with_snippet: false,
        strict_booleans: true,
    };
    let dto: ScriptDto =
        serde_saphyr::from_str_with_options(source, options).map_err(|source| {
            let location =
                source
                    .location()
                    .map_or(SourceLocation { line: 1, column: 1 }, |location| {
                        SourceLocation {
                            line: usize::try_from(location.line()).unwrap_or(usize::MAX),
                            column: usize::try_from(location.column()).unwrap_or(usize::MAX),
                        }
                    });
            ParseError {
                location,
                kind: ParseErrorKind::Yaml(YamlError::new(source.to_string())),
            }
        })?;

    let masks = dto
        .masks
        .into_iter()
        .map(|mask| {
            let location = source_location(mask.referenced);
            mask.value.into_rule(location)
        })
        .collect::<Result<Vec<_>, _>>()?;
    let steps = dto
        .steps
        .into_iter()
        .map(convert_step)
        .collect::<Result<Vec<_>, _>>()?;
    let env = dto
        .env
        .into_iter()
        .map(|(name, value)| match value {
            Some(value) => EnvironmentChange::Set { name, value },
            None => EnvironmentChange::Remove { name },
        })
        .collect();

    Ok(Script::new(dto.cwd.map(PathBuf::from), env, masks, steps))
}

fn reject_composition_features(source: &str) -> Result<(), ParseError> {
    for next in granit_parser::Parser::new_from_str(source) {
        let Ok((event, span)) = next else {
            return Ok(());
        };
        let feature = match event {
            granit_parser::Event::Alias(_) => Some("YAML aliases are not supported"),
            granit_parser::Event::Scalar(_, _, anchor, tag)
            | granit_parser::Event::SequenceStart(_, anchor, tag)
            | granit_parser::Event::MappingStart(_, anchor, tag)
                if anchor != 0 || tag.is_some() =>
            {
                Some(if anchor != 0 {
                    "YAML anchors are not supported"
                } else {
                    "explicit YAML tags are not supported"
                })
            }
            _ => None,
        };
        if let Some(message) = feature {
            let marker = span.tag_start.unwrap_or(span.start);
            return Err(ParseError {
                location: SourceLocation {
                    line: marker.line(),
                    column: marker.col() + 1,
                },
                kind: ParseErrorKind::UnsupportedFeature(message),
            });
        }
    }
    Ok(())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ScriptDto {
    cwd: Option<String>,
    #[serde(default)]
    env: BTreeMap<String, Option<String>>,
    #[serde(default)]
    masks: Vec<Spanned<MaskDto>>,
    steps: Vec<Spanned<StepDto>>,
}

#[derive(Deserialize)]
#[serde(rename_all = "lowercase")]
enum StepDto {
    Mask(MaskDto),
    Expect(ExpectDto),
    Wait(WaitDto),
    Key(String),
    Type(String),
    Typing(TypingDto),
    Send(ByteValueDto),
    Show(ByteValueDto),
    Simulate(SimulateDto),
}

#[derive(Deserialize)]
#[serde(untagged)]
enum MaskDto {
    Automatic(String),
    Full(MaskFieldsDto),
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct MaskFieldsDto {
    from: ByteValueDto,
    to: Option<ByteValueDto>,
}

impl MaskDto {
    fn into_rule(self, location: SourceLocation) -> Result<MaskRule, ParseError> {
        let result = match self {
            Self::Automatic(from) => MaskRule::automatic(from.into_bytes()),
            Self::Full(MaskFieldsDto { from, to }) => {
                let from = from.into_bytes(location)?;
                match to {
                    Some(to) => MaskRule::new(from, to.into_bytes(location)?),
                    None => MaskRule::automatic(from),
                }
            }
        };
        result.map_err(|kind| ParseError {
            location,
            kind: ParseErrorKind::Validation(kind.into()),
        })
    }
}

#[derive(Deserialize)]
#[serde(untagged)]
enum ExpectDto {
    Pattern(String),
    Full(ExpectFieldsDto),
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ExpectFieldsDto {
    pattern: ByteValueDto,
    timeout: Option<String>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum WaitDto {
    Duration(String),
    Full(WaitFieldsDto),
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WaitFieldsDto {
    duration: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TypingDto {
    min: String,
    max: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SimulateDto {
    command: String,
    output: ByteValueDto,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum ByteValueDto {
    Text(String),
    Hex(HexDto),
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct HexDto {
    hex: String,
}

impl ByteValueDto {
    fn into_bytes(self, location: SourceLocation) -> Result<Vec<u8>, ParseError> {
        match self {
            Self::Text(text) => Ok(text.into_bytes()),
            Self::Hex(HexDto { hex }) => decode_hex(&hex).ok_or(ParseError {
                location,
                kind: ParseErrorKind::InvalidHex,
            }),
        }
    }
}

fn decode_hex(value: &str) -> Option<Vec<u8>> {
    if value.len() % 2 != 0
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte.to_ascii_lowercase()))
    {
        return None;
    }
    value
        .as_bytes()
        .chunks_exact(2)
        .map(|digits| {
            let text = std::str::from_utf8(digits).ok()?;
            u8::from_str_radix(text, 16).ok()
        })
        .collect()
}

fn convert_step(step: Spanned<StepDto>) -> Result<LocatedCommand, ParseError> {
    let location = source_location(step.referenced);
    let command = match step.value {
        StepDto::Mask(mask) => Command::Mask(mask.into_rule(location)?),
        StepDto::Expect(expect) => convert_expect(expect, location)?,
        StepDto::Wait(wait) => {
            let value = match wait {
                WaitDto::Duration(value) => value,
                WaitDto::Full(WaitFieldsDto { duration }) => duration,
            };
            Command::Wait(parse_duration(&value, location)?)
        }
        StepDto::Key(key) => Command::Key(parse_key_value(&key).ok_or(ParseError {
            location,
            kind: ParseErrorKind::InvalidKey(key),
        })?),
        StepDto::Type(text) => Command::Type(text),
        StepDto::Typing(TypingDto { min, max }) => {
            let minimum = parse_duration(&min, location)?;
            let maximum = parse_duration(&max, location)?;
            let range = TypingRange::new(minimum, maximum).map_err(|kind| ParseError {
                location,
                kind: ParseErrorKind::Validation(kind.into()),
            })?;
            Command::Typing(range)
        }
        StepDto::Send(value) => Command::Send(value.into_bytes(location)?),
        StepDto::Show(value) => Command::Show(value.into_bytes(location)?),
        StepDto::Simulate(SimulateDto { command, output }) => Command::Simulate {
            command,
            output: output.into_bytes(location)?,
        },
    };
    Ok(LocatedCommand { command, location })
}

fn source_location(location: serde_saphyr::Location) -> SourceLocation {
    SourceLocation {
        line: usize::try_from(location.line()).unwrap_or(usize::MAX),
        column: usize::try_from(location.column()).unwrap_or(usize::MAX),
    }
}

fn convert_expect(expect: ExpectDto, location: SourceLocation) -> Result<Command, ParseError> {
    let (pattern, timeout) = match expect {
        ExpectDto::Pattern(pattern) => (pattern.into_bytes(), DEFAULT_EXPECT_TIMEOUT),
        ExpectDto::Full(ExpectFieldsDto { pattern, timeout }) => (
            pattern.into_bytes(location)?,
            timeout.map_or(Ok(DEFAULT_EXPECT_TIMEOUT), |value| {
                parse_duration(&value, location)
            })?,
        ),
    };
    if pattern.is_empty() {
        return Err(ParseError {
            location,
            kind: ParseErrorKind::Validation(
                crate::presentation::ConfigurationError::EmptyExpectPattern.into(),
            ),
        });
    }
    Ok(Command::Expect { pattern, timeout })
}

fn parse_duration(value: &str, location: SourceLocation) -> Result<Duration, ParseError> {
    parse_duration_value(value).ok_or_else(|| ParseError {
        location,
        kind: ParseErrorKind::InvalidDuration(value.to_owned()),
    })
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::{ParseErrorKind, parse_script};
    use crate::automation::Command;
    use crate::automation::terminal::EnvironmentChange;

    #[test]
    fn complete_schema_preserves_values_defaults_and_locations() {
        let script = parse_script(
            r#"
cwd: fixtures/app
env:
  TERM: xterm-256color
  SECRET: null
masks:
  - secret
  - é
  - from: { hex: ff }
    to: { hex: 2a }
steps:
  - expect: ready
  - expect: { pattern: { hex: 00ff }, timeout: 1.5s }
  - wait: { duration: 5ms }
  - key: enter
  - type: |-
      hello
  - typing: { min: 1ms, max: 2ms }
  - send: { hex: 0d0a }
  - show: |
      result
  - simulate: { command: "demo\n", output: done }
"#,
        )
        .unwrap();

        assert_eq!(script.cwd().unwrap().to_str(), Some("fixtures/app"));
        assert_eq!(
            script.env(),
            [
                EnvironmentChange::Remove {
                    name: "SECRET".into()
                },
                EnvironmentChange::Set {
                    name: "TERM".into(),
                    value: "xterm-256color".into()
                }
            ]
        );
        assert_eq!(script.masks()[0].to(), b"******");
        assert_eq!(script.masks()[1].from(), "é".as_bytes());
        assert_eq!(script.masks()[1].to(), b"**");
        assert_eq!(script.masks()[2].from(), b"\xff");
        assert_eq!(script.steps().len(), 9);
        assert_eq!(script.steps()[0].location.line, 12);
        assert_eq!(script.steps()[4].command, Command::Type("hello".into()));
        assert_eq!(script.steps()[6].command, Command::Send(b"\r\n".to_vec()));
        assert_eq!(
            script.steps()[7].command,
            Command::Show(b"result\n".to_vec())
        );
        assert!(matches!(
            script.steps()[1].command,
            Command::Expect {
                timeout,
                ..
            } if timeout == Duration::from_millis(1_500)
        ));
    }

    #[test]
    fn schema_and_yaml_composition_fail_closed() {
        for invalid in [
            "send \"old format\"",
            "cwd: here",
            "steps: []\nunknown: true",
            "steps:\n  - send: a\n    show: b",
            "steps: []\nsteps: []",
            "steps: &steps []",
            "steps: *steps",
            "steps: !!seq []",
            "steps:\n  - <<: { send: value }",
            "steps: []\n---\nsteps: []",
        ] {
            assert!(parse_script(invalid).is_err(), "{invalid}");
        }
    }

    #[test]
    fn intrinsic_values_fail_at_the_owning_step() {
        for (source, expected_line) in [
            ("steps:\n  - expect: ''", 2),
            ("steps:\n  - mask: ''", 2),
            ("masks:\n  - ''\nsteps: []", 2),
            ("steps:\n  - wait: soon", 2),
            ("steps:\n  - key: Shift+A", 2),
            ("steps:\n  - send: { hex: abc }", 2),
            ("steps:\n  - typing: { min: 2ms, max: 1ms }", 2),
        ] {
            let error = parse_script(source).unwrap_err();
            assert_eq!(error.location.line, expected_line, "{source}");
            assert!(!matches!(error.kind, ParseErrorKind::Yaml(_)), "{source}");
        }
    }
}
