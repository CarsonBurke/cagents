//! Private actor operations. These are never ordinary prompt/control input.
use super::Allowance;
use std::time::Instant;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Attempt {
    pub(crate) expected: super::ProviderAccount,
    pub(crate) credit_id: String,
    pub(crate) key: String,
}

/// Produced only from the current, locked durable ledger. An inventory
/// observation or a caller's requested outcome cannot construct this proof.
#[derive(Clone)]
pub(crate) struct Verified {
    pub(crate) attempt: Attempt,
    _private: (),
}

impl Verified {
    pub(super) fn recorded(attempt: Attempt) -> Self {
        Self {
            attempt,
            _private: (),
        }
    }
}

#[derive(Clone)]
pub(crate) struct Reservation {
    pub(crate) expires: Instant,
    pub(crate) token: uuid::Uuid,
    pub(crate) attempt: Attempt,
    pub(crate) verified: Vec<Verified>,
    _private: (),
}

impl Reservation {
    pub(super) fn prepared(
        expires: Instant,
        expected: super::ProviderAccount,
        credit_id: String,
        key: String,
        verified: Vec<Verified>,
    ) -> Self {
        Self {
            expires,
            token: uuid::Uuid::new_v4(),
            attempt: Attempt {
                expected,
                credit_id,
                key,
            },
            verified,
            _private: (),
        }
    }

    pub(crate) fn check(&self) -> Result<(), String> {
        if Instant::now() >= self.expires {
            return Err(
                "Reset reservation deadline elapsed; no new consuming request was admitted".into(),
            );
        }
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn fixture(request: &Consume) -> Self {
        Self {
            expires: request.expires,
            token: request.reservation,
            attempt: request.attempt(),
            verified: Vec::new(),
            _private: (),
        }
    }
}

#[derive(Clone)]
pub(crate) struct Read {
    pub expires: Instant,
    pub reply: crossbeam_channel::Sender<Result<Allowance, String>>,
}

impl Read {
    pub(crate) fn check(&self) -> Result<(), String> {
        if Instant::now() >= self.expires {
            return Err("Provider allowance read timed out; the running session was kept".into());
        }
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
    pub(crate) condition: Option<super::request::Threshold>,
    pub(crate) reservation: uuid::Uuid,
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
    pub(super) fn prepared(
        reservation: &Reservation,
        reconcile: bool,
        condition: Option<super::request::Threshold>,
        reply: crossbeam_channel::Sender<Result<Consumed, String>>,
    ) -> Self {
        Self {
            expires: reservation.expires,
            expected: reservation.attempt.expected.clone(),
            credit_id: reservation.attempt.credit_id.clone(),
            key: reservation.attempt.key.clone(),
            reconcile,
            condition,
            reservation: reservation.token,
            reply,
            _private: (),
        }
    }
    pub(crate) fn attempt(&self) -> Attempt {
        Attempt {
            expected: self.expected.clone(),
            credit_id: self.credit_id.clone(),
            key: self.key.clone(),
        }
    }
    pub(crate) fn check(&self) -> Result<(), String> {
        if Instant::now() >= self.expires {
            return Err(
                "Reset deadline elapsed; the durable attempt must be reconciled before any retry"
                    .into(),
            );
        }
        super::identifier_str(&self.expected.account_id)?;
        super::identifier_str(&self.expected.backend_origin)?;
        super::identifier_str(&self.credit_id)?;
        uuid::Uuid::parse_str(&self.key).map_err(|_| "Invalid durable reset attempt key")?;
        Ok(())
    }
    #[cfg(test)]
    pub(crate) fn fixture(
        expires: Instant,
        expected: super::ProviderAccount,
        reply: crossbeam_channel::Sender<Result<Consumed, String>>,
    ) -> Self {
        let reservation = Reservation::prepared(
            expires,
            expected,
            "actual-credit".into(),
            "d4883d22-89ad-8c5f-a577-53ada6ed3afb".into(),
            Vec::new(),
        );
        Self::prepared(&reservation, false, None, reply)
    }
}
