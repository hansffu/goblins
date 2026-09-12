//! Public request data only. This crate has no launcher, Nix or approval authority.
use serde::{Deserialize, Serialize};
use std::time::{Duration, Instant};

pub const MAX_FRAME: usize = 4096;
pub const FRAME_TIMEOUT: Duration = Duration::from_secs(3);
pub const REQUEST_SOCKET: &str = "/run/goblins/request.sock";

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub v: u32,
    pub id: String,
    pub op: String,
    pub package: String,
    pub reason: String,
}
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Reply {
    pub v: u32,
    pub id: Option<String>,
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}
impl Reply {
    pub fn new(id: Option<String>, status: &str, message: Option<String>) -> Self {
        Self {
            v: 1,
            id,
            status: status.into(),
            message,
        }
    }
}
pub fn package_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 200
        && name.split('.').all(|part| {
            let mut bytes = part.bytes();
            bytes
                .next()
                .is_some_and(|b| b.is_ascii_alphabetic() || b == b'_')
                && bytes.all(|b| b.is_ascii_alphanumeric() || b"_-".contains(&b))
        })
}
pub fn identifier(name: &str) -> bool {
    (1..=64).contains(&name.len())
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_-".contains(&b))
}
pub fn parse_request(data: &[u8]) -> Result<Request, Reply> {
    let error = |message: &str| Reply::new(None, "error", Some(message.into()));
    if data.len() > MAX_FRAME
        || data.last() != Some(&b'\n')
        || data[..data.len().saturating_sub(1)].contains(&b'\n')
    {
        return Err(error("one bounded JSON line required"));
    }
    // A derived struct rejects duplicate and unknown fields. Value is used only
    // for the two fields whose validation errors must retain a valid request ID.
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Wire {
        v: u32,
        id: String,
        op: String,
        package: serde_json::Value,
        reason: serde_json::Value,
    }
    let raw: Wire =
        serde_json::from_slice(data).map_err(|_| error("invalid request fields or JSON"))?;
    if raw.v != 1 || raw.op != "request-package" {
        return Err(error("unsupported operation"));
    }
    if !identifier(&raw.id) {
        return Err(error("invalid request id"));
    }
    let correlated =
        |message: &str| Reply::new(Some(raw.id.clone()), "error", Some(message.into()));
    let package = raw
        .package
        .as_str()
        .filter(|p| package_name(p))
        .ok_or_else(|| {
            correlated(
                "package must be a nixpkgs attribute such as cowsay or python3Packages.black",
            )
        })?;
    let reason = raw
        .reason
        .as_str()
        .filter(|r| (1..=1024).contains(&r.chars().count()))
        .ok_or_else(|| correlated("reason must contain 1..1024 characters"))?;
    Ok(Request {
        v: 1,
        id: raw.id,
        op: raw.op,
        package: package.into(),
        reason: reason.into(),
    })
}
pub fn line(value: &impl Serialize) -> Result<Vec<u8>, String> {
    let mut bytes = serde_json::to_vec(value).map_err(|e| e.to_string())?;
    bytes.push(b'\n');
    if bytes.len() > MAX_FRAME {
        return Err("frame exceeds 4096 bytes".into());
    }
    Ok(bytes)
}

/// Deadline starts at accept, never at the most recent byte. Call `push` even
/// for an empty read tick so a silent client also expires. Memory stays bounded.
pub struct Frame {
    bytes: Vec<u8>,
    deadline: Instant,
}
impl Frame {
    pub fn new(now: Instant) -> Self {
        Self {
            bytes: Vec::new(),
            deadline: now + FRAME_TIMEOUT,
        }
    }
    pub fn push(&mut self, bytes: &[u8], now: Instant) -> Result<Option<&[u8]>, &'static str> {
        if now >= self.deadline {
            return Err("whole request deadline exceeded");
        }
        if self.bytes.len() + bytes.len() > MAX_FRAME {
            return Err("request too large");
        }
        self.bytes.extend_from_slice(bytes);
        if self.bytes.contains(&b'\n') {
            Ok(Some(&self.bytes))
        } else {
            Ok(None)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn request() -> serde_json::Value {
        serde_json::json!({"v":1,"id":"r1","op":"request-package","package":"jq","reason":"test"})
    }
    #[test]
    fn grammar_and_correlation() {
        for name in ["jq", "cowsay", "python3Packages.black", "unknownPackage"] {
            assert!(package_name(name));
        }
        for name in [
            "../hello",
            "nixpkgs#hello",
            "hello^out",
            "--impure",
            "foo..bar",
            "github:owner/repo",
            "foo.\"bar\"",
        ] {
            let mut req = request();
            req["package"] = name.into();
            assert_eq!(
                parse_request(&line(&req).unwrap())
                    .unwrap_err()
                    .id
                    .as_deref(),
                Some("r1")
            );
        }
        let mut req = request();
        req["reason"] = "\x1b[2J\n approve".into();
        assert_eq!(
            parse_request(&line(&req).unwrap()).unwrap().reason,
            "\x1b[2J\n approve"
        );
    }
    #[test]
    fn reject_malformed_and_authority_fields() {
        for bytes in [
            b"{}\n".as_slice(),
            b"[]\n",
            b"{}\n{}\n",
            b"{\"v\":1,\"v\":1}\n",
            b"\xff\n",
        ] {
            assert!(parse_request(bytes).is_err());
        }
        for (key, value) in [
            ("v", serde_json::json!(true)),
            ("op", "approve".into()),
            ("pid", 1.into()),
            ("flags", 4096.into()),
            ("session", "other".into()),
            ("reason", "".into()),
            ("reason", "x".repeat(1025).into()),
            ("package", "x".repeat(201).into()),
        ] {
            let mut req = request();
            req[key] = value;
            assert!(parse_request(&line(&req).unwrap()).is_err());
        }
        let deep = format!("{}{}\n", "[".repeat(1500), "]".repeat(1500));
        assert!(parse_request(deep.as_bytes()).is_err());
        let duplicate = String::from_utf8(line(&request()).unwrap())
            .unwrap()
            .replace("\"v\":1", "\"v\":1,\"v\":1");
        assert!(parse_request(duplicate.as_bytes()).is_err());
    }
    #[test]
    fn whole_frame_deadline_does_not_slide() {
        let start = Instant::now();
        let mut frame = Frame::new(start);
        for millis in [0, 850, 1700, 2550] {
            assert!(
                frame
                    .push(b" ", start + Duration::from_millis(millis))
                    .unwrap()
                    .is_none()
            );
        }
        assert!(
            frame
                .push(b"\n", start + Duration::from_millis(3400))
                .is_err()
        );
        assert!(
            Frame::new(start)
                .push(&[b'x'; MAX_FRAME + 1], start)
                .is_err()
        );
        let mut frame = Frame::new(start);
        assert!(frame.push(b"{", start).unwrap().is_none());
        assert_eq!(frame.push(b"}\n", start).unwrap(), Some(b"{}\n".as_slice()));
    }
}
