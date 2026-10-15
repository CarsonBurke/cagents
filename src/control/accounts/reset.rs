use super::{AccountCommand, super::*};
use crate::account::reset::{self, Decision, Outcome, ProviderAccount, request::{Action, Report, Request, Threshold}};

pub(super) fn execute(command: AccountCommand) -> Result<CommandOutput, AppError> {
    let socket = terminal::ensure_daemon_ready().map_err(|e| AppError::new("daemon_start", e))?;
    let mut client = ControlClient::connect(&socket).map_err(|e| AppError::new("daemon_connection", e))?;
    let mut invoke = |request: Request| {
        request.validate().map_err(|e| AppError::new("account_reset_request", e))?;
        let session = request.session;
        let reply = client.device_operation(crate::device_service::Operation::AccountAllowance {request}).map_err(protocol_error)?;
        let crate::device_service::Reply::AccountAllowance(report) = reply else {
            return Err(AppError::new("account_allowance", "Unexpected allowance response; no alternative target or local fallback was selected"));
        };
        report.validate().map_err(|e| AppError::new("account_allowance", e))?;
        if report.session != session { return Err(AppError::new("account_allowance", "Allowance reply names a different session")); }
        Ok(report)
    };
    match command {
        AccountCommand::Allowance {session} => {
            let report = invoke(Request {session, action:Action::Inspect})?;
            output("account.allowance", report)
        }
        AccountCommand::Redeem {session, provider_account, backend_origin, credit, authorize} => {
            let report = invoke(Request {session, action:Action::Redeem {
                expected:ProviderAccount {account_id:provider_account, backend_origin}, credit_id:credit,
                authorized:authorize, condition:None}})?;
            output("account.redeem", report)
        }
        AccountCommand::Monitor {session, threshold, interval_seconds, checks, max_redemptions, authorize} => {
            monitor(session, threshold, interval_seconds, checks, max_redemptions, authorize, &mut invoke, thread::sleep)
        }
        AccountCommand::Info {..} => Err(AppError::new("account_command", "Account info uses its existing read route")),
    }
}

fn output(command: &'static str, report: Report) -> Result<CommandOutput, AppError> {
    Ok(CommandOutput {command, human:human(&report), data:serde_json::to_value(report).map_err(|e| AppError::new("account_allowance",e))?})
}

fn human(report: &Report) -> String {
    let account = report.allowance.account.as_ref().map(|a| a.account_id.as_str()).unwrap_or("unknown; redemption unavailable");
    let mut lines = vec![format!("Session {}  provider account {account}", report.session),
        format!("Source {:?}; fetched at {} ms", report.source, report.allowance.observed_at_ms)];
    for window in &report.allowance.windows {
        lines.push(format!("{} {:?}: {}% used; scheduled reset {}", window.bucket, window.period, window.used_percent,
            window.resets_at_ms.map(|at| at.to_string()).unwrap_or_else(|| "unknown".into())));
    }
    lines.push(match &report.allowance.reset_credits {
        Some(credits) => format!("Available earned reset credits: {}; {} detail rows", credits.available_count, credits.details.as_ref().map(|v|v.len().to_string()).unwrap_or_else(|| "unknown".into())),
        None => "Earned reset-credit inventory: unavailable from this provider reply".into(),
    });
    if let Some(outcome) = report.outcome { lines.push(format!("Reset result: {outcome:?}{}", if report.reconciled {" (original attempt reconciled; no new key)"} else {""})); }
    if let Some(detail) = &report.allowance.account_binding_detail {lines.push(detail.clone());}
    lines.join("\n")
}

