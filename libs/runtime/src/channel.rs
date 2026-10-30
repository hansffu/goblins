//! Private, nonblocking JSON-RPC stream transport with frame-owned descriptors.
//! Only this endpoint accepts Descriptor-Count; sandbox framing stays unchanged.
use goblins_jsonrpc::Decoder;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::{BTreeSet, VecDeque},
    io, mem,
    net::Shutdown,
    os::{
        fd::{AsRawFd, FromRawFd, OwnedFd, RawFd},
        unix::net::UnixStream,
    },
    time::Instant,
};

// Independent of init's public three-descriptor limit. Linux permits at most
// 253 SCM_RIGHTS descriptors per sendmsg; larger transfers must be batched by
// the message protocol rather than raising the kernel's ancillary limit.
pub const MAX_FDS: usize = 240;
// Relayed sandbox requests/replies (at most 1 MiB) fit with their envelopes.
pub const MAX_BODY: usize = 8 * 1024 * 1024;
const MAX_QUEUED_BYTES: usize = 2 * MAX_BODY;
const MAX_QUEUED_FRAMES: usize = 64;
const MAX_QUEUED_FDS: usize = MAX_FDS * 2;
const HEADER: &str = "Descriptor-Count";

/// A reference to one descriptor attached to this frame, never a raw fd number.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Descriptor {
    #[serde(rename = "$fd")]
    pub index: usize,
}
impl Descriptor {
    pub fn new(index: usize) -> Self {
        Self { index }
    }
}
#[derive(Debug)]
pub struct Frame {
    pub value: Value,
    pub fds: Vec<OwnedFd>,
}
struct Output {
    bytes: Vec<u8>,
    offset: usize,
    // Removed only after the first successful sendmsg, never after EAGAIN.
    fds: Vec<OwnedFd>,
}

pub struct Channel {
    stream: Option<UnixStream>,
    decoder: Decoder,
    input: Vec<u8>,
    partial: bool,
    received: VecDeque<OwnedFd>,
    output: VecDeque<Output>,
    queued_bytes: usize,
    queued_fds: usize,
}
fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}
fn decoder() -> Decoder {
    Decoder::with_headers(MAX_BODY, None, &[HEADER])
}

