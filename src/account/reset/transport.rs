//! Private actor operations. These are never ordinary prompt/control input.
use super::Allowance;
use std::time::Instant;

#[derive(Clone)]
pub(crate) struct Read {
    pub expires: Instant,
    pub reply: crossbeam_channel::Sender<Result<Allowance, String>>,
}

impl Read {
    pub(crate) fn check(&self) -> Result<(), String> {
        if Instant::now() >= self.expires { return Err("Provider allowance read timed out; the running session was kept".into()); }
        Ok(())
    }
}
