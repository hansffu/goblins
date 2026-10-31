//! Temporary in-process adapter over the same engine used by the supervisor.
use goblins_runtime::{Command, Event, Runtime, Source};
use std::path::PathBuf;

pub struct Local(crate::engine::Engine);
impl Local {
    pub fn start(
        source: Source,
        directory: PathBuf,
        dimensions: (u16, u16),
        detached: bool,
    ) -> crate::Result<Self> {
        crate::engine::Engine::start(source, directory, dimensions, detached).map(Self)
    }
}
impl Runtime for Local {
    fn send(&mut self, command: Command) -> Result<(), Command> {
        self.0.send(command)
    }
    fn poll(&mut self) -> Option<Event> {
        self.0.poll()
    }
    fn released(&self) -> bool {
        self.0.released()
    }
    fn ended(&self) -> bool {
        self.0.ended()
    }
}
