//! Agent mailbox wire data. Sender identity comes from the authenticated endpoint.
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::io::{self, Read};
use std::{
    fs::OpenOptions,
    os::unix::{fs::OpenOptionsExt, net::UnixStream},
    path::Path,
};

pub const FEATURE: &str = "agent-inbox-v1";
pub const MAX_BODY: usize = 8 * 1024;
pub const MAX_REQUEST: usize = 64 * 1024;

use crate::contract::{self, Method};
pub use crate::contract::{
    MailboxClaimParams as Claim, MailboxMessage as Message, MailboxNextParams as Next,
    MailboxOperation as Operation, MailboxRaidNotice as RaidNotice, MailboxReplyParams as Reply,
    MailboxRequeueParams as Requeue, MailboxSendParams as Send, MailboxState as State,
};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "method", content = "params")]
pub enum Mutation {
    #[serde(rename = "messages.send")]
    Send(Send),
    #[serde(rename = "inbox.next")]
    Next(Next),
    #[serde(rename = "messages.reply")]
    Reply(Reply),
    #[serde(rename = "inbox.complete")]
    Complete(Claim),
    #[serde(rename = "inbox.requeue")]
    Requeue(Requeue),
}

impl Mutation {
    pub fn parse(method: &str, params: Value) -> Result<Self, String> {
        let mutation: Self = serde_json::from_value(serde_json::json!({
            "method": method, "params": params,
        }))
        .map_err(|e| e.to_string())?;
        mutation.validate()?;
        Ok(mutation)
    }

    pub fn method(&self) -> &'static str {
        match self {
            Self::Send(_) => contract::host::MessagesSend::NAME,
            Self::Next(_) => contract::host::InboxNext::NAME,
            Self::Reply(_) => contract::host::MessagesReply::NAME,
            Self::Complete(_) => contract::host::InboxComplete::NAME,
            Self::Requeue(_) => contract::host::InboxRequeue::NAME,
        }
    }

    pub fn key(&self) -> &str {
        match self {
            Self::Send(p) => &p.key,
            Self::Next(p) => &p.key,
            Self::Reply(p) => &p.key,
            Self::Complete(p) => &p.key,
            Self::Requeue(p) => &p.key,
        }
    }

    pub fn validate(&self) -> Result<(), String> {
        if !crate::identifier(self.key()) {
            return Err("invalid operation key".into());
        }
        match self {
            Self::Send(p) => {
                if p.to.is_empty()
                    || p.to.len() > 512
                    || p.conversation
                        .as_ref()
                        .is_some_and(|id| !crate::identifier(id))
                {
                    return Err("invalid recipient or conversation".into());
                }
                validate_body(&p.body)?;
            }
            Self::Reply(p) => {
                validate_claim(&p.message, p.claim_generation)?;
                validate_body(&p.body)?;
            }
            Self::Complete(p) => validate_claim(&p.message, p.claim_generation)?,
            Self::Requeue(p) => {
                validate_claim(&p.message, p.claim_generation)?;
                if p.reason.is_empty() || p.reason.len() > 512 {
                    return Err("requeue reason must contain 1..512 UTF-8 bytes".into());
                }
            }
            Self::Next(_) => (),
        }
        Ok(())
    }
}

fn validate_claim(message: &str, generation: u64) -> Result<(), String> {
    if !crate::identifier(message) || generation == 0 {
        return Err("invalid message ID or claim generation".into());
    }
    Ok(())
}

pub fn validate_body(body: &str) -> Result<(), String> {
    if body.len() > MAX_BODY {
        Err("message exceeds 8 KiB of UTF-8 text".into())
    } else {
        Ok(())
    }
}

/// Used for sender-local file/stdin input. Never transmit a pathname to the host.
/// Read at most one extra byte so oversized input does not allocate without bound.
pub fn read_body(reader: impl Read) -> io::Result<String> {
    let mut bytes = Vec::new();
    reader.take((MAX_BODY + 1) as u64).read_to_end(&mut bytes)?;
    if bytes.len() > MAX_BODY {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "message exceeds 8 KiB",
        ));
    }
    String::from_utf8(bytes).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

