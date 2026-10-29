//! Consecutive identity + allowance reads on the actual target connection.
//! Errors return only to the requesting CLI, never as conversation failures.
use super::{Codex, Pending, Step, Value, json};
use crate::account::reset::{parse_codex, transport};

#[derive(Clone)]
pub(super) struct Read {
    request: transport::Read,
    account: Option<Value>,
    pub(super) invalidated: bool,
}
impl Read {
    pub(super) fn stale(&self) -> bool {
        self.invalidated || self.request.check().is_err()
    }
    pub(super) fn retire(&self) {
        let _ = self.request.reply.try_send(Err("The previous allowance read expired or its account changed; inspect the same live actor again".into()));
    }
}

#[cfg(test)]
impl Read {
    pub(super) fn expire_for_test(&mut self) {
        self.request.expires = std::time::Instant::now() - std::time::Duration::from_millis(1);
    }
}

pub(super) fn start(adapter: &mut Codex, request: transport::Read) -> Result<Step, String> {
    request.check()?;
    if !adapter.ready {
        return Err("Codex connection is not ready for allowance inspection".into());
    }
    super::reset_credit::retire_stale(adapter)?;
    if adapter
        .pending
        .values()
        .any(|pending| matches!(pending, Pending::Allowance(_) | Pending::ResetCredit(_)))
    {
        return Err("An allowance request is already in flight on this connection".into());
    }
    let mut step = Step::default();
    adapter.call(
        &mut step,
        "account/read",
        json!({"refreshToken":false}),
        Pending::Allowance(Read {
            request,
            account: None,
            invalidated: false,
        }),
    )?;
    Ok(step)
}

pub(super) fn receive(adapter: &mut Codex, mut read: Read, frame: &Value) -> Result<Step, String> {
    let mut step = Step::default();
    let result = (|| {
        read.request.check()?;
        if read.invalidated {
            return Err(
                "Provider login changed during allowance inspection; inspect the target again"
                    .into(),
            );
        }
        if let Some(error) = frame.get("error") {
            return Err(format!(
                "Codex allowance inspection failed (provider code {}); this installed app-server may not expose the requested account method",
                error
                    .get("code")
                    .and_then(Value::as_i64)
                    .map(|code| code.to_string())
                    .unwrap_or_else(|| "unknown".into())
            ));
        }
        let result = frame
            .get("result")
            .ok_or("Codex allowance reply has no result")?;
        match read.account.take() {
            None => {
                let account = account_identity(result)?;
                read.account = Some(account);
                adapter.call(
                    &mut step,
                    "account/rateLimits/read",
                    json!({}),
                    Pending::Allowance(read.clone()),
                )?;
                Ok(None)
            }
            Some(account) => {
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map_err(|_| "System clock cannot timestamp provider allowance")?
                    .as_millis();
                let now = u64::try_from(now)
                    .map_err(|_| "System clock is outside the allowance timestamp range")?;
                let allowance = parse_codex(&account, result, now)?;
                read.request.check()?;
                Ok(Some(allowance))
            }
        }
    })();
    match result {
        Ok(None) => {}
        result => {
            let _ = read
                .request
                .reply
                .send(result.map(|value| value.expect("completed allowance")));
        }
    }
    Ok(step)
}

pub(super) fn account_identity(result: &Value) -> Result<Value, String> {
    if result.pointer("/account/type").and_then(Value::as_str) != Some("chatgpt") {
        return Err("This Codex connection is not authenticated to a ChatGPT plan account; earned reset inventory is unavailable".into());
    }
    let routing = result.get("workspaceRouting").filter(|v| !v.is_null());
    match routing {
        Some(routing) => {
            let id = crate::account::reset::identifier_str(
                routing
                    .get("chatgptAccountId")
                    .and_then(Value::as_str)
                    .ok_or("Codex account routing identifier is missing")?,
            )?;
            let origin = crate::account::reset::identifier_str(
                routing
                    .get("backendOrigin")
                    .and_then(Value::as_str)
                    .ok_or("Codex account routing origin is missing")?,
            )?;
            Ok(
                json!({"account":{"type":"chatgpt"},"workspaceRouting":{"chatgptAccountId":id,"backendOrigin":origin}}),
            )
        }
        None => Ok(json!({"account":{"type":"chatgpt"}})),
    }
}