fn monitor(session: TerminalId, threshold: u8, interval: u64, checks: u32, maximum: u16, authorized: bool,
    mut invoke: impl FnMut(Request)->Result<Report,AppError>, mut sleep: impl FnMut(Duration)) -> Result<CommandOutput,AppError> {
    if checks == 0 || checks > 1_000_000 || maximum == 0 || maximum > 100 || interval == 0 || interval > 3600 || threshold == 0 || threshold > 100 {return Err(AppError::new("account_monitor", "Use positive bounded checks, interval and redemption count"));}
    let mut pinned = None;
    let mut completed = 0;
    let mut redemptions = 0;
    let mut last = None;
    let mut stopped = "check_limit";
    for index in 0..checks {
        let mut report = invoke(Request {session, action:Action::Inspect})?;
        if authorized {
            let actual = report.allowance.account.clone().ok_or_else(|| AppError::new("account_monitor_identity", "Actual provider account identity is unknown; no reset was authorized for a guessed account"))?;
            match &pinned {
                None => pinned = Some(actual),
                Some(expected) if expected != &actual => return Err(AppError::new("account_monitor_identity", "The target provider account changed; restart monitoring with fresh explicit authorization")),
                _ => {}
            }
        }
        let now = crate::chat::now_ms().ok_or_else(|| AppError::new("account_monitor_clock", "Cannot timestamp allowance freshness"))?;
        let decision = reset::decision(&report.allowance, now, 30_000, threshold, authorized).map_err(|e|AppError::new("account_monitor",e))?;
        eprintln!("Allowance check {}: {decision:?}", index+1);
        match decision {
            Decision::Eligible {credit_id} => {
                let expected = pinned.clone().ok_or_else(||AppError::new("account_monitor_identity","Monitor has no authorized actual provider account"))?;
                report = invoke(Request {session, action:Action::Redeem {expected,credit_id,authorized,
                    condition:Some(Threshold {used_percent:threshold,max_age_ms:30_000})}})?;
                if report.allowance.account.as_ref() != pinned.as_ref() {return Err(AppError::new("account_monitor_identity","Provider account changed during redemption verification; reconcile the original attempt"));}
                match report.outcome {
                    Some(Outcome::Reset | Outcome::AlreadyRedeemed) => redemptions += 1,
                    Some(Outcome::NoCredit) => stopped = "provider_no_credit",
                    _ => {}
                }
            }
            Decision::NoCredits => stopped = "inventory_exhausted",
            Decision::Stale | Decision::UnknownUsage | Decision::UnknownAccount | Decision::InventoryUnknown | Decision::CreditDetailsUnavailable => {
                return Err(AppError::new("account_monitor_unavailable", format!("Allowance/reset state is {decision:?}; monitor stopped without guessing or resuming work")));
            }
            Decision::BelowThreshold | Decision::AuthorizationRequired => {}
        }
        completed += 1;
        last = Some(report);
        if redemptions >= maximum {stopped="authorization_limit"; break;}
        if stopped != "check_limit" {break;}
        if index+1 < checks {sleep(Duration::from_secs(interval));}
    }
    let last = last.ok_or_else(||AppError::new("account_monitor", "Monitor obtained no reading"))?;
    Ok(CommandOutput {command:"account.monitor", human:format!("{}\nMonitor stopped: {stopped}; {completed} checks, {redemptions} successful reset results",human(&last)),
        data:serde_json::json!({"last":last,"checks_completed":completed,"successful_reset_results":redemptions,"authorized":authorized,"stopped_reason":stopped})})
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::account::reset::{Allowance, Period, ResetCredit, ResetCredits, Window, request::Source};
    fn reading(session: TerminalId) -> Report {
        let now = crate::chat::now_ms().unwrap();
        Report {session, provider:crate::agent::Provider::Codex, source:Source::CodexAppServer,
            allowance:Allowance {observed_at_ms:now, account:Some(ProviderAccount {account_id:"actual-account".into(),backend_origin:"https://chatgpt.com".into()}), account_binding_detail:None,
                windows:vec![Window {bucket:"codex".into(), period:Period::Primary, used_percent:96,resets_at_ms:Some(now+60_000)}],
                reset_credits:Some(ResetCredits {available_count:1,details:Some(vec![ResetCredit {id:"actual-credit".into(),supported_type:true,available:true,expires_at_ms:None}])})},
            outcome:None,reconciled:false}
    }
    #[test]
    fn observe_only_monitor_never_constructs_a_redeem_request() {
        let session = TerminalId::new();
        let mut requests = Vec::new();
        let result = monitor(session,90,1,2,1,false,|request|{
            assert_eq!(request.session,session);
            requests.push(request.action);
            Ok(reading(session))
        },|_|{}).unwrap();
        assert!(requests.iter().all(|action|matches!(action,Action::Inspect)));
        assert_eq!(requests.len(),2);
        assert_eq!(result.data["successful_reset_results"],0);
    }
    #[test]
    fn authorized_monitor_pins_actual_account_credit_condition_and_finite_authorization_budget() {
        let session=TerminalId::new();
        let mut requests=Vec::new();
        let result=monitor(session,90,1,10,1,true,|request|{
            requests.push(request.clone());
            let mut report=reading(session);
            if let Action::Redeem {expected,credit_id,authorized,condition}=request.action {
                assert_eq!(expected,report.allowance.account.clone().unwrap());
                assert_eq!(credit_id,"actual-credit"); assert!(authorized);
                assert_eq!(condition,Some(Threshold {used_percent:90,max_age_ms:30_000}));
                report.outcome=Some(Outcome::Reset);
                report.allowance.windows[0].used_percent=0;
                report.allowance.reset_credits.as_mut().unwrap().available_count=0;
            }
            Ok(report)
        },|_|panic!("authorization limit must stop without another poll")).unwrap();
        assert_eq!(requests.len(),2);
        assert!(requests.iter().all(|r|r.session==session));
        assert_eq!(result.data["stopped_reason"],"authorization_limit");
        assert_eq!(result.data["successful_reset_results"],1);
    }
    #[test]
    fn unknown_stale_or_changed_actual_account_never_becomes_an_authorized_guess() {
        for unknown in [true,false] {
            let session=TerminalId::new(); let mut requests=Vec::new();
            let result=monitor(session,90,1,2,1,true,|request|{
                requests.push(request);
                let mut report=reading(session);
                if unknown {report.allowance.account=None;} else {report.allowance.observed_at_ms=0;}
                Ok(report)
            },|_|{});
            assert!(result.is_err());
            assert!(requests.iter().all(|r|matches!(r.action,Action::Inspect)));
        }
        let session=TerminalId::new(); let mut count=0;
        let error=monitor(session,90,1,2,1,true,|request|{
            assert!(matches!(request.action,Action::Inspect)); count+=1;
            let mut report=reading(session); report.allowance.windows[0].used_percent=10;
            if count==2 {report.allowance.account.as_mut().unwrap().account_id="different-account".into();}
            Ok(report)
        },|_|{}).err().unwrap();
        assert!(error.message.contains("account changed"));
        assert_eq!(count,2);
    }
    #[test]
    fn uncertain_redeem_response_stops_without_retry_or_selecting_another_credit() {
        let session=TerminalId::new(); let mut count=0;
        let error=monitor(session,90,1,10,2,true,|request|{
            count+=1;
            match request.action {Action::Inspect=>Ok(reading(session)),Action::Redeem {..}=>Err(AppError::new("reset_uncertain","Original reset outcome is uncertain; reconcile before retrying"))}
        },|_|panic!("uncertain effect cannot spin")).err().unwrap();
        assert_eq!(count,2);assert!(error.message.contains("uncertain"));
    }
}
