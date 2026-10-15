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

#[derive(Clone)]
pub(crate) struct Consume {
    pub(crate) expires: Instant,
    pub(crate) expected: super::ProviderAccount,
    pub(crate) credit_id: String,
    pub(crate) key: String,
    pub(crate) reconcile: bool,
    pub(crate) reply: crossbeam_channel::Sender<Result<Consumed, String>>,
    _private: (),
}

pub(crate) struct Consumed {
    pub outcome: super::Outcome,
    pub allowance: Allowance,
}

impl Consume {
    /// Called only inside the reset controller after durable Ledger::begin
    /// and explicit authenticated user intent. No deserializable effect token.
    pub(super) fn prepared(expires: Instant, expected: super::ProviderAccount, credit_id: String,
        key: String, reconcile: bool, reply: crossbeam_channel::Sender<Result<Consumed, String>>) -> Self {
        Self {expires, expected, credit_id, key, reconcile, reply, _private:()}
    }
    pub(crate) fn check(&self) -> Result<(), String> {
        if Instant::now() >= self.expires { return Err("Reset deadline elapsed; the durable attempt must be reconciled before any retry".into()); }
        super::identifier_str(&self.expected.account_id)?;
        super::identifier_str(&self.expected.backend_origin)?;
        super::identifier_str(&self.credit_id)?;
        uuid::Uuid::parse_str(&self.key).map_err(|_| "Invalid durable reset attempt key")?;
        Ok(())
    }
    #[cfg(test)]
    pub(crate) fn fixture(expires: Instant, expected: super::ProviderAccount, reply: crossbeam_channel::Sender<Result<Consumed, String>>) -> Self {
        Self::prepared(expires, expected, "actual-credit".into(), "d4883d22-89ad-8c5f-a577-53ada6ed3afb".into(), false, reply)
    }
}
