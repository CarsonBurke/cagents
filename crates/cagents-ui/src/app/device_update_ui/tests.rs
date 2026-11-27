use super::*;

fn app() -> Cagents {
    super::super::tests::context_inspector_test_app(false)
}

fn offer(app: &mut Cagents, device: &str, actual: Option<u16>) {
    let _ = app.update_device_update(Action::Offer {
        device: device.into(),
        actual,
        replace: true,
    });
}

/// An update in progress, as `Start` leaves it, without starting the CLI.
fn running(app: &mut Cagents, device: &str) -> u64 {
    let state = &mut app.device_update_ui;
    state.generation += 1;
    state.run = Some(Run {
        device: device.into(),
        status: RunStatus::Running(format!("Checking {device}…")),
    });
    state.generation
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
            r#"{{"ok":true,"command":"device.update.check","data":{{"device":"rr","state":"{state}","protocol":{protocol},"expected":210}}}}"#
        ))
    };
    assert!(matches!(
        check("install", "null"),
        Some(device_ui::OpenFailure::Missing)
    ));
    assert!(matches!(
        check("update", "208"),
        Some(device_ui::OpenFailure::Incompatible(208))
    ));
    assert!(matches!(
        check("newer", "212"),
        Some(device_ui::OpenFailure::Incompatible(212))
    ));
    // Without the device's protocol the daemon status is asked instead, and a
    // compatible device failed for some other reason.
    assert!(check("update", "null").is_none());
    assert!(check("compatible", "null").is_none());
    // A development build refuses to inspect at all.
    assert!(checked(r#"{"ok":false,"error":{"kind":"device","message":"…"}}"#).is_none());
}

#[test]
fn a_failed_update_keeps_the_devices_explanation_without_its_progress() {
    assert_eq!(
        outcome(true, r#"{"ok":true,"data":{}}"#, Vec::new()),
        Ok(())
    );
    let error = "rr has work that would stop: a native turn in api.";
    let failure = outcome(
        false,
        &format!(r#"{{"ok":false,"error":{{"kind":"device","message":"{error}"}}}}"#),
        vec![
            "Checking rr…".into(),
            "Sending cagents 0.2.5 to rr: 40%".into(),
            "Kept: 3 terminals".into(),
            format!("error: {error}"),
        ],
    )
    .unwrap_err();
    assert_eq!(failure.error, error);
    assert_eq!(failure.report.as_deref(), Some("Kept: 3 terminals"));
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
    offer(&mut app, "rr", Some(208));
    assert!(app.device_update_banner().is_some());
    let _ = app.update_device_update(Action::Dismiss);
    assert!(app.device_update_banner().is_none());
    assert_eq!(app.device_update_ui.offer, None);
}

#[test]
fn a_running_update_cannot_be_dismissed_or_started_twice() {
    let mut app = app();
    offer(&mut app, "rr", None);
    let generation = running(&mut app, "rr");
    let _ = app.update_device_update(Action::Dismiss);
    assert!(app.device_update_ui.offer.is_some());
    let _ = app.update_device_update(Action::Start("rr".into()));
    assert_eq!(app.device_update_ui.generation, generation);
    // Being offered the same device again keeps the update it is running.
    offer(&mut app, "rr", None);
    assert!(matches!(
        app.device_update_ui.run,
        Some(Run {
            status: RunStatus::Running(_),
            ..
        })
    ));
}

#[test]
fn an_abandoned_updates_progress_and_result_are_ignored() {
    let mut app = app();
    offer(&mut app, "rr", Some(208));
    let stale = running(&mut app, "rr");
    let current = running(&mut app, "rr");
    let _ = app.update_device_update(Action::Progress {
        generation: stale,
        line: "Sending cagents 0.2.5 to rr: 90%".into(),
    });
    let _ = app.update_device_update(Action::Finished {
        generation: stale,
        result: Ok(()),
    });
    assert!(app.device_update_ui.offer.is_some());
    assert!(matches!(
        &app.device_update_ui.run,
        Some(Run { status: RunStatus::Running(line), .. }) if line == "Checking rr…"
    ));
    let _ = app.update_device_update(Action::Progress {
        generation: current,
        line: "Sending cagents 0.2.5 to rr: 10%".into(),
    });
    assert!(matches!(
        &app.device_update_ui.run,
        Some(Run { status: RunStatus::Running(line), .. }) if line.ends_with("10%")
    ));
}

#[test]
fn a_failed_update_stays_offered_for_another_try() {
    let mut app = app();
    offer(&mut app, "rr", Some(208));
    let generation = running(&mut app, "rr");
    let _ = app.update_device_update(Action::Finished {
        generation,
        result: Err(Failure {
            error: "ssh: connection refused".into(),
            report: None,
        }),
    });
    assert!(matches!(
        &app.device_update_ui.run,
        Some(Run { status: RunStatus::Failed { error, .. }, .. }) if error == "ssh: connection refused"
    ));
    assert!(app.device_update_ui.offer.is_some());
    assert!(app.device_update_banner().is_some());
}

#[test]
fn a_finished_update_clears_its_offer_and_says_so() {
    let mut app = app();
    offer(&mut app, "rr", Some(208));
    let generation = running(&mut app, "rr");
    let _ = app.update_device_update(Action::Finished {
        generation,
        result: Ok(()),
    });
    assert_eq!(app.device_update_ui.offer, None);
    assert!(app.device_update_ui.run.is_none());
    assert!(app.status.is_some());
    assert!(app.device_update_banner().is_none());
}
