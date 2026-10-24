//! Standard OSC 10/11 terminal colors. Never invent a palette for a headless client.
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Palette {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub foreground: Option<[u16; 3]>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub background: Option<[u16; 3]>,
}
impl Palette {
    pub fn is_empty(&self) -> bool {
        self.foreground.is_none() && self.background.is_none()
    }
    pub fn reply(&self, code: u8) -> Option<Vec<u8>> {
        let [r, g, b] = match code {
            10 => self.foreground,
            11 => self.background,
            _ => None,
        }?;
        Some(format!("\x1b]{code};rgb:{r:04x}/{g:04x}/{b:04x}\x1b\\").into_bytes())
    }
    /// Accept only complete, bounded standard RGB replies (BEL or ST).
    pub fn observe(&mut self, report: &[u8]) -> bool {
        let Some(body) = report.strip_prefix(b"\x1b]").and_then(|s| {
            s.strip_suffix(b"\x07")
                .or_else(|| s.strip_suffix(b"\x1b\\"))
        }) else {
            return false;
        };
        let Some(split) = body.iter().position(|b| *b == b';') else {
            return false;
        };
        let (code, rgb) = (&body[..split], &body[split + 1..]);
        let slot = match code {
            b"10" => &mut self.foreground,
            b"11" => &mut self.background,
            _ => return false,
        };
        let Some(rgb) = rgb.strip_prefix(b"rgb:") else {
            return false;
        };
        let mut parts = rgb.split(|b| *b == b'/');
        let mut color = [0; 3];
        for component in &mut color {
            let Some(part) = parts.next() else {
                return false;
            };
            if part.is_empty() || part.len() > 4 || !part.iter().all(u8::is_ascii_hexdigit) {
                return false;
            }
            let value = u32::from_str_radix(std::str::from_utf8(part).unwrap(), 16).unwrap();
            *component = (value * 65535 / ((1u32 << (4 * part.len())) - 1)) as u16;
        }
        if parts.next().is_some() {
            return false;
        }
        *slot = Some(color);
        true
    }
}

/// Split requested color replies from typeahead, preserving every other byte.
/// Used only during the bounded startup probe; incomplete sequences stay input.
pub(super) fn probe_replies(bytes: &[u8]) -> (Palette, Vec<u8>, bool) {
    let mut palette = Palette::default();
    let mut input = Vec::new();
    let mut i = 0;
    let mut paste = false;
    while i < bytes.len() {
        if bytes[i..].starts_with(b"\x1b[200~") {
            paste = true;
        }
        if bytes[i..].starts_with(b"\x1b[201~") {
            paste = false;
        }
        if !paste
            && bytes[i..].starts_with(b"\x1b")
            && bytes.get(i + 1).is_some_and(|b| b"P_^X".contains(b))
        {
            let end = bytes[i + 2..]
                .windows(2)
                .position(|w| w == b"\x1b\\")
                .map(|offset| i + 2 + offset + 2)
                .unwrap_or(bytes.len());
            input.extend_from_slice(&bytes[i..end]);
            i = end;
            continue;
        }
        // DA1 is sent after both OSC requests. Its reply bounds the probe
        // even when colors are unsupported, without a fixed short sleep.
        if !paste && bytes[i..].starts_with(b"\x1b[?") {
            let end = bytes[i + 3..]
                .iter()
                .position(|b| !b.is_ascii_digit() && *b != b';');
            if let Some(offset) = end
                && offset > 0
                && offset < 60
                && bytes[i + 3 + offset] == b'c'
            {
                input.extend_from_slice(&bytes[i + 4 + offset..]);
                return (palette, input, true);
            }
        }
        if !paste && bytes[i..].starts_with(b"\x1b]") {
            let end = (i + 2..bytes.len().min(i + 128)).find_map(|j| {
                if bytes[j] == 7 {
                    Some(j + 1)
                } else if bytes[j..].starts_with(b"\x1b\\") {
                    Some(j + 2)
                } else {
                    None
                }
            });
            if let Some(end) = end {
                if palette.observe(&bytes[i..end]) {
                    i = end;
                    continue;
                }
                input.extend_from_slice(&bytes[i..end]);
                i = end;
                continue;
            }
        }
        input.push(bytes[i]);
        i += 1;
    }
    (palette, input, false)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn standard_rgb_reports_are_normalized_and_validated() {
        let mut p = Palette::default();
        assert!(p.observe(b"\x1b]10;rgb:a/12/abc\x07"));
        assert_eq!(p.foreground, Some([0xaaaa, 0x1212, 0xabca]));
        assert_eq!(p.reply(10).unwrap(), b"\x1b]10;rgb:aaaa/1212/abca\x1b\\");
        let before = p;
        for bad in [
            b"\x1b]10;rgb:zz/00/00\x07".as_slice(),
            b"\x1b]11;rgb:0/0/0/0\x07",
            b"\x1b]11;rgb:00000/0/0\x07",
            b"\x1b]12;rgb:0/0/0\x07",
            b"\x1b]11;rgb:0/0/0",
        ] {
            assert!(!p.observe(bad));
            assert_eq!(p, before);
        }
        assert!(serde_json::from_str::<Palette>(r#"{"background":[65536,0,0]}"#).is_err());
        assert!(
            serde_json::from_str::<Palette>(r#"{"background":[0,0,0],"escape":"bad"}"#).is_err()
        );
    }
    #[test]
    fn startup_probe_preserves_typeahead_paste_and_incomplete_reports() {
        let bytes = b"draft\x1b]11;rgb:1/2/3\x07\x1b[200~\x1b]10;rgb:0/0/0\x07\x1b[201~\x1b]10;rgb:ffff/ffff/ffff\x1b\\\x1b]11;rgb:";
        let (p, input, boundary) = probe_replies(bytes);
        assert!(!boundary);
        assert_eq!(p.background, Some([0x1111, 0x2222, 0x3333]));
        assert_eq!(p.foreground, Some([65535; 3]));
        assert_eq!(
            input,
            b"draft\x1b[200~\x1b]10;rgb:0/0/0\x07\x1b[201~\x1b]11;rgb:"
        );
    }
    #[test]
    fn probe_does_not_extract_reports_from_other_control_strings() {
        let bytes = b"\x1bPignored\x1b]11;rgb:0/0/0\x07\x1b\\";
        let (palette, input, boundary) = probe_replies(bytes);
        assert!(!boundary);
        assert!(palette.is_empty());
        assert_eq!(input, bytes);
    }
    #[test]
    fn da1_bounds_the_probe_and_preserves_input_on_both_sides() {
        let bytes = b"draft\x1b]11;rgb:1/2/3\x07\x1b[?1;2cafter";
        let (p, input, boundary) = probe_replies(bytes);
        assert!(boundary);
        assert_eq!(input, b"draftafter");
        assert_eq!(p.background, Some([0x1111, 0x2222, 0x3333]));
        let (p, input, boundary) = probe_replies(b"\x1b[?62;22c");
        assert!(boundary);
        assert!(p.is_empty());
        assert!(input.is_empty());
        let (_, input, boundary) = probe_replies(b"\x1b[200~\x1b[?1;2c\x1b[201~");
        assert!(!boundary);
        assert_eq!(input, b"\x1b[200~\x1b[?1;2c\x1b[201~");
        for length in 0..bytes.len() - 5 {
            assert!(!probe_replies(&bytes[..length]).2);
        }
    }
}
