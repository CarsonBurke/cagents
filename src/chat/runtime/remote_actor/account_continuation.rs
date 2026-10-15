//! Read-only current-controls RPC. Neither cached final metadata nor scalar
//! activity is an attestation. The later send needs its own atomic admission.
use super::*;

/// Same-machine monotonic absolute expiry; never renew a relative budget after
/// time spent connecting, authenticating, waiting for locks or dispatching RPC.
#[derive(Clone, Copy, Serialize, Deserialize)]
pub(super) struct Deadline(u64);
impl Deadline {
    pub(super) fn capture(deadline: Instant) -> Result<Self, String> {
        // Sampling the shared clock first conservatively shortens the budget.
        let clock = monotonic_nanos()?;
        let left = deadline.checked_duration_since(Instant::now())
            .filter(|left| !left.is_zero()).ok_or("Continuation capture deadline expired")?;
        let left = u64::try_from(left.as_nanos()).map_err(|_| "Continuation budget is too large")?;
        Ok(Self(clock.checked_add(left).ok_or("Continuation deadline overflow")?))
    }
    fn local(self) -> Result<Instant, String> {
        // Sample Instant first so conversion cannot extend the shared expiry.
        let now = Instant::now();
        let left = self.0.checked_sub(monotonic_nanos()?)
            .filter(|left| *left > 0).ok_or("Continuation capture deadline expired")?;
        now.checked_add(Duration::from_nanos(left)).ok_or_else(|| "Continuation deadline overflow".into())
    }
}
fn monotonic_nanos() -> Result<u64, String> {
    let mut clock = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    // SAFETY: clock points to writable initialized storage for this syscall.
    if unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut clock) } != 0 {
        return Err(std::io::Error::last_os_error().to_string());
    }
    u64::try_from(clock.tv_sec).ok().and_then(|seconds| seconds.checked_mul(1_000_000_000))
        .and_then(|value| value.checked_add(u64::try_from(clock.tv_nsec).ok()?))
        .ok_or_else(|| "Monotonic clock is outside supported range".into())
}
#[derive(Serialize, Deserialize)]
pub(super) struct Captured {
    epoch: uuid::Uuid,
    options: Option<super::super::account_continuation::Options>,
}
impl RemoteActor {
    fn continuation_proxy_revision(&self, deadline: Instant) -> Result<u64, String> {
        // Match input admission's lock order; no guard crosses a socket call.
        let pending = self.0.pending.lock().unwrap_or_else(|p| p.into_inner());
        let uncertain = self.0.uncertain_input.lock().unwrap_or_else(|p| p.into_inner());
        let sequence = self.0.input_sequence.lock().unwrap_or_else(|p| p.into_inner());
        if Instant::now() >= deadline {
            return Err("Continuation capture deadline expired while checking input receipts".into());
        }
        if self.0.stop.load(Ordering::Acquire)
            || self.0.recovering_inputs.load(Ordering::Acquire)
            || self.0.preserved.load(Ordering::Acquire)
            || !pending.is_empty() || uncertain.is_some()
        {
            return Err("Continuation controls are unavailable while retained input or attachment authority is unsettled".into());
        }
        Ok(*sequence)
    }
    pub(crate) fn capture_continuation_options(
        &self,
        deadline: Instant,
    ) -> Result<Option<super::super::account_continuation::Options>, String> {
        let expiry = Deadline::capture(deadline)?;
        let revision = self.continuation_proxy_revision(deadline)?;
        let control = registry::connect_until(&self.0.socket, deadline)
            .map_err(|error| error.to_string())?;
        verify_peer(&control, &self.0.identity.record)?;
        deadline_io::send_until(&control, &Invocation {
            proof: self.0.proof.clone(), epoch: Some(self.0.epoch),
            method: "capture_continuation_options".into(), args: json!([expiry]),
        }, deadline)?;
        let result: Result<Captured, String> = deadline_io::receive_until(&control, deadline)?;
        let captured = result?;
        if self.continuation_proxy_revision(deadline)? != revision || captured.epoch != self.0.epoch {
            return Err("Continuation capture was superseded by input or attachment changes".into());
        }
        Ok(captured.options)
    }
}
impl Server {
    fn continuation_capture_ready(&self, deadline: Instant) -> Result<(), String> {
        if self.session_stopping || self.attachment_preserved || self.closed_snapshot.is_some()
            || !self.pending.is_empty()
        {
            return Err("Continuation capture requires a current live attachment with settled inputs".into());
        }
        let journal = self.journal.lock().unwrap_or_else(|p| p.into_inner());
        if !journal.continuation_inputs_settled()? {
            return Err("Continuation capture refuses unknown or unacknowledged retained inputs".into());
        }
        if Instant::now() >= deadline {
            return Err("Continuation capture deadline expired while checking helper receipts".into());
        }
        Ok(())
    }
    pub(super) fn capture_continuation_options(&self, expiry: Deadline) -> Result<Captured, String> {
        let deadline = expiry.local()?;
        self.continuation_capture_ready(deadline)?;
        let options = self.actor.capture_continuation_options(deadline)?;
        self.continuation_capture_ready(deadline)?;
        Ok(Captured { epoch: self.epoch, options })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn continuation_rpc_deadline_roundtrip_never_renews_original_budget() {
        let original = Instant::now() + Duration::from_secs(1);
        let wire = Deadline::capture(original).unwrap();
        assert!(wire.local().unwrap() <= original);
        assert!(Deadline::capture(Instant::now() - Duration::from_secs(1)).is_err());
        assert!(Deadline(0).local().is_err());
    }
}
