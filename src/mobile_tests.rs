use super::*;

fn hello(api_min: u32, api_max: u32) -> api::Hello {
    api::Hello {
        api_min,
        api_max,
        client: api::ClientInfo {
            name: "test".into(),
            version: "0".into(),
        },
    }
}

#[test]
fn the_newest_common_api_version_is_chosen() {
    assert_eq!(negotiate(&hello(1, 1)), Some(1));
    assert_eq!(negotiate(&hello(1, 99)), Some(api::API_VERSION));
    assert_eq!(negotiate(&hello(api::API_VERSION + 1, 99)), None);
    assert_eq!(negotiate(&hello(0, 0)), None);
}

#[test]
fn lines_end_at_newlines_and_skip_blank_ones() {
    let mut input = io::Cursor::new(b"{\"a\":1}\r\n\n  \n{\"b\":2}\n{\"c\":3}".to_vec());
    let mut lines = Vec::new();
    while let Ok(Some(line)) = read_line(&mut input) {
        lines.push(String::from_utf8(line).unwrap());
    }
    assert_eq!(lines, [r#"{"a":1}"#, r#"{"b":2}"#, r#"{"c":3}"#]);
}

#[test]
fn an_overlong_line_is_refused_before_it_is_buffered_whole() {
    let mut long = vec![b'x'; api::MAX_CLIENT_LINE_BYTES + 2];
    long.push(b'\n');
    let mut input = io::BufReader::with_capacity(4096, io::Cursor::new(long));
    assert!(matches!(read_line(&mut input), Err(TooLong)));
}

#[test]
fn a_request_from_a_newer_app_is_told_apart_from_a_malformed_one() {
    let (id, error) =
        parse_request(br#"{"type":"request","id":7,"op":"terminal.open","params":{}}"#)
            .unwrap_err();
    assert_eq!(id, Some(7));
    assert_eq!(error.code, api::ErrorCode::UnknownOp);

    let (id, error) =
        parse_request(br#"{"type":"request","id":8,"op":"conversation.send","params":{}}"#)
            .unwrap_err();
    assert_eq!(id, Some(8));
    assert_eq!(error.code, api::ErrorCode::BadRequest);

    let (id, error) = parse_request(b"not json").unwrap_err();
    assert_eq!(id, None);
    assert_eq!(error.code, api::ErrorCode::BadRequest);
}

#[test]
fn a_request_parses_with_its_parameters_and_defaults() {
    let request = parse_request(
        br#"{"type":"request","id":3,"op":"conversation.send","params":{"session":"s","text":"hi","future":true}}"#,
    )
    .unwrap();
    assert_eq!(
        request,
        api::Request {
            id: 3,
            call: api::Call::Send {
                session: "s".into(),
                text: "hi".into(),
                mode: api::SendMode::Send,
            },
        }
    );
    let request = parse_request(br#"{"type":"request","id":4,"op":"sessions.watch"}"#).unwrap();
    assert_eq!(request.call, api::Call::WatchSessions {});
}

#[test]
fn an_op_without_parameters_reads_an_empty_or_future_params_object() {
    for params in [
        r#""#,
        r#","params":null"#,
        r#","params":{}"#,
        r#","params":{"include_closed":true}"#,
    ] {
        let line = format!(r#"{{"type":"request","id":5,"op":"ping"{params}}}"#);
        assert_eq!(
            parse_request(line.as_bytes()).unwrap().call,
            api::Call::Ping {},
            "{line}"
        );
    }
}

#[test]
fn a_send_mode_from_a_newer_app_decodes_so_it_can_be_refused_plainly() {
    let request = parse_request(
        br#"{"type":"request","id":6,"op":"conversation.send","params":{"session":"s","text":"hi","mode":"after_review"}}"#,
    )
    .unwrap();
    assert!(matches!(
        request.call,
        api::Call::Send {
            mode: api::SendMode::Unknown,
            ..
        }
    ));
}

#[test]
fn every_op_the_api_names_is_known_to_the_bridge() {
    for op in api::Call::OPS {
        let line = format!(r#"{{"type":"request","id":1,"op":"{op}"}}"#);
        if let Err((_, error)) = parse_request(line.as_bytes()) {
            assert_ne!(error.code, api::ErrorCode::UnknownOp, "{op}");
        }
    }
}

#[test]
fn the_phone_reads_everything_sent_before_the_bridge_exits_and_nothing_after() {
    let (outbox, lines) = Outbox::new();
    outbox.ok(1);
    outbox.close("bye".into());
    outbox.ok(2);
    let mut written = Vec::new();
    write_lines(lines, &mut written);
    assert_eq!(
        String::from_utf8(written).unwrap(),
        concat!(
            r#"{"type":"ok","id":1}"#,
            "\n",
            r#"{"type":"event","event":"closing","data":{"message":"bye"}}"#,
            "\n",
        )
    );
}

#[test]
fn conversations_hold_back_while_the_phone_is_behind() {
    let (outbox, lines) = Outbox::new();
    for id in 0..MAX_BACKLOG as u64 {
        assert!(!outbox.congested());
        outbox.ok(id);
    }
    assert!(outbox.congested());
    let writer = thread::spawn(move || write_lines(lines, io::sink()));
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while outbox.congested() {
        assert!(
            std::time::Instant::now() < deadline,
            "the writer drains the backlog"
        );
        thread::sleep(Duration::from_millis(5));
    }
    drop(outbox);
    writer.join().unwrap();
}

#[test]
fn an_answer_must_choose_an_offered_action_and_known_fields() {
    use conversation::answerable;
    let snapshot = chat::Snapshot {
        requests: vec![chat::Request {
            kind: chat::RequestKind::Question,
            id: "q".into(),
            title: "Which branch?".into(),
            description: String::new(),
            fields: vec![
                chat::Field {
                    id: "branch".into(),
                    label: "Branch".into(),
                    description: String::new(),
                    choices: Vec::new(),
                    multiple: false,
                    secret: false,
                    initial: String::new(),
                },
                chat::Field {
                    id: "note".into(),
                    label: "Note".into(),
                    description: String::new(),
                    choices: Vec::new(),
                    multiple: false,
                    secret: false,
                    initial: String::new(),
                },
            ],
            actions: vec![chat::Choice {
                id: "submit".into(),
                label: "Submit".into(),
                description: String::new(),
            }],
        }],
        ..chat::Snapshot::default()
    };
    let answer = |request: &str, action: &str, field: &str| chat::Command::Answer {
        request_id: request.into(),
        action: action.into(),
        answers: vec![chat::Answer {
            field: field.into(),
            values: vec!["main".into()],
        }],
    };
    assert_eq!(
        answerable(&snapshot, answer("gone", "submit", "branch"))
            .unwrap_err()
            .code,
        api::ErrorCode::NotFound
    );
    assert_eq!(
        answerable(&snapshot, answer("q", "allow", "branch"))
            .unwrap_err()
            .code,
        api::ErrorCode::BadRequest
    );
    assert_eq!(
        answerable(&snapshot, answer("q", "submit", "colour"))
            .unwrap_err()
            .code,
        api::ErrorCode::BadRequest
    );
    assert_eq!(
        answerable(&snapshot, answer("q", "submit", "branch")).unwrap(),
        chat::Command::Answer {
            request_id: "q".into(),
            action: "submit".into(),
            answers: vec![
                chat::Answer {
                    field: "branch".into(),
                    values: vec!["main".into()],
                },
                chat::Answer {
                    field: "note".into(),
                    values: Vec::new(),
                },
            ],
        },
        "a field left out is answered with no values, as the desktop does"
    );
    assert_eq!(
        answerable(&snapshot, chat::Command::Interrupt).unwrap(),
        chat::Command::Interrupt
    );
}