// Every attached descriptor has exactly one typed reference. Reject aliases,
// unreferenced capabilities and references into another frame's fd list.
fn references(value: &Value, count: usize) -> io::Result<()> {
    fn visit(v: &Value, count: usize, seen: &mut BTreeSet<usize>) -> io::Result<()> {
        match v {
            Value::Object(object) if object.contains_key("$fd") => {
                let index = object["$fd"].as_u64().and_then(|n| usize::try_from(n).ok());
                if object.len() != 1 || index.is_none_or(|n| n >= count || !seen.insert(n)) {
                    return Err(invalid("invalid descriptor reference"));
                }
            }
            Value::Object(object) => {
                for value in object.values() {
                    visit(value, count, seen)?;
                }
            }
            Value::Array(array) => {
                for value in array {
                    visit(value, count, seen)?;
                }
            }
            _ => (),
        }
        Ok(())
    }
    let mut seen = BTreeSet::new();
    visit(value, count, &mut seen)?;
    if seen.len() != count {
        return Err(invalid("unreferenced descriptors"));
    }
    Ok(())
}
impl Channel {
    pub fn new(stream: UnixStream) -> io::Result<Self> {
        stream.set_nonblocking(true)?;
        Ok(Self {
            stream: Some(stream),
            decoder: decoder(),
            input: vec![],
            partial: false,
            received: VecDeque::new(),
            output: VecDeque::new(),
            queued_bytes: 0,
            queued_fds: 0,
        })
    }
    pub fn fd(&self) -> Option<RawFd> {
        self.stream.as_ref().map(AsRawFd::as_raw_fd)
    }
    pub fn closed(&self) -> bool {
        self.stream.is_none()
    }
    pub fn wants_write(&self) -> bool {
        !self.output.is_empty()
    }
    /// A full queue returns WouldBlock without accepting any descriptors.
    /// Callers retain their message until space is available.
    pub fn queue(&mut self, value: &impl Serialize, fds: &mut Vec<OwnedFd>) -> io::Result<()> {
        if self.closed() {
            return Err(io::ErrorKind::BrokenPipe.into());
        }
        if fds.len() > MAX_FDS {
            return Err(invalid(
                "descriptor frame budget exceeded; batch the transfer",
            ));
        }
        let value = serde_json::to_value(value)?;
        references(&value, fds.len())?;
        let body = serde_json::to_vec(&value)?;
        if body.is_empty() || body.len() > MAX_BODY {
            return Err(invalid("control frame too large"));
        }
        let mut bytes = format!(
            "Content-Length: {}\r\n{HEADER}: {}\r\n\r\n",
            body.len(),
            fds.len()
        )
        .into_bytes();
        bytes.extend(body);
        if self.output.len() >= MAX_QUEUED_FRAMES
            || self.queued_bytes + bytes.len() > MAX_QUEUED_BYTES
            || self.queued_fds + fds.len() > MAX_QUEUED_FDS
        {
            return Err(io::ErrorKind::WouldBlock.into());
        }
        self.queued_bytes += bytes.len();
        self.queued_fds += fds.len();
        self.output.push_back(Output {
            bytes,
            offset: 0,
            fds: mem::take(fds),
        });
        Ok(())
    }
    pub fn close(&mut self) {
        if let Some(stream) = self.stream.take() {
            let _ = stream.shutdown(Shutdown::Both);
        }
        self.input.clear();
        self.received.clear();
        self.output.clear();
        self.queued_bytes = 0;
        self.queued_fds = 0;
        self.partial = false;
        self.decoder = decoder();
    }
    pub fn flush(&mut self) -> io::Result<()> {
        let result = self.flush_inner();
        if result.is_err() {
            self.close();
        }
        result
    }
    fn flush_inner(&mut self) -> io::Result<()> {
        let Some(stream) = &self.stream else {
            return Err(io::ErrorKind::BrokenPipe.into());
        };
        // Bound each pump turn even if another process drains continuously.
        for _ in 0..32 {
            let Some(frame) = self.output.front_mut() else {
                break;
            };
            let mut iov = libc::iovec {
                iov_base: frame.bytes[frame.offset..].as_ptr().cast_mut().cast(),
                iov_len: frame.bytes.len() - frame.offset,
            };
            let size = unsafe { libc::CMSG_SPACE((frame.fds.len() * mem::size_of::<RawFd>()) as _) }
                as usize;
            // usize alignment satisfies cmsghdr on every supported Unix target.
            let mut ancillary = vec![0usize; size.div_ceil(mem::size_of::<usize>())];
            let mut msg: libc::msghdr = unsafe { mem::zeroed() };
            msg.msg_iov = &mut iov;
            msg.msg_iovlen = 1;
            if !frame.fds.is_empty() {
                msg.msg_control = ancillary.as_mut_ptr().cast();
                msg.msg_controllen = size;
                unsafe {
                    let header = libc::CMSG_FIRSTHDR(&msg);
                    (*header).cmsg_level = libc::SOL_SOCKET;
                    (*header).cmsg_type = libc::SCM_RIGHTS;
                    (*header).cmsg_len =
                        libc::CMSG_LEN((frame.fds.len() * mem::size_of::<RawFd>()) as _) as _;
                    let data = libc::CMSG_DATA(header).cast::<RawFd>();
                    for (i, fd) in frame.fds.iter().enumerate() {
                        data.add(i).write(fd.as_raw_fd());
                    }
                }
            }
            let sent = unsafe {
                libc::sendmsg(
                    stream.as_raw_fd(),
                    &msg,
                    libc::MSG_NOSIGNAL | libc::MSG_DONTWAIT,
                )
            };
            if sent < 0 {
                let e = io::Error::last_os_error();
                if e.kind() == io::ErrorKind::WouldBlock {
                    return Ok(());
                }
                if e.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(e);
            }
            if sent == 0 {
                return Err(io::ErrorKind::WriteZero.into());
            }
            self.queued_fds -= frame.fds.len();
            frame.fds.clear();
            frame.offset += sent as usize;
            self.queued_bytes -= sent as usize;
            if frame.offset == frame.bytes.len() {
                self.output.pop_front();
            }
        }
        Ok(())
    }
    /// One decoded frame, or None when more input is needed (or after clean EOF).
    /// Any malformed/truncated input closes the socket and every owned fd.
    pub fn receive(&mut self) -> io::Result<Option<Frame>> {
        let result = self.receive_inner();
        if result.is_err() {
            self.close();
        }
        result
    }
    fn decode(&mut self) -> io::Result<Option<Frame>> {
        if self.input.is_empty() {
            return Ok(None);
        }
        self.partial = true;
        let (consumed, body) = self.decoder.push(&self.input, Instant::now())?;
        self.input.drain(..consumed);
        let Some(body) = body else {
            return Ok(None);
        };
        let text = self
            .decoder
            .header(HEADER)
            .ok_or_else(|| invalid("missing descriptor count"))?;
        if text.is_empty() || !text.bytes().all(|b| b.is_ascii_digit()) {
            return Err(invalid("invalid descriptor count"));
        }
        let count: usize = text
            .parse()
            .map_err(|_| invalid("invalid descriptor count"))?;
        if count > MAX_FDS || count != self.received.len() {
            return Err(invalid("descriptor count mismatch"));
        }
        let value = goblins_jsonrpc::parse_with_limit(&body, MAX_BODY).map_err(io::Error::other)?;
        references(&value, count)?;
        let fds = self.received.drain(..count).collect();
        self.decoder = decoder();
        self.partial = false;
        Ok(Some(Frame { value, fds }))
    }
    fn receive_inner(&mut self) -> io::Result<Option<Frame>> {
        // Budget a complete bounded header plus the existing 32 body reads.
        // Header reads are one byte each to preserve ancillary association.
        for _ in 0..goblins_jsonrpc::MAX_HEADER + 32 {
            if let Some(frame) = self.decode()? {
                return Ok(Some(frame));
            }
            let Some(stream) = &self.stream else {
                return Ok(None);
            };
            if self.decoder.expired(Instant::now()) {
                return Err(io::ErrorKind::TimedOut.into());
            }
            let room = (64 * 1024).min(self.decoder.remaining());
            let mut bytes = vec![0u8; room];
            // Do not let one recvmsg span two frames: SCM_RIGHTS has no byte
            // offset within a returned buffer. Read the unknown header one
            // byte at a time, then exactly the current frame's remaining body.
            let first_byte = !self.partial && self.input.is_empty();
            let before_fds = self.received.len();
            let mut iov = libc::iovec {
                iov_base: bytes.as_mut_ptr().cast(),
                iov_len: room,
            };
            let size =
                unsafe { libc::CMSG_SPACE((MAX_FDS * mem::size_of::<RawFd>()) as _) } as usize;
            let mut ancillary = vec![0usize; size.div_ceil(mem::size_of::<usize>())];
            let mut msg: libc::msghdr = unsafe { mem::zeroed() };
            msg.msg_iov = &mut iov;
            msg.msg_iovlen = 1;
            msg.msg_control = ancillary.as_mut_ptr().cast();
            msg.msg_controllen = size;
            let n = unsafe {
                libc::recvmsg(
                    stream.as_raw_fd(),
                    &mut msg,
                    libc::MSG_DONTWAIT | libc::MSG_CMSG_CLOEXEC,
                )
            };
            if n < 0 {
                let e = io::Error::last_os_error();
                if e.kind() == io::ErrorKind::WouldBlock {
                    return Ok(None);
                }
                if e.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(e);
            }
            let mut bad = msg.msg_flags & (libc::MSG_CTRUNC | libc::MSG_TRUNC) != 0;
            unsafe {
                let mut header = libc::CMSG_FIRSTHDR(&msg);
                while !header.is_null() {
                    if (*header).cmsg_level != libc::SOL_SOCKET
                        || (*header).cmsg_type != libc::SCM_RIGHTS
                    {
                        bad = true;
                    } else {
                        let length = (*header).cmsg_len as usize;
                        let prefix = libc::CMSG_LEN(0) as usize;
                        if length < prefix
                            || !(length - prefix).is_multiple_of(mem::size_of::<RawFd>())
                        {
                            bad = true;
                        } else {
                            let data = libc::CMSG_DATA(header).cast::<RawFd>();
                            for i in 0..(length - prefix) / mem::size_of::<RawFd>() {
                                self.received
                                    .push_back(OwnedFd::from_raw_fd(data.add(i).read()));
                            }
                        }
                    }
                    header = libc::CMSG_NXTHDR(&msg, header);
                }
            }
            if !first_byte && self.received.len() != before_fds {
                bad = true;
            }
            if bad || self.received.len() > MAX_FDS {
                return Err(invalid("invalid ancillary data"));
            }
            if n == 0 {
                if self.partial || !self.received.is_empty() {
                    return Err(io::ErrorKind::UnexpectedEof.into());
                }
                self.close();
                return Ok(None);
            }
            self.input.extend_from_slice(&bytes[..n as usize]);
        }
        Ok(None)
    }
}
impl Drop for Channel {
    fn drop(&mut self) {
        self.close();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::{
        fs::File,
        io::{Read, Write},
    };

    fn pipe() -> (File, OwnedFd) {
        let mut pair = [-1; 2];
        assert_eq!(
            unsafe { libc::pipe2(pair.as_mut_ptr(), libc::O_CLOEXEC | libc::O_NONBLOCK) },
            0
        );
        unsafe { (File::from_raw_fd(pair[0]), OwnedFd::from_raw_fd(pair[1])) }
    }
    fn raw_frame(value: Value, count: usize) -> Vec<u8> {
        let body = serde_json::to_vec(&value).unwrap();
        let mut bytes = format!(
            "Content-Length: {}\r\n{HEADER}: {count}\r\n\r\n",
            body.len()
        )
        .into_bytes();
        bytes.extend(body);
        bytes
    }
    fn send_fds(stream: &UnixStream, bytes: &[u8], fds: &[RawFd]) {
        let size = unsafe { libc::CMSG_SPACE(mem::size_of_val(fds) as _) } as usize;
        let mut ancillary = vec![0usize; size.div_ceil(mem::size_of::<usize>())];
        let mut iov = libc::iovec {
            iov_base: bytes.as_ptr().cast_mut().cast(),
            iov_len: bytes.len(),
        };
        let mut msg: libc::msghdr = unsafe { mem::zeroed() };
        msg.msg_iov = &mut iov;
        msg.msg_iovlen = 1;
        if !fds.is_empty() {
            msg.msg_control = ancillary.as_mut_ptr().cast();
            msg.msg_controllen = size;
            unsafe {
                let header = libc::CMSG_FIRSTHDR(&msg);
                (*header).cmsg_level = libc::SOL_SOCKET;
                (*header).cmsg_type = libc::SCM_RIGHTS;
                (*header).cmsg_len = libc::CMSG_LEN(mem::size_of_val(fds) as _) as _;
                std::ptr::copy_nonoverlapping(
                    fds.as_ptr(),
                    libc::CMSG_DATA(header).cast(),
                    fds.len(),
                );
            }
        }
        assert_eq!(
            unsafe { libc::sendmsg(stream.as_raw_fd(), &msg, libc::MSG_NOSIGNAL) },
            bytes.len() as isize
        );
    }
    fn drain(channel: &mut Channel) -> Frame {
        for _ in 0..1000 {
            if let Some(frame) = channel.receive().unwrap() {
                return frame;
            }
        }
        panic!("frame not delivered");
    }
    fn eof(reader: &mut File) {
        let mut b = [0];
        assert_eq!(
            reader.read(&mut b).unwrap(),
            0,
            "received descriptor leaked"
        );
    }
    #[test]
    fn fragmented_and_coalesced_frames_keep_descriptor_ownership() {
        let (mut sender, receiver) = UnixStream::pair().unwrap();
        let mut receiver = Channel::new(receiver).unwrap();
        let (mut r1, w1) = pipe();
        let (mut r2, w2) = pipe();
        let first = raw_frame(
            json!({"jsonrpc":"2.0", "method":"one", "params":{"file":{"$fd":0}}}),
            1,
        );
        send_fds(&sender, &first[..1], &[w1.as_raw_fd()]);
        drop(w1);
        assert!(receiver.receive().unwrap().is_none());
        for byte in &first[1..first.len() - 1] {
            sender.write_all(&[*byte]).unwrap();
            assert!(receiver.receive().unwrap().is_none());
        }
        // The tail of one frame and a whole descriptor-free frame coalesce.
        let mut tail = vec![*first.last().unwrap()];
        tail.extend(raw_frame(
            json!({"jsonrpc":"2.0", "method":"two", "params":{}}),
            0,
        ));
        sender.write_all(&tail).unwrap();
        let mut one = drain(&mut receiver);
        let two = drain(&mut receiver);
        let three = raw_frame(
            json!({"jsonrpc":"2.0", "method":"three", "params":[{"$fd":0}]}),
            1,
        );
        send_fds(&sender, &three, &[w2.as_raw_fd()]);
        drop(w2);
        let mut three = drain(&mut receiver);
        assert_eq!(one.value["method"], "one");
        assert_eq!(two.value["method"], "two");
        assert!(two.fds.is_empty());
        assert_ne!(
            unsafe { libc::fcntl(one.fds[0].as_raw_fd(), libc::F_GETFD) } & libc::FD_CLOEXEC,
            0
        );
        File::from(one.fds.pop().unwrap()).write_all(b"a").unwrap();
        File::from(three.fds.pop().unwrap())
            .write_all(b"b")
            .unwrap();
        let mut b = [0];
        r1.read_exact(&mut b).unwrap();
        assert_eq!(b, [b'a']);
        r2.read_exact(&mut b).unwrap();
        assert_eq!(b, [b'b']);
        eof(&mut r1);
        eof(&mut r2);
    }
    #[test]
    fn short_writes_resume_and_send_descriptors_once() {
        let (left, right) = UnixStream::pair().unwrap();
        let size: libc::c_int = 4096;
        assert_eq!(
            unsafe {
                libc::setsockopt(
                    left.as_raw_fd(),
                    libc::SOL_SOCKET,
                    libc::SO_SNDBUF,
                    (&size as *const libc::c_int).cast(),
                    mem::size_of_val(&size) as _,
                )
            },
            0
        );
        let mut sender = Channel::new(left).unwrap();
        let mut receiver = Channel::new(right).unwrap();
        let (mut reader, writer) = pipe();
        let value = json!({"jsonrpc":"2.0", "method":"large", "params":{"file":{"$fd":0},"data":"x".repeat(2*1024*1024)}});
        sender.queue(&value, &mut vec![writer]).unwrap();
        sender.flush().unwrap();
        assert!(sender.wants_write(), "test must exercise a short write");
        let mut result = None;
        for _ in 0..5000 {
            sender.flush().unwrap();
            if let Some(frame) = receiver.receive().unwrap() {
                result = Some(frame);
                break;
            }
        }
        let frame = result.expect("backpressure stalled");
        assert_eq!(frame.value, value);
        assert_eq!(frame.fds.len(), 1);
        assert!(!sender.wants_write());
        drop(frame);
        eof(&mut reader);
    }
    #[test]
    fn eof_and_protocol_errors_close_all_descriptors() {
        for kind in 0..7 {
            let (mut sender, receiver) = UnixStream::pair().unwrap();
            let mut receiver = Channel::new(receiver).unwrap();
            let (mut reader, writer) = pipe();
            let value = match kind {
                0 => json!({"fd":{"$fd":0}}),              // EOF in header
                1 => json!({"fd":{"$fd":0}}),              // EOF in body
                2 => json!({"fd":{"$fd":1}}),              // out-of-range index
                3 => json!({"a":{"$fd":0},"b":{"$fd":0}}), // alias
                4 => json!({}),                            // unreferenced fd
                5 => json!({"fd":{"$fd":0}}),              // missing descriptor
                _ => json!({"fd":{"$fd":0}}),              // trailing descriptor at EOF
            };
            let mut bytes = raw_frame(
                value,
                if kind == 5 {
                    2
                } else if kind == 6 {
                    0
                } else {
                    1
                },
            );
            if kind == 0 {
                bytes.truncate(8);
            }
            if kind == 1 {
                bytes.pop();
            }
            if kind == 6 {
                bytes = raw_frame(json!({}), 0);
            }
            send_fds(&sender, &bytes, &[writer.as_raw_fd()]);
            drop(writer);
            sender.shutdown(Shutdown::Write).unwrap();
            let mut failed = false;
            for _ in 0..20 {
                match receiver.receive() {
                    Err(_) => {
                        failed = true;
                        break;
                    }
                    Ok(frame) => drop(frame),
                }
            }
            assert!(failed, "protocol error {kind} accepted");
            assert!(receiver.closed());
            eof(&mut reader);
            // Error also drains queued outgoing capabilities, not just input.
            let _ = sender.write(b"x");
        }
    }
    #[test]
    fn a_later_frame_cannot_borrow_a_misattached_descriptor() {
        let (mut sender, stream) = UnixStream::pair().unwrap();
        let mut receiver = Channel::new(stream).unwrap();
        let (mut reader, writer) = pipe();
        send_fds(
            &sender,
            &raw_frame(json!({"jsonrpc":"2.0","method":"a","params":{}}), 0),
            &[writer.as_raw_fd()],
        );
        drop(writer);
        sender
            .write_all(&raw_frame(
                json!({"jsonrpc":"2.0","method":"b","params":{"fd":{"$fd":0}}}),
                1,
            ))
            .unwrap();
        let mut refused = false;
        for _ in 0..8 {
            match receiver.receive() {
                Err(_) => {
                    refused = true;
                    break;
                }
                Ok(None) => (),
                Ok(Some(_)) => {
                    panic!("accepted a frame with a descriptor attached to another frame")
                }
            }
        }
        assert!(refused);
        assert!(receiver.closed());
        eof(&mut reader);
    }

    #[test]
    fn descriptor_attached_after_its_frames_first_byte_is_refused() {
        let (mut sender, stream) = UnixStream::pair().unwrap();
        let mut receiver = Channel::new(stream).unwrap();
        let (mut reader, writer) = pipe();
        let bytes = raw_frame(json!({"fd":{"$fd":0}}), 1);
        sender.write_all(&bytes[..1]).unwrap();
        assert!(receiver.receive().unwrap().is_none());
        send_fds(&sender, &bytes[1..], &[writer.as_raw_fd()]);
        drop(writer);
        let mut refused = false;
        for _ in 0..8 {
            match receiver.receive() {
                Err(_) => {
                    refused = true;
                    break;
                }
                Ok(None) => (),
                Ok(Some(_)) => panic!("accepted late ancillary data"),
            }
        }
        assert!(refused);
        assert!(receiver.closed());
        eof(&mut reader);
    }

    #[test]
    fn full_queue_returns_ownership_and_error_drains_pending_output() {
        let (mut peer, stream) = UnixStream::pair().unwrap();
        let mut channel = Channel::new(stream).unwrap();
        let value = json!({"fd":{"$fd":0}});
        let (mut reader, writer) = pipe();
        channel.queue(&value, &mut vec![writer]).unwrap();
        for _ in 1..MAX_QUEUED_FRAMES {
            channel.queue(&json!({}), &mut vec![]).unwrap();
        }
        let (mut other_reader, writer) = pipe();
        let mut retained = vec![writer];
        assert_eq!(
            channel.queue(&value, &mut retained).unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
        assert_eq!(retained.len(), 1);
        drop(retained);
        eof(&mut other_reader);
        peer.write_all(b"Invalid\r\n\r\n").unwrap();
        assert!(channel.receive().is_err());
        eof(&mut reader);
    }
    #[test]
    fn ancillary_truncation_closes_every_installed_descriptor() {
        let (sender, stream) = UnixStream::pair().unwrap();
        let mut receiver = Channel::new(stream).unwrap();
        let (mut reader, writer) = pipe();
        // Linux permits 253 fds; our receiving ancillary buffer deliberately
        // permits fewer. All installed fds, including those before MSG_CTRUNC,
        // must be closed on the error path.
        send_fds(&sender, b"C", &vec![writer.as_raw_fd(); 253]);
        drop(writer);
        assert!(receiver.receive().is_err());
        eof(&mut reader);
    }
    #[test]
    fn peer_close_drains_unsent_descriptors() {
        let (peer, stream) = UnixStream::pair().unwrap();
        let mut channel = Channel::new(stream).unwrap();
        let (mut reader, writer) = pipe();
        channel
            .queue(&json!({"fd":{"$fd":0}}), &mut vec![writer])
            .unwrap();
        drop(peer);
        assert!(channel.flush().is_err());
        eof(&mut reader);
    }
}
