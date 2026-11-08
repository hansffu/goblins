//! What the daemon and a goblin's supervisor agree on for terminals the
//! supervisor emulates: limits, the calls made on one, and key encoding.
use serde::{Deserialize, Serialize};

/// Terminals per goblin.
pub const MAX_TERMINALS: usize = 16;
/// Waits in progress per goblin.
pub const MAX_WAITS: usize = 16;
/// The most rows or columns a terminal may have.
pub const MAX_SIZE: u16 = 500;
/// A terminal nobody has attached to yet, unless `create` sets a size.
pub const DEFAULT_SIZE: (u16, u16) = (40, 120);
/// Lines of scrollback the emulator keeps.
pub const SCROLLBACK: usize = 5_000;
/// The most `text` a `send` takes, in bytes.
pub const MAX_TEXT: usize = 64 * 1024;
/// The most keys a `send` takes.
pub const MAX_KEYS: usize = 64;
/// Lines `read` returns, and `wait` scans, unless asked otherwise.
pub const DEFAULT_LINES: usize = 100;
/// The most lines either asks for.
pub const MAX_LINES: usize = 5_000;
/// Lines of an exited terminal a `wait` returns, unless asked otherwise.
pub const EXIT_LINES: usize = 50;
/// Milliseconds a `wait` lasts unless asked otherwise.
pub const DEFAULT_TIMEOUT: u64 = 30_000;
/// The most milliseconds a `wait` may last.
pub const MAX_TIMEOUT: u64 = 3_600_000;
/// Text in a result keeps the newest lines up to this many bytes.
pub const TEXT_CAP: usize = 256 * 1024;
/// A matched line is cut to this many bytes.
pub const LINE_CAP: usize = 4 * 1024;
/// The compiled size a `regex` may reach.
const REGEX_LIMIT: usize = 256 * 1024;

pub fn valid_size(rows: u16, cols: u16) -> bool {
    (1..=MAX_SIZE).contains(&rows) && (1..=MAX_SIZE).contains(&cols)
}

/// A name for a terminal: not of the form of an ID.
pub fn valid_name(name: &str) -> bool {
    let bytes = name.as_bytes();
    (1..=32).contains(&bytes.len())
        && bytes[0].is_ascii_lowercase()
        && bytes
            .iter()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'_' | b'-'))
        && !is_id(name)
}

