//! A fake daemon on a temporary `host.sock`, for host CLI tests: it answers
//! `initialize`, then records the one call that follows and answers it.
use goblins_jsonrpc as rpc;
use serde_json::Value;
use std::{
    io::Write,
    os::{fd::AsRawFd, unix::net::UnixListener},
    path::PathBuf,
    thread::JoinHandle,
};

pub fn fixtures() -> Value {
    serde_json::from_str(include_str!("../../../libs/api/tests/fixtures.json")).unwrap()
}

pub struct Daemon {
    pub state: PathBuf,
    server: Option<JoinHandle<Value>>,
}

impl Daemon {
    /// A daemon with the fixture features plus `features`, whose one call
    /// gets `result`.
    pub fn start(features: &[&str], result: Value) -> Self {
        let state = goblins_unix::temp_directory().unwrap();
        let listener = UnixListener::bind(state.join("host.sock")).unwrap();
        let mut init = fixtures()["host_initialize"].clone();
        let known = init["features"].as_array_mut().unwrap();
        known.extend(features.iter().map(|f| Value::from(*f)));
        let server = std::thread::spawn(move || {
            assert!(
                goblins_unix::readable(listener.as_raw_fd(), 5000).unwrap(),
                "the CLI did not connect"
            );
            let (mut peer, _) = listener.accept().unwrap();
            peer.set_read_timeout(Some(rpc::TIMEOUT)).unwrap();
            let request = rpc::read(&mut peer).unwrap();
            assert_eq!(request["method"], "initialize");
            peer.write_all(&rpc::encode(&rpc::result(request["id"].clone(), init)).unwrap())
                .unwrap();
            let call = rpc::read(&mut peer).unwrap();
            peer.write_all(&rpc::encode(&rpc::result(call["id"].clone(), result)).unwrap())
                .unwrap();
            call
        });
        Self {
            state,
            server: Some(server),
        }
    }

    /// The call the CLI made.
    pub fn call(mut self) -> Value {
        self.server.take().unwrap().join().unwrap()
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.state);
    }
}
