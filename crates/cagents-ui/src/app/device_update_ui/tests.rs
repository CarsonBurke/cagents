use super::*;

fn app() -> Cagents {
    super::super::tests::context_inspector_test_app(false)
}

fn offer(app: &mut Cagents, device: &str, need: Need) {
    let _ = app.update_device_update(Action::Offer {
        device: device.into(),
        need,
        blocked: None,
        replace: true,
    });
}

/// An update in progress, as `Start` leaves it, without starting the CLI.
fn running(app: &mut Cagents, device: &str, need: Need) {
    app.device_update_ui.run = Some(Run {
        device: device.into(),
        need,
        status: RunStatus::Running(format!("Checking {device}…")),
    });
}

fn success() -> Success {
    Success {
        unchanged: false,
        note: None,
    }
}

#[test]
fn a_device_command_reports_a_mismatch_with_the_devices_protocol() {
    assert_eq!(
        mismatch(
            r#"{"ok":false,"error":{"kind":"device_incompatible","message":"…","details":{"device":"rr","protocol":208,"expected":210}}}"#
        ),
        Some(208)
    );
    // Another failure, or a mismatch without its details, is not offered.
    assert_eq!(
        mismatch(r#"{"ok":false,"error":{"kind":"device","message":"ssh: no route"}}"#),
        None
    );
    assert_eq!(
        mismatch(r#"{"ok":false,"error":{"kind":"device_incompatible","message":"…"}}"#),
        None
    );
    assert_eq!(mismatch("ssh: Could not resolve hostname"), None);
}

#[test]
fn an_update_check_names_what_the_device_lacks() {
    let check = |state: &str, protocol: &str| {
        checked(&format!(
            r#"{{"ok":true,"command":"device.update.check","data":{{"device":"rr","state":"{state}","protocol":{protocol},"expected":210,"note":null}}}}"#
        ))
        .map(|checked| checked.need)
    };
    assert_eq!(check("install", "null"), Some(Some(Need::Install)));
    assert_eq!(check("update", "208"), Some(Some(Need::Update(Some(208)))));
    // A device whose daemon did not answer still needs the update.
    assert_eq!(check("update", "null"), Some(Some(Need::Update(None))));
    assert_eq!(check("newer", "212"), Some(Some(Need::Newer(212))));
    assert_eq!(check("compatible", "null"), Some(None));
    // A check that refused, as a development build or a device not installed
    // with install.sh does, inspected nothing.
    assert!(checked(r#"{"ok":false,"error":{"kind":"device","message":"…"}}"#).is_none());
    let note = checked(
        r#"{"ok":true,"data":{"state":"compatible","protocol":null,"note":"Set it to ~/.local/bin/cagents."}}"#,
    )
    .unwrap()
    .note;
    assert_eq!(note.as_deref(), Some("Set it to ~/.local/bin/cagents."));
}

#[test]
fn a_finished_update_reports_what_changed_and_what_to_do() {
    assert_eq!(
        outcome(
            true,
            r#"{"ok":true,"data":{"state":"updated"}}"#,
            Vec::new()
        ),
        Ok(success())
    );
    assert_eq!(
        outcome(
            true,
            r#"{"ok":true,"data":{"state":"compatible","note":"Set this device's executable to /h/.local/bin/cagents."}}"#,
            Vec::new()
        ),
        Ok(Success {
            unchanged: true,
            note: Some("Set this device's executable to /h/.local/bin/cagents.".into()),
        })
    );
}

#[test]
fn a_failed_update_keeps_the_devices_explanation_without_its_progress() {
    let error = "rr has work that would stop: a native turn in api.";
    // The CLI writes its progress and then the failure's JSON to stderr.
    let stderr = [
        "Checking rr…".to_owned(),
        "Sending cagents 0.2.5 to rr: 40%".into(),
        "Kept: 3 terminals".into(),
        "Disk is 100% full".into(),
        format!(r#"{{"ok":false,"error":{{"kind":"device","message":"{error}"}}}}"#),
    ];
    let mut shown = Vec::new();
    let (report, failed) = read_progress(stderr.into_iter(), |line| shown.push(line));
    assert_eq!(shown.len(), 4, "the JSON is not progress: {shown:?}");
    let failure = outcome(false, &failed.unwrap(), report).unwrap_err();
    assert_eq!(failure.error, error);
    assert_eq!(
        failure.report.as_deref(),
        Some("Kept: 3 terminals\nDisk is 100% full")
    );
    // A CLI that died before printing its result still fails, and says so.
    let failure = outcome(false, "", Vec::new()).unwrap_err();
    assert_eq!(failure.error, "The update stopped without saying why.");
    assert_eq!(failure.report, None);
    // Success needs the CLI's word as well as its exit status.
    assert!(outcome(true, "", Vec::new()).is_err());
}

#[test]
fn a_failed_switch_offers_an_update_until_it_is_dismissed() {
    let mut app = app();
    assert!(app.device_update_banner().is_none());
    offer(&mut app, "rr", Need::Update(Some(208)));
    assert!(app.device_update_banner().is_some());
    assert!(!app.device_update_replaces_health());
    let _ = app.update_device_update(Action::Dismiss);
    assert!(app.device_update_banner().is_none());
    assert_eq!(app.device_update_ui.offer, None);
}

#[test]
fn an_update_this_computer_cannot_send_is_never_started() {
    let mut app = app();
    offer(&mut app, "new", Need::Newer(212));
    let _ = app.update_device_update(Action::Start("new".into()));
    assert!(app.device_update_ui.run.is_none());
    let _ = app.update_device_update(Action::Offer {
        device: "rr".into(),
        need: Need::Update(Some(208)),
        blocked: Some("cagents on rr was not installed with install.sh.".into()),
        replace: true,
    });
    assert!(app.device_update_banner().is_some());
    let _ = app.update_device_update(Action::Start("rr".into()));
    assert!(app.device_update_ui.run.is_none());
}

#[test]
fn a_running_update_cannot_be_dismissed_or_started_twice() {
    let mut app = app();
    offer(&mut app, "fresh", Need::Install);
    running(&mut app, "fresh", Need::Install);
    let _ = app.update_device_update(Action::Dismiss);
    assert!(app.device_update_ui.offer.is_some());
    let _ = app.update_device_update(Action::Start("fresh".into()));
    assert!(matches!(
        &app.device_update_ui.run,
        Some(Run { status: RunStatus::Running(line), .. }) if line == "Checking fresh…"
    ));
    // Being offered the same device again keeps the update it is running.
    offer(&mut app, "fresh", Need::Install);
    assert!(matches!(
        app.device_update_ui.run,
        Some(Run {
            status: RunStatus::Running(_),
            ..
        })
    ));
}

#[test]
fn another_device_waits_for_the_running_update() {
    let mut app = app();
    offer(&mut app, "rr", Need::Update(Some(208)));
    running(&mut app, "rr", Need::Update(Some(208)));
    offer(&mut app, "fresh", Need::Install);
    // The strip keeps following rr's update, and says why fresh is not
    // offered yet.
    assert_eq!(
        app.device_update_ui
            .offer
            .as_ref()
            .map(|offer| offer.device.as_str()),
        Some("rr")
    );
    assert!(app.status.is_some());
    let _ = app.update_device_update(Action::Finished(Ok(success())));
    assert_eq!(app.device_update_ui.offer, None);
    assert!(app.device_update_ui.run.is_none());
}

#[test]
fn a_failed_update_stays_offered_for_another_try() {
    let mut app = app();
    offer(&mut app, "rr", Need::Update(Some(208)));
    running(&mut app, "rr", Need::Update(Some(208)));
    let _ = app.update_device_update(Action::Finished(Err(Failure {
        error: "ssh: connection refused".into(),
        report: None,
    })));
    assert!(matches!(
        &app.device_update_ui.run,
        Some(Run { status: RunStatus::Failed { error, .. }, .. }) if error == "ssh: connection refused"
    ));
    assert!(app.device_update_ui.offer.is_some());
    assert!(app.device_update_banner().is_some());
    // A result with no update running changes nothing.
    let _ = app.update_device_update(Action::Finished(Ok(success())));
    assert!(app.device_update_ui.offer.is_some());
}

#[test]
fn a_finished_update_clears_its_offer_and_says_so() {
    let mut app = app();
    offer(&mut app, "rr", Need::Update(None));
    running(&mut app, "rr", Need::Update(None));
    let _ = app.update_device_update(Action::Progress("Sending cagents 0.2.5 to rr: 10%".into()));
    assert!(matches!(
        &app.device_update_ui.run,
        Some(Run { status: RunStatus::Running(line), .. }) if line.ends_with("10%")
    ));
    let _ = app.update_device_update(Action::Finished(Ok(success())));
    assert_eq!(app.device_update_ui.offer, None);
    assert!(app.device_update_ui.run.is_none());
    assert!(app.status.is_some());
    assert!(app.device_update_banner().is_none());
}

#[test]
fn a_failed_commands_message_is_what_a_failed_open_reports() {
    assert_eq!(
        error_message(
            r#"{"ok":false,"error":{"kind":"launch","message":"could not connect to device rr"}}"#
        )
        .as_deref(),
        Some("could not connect to device rr")
    );
    assert_eq!(
        error_message(r#"{"ok":false,"error":{"kind":"launch","message":""}}"#),
        None
    );
    assert_eq!(error_message("not json"), None);
}
