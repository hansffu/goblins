//! Untrusted request client. No controller or helper dependency.
use goblins_protocol::{REQUEST_SOCKET, rpc};
use serde_json::json;
use std::{io::Read, os::unix::net::UnixStream};

fn parse_args(legacy: bool, args: &[String]) -> Result<(String, String), String> {
    let mut args = args.iter();
    let kind = if legacy { "package" } else { "request-package" };
    if args.next().map(String::as_str) != Some(kind) {
        return Err("usage: goblins request-package PACKAGE [--reason TEXT]".into());
    }
    let mut package = None;
    let mut reason = "Requested from the sandbox shell".to_string();
    let mut positional = false;
    while let Some(arg) = args.next() {
        if !positional && arg == "--reason" {
            reason = args.next().ok_or("--reason requires text")?.clone();
        } else if !positional && arg == "--" {
            positional = true;
        } else if !positional && arg.starts_with('-') {
            return Err(format!("unknown option: {arg}"));
        } else if package.replace(arg.clone()).is_some() {
            return Err("expected exactly one package attribute".into());
        }
    }
    Ok((package.ok_or("missing package attribute")?, reason))
}
fn run() -> Result<i32, String> {
    let mut args = std::env::args();
    let legacy = args.next().is_some_and(|a| {
        std::path::Path::new(&a)
            .file_name()
            .is_some_and(|n| n == "goblins-request")
    });
    let args: Vec<_> = args.collect();
    if args.iter().any(|arg| arg == "--help" || arg == "-h") {
        println!(
            "usage: goblins {} PACKAGE [--reason TEXT]",
            if legacy { "package" } else { "request-package" }
        );
        if !legacy {
            println!("       goblins detatch (alias: detach)");
        }
        return Ok(0);
    }
    if !legacy
        && args
            .first()
            .is_some_and(|arg| arg == "detatch" || arg == "detach")
    {
        if args.len() != 1 {
            return Err("usage: goblins detatch".into());
        }
        let mut socket = UnixStream::connect(REQUEST_SOCKET).map_err(|e| e.to_string())?;
        let init = rpc::exchange(&mut socket, json!(0), "initialize", json!({"api":1}))
            .map_err(|e| e.to_string())?;
        if init["api"] != 1 || init["role"] != "sandbox" {
            return Err("incompatible daemon".into());
        }
        let reply = rpc::exchange(&mut socket, json!(1), "sessions.detach", json!({}))
            .map_err(|e| e.to_string())?;
        if reply["accepted"] != true {
            return Err("detach was not accepted".into());
        }
        return Ok(0);
    }
    let (package, reason) = parse_args(legacy, &args)?;
    let mut random = [0; 16];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut random))
        .map_err(|e| e.to_string())?;
    let id: String = random.iter().map(|b| format!("{b:02x}")).collect();
    let exchange = || -> Result<serde_json::Value, Box<dyn std::error::Error>> {
        let mut socket = UnixStream::connect(REQUEST_SOCKET)?;
        let init = rpc::exchange(&mut socket, json!(0), "initialize", json!({"api":1}))?;
        if init["api"] != 1 || init["role"] != "sandbox" {
            return Err("incompatible daemon".into());
        }
        let params = json!({"kind":"package","package":package,"reason":reason});
        socket.set_write_timeout(Some(rpc::TIMEOUT))?;
        use std::io::Write;
        socket.write_all(&rpc::encode(&rpc::request(
            json!(id),
            "permissions.request",
            params,
        ))?)?;
        let reply = rpc::read(&mut socket)?;
        rpc::validate_response(&reply, &json!(id))?;
        if let Some(error) = reply.get("error") {
            return Ok(json!({"status":"error","message":error}));
        }
        let result: rpc::PermissionResult = serde_json::from_value(reply["result"].clone())?;
        if !["ready", "denied", "error"].contains(&result.status.as_str())
            || !goblins_protocol::identifier(&result.request)
        {
            return Err("invalid terminal result".into());
        }
        Ok(serde_json::to_value(result)?)
    };
    match exchange() {
        Ok(reply) => {
            println!("{}", serde_json::to_string(&reply).unwrap());
            Ok(if reply["status"] == "ready" { 0 } else { 1 })
        }
        Err(e) => {
            eprintln!("goblins-request: outcome unknown; do not retry automatically: {e}");
            Ok(2)
        }
    }
}
fn main() {
    std::process::exit(match run() {
        Ok(code) => code,
        Err(e) => {
            eprintln!("goblins: {e}");
            2
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn request_options_can_precede_or_follow_the_package() {
        for args in [
            ["request-package", "hello", "--reason", "test"],
            ["request-package", "--reason", "test", "hello"],
        ] {
            assert_eq!(
                parse_args(false, &args.map(String::from)).unwrap(),
                ("hello".into(), "test".into())
            );
        }
        assert!(parse_args(false, &["tui".into()]).is_err());
        assert!(parse_args(true, &["package".into(), "hello".into()]).is_ok());
    }
}
