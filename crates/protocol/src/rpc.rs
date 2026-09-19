//! JSON-RPC 2.0 values and bounded Content-Length framing, without host policy.
use serde::{
    Deserialize, Serialize,
    de::{self, MapAccess, SeqAccess, Visitor},
};
use serde_json::{Value, json};
use std::{
    fmt,
    io::{self, Read, Write},
    time::{Duration, Instant},
};

pub const MAX_HEADER: usize = 256;
pub const MAX_BODY: usize = 1024 * 1024;
pub const TIMEOUT: Duration = Duration::from_secs(3);

// Value normally accepts duplicate keys. Reject them recursively before typed
// deserialization, so operation params have the same strictness as the envelope.
struct Unique(Value);
impl<'de> Deserialize<'de> for Unique {
    fn deserialize<D: de::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = Unique;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("JSON without duplicate keys")
            }
            fn visit_bool<E: de::Error>(self, v: bool) -> Result<Unique, E> {
                Ok(Unique(v.into()))
            }
            fn visit_i64<E: de::Error>(self, v: i64) -> Result<Unique, E> {
                Ok(Unique(v.into()))
            }
            fn visit_u64<E: de::Error>(self, v: u64) -> Result<Unique, E> {
                Ok(Unique(v.into()))
            }
            fn visit_f64<E: de::Error>(self, v: f64) -> Result<Unique, E> {
                Ok(Unique(json!(v)))
            }
            fn visit_str<E: de::Error>(self, v: &str) -> Result<Unique, E> {
                Ok(Unique(v.into()))
            }
            fn visit_unit<E: de::Error>(self) -> Result<Unique, E> {
                Ok(Unique(Value::Null))
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut a: A) -> Result<Unique, A::Error> {
                let mut v = Vec::new();
                while let Some(Unique(x)) = a.next_element()? {
                    v.push(x);
                }
                Ok(Unique(v.into()))
            }
            fn visit_map<A: MapAccess<'de>>(self, mut a: A) -> Result<Unique, A::Error> {
                let mut v = serde_json::Map::new();
                while let Some((k, Unique(x))) = a.next_entry::<String, Unique>()? {
                    if v.insert(k, x).is_some() {
                        return Err(de::Error::custom("duplicate field"));
                    }
                }
                Ok(Unique(v.into()))
            }
        }
        d.deserialize_any(V)
    }
}
pub fn parse(bytes: &[u8]) -> Result<Value, String> {
    if bytes.len() > MAX_BODY {
        return Err("body too large".into());
    }
    let mut depth = 0usize;
    let mut quoted = false;
    let mut escaped = false;
    for b in bytes {
        if quoted {
            if escaped {
                escaped = false;
            } else if *b == b'\\' {
                escaped = true;
            } else if *b == b'"' {
                quoted = false;
            }
        } else {
            match b {
                b'"' => quoted = true,
                b'[' | b'{' => {
                    depth += 1;
                    if depth > 32 {
                        return Err("JSON nesting exceeds 32".into());
                    }
                }
                b']' | b'}' => depth = depth.saturating_sub(1),
                _ => (),
            }
        }
    }
    serde_json::from_slice::<Unique>(bytes)
        .map(|u| u.0)
        .map_err(|e| e.to_string())
}
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Call {
    pub jsonrpc: String,
    #[serde(default)]
    pub id: Option<Value>,
    pub method: String,
    #[serde(default = "empty")]
    pub params: Value,
}
fn empty() -> Value {
    json!({})
}
pub fn call(bytes: &[u8]) -> Result<Call, Value> {
    let value = parse(bytes).map_err(|_| error(Value::Null, -32700, "invalid JSON"))?;
    if value.get("id").is_some_and(Value::is_null) {
        return Err(error(Value::Null, -32600, "null IDs are unsupported"));
    }
    let c: Call = serde_json::from_value(value)
        .map_err(|_| error(Value::Null, -32600, "invalid request envelope"))?;
    if c.jsonrpc != "2.0" || c.method.len() > 64 || c.id.as_ref().is_some_and(|id| !valid_id(id)) {
        return Err(error(Value::Null, -32600, "invalid JSON-RPC version or ID"));
    }
    Ok(c)
}
pub fn valid_id(id: &Value) -> bool {
    id.as_str().is_some_and(crate::identifier)
        || id.as_u64().is_some_and(|n| n <= 9_007_199_254_740_991)
}
pub fn request(id: Value, method: &str, params: Value) -> Value {
    json!({"jsonrpc":"2.0","id":id,"method":method,"params":params})
}
pub fn result(id: Value, result: Value) -> Value {
    json!({"jsonrpc":"2.0","id":id,"result":result})
}
pub fn error(id: Value, code: i32, message: &str) -> Value {
    json!({"jsonrpc":"2.0","id":id,"error":{"code":code,"message":message}})
}
pub fn encode(v: &impl Serialize) -> io::Result<Vec<u8>> {
    let body = serde_json::to_vec(v)?;
    if body.len() > MAX_BODY {
        return Err(io::Error::other("output too large"));
    }
    let mut data = format!("Content-Length: {}\r\n\r\n", body.len()).into_bytes();
    data.extend(body);
    Ok(data)
}

