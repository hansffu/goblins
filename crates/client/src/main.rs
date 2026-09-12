//! Untrusted request client. No controller or helper dependency.
use goblins_protocol::{MAX_FRAME, REQUEST_SOCKET, Reply, Request, line};
use std::{
    io::{Read, Write},
    os::unix::net::UnixStream,
};

fn parse_reply(data: &[u8], request_id: &str) -> Result<Reply, Box<dyn std::error::Error>> {
    if data.len() > MAX_FRAME
        || data.last() != Some(&b'\n')
        || data[..data.len() - 1].contains(&b'\n')
    {
        return Err("invalid reply framing".into());
    }
    let reply: Reply = serde_json::from_slice(data)?;
    if reply.v != 1
        || reply.id.as_deref() != Some(request_id)
        || !["ready", "denied", "error"].contains(&reply.status.as_str())
    {
        return Err("invalid reply".into());
    }
    Ok(reply)
}

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
        return Ok(0);
    }
    let (package, reason) = parse_args(legacy, &args)?;
    let mut random = [0; 16];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut random))
        .map_err(|e| e.to_string())?;
    let req = Request {
        v: 1,
        id: random.iter().map(|b| format!("{b:02x}")).collect(),
        op: "request-package".into(),
        package,
        reason,
    };
    let bytes = line(&req)?;
    let exchange = || -> Result<Reply, Box<dyn std::error::Error>> {
        let mut socket = UnixStream::connect(REQUEST_SOCKET)?;
        socket.write_all(&bytes)?;
        // Approval/build has no deadline; response framing and memory are bounded.
        let mut data = Vec::new();
        loop {
            let mut chunk = [0; MAX_FRAME + 1];
            let n = socket.read(&mut chunk)?;
            if n == 0 || data.len() + n > MAX_FRAME {
                return Err("missing or oversized reply".into());
            }
            data.extend_from_slice(&chunk[..n]);
            if data.contains(&b'\n') {
                break;
            }
        }
        parse_reply(&data, &req.id)
    };
    match exchange() {
        Ok(reply) => {
            println!("{}", serde_json::to_string(&reply).unwrap());
            Ok(if reply.status == "ready" { 0 } else { 1 })
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
        assert!(parse_args(false, &["serve".into()]).is_err());
        assert!(parse_args(true, &["package".into(), "hello".into()]).is_ok());
    }
    #[test]
    fn replies_must_be_bounded_correlated_terminal_results() {
        for status in ["ready", "denied", "error"] {
            let data = format!("{{\"v\":1,\"id\":\"r1\",\"status\":\"{status}\"}}\n");
            assert!(parse_reply(data.as_bytes(), "r1").is_ok());
            assert!(parse_reply(data.as_bytes(), "other").is_err());
        }
        for data in [
            "",
            "{}",
            "{}\n{}\n",
            "{\"v\":true,\"id\":\"r1\",\"status\":\"ready\"}\n",
            "{\"v\":1,\"v\":1,\"id\":\"r1\",\"status\":\"ready\"}\n",
            "{\"v\":1,\"id\":\"r1\",\"status\":\"pending\"}\n",
        ] {
            assert!(parse_reply(data.as_bytes(), "r1").is_err());
        }
        assert!(parse_reply(&[b' '; MAX_FRAME + 1], "r1").is_err());
    }
}