pub fn load_body(message: Option<String>, file: Option<&Path>) -> io::Result<String> {
    match (message, file) {
        (Some(text), None) => {
            validate_body(&text).map_err(io::Error::other)?;
            Ok(text)
        }
        (None, Some(path)) if path == Path::new("-") => read_body(io::stdin().lock()),
        (None, Some(path)) => {
            let file = OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_NONBLOCK)
                .open(path)?;
            if !file.metadata()?.is_file() {
                return Err(io::Error::other(
                    "message input must be a regular file or stdin",
                ));
            }
            read_body(file)
        }
        _ => Err(io::Error::other(
            "provide exactly one of --message or --file",
        )),
    }
}

pub fn operation_key(key: Option<String>) -> io::Result<String> {
    let key = match key {
        Some(key) => key,
        None => {
            let mut bytes = [0; 16];
            std::fs::File::open("/dev/urandom")?.read_exact(&mut bytes)?;
            bytes.iter().map(|b| format!("{b:02x}")).collect()
        }
    };
    if !crate::identifier(&key) {
        return Err(io::Error::other("invalid operation key"));
    }
    Ok(key)
}

/// Separate known daemon rejection from transport uncertainty, unlike the
/// general convenience RPC helper which combines both in an io::Error.
pub fn exchange(
    stream: &mut UnixStream,
    id: Value,
    method: &str,
    params: Value,
) -> io::Result<Result<Value, Value>> {
    use std::io::Write;
    stream.set_write_timeout(Some(goblins_jsonrpc::TIMEOUT))?;
    stream.write_all(&goblins_jsonrpc::encode(&goblins_jsonrpc::request(
        id.clone(),
        method,
        params,
    ))?)?;
    let reply = goblins_jsonrpc::read_started(stream, Some(std::time::Instant::now()))?;
    goblins_jsonrpc::validate_response(&reply, &id)?;
    Ok(match reply.get("error") {
        Some(e) => Err(e.clone()),
        None => Ok(reply["result"].clone()),
    })
}

pub fn print_outcome(result: io::Result<Result<Value, Value>>, key: Option<&str>) -> i32 {
    match result {
        Ok(Ok(mut value)) => {
            if let (Some(key), Some(object)) = (key, value.as_object_mut()) {
                object.insert("key".into(), key.into());
            }
            println!("{value}");
            0
        }
        Ok(Err(error)) => {
            eprintln!("goblins: {error}");
            1
        }
        Err(error) => {
            eprintln!(
                "goblins: outcome unknown; retry only with the same key and contents: {error}"
            );
            2
        }
    }
}

impl State {
    pub fn unresolved(self) -> bool {
        matches!(self, Self::Queued | Self::Claimed)
    }
}
/// Sender of daemon-authored operation and raid notices.
pub const NOTICE_SENDER: &str = "goblins";

/// CLI send retains its original absent conversation field.
pub fn send_request(key: String, to: String, body: String) -> serde_json::Result<Value> {
    let mut params = serde_json::to_value(Send {
        key,
        to,
        body,
        conversation: None,
    })?;
    params.as_object_mut().unwrap().remove("conversation");
    Ok(params)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn sender_identity_and_file_paths_are_not_wire_parameters() {
        for extra in ["sender", "file", "session", "claim_generation"] {
            let mut params = json!({"key":"k", "to":"scout", "body":"hello"});
            params[extra] = json!("host");
            assert!(Mutation::parse("messages.send", params).is_err());
        }
    }

    #[test]
    fn content_limits_count_utf8_bytes_and_allow_json_escaping() {
        let body = "\0".repeat(MAX_BODY);
        let mutation = Mutation::parse(
            "messages.send",
            json!({
                "key":"k", "to":"scout", "body":body,
            }),
        )
        .unwrap();
        let encoded = serde_json::to_vec(&mutation).unwrap();
        assert!(encoded.len() > MAX_BODY);
        assert!(encoded.len() < MAX_REQUEST);
        assert!(read_body("🐟".repeat(MAX_BODY / 4).as_bytes()).is_ok());
        assert!(read_body("🐟".repeat(MAX_BODY / 4 + 1).as_bytes()).is_err());
        assert!(read_body(&[0xff][..]).is_err());
        assert_eq!(
            read_body("literal $(false)\nsecond line".as_bytes()).unwrap(),
            "literal $(false)\nsecond line"
        );
    }
}
