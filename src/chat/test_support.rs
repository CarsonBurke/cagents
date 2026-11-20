//! Real provider decoding and snapshot projection for application view tests.
use super::{Command, Snapshot};
use serde_json::Value;

#[allow(dead_code)]
#[path = "protocol_test_support.rs"]
mod protocol;
#[allow(dead_code)]
#[path = "runtime_test_support.rs"]
mod runtime;

/// An already initialized OMP conversation, without a provider or transport.
pub struct OmpReplay {
    adapter: Box<dyn protocol::Adapter>,
    snapshot: Snapshot,
}

impl OmpReplay {
    pub fn new(session_id: &str) -> Self {
        Self {
            adapter: protocol::omp::fixture(session_id),
            snapshot: Snapshot {
                session_id: Some(session_id.into()),
                ready: true,
                ..Default::default()
            },
        }
    }

    pub fn command_for_run(
        &mut self,
        run: crate::model::RunId,
        command: Command,
    ) -> Result<Vec<Value>, String> {
        let step = self.adapter.command_for_run(run, command)?;
        runtime::project_events_for_test(&mut self.snapshot, step.events);
        Ok(step.writes)
    }

    pub fn receive(&mut self, frame: Value) -> Result<(), String> {
        let step = self.adapter.receive(frame)?;
        runtime::project_events_for_test(&mut self.snapshot, step.events);
        Ok(())
    }

    pub fn snapshot(&self) -> Snapshot {
        self.snapshot.clone()
    }
}
