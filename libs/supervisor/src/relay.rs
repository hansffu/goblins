//! Linux sandbox RPC transport. The daemon receives checked calls, never sockets.
use goblins_jsonrpc::{self as rpc, Decoder};
use goblins_runtime::{Event, policy::Capabilities, sandbox};
use serde_json::Value;
use std::{
    collections::{BTreeMap, VecDeque},
    io::{self, Read, Write},
    os::unix::net::UnixStream,
    time::Instant,
};

const QUEUE: usize = 2 * 1024 * 1024;
const CONNECTIONS: usize = 8;

#[derive(Default)]
pub(crate) struct Relay {
    next: u64,
    peers: BTreeMap<u64, Peer>,
}
struct Waiting {
    id: Value,
    initialize: bool,
}
struct Peer {
    stream: UnixStream,
    decoder: Decoder,
    input: Vec<u8>,
    output: VecDeque<Vec<u8>>,
    offset: usize,
    queued: usize,
    initialized: bool,
    waiting: Option<Waiting>,
    close: bool,
    dead: bool,
    calls: usize,
}
impl Relay {
    pub fn open(&mut self, stream: UnixStream, events: &mut VecDeque<Event>) {
        if self.peers.len() >= CONNECTIONS || stream.set_nonblocking(true).is_err() {
            return;
        }
        self.next += 1;
        let connection = self.next;
        self.peers.insert(
            connection,
            Peer {
                stream,
                decoder: Decoder::new(
                    goblins_protocol::messages::MAX_REQUEST,
                    Some(Instant::now()),
                ),
                input: vec![],
                output: VecDeque::new(),
                offset: 0,
                queued: 0,
                initialized: false,
                waiting: None,
                close: false,
                dead: false,
                calls: 0,
            },
        );
        events.push_back(Event::SandboxOpened { connection });
    }
    /// A notification does not complete the waiting call (host.submitted).
    pub fn send(&mut self, connection: u64, message: Value) {
        if let Some(peer) = self.peers.get_mut(&connection) {
            if let Some(waiting) = &peer.waiting
                && message.get("id") == Some(&waiting.id)
                && (message.get("result").is_some() || message.get("error").is_some())
            {
                if waiting.initialize && message.get("result").is_some() {
                    peer.initialized = true;
                    peer.decoder = Decoder::new(
                        goblins_protocol::messages::MAX_REQUEST,
                        Some(Instant::now()),
                    );
                } else {
                    peer.close = true;
                }
                peer.waiting = None;
            }
            peer.queue(message);
        }
    }
    pub fn close(&mut self, connection: u64) {
        if let Some(peer) = self.peers.get_mut(&connection) {
            peer.close = true;
        }
    }
    pub fn end(&mut self, events: &mut VecDeque<Event>) {
        for (connection, _) in std::mem::take(&mut self.peers) {
            events.push_back(Event::SandboxClosed { connection });
        }
    }
    pub fn tick(
        &mut self,
        capabilities: &Capabilities<'_>,
        stopping: bool,
        events: &mut VecDeque<Event>,
    ) {
        self.tick_at(capabilities, stopping, events, Instant::now());
    }
    fn tick_at(
        &mut self,
        capabilities: &Capabilities<'_>,
        stopping: bool,
        events: &mut VecDeque<Event>,
        now: Instant,
    ) {
        self.peers.retain(|connection, peer| {
            peer.flush();
            if let Some(call) = peer.read(capabilities, stopping, now) {
                events.push_back(Event::SandboxCall {
                    connection: *connection,
                    call,
                });
            }
            // Transport refusals get a bounded write in this round, too.
            if peer.close {
                peer.flush();
            }
            if peer.dead {
                events.push_back(Event::SandboxClosed {
                    connection: *connection,
                });
            }
            !peer.dead
        });
    }
}
impl Peer {
    fn queue(&mut self, value: Value) {
        match rpc::encode(&value) {
            Ok(data) if self.queued + data.len() <= QUEUE => {
                self.queued += data.len();
                self.output.push_back(data);
            }
            _ => self.dead = true,
        }
    }
    fn flush(&mut self) {
        if self.dead {
            return;
        }
        if let Some(front) = self.output.front() {
            match self.stream.write(&front[self.offset..]) {
                Ok(0) => self.dead = true,
                Ok(n) => {
                    self.offset += n;
                    self.queued -= n;
                    if self.offset == front.len() {
                        self.output.pop_front();
                        self.offset = 0;
                    }
                }
                Err(e)
                    if matches!(
                        e.kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                    ) =>
                {
                    ()
                }
                Err(_) => self.dead = true,
            }
        }
        if self.close && self.output.is_empty() {
            self.dead = true;
        }
    }
    fn read(
        &mut self,
        capabilities: &Capabilities<'_>,
        stopping: bool,
        now: Instant,
    ) -> Option<rpc::Call> {
        if self.close || self.dead {
            return None;
        }
        let mut bytes = [0; 16384];
        match self.stream.read(&mut bytes) {
            Ok(0) => {
                self.dead = true;
                return None;
            }
            Ok(n) => {
                // Already-buffered initialize pipelining waits for its reply.
                // Unexpected input on a pending one-shot is a withdrawal.
                if self.waiting.as_ref().is_some_and(|w| !w.initialize)
                    || self.input.len() + n > 32768
                {
                    self.dead = true;
                    return None;
                }
                self.input.extend_from_slice(&bytes[..n]);
            }
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                ) =>
            {
                ()
            }
            Err(_) => {
                self.dead = true;
                return None;
            }
        }
        if self.waiting.is_some() {
            return None;
        }
        match self.decoder.push(&self.input, now) {
            Ok((n, body)) => {
                self.input.drain(..n);
                let body = body?;
                self.calls += 1;
                if self.calls > 2 {
                    self.dead = true;
                    return None;
                }
                self.decoder = Decoder::new(goblins_protocol::messages::MAX_REQUEST, Some(now));
                let call = match rpc::call(&body) {
                    Ok(call) => call,
                    Err(error) => {
                        self.queue(error);
                        self.close = true;
                        return None;
                    }
                };
                // Notifications never authorize work or enter the gate.
                let id = call.id.as_ref()?;
                match sandbox::check(
                    capabilities,
                    &call,
                    body.len(),
                    self.initialized,
                    stopping,
                    !self.input.is_empty(),
                ) {
                    Ok(()) => {
                        self.waiting = Some(Waiting {
                            id: id.clone(),
                            initialize: call.method == "initialize",
                        });
                        Some(call)
                    }
                    Err(refusal) => {
                        let (code, message) = sandbox::fault(refusal);
                        self.queue(rpc::error(id.clone(), code, &message));
                        self.close = true;
                        None
                    }
                }
            }
            Err(error) => {
                self.queue(rpc::error(Value::Null, -32600, &error.to_string()));
                self.close = true;
                None
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use goblins_runtime::policy::{Policy, Support};
    use serde_json::json;
    use std::{net::Shutdown, time::Duration};

    struct Fixture {
        relay: Relay,
        client: UnixStream,
        events: VecDeque<Event>,
        policy: Policy,
        support: Support,
    }
    impl Fixture {
        fn new() -> Self {
            let (stream, client) = UnixStream::pair().unwrap();
            let mut f = Self {
                relay: Relay::default(),
                client,
                events: VecDeque::new(),
                policy: Policy::fixture("shell", &[]),
                support: Support::linux(),
            };
            f.relay.open(stream, &mut f.events);
            assert!(matches!(
                f.events.pop_front(),
                Some(Event::SandboxOpened { connection: 1 })
            ));
            f
        }
        fn write(&mut self, id: Option<u64>, method: &str, params: Value) {
            let mut value = rpc::request(json!(id), method, params);
            if id.is_none() {
                value.as_object_mut().unwrap().remove("id");
            }
            self.client
                .write_all(&rpc::encode(&value).unwrap())
                .unwrap();
        }
        fn tick(&mut self, stopping: bool) {
            self.relay.tick(
                &Capabilities {
                    policy: &self.policy,
                    support: &self.support,
                },
                stopping,
                &mut self.events,
            );
        }
        fn reply(&mut self) -> Value {
            rpc::read_started(&mut self.client, Some(Instant::now())).unwrap()
        }
        fn initialize(&mut self) {
            self.write(Some(1), "initialize", json!({"api":1}));
            self.tick(false);
            assert!(
                matches!(self.events.pop_front(), Some(Event::SandboxCall { call, .. }) if call.method == "initialize")
            );
            self.relay.send(1, rpc::result(json!(1), json!({"api":1})));
            self.tick(false);
            assert_eq!(self.reply()["result"]["api"], 1);
            assert!(self.events.is_empty());
        }
    }
    #[test]
    fn editor_policy_refusal_is_delivered_in_full_before_eof() {
        // A concurrent fork must not retain the peer and hide its real EOF.
        let _no_forks = goblins_unix::no_forks();
        let mut f = Fixture::new();
        f.initialize();
        f.write(
            Some(2),
            "editor.open",
            json!({"files":[{"path":"/tmp/r1"}],"wait":true}),
        );
        f.tick(false);
        assert_eq!(
            f.reply(),
            rpc::error(
                json!(2),
                -32009,
                "no editor is configured; enable editor.emacs in mkGoblins"
            )
        );
        assert_eq!(f.client.read(&mut [0]).unwrap(), 0);
        assert!(matches!(
            f.events.pop_front(),
            Some(Event::SandboxClosed { connection: 1 })
        ));
        assert!(f.events.is_empty());
    }
    #[test]
    fn editor_daemon_refusal_and_close_drain_queued_partial_output_before_eof() {
        use std::os::fd::AsRawFd;
        let _no_forks = goblins_unix::no_forks();
        let mut f = Fixture::new();
        f.policy.editor = Some(goblins_runtime::config::Editor {
            kind: "emacs".into(),
            command: "emacsclient".into(),
        });
        f.initialize();
        f.write(
            Some(2),
            "editor.open",
            json!({"files":[{"path":"/tmp/r1"}],"wait":true}),
        );
        f.tick(false);
        assert!(
            matches!(f.events.pop_front(), Some(Event::SandboxCall { call, .. }) if call.method == "editor.open")
        );
        let size: libc::c_int = 4096;
        assert_eq!(
            unsafe {
                libc::setsockopt(
                    f.relay.peers[&1].stream.as_raw_fd(),
                    libc::SOL_SOCKET,
                    libc::SO_SNDBUF,
                    (&size as *const libc::c_int).cast(),
                    std::mem::size_of_val(&size) as _,
                )
            },
            0
        );
        let queued = json!({"jsonrpc":"2.0","method":"notice","params":{"data":"x".repeat(64000)}});
        let refusal = rpc::result(
            json!(2),
            json!({"code":1,"message":"Goblins: opening the files in Emacs was refused"}),
        );
        f.relay.send(1, queued.clone());
        f.relay.send(1, refusal.clone());
        f.relay.close(1);
        f.tick(false);
        assert!(f.relay.peers[&1].queued > 0);
        assert!(f.events.is_empty());
        f.client.set_nonblocking(true).unwrap();
        let mut received = vec![];
        let mut eof = false;
        for _ in 0..4096 {
            let mut bytes = [0; 8192];
            match f.client.read(&mut bytes) {
                Ok(0) => {
                    eof = true;
                    break;
                }
                Ok(n) => received.extend_from_slice(&bytes[..n]),
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => (),
                other => panic!("read refused-editor output: {other:?}"),
            }
            f.tick(false);
        }
        assert!(eof, "queued editor output never reached EOF");
        let mut input = received.as_slice();
        for expected in [queued, refusal] {
            let (n, body) = Decoder::new(rpc::MAX_BODY, None)
                .push(input, Instant::now())
                .unwrap();
            assert_eq!(rpc::parse(&body.unwrap()).unwrap(), expected);
            input = &input[n..];
        }
        assert!(input.is_empty());
        assert!(matches!(
            f.events.pop_front(),
            Some(Event::SandboxClosed { connection: 1 })
        ));
        assert!(f.events.is_empty());
    }
    #[test]
    fn pipelined_initialize_waits_for_the_daemon_before_gating_the_next_call() {
        let mut f = Fixture::new();
        f.policy.nix = true;
        f.write(Some(1), "initialize", json!({"api":1}));
        f.write(
            Some(2),
            "permissions.request",
            json!({"kind":"package","package":"hello","reason":"t"}),
        );
        f.tick(false);
        assert!(
            matches!(f.events.pop_front(), Some(Event::SandboxCall { call, .. }) if call.method == "initialize")
        );
        f.tick(false);
        assert!(f.events.is_empty());
        f.relay.send(1, rpc::result(json!(1), json!({"api":1})));
        f.tick(false);
        assert_eq!(f.reply()["result"]["api"], 1);
        assert_eq!(f.reply()["error"]["message"], goblins_runtime::NIX_PACKAGES);
        assert!(matches!(
            f.events.pop_front(),
            Some(Event::SandboxClosed { connection: 1 })
        ));
        assert!(f.events.is_empty());
    }
    #[test]
    fn failed_initialize_and_notifications_do_not_authorize_a_next_call() {
        let mut f = Fixture::new();
        f.write(Some(1), "initialize", json!({"api":2}));
        f.write(
            Some(2),
            "permissions.request",
            json!({"kind":"package","package":"hello","reason":"t"}),
        );
        f.tick(false);
        assert!(matches!(
            f.events.pop_front(),
            Some(Event::SandboxCall { .. })
        ));
        f.relay
            .send(1, rpc::error(json!(1), -32002, "unsupported Goblins API"));
        f.tick(false);
        assert_eq!(f.reply()["error"]["code"], -32002);
        assert!(matches!(
            f.events.pop_front(),
            Some(Event::SandboxClosed { .. })
        ));
        assert!(f.events.is_empty());
        let mut f = Fixture::new();
        f.policy.nix = true;
        f.write(None, "permissions.request", json!({"invalid":true}));
        f.tick(false);
        assert!(f.events.is_empty());
        assert!(f.relay.peers[&1].output.is_empty());
        f.write(Some(2), "sessions.status", json!({}));
        f.tick(false);
        assert_eq!(f.reply()["error"]["code"], -32001);
    }
    #[test]
    fn waiting_eof_or_unexpected_bytes_closes_once_and_notification_precedes_reply() {
        for unexpected in [false, true] {
            let mut f = Fixture::new();
            f.initialize();
            f.write(
                Some(2),
                "permissions.request",
                json!({"kind":"package","package":"hello","reason":"t"}),
            );
            f.tick(false);
            assert!(matches!(
                f.events.pop_front(),
                Some(Event::SandboxCall { .. })
            ));
            if unexpected {
                f.client.write_all(b"x").unwrap();
            } else {
                f.client.shutdown(Shutdown::Both).unwrap();
            }
            f.tick(false);
            assert!(matches!(
                f.events.pop_front(),
                Some(Event::SandboxClosed { connection: 1 })
            ));
            f.tick(false);
            assert!(f.events.is_empty());
        }
        let mut f = Fixture::new();
        f.initialize();
        f.write(
            Some(2),
            "permissions.request",
            json!({"kind":"package","package":"hello","reason":"t"}),
        );
        f.tick(false);
        f.events.clear();
        f.relay.send(
            1,
            json!({"jsonrpc":"2.0","method":"host.submitted","params":{"request":"r"}}),
        );
        assert!(f.relay.peers[&1].waiting.is_some());
        f.relay
            .send(1, rpc::result(json!(2), json!({"status":"ready"})));
        f.tick(false);
        assert_eq!(f.reply()["method"], "host.submitted");
        assert_eq!(f.reply()["result"]["status"], "ready");
        assert!(matches!(
            f.events.pop_front(),
            Some(Event::SandboxClosed { .. })
        ));
    }
    #[test]
    fn transport_refusals_keep_codes_messages_and_limits() {
        for (bytes, initialized, code, message) in [
            (
                rpc::encode(&rpc::request(json!(1), "sessions.status", json!({}))).unwrap(),
                false,
                -32001,
                "initialize first",
            ),
            (
                rpc::encode(&rpc::request(json!(1), "unknown", json!({}))).unwrap(),
                true,
                -32601,
                "method unavailable on sandbox endpoint",
            ),
            (
                b"Content-Length: 1\r\n\r\nx".to_vec(),
                false,
                -32700,
                "invalid JSON",
            ),
            (
                rpc::encode(&json!([])).unwrap(),
                false,
                -32600,
                "invalid request envelope",
            ),
            (
                rpc::encode(&rpc::request(
                    json!(1),
                    "sessions.status",
                    json!({"extra":"x".repeat(5000)}),
                ))
                .unwrap(),
                true,
                -32602,
                "request exceeds method frame limit",
            ),
        ] {
            let mut f = Fixture::new();
            f.relay.peers.get_mut(&1).unwrap().initialized = initialized;
            f.client.write_all(&bytes).unwrap();
            f.tick(false);
            let reply = f.reply();
            assert_eq!(reply["error"]["code"], code);
            assert_eq!(reply["error"]["message"], message);
            assert!(
                !f.events
                    .iter()
                    .any(|e| matches!(e, Event::SandboxCall { .. }))
            );
        }
        for bytes in [
            b"X: 1\r\n\r\n".to_vec(),
            format!(
                "Content-Length: {}\r\n\r\n",
                goblins_protocol::messages::MAX_REQUEST + 1
            )
            .into_bytes(),
        ] {
            let mut f = Fixture::new();
            f.client.write_all(&bytes).unwrap();
            f.tick(false);
            assert_eq!(f.reply()["error"]["code"], -32600);
        }
        let mut f = Fixture::new();
        f.relay.tick_at(
            &Capabilities {
                policy: &f.policy,
                support: &f.support,
            },
            false,
            &mut f.events,
            Instant::now() + Duration::from_secs(4),
        );
        assert_eq!(f.reply()["error"]["code"], -32600);
        let mut f = Fixture::new();
        for _ in 0..3 {
            f.write(None, "initialize", json!({"api":1}));
            f.tick(false);
        }
        assert!(matches!(
            f.events.pop_front(),
            Some(Event::SandboxClosed { .. })
        ));
        assert!(f.relay.peers.is_empty());
        let mut f = Fixture::new();
        f.initialize();
        f.write(
            Some(2),
            "permissions.request",
            json!({"kind":"package","package":"hello","reason":"t"}),
        );
        f.client.write_all(b"trailing").unwrap();
        f.tick(false);
        assert_eq!(f.reply()["error"]["message"], "unexpected trailing input");
    }
    #[test]
    fn stopping_relays_initialize_and_messaging_but_conflicts_before_other_checks() {
        for method in ["unknown", "permissions.request"] {
            let mut f = Fixture::new();
            f.policy.nix = true;
            f.initialize();
            f.write(Some(2), method, json!({"invalid":true}));
            f.tick(true);
            assert_eq!(f.reply()["error"]["code"], -32009);
        }
        let mut f = Fixture::new();
        f.write(Some(1), "sessions.status", json!({}));
        f.tick(true);
        assert_eq!(f.reply()["error"]["code"], -32001);
        let mut f = Fixture::new();
        f.write(Some(1), "initialize", json!({"api":1}));
        f.tick(true);
        assert!(
            matches!(f.events.pop_front(), Some(Event::SandboxCall { call, .. }) if call.method == "initialize")
        );
        f.relay.send(1, rpc::result(json!(1), json!({"api":1})));
        f.tick(true);
        f.reply();
        f.write(Some(2), "inbox.status", json!({}));
        f.tick(true);
        assert!(
            matches!(f.events.pop_front(), Some(Event::SandboxCall { call, .. }) if call.method == "inbox.status")
        );
    }
    #[test]
    fn output_cap_and_connection_cap_close_only_the_affected_peer() {
        let mut f = Fixture::new();
        for _ in 0..4 {
            f.relay.send(1, json!({"data":"x".repeat(700_000)}));
        }
        assert!(f.relay.peers[&1].dead);
        assert!(f.relay.peers[&1].queued <= QUEUE);
        f.tick(false);
        assert!(matches!(
            f.events.pop_front(),
            Some(Event::SandboxClosed { connection: 1 })
        ));
        let mut clients = vec![];
        for _ in 0..9 {
            let (stream, client) = UnixStream::pair().unwrap();
            f.relay.open(stream, &mut f.events);
            clients.push(client);
        }
        assert_eq!(f.relay.peers.len(), 8);
        assert_eq!(f.events.len(), 8);
        f.relay.end(&mut f.events);
        assert!(f.relay.peers.is_empty());
        assert_eq!(
            f.events
                .iter()
                .filter(|e| matches!(e, Event::SandboxClosed { .. }))
                .count(),
            8
        );
    }
}
