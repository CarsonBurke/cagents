//! Sends watching windows the device's load.
//!
//! One sampling thread runs while at least one connection watches, and ends
//! at its first pass with none, so an unwatched daemon spends nothing here,
//! not even an idle thread. Each pass reads a few kernel counters, and a
//! reading goes out only when a figure a window shows has moved.
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex, MutexGuard, Weak},
    time::Duration,
};

use crate::{
    device_stats::{DeviceStats, SAMPLE_INTERVAL, Sampler, Shown},
    model::HostSessionProof,
    terminal_ipc::ServerMessage,
};

use super::{
    delivery::{DeliveryQueue, Outbound},
    service::{self, Next},
    state::ClientId,
};

/// The second reading follows the first this soon, since processor use is
/// the difference between two, so a newly opened window waits only this
/// long for its first processor figure.
const FIRST_INTERVAL: Duration = Duration::from_millis(500);

#[derive(Clone, Default)]
pub(super) struct Watch(Arc<Mutex<Inner>>);

#[derive(Default)]
struct Inner {
    watchers: BTreeMap<ClientId, Watcher>,
    sampling: bool,
    revision: u64,
    latest: Option<(DeviceStats, Shown)>,
}

struct Watcher {
    delivery: Weak<DeliveryQueue>,
    proof: Arc<HostSessionProof>,
}

impl Watch {
    /// Subscribes `client` and returns the latest reading, with the revision
    /// every later one supersedes. The first watcher starts the sampler, and
    /// the reading then arrives in its first push.
    pub(super) fn watch(
        &self,
        client: ClientId,
        delivery: Weak<DeliveryQueue>,
        proof: Arc<HostSessionProof>,
    ) -> Result<(u64, Option<DeviceStats>), String> {
        let mut inner = self.inner();
        let answer = Self::subscribe(&mut inner, client, delivery, proof);
        if !inner.sampling {
            let watch = self.clone();
            let mut sampler = Sampler::new();
            let mut first = true;
            service::spawn("device-stats", SAMPLE_INTERVAL, move || {
                let next = watch.pass(&mut sampler);
                if next == Next::Continue {
                    std::thread::sleep(if std::mem::take(&mut first) {
                        FIRST_INTERVAL
                    } else {
                        SAMPLE_INTERVAL
                    });
                }
                next
            })
            .map_err(|error| format!("The device's load could not be read: {error}"))?;
            inner.sampling = true;
        }
        Ok(answer)
    }

    fn subscribe(
        inner: &mut Inner,
        client: ClientId,
        delivery: Weak<DeliveryQueue>,
        proof: Arc<HostSessionProof>,
    ) -> (u64, Option<DeviceStats>) {
        inner.watchers.insert(client, Watcher { delivery, proof });
        if !inner.sampling {
            // A reading from an earlier watch is stale by now.
            inner.latest = None;
        }
        (inner.revision, inner.latest.map(|(stats, _)| stats))
    }

    pub(super) fn unwatch(&self, client: ClientId) {
        self.inner().watchers.remove(&client);
    }

    /// Reads the device and sends the watchers anything new, or ends the
    /// sampler when nobody watches.
    fn pass(&self, sampler: &mut Sampler) -> Next {
        if !self.prune() {
            return Next::Stop;
        }
        // Read outside the lock: a home directory on a stalled network file
        // system may hold up `statvfs`, and that must not hold up a watch.
        let stats = sampler.sample();
        let shown = stats.shown();
        let mut inner = self.inner();
        if inner.latest.is_some_and(|(_, previous)| previous == shown) {
            return Next::Continue;
        }
        inner.revision += 1;
        inner.latest = Some((stats, shown));
        let revision = inner.revision;
        inner.watchers.retain(|_, watcher| {
            let Some(delivery) = watcher.delivery.upgrade() else {
                return false;
            };
            delivery
                .push(Outbound::broadcast(
                    &watcher.proof,
                    ServerMessage::DeviceStats { revision, stats },
                ))
                .is_ok()
        });
        Next::Continue
    }

    /// Forgets watchers whose connections closed, and says whether any
    /// remain. With none, the sampler is marked stopped under the same lock a
    /// new watcher takes, so that watcher starts another.
    fn prune(&self) -> bool {
        let mut inner = self.inner();
        inner
            .watchers
            .retain(|_, watcher| watcher.delivery.strong_count() > 0);
        if inner.watchers.is_empty() {
            inner.sampling = false;
            return false;
        }
        true
    }

    fn inner(&self) -> MutexGuard<'_, Inner> {
        self.0.lock().unwrap_or_else(|p| p.into_inner())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn proof() -> Arc<HostSessionProof> {
        let host = service::test_host();
        let state = host.lock().unwrap();
        Arc::new(state.proof().clone())
    }

    #[test]
    fn the_sampler_runs_only_while_watched_and_sends_readings() {
        let watch = Watch::default();
        let delivery = Arc::new(DeliveryQueue::new(1, 16));
        // Subscribed as `watch` does, with the test driving the passes the
        // sampler thread would.
        let answer = {
            let mut inner = watch.inner();
            let answer = Watch::subscribe(&mut inner, 1, Arc::downgrade(&delivery), proof());
            inner.sampling = true;
            answer
        };
        assert_eq!(answer, (0, None));

        let mut sampler = Sampler::new();
        assert_eq!(watch.pass(&mut sampler), Next::Continue);
        assert_eq!(watch.inner().revision, 1);
        assert!(watch.inner().latest.is_some());
        // A new watcher is answered with the latest reading.
        let other = Arc::new(DeliveryQueue::new(2, 16));
        let (revision, latest) =
            Watch::subscribe(&mut watch.inner(), 2, Arc::downgrade(&other), proof());
        assert_eq!(revision, 1);
        assert!(latest.is_some());

        // Once every connection is gone, the next pass ends the sampler, and
        // the next watcher is not answered with its stale reading.
        drop((delivery, other));
        assert_eq!(watch.pass(&mut sampler), Next::Stop);
        let mut inner = watch.inner();
        assert!(!inner.sampling);
        assert!(inner.watchers.is_empty());
        let third = Arc::new(DeliveryQueue::new(3, 16));
        let (_, latest) = Watch::subscribe(&mut inner, 3, Arc::downgrade(&third), proof());
        assert_eq!(latest, None);
    }
}