/// Feed only one frame at a time. Surplus bytes are returned to the caller for
/// the next frame (or rejected on the one-request sandbox connection).
pub struct Decoder {
    data: Vec<u8>,
    body: Option<(usize, usize)>,
    deadline: Option<Instant>,
    limit: usize,
}
impl Decoder {
    pub fn new(limit: usize, start: Option<Instant>) -> Self {
        Self {
            data: vec![],
            body: None,
            deadline: start.map(|n| n + TIMEOUT),
            limit,
        }
    }
    pub fn remaining(&self) -> usize {
        self.body
            .map(|(offset, length)| offset + length - self.data.len())
            .unwrap_or(1)
    }
    pub fn expired(&self, now: Instant) -> bool {
        self.deadline.is_some_and(|d| now >= d)
    }
    pub fn push(&mut self, bytes: &[u8], now: Instant) -> io::Result<(usize, Option<Vec<u8>>)> {
        if self.deadline.is_none() && !bytes.is_empty() {
            self.deadline = Some(now + TIMEOUT);
        }
        if self.expired(now) {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "whole-frame deadline exceeded",
            ));
        }
        for (i, b) in bytes.iter().enumerate() {
            self.data.push(*b);
            if self.body.is_none() {
                if self.data.len() > MAX_HEADER {
                    return Err(io::Error::other("header too large"));
                }
                if self.data.ends_with(b"\r\n\r\n") {
                    let text = std::str::from_utf8(&self.data).map_err(io::Error::other)?;
                    let mut length = None;
                    let mut content_type = false;
                    for line in text[..text.len() - 4].split("\r\n") {
                        let (key, value) = line
                            .split_once(':')
                            .ok_or_else(|| io::Error::other("invalid header"))?;
                        let value = value.trim();
                        if key.eq_ignore_ascii_case("Content-Length")
                            && length.is_none()
                            && !value.is_empty()
                            && value.bytes().all(|b| b.is_ascii_digit())
                        {
                            length = Some(value.parse::<usize>().map_err(io::Error::other)?);
                        } else if key.eq_ignore_ascii_case("Content-Type")
                            && !content_type
                            && [
                                "application/json",
                                "application/vscode-jsonrpc; charset=utf-8",
                            ]
                            .contains(&value)
                        {
                            content_type = true;
                        } else {
                            return Err(io::Error::other("invalid or duplicate header"));
                        }
                    }
                    let length = length
                        .filter(|n| *n > 0 && *n <= self.limit)
                        .ok_or_else(|| io::Error::other("invalid body length"))?;
                    self.body = Some((self.data.len(), length));
                }
            }
            if let Some((offset, length)) = self.body
                && self.data.len() == offset + length
            {
                return Ok((i + 1, Some(self.data[offset..].to_vec())));
            }
        }
        Ok((bytes.len(), None))
    }
}
/// Blocking client reader: idle approval time is unlimited, but once the first
/// reply byte arrives every subsequent read shares one monotonic deadline.
pub fn read(stream: &mut std::os::unix::net::UnixStream) -> io::Result<Value> {
    read_started(stream, None)
}
pub(crate) fn read_started(
    stream: &mut std::os::unix::net::UnixStream,
    start: Option<Instant>,
) -> io::Result<Value> {
    stream.set_read_timeout(start.map(|_| TIMEOUT))?;
    let mut decoder = Decoder::new(MAX_BODY, start);
    let mut byte = [0; 8192];
    let mut deadline: Option<Instant> = start.map(|n| n + TIMEOUT);
    loop {
        if let Some(d) = deadline {
            stream.set_read_timeout(Some(
                d.saturating_duration_since(Instant::now())
                    .max(Duration::from_millis(1)),
            ))?;
        }
        let room = byte.len().min(decoder.remaining());
        let n = stream.read(&mut byte[..room])?;
        if n == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "incomplete reply",
            ));
        }
        let now = Instant::now();
        deadline.get_or_insert(now + TIMEOUT);
        if let (_, Some(body)) = decoder.push(&byte[..n], now)? {
            return parse(&body).map_err(io::Error::other);
        }
    }
}
/// Validate the envelope before a client interprets application outcomes.
pub fn validate_response(reply: &Value, id: &Value) -> io::Result<()> {
    let object = reply
        .as_object()
        .ok_or_else(|| io::Error::other("invalid response"))?;
    if object.len() != 3
        || reply["jsonrpc"] != "2.0"
        || &reply["id"] != id
        || reply.get("result").is_some() == reply.get("error").is_some()
    {
        return Err(io::Error::other("invalid correlated response"));
    }
    if let Some(error) = reply.get("error") {
        let fields = error
            .as_object()
            .ok_or_else(|| io::Error::other("invalid error"))?;
        if error["code"].as_i64().is_none()
            || error["message"].as_str().is_none()
            || fields
                .keys()
                .any(|k| !["code", "message", "data"].contains(&k.as_str()))
        {
            return Err(io::Error::other("invalid RPC error"));
        }
    }
    Ok(())
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PermissionResult {
    pub request: String,
    pub status: String,
    pub message: Option<String>,
}
pub fn exchange(
    stream: &mut std::os::unix::net::UnixStream,
    id: Value,
    method: &str,
    params: Value,
) -> io::Result<Value> {
    stream.set_write_timeout(Some(TIMEOUT))?;
    stream.write_all(&encode(&request(id.clone(), method, params))?)?;
    let reply = read_started(stream, Some(Instant::now()))?;
    validate_response(&reply, &id)?;
    if let Some(e) = reply.get("error") {
        return Err(io::Error::other(e.to_string()));
    }
    Ok(reply["result"].clone())
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PermissionParams {
    pub kind: String,
    pub package: String,
    pub reason: String,
}
impl PermissionParams {
    pub fn validate(&self) -> bool {
        self.kind == "package"
            && crate::package_name(&self.package)
            && (1..=1024).contains(&self.reason.chars().count())
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn hostile_frames_and_deadlines() {
        let now = Instant::now();
        for data in [
            b"Content-Length: 1\r\nContent-Length: 1\r\n\r\nx".as_slice(),
            b"Content-Length: 99999999\r\n\r\n",
            b"Content-Length: -1\r\n\r\n",
            b"X: 1\r\n\r\n",
            &[b'a'; 257],
        ] {
            assert!(Decoder::new(4096, Some(now)).push(data, now).is_err());
        }
        let encoded = encode(&json!({"utf8":"🐟"})).unwrap();
        let mut d = Decoder::new(4096, Some(now));
        let mut result = None;
        for byte in &encoded {
            result = d.push(&[*byte], now).unwrap().1;
        }
        assert_eq!(parse(&result.unwrap()).unwrap()["utf8"], "🐟");
        let mut d = Decoder::new(4096, Some(now));
        for i in 0..3 {
            assert!(d.push(b"C", now + Duration::from_secs(i)).is_ok());
        }
        assert!(d.push(b"x", now + TIMEOUT).is_err());
        assert!(
            Decoder::new(4096, Some(now))
                .push(&[], now + TIMEOUT)
                .is_err()
        );
    }
    #[test]
    fn strict_envelope_and_nested_fields() {
        for s in [r#"{"a":1,"a":2}"#, r#"{"params":{"a":1,"a":2}}"#] {
            assert!(parse(s.as_bytes()).is_err());
        }
        for s in [
            r#"[]"#,
            r#"{"jsonrpc":"1.0","id":1,"method":"x"}"#,
            r#"{"jsonrpc":"2.0","id":true,"method":"x"}"#,
        ] {
            assert!(call(s.as_bytes()).is_err());
        }
        assert!(parse(format!("{}{}", "[".repeat(33), "]".repeat(33)).as_bytes()).is_err());
        assert!(
            call(br#"{"jsonrpc":"2.0","method":"x"}"#)
                .unwrap()
                .id
                .is_none()
        );
    }
}