/// `t` followed by digits.
pub fn is_id(text: &str) -> bool {
    text.strip_prefix('t')
        .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Source {
    Visible,
    Recent,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Condition {
    /// A literal substring.
    Match(String),
    /// A Rust regular expression.
    Regex(String),
    /// The terminal's program exits.
    Exit,
}

/// A call on one terminal, answered with `Event::TerminalResult`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Call {
    Read {
        source: Source,
        lines: Option<usize>,
    },
    Send {
        text: Option<String>,
        keys: Vec<String>,
    },
    Wait {
        condition: Condition,
        /// Milliseconds.
        timeout: u64,
        lines: Option<usize>,
    },
}

/// Compile a `wait` regex with a size limit, or say why it cannot be used.
pub fn compile(pattern: &str) -> Result<regex::Regex, String> {
    regex::RegexBuilder::new(pattern)
        .size_limit(REGEX_LIMIT)
        .build()
        .map_err(|e| format!("invalid regex: {e}"))
}

/// Check a call's parameters, as the daemon does before it asks the
/// supervisor and the supervisor does before it acts.
pub fn validate(call: &Call) -> Result<(), String> {
    match call {
        Call::Read { lines, .. } => lines_ok(*lines),
        Call::Send { text, keys } => {
            if text.as_ref().is_some_and(|t| t.len() > MAX_TEXT) {
                return Err(format!("text is longer than {MAX_TEXT} bytes"));
            }
            if keys.len() > MAX_KEYS {
                return Err(format!("more than {MAX_KEYS} keys"));
            }
            if text.is_none() && keys.is_empty() {
                return Err("nothing to send".into());
            }
            match keys.iter().find(|key| key_bytes(key, false).is_none()) {
                Some(key) => Err(format!("unknown key {key:?}")),
                None => Ok(()),
            }
        }
        Call::Wait {
            condition,
            timeout,
            lines,
        } => {
            if *timeout > MAX_TIMEOUT {
                return Err(format!("timeout is longer than {MAX_TIMEOUT} milliseconds"));
            }
            lines_ok(*lines)?;
            match condition {
                Condition::Match(text) if text.is_empty() => Err("match is empty".into()),
                Condition::Regex(pattern) => compile(pattern).map(drop),
                _ => Ok(()),
            }
        }
    }
}

fn lines_ok(lines: Option<usize>) -> Result<(), String> {
    match lines {
        Some(n) if !(1..=MAX_LINES).contains(&n) => {
            Err(format!("lines must be 1 to {MAX_LINES}"))
        }
        _ => Ok(()),
    }
}

/// The bytes a terminal program reads for key `name`; `application` is the
/// cursor mode the program set. None for a name that is not a key.
pub fn key_bytes(name: &str, application: bool) -> Option<Vec<u8>> {
    let name = name.to_ascii_lowercase();
    let cursor = |letter: u8| {
        if application {
            vec![0x1b, b'O', letter]
        } else {
            vec![0x1b, b'[', letter]
        }
    };
    let fixed: &[u8] = match name.as_str() {
        "enter" => b"\r",
        "tab" => b"\t",
        "esc" => b"\x1b",
        "backspace" => b"\x7f",
        "space" => b" ",
        "up" => return Some(cursor(b'A')),
        "down" => return Some(cursor(b'B')),
        "right" => return Some(cursor(b'C')),
        "left" => return Some(cursor(b'D')),
        "home" => return Some(cursor(b'H')),
        "end" => return Some(cursor(b'F')),
        "pageup" => b"\x1b[5~",
        "pagedown" => b"\x1b[6~",
        "insert" => b"\x1b[2~",
        "delete" => b"\x1b[3~",
        "shift+tab" => b"\x1b[Z",
        "f1" => b"\x1bOP",
        "f2" => b"\x1bOQ",
        "f3" => b"\x1bOR",
        "f4" => b"\x1bOS",
        "f5" => b"\x1b[15~",
        "f6" => b"\x1b[17~",
        "f7" => b"\x1b[18~",
        "f8" => b"\x1b[19~",
        "f9" => b"\x1b[20~",
        "f10" => b"\x1b[21~",
        "f11" => b"\x1b[23~",
        "f12" => b"\x1b[24~",
        _ => {
            if let Some(letter) = name.strip_prefix("ctrl+") {
                return match letter.as_bytes() {
                    [b @ b'a'..=b'z'] => Some(vec![b & 0x1f]),
                    _ => None,
                };
            }
            if let Some(character) = name.strip_prefix("alt+") {
                return match character.as_bytes() {
                    [b] if b.is_ascii_graphic() => Some(vec![0x1b, *b]),
                    _ => None,
                };
            }
            return None;
        }
    };
    Some(fixed.to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_not_ids() {
        for good in ["app", "a", "dev-server", "w_1", "t", "tx1", "t1x"] {
            assert!(valid_name(good), "{good}");
        }
        for bad in ["", "t1", "t42", "1a", "-a", "A", "has space", &"a".repeat(33)] {
            assert!(!valid_name(bad), "{bad}");
        }
        assert!(is_id("t1") && !is_id("t") && !is_id("t1a") && !is_id("app"));
    }

    #[test]
    fn sizes_are_bounded() {
        assert!(valid_size(1, 1) && valid_size(500, 500));
        assert!(!valid_size(0, 80) && !valid_size(24, 0) && !valid_size(501, 80));
        assert!(!valid_size(24, 501));
    }

    #[test]
    fn keys_follow_the_cursor_mode() {
        assert_eq!(key_bytes("up", false).unwrap(), b"\x1b[A");
        assert_eq!(key_bytes("up", true).unwrap(), b"\x1bOA");
        assert_eq!(key_bytes("Home", true).unwrap(), b"\x1bOH");
        assert_eq!(key_bytes("end", false).unwrap(), b"\x1b[F");
        assert_eq!(key_bytes("enter", true).unwrap(), b"\r");
        assert_eq!(key_bytes("ctrl+c", false).unwrap(), b"\x03");
        assert_eq!(key_bytes("ctrl+Z", false).unwrap(), b"\x1a");
        assert_eq!(key_bytes("alt+x", false).unwrap(), b"\x1bx");
        assert_eq!(key_bytes("shift+tab", false).unwrap(), b"\x1b[Z");
        assert_eq!(key_bytes("f1", false).unwrap(), b"\x1bOP");
        assert_eq!(key_bytes("f12", false).unwrap(), b"\x1b[24~");
        for bad in ["f13", "ctrl+1", "ctrl+ab", "alt+", "alt+é", "ctrl", "nothing"] {
            assert!(key_bytes(bad, false).is_none(), "{bad}");
        }
    }

    #[test]
    fn calls_are_checked_before_anything_is_done() {
        let send = |text: Option<&str>, keys: &[&str]| Call::Send {
            text: text.map(Into::into),
            keys: keys.iter().map(|k| k.to_string()).collect(),
        };
        assert!(validate(&send(Some("ls"), &["enter"])).is_ok());
        assert!(validate(&send(None, &["nope"])).is_err());
        assert!(validate(&send(Some("ls"), &["enter", "nope"])).is_err());
        assert!(validate(&send(None, &[])).is_err());
        assert!(validate(&send(Some(&"x".repeat(MAX_TEXT + 1)), &[])).is_err());
        assert!(validate(&send(Some(&"x".repeat(MAX_TEXT)), &[])).is_ok());
        assert!(validate(&send(None, &vec!["enter"; MAX_KEYS + 1])).is_err());
        let wait = |condition, timeout, lines| Call::Wait {
            condition,
            timeout,
            lines,
        };
        assert!(validate(&wait(Condition::Exit, MAX_TIMEOUT, Some(5000))).is_ok());
        assert!(validate(&wait(Condition::Exit, MAX_TIMEOUT + 1, None)).is_err());
        assert!(validate(&wait(Condition::Exit, 1, Some(0))).is_err());
        assert!(validate(&wait(Condition::Exit, 1, Some(5001))).is_err());
        assert!(validate(&wait(Condition::Match(String::new()), 1, None)).is_err());
        assert!(validate(&wait(Condition::Regex("^done$".into()), 1, None)).is_ok());
        assert!(validate(&wait(Condition::Regex("(".into()), 1, None)).is_err());
        // A pattern that compiles past the size limit is refused.
        assert!(validate(&wait(Condition::Regex("(a{1000}){1000}".into()), 1, None)).is_err());
    }
}
