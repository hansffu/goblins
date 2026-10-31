//! Control actor around the shared engine. Only the Nix worker may block.
use crate::{
    Result,
    engine::{EVENTS, Engine},
};
use goblins_runtime::{
    Command, Event, Runtime,
    channel::{Channel, Frame},
    supervisor::{Decoder, Encoder, Message, outbound::Writer},
};
use std::{collections::VecDeque, io::Write, os::fd::OwnedFd};

pub(crate) fn start(mut channel: Channel, first: Frame) -> Result<()> {
    let mut decoder = Decoder::default();
    let result = (|| {
        let mut received = decoder.receive(first)?;
        let received = loop {
            if let Some(received) = received.take() {
                break received;
            }
            if channel.closed() {
                return Err("EOF during supervisor startup".into());
            }
            crate::entry::wait(&channel);
            received = decoder.receive_channel(&mut channel)?;
        };
        let Message::SupervisorStart(start) = received.message else {
            return Err("SupervisorStart must complete before commands".into());
        };
        let mut engine = Engine::start(
            start.source,
            start.directory,
            start.dimensions,
            start.detached,
        )?;
        if let Some(handoff) = start.handoff {
            if let Err(command) = engine.send(Command::Start(handoff)) {
                decoder.close(&mut channel);
                drop(command);
                return Err("startup handoff refused".into());
            }
        }
        let mut actor = Actor::new(engine);
        if let Some(transfer) = received.adopted {
            actor.output.push_back(Message::Adopted { transfer });
        }
        let result = actor.run(&mut channel, &mut decoder);
        // Dispose raw/partial receive capabilities before outbound typed owners,
        // and close control before dropping the engine/worker on every failure.
        decoder.close(&mut channel);
        actor.writer.close(&mut channel);
        actor.output.clear();
        result
    })();
    decoder.close(&mut channel);
    result
}

pub(super) struct Actor {
    engine: Engine,
    encoder: Encoder,
    writer: Writer,
    pub(super) output: VecDeque<Message>,
    pending_stop: Option<Event>,
    stopping: bool,
    released: bool,
}
impl Actor {
    pub(super) fn new(engine: Engine) -> Self {
        Self {
            engine,
            encoder: Encoder::default(),
            writer: Writer::default(),
            output: VecDeque::new(),
            pending_stop: None,
            stopping: false,
            released: false,
        }
    }
    fn run(&mut self, channel: &mut Channel, decoder: &mut Decoder) -> Result<()> {
        loop {
            self.tick(channel, decoder)?;
            if channel.closed()
                || (self.engine.ended() && self.output.is_empty() && self.writer.drained(channel))
            {
                return Ok(());
            }
            crate::entry::wait(channel);
        }
    }
    pub(super) fn tick(&mut self, channel: &mut Channel, decoder: &mut Decoder) -> Result<()> {
        self.writer.pump(channel)?;
        if self.writer.idle(channel) {
            if let Some(message) = self.output.pop_front() {
                self.writer
                    .begin(self.encoder.prepare(message)?)
                    .map_err(|_| "supervisor writer busy")?;
                self.writer.pump(channel)?;
            }
        }
        // At most two output messages per command: adoption plus refusal, or
        // Stop acknowledgement. Full output pauses input, never waits on Nix.
        for _ in 0..EVENTS {
            if self.output.len() + 2 > EVENTS {
                break;
            }
            let Some(received) = decoder.receive_channel(channel)? else {
                break;
            };
            match received.message {
                Message::Adopted { transfer } => self.writer.adopted(transfer)?,
                Message::Command(command) => {
                    let stop = matches!(command, Command::Stop);
                    if let Err(command) = self.engine.send(command) {
                        if let Some(event) = refuse(command, &self.engine)? {
                            self.output.push_back(Message::Event(event));
                        }
                    }
                    if stop && !self.stopping {
                        self.stopping = true;
                        self.output.push_back(Message::Stopping);
                    }
                }
                _ => return Err("invalid daemon supervisor message".into()),
            }
            if let Some(transfer) = received.adopted {
                self.output.push_back(Message::Adopted { transfer });
            }
        }
        if channel.closed() {
            return Ok(());
        }
        for _ in 0..EVENTS {
            if self.output.len() == EVENTS {
                break;
            }
            if self.pending_stop.is_some()
                && (!self.output.is_empty() || !self.writer.drained(channel))
            {
                break;
            }
            let event = self.pending_stop.take().or_else(|| self.engine.poll());
            let Some(event) = event else {
                break;
            };
            if matches!(event, Event::Stopped { .. })
                && (!self.output.is_empty() || !self.writer.drained(channel))
            {
                // Leave output free to drain and acknowledgements free to
                // arrive; a queued lifecycle barrier could deadlock adoption.
                self.pending_stop = Some(event);
                break;
            }
            self.output.push_back(Message::Event(event));
        }
        if !self.released
            && self.engine.released()
            && self.pending_stop.is_none()
            && self.output.is_empty()
            && self.writer.drained(channel)
        {
            self.released = true;
            self.output.push_back(Message::Released);
        }
        Ok(())
    }
}

const CAPACITY: &str = "resource limit reached";
/// Admission in the daemon still returns -32010 synchronously. Exhaustion
/// after transport acceptance uses the command's existing correlated failure.
fn refuse(command: Command, engine: &Engine) -> Result<Option<Event>> {
    Ok(Some(match command {
        Command::Preview { approval, .. } => Event::Preview {
            approval,
            result: Err(CAPACITY.into()),
        },
        Command::Refresh { approval, .. } => Event::Refresh {
            approval,
            result: Err(CAPACITY.into()),
        },
        Command::Run { serial, .. } | Command::Diff { serial, .. } => Event::Call {
            serial,
            result: Err(CAPACITY.into()),
        },
        Command::Decide { request, .. } => Event::Granted {
            reply: goblins_protocol::Reply::new(Some(request.id), "error", Some(CAPACITY.into())),
            detail: Some(CAPACITY.into()),
            output: None,
        },
        Command::Spawn { id, .. } => Event::SpawnFailed {
            id,
            code: None,
            message: CAPACITY.into(),
        },
        Command::Export { member } => Event::Exported {
            member,
            result: Err(CAPACITY.into()),
        },
        Command::PrepareHostJob { request, .. } => Event::HostJobPrepared {
            request,
            result: Err(CAPACITY.into()),
        },
        Command::RunHostJob { request, .. } => Event::HostJobFailed {
            request,
            error: CAPACITY.into(),
        },
        Command::Attach {
            stream, terminal, ..
        } => {
            let mut stream = std::os::unix::net::UnixStream::from(OwnedFd::from(stream));
            stream.set_nonblocking(true)?;
            let bytes = goblins_jsonrpc::encode(&goblins_jsonrpc::error(
                serde_json::json!(0),
                -32009,
                CAPACITY,
            ))?;
            let _ = stream.write_all(&bytes);
            return Ok(Some(Event::Terminal {
                terminal,
                status: engine.terminal_status(terminal),
                error: None,
            }));
        }
        Command::Start(_) => return Err("startup handoff refused: resource limit reached".into()),
        _ => return Err("supervisor refused a non-refusable command".into()),
    }))
}
