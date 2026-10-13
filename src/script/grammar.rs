use std::time::Duration;

pub fn parse_duration_value(token: &str) -> Option<Duration> {
    token.strip_suffix("ms").map_or_else(
        || token.strip_suffix('s').and_then(parse_seconds),
        parse_milliseconds,
    )
}

fn parse_milliseconds(value: &str) -> Option<Duration> {
    if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    value.parse().ok().map(Duration::from_millis)
}

fn parse_seconds(value: &str) -> Option<Duration> {
    let (whole, fractional) = match value.split_once('.') {
        Some((whole, fractional)) => (whole, Some(fractional)),
        None => (value, None),
    };
    if whole.is_empty() || !whole.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    let seconds = whole.parse::<u64>().ok()?;
    let nanoseconds = match fractional {
        Some(fractional)
            if !fractional.is_empty() && fractional.bytes().all(|byte| byte.is_ascii_digit()) =>
        {
            let significant = fractional.trim_end_matches('0');
            if significant.len() > 9 {
                return None;
            }
            let fraction = significant.parse::<u32>().unwrap_or(0);
            fraction.checked_mul(10_u32.pow(9 - u32::try_from(significant.len()).ok()?))?
        }
        Some(_) => return None,
        None => 0,
    };
    Some(Duration::new(seconds, nanoseconds))
}

pub fn parse_key_value(token: &str) -> Option<Vec<u8>> {
    let (control, after_control) = token
        .strip_prefix("Ctrl+")
        .map_or((false, token), |base| (true, base));
    let (alternate, base) = after_control
        .strip_prefix("Alt+")
        .map_or((false, after_control), |base| (true, base));

    let mut sequence = if control {
        control_key(base)
    } else {
        base_key(base)
    }?;

    if alternate {
        sequence.insert(0, 0x1b);
    }
    Some(sequence)
}

fn control_key(base: &str) -> Option<Vec<u8>> {
    match base.as_bytes() {
        [letter] if letter.is_ascii_alphabetic() => {
            Some(vec![letter.to_ascii_uppercase() - b'A' + 1])
        }
        _ => None,
    }
}

fn base_key(base: &str) -> Option<Vec<u8>> {
    const NAMED_KEYS: [(&str, &[u8]); 27] = [
        ("Enter", b"\r"),
        ("Tab", b"\t"),
        ("Backspace", b"\x7f"),
        ("Escape", b"\x1b"),
        ("Esc", b"\x1b"),
        ("Up", b"\x1b[A"),
        ("Down", b"\x1b[B"),
        ("Right", b"\x1b[C"),
        ("Left", b"\x1b[D"),
        ("Home", b"\x1b[H"),
        ("End", b"\x1b[F"),
        ("Insert", b"\x1b[2~"),
        ("Delete", b"\x1b[3~"),
        ("PageUp", b"\x1b[5~"),
        ("PageDown", b"\x1b[6~"),
        ("F1", b"\x1bOP"),
        ("F2", b"\x1bOQ"),
        ("F3", b"\x1bOR"),
        ("F4", b"\x1bOS"),
        ("F5", b"\x1b[15~"),
        ("F6", b"\x1b[17~"),
        ("F7", b"\x1b[18~"),
        ("F8", b"\x1b[19~"),
        ("F9", b"\x1b[20~"),
        ("F10", b"\x1b[21~"),
        ("F11", b"\x1b[23~"),
        ("F12", b"\x1b[24~"),
    ];

    NAMED_KEYS
        .iter()
        .find_map(|(name, sequence)| name.eq_ignore_ascii_case(base).then(|| sequence.to_vec()))
        .or_else(|| match base.as_bytes() {
            [byte] if byte.is_ascii_graphic() || *byte == b' ' => Some(vec![*byte]),
            _ => None,
        })
}

#[cfg(test)]
mod tests {
    use super::{parse_duration_value, parse_key_value};
    use std::time::Duration;

    #[test]
    fn duration_grammar_preserves_exact_supported_boundaries() {
        assert_eq!(parse_duration_value("0ms"), Some(Duration::ZERO));
        assert_eq!(
            parse_duration_value("1.000000001s"),
            Some(Duration::new(1, 1))
        );
        assert_eq!(
            parse_duration_value("18446744073709551615.999999999s"),
            Some(Duration::new(u64::MAX, 999_999_999))
        );
        for invalid in ["1", "-1s", ".5s", "1.s", "0.0000000001s"] {
            assert_eq!(parse_duration_value(invalid), None, "{invalid}");
        }
    }

    #[test]
    fn key_mapping_retains_supported_sequences_and_rejects_guesses() {
        assert_eq!(parse_key_value("enter"), Some(b"\r".to_vec()));
        assert_eq!(parse_key_value("Ctrl+Alt+a"), Some(b"\x1b\x01".to_vec()));
        assert_eq!(parse_key_value("Alt+x"), Some(b"\x1bx".to_vec()));
        for invalid in ["é", "Ctrl+1", "Alt+Ctrl+A", "Shift+A", "ctrl+A"] {
            assert_eq!(parse_key_value(invalid), None, "{invalid}");
        }
    }
}
