//! Bounded worker output with a producer-held completion during cancellation.
//! Only the Nix worker waits for capacity; the dispatcher can still cancel it.
use crate::{
    Result,
    process::{Cancellation, WORKER_TICK},
};
use goblins_runtime::Event;
use std::{
    collections::VecDeque,
    sync::{
        Arc, Mutex,
        mpsc::{SyncSender, TrySendError},
    },
    thread,
};

#[derive(Clone)]
pub(crate) struct Results {
    sender: SyncSender<Event>,
    cancel: Cancellation,
    // A cancelled active operation may own its undelivered progress/summary
    // and the resulting permission reply. No further operation starts then.
    // Keep both producer values so Session can tear down before final output
    // waits for the receiver; this is not an additional event-channel queue.
    held: Arc<Mutex<VecDeque<Event>>>,
}
impl Results {
    pub fn new(sender: SyncSender<Event>, cancel: Cancellation) -> Self {
        Self {
            sender,
            cancel,
            held: Arc::default(),
        }
    }
    fn hold(&self, event: Event) -> Result<()> {
        let mut held = self.held.lock().unwrap_or_else(|p| p.into_inner());
        assert!(
            held.len() < 2,
            "only the active operation and its derived permission reply can be held"
        );
        held.push_back(event);
        Err("session cancelled".into())
    }
    pub fn send(&self, mut event: Event) -> Result<()> {
        if !self
            .held
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .is_empty()
        {
            return self.hold(event);
        }
        loop {
            match self.sender.try_send(event) {
                Ok(()) => return Ok(()),
                Err(TrySendError::Disconnected(_)) => {
                    return Err("runtime event receiver closed".into());
                }
                Err(TrySendError::Full(returned)) => event = returned,
            }
            if self.cancel.check().is_err() {
                return self.hold(event);
            }
            // Bounded producer backpressure, with the same cancellation
            // observation interval as running worker operations.
            thread::sleep(WORKER_TICK);
        }
    }
    /// Called only after Session/its sandbox resources have been dropped.
    /// A live dispatcher drains every completion before final lifecycle.
    pub fn finish_held(&self) -> Result<()> {
        loop {
            let event = self
                .held
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .pop_front();
            let Some(event) = event else {
                return Ok(());
            };
            self.final_event(event)?;
        }
    }
    pub fn final_event(&self, event: Event) -> Result<()> {
        self.sender
            .send(event)
            .map_err(|_| "runtime event receiver closed".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        sync::{Barrier, mpsc},
        time::Duration,
    };
    #[test]
    fn full_output_preserves_active_and_derived_results_and_cleanup_precedes_final_output() {
        let _no_forks = goblins_unix::no_forks();
        let root = goblins_unix::temp_directory().unwrap();
        std::fs::write(root.join("keep"), "fixture").unwrap();
        let resources = root.join("resources");
        let launch=serde_json::from_value(serde_json::json!({"shell":"/unused","posix_shell":"/unused","helper":"/unused","bwrap":"/unused","flake":"unused","client_package":"/unused"})).unwrap();
        let session = crate::session::Session::new_in(
            launch,
            None,
            Cancellation::default(),
            resources.clone(),
        )
        .unwrap();
        let (sender, receiver) = mpsc::sync_channel(1);
        sender
            .send(Event::Mode { server: false })
            .unwrap_or_else(|_| panic!("receiver"));
        let cancel = Cancellation::default();
        let results = Results::new(sender, cancel.clone());
        let barrier = Arc::new(Barrier::new(2));
        let ready = barrier.clone();
        let (cleaned, cleanup) = mpsc::channel();
        let thread = thread::spawn(move || {
            ready.wait();
            assert!(
                results
                    .send(Event::Progress {
                        request: "grant".into(),
                        state: "mounting".into()
                    })
                    .is_err()
            );
            assert!(
                results
                    .send(Event::Granted {
                        reply: goblins_protocol::Reply::new(
                            Some("grant".into()),
                            "error",
                            Some("cancelled".into())
                        ),
                        detail: None,
                        output: None
                    })
                    .is_err()
            );
            // Exercise the real Session destructor while output is still
            // full, before any retained/final event waits for capacity.
            drop(session);
            cleaned.send(()).unwrap();
            results.finish_held().unwrap();
            results
                .final_event(Event::Stopped { exit_code: None })
                .unwrap();
        });
        barrier.wait();
        cancel.cancel();
        cleanup.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(
            !resources.exists(),
            "real Session cleanup precedes output capacity"
        );
        assert!(matches!(receiver.recv().unwrap(), Event::Mode { .. }));
        assert!(matches!(receiver.recv().unwrap(), Event::Progress { .. }));
        assert!(matches!(receiver.recv().unwrap(), Event::Granted { .. }));
        assert!(matches!(receiver.recv().unwrap(), Event::Stopped { .. }));
        thread.join().unwrap();
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn live_backpressure_delivers_every_result_and_receiver_close_releases_owned_manifest() {
        let _no_forks = goblins_unix::no_forks();
        let root = goblins_unix::temp_directory().unwrap();
        let gc = root.join("root");
        std::fs::write(&gc, "rooted").unwrap();
        let (sender, receiver) = mpsc::sync_channel(1);
        let results = Results::new(sender, Cancellation::default());
        let thread = thread::spawn(move || {
            results.send(Event::Mode { server: true }).unwrap();
            results
                .send(Event::Call {
                    serial: 1,
                    result: Ok(serde_json::json!(1)),
                })
                .unwrap();
            results
                .send(Event::Call {
                    serial: 2,
                    result: Ok(serde_json::json!(2)),
                })
                .unwrap();
        });
        assert!(matches!(receiver.recv().unwrap(), Event::Mode { .. }));
        for expected in 1..=2 {
            assert!(matches!(receiver.recv().unwrap(),Event::Call {serial,..} if serial==expected));
        }
        thread.join().unwrap();
        let (sender, receiver) = mpsc::sync_channel(1);
        drop(receiver);
        let results = Results::new(sender, Cancellation::default());
        assert!(
            results
                .send(Event::Manifest {
                    sequence: 1,
                    lease: goblins_runtime::config::ManifestLease::adopt(
                        root.join("manifest"),
                        Some(gc.clone())
                    ),
                    catalogue: Default::default(),
                })
                .is_err()
        );
        assert!(!gc.exists());
        std::fs::remove_dir(root).unwrap();
    }
}
