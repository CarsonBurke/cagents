use super::*;
use crate::agent::Provider;

fn owned_listener_test_launch(home: &std::path::Path) -> Launch {
    Launch {
        provider: crate::agent::Provider::Codex,
        session_id: None,
        recipe: crate::model::LaunchRecipe {
            unset_environment: Default::default(),
            program: home
                .join("never-executed-codex")
                .to_string_lossy()
                .into_owned(),
            arguments: vec!["--remote".into(), "unix://".into()],
            working_directory: home.into(),
            environment: BTreeMap::from([(
                "CODEX_HOME".into(),
                home.to_string_lossy().into_owned(),
            )]),
        },
    }
}

#[cfg(target_os = "linux")]
#[test]
fn owned_listener_rejects_unusable_unix_addresses_before_preparation_or_spawn() {
    use std::os::unix::ffi::OsStringExt;

    let home = tempfile::tempdir().unwrap();
    let launch = owned_listener_test_launch(home.path());
    let before = launch.recipe.clone();
    for socket in [
        PathBuf::new(),
        PathBuf::from("/nul\0socket"),
        PathBuf::from(std::ffi::OsString::from_vec(vec![b'/', 0xff])),
        PathBuf::from(format!("/{}", "x".repeat(107))),
        PathBuf::from(format!("/{}", "x".repeat(108))),
        PathBuf::from(format!("/{}x", "λ".repeat(53))),
    ] {
        let failure = create_with_listener(&launch, socket)
            .err()
            .expect("invalid address must fail before returning a spawn specification");
        assert!(failure.contains("Owned Codex listener"), "{failure}");
        assert!(failure.contains("shorter than 108 bytes"), "{failure}");
        assert_eq!(launch.recipe, before);
        assert_eq!(
            std::fs::read_dir(home.path()).unwrap().count(),
            0,
            "invalid owned addresses must not create endpoint directories or execute the provider"
        );
    }
}

#[cfg(target_os = "linux")]
#[test]
fn owned_listener_accepts_107_actual_bytes_without_changing_launch_identity() {
    let home = tempfile::tempdir().unwrap();
    let launch = owned_listener_test_launch(home.path());
    let before = launch.recipe.clone();
    for socket in [
        PathBuf::from(format!("/{}", "x".repeat(106))),
        PathBuf::from(format!("/{}", "λ".repeat(53))),
    ] {
        assert_eq!(socket.as_os_str().as_encoded_bytes().len(), 107);
        let (spec, _adapter) = create_with_listener(&launch, socket.clone()).unwrap();
        assert!(
            matches!(&spec.transport, TransportSpec::OwnedUnixWebSocket { socket: actual } if actual == &socket)
        );
        assert_eq!(
            spec.arguments,
            [
                "app-server".to_owned(),
                "--listen".to_owned(),
                format!("unix://{}", socket.display())
            ]
        );
        assert_eq!(spec.program, launch.recipe.program);
        assert_eq!(spec.working_directory, launch.recipe.working_directory);
        assert_eq!(spec.environment, launch.recipe.environment);
        assert_eq!(launch.recipe, before);
        assert_eq!(std::fs::read_dir(home.path()).unwrap().count(), 0);
    }
}

#[cfg(target_os = "linux")]
#[test]
fn shared_long_listener_keeps_its_existing_verified_alias_and_never_launches() {
    use std::os::unix::fs::{MetadataExt, symlink};
    let home = tempfile::tempdir().unwrap();
    let socket = home.path().join("s");
    let _listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
    let original = std::fs::metadata(&socket).unwrap();
    let long = home
        .path()
        .join("long-exact-socket-".to_owned() + &"x".repeat(108));
    symlink(&socket, &long).unwrap();
    assert!(long.as_os_str().as_encoded_bytes().len() >= 108);
    let mut launch = owned_listener_test_launch(home.path());
    launch.recipe.arguments = vec!["--remote".into(), format!("unix://{}", long.display())];
    let before = launch.recipe.clone();
    let (spec, adapter) = create(&launch).unwrap();
    let TransportSpec::UnixWebSocket {
        socket: alias,
        device,
        inode,
    } = &spec.transport
    else {
        panic!("shared listener must remain a verified existing connection, never an owned launch");
    };
    assert!(alias.as_os_str().as_encoded_bytes().len() < 108);
    assert_ne!(alias, &long);
    assert_eq!((*device, *inode), (original.dev(), original.ino()));
    let aliased = std::fs::metadata(alias).unwrap();
    assert_eq!(
        (aliased.dev(), aliased.ino()),
        (original.dev(), original.ino())
    );
    assert!(spec.arguments.is_empty());
    assert_eq!(launch.recipe, before);
    let alias = alias.clone();
    drop(adapter);
    assert!(std::fs::symlink_metadata(&alias).is_err());
    assert!(std::fs::metadata(&long).is_ok());
    let unchanged = std::fs::metadata(&socket).unwrap();
    assert_eq!(
        (unchanged.dev(), unchanged.ino()),
        (original.dev(), original.ino())
    );
}

#[test]
fn unverified_owned_image_history_never_reads_or_replays_a_coincident_device_path() {
    crate::chat::attachment_assets::with_test_image(|image| {
        let reference = image.reference.clone().unwrap();
        let bytes = std::fs::read(&image.path).unwrap();
        for failure in [0, 1, 2] {
            let mut candidate = reference.clone();
            if failure == 0 {
                candidate.device = uuid::Uuid::new_v4();
            } else if failure == 1 {
                candidate.id = uuid::Uuid::new_v4();
            }
            let mut marker = super::super::image_references::legacy::marker(&candidate).unwrap();
            if failure == 2 {
                marker = marker.replace("{\"id\"", "{\"future\":true,\"id\"");
            }
            let inputs = json!([
                {"type":"text","text":marker},
                {"type":"localImage","path":image.path.to_str().unwrap()}
            ]);
            let items = parse_item(
                &json!({"type":"userMessage","id":"foreign","content":inputs}),
                true,
                "turn",
            )
            .unwrap();
            assert_eq!(items.len(), 2, "unverified marker remains literal");
            assert!(matches!(&items[0].content, ItemContent::Text { text, .. } if text == &marker));
            assert!(
                matches!(&items[1].content, ItemContent::Image { path, reference: None, description } if path.is_empty() && description.contains("originating device")),
                "even a real coincident local file is not preview authority"
            );
            let mut step = Step::default();
            adapter()
                .queue_page(
                    &json!({"data":[{"id":"foreign-image-only","input":inputs}],"nextCursor":null}),
                    &mut step,
                )
                .unwrap();
            let queued = step
                .events
                .iter()
                .find_map(|event| match event {
                    Event::QueueChanged(messages) => messages.first(),
                    _ => None,
                })
                .unwrap();
            assert!(
                !queued.editable,
                "image-only canonical marker cannot become legacy path replay"
            );
            assert!(queued.attachments[0].path.as_os_str().is_empty());
            assert!(queued.attachments[0].reference.is_none());
            assert_eq!(std::fs::read(&image.path).unwrap(), bytes);
            let inline = format!(
                "data:image/png;base64,{}",
                base64::Engine::encode(&base64::engine::general_purpose::STANDARD, &bytes)
            );
            let inline_items = parse_item(
                &json!({"type":"userMessage","id":"inline","content":[
                    {"type":"text","text":marker}, {"type":"image","url":inline}
                ]}),
                true,
                "turn",
            )
            .unwrap();
            assert!(
                matches!(&inline_items[1].content, ItemContent::Image { path, reference: None, .. } if path == &inline),
                "self-contained real image bytes remain viewable without borrowing image authority"
            );
        }
    });
}

#[test]
fn referenced_image_is_real_provider_input_and_survives_history_and_queue() {
    crate::chat::attachment_assets::with_test_image(|attachment| {
        let display = tempfile::tempdir().unwrap();
        let other_pixels = crate::chat::attachment_assets::test_image("display-side pixels.png");
        let mut hinted = attachment.clone();
        hinted.path = display.path().join(attachment.path.file_name().unwrap());
        std::fs::copy(&other_pixels.path, &hinted.path).unwrap();
        assert_ne!(
            std::fs::read(&hinted.path).unwrap(),
            std::fs::read(&attachment.path).unwrap()
        );
        let reference = attachment.reference.clone().unwrap();
        let mut adapter = adapter();
        adapter
            .modalities
            .insert("model".into(), vec!["image".into()]);
        let text = format!(
            "Earlier [Image 77777] was dark; compare new {} after",
            reference.token()
        );
        let input = adapter.input(text.clone(), vec![hinted.clone()]).unwrap();
        assert_eq!(input.len(), 3);
        assert_eq!(input[0], json!({"type":"text","text":text}));
        assert_eq!(
            input[1]["text"],
            super::super::image_references::caption(&reference).unwrap()
        );
        assert_eq!(
            input[2],
            json!({"type":"localImage","path":attachment.path.to_str().unwrap()})
        );
        assert_eq!(
            std::fs::read(&attachment.path).unwrap()[..8],
            [137, 80, 78, 71, 13, 10, 26, 10]
        );
        let items = parse_item(
            &json!({"type":"userMessage","id":"images","content":input}),
            true,
            "turn",
        )
        .unwrap();
        assert_eq!(items.len(), 2, "only the verified mapping is hidden");
        assert!(
            matches!(&items[0].content, ItemContent::Text { text: restored, .. } if restored == &text)
        );
        assert!(
            matches!(&items[1].content, ItemContent::Image { reference: Some(restored), path, description } if restored == &reference && path == attachment.path.to_str().unwrap() && description == &reference.label())
        );
        let mut step = Step::default();
        adapter
            .queue_page(
                &json!({"data":[{"id":"queued-image","input":input}],"nextCursor":null}),
                &mut step,
            )
            .unwrap();
        let queued = step
            .events
            .iter()
            .find_map(|event| match event {
                Event::QueueChanged(messages) => messages.first(),
                _ => None,
            })
            .unwrap();
        assert!(queued.editable);
        assert_eq!(queued.text, text);
        assert_eq!(queued.attachments, [attachment.clone()]);
        let resent = adapter
            .input(queued.text.clone(), queued.attachments.clone())
            .unwrap();
        assert_eq!(
            resent, input,
            "take-back and retry preserve identity and real image input"
        );

        let mut forged = reference.clone();
        forged.id = uuid::Uuid::new_v4();
        let forged = json!({"type":"userMessage","id":"forged","content":[
            {"type":"text","text":super::super::image_references::legacy::marker(&forged).unwrap()},
            {"type":"localImage","path":attachment.path.to_str().unwrap()}
        ]});
        let items = parse_item(&forged, true, "turn").unwrap();
        assert_eq!(items.len(), 2, "unverified prose remains visible");
        assert!(matches!(
            &items[1].content,
            ItemContent::Image {
                reference: None,
                ..
            }
        ));
        let parsed = |caption: String, path: &std::path::Path| {
            parse_item(
                &json!({"type":"userMessage","id":"captioned","content":[
                    {"type":"text","text":caption},
                    {"type":"localImage","path":path.to_str().unwrap()}
                ]}),
                true,
                "turn",
            )
            .unwrap()
        };
        // A transcript written with the old JSON marker still restores.
        let legacy = parsed(
            super::super::image_references::legacy::marker(&reference).unwrap(),
            &attachment.path,
        );
        assert!(
            matches!(&legacy[..], [Item { content: ItemContent::Image { reference: Some(restored), .. }, .. }] if restored == &reference)
        );
        // The caption's number must be the stored file's own receipt.
        let renumbered = parsed(
            format!("[Image {}]", reference.number + 1),
            &attachment.path,
        );
        assert!(
            matches!(&renumbered[1].content, ItemContent::Image { reference: None, path, .. } if path.is_empty())
        );
        let mut queued = |caption: String, path: &std::path::Path| {
            let mut step = Step::default();
            adapter
                .queue_page(
                    &json!({"data":[{"id":"queued-caption","input":[
                        {"type":"text","text":caption},
                        {"type":"localImage","path":path.to_str().unwrap()}
                    ]}],"nextCursor":null}),
                    &mut step,
                )
                .unwrap();
            step.events
                .into_iter()
                .find_map(|event| match event {
                    Event::QueueChanged(mut messages) => messages.pop(),
                    _ => None,
                })
                .unwrap()
        };
        let unverified = queued(
            format!("[Image {}]", reference.number + 1),
            &attachment.path,
        );
        assert!(
            !unverified.editable,
            "an unverified caption is never replayed by path"
        );
        assert!(unverified.attachments[0].path.as_os_str().is_empty());
        // Text that only says [Image N] beside an ordinary file is the
        // user's own words, and the file stays theirs to edit and resend.
        let ordinary = display.path().join("screenshot.png");
        std::fs::copy(&attachment.path, &ordinary).unwrap();
        let typed = queued(reference.token(), &ordinary);
        assert!(typed.editable);
        assert_eq!(typed.text, reference.token());
        assert_eq!(typed.attachments, [Attachment::file(&ordinary)]);
        let typed = parsed(reference.token(), &ordinary);
        assert!(
            matches!(&typed[..], [Item { content: ItemContent::Text { .. }, .. }, Item { content: ItemContent::Image { reference: None, path, .. }, .. }] if path == ordinary.to_str().unwrap())
        );
        let submitted = adapter
            .command(Command::Prompt {
                text,
                attachments: vec![hinted],
                mode: PromptMode::Send,
            })
            .unwrap();
        assert_eq!(
            submitted.writes[0]["params"]["input"][2]["path"],
            attachment.path.to_str().unwrap()
        );
        assert!(
            crate::chat::attachment_assets::list()
                .unwrap()
                .images
                .iter()
                .find(|image| image.id == reference.id)
                .unwrap()
                .retained
        );
    });
}

#[test]
fn multiple_image_inputs_follow_caret_order_without_renumbering() {
    crate::chat::attachment_assets::with_test_image(|first| {
        let second = crate::chat::attachment_assets::test_image("second.png");
        let first_ref = first.reference.as_ref().unwrap();
        let second_ref = second.reference.as_ref().unwrap();
        let text = format!(
            "Compare {} before {} and again {}",
            second_ref.token(),
            first_ref.token(),
            second_ref.token()
        );
        let mut adapter = adapter();
        adapter
            .modalities
            .insert("model".into(), vec!["image".into()]);
        let input = adapter
            .input(text.clone(), vec![first.clone(), second.clone()])
            .unwrap();
        assert_eq!(input[0]["text"], text);
        assert_eq!(
            input[1]["text"],
            super::super::image_references::caption(second_ref).unwrap()
        );
        assert_eq!(input[2]["path"], second.path.to_str().unwrap());
        assert_eq!(
            input[3]["text"],
            super::super::image_references::caption(first_ref).unwrap()
        );
        assert_eq!(input[4]["path"], first.path.to_str().unwrap());
        assert!(adapter.input(text, vec![first.clone(), first]).is_err());
    });
}

#[test]
fn image_only_legacy_and_foreign_references_keep_their_distinct_contracts() {
    crate::chat::attachment_assets::with_test_image(|attachment| {
        let mut adapter = adapter();
        adapter
            .modalities
            .insert("model".into(), vec!["image".into()]);
        let image_only = adapter
            .input(String::new(), vec![attachment.clone()])
            .unwrap();
        assert_eq!(image_only.len(), 2);
        assert_eq!(image_only[1]["type"], "localImage");
        let legacy = adapter
            .input(
                "[Image 1] literal".into(),
                vec![Attachment::file(&attachment.path)],
            )
            .unwrap();
        assert_eq!(
            legacy.len(),
            2,
            "legacy files do not acquire a UUID from text"
        );
        let mut foreign = attachment.clone();
        foreign.reference.as_mut().unwrap().device = uuid::Uuid::new_v4();
        foreign.path = "/missing/foreign-image.png".into();
        let error = adapter.input(String::new(), vec![foreign]).unwrap_err();
        assert!(
            !error.contains("Attachment is not a file"),
            "foreign origin is refused before path access: {error}"
        );
        adapter.modalities.clear();
        assert!(
            adapter
                .input(String::new(), vec![attachment])
                .unwrap_err()
                .contains("does not advertise image")
        );
    });
}

fn output_window_command(output: &str) -> Value {
    json!({"id":"window-cmd","type":"commandExecution","command":"cat data","cwd":"/tmp","status":"inProgress","aggregatedOutput":output})
}
fn output_window_started(output: &str) -> Value {
    json!({"method":"item/started","params":{"threadId":"thread-exact","turnId":"window-turn","item":output_window_command(output)}})
}
fn output_window_delta(delta: &str) -> Value {
    json!({"method":"item/commandExecution/outputDelta","params":{"threadId":"thread-exact","turnId":"window-turn","itemId":"window-cmd","delta":delta}})
}
fn large_window_output() -> String {
    format!(
        "first🦀\n{}last🦀\n",
        "middle🦀\n".repeat(MAX_ITEM_BYTES / 8)
    )
}
fn assert_window_accounting(adapter: &Codex, source: &str) {
    let key = item_key("window-turn", "window-cmd");
    let item = adapter.active_items.get(&key).unwrap();
    let ItemContent::Command { output, .. } = &item.content else {
        panic!("missing command");
    };
    let window = adapter.output_windows.get(&key).unwrap();
    assert!(window.omitted > 0);
    let marker = format!("\n[... {} bytes omitted ...]\n", window.omitted);
    assert!(output.contains(&marker));
    assert!(output.starts_with("first🦀\n"));
    assert!(source.ends_with(output.rsplit_once(&marker).unwrap().1));
    assert_eq!(source.len(), output.len() - marker.len() + window.omitted);
    assert!(item.content.byte_len() <= MAX_ITEM_BYTES);
}
#[test]
fn snapshot_output_preserves_its_partial_terminal_state_for_the_next_delta() {
    for snapshot in ["item", "turn", "history"] {
        for (raw, suffix, expected) in [
            ("before🦀\u{1b}[", "31m-tail", "before🦀-tail"),
            (
                "before🦀\u{1b}]8;;https://example.test/",
                "\u{7}tail",
                "before🦀tail",
            ),
            ("before🦀\u{1b}Ppayload\u{1b}", "\\tail", "before🦀tail"),
            ("before🦀\r", "after", "before🦀\nafter"),
        ] {
            let mut adapter = adapter();
            adapter.busy = true;
            adapter.receive(output_window_started("")).unwrap();
            adapter.receive(output_window_delta("old\u{1b}[")).unwrap();
            match snapshot {
                "item" => {
                    adapter.receive(output_window_started(raw)).unwrap();
                }
                "turn" => {
                    adapter.receive(json!({"method":"turn/started","params":{"threadId":"thread-exact","turn":{"id":"window-turn","status":"inProgress","items":[output_window_command(raw)]}}})).unwrap();
                }
                "history" => {
                    let key = item_key("window-turn", "window-cmd");
                    adapter.active_items.remove(&key);
                    adapter.output_streams.remove(&key);
                    adapter.output_windows.remove(&key);
                    adapter.page(&json!({"data":[{"id":"window-turn","status":"inProgress","items":[output_window_command(raw)]}],"nextCursor":null}), &mut Step::default(), false).unwrap();
                }
                _ => unreachable!(),
            }
            adapter.receive(output_window_delta(suffix)).unwrap();
            let key = item_key("window-turn", "window-cmd");
            let ItemContent::Command { output, .. } = &adapter.active_items[&key].content else {
                panic!("command output");
            };
            assert_eq!(
                output, expected,
                "{snapshot} snapshot must seed its own terminal continuation, not the old stream or default grammar"
            );
        }
    }
}

#[test]
fn small_marker_budgets_still_retain_the_latest_complete_utf8_tail() {
    let source = format!("first🦀\n{}LAST🦀\n", "numbered🦀\n".repeat(100));
    for limit in 64..=96 {
        let mut output = String::new();
        let mut window = OutputWindow::default();
        append_display_output(&mut output, &source, limit, &mut window);
        assert!(output.len() <= limit);
        assert!(
            output.ends_with("LAST🦀\n"),
            "a {limit}-byte window has room for its marker and newest line"
        );
        let marker = format!("\n[... {} bytes omitted ...]\n", window.omitted);
        assert_eq!(source.len(), output.len() - marker.len() + window.omitted);
    }
}

#[test]
fn repeated_native_started_replaces_clipped_window_and_partial_decoder_together() {
    for whole_turn in [false, true] {
        let mut adapter = adapter();
        adapter.busy = true;
        adapter.receive(output_window_started("")).unwrap();
        let source = large_window_output();
        adapter.receive(output_window_delta(&source)).unwrap();
        assert_window_accounting(&adapter, &source);
        adapter.receive(output_window_delta("\u{1b}[")).unwrap();
        let key = item_key("window-turn", "window-cmd");
        assert!(adapter.output_streams.contains_key(&key));
        let replacement = "short🦀";
        let frame = if whole_turn {
            json!({"method":"turn/started","params":{"threadId":"thread-exact","turn":{"id":"window-turn","status":"inProgress","items":[output_window_command(replacement)]}}})
        } else {
            output_window_started(replacement)
        };
        adapter.receive(frame).unwrap();
        let window = adapter.output_windows.get(&key).unwrap();
        assert_eq!((window.omitted, window.head), (0, 0));
        let delta = "31m-tail🦀";
        let step = adapter.receive(output_window_delta(delta)).unwrap();
        assert!(step.events.iter().any(|event|matches!(event,Event::OutputDelta {offset,delta:seen,..} if *offset==replacement.len() && seen==delta)));
        assert!(
            matches!(&adapter.active_items[&key].content,ItemContent::Command {output,..} if output==&format!("{replacement}{delta}"))
        );
    }
}
#[test]
fn native_started_snapshot_retains_original_clipping_accounting_for_next_delta() {
    let mut adapter = adapter();
    adapter.busy = true;
    let mut source = large_window_output();
    adapter.receive(output_window_started(&source)).unwrap();
    assert_window_accounting(&adapter, &source);
    let delta = "new-tail🦀\n";
    source.push_str(delta);
    adapter.receive(output_window_delta(delta)).unwrap();
    assert_window_accounting(&adapter, &source);
}
#[test]
fn active_history_hydration_retains_clipping_window_without_replacing_live_owner() {
    for existing in [false, true] {
        let mut adapter = adapter();
        adapter.busy = true;
        let mut source = large_window_output();
        if existing {
            adapter.receive(output_window_started("")).unwrap();
            adapter.receive(output_window_delta(&source)).unwrap();
        }
        let mut step = Step::default();
        let page_output = if existing {
            "older page must not replace the current stream"
        } else {
            &source
        };
        adapter.page(&json!({"data":[{"id":"window-turn","status":"inProgress","items":[output_window_command(page_output)]}],"nextCursor":null}),&mut step,false).unwrap();
        assert_window_accounting(&adapter, &source);
        let delta = "history-followed-by-tail🦀\n";
        source.push_str(delta);
        adapter.receive(output_window_delta(delta)).unwrap();
        assert_window_accounting(&adapter, &source);
    }
}

#[test]
fn rolling_output_preserves_first_last_lines_and_exact_omission_bytes() {
    let source = (1..=200000)
        .map(|line| format!("{line}:🦀\n"))
        .collect::<String>();
    let mut output = String::new();
    let mut window = OutputWindow::default();
    let mut start = 0;
    while start < source.len() {
        let end = source.floor_char_boundary((start + 1024).min(source.len()));
        append_display_output(
            &mut output,
            &source[start..end],
            MAX_ITEM_BYTES,
            &mut window,
        );
        assert!(output.len() <= MAX_ITEM_BYTES);
        start = end;
    }
    assert!(output.starts_with("1:🦀\n"));
    assert!(output.ends_with("200000:🦀\n"));
    let marker = format!("\n[... {} bytes omitted ...]\n", window.omitted);
    assert!(output.contains(&marker));
    assert_eq!(source.len(), output.len() - marker.len() + window.omitted);
}

#[test]
fn absent_or_failed_predictions_and_observed_child_turns_never_produce_prompts() {
    for result in [
        json!({"type":"failed"}),
        json!({"type":"completed","text":null}),
        json!({"type":"completed","text":""}),
        json!({"type":"completed","text":"x".repeat(chat::PromptSuggestion::MAX_BYTES + 1)}),
    ] {
        let mut current = adapter();
        current.receive(json!({"method":"turn/started","params":{"threadId":"thread-exact","turn":{"id":"turn","status":"inProgress","items":[]}}})).unwrap();
        current.receive(json!({"method":"turn/completed","params":{"threadId":"thread-exact","turn":{"id":"turn","status":"completed","items":[]}}})).unwrap();
        let absent = current.receive(json!({"method":"thread/prediction/updated","params":{"threadId":"thread-exact","sourceTurnId":"turn","result":result}})).unwrap();
        assert!(absent.events.is_empty());
        assert!(absent.writes.is_empty());
        assert!(current.ready);
    }
    let mut child = adapter();
    child.is_child = true;
    child.receive(json!({"method":"turn/started","params":{"threadId":"thread-exact","turn":{"id":"turn","status":"inProgress","items":[]}}})).unwrap();
    let finished = child.receive(json!({"method":"turn/completed","params":{"threadId":"thread-exact","turn":{"id":"turn","status":"completed","items":[]}}})).unwrap();
    assert!(
        !finished
            .writes
            .iter()
            .any(|frame| frame["method"] == "thread/prediction/request")
    );
}

#[test]
fn next_prompt_prediction_is_correlated_and_legacy_errors_are_quiet() {
    let mut current = adapter();
    let completion = |id: &str| json!({"method":"turn/completed","params":{"threadId":"thread-exact","turn":{"id":id,"status":"completed","items":[]}}});
    let start = |id: &str| json!({"method":"turn/started","params":{"threadId":"thread-exact","turn":{"id":id,"status":"inProgress","items":[]}}});
    current.receive(start("turn-one")).unwrap();
    let finished = current.receive(completion("turn-one")).unwrap();
    let request = finished
        .writes
        .iter()
        .find(|frame| frame["method"] == "thread/prediction/request")
        .unwrap();
    assert_eq!(request["params"]["sourceTurnId"], "turn-one");
    let prediction = |thread: &str, turn: &str| json!({"method":"thread/prediction/updated","params":{"threadId":thread,"sourceTurnId":turn,"result":{"type":"completed","text":"Check the resulting diff"}}});
    assert!(
        current
            .receive(prediction("another-thread", "turn-one"))
            .unwrap()
            .events
            .is_empty()
    );
    assert!(
        current
            .receive(prediction("thread-exact", "old-turn"))
            .unwrap()
            .events
            .is_empty()
    );
    assert!(
        matches!(current.receive(prediction("thread-exact", "turn-one")).unwrap().events.as_slice(), [Event::PromptSuggestion { text, .. }] if text == "Check the resulting diff")
    );
    current
        .receive(json!({"id":request["id"],"result":{}}))
        .unwrap();
    assert!(
        !current
            .receive(completion("old-turn"))
            .unwrap()
            .writes
            .iter()
            .any(|frame| frame["method"] == "thread/prediction/request")
    );
    current.receive(start("turn-two")).unwrap();
    current.receive(completion("turn-two")).unwrap();
    current
        .command(Command::Prompt {
            text: "My next message".into(),
            attachments: vec![],
            mode: PromptMode::Send,
        })
        .unwrap();
    assert!(
        current
            .receive(prediction("thread-exact", "turn-two"))
            .unwrap()
            .events
            .is_empty()
    );

    let mut legacy = adapter();
    legacy.receive(start("turn-one")).unwrap();
    let finished = legacy.receive(completion("turn-one")).unwrap();
    let request = finished
        .writes
        .iter()
        .find(|frame| frame["method"] == "thread/prediction/request")
        .unwrap();
    let error = legacy
        .receive(json!({"id":request["id"],"error":{"code":-32601,"message":"Method not found"}}))
        .unwrap();
    assert!(error.events.is_empty());
    assert!(error.writes.is_empty());
    assert!(
        !legacy
            .receive(completion("turn-two"))
            .unwrap()
            .writes
            .iter()
            .any(|frame| frame["method"] == "thread/prediction/request")
    );
}

#[test]
fn tool_rows_fixture_settles_parent_and_child_without_leaving_update_work() {
    // Use the actual offline peer's frames and RPC responses, including
    // child hydration. No provider executable, account or socket is used.
    let export = std::process::Command::new("/usr/bin/python3")
        .args([
            "-c",
            r#"
import json, runpy, sys
peer = runpy.run_path(sys.argv[1])
scenario = peer['Scenario']()
scenario.thread, scenario.child = 'thread-exact', 'child-exact'
frames = scenario.request({'id': 1, 'method': 'turn/start', 'params': {
    'threadId': scenario.thread}}, '/private/codex')[1:]
resume = scenario.request({'id': 2, 'method': 'thread/resume', 'params': {
    'threadId': scenario.child}}, '/private/codex')
turn = frames[0]['params']['turn']['id']
prediction = scenario.request({'id': 3, 'method': 'thread/prediction/request', 'params': {
    'threadId': scenario.thread, 'sourceTurnId': turn}}, '/private/codex')
print(json.dumps({'frames': frames, 'replies': {
    'thread/resume': resume, 'thread/prediction/request': prediction}}))
"#,
        ])
        .arg(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("tests/fixtures/codex-tool-rows.py"),
        )
        .output()
        .unwrap();
    assert!(
        export.status.success(),
        "fixture export failed: {}",
        String::from_utf8_lossy(&export.stderr)
    );
    let fixture: Value = serde_json::from_slice(&export.stdout).unwrap();
    let frames = fixture["frames"].as_array().unwrap();
    let pause = frames
        .iter()
        .position(|frame| frame.get("_sleep").is_some())
        .unwrap();

    struct Fixture {
        parser: Codex,
        replies: Value,
    }
    impl Adapter for Fixture {
        fn initialize(&mut self) -> Result<Step, String> {
            Ok(Step::default())
        }
        fn command(&mut self, _: Command) -> Result<Step, String> {
            Err("fixture accepts provider frames only".into())
        }
        fn receive(&mut self, frame: Value) -> Result<Step, String> {
            let mut pending = std::collections::VecDeque::from([frame]);
            let mut result = Step::default();
            let mut consumed = 0;
            while let Some(frame) = pending.pop_front() {
                consumed += 1;
                assert!(consumed < 32, "fixture RPC responses did not settle");
                let step = self.parser.receive(frame)?;
                result.events.extend(step.events);
                for request in step.writes {
                    let method = request["method"].as_str().unwrap();
                    let replies = self.replies[method]
                        .as_array()
                        .unwrap_or_else(|| panic!("unexpected fixture RPC: {request}"));
                    for mut reply in replies.iter().cloned() {
                        if reply.get("id").is_some() {
                            reply["id"] = request["id"].clone();
                        }
                        pending.push_back(reply);
                    }
                }
            }
            Ok(result)
        }
    }
    let snapshot = |frames: Vec<Value>, replies: Value| {
        crate::chat::runtime::notification_frames_with_snapshot_for_test(
            Box::new(Fixture {
                parser: adapter(),
                replies,
            }),
            frames,
            false,
        )
        .1
    };
    // There is deliberately no outcome before the pause. Inspect the
    // parser directly rather than asking a notification-outcome harness
    // to synthesize a completion for still-running work.
    let mut active = Fixture {
        parser: adapter(),
        replies: fixture["replies"].clone(),
    };
    for frame in &frames[..pause] {
        active.receive(frame.clone()).unwrap();
    }
    assert!(active.parser.busy);
    assert!(
        active.parser.children["child-exact"].busy,
        "the pause must represent actual delegated work"
    );
    assert_eq!(
        active.parser.children["child-exact"].subagent_status,
        Some(ToolStatus::Running)
    );
    let completed: Vec<_> = frames
        .iter()
        .filter(|frame| frame.get("_sleep").is_none())
        .cloned()
        .collect();
    let settled = snapshot(completed.clone(), fixture["replies"].clone());
    assert!(!settled.busy && !settled.is_working());
    assert!(settled.requests.is_empty());
    assert_eq!(settled.subagents.len(), 1);
    assert_eq!(settled.subagents[0].status, ToolStatus::Succeeded);

    // Recreate the old fixture: hydration says idle, completed collab
    // items assert Running again, and no child lifecycle ever follows.
    // A parent's success alone cannot prove its background work ended.
    let broken: Vec<_> = completed
        .into_iter()
        .filter(|frame| frame["params"]["threadId"] != "child-exact")
        .map(|mut frame| {
            if frame["method"] == "item/completed"
                && frame["params"]["item"]["type"] == "collabAgentToolCall"
            {
                frame["params"]["item"]["agentsStates"]["child-exact"]["status"] = json!("running");
            }
            frame
        })
        .collect();
    let mut replies = fixture["replies"].clone();
    replies["thread/resume"][0]["result"]["thread"]["status"]["type"] = json!("idle");
    replies["thread/resume"][0]["result"]["initialTurnsPage"]["data"] = json!([]);
    let stranded = snapshot(broken, replies);
    assert!(!stranded.busy, "the parent turn really completed");
    assert!(
        stranded.is_working(),
        "the old update check still sees the orphaned Running child"
    );
    assert_eq!(stranded.subagents[0].status, ToolStatus::Running);
}

#[test]
fn rejected_turn_start_releases_optimistic_activity_for_sign_in_recovery() {
    let mut adapter = adapter();
    let submitted = adapter
        .command(Command::Prompt {
            text: "first attempt".into(),
            attachments: vec![],
            mode: PromptMode::Send,
        })
        .unwrap();
    let request = submitted
        .writes
        .iter()
        .find(|frame| frame["method"] == "turn/start")
        .unwrap();
    let rejected = adapter.receive(json!({"id":request["id"],"error":{"code":-32000,"message":"Your access token could not be refreshed. Please sign in again."}})).unwrap();
    assert!(
        rejected
            .events
            .iter()
            .any(|event| matches!(event, Event::Waiting(false)))
    );
    assert!(rejected.events.iter().any(
        |event| matches!(event, Event::Status { busy: false, detail } if detail == "Turn failed")
    ));
    assert!(rejected.events.iter().any(|event| matches!(event, Event::Error { message, fatal: false } if chat::requires_sign_in(crate::agent::Provider::Codex, message))));
    assert!(!adapter.busy);
    assert!(adapter.turn.is_none());
    assert!(
        adapter
            .command(Command::Prompt {
                text: "explicit new attempt".into(),
                attachments: vec![],
                mode: PromptMode::Send,
            })
            .is_ok()
    );
}

#[test]
fn rejected_turn_start_does_not_settle_a_newer_provider_turn() {
    for completed in [false, true] {
        let mut adapter = adapter();
        let submitted = adapter
            .command(Command::Prompt {
                text: "old attempt".into(),
                attachments: vec![],
                mode: PromptMode::Send,
            })
            .unwrap();
        let request = submitted
            .writes
            .iter()
            .find(|frame| frame["method"] == "turn/start")
            .unwrap();
        adapter.receive(json!({"method":"turn/started","params":{"threadId":"thread-exact","turn":{"id":"newer-turn","status":"inProgress","items":[]}}})).unwrap();
        if completed {
            adapter.receive(json!({"method":"turn/completed","params":{"threadId":"thread-exact","turn":{"id":"newer-turn","status":"completed","items":[]}}})).unwrap();
        }
        let revision = adapter.lifecycle_revision;
        let rejected = adapter
            .receive(
                json!({"id":request["id"],"error":{"code":-32000,"message":"Old request failed"}}),
            )
            .unwrap();
        assert!(!rejected.events.iter().any(|event| matches!(
            event,
            Event::Status { .. }
                | Event::Waiting(_)
                | Event::TurnSettled
                | Event::TurnFinished { .. }
        )));
        assert_eq!(adapter.lifecycle_revision, revision);
        assert_eq!(adapter.busy, !completed);
        assert_eq!(
            adapter.turn.as_deref(),
            (!completed).then_some("newer-turn")
        );
    }
}

/// Official app-server Thread.name and thread/name/updated threadName are
/// metadata, not evidence that every thread receives a generated title.
#[test]
fn supported_thread_names_follow_only_the_exact_codex_conversation() {
    let mut adapter = adapter();
    let mut restored = Step::default();
    adapter
        .thread_state(
            &json!({"id":"thread-exact","name":"Encrypted bundle review","status":{"type":"idle"}}),
            &mut restored,
        )
        .unwrap();
    assert!(restored.events.iter().any(|event| matches!(event,
        Event::Session {id, title: Some(title)} if id == "thread-exact" && title == "Encrypted bundle review")));
    assert!(
        restored.writes.is_empty(),
        "reusing a provider name requires no inference request"
    );
    let foreign = adapter.receive(json!({"method":"thread/name/updated","params":{"threadId":"other","threadName":"Foreign name"}})).unwrap();
    assert!(
        !foreign
            .events
            .iter()
            .any(|event| matches!(event, Event::Session { .. }))
    );
    for name in [
        Some("Bundle throughput audit"),
        Some("My custom bundle review"),
        Some("1341"),
        Some("🔐"),
        None,
    ] {
        let mut params = json!({"threadId":"thread-exact"});
        if let Some(name) = name {
            params["threadName"] = json!(name);
        }
        let update = adapter
            .receive(json!({"method":"thread/name/updated","params":params}))
            .unwrap();
        let names = update
            .events
            .iter()
            .filter_map(|event| match event {
                Event::Session { id, title } => Some((id.as_str(), title.as_deref())),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(names, vec![("thread-exact", name)]);
        assert!(update.writes.is_empty());
    }
    // Identity-only None preservation is covered at the actual Runtime
    // boundary, which owns the current displayed name and Reset behavior.
}

#[test]
fn notification_questions_use_real_codex_prompts_and_same_id_revisions() {
    let question = |id: u64, text: &str| {
        json!({
            "id":id, "method":"item/tool/requestUserInput",
            "params":{"threadId":"thread-exact", "turnId":"notification-turn",
                "questions":[{"id":"migration", "header":"Migration", "question":text,
                    "options":[{"label":"Keep","description":"Keep existing"},
                        {"label":"Replace","description":"Use replacement"}]}]}
        })
    };
    let frames = vec![
        question(71, "Which config_file.rs strategy should A use?"),
        question(72, "Which migration should B use?"),
        json!({"method":"serverRequest/resolved",
            "params":{"threadId":"thread-exact", "requestId":71}}),
        question(72, "Updated B: preserve config_file.rs?"),
        question(72, "Updated B: preserve config_file.rs?"),
    ];
    let receipts =
        crate::chat::runtime::notification_frames_for_test(Box::new(adapter()), frames, false);
    assert_eq!(
        receipts.len(),
        3,
        "identical repeated metadata must not notify again"
    );
    for (index, expected) in [
        "Which config_file.rs strategy should A use?",
        "Which migration should B use?",
        "Updated B: preserve config_file.rs?",
    ]
    .into_iter()
    .enumerate()
    {
        let (evidence, content) = &receipts[index];
        assert_eq!(evidence.kind, crate::notifications::Kind::Waiting);
        assert_eq!(
            content.request,
            Some(crate::notifications::RequestType::Question)
        );
        assert!(content.preview.contains(expected), "{}", content.preview);
        assert!(!content.preview.contains("waiting for your answer"));
    }
    assert_ne!(receipts[0].0.key, receipts[1].0.key);
    assert_ne!(receipts[1].0.key, receipts[2].0.key);
}

#[test]
fn notification_secret_question_revisions_notify_without_exposing_prompts() {
    let question = |wording: &str| {
        json!({
            "id":73, "method":"item/tool/requestUserInput",
            "params":{"threadId":"thread-exact", "turnId":"notification-turn",
                "questions":[{"id":"secret", "header":"PRIVATE_HEADER", "question":wording,
                    "isSecret":true, "options":[]}]}
        })
    };
    let receipts = crate::chat::runtime::notification_frames_for_test(
        Box::new(adapter()),
        vec![
            question("PRIVATE_FIRST_QUESTION"),
            question("PRIVATE_CHANGED_QUESTION"),
            question("PRIVATE_CHANGED_QUESTION"),
        ],
        false,
    );
    assert_eq!(
        receipts.len(),
        2,
        "changed secret prompts still need fresh attention"
    );
    assert_ne!(receipts[0].0.key, receipts[1].0.key);
    for (evidence, content) in receipts {
        assert_eq!(evidence.kind, crate::notifications::Kind::Waiting);
        assert_eq!(
            content.request,
            Some(crate::notifications::RequestType::Question)
        );
        assert_eq!(content.preview, "Codex is waiting for your answer.");
        assert!(!content.preview.contains("PRIVATE"));
    }
}

#[test]
fn notification_old_completion_preserves_current_question_topic_and_error_state() {
    let start = json!({"method":"turn/started","params":{"threadId":"thread-exact",
        "turn":{"id":"new-b","status":"inProgress","items":[]}}});
    let user = json!({"method":"item/completed","params":{"threadId":"thread-exact","turnId":"new-b",
        "item":{"id":"user-b","type":"userMessage","content":[{"type":"text","text":"Review the new B migration"}]}}});
    let question = |text| {
        json!({"id":71,"method":"item/tool/requestUserInput",
        "params":{"threadId":"thread-exact","turnId":"new-b",
            "questions":[{"id":"migration","header":"Migration","question":text,"options":[]}]}})
    };
    let old = json!({"method":"turn/completed","params":{"threadId":"thread-exact",
        "turn":{"id":"old-a","status":"failed","error":{"message":"Old A failure must not become B's error"},
            "items":[{"id":"old-user","type":"userMessage","content":[{"type":"text","text":"Old A private topic"}]},
                {"id":"old-answer","type":"agentMessage","text":"Old A final response"}]}}});
    let old_user = json!({"method":"item/completed","params":{"threadId":"thread-exact","turnId":"old-a",
        "item":{"id":"late-old-user","type":"userMessage","content":[{"type":"text","text":"Late old A topic"}]}}});
    let old_delta = json!({"method":"item/agentMessage/delta","params":{"threadId":"thread-exact","turnId":"old-a","itemId":"late-old-answer","delta":"Old A delta"}});
    let mut adapter = adapter();
    let submitted = adapter
        .command(Command::Prompt {
            text: "Old A".into(),
            attachments: vec![],
            mode: PromptMode::Send,
        })
        .unwrap();
    let old_request = submitted
        .writes
        .iter()
        .find(|frame| frame["method"] == "turn/start")
        .unwrap()["id"]
        .clone();
    let old_rejection = json!({"id":old_request,"error":{"code":-32000,"message":"Old A rejected after B started"}});
    let (callbacks, state) = crate::chat::runtime::notification_frames_with_snapshot_for_test(
        Box::new(adapter),
        vec![
            start,
            user,
            question("Choose B migration"),
            old_user,
            old_delta,
            old,
            old_rejection,
            question("Updated B migration question"),
        ],
        false,
    );
    assert_eq!(
        callbacks.len(),
        2,
        "old A must not publish current completion or another waiting revision"
    );
    for (evidence, content) in &callbacks {
        assert_eq!(evidence.kind, crate::notifications::Kind::Waiting);
        assert_eq!(content.conversation, "Review the new B migration");
        assert!(!content.preview.contains("Old A"));
    }
    assert!(
        callbacks[1]
            .1
            .preview
            .contains("Updated B migration question")
    );
    assert_eq!(state.requests.len(), 1);
    assert_eq!(state.requests[0].id, "71");
    assert!(state.error.is_none());
    assert_eq!(state.activity(), crate::agent::ActivityState::Waiting);
}

#[test]
fn notification_exact_early_ack_restores_streamed_context_without_reappending_deltas() {
    let mut adapter = adapter();
    let submitted = adapter
        .command(Command::Prompt {
            text: "B".into(),
            attachments: vec![],
            mode: PromptMode::Send,
        })
        .unwrap();
    let request = submitted
        .writes
        .iter()
        .find(|frame| frame["method"] == "turn/start")
        .unwrap()["id"]
        .clone();
    let (callbacks, state) = crate::chat::runtime::notification_frames_with_snapshot_for_test(
        Box::new(adapter),
        vec![
            json!({"method":"item/completed","params":{"threadId":"thread-exact","turnId":"early-b",
            "item":{"id":"b-user","type":"userMessage","content":[{"type":"text","text":"Review B migration"}]}}}),
            json!({"method":"item/agentMessage/delta","params":{"threadId":"thread-exact","turnId":"early-b","itemId":"b-answer","delta":"Exact streamed B response."}}),
            json!({"method":"turn/completed","params":{"threadId":"thread-exact","turn":{"id":"early-b","status":"completed","items":[]}}}),
            json!({"method":"thread/status/changed","params":{"threadId":"thread-exact","status":{"type":"idle"}}}),
            json!({"id":request,"result":{"turn":{"id":"early-b"}}}),
        ],
        false,
    );
    assert_eq!(callbacks.len(), 1);
    assert_eq!(callbacks[0].0.kind, crate::notifications::Kind::Succeeded);
    assert_eq!(callbacks[0].1.conversation, "Review B migration");
    assert_eq!(callbacks[0].1.preview, "Exact streamed B response.");
    assert_eq!(
        state
            .notification_content(crate::notifications::Kind::Succeeded)
            .preview,
        "Exact streamed B response."
    );
}

#[test]
fn notification_observed_start_transfers_early_context_before_live_deltas_and_ack() {
    let mut adapter = adapter();
    let submitted = adapter
        .command(Command::Prompt {
            text: "B".into(),
            attachments: vec![],
            mode: PromptMode::Send,
        })
        .unwrap();
    let request = submitted
        .writes
        .iter()
        .find(|frame| frame["method"] == "turn/start")
        .unwrap()["id"]
        .clone();
    let delta = |text| json!({"method":"item/agentMessage/delta","params":{"threadId":"thread-exact","turnId":"early-b","itemId":"b-answer","delta":text}});
    let (callbacks, state) = crate::chat::runtime::notification_frames_with_snapshot_for_test(
        Box::new(adapter),
        vec![
            json!({"method":"item/completed","params":{"threadId":"thread-exact","turnId":"early-b",
            "item":{"id":"b-user","type":"userMessage","content":[{"type":"text","text":"Review B migration"}]}}}),
            delta("First part. "),
            json!({"method":"turn/started","params":{"threadId":"thread-exact","turn":{"id":"early-b","status":"inProgress","items":[]}}}),
            delta("Second part."),
            json!({"id":request,"result":{"turn":{"id":"early-b"}}}),
            json!({"method":"turn/completed","params":{"threadId":"thread-exact","turn":{"id":"early-b","status":"completed","items":[]}}}),
        ],
        false,
    );
    assert_eq!(callbacks.len(), 1);
    assert_eq!(callbacks[0].0.kind, crate::notifications::Kind::Succeeded);
    assert_eq!(callbacks[0].1.conversation, "Review B migration");
    assert_eq!(callbacks[0].1.preview, "First part. Second part.");
    assert_eq!(
        state
            .notification_content(crate::notifications::Kind::Succeeded)
            .preview,
        "First part. Second part."
    );
}

#[test]
fn notification_observed_start_keeps_already_decoded_completion_for_exact_ack() {
    let mut adapter = adapter();
    let submitted = adapter
        .command(Command::Prompt {
            text: "B".into(),
            attachments: vec![],
            mode: PromptMode::Send,
        })
        .unwrap();
    let request = submitted
        .writes
        .iter()
        .find(|frame| frame["method"] == "turn/start")
        .unwrap()["id"]
        .clone();
    // A conservative decoder-order control, not a claim that a supported
    // provider emits start after completion on one ordered connection.
    let (callbacks, state) = crate::chat::runtime::notification_frames_with_snapshot_for_test(
        Box::new(adapter),
        vec![
            json!({"method":"turn/completed","params":{"threadId":"thread-exact","turn":{"id":"early-b","status":"completed","items":[
            {"id":"b-user","type":"userMessage","content":[{"type":"text","text":"Review B migration"}]},
            {"id":"b-answer","type":"agentMessage","text":"Already completed B response."}]}}}),
            json!({"method":"turn/started","params":{"threadId":"thread-exact","turn":{"id":"early-b","status":"inProgress","items":[]}}}),
            json!({"id":request,"result":{"turn":{"id":"early-b"}}}),
        ],
        false,
    );
    assert_eq!(callbacks.len(), 1);
    assert_eq!(callbacks[0].0.kind, crate::notifications::Kind::Succeeded);
    assert_eq!(callbacks[0].1.conversation, "Review B migration");
    assert_eq!(callbacks[0].1.preview, "Already completed B response.");
    assert!(!state.is_working());
}

#[test]
fn notification_history_and_scalar_idle_cannot_replace_exact_live_turn_proof() {
    let start = |id| {
        json!({"method":"turn/started","params":{"threadId":"thread-exact",
        "turn":{"id":id,"status":"inProgress","items":[]}}})
    };
    let mut adapter = adapter();
    adapter.receive(start("old-a")).unwrap();
    let mut request = Step::default();
    adapter.history(&mut request, None).unwrap();
    let id = request.writes[0]["id"].clone();
    adapter.receive(start("new-b")).unwrap();
    adapter.receive(json!({"id":id,"result":{"data":[{"id":"old-a","status":"inProgress","items":[]}],"nextCursor":null}})).unwrap();
    assert_eq!(adapter.turn.as_deref(), Some("new-b"));
    assert_eq!(adapter.notification_turn.as_deref(), Some("new-b"));
    let old = adapter.receive(json!({"method":"turn/completed","params":{"threadId":"thread-exact","turn":{"id":"old-a","status":"completed","items":[]}}})).unwrap();
    assert!(old.suppress_current_notification);
    adapter.receive(json!({"method":"thread/status/changed","params":{"threadId":"thread-exact","status":{"type":"idle"}}})).unwrap();
    assert!(adapter.turn.is_none());
    let current = adapter.receive(json!({"method":"turn/completed","params":{"threadId":"thread-exact","turn":{"id":"new-b","status":"completed","items":[]}}})).unwrap();
    assert!(!current.suppress_current_notification);
    assert!(current.events.iter().any(|event| matches!(event, Event::TurnFinished { key, .. } if key.as_deref() == Some("codex:thread-exact:new-b"))));
}

#[test]
fn notification_pending_request_promotes_only_its_exact_early_completion() {
    let start = |id| {
        json!({"method":"turn/started","params":{"threadId":"thread-exact",
        "turn":{"id":id,"status":"inProgress","items":[]}}})
    };
    let finish = |id, text| {
        json!({"method":"turn/completed","params":{"threadId":"thread-exact",
        "turn":{"id":id,"status":"completed","items":[{"id":format!("answer-{id}"),"type":"agentMessage","text":text}]}}})
    };
    let mut adapter = adapter();
    adapter.receive(start("old-a")).unwrap();
    adapter.receive(finish("old-a", "Old A completed")).unwrap();
    let submitted = adapter
        .command(Command::Prompt {
            text: "New B".into(),
            attachments: vec![],
            mode: PromptMode::Send,
        })
        .unwrap();
    let request = submitted
        .writes
        .iter()
        .find(|frame| frame["method"] == "turn/start")
        .unwrap()["id"]
        .clone();
    let a = adapter.receive(finish("old-a", "Replayed A")).unwrap();
    let b = adapter
        .receive(finish("new-b", "Exact B final response"))
        .unwrap();
    for pending in [&a, &b] {
        assert!(pending.suppress_current_notification);
        assert!(
            !pending
                .events
                .iter()
                .any(|event| matches!(event, Event::TurnFinished { .. }))
        );
        assert!(
            pending
                .events
                .iter()
                .any(|event| matches!(event, Event::Item(_))),
            "history still receives completed items"
        );
    }
    let accepted = adapter
        .receive(json!({"id":request,"result":{"turn":{"id":"new-b"}}}))
        .unwrap();
    assert!(!accepted.suppress_current_notification);
    let outcomes = accepted
        .events
        .iter()
        .filter_map(|event| match event {
            Event::TurnFinished { key, outcome } => Some((key.as_deref(), *outcome)),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        outcomes,
        vec![(
            Some("codex:thread-exact:new-b"),
            crate::notifications::Kind::Succeeded
        )]
    );
    let projected =
        crate::chat::notification::FrameContext::capture(&crate::chat::Snapshot::default())
            .project(&accepted.events);
    assert_eq!(projected.len(), 1);
    assert_eq!(projected[0].preview, "Exact B final response");
    assert!(!adapter.busy);
}

#[test]
fn notification_early_completion_buffer_is_bounded_and_replay_replaces_in_place() {
    let mut adapter = adapter();
    adapter
        .command(Command::Prompt {
            text: "B".into(),
            attachments: vec![],
            mode: PromptMode::Send,
        })
        .unwrap();
    let frame = |id: &str| {
        json!({"method":"turn/completed","params":{"threadId":"thread-exact",
        "turn":{"id":id,"status":"completed","items":[]}}})
    };
    for _ in 0..32 {
        adapter.receive(frame("same-old-turn")).unwrap();
    }
    let (count, bytes) = adapter
        .pending
        .values()
        .find_map(|pending| match pending {
            Pending::Turn {
                early, early_bytes, ..
            } => Some((early.len(), *early_bytes)),
            _ => None,
        })
        .unwrap();
    assert_eq!(count, 1);
    let raw = frame("same-old-turn");
    assert_eq!(
        bytes,
        serde_json::to_vec(&raw["params"]["turn"]).unwrap().len()
    );
    for index in 0..7 {
        adapter
            .receive(frame(&format!("unrelated-{index}")))
            .unwrap();
    }
    let refused = adapter.receive(frame("ninth-unrelated-turn")).unwrap_err();
    assert!(refused.contains("correlation capacity"));
    assert!(
        adapter
            .pending
            .values()
            .any(|pending| matches!(pending, Pending::Turn { early, .. } if early.len() == 8))
    );
}

#[test]
fn notification_early_managed_result_cannot_replace_a_newer_current_turn() {
    use crate::agent_execution::ProviderEvent;
    let mut adapter = adapter();
    let run = crate::model::RunId::new();
    let submitted = adapter
        .command_for_run(
            run,
            Command::Prompt {
                text: "B".into(),
                attachments: vec![],
                mode: PromptMode::Send,
            },
        )
        .unwrap();
    let request = submitted
        .writes
        .iter()
        .find(|frame| frame["method"] == "turn/start")
        .unwrap()["id"]
        .clone();
    adapter.receive(json!({"method":"turn/completed","params":{"threadId":"thread-exact",
        "turn":{"id":"early-b","status":"completed","items":[{"id":"b-answer","type":"agentMessage","text":"B result retained"}]}}})).unwrap();
    adapter
        .receive(
            json!({"method":"turn/started","params":{"threadId":"thread-exact",
        "turn":{"id":"new-c","status":"inProgress","items":[]}}}),
        )
        .unwrap();
    let accepted = adapter
        .receive(json!({"id":request,"result":{"turn":{"id":"early-b"}}}))
        .unwrap();
    assert!(accepted.suppress_current_notification);
    assert_eq!(adapter.turn.as_deref(), Some("new-c"));
    assert!(adapter.busy);
    assert!(!accepted.events.iter().any(|event| matches!(
        event,
        Event::TurnFinished { .. } | Event::Status { busy: false, .. }
    )));
    assert!(accepted.events.iter().any(|event| matches!(event, Event::Execution { run: current, event: ProviderEvent::Finished { outcome: crate::notifications::Kind::Succeeded } } if *current == run)));
    assert!(accepted.events.iter().any(|event| matches!(event, Event::Item(item) if matches!(&item.content, ItemContent::Text { text, .. } if text == "B result retained"))));
}

#[test]
fn notification_callback_sees_completed_frame_text_and_failed_frame_diagnosis() {
    for (status, expected_kind, error, expected) in [
        (
            "completed",
            crate::notifications::Kind::Succeeded,
            serde_json::Value::Null,
            "Updated notification_preview in config_file.rs.",
        ),
        (
            "failed",
            crate::notifications::Kind::Failed,
            json!({"message":"The provider rejected this request"}),
            "The provider rejected this request",
        ),
    ] {
        let frame = json!({"method":"turn/completed","params":{"threadId":"thread-exact","turn":{"id":"notification-turn","status":status,
            "items":[{"id":"final-answer","type":"agentMessage","text":"Updated `notification_preview` in config_file.rs."}],"error":error}}});
        for fail_store in [false, true] {
            let mut callbacks = crate::chat::runtime::notification_frames_for_test(
                Box::new(adapter()),
                vec![
                    json!({"method":"turn/started","params":{"threadId":"thread-exact",
                    "turn":{"id":"notification-turn","status":"inProgress","items":[]}}}),
                    frame.clone(),
                ],
                fail_store,
            );
            assert_eq!(callbacks.len(), 1);
            let (evidence, content) = callbacks.pop().unwrap();
            assert_eq!(
                evidence.kind, expected_kind,
                "storage failure cannot erase the provider's authoritative outcome"
            );
            assert_eq!(
                content.preview, expected,
                "immutable callback context must include validated final metadata independently of storage success"
            );
        }
    }
}

#[test]
fn attention_requires_explicit_settlement_of_the_bound_thread() {
    use crate::notifications::Kind;
    for (status, expected) in [
        ("completed", Some(Kind::Succeeded)),
        ("failed", Some(Kind::Failed)),
        ("interrupted", Some(Kind::Cancelled)),
        ("future", None),
    ] {
        let mut adapter = adapter();
        let other = adapter.receive(json!({"method":"turn/completed","params":{"threadId":"other","turn":{"id":"turn","status":status,"items":[]}}})).unwrap();
        assert!(
            !other
                .events
                .iter()
                .any(|event| matches!(event, Event::TurnFinished { .. }))
        );
        adapter.receive(json!({"method":"turn/started","params":{"threadId":"thread-exact","turn":{"id":"turn","status":"inProgress","items":[]}}})).unwrap();
        let step = adapter.receive(json!({"method":"turn/completed","params":{"threadId":"thread-exact","turn":{"id":"turn","status":status,"items":[]}}})).unwrap();
        let outcome = step.events.iter().find_map(|event| match event {
            Event::TurnFinished { key, outcome } => Some((key.as_deref(), *outcome)),
            _ => None,
        });
        assert_eq!(
            outcome,
            expected.map(|kind| (Some("codex:thread-exact:turn"), kind))
        );
    }
}

#[test]
fn an_explicit_default_remote_starts_new_threads_on_the_shared_server() {
    let home = tempfile::tempdir().unwrap();
    let directory = home.path().join("app-server-control");
    std::fs::create_dir(&directory).unwrap();
    let socket = directory.join("app-server-control.sock");
    let _listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
    let launch = Launch {
        provider: crate::agent::Provider::Codex,
        session_id: None,
        recipe: crate::model::LaunchRecipe {
            unset_environment: Default::default(),
            program: "codex".into(),
            arguments: vec!["--remote".into(), "unix://".into()],
            working_directory: home.path().into(),
            environment: BTreeMap::from([(
                "CODEX_HOME".into(),
                home.path().to_string_lossy().into_owned(),
            )]),
        },
    };
    let (spec, mut adapter) = create(&launch).unwrap();
    assert!(matches!(
        spec.transport,
        TransportSpec::UnixWebSocket { .. }
    ));
    assert!(spec.arguments.is_empty());
    let initialize = adapter.initialize().unwrap();
    let result = adapter.receive(json!({"id":initialize.writes[0]["id"],"result":{"userAgent":"codex","codexHome":home.path()}})).unwrap();
    assert!(
        result
            .writes
            .iter()
            .any(|frame| frame["method"] == "thread/start")
    );
    drop(_listener);
    std::fs::remove_file(socket).unwrap();
    assert!(
        create(&launch).is_err(),
        "an unavailable selected server must not create a private thread"
    );
}

#[test]
fn align_resume_identity_rewrites_resume_thread() {
    let mut arguments = vec![
        "app-server".to_string(),
        "resume".to_string(),
        "old".to_string(),
    ];
    align_resume_identity(&mut arguments, "new").unwrap();
    assert_eq!(arguments, vec!["app-server", "resume", "new"]);
    assert!(align_resume_identity(&mut ["resume".to_string()], "new").is_err());
}

#[test]
fn native_queue_is_confirmed_paginated_and_refreshed_after_consumption() {
    let mut adapter = adapter();
    let prompt = adapter
        .command(Command::Prompt {
            text: "next".into(),
            attachments: Vec::new(),
            mode: PromptMode::Queue,
        })
        .unwrap();
    assert!(
        !prompt
            .events
            .iter()
            .any(|event| matches!(event, Event::QueueChanged(_)))
    );
    let added = adapter.receive(json!({"id":prompt.writes[0]["id"], "result":{"queuedSubmission":{"id":"q1","input":[{"type":"text","text":"next"}]}}})).unwrap();
    assert_eq!(added.writes[0]["method"], "thread/queue/list");
    let page = adapter.receive(json!({"id":added.writes[0]["id"],"result":{"data":[{"id":"q1","input":[{"type":"text","text":"next"}]}],"nextCursor":"page2"}})).unwrap();
    assert!(
        !page
            .events
            .iter()
            .any(|event| matches!(event, Event::QueueChanged(_)))
    );
    assert_eq!(page.writes[0]["params"]["cursor"], "page2");
    let last = adapter.receive(json!({"id":page.writes[0]["id"],"result":{"data":[{"id":"q2","input":[{"type":"localImage","path":"/fixture/image.png"}]}],"nextCursor":null}})).unwrap();
    assert!(last.events.iter().any(|event| matches!(event, Event::QueueChanged(messages) if messages.len() == 2 && messages[0].text == "next" && messages[0].editable && messages[1].text.is_empty() && messages[1].editable && messages[1].attachments == [Attachment { path: "/fixture/image.png".into(), reference: None }])));
    let changed = adapter
        .receive(json!({"method":"thread/queue/changed","params":{"threadId":"thread-exact"}}))
        .unwrap();
    let empty = adapter
        .receive(json!({"id":changed.writes[0]["id"],"result":{"data":[],"nextCursor":null}}))
        .unwrap();
    assert!(
        empty
            .events
            .iter()
            .any(|event| matches!(event, Event::QueueChanged(messages) if messages.is_empty()))
    );
}

#[test]
fn native_queue_rejection_and_stale_pages_never_publish_false_previews() {
    let mut adapter = adapter();
    let prompt = adapter
        .command(Command::Prompt {
            text: "rejected".into(),
            attachments: Vec::new(),
            mode: PromptMode::Queue,
        })
        .unwrap();
    let rejected = adapter
        .receive(
            json!({"id":prompt.writes[0]["id"],"error":{"code":-1,"message":"queue rejected"}}),
        )
        .unwrap();
    assert!(
        rejected
            .events
            .iter()
            .any(|event| matches!(event, Event::Error { fatal: false, .. }))
    );
    assert!(
        !rejected
            .events
            .iter()
            .any(|event| matches!(event, Event::QueueChanged(_)))
    );
    let first = adapter
        .receive(json!({"method":"thread/queue/changed","params":{"threadId":"thread-exact"}}))
        .unwrap();
    let invalidated = adapter
        .receive(json!({"method":"thread/queue/changed","params":{"threadId":"thread-exact"}}))
        .unwrap();
    assert!(invalidated.writes.is_empty());
    let stale = adapter.receive(json!({"id":first.writes[0]["id"],"result":{"data":[{"id":"old","input":[{"type":"text","text":"old"}]}],"nextCursor":null}})).unwrap();
    assert!(
        !stale
            .events
            .iter()
            .any(|event| matches!(event, Event::QueueChanged(_)))
    );
    assert_eq!(stale.writes[0]["method"], "thread/queue/list");
}

#[test]
fn fast_mode_acknowledgements_preserve_model_and_effort_and_reject_false_state() {
    let mut adapter = adapter();
    adapter.fast_models.insert("model".into());
    adapter.reasoning = Some("future-effort".into());
    let request = adapter
        .command(Command::SetFastMode { enabled: true })
        .unwrap();
    assert_eq!(request.writes[0]["method"], "thread/settings/update");
    assert_eq!(
        request.writes[0]["params"],
        json!({"threadId":"thread-exact","serviceTier":"fast"})
    );
    assert!(request.events.is_empty());
    assert!(
        adapter
            .command(Command::SetFastMode { enabled: true })
            .is_err()
    );
    let refused = adapter
        .receive(
            json!({"id":request.writes[0]["id"],"error":{"code":-1,"message":"fast unavailable"}}),
        )
        .unwrap();
    assert!(
        refused
            .events
            .iter()
            .any(|event| matches!(event, Event::Error { fatal: false, .. }))
    );
    assert!(adapter.service_tier.is_none());
    let request = adapter
        .command(Command::SetFastMode { enabled: true })
        .unwrap();
    let accepted = adapter
        .receive(json!({"id":request.writes[0]["id"],"result":{}}))
        .unwrap();
    assert!(
        accepted
            .events
            .iter()
            .any(|event| matches!(event, Event::FastMode(value) if value.enabled()))
    );
    assert_eq!(adapter.selected.as_deref(), Some("model"));
    assert_eq!(adapter.reasoning.as_deref(), Some("future-effort"));
    let request = adapter
        .command(Command::SetFastMode { enabled: false })
        .unwrap();
    assert!(request.writes[0]["params"]["serviceTier"].is_null());
    adapter
        .receive(json!({"id":request.writes[0]["id"],"result":{}}))
        .unwrap();
    assert!(adapter.service_tier.is_none());
}

#[test]
fn fast_mode_applies_staged_model_and_effort_with_the_tier() {
    let mut adapter = adapter();
    adapter.effective_model = Some("previous-model".into());
    adapter.fast_models.insert("model".into());
    adapter
        .command(Command::SetModel {
            model: "model".into(),
            reasoning: Some("future-effort".into()),
        })
        .unwrap();
    let request = adapter
        .command(Command::SetFastMode { enabled: true })
        .unwrap();
    assert_eq!(
        request.writes[0]["params"],
        json!({"threadId":"thread-exact","serviceTier":"fast","model":"model","effort":"future-effort"})
    );
    adapter
        .receive(json!({"id":request.writes[0]["id"],"result":{}}))
        .unwrap();
    assert_eq!(adapter.effective_model.as_deref(), Some("model"));
    assert_eq!(adapter.reasoning.as_deref(), Some("future-effort"));
}

#[test]
fn fast_mode_settings_notifications_win_over_delayed_acknowledgements() {
    let mut adapter = adapter();
    adapter.fast_models.insert("model".into());
    let request = adapter
        .command(Command::SetFastMode { enabled: true })
        .unwrap();
    adapter.receive(json!({"method":"thread/settings/updated","params":{"threadId":"thread-exact","threadSettings":{"model":"model","effort":null,"serviceTier":null}}})).unwrap();
    let reply = adapter
        .receive(json!({"id":request.writes[0]["id"],"result":{}}))
        .unwrap();
    assert!(adapter.service_tier.is_none());
    assert!(
        !reply
            .events
            .iter()
            .any(|event| matches!(event, Event::FastMode(value) if value.enabled()))
    );
}

#[test]
fn fast_mode_is_advertised_only_by_selected_model_service_tiers() {
    let mut adapter = adapter();
    assert!(
        adapter
            .command(Command::SetFastMode { enabled: true })
            .is_err()
    );
    for field in [
        json!({"serviceTiers":[{"id":"fast","name":"Fast","description":"Faster generation"}]}),
        json!({"additionalSpeedTiers":["fast"]}),
    ] {
        let mut value =
            json!({"model":"model","displayName":"Model","supportedReasoningEfforts":[]});
        value
            .as_object_mut()
            .unwrap()
            .extend(field.as_object().unwrap().clone());
        let mut step = Step::default();
        adapter
            .call(&mut step, "model/list", json!({}), Pending::Models)
            .unwrap();
        adapter
            .receive(json!({"id":step.writes[0]["id"],"result":{"data":[value],"nextCursor":null}}))
            .unwrap();
        assert!(adapter.capabilities().fast_mode);
    }
    adapter.direct_input = false;
    assert!(!adapter.capabilities().fast_mode);
    assert!(
        adapter
            .command(Command::SetFastMode { enabled: true })
            .is_err()
    );
}

#[test]
fn codex_cost_estimate_keeps_active_turn_tariff_through_model_and_fast_changes() {
    let mut adapter = adapter();
    adapter.selected = Some("gpt-6.1-sol".into());
    adapter.effective_model = adapter.selected.clone();
    let started = |id| json!({"method":"turn/started","params":{"threadId":"thread-exact","turn":{"id":id,"items":[],"status":"inProgress"}}});
    let usage = |id, input, output, last_input, last_output| json!({"method":"thread/tokenUsage/updated","params":{"threadId":"thread-exact","turnId":id,"tokenUsage":{"total":{"inputTokens":input,"cachedInputTokens":0,"outputTokens":output},"last":{"inputTokens":last_input,"cachedInputTokens":0,"outputTokens":last_output}}}});
    let estimate = |step: Step| {
        step.events
            .into_iter()
            .find_map(|event| match event {
                Event::Usage(usage) => usage.cost_estimate.map(|estimate| estimate.microusd),
                _ => None,
            })
            .unwrap()
    };
    adapter.receive(started("first")).unwrap();
    assert_eq!(
        estimate(
            adapter
                .receive(usage("first", 1000, 100, 1000, 100))
                .unwrap()
        ),
        3000
    );
    adapter.selected = Some("unpriced-staged-selection".into());
    adapter.service_tier = Some("fast".into());
    assert_eq!(
        estimate(
            adapter
                .receive(usage("first", 2000, 200, 2000, 200))
                .unwrap()
        ),
        6000
    );
    adapter.receive(started("second")).unwrap();
    assert_eq!(
        estimate(
            adapter
                .receive(usage("second", 3000, 300, 1000, 100))
                .unwrap()
        ),
        12000
    );
    assert_eq!(
        estimate(
            adapter
                .receive(usage("first", 2000, 200, 2000, 200))
                .unwrap()
        ),
        12000
    );
}

fn adapter() -> Codex {
    Codex {
        server_identity: None,
        retained_identity: None,
        thread: Some("thread-exact".into()),
        start: json!({}),
        alias: None,
        cwd: PathBuf::from("/tmp"),
        expected_home: std::fs::canonicalize("/tmp").unwrap(),
        next_id: 0,
        pending: BTreeMap::new(),
        requests: BTreeMap::new(),
        active_items: BTreeMap::new(),
        output_streams: BTreeMap::new(),
        output_windows: BTreeMap::new(),
        models: vec![Model {
            id: "model".into(),
            label: "Model".into(),
            reasoning: vec!["future-effort".into()],
        }],
        listing: Vec::new(),
        modalities: BTreeMap::new(),
        selected: Some("model".into()),
        reasoning: None,
        selection_dirty: false,
        service_tier: None,
        service_tier_revision: 0,
        fast_models: Default::default(),
        effective_model: None,
        effective_reasoning: None,
        running_model: None,
        running_reasoning: None,
        model_turn: None,
        model_turn_settled: false,
        recent_model_turns: Default::default(),
        default_reasoning: Default::default(),
        pricing: Default::default(),
        turn_pricing: Default::default(),
        turn: None,
        notification_turn: None,
        notification_revision: 0,
        busy: false,
        lifecycle_revision: 0,
        suggestion_turn: None,
        prediction_turn: None,
        predictions_unsupported: false,
        subagent_status: None,
        subagent_waiting: false,
        subagent_detail: String::new(),
        ready: true,
        attached: true,
        direct_input: true,
        cursor: None,
        history_initialized: false,
        history_loading: false,
        history_refresh: false,
        queue_available: true,
        queue_stale: false,
        queue_listing: Vec::new(),
        queue_cursors: std::collections::BTreeSet::new(),
        skills_stale: false,
        skill_paths: Vec::new(),
        materialized: true,
        execution: None,
        children: BTreeMap::new(),
        child_names: BTreeMap::new(),
        branch_points: Vec::new(),
        is_child: false,
        child_failure: None,
        child_buffer: BTreeMap::new(),
        turn_diffs: BTreeMap::new(),
        repo_root: None,
    }
}

#[test]
fn retry_notices_are_timeline_items_and_exhausted_retries_are_errors() {
    let mut adapter = adapter();
    let frame = json!({"method":"error","params":{"threadId":"thread-exact","turnId":"turn","willRetry":true,"error":{"message":"Reconnecting... 1/2"}}});
    let first = adapter.receive(frame.clone()).unwrap();
    let second = adapter.receive(frame.clone()).unwrap();
    let [Event::Item(item)] = first.events.as_slice() else {
        panic!("{:?}", first.events)
    };
    assert_eq!(item.role, Role::System);
    assert_eq!(
        chat::failure::provider_error_text(Provider::Codex, item),
        Some("Reconnecting... 1/2")
    );
    assert!(chat::failure::provider_error_text(Provider::Claude, item).is_none());
    let [Event::Item(next)] = second.events.as_slice() else {
        panic!("{:?}", second.events)
    };
    assert_ne!(item.id, next.id);
    assert_eq!(
        chat::failure::provider_error_text(Provider::Codex, next),
        Some("Reconnecting... 1/2")
    );
    for (cause, expected_kind) in [
        (
            "You've hit your limit · resets 1:30pm (America/Los_Angeles)",
            chat::failure::FailureKind::AccountLimit,
        ),
        (
            "Your access token could not be refreshed. Please sign in again.",
            chat::failure::FailureKind::Authentication,
        ),
    ] {
        let raw = format!(
            "{cause}\nHTTP raw response: {}",
            "retained diagnostic ".repeat(120_000)
        );
        assert!(raw.len() > 2 * 1024 * 1024);
        let mut retry = frame.clone();
        retry["params"]["error"]["message"] = json!(raw);
        let retry = adapter.receive(retry).unwrap();
        let [Event::Item(error)] = retry.events.as_slice() else {
            panic!("retry must remain only a timeline item");
        };
        assert!(error.id.starts_with("codex-notice:"));
        assert_ne!(error.id, item.id);
        assert_eq!(
            chat::failure::provider_error_text(Provider::Codex, error),
            Some(raw.as_str())
        );
        assert_eq!(
            chat::failure::Failure::parse(Provider::Codex, &raw)
                .unwrap()
                .kind,
            expected_kind
        );
        let warning = adapter
            .receive(json!({"method":"warning","params":{"threadId":"thread-exact","message":raw}}))
            .unwrap();
        let [Event::Item(warning)] = warning.events.as_slice() else {
            panic!("warning must remain a timeline item");
        };
        assert!(chat::failure::provider_error_text(Provider::Codex, warning).is_none());
        assert!(matches!(&warning.content, ItemContent::Text { text, .. } if text == &raw));
    }
    for will_retry in [json!(false), Value::Null] {
        let mut failed = frame.clone();
        failed["params"]["willRetry"] = will_retry;
        assert!(matches!(
            adapter.receive(failed).unwrap().events.as_slice(),
            [Event::Error { fatal: false, .. }]
        ));
    }
    let mut foreign = frame;
    foreign["params"]["threadId"] = json!("another-thread");
    assert!(adapter.receive(foreign).unwrap().events.is_empty());
}

#[test]
fn child_retry_is_recorded_in_the_child_timeline() {
    let mut parent = adapter();
    let mut child = adapter();
    child.thread = Some("child-thread".into());
    child.is_child = true;
    parent
        .children
        .insert("child-thread".into(), Box::new(child));
    let step = parent.receive(json!({"method":"error","params":{"threadId":"child-thread","turnId":"child-turn","willRetry":true,"error":{"message":"Reconnecting... 1/2"}}})).unwrap();
    let [Event::SubagentItem { id, item }] = step.events.as_slice() else {
        panic!("A child retry must remain only in its exact child timeline");
    };
    assert_eq!(id, "child-thread");
    assert_eq!(item.role, Role::System);
    assert!(item.id.starts_with("codex-notice:"));
    assert_eq!(
        chat::failure::provider_error_text(Provider::Codex, item),
        Some("Reconnecting... 1/2")
    );
    assert!(chat::failure::provider_error_text(Provider::Claude, item).is_none());
}

#[test]
fn a_child_failed_turn_restating_its_final_error_adds_no_second_notice() {
    let mut parent = adapter();
    let mut child = adapter();
    child.thread = Some("child-thread".into());
    child.is_child = true;
    parent
        .children
        .insert("child-thread".into(), Box::new(child));
    parent.receive(json!({"method":"turn/started","params":{"threadId":"child-thread","turn":{"id":"child-turn","items":[]}}})).unwrap();
    let notices = |step: Step| {
        step.events
            .iter()
            .filter(|event| {
                matches!(event, Event::SubagentItem { item, .. }
                if chat::failure::provider_error_text(Provider::Codex, item).is_some())
            })
            .count()
    };
    let error = parent.receive(json!({"method":"error","params":{"threadId":"child-thread","turnId":"child-turn","willRetry":false,"error":{"message":"Usage limit · resets 3:40pm"}}})).unwrap();
    assert_eq!(notices(error), 1);
    let failed = |turn: &str, message: &str| json!({"method":"turn/completed","params":{"threadId":"child-thread","turn":{"id":turn,"status":"failed","error":{"message":message},"items":[]}}});
    let restated = parent
        .receive(failed("child-turn", "Usage limit · resets 3:40pm"))
        .unwrap();
    assert_eq!(notices(restated), 0, "the same failure is already shown");
    parent.receive(json!({"method":"turn/started","params":{"threadId":"child-thread","turn":{"id":"next-turn","items":[]}}})).unwrap();
    let different = parent
        .receive(failed("next-turn", "Usage limit · resets 3:40pm"))
        .unwrap();
    assert_eq!(notices(different), 1, "another turn's failure is its own");
}

#[test]
fn terminal_child_errors_keep_exact_provider_notices_without_claiming_lifecycle() {
    let mut parent = adapter();
    let mut child = adapter();
    child.thread = Some("child-thread".into());
    child.is_child = true;
    parent
        .children
        .insert("child-thread".into(), Box::new(child));
    parent.receive(json!({"method":"turn/started","params":{"threadId":"thread-exact","turn":{"id":"parent-turn","items":[]}}})).unwrap();
    parent.receive(json!({"method":"turn/started","params":{"threadId":"child-thread","turn":{"id":"child-turn","items":[]}}})).unwrap();
    parent.receive(json!({"method":"thread/status/changed","params":{"threadId":"child-thread","status":{"type":"active","activeFlags":["waitingOnUserInput"]}}})).unwrap();
    let raw = format!(
        "You've hit your session limit · resets 3:40pm (America/Vancouver)\nrequest-id: child-original\n{}\nFinal diagnostic λ",
        "original response\n".repeat(5_000)
    );
    let mut ids = std::collections::BTreeSet::new();
    for will_retry in [Some(json!(false)), Some(Value::Null), None] {
        let mut frame = json!({"method":"error","params":{"threadId":"child-thread","turnId":"child-turn","error":{"message":raw}}});
        if let Some(flag) = will_retry {
            frame["params"]["willRetry"] = flag;
        }
        // Repeated occurrences have no event ID on this wire. Preserve
        // each original rather than deduplicating equal diagnostic text.
        for _ in 0..2 {
            let observed = parent.receive(frame.clone()).unwrap();
            let [Event::SubagentItem { id, item }] = observed.events.as_slice() else {
                panic!("A child failure must be only its own transcript item");
            };
            assert_eq!(id, "child-thread");
            assert_eq!(item.role, Role::System);
            assert!(ids.insert(item.id.clone()));
            assert_eq!(
                chat::failure::provider_error_text(Provider::Codex, item),
                Some(raw.as_str())
            );
            assert!(chat::failure::provider_error_text(Provider::Claude, item).is_none());
            assert!(
                observed.writes.is_empty(),
                "A diagnostic never retries or sends a command"
            );
            assert!(parent.busy);
            assert_eq!(parent.turn.as_deref(), Some("parent-turn"));
            let child = &parent.children["child-thread"];
            assert!(child.busy && child.subagent_waiting);
            assert_eq!(child.turn.as_deref(), Some("child-turn"));
            assert_eq!(child.subagent_status, Some(ToolStatus::Running));
            assert_eq!(child.subagent_detail, "Waiting for your answer");
        }
        let mut foreign = frame.clone();
        foreign["params"]["threadId"] = json!("unrelated-thread");
        let ignored = parent.receive(foreign).unwrap();
        assert!(ignored.events.is_empty() && ignored.writes.is_empty());
        frame["params"]["threadId"] = json!("thread-exact");
        let original = parent.receive(frame).unwrap();
        assert!(
            matches!(original.events.as_slice(), [Event::Error {message, fatal:false}] if message == &raw)
        );
        assert!(original.writes.is_empty());
    }
    let warning = parent
        .receive(json!({"method":"warning","params":{"threadId":"child-thread","message":raw}}))
        .unwrap();
    let [Event::SubagentItem { id, item }] = warning.events.as_slice() else {
        panic!("A child advisory must remain a child advisory");
    };
    assert_eq!(id, "child-thread");
    assert!(chat::failure::provider_error_text(Provider::Codex, item).is_none());
    assert!(matches!(&item.content, ItemContent::Text {text,..} if text == &raw));
}

/// Known child parsers already attached to this account's parent thread,
/// as in the existing child routing tests. Native metadata frames below
/// publish their exact names through the real adapter and runtime store.
fn child_error_runtime_fixture() -> (Codex, Vec<Value>) {
    let mut parent = adapter();
    for id in ["child-thread", "sibling-thread"] {
        let mut child = adapter();
        child.thread = Some(id.into());
        child.is_child = true;
        parent.children.insert(id.into(), Box::new(child));
    }
    let mut frames = vec![
        json!({"method":"thread/name/updated","params":{"threadId":"thread-exact","threadName":"Parent conversation"}}),
        json!({"method":"turn/started","params":{"threadId":"thread-exact","turn":{"id":"parent-turn","items":[{"id":"parent-answer","type":"agentMessage","text":"Parent work remains here"}]}}}),
        json!({"method":"thread/tokenUsage/updated","params":{"threadId":"thread-exact","turnId":"parent-turn","tokenUsage":{"total":{"inputTokens":71,"outputTokens":19},"last":{"totalTokens":90},"modelContextWindow":1000}}}),
    ];
    for (id, name) in [("child-thread", "Quinn"), ("sibling-thread", "Nora")] {
        frames.push(json!({"method":"thread/started","params":{"thread":{"id":id,"parentThreadId":"thread-exact","agentNickname":name,"model":"child-model","status":{"type":"active"}}}}));
        frames.push(json!({"method":"turn/started","params":{"threadId":id,"turn":{"id":"child-turn","items":[{"id":"answer","type":"agentMessage","text":"Original child work"}]}}}));
    }
    (parent, frames)
}

fn parent_question_after_child_error() -> Value {
    // Supply an independently fingerprinted parent question while its
    // turn stays running. A waiting child can already produce aggregate
    // Waiting attention; a child error must not supply a parent outcome.
    json!({"id":71,"method":"item/tool/requestUserInput","params":{"threadId":"thread-exact","turnId":"parent-turn","questions":[{"id":"parent-choice","header":"Parent choice","question":"Continue parent work?","options":[{"label":"Keep","description":"Keep parent work"},{"label":"Replace","description":"Replace parent work"}]}]}})
}

fn assert_child_error_parent_and_sibling(state: &chat::Snapshot) {
    assert_eq!(state.session_id.as_deref(), Some("thread-exact"));
    assert_eq!(state.title.as_deref(), Some("Parent conversation"));
    assert!(state.busy && state.ready && !state.exited);
    assert!(
        state.error.is_none(),
        "Child failures must not become parent errors"
    );
    assert_eq!(
        state.turn_generation, 2,
        "Only the helper's initial and actual parent starts count"
    );
    assert_eq!(state.usage.input_tokens, 71);
    assert_eq!(state.usage.output_tokens, 19);
    assert_eq!(state.usage.context_tokens, 90);
    assert_eq!(state.usage.context_limit, 1000);
    assert_eq!(
        state.requests.len(),
        1,
        "Only the genuine parent question requests attention"
    );
    assert_eq!(state.items.len(), 1);
    assert!(
        matches!(&state.items[0].content, ItemContent::Text {text,..} if text == "Parent work remains here")
    );
    assert!(chat::failure::provider_error_text(Provider::Codex, &state.items[0]).is_none());
    assert_eq!(
        state.subagents.len(),
        2,
        "Foreign thread frames must not create a child"
    );
    let sibling = state
        .subagents
        .iter()
        .find(|child| child.id == "sibling-thread")
        .unwrap();
    assert_eq!(sibling.kind, "Nora");
    assert_eq!(sibling.model.as_deref(), Some("child-model"));
    assert!(sibling.background && !sibling.waiting);
    assert_eq!(sibling.status, ToolStatus::Running);
    assert!(sibling.detail.is_empty());
    assert_eq!(sibling.items.len(), 1);
    assert!(
        matches!(&sibling.items[0].content, ItemContent::Text {text,..} if text == "Original child work")
    );
}

#[test]
fn terminal_child_error_originals_survive_actor_store_without_parent_contamination() {
    let (parent, mut frames) = child_error_runtime_fixture();
    frames.push(json!({"method":"thread/status/changed","params":{"threadId":"child-thread","status":{"type":"active","activeFlags":["waitingOnUserInput"]}}}));
    let first = format!(
        "You've hit your session limit · resets 3:40pm (America/Vancouver)\nrequest-id: first\n{}\nFinal original λ",
        "response line\n".repeat(400)
    );
    let second = "Connection reset by peer\nrequest-id: second\nDistinct original diagnostic";
    let retry = "Reconnecting... 1/2\nrequest-id: genuine-retry";
    for (flag, raw) in [
        (Some(json!(false)), first.as_str()),
        (None, second),
        (Some(Value::Null), first.as_str()),
        (Some(json!(true)), retry),
    ] {
        let mut frame = json!({"method":"error","params":{"threadId":"child-thread","turnId":"child-turn","error":{"message":raw}}});
        if let Some(flag) = flag {
            frame["params"]["willRetry"] = flag;
        }
        frames.push(frame);
    }
    frames.push(json!({"method":"warning","params":{"threadId":"child-thread","message":first}}));
    frames.push(json!({"method":"error","params":{"threadId":"unrelated-thread","turnId":"child-turn","error":{"message":"Foreign child diagnostic"}}}));
    frames.push(json!({"method":"thread/tokenUsage/updated","params":{"threadId":"child-thread","turnId":"child-turn","tokenUsage":{"total":{"inputTokens":9000,"outputTokens":8000},"last":{"totalTokens":17000},"modelContextWindow":20000}}}));
    frames.push(parent_question_after_child_error());
    let (receipts, state) = crate::chat::runtime::notification_frames_with_snapshot_for_test(
        Box::new(parent),
        frames,
        false,
    );
    let [
        (child_waiting, child_context),
        (parent_question, parent_context),
    ] = receipts.as_slice()
    else {
        panic!("only the real child wait and parent question notify: {receipts:?}");
    };
    assert_eq!(child_waiting.kind, crate::notifications::Kind::Waiting);
    assert_eq!(parent_question.kind, crate::notifications::Kind::Waiting);
    assert_eq!(
        child_context,
        &crate::notifications::Content {
            conversation: "Parent conversation".into(),
            preview: "Parent work remains here".into(),
            request: None,
        },
        "child waiting announces aggregate activity before a parent request exists"
    );
    assert_eq!(
        parent_context,
        &crate::notifications::Content {
            conversation: "Parent conversation".into(),
            preview: "Parent choice · Continue parent work?".into(),
            request: Some(crate::notifications::RequestType::Question),
        },
        "only the genuine parent question changes actionable request metadata"
    );
    let (child_owner, child_generation) = child_waiting.key.rsplit_once(":waiting:").unwrap();
    let (parent_owner, parent_generation) = parent_question.key.rsplit_once(":waiting:").unwrap();
    assert_eq!(child_owner, parent_owner);
    assert!(child_generation.parse::<u64>().unwrap() < parent_generation.parse::<u64>().unwrap());
    assert_child_error_parent_and_sibling(&state);
    let child = state
        .subagents
        .iter()
        .find(|child| child.id == "child-thread")
        .unwrap();
    assert_eq!(child.kind, "Quinn");
    assert_eq!(child.model.as_deref(), Some("child-model"));
    assert!(child.background && child.waiting);
    assert_eq!(
        child.status,
        ToolStatus::Running,
        "No error invents a terminal lifecycle transition"
    );
    assert_eq!(child.detail, "Waiting for your answer");
    let notices: Vec<_> = child
        .items
        .iter()
        .filter(|item| chat::failure::provider_error_text(Provider::Codex, item).is_some())
        .collect();
    assert_eq!(notices.len(), 4);
    assert_eq!(
        notices
            .iter()
            .map(|item| chat::failure::provider_error_text(Provider::Codex, item).unwrap())
            .collect::<Vec<_>>(),
        [first.as_str(), second, first.as_str(), retry]
    );
    assert_eq!(
        notices
            .iter()
            .map(|item| &item.id)
            .collect::<std::collections::BTreeSet<_>>()
            .len(),
        4
    );
    assert!(
        notices
            .iter()
            .all(|item| item.id.starts_with("codex-notice:")
                && item.role == Role::System
                && chat::failure::provider_error_text(Provider::Claude, item).is_none())
    );
    assert_eq!(
        child.items.len(),
        6,
        "Original answer, four failures and unmarked advisory all remain"
    );
    assert!(
        matches!(&child.items[0].content, ItemContent::Text {text,..} if text == "Original child work")
    );
    assert!(matches!(&child.items[5].content, ItemContent::Text {text,..} if text == &first));
}

#[test]
fn failed_child_turn_keeps_original_error_and_authentic_outcome_through_actor_store() {
    let (parent, mut frames) = child_error_runtime_fixture();
    let raw = "You've hit your session limit · resets 3:40pm (America/Vancouver)\nrequest-id: failed-turn\nOriginal server response λ";
    frames.push(json!({"method":"turn/completed","params":{"threadId":"child-thread","turn":{"id":"child-turn","status":"failed","error":{"message":raw},"items":[{"id":"partial","type":"agentMessage","text":"Partial child answer"}]}}}));
    frames.push(json!({"method":"thread/status/changed","params":{"threadId":"child-thread","status":{"type":"idle"}}}));
    frames.push(json!({"method":"turn/completed","params":{"threadId":"unrelated-thread","turn":{"id":"foreign-turn","status":"failed","error":{"message":"Foreign failure"},"items":[]}}}));
    frames.push(parent_question_after_child_error());
    let (receipts, state) = crate::chat::runtime::notification_frames_with_snapshot_for_test(
        Box::new(parent),
        frames,
        false,
    );
    assert_eq!(
        receipts.len(),
        1,
        "Child failure must not notify as a parent result"
    );
    assert_eq!(receipts[0].0.kind, crate::notifications::Kind::Waiting);
    assert_child_error_parent_and_sibling(&state);
    let child = state
        .subagents
        .iter()
        .find(|child| child.id == "child-thread")
        .unwrap();
    assert_eq!(child.kind, "Quinn");
    assert_eq!(child.status, ToolStatus::Failed);
    assert!(!child.waiting);
    assert_eq!(child.items.len(), 3);
    for (item, expected) in child
        .items
        .iter()
        .zip(["Original child work", "Partial child answer"])
    {
        assert!(matches!(&item.content, ItemContent::Text {text,..} if text == expected));
    }
    let error = &child.items[2];
    assert_eq!(
        chat::failure::provider_error_text(Provider::Codex, error),
        Some(raw)
    );
    assert!(chat::failure::provider_error_text(Provider::Claude, error).is_none());
    assert!(error.id.starts_with("codex-notice:"));
}

#[test]
fn child_disconnect_diagnostics_remain_unmarked_and_do_not_fail_the_parent() {
    for method in ["thread/closed", "thread/deleted"] {
        let (parent, mut frames) = child_error_runtime_fixture();
        frames.push(json!({"method":method,"params":{"threadId":"child-thread"}}));
        frames.push(parent_question_after_child_error());
        let (receipts, state) = crate::chat::runtime::notification_frames_with_snapshot_for_test(
            Box::new(parent),
            frames,
            false,
        );
        assert_eq!(receipts.len(), 1);
        assert_eq!(receipts[0].0.kind, crate::notifications::Kind::Waiting);
        assert_child_error_parent_and_sibling(&state);
        let child = state
            .subagents
            .iter()
            .find(|child| child.id == "child-thread")
            .unwrap();
        assert_eq!(child.status, ToolStatus::Stopped);
        assert!(!child.waiting);
        assert_eq!(child.items.len(), 2);
        let notice = &child.items[1];
        assert_eq!(notice.role, Role::System);
        let expected = if method == "thread/deleted" {
            "Codex removed this agent before it finished"
        } else {
            "Codex closed this agent before it finished"
        };
        assert!(matches!(&notice.content, ItemContent::Text {text,..} if text == expected));
        assert!(chat::failure::provider_error_text(Provider::Codex, notice).is_none());
    }
}

#[test]
fn large_child_originals_survive_actor_reads_and_old_completion_cannot_settle_next_turn() {
    let (parent, mut frames) = child_error_runtime_fixture();
    let raw = format!(
        "You've hit your session limit · resets 3:40pm (America/Vancouver)\nrequest-id: large-original\n{}\nTAIL original λ",
        "猫 café diagnostic\n".repeat(20_000)
    );
    frames.push(json!({"method":"error","params":{"threadId":"child-thread","turnId":"child-turn","error":{"message":raw},"willRetry":false}}));
    let failed = json!({"method":"turn/completed","params":{"threadId":"child-thread","turn":{"id":"child-turn","status":"failed","error":{"message":raw},"items":[]}}});
    frames.push(failed.clone());
    frames.push(json!({"method":"thread/status/changed","params":{"threadId":"child-thread","status":{"type":"idle"}}}));
    frames.push(json!({"method":"thread/closed","params":{"threadId":"child-thread"}}));
    frames.push(json!({"method":"turn/started","params":{"threadId":"child-thread","turn":{"id":"next-child","status":"inProgress","items":[]}}}));
    frames.push(failed);
    frames.push(parent_question_after_child_error());
    let (receipts, state, originals) =
        crate::chat::runtime::notification_frames_with_probe_for_test(
            Box::new(parent),
            frames,
            false,
            |state, read| -> Result<Vec<String>, String> {
                let child = state
                    .subagents
                    .iter()
                    .find(|child| child.id == "child-thread")
                    .ok_or("missing child")?;
                let sources = state
                    .text_copy_sources
                    .children
                    .get("child-thread")
                    .ok_or("missing child authority")?;
                let mut originals = Vec::new();
                for (index, item) in child.items.iter().enumerate() {
                    if chat::failure::provider_error_text(Provider::Codex, item).is_none() {
                        continue;
                    }
                    let source = sources
                        .get(index)
                        .and_then(Option::as_ref)
                        .ok_or("original not retained")?;
                    let mut raw = String::new();
                    loop {
                        let page = read(source, raw.len() as u64)?;
                        if page.source != *source || page.offset != raw.len() as u64 {
                            return Err("wrong page identity".into());
                        }
                        raw.push_str(&page.text);
                        if page.done {
                            break;
                        }
                        if page.text.is_empty() {
                            return Err("empty original page".into());
                        }
                    }
                    originals.push(raw);
                }
                Ok(originals)
            },
        );
    assert_child_error_parent_and_sibling(&state);
    assert_eq!(receipts.len(), 1);
    assert_eq!(receipts[0].0.kind, crate::notifications::Kind::Waiting);
    assert_eq!(
        receipts[0].1.preview,
        "Parent choice · Continue parent work?"
    );
    let child = state
        .subagents
        .iter()
        .find(|child| child.id == "child-thread")
        .unwrap();
    assert_eq!(child.status, ToolStatus::Running);
    assert!(!child.waiting);
    assert!(child.items.len() <= 64);
    assert!(
        child
            .items
            .iter()
            .map(|item| item.content.byte_len())
            .sum::<usize>()
            <= 64 * 1024
    );
    let originals = originals.unwrap();
    // The failed completion and its stale repeat restate the error
    // notification, so the child shows the one failure once.
    assert_eq!(originals, vec![raw]);
}

#[test]
fn sparse_child_diagnostic_shapes_preserve_exact_fallback_without_lifecycle_claims() {
    for flag in [
        Value::Null,
        json!(false),
        json!(true),
        json!(0),
        json!(1),
        json!("true"),
        json!("false"),
        json!([]),
        json!({}),
    ] {
        for error in [
            Value::Null,
            json!("diagnostic string"),
            json!({"code":429,"requestId":"exact λ"}),
            json!({"message":17}),
        ] {
            let (mut parent, _) = child_error_runtime_fixture();
            let observed = parent.receive(json!({"method":"error","params":{"threadId":"child-thread","willRetry":flag,"error":error}})).unwrap();
            let [Event::SubagentItem { id, item }] = observed.events.as_slice() else {
                panic!("only an exact child diagnostic is allowed");
            };
            assert_eq!(id, "child-thread");
            assert_eq!(
                chat::failure::provider_error_text(Provider::Codex, item),
                Some(error.to_string().as_str())
            );
            assert!(observed.writes.is_empty());
            assert!(!parent.busy && !parent.children["child-thread"].busy);
        }
    }
}

#[test]
fn provider_warnings_are_readable_advisory_timeline_items() {
    for (method, params, expected) in [
        (
            "warning",
            json!({"message":"Warning", "threadId":"thread-exact"}),
            "Warning",
        ),
        (
            "guardianWarning",
            json!({"message":"Guardian warning", "threadId":"thread-exact"}),
            "Guardian warning",
        ),
        (
            "configWarning",
            json!({"summary":"Config warning", "details":"Fix this setting", "path":"/config.toml"}),
            "Config warning\n\nFix this setting\n\n/config.toml",
        ),
        (
            "deprecationNotice",
            json!({"summary":"Deprecated", "details":"Use the new option"}),
            "Deprecated\n\nUse the new option",
        ),
    ] {
        let step = adapter()
            .receive(json!({"method":method,"params":params}))
            .unwrap();
        assert!(
            matches!(step.events.as_slice(), [Event::Item(Item { role: Role::System, content: ItemContent::Text {text, ..}, .. })] if text == expected),
            "{method}: {:?}",
            step.events
        );
    }
}

#[test]
fn history_loading_lasts_until_the_provider_response_and_refresh_coalesces() {
    let mut adapter = adapter();
    adapter.cursor = Some("older".into());
    let request = adapter.command(Command::LoadEarlier).unwrap();
    assert!(request.events.contains(&Event::HistoryLoading(true)));
    assert!(adapter.history_loading);
    assert!(adapter.command(Command::LoadEarlier).is_err());

    // Returning to cached latest must succeed while the older page is
    // still in flight, without issuing a competing history request.
    let refresh = adapter.command(Command::Refresh).unwrap();
    assert!(refresh.writes.is_empty());
    assert!(adapter.command(Command::Refresh).unwrap().writes.is_empty());
    assert!(adapter.history_loading);
    assert!(!refresh.events.contains(&Event::HistoryLoading(false)));

    let answer = adapter
        .receive(json!({"id":request.writes[0]["id"],"result":{"data":[],"nextCursor":null}}))
        .unwrap();
    assert!(
        adapter.history_loading,
        "the requested latest reread follows the older page"
    );
    assert!(matches!(answer.events[0], Event::History { .. }));
    assert_eq!(answer.writes.len(), 2);
    assert_eq!(answer.writes[0]["method"], "thread/read");
    assert_eq!(answer.writes[1]["method"], "thread/turns/list");
    assert!(answer.writes[1]["params"]["cursor"].is_null());
    assert_eq!(answer.events.last(), Some(&Event::LatestHistoryStarted));
    let latest = adapter
        .receive(json!({"id":answer.writes[1]["id"],"result":{"data":[],"nextCursor":null}}))
        .unwrap();
    assert!(!adapter.history_loading);
    assert!(matches!(latest.events[0], Event::LatestHistory { .. }));
    assert_eq!(latest.events.last(), Some(&Event::HistoryLoading(false)));
}

#[test]
fn failed_history_request_releases_loading_and_can_be_retried() {
    let mut adapter = adapter();
    adapter.cursor = Some("older".into());
    let request = adapter.command(Command::LoadEarlier).unwrap();
    let failed = adapter
        .receive(json!({"id":request.writes[0]["id"],"error":{"code":-1,"message":"try later"}}))
        .unwrap();
    assert!(!adapter.history_loading);
    assert_eq!(failed.events[0], Event::HistoryLoading(false));
    assert!(matches!(failed.events[1], Event::HistoryError(_)));
    assert!(adapter.command(Command::LoadEarlier).is_ok());
}

#[test]
fn failed_older_history_still_runs_one_coalesced_latest_refresh() {
    let mut adapter = adapter();
    adapter.cursor = Some("older".into());
    let request = adapter.command(Command::LoadEarlier).unwrap();
    for _ in 0..10 {
        assert!(adapter.command(Command::Refresh).unwrap().writes.is_empty());
    }
    let failed = adapter
        .receive(json!({"id":request.writes[0]["id"],"error":{"message":"older page unavailable"}}))
        .unwrap();
    assert!(adapter.history_loading);
    assert_eq!(failed.writes.len(), 2);
    assert_eq!(failed.writes[1]["method"], "thread/turns/list");
    assert!(failed.writes[1]["params"]["cursor"].is_null());
    let latest = adapter
        .receive(json!({"id":failed.writes[1]["id"],"result":{"data":[],"nextCursor":null}}))
        .unwrap();
    assert!(!adapter.history_loading);
    assert_eq!(latest.events.last(), Some(&Event::HistoryLoading(false)));
}

#[test]
fn latest_history_refresh_keeps_the_furthest_older_cursor_including_exhaustion() {
    let mut adapter = adapter();
    let mut step = Step::default();
    for (cursor, earlier) in [
        (Some("100"), false),
        (Some("80"), true),
        (Some("60"), true),
        (Some("100"), false),
    ] {
        adapter
            .page(&json!({"data":[],"nextCursor":cursor}), &mut step, earlier)
            .unwrap();
    }
    let request = adapter.command(Command::LoadEarlier).unwrap();
    assert_eq!(request.writes[0]["params"]["cursor"], "60");
    adapter
        .receive(json!({"id":request.writes[0]["id"],"result":{"data":[],"nextCursor":null}}))
        .unwrap();
    adapter
        .page(&json!({"data":[],"nextCursor":"100"}), &mut step, false)
        .unwrap();
    assert!(adapter.cursor.is_none());
    assert!(adapter.command(Command::LoadEarlier).is_err());
}

#[test]
fn malformed_history_page_preserves_initialization_and_the_retry_cursor() {
    let mut adapter = adapter();
    let mut step = Step::default();
    let malformed = json!({"data":[{"id":"turn","status":"completed","itemsView":"compact","items":[]}],"nextCursor":"lost"});
    assert!(adapter.page(&malformed, &mut step, false).is_err());
    assert!(!adapter.history_initialized);
    adapter
        .page(&json!({"data":[],"nextCursor":"100"}), &mut step, false)
        .unwrap();
    assert_eq!(adapter.cursor.as_deref(), Some("100"));
    assert!(adapter.page(&malformed, &mut step, true).is_err());
    assert_eq!(adapter.cursor.as_deref(), Some("100"));
}

#[test]
fn failed_initial_history_request_can_initialize_its_cursor_on_retry() {
    let mut adapter = adapter();
    let request = adapter.command(Command::Refresh).unwrap();
    adapter
        .receive(json!({"id":request.writes[1]["id"],"error":{"message":"try later"}}))
        .unwrap();
    assert!(!adapter.history_initialized);
    assert!(adapter.cursor.is_none());
    let retry = adapter.command(Command::Refresh).unwrap();
    adapter
        .receive(json!({"id":retry.writes[1]["id"],"result":{"data":[],"nextCursor":"100"}}))
        .unwrap();
    assert!(adapter.history_initialized);
    assert_eq!(adapter.cursor.as_deref(), Some("100"));
}

#[test]
fn reattachment_requests_subagent_sources_explicitly() {
    let mut parent = adapter();
    parent
        .pending
        .insert(json!("attach").to_string(), Pending::Attach);
    let attached = parent.receive(json!({"id":"attach","result":{"thread":{"id":"thread-exact","status":{"type":"idle"}},"initialTurnsPage":{"data":[],"nextCursor":null}}})).unwrap();
    let list = attached
        .writes
        .iter()
        .find(|frame| frame["method"] == "thread/list")
        .unwrap();
    assert_eq!(
        list["params"]["sourceKinds"],
        json!(["subAgentThreadSpawn"])
    );
    assert_eq!(list["params"]["ancestorThreadId"], "thread-exact");
}

#[test]
fn child_identity_uses_only_reported_nonblank_nickname_without_guessing_roles() {
    let mut parent = adapter();
    for (nickname, expected) in [
        (Value::Null, ""),
        (json!(""), ""),
        (json!(" \t\n"), ""),
        (json!(" Scout "), "Scout"),
        // A real nickname may happen to equal a role or the old default.
        (json!("worker"), "worker"),
        (json!("Codex agent"), "Codex agent"),
    ] {
        let mut step = Step::default();
        parent
            .discover_child(
                "child",
                &json!({"agentNickname":nickname,"agentRole":"worker"}),
                &mut step,
            )
            .unwrap();
        assert!(step.events.iter().any(|event| matches!(event,
            Event::Subagent { id, kind, .. } if id == "child" && kind == expected
        )));
    }
}

#[test]
fn proven_child_start_discovers_early_streams_and_hydration_failure_keeps_them() {
    let mut parent = adapter();
    let started = parent.receive(json!({"method":"thread/started","params":{"thread":{"id":"child", "parentThreadId":"thread-exact", "agentNickname":"Scout", "status":{"type":"active"}}}})).unwrap();
    assert_eq!(started.writes[0]["method"], "thread/resume");
    assert!(parent.children.contains_key("child"));
    let live = parent.receive(json!({"method":"item/agentMessage/delta","params":{"threadId":"child","turnId":"turn","itemId":"answer","delta":"Early streamed text"}})).unwrap();
    assert!(live.events.is_empty());
    let failed = parent
        .receive(json!({"id":started.writes[0]["id"],"error":{"message":"History read failed"}}))
        .unwrap();
    assert!(parent.children.contains_key("child"));
    assert!(!parent.child_buffer.contains_key("child"));
    assert!(failed.events.iter().any(|event| matches!(event, Event::SubagentTextDelta { id, delta, .. } if id == "child" && delta == "Early streamed text")));
    assert!(!failed.events.iter().any(|event| matches!(
        event,
        Event::Subagent {
            status: Some(ToolStatus::Failed),
            ..
        }
    )));
    let foreign = parent.receive(json!({"method":"thread/started","params":{"thread":{"id":"foreign","parentThreadId":"another-root"}}})).unwrap();
    assert!(foreign.events.is_empty());
    assert!(!parent.children.contains_key("foreign"));
}

#[test]
fn child_discovery_cannot_replay_active_metadata_over_idle_hydration() {
    let mut parent = adapter();
    let started = parent.receive(json!({"method":"thread/started","params":{"thread":{"id":"child","parentThreadId":"thread-exact","agentNickname":"Scout","status":{"type":"active"}}}})).unwrap();
    let hydrated = parent.receive(json!({"id":started.writes[0]["id"],"result":{"thread":{"id":"child","status":{"type":"idle"}},"initialTurnsPage":{"data":[],"nextCursor":null}}})).unwrap();
    assert!(!parent.children["child"].busy);
    assert!(hydrated.events.iter().any(|event| matches!(
        event,
        Event::Subagent {
            status: Some(ToolStatus::Succeeded),
            ..
        }
    )));
    let mut sparse = Step::default();
    parent
        .discover_child("child", &json!({}), &mut sparse)
        .unwrap();
    assert!(
        sparse.events.iter().any(
            |event| matches!(event, Event::Subagent {kind, status:None, ..} if kind.is_empty())
        )
    );
}

#[test]
fn native_prompt_late_ack_does_not_claim_a_new_active_turn_without_an_id() {
    let mut adapter = adapter();
    let sent = adapter
        .command(Command::Prompt {
            text: "Prompt".into(),
            attachments: Vec::new(),
            mode: PromptMode::Send,
        })
        .unwrap();
    adapter.receive(json!({"method":"turn/completed","params":{"threadId":"thread-exact","turn":{"id":"old","status":"completed","items":[]}}})).unwrap();
    adapter.receive(json!({"method":"thread/status/changed","params":{"threadId":"thread-exact","status":{"type":"active"}}})).unwrap();
    adapter.receive(json!({"id":sent.writes[0]["id"],"result":{"turn":{"id":"old","status":"inProgress","items":[]}}})).unwrap();
    assert!(adapter.busy);
    assert_eq!(adapter.turn, None);
    assert_eq!(adapter.notification_turn, None);
}

#[test]
fn child_terminal_outcomes_survive_idle_and_closure_and_new_turn_restarts_work() {
    for (outcome, expected) in [
        ("failed", ToolStatus::Failed),
        ("interrupted", ToolStatus::Stopped),
        ("completed", ToolStatus::Succeeded),
    ] {
        let mut parent = adapter();
        let mut child = adapter();
        child.thread = Some("child".into());
        child.is_child = true;
        parent.children.insert("child".into(), Box::new(child));
        parent.receive(json!({"method":"turn/started","params":{"threadId":"child","turn":{"id":"t","status":"inProgress","items":[]}}})).unwrap();
        parent.receive(json!({"method":"turn/completed","params":{"threadId":"child","turn":{"id":"t","status":outcome,"items":[]}}})).unwrap();
        for frame in [
            json!({"method":"thread/status/changed","params":{"threadId":"child","status":{"type":"idle"}}}),
            json!({"method":"thread/closed","params":{"threadId":"child"}}),
        ] {
            let observed = parent.receive(frame).unwrap();
            assert!(observed.events.iter().any(|event| matches!(event, Event::Subagent {status:Some(status), ..} if *status == expected)), "{:?}", observed.events);
            // A finished child's thread closing is routine, not a notice.
            assert!(
                !observed
                    .events
                    .iter()
                    .any(|event| matches!(event, Event::SubagentItem { .. })),
                "{:?}",
                observed.events
            );
        }
        let next = parent.receive(json!({"method":"turn/started","params":{"threadId":"child","turn":{"id":"next","items":[]}}})).unwrap();
        assert!(next.events.iter().any(|event| matches!(
            event,
            Event::Subagent {
                status: Some(ToolStatus::Running),
                ..
            }
        )));
    }
    let mut parent = adapter();
    let mut child = adapter();
    child.thread = Some("child".into());
    child.is_child = true;
    child.subagent_status = Some(ToolStatus::Running);
    parent.children.insert("child".into(), Box::new(child));
    let closed = parent
        .receive(json!({"method":"thread/closed","params":{"threadId":"child"}}))
        .unwrap();
    assert!(closed.events.iter().any(|event| matches!(
        event,
        Event::Subagent {
            status: Some(ToolStatus::Stopped),
            ..
        }
    )));
}

/// A child is a thread of its own: stopping it interrupts its turn alone,
/// and cancelling the parent's turn names only the parent's.
#[test]
fn stopping_a_child_interrupts_only_its_own_turn() {
    let mut parent = adapter();
    parent.thread = Some("parent".into());
    parent.turn = Some("parent-turn".into());
    parent.ready = true;
    for (id, turn) in [
        ("busy-child", Some("child-turn")),
        ("sibling", Some("sibling-turn")),
        ("idle-child", None),
    ] {
        let mut child = adapter();
        child.thread = Some(id.into());
        child.is_child = true;
        child.turn = turn.map(str::to_owned);
        parent.children.insert(id.into(), Box::new(child));
    }
    assert!(parent.capabilities().stop_subagent);
    assert!(!parent.capabilities().interrupt_stops_background);
    let stop = parent
        .command(Command::StopSubagent {
            id: "busy-child".into(),
        })
        .unwrap();
    assert_eq!(stop.writes.len(), 1);
    assert_eq!(stop.writes[0]["method"], "turn/interrupt");
    assert_eq!(
        stop.writes[0]["params"],
        json!({"threadId":"busy-child","turnId":"child-turn"})
    );
    let refused = parent
        .receive(json!({"id":stop.writes[0]["id"],"error":{"code":-32600,"message":"no active turn to interrupt"}}))
        .unwrap();
    assert!(refused.events.iter().any(|event| matches!(
        event,
        Event::Error { message, fatal: false } if message.starts_with("Codex could not stop the subagent")
    )));
    assert!(
        parent
            .command(Command::StopSubagent {
                id: "idle-child".into()
            })
            .is_err()
    );
    assert!(
        parent
            .command(Command::StopSubagent {
                id: "unknown".into()
            })
            .is_err()
    );
    let stop = parent
        .command(Command::StopSubagent {
            id: "busy-child".into(),
        })
        .unwrap();
    let acknowledged = parent
        .receive(json!({"id":stop.writes[0]["id"],"result":{}}))
        .unwrap();
    assert!(acknowledged.events.is_empty());
    assert_eq!(parent.turn.as_deref(), Some("parent-turn"));
    assert_eq!(
        parent.children["sibling"].turn.as_deref(),
        Some("sibling-turn")
    );
    assert_eq!(
        parent.children["busy-child"].turn.as_deref(),
        Some("child-turn")
    );
    let cancel = parent.command(Command::Interrupt).unwrap();
    assert_eq!(
        cancel.writes[0]["params"],
        json!({"threadId":"parent","turnId":"parent-turn"})
    );
}

#[test]
fn native_prompt_late_ack_and_old_completion_cannot_reactivate_or_stop_a_new_turn() {
    for newer in [false, true] {
        let mut adapter = adapter();
        let sent = adapter
            .command(Command::Prompt {
                text: "Prompt".into(),
                attachments: Vec::new(),
                mode: PromptMode::Send,
            })
            .unwrap();
        let start = |id| json!({"method":"turn/started","params":{"threadId":"thread-exact","turn":{"id":id,"status":"inProgress","items":[]}}});
        let complete = json!({"method":"turn/completed","params":{"threadId":"thread-exact","turn":{"id":"old","status":"completed","items":[]}}});
        adapter.receive(start("old")).unwrap();
        adapter.receive(complete.clone()).unwrap();
        if newer {
            adapter.receive(start("new")).unwrap();
        }
        let ack = adapter.receive(json!({"id":sent.writes[0]["id"],"result":{"turn":{"id":"old","status":"inProgress","items":[]}}})).unwrap();
        assert_eq!(adapter.busy, newer);
        assert_eq!(adapter.turn.as_deref(), newer.then_some("new"));
        assert!(
            !ack.events
                .iter()
                .any(|event| matches!(event, Event::Status { busy: true, .. }))
        );
        if newer {
            adapter.receive(json!({"method":"item/started","params":{"threadId":"thread-exact","turnId":"new","item":{"id":"cmd","type":"commandExecution","command":"echo hi","cwd":"/tmp","status":"inProgress","commandActions":[]}}})).unwrap();
            let old = adapter.receive(complete).unwrap();
            assert!(
                !old.events
                    .iter()
                    .any(|event| matches!(event, Event::TurnSettled))
            );
            assert!(adapter.busy);
            assert_eq!(adapter.turn.as_deref(), Some("new"));
            assert!(!adapter.active_items.is_empty());
            let current = adapter.receive(json!({"method":"turn/completed","params":{"threadId":"thread-exact","turn":{"id":"new","status":"completed","items":[]}}})).unwrap();
            assert!(
                current
                    .events
                    .iter()
                    .any(|event| matches!(event, Event::TurnSettled))
            );
        }
    }
}

#[test]
fn child_hydration_buffers_live_output_and_routes_unique_rpc_ids() {
    let mut adapter = adapter();
    let mut discovered = Step::default();
    adapter
        .discover_child("child-exact", &json!({"status":"running"}), &mut discovered)
        .unwrap();
    assert_eq!(
        discovered.writes[0]["params"]["initialTurnsPage"]["itemsView"],
        "full"
    );
    let live = adapter.receive(json!({"method":"item/agentMessage/delta","params":{"threadId":"child-exact","turnId":"turn","itemId":"answer","delta":" tail"}})).unwrap();
    assert!(live.events.is_empty());
    let hydrated = adapter.receive(json!({"id":discovered.writes[0]["id"],"result":{"thread":{"id":"child-exact","status":{"type":"active"}},"initialTurnsPage":{"data":[{"id":"turn","status":"inProgress","items":[{"id":"answer","type":"agentMessage","text":"head"}]}],"nextCursor":null}}})).unwrap();
    assert_eq!(
        adapter.children["child-exact"].notification_turn.as_deref(),
        Some("turn")
    );
    assert!(hydrated.events.iter().any(|event| matches!(event, Event::SubagentTextDelta { id, delta, .. } if id == "child-exact" && delta == " tail")));
    assert!(
        !hydrated
            .events
            .iter()
            .any(|event| matches!(event, Event::Status { .. }))
    );
    let invalidated = adapter
        .receive(json!({"method":"skills/changed","params":{"threadId":"child-exact"}}))
        .unwrap();
    assert_eq!(invalidated.writes.len(), 1);
    assert_ne!(invalidated.writes[0]["id"], discovered.writes[0]["id"]);
    let replied = adapter
        .receive(json!({"id":invalidated.writes[0]["id"],"result":{"data":[]}}))
        .unwrap();
    assert!(replied.events.is_empty());
    let stale = adapter.receive(json!({"method":"turn/completed","params":{"threadId":"child-exact","turn":{"id":"older","status":"failed","items":[]}}})).unwrap();
    assert!(adapter.children["child-exact"].busy);
    assert!(!stale.events.iter().any(|event| matches!(
        event,
        Event::Subagent {
            status: Some(ToolStatus::Failed),
            ..
        }
    )));
    let completed = adapter.receive(json!({"method":"turn/completed","params":{"threadId":"child-exact","turn":{"id":"turn","status":"completed","items":[]}}})).unwrap();
    assert!(completed.events.iter().any(|event| matches!(event, Event::Subagent { id, status:Some(ToolStatus::Succeeded), .. } if id == "child-exact")));
    let foreign = adapter.receive(json!({"method":"turn/started","params":{"threadId":"unrelated","turn":{"id":"bad","items":[]}}})).unwrap();
    assert!(foreign.events.is_empty());
}

#[test]
fn a_queue_removal_is_answered_as_removed_or_refused() {
    for (answer, removed) in [
        (json!({"result":{}}), true),
        (
            json!({"error":{"code":-32000,"message":"already started"}}),
            false,
        ),
    ] {
        let mut adapter = adapter();
        let request = adapter
            .command(Command::RemoveQueued { id: "q1".into() })
            .unwrap();
        let mut frame = answer;
        frame["id"] = request.writes[0]["id"].clone();
        let step = adapter.receive(frame).unwrap();
        assert!(step.events.iter().any(|event| matches!(
            event,
            Event::QueueRemoval(removal) if removal.id == "q1" && removal.removed == removed
        )));
        // Either way the listing is read again rather than assumed.
        assert!(
            step.writes
                .iter()
                .any(|write| write["method"] == "thread/queue/list")
        );
    }
}

#[test]
fn queue_mutations_and_forks_use_exact_provider_identifiers() {
    let mut adapter = adapter();
    for (command, method) in [
        (
            Command::RemoveQueued {
                id: "queue-exact".into(),
            },
            "thread/queue/delete",
        ),
        (
            Command::StartQueued {
                id: "queue-exact".into(),
            },
            "thread/queue/start",
        ),
    ] {
        let request = adapter.command(command).unwrap();
        assert_eq!(request.writes[0]["method"], method);
        assert_eq!(request.writes[0]["params"]["threadId"], "thread-exact");
        assert_eq!(
            request.writes[0]["params"]["queuedSubmissionId"],
            "queue-exact"
        );
    }
    adapter.branch_points.push(chat::BranchPoint {
        id: "completed-turn".into(),
        label: "Prompt".into(),
    });
    assert!(
        adapter
            .command(Command::Branch {
                id: Some("unknown-turn".into())
            })
            .is_err()
    );
    adapter.busy = true;
    assert!(adapter.command(Command::Branch { id: None }).is_err());
    adapter.busy = false;
    let fork = adapter
        .command(Command::Branch {
            id: Some("completed-turn".into()),
        })
        .unwrap();
    assert_eq!(fork.writes[0]["params"]["lastTurnId"], "completed-turn");
    let fork_id = "f0e49307-af96-4edd-b23f-60b185fe4fea";
    let response = adapter
        .receive(json!({"id":fork.writes[0]["id"],"result":{"thread":{"id":fork_id}}}))
        .unwrap();
    assert!(response.events.contains(&Event::Forked(fork_id.into())));
    assert_eq!(adapter.thread.as_deref(), Some("thread-exact"));
    assert_eq!(response.writes[0]["method"], "thread/unsubscribe");
}

#[test]
fn child_listing_recovers_newest_first_descendants_without_trusting_foreign_threads() {
    let mut adapter = adapter();
    let mut request = Step::default();
    adapter
        .call(&mut request, "thread/list", json!({}), Pending::ChildList)
        .unwrap();
    adapter
        .receive(json!({"id":request.writes[0]["id"],"result":{"data":[
            {"id":"grandchild","parentThreadId":"child","status":{"type":"idle"}},
            {"id":"foreign","parentThreadId":"another-root"},
            {"id":"child","parentThreadId":"thread-exact","status":{"type":"idle"}}
        ]}}))
        .unwrap();
    assert!(adapter.children.contains_key("child"));
    assert!(adapter.children.contains_key("grandchild"));
    assert!(!adapter.children.contains_key("foreign"));
}

#[test]
fn child_history_recovers_failed_and_interrupted_outcomes_without_rewinding_live_work() {
    for (outcome, expected) in [
        ("failed", ToolStatus::Failed),
        ("interrupted", ToolStatus::Stopped),
        ("completed", ToolStatus::Succeeded),
    ] {
        let mut parent = adapter();
        let mut discovered = Step::default();
        parent
            .discover_child("child", &json!({"status":"running"}), &mut discovered)
            .unwrap();
        let restored = parent.receive(json!({"id":discovered.writes[0]["id"],"result":{"thread":{"id":"child","status":{"type":"idle"}},"initialTurnsPage":{"data":[{"id":"latest","status":outcome,"items":[]},{"id":"older","status":"completed","items":[]}],"nextCursor":null}}})).unwrap();
        assert_eq!(parent.children["child"].subagent_status, Some(expected));
        assert!(restored.events.iter().any(|event| matches!(event, Event::Subagent { id, status:Some(status), .. } if id == "child" && *status == expected)));
        let child = parent.children.get_mut("child").unwrap();
        let mut page = Step::default();
        child.page(&json!({"data":[{"id":"oldest","status":"completed","items":[]}],"nextCursor":null}), &mut page, true).unwrap();
        assert_eq!(child.subagent_status, Some(expected));
        child.observe_thread_status(&json!({"type":"active"}));
        child
            .page(
                &json!({"data":[{"id":"latest","status":outcome,"items":[]}],"nextCursor":null}),
                &mut page,
                false,
            )
            .unwrap();
        assert_eq!(child.subagent_status, Some(ToolStatus::Running));
        assert!(child.busy);
    }
}

#[test]
fn paging_historical_collaboration_cannot_restart_a_finished_child() {
    let mut parent = adapter();
    let mut child = adapter();
    child.is_child = true;
    child.thread = Some("child".into());
    child.subagent_status = Some(ToolStatus::Failed);
    child.selected = Some("gpt-new".into());
    parent.children.insert("child".into(), Box::new(child));
    let mut step = Step::default();
    parent.page(&json!({"data":[{"id":"older","status":"completed","items":[{"id":"spawn","type":"collabAgentToolCall","tool":"spawnAgent","status":"completed","senderThreadId":"thread-exact","receiverThreadIds":["child"],"agentsStates":{"child":{"status":"running","agentNickname":"Scout","model":"gpt-old"}}}]}],"nextCursor":null}), &mut step, true).unwrap();
    assert_eq!(
        parent.children["child"].subagent_status,
        Some(ToolStatus::Failed)
    );
    assert_eq!(
        parent.children["child"].selected.as_deref(),
        Some("gpt-new")
    );
    assert!(step.events.iter().any(|event| matches!(event, Event::Subagent { id, kind, status:None, model:None, .. } if id == "child" && kind == "Scout")));
    assert!(!step.events.iter().any(|event| matches!(
        event,
        Event::Subagent {
            status: Some(ToolStatus::Running),
            ..
        }
    )));
}

#[test]
fn older_history_cannot_rename_a_child_evicted_after_its_live_name() {
    let mut parent = adapter();
    let mut live = Step::default();
    for index in 0..32 {
        parent
            .discover_child(
                &format!("child-{index:02}"),
                &json!({"agentNickname":format!("Live {index}")}),
                &mut live,
            )
            .unwrap();
    }
    for child in parent.children.values_mut() {
        child.ready = true;
    }
    parent.child_buffer.clear();
    // A crowded conversation evicts a finished child to admit another.
    parent
        .discover_child("newest", &json!({}), &mut live)
        .unwrap();
    assert!(!parent.children.contains_key("child-00"));
    let mut paged = Step::default();
    parent.page(&json!({"data":[{"id":"older","status":"completed","items":[{"id":"spawn","type":"collabAgentToolCall","tool":"spawnAgent","status":"completed","senderThreadId":"thread-exact","receiverThreadIds":["child-00"],"agentsStates":{"child-00":{"agentNickname":"Stale"}}}]}],"nextCursor":null}), &mut paged, true).unwrap();
    assert!(paged.events.iter().any(|event| matches!(event, Event::Subagent { id, kind, .. } if id == "child-00" && kind.is_empty())), "{:?}", paged.events);
    assert!(
        !paged
            .events
            .iter()
            .any(|event| matches!(event, Event::Subagent { kind, .. } if kind == "Stale"))
    );
}

#[test]
fn metadata_refresh_preserves_the_model_selected_for_the_next_prompt() {
    let mut adapter = adapter();
    adapter.models.push(Model {
        id: "gpt-next".into(),
        label: "Next model".into(),
        reasoning: Vec::new(),
    });
    adapter
        .command(Command::SetModel {
            model: "gpt-next".into(),
            reasoning: None,
        })
        .unwrap();
    adapter.receive(json!({"method":"thread/started","params":{"thread":{"id":"thread-exact","model":"gpt-prior","canAcceptDirectInput":true,"status":{"type":"idle"}}}})).unwrap();
    assert_eq!(adapter.selected.as_deref(), Some("gpt-next"));
    let prompt = adapter
        .command(Command::Prompt {
            text: "Next prompt".into(),
            attachments: Vec::new(),
            mode: PromptMode::Send,
        })
        .unwrap();
    assert_eq!(prompt.writes[0]["method"], "turn/start");
    assert_eq!(prompt.writes[0]["params"]["model"], "gpt-next");
}

#[test]
fn child_model_and_waiting_state_follow_its_own_thread_lifecycle() {
    let mut parent = adapter();
    let mut discovered = Step::default();
    parent
        .discover_child("child", &json!({}), &mut discovered)
        .unwrap();
    assert!(
        discovered
            .events
            .iter()
            .any(|event| matches!(event, Event::Subagent { model: None, .. }))
    );
    let attached = parent.receive(json!({"id":discovered.writes[0]["id"],"result":{"model":"gpt-child","thread":{"id":"child","status":{"type":"active","activeFlags":["waitingOnApproval"]}},"initialTurnsPage":{"data":[],"nextCursor":null}}})).unwrap();
    assert!(attached.events.iter().any(|event| matches!(event, Event::Subagent { model:Some(model), waiting:Some(true), detail:Some(detail), .. } if model == "gpt-child" && detail == "Waiting for your approval")));
    parent.receive(json!({"method":"turn/started","params":{"threadId":"child","turn":{"id":"latest","status":"inProgress","items":[]}}})).unwrap();
    let answered = parent.receive(json!({"method":"thread/status/changed","params":{"threadId":"child","status":{"type":"active","activeFlags":[]}}})).unwrap();
    assert!(answered.events.iter().any(|event| matches!(event, Event::Subagent { model:Some(model), waiting:Some(false), detail:Some(detail), status:Some(ToolStatus::Running), .. } if model == "gpt-child" && detail.is_empty())));
    parent.receive(json!({"method":"thread/status/changed","params":{"threadId":"child","status":{"type":"active","activeFlags":["waitingOnUserInput"]}}})).unwrap();
    let finished = parent.receive(json!({"method":"turn/completed","params":{"threadId":"child","turn":{"id":"latest","status":"completed","items":[]}}})).unwrap();
    assert!(finished.events.iter().any(|event| matches!(
        event,
        Event::Subagent {
            waiting: Some(false),
            status: Some(ToolStatus::Succeeded),
            ..
        }
    )));
    assert_eq!(parent.selected.as_deref(), Some("model"));
}

#[test]
fn child_legacy_embedded_history_retains_the_latest_turn_outcome() {
    let mut parent = adapter();
    let mut discovered = Step::default();
    parent
        .discover_child("child", &json!({}), &mut discovered)
        .unwrap();
    parent.receive(json!({"id":discovered.writes[0]["id"],"result":{"thread":{"id":"child","status":{"type":"idle"},"turns":[{"id":"older","status":"completed","items":[]},{"id":"latest","status":"failed","items":[]}]}}})).unwrap();
    assert_eq!(
        parent.children["child"].subagent_status,
        Some(ToolStatus::Failed)
    );
}

#[test]
fn child_resume_without_embedded_turns_reads_history_and_settles_idle() {
    let mut adapter = adapter();
    let mut discovered = Step::default();
    adapter
        .discover_child("child-exact", &json!({"status":"running"}), &mut discovered)
        .unwrap();
    adapter.receive(json!({"method":"item/agentMessage/delta","params":{"threadId":"child-exact","turnId":"turn","itemId":"answer","delta":" tail"}})).unwrap();
    let resumed = adapter.receive(json!({"id":discovered.writes[0]["id"],"result":{"thread":{"id":"child-exact","status":{"type":"idle"},"turns":[]}}})).unwrap();
    assert!(resumed.events.iter().any(|event| matches!(event, Event::Subagent { id, status:Some(ToolStatus::Succeeded), .. } if id == "child-exact")));
    assert_eq!(resumed.writes[0]["method"], "thread/turns/list");
    assert!(adapter.child_buffer.contains_key("child-exact"));
    let history = adapter.receive(json!({"id":resumed.writes[0]["id"],"result":{"data":[{"id":"turn","status":"completed","items":[{"id":"answer","type":"agentMessage","text":"head"}]}],"nextCursor":null}})).unwrap();
    assert!(
        history
            .events
            .iter()
            .any(|event| matches!(event, Event::SubagentItem { id, .. } if id == "child-exact"))
    );
    assert!(!adapter.child_buffer.contains_key("child-exact"));
}

#[test]
fn child_approvals_resolve_in_parent_and_failed_turns_keep_their_outcome() {
    let mut adapter = adapter();
    let mut discovered = Step::default();
    adapter
        .discover_child("child-exact", &json!({"status":"running"}), &mut discovered)
        .unwrap();
    adapter.receive(json!({"id":discovered.writes[0]["id"],"result":{"thread":{"id":"child-exact","status":{"type":"active"}},"initialTurnsPage":{"data":[],"nextCursor":null}}})).unwrap();
    adapter.receive(json!({"method":"turn/started","params":{"threadId":"child-exact","turn":{"id":"turn","status":"inProgress","items":[]}}})).unwrap();
    let approval = |id| json!({"id":id,"method":"item/commandExecution/requestApproval","params":{"threadId":"child-exact","turnId":"turn","itemId":"tool","availableDecisions":["accept","decline"],"command":"echo requested"}});
    let requested = adapter.receive(approval(71)).unwrap();
    assert!(requested.events.iter().any(
        |event| matches!(event, Event::Request(request) if request.title.starts_with("Child agent"))
    ));
    let resolved = adapter.receive(json!({"method":"serverRequest/resolved","params":{"threadId":"child-exact","requestId":71}})).unwrap();
    assert!(
        resolved
            .events
            .iter()
            .any(|event| matches!(event, Event::RequestResolved { id } if id == "71"))
    );
    assert!(adapter.requests.is_empty());
    let failed = adapter.receive(json!({"method":"turn/completed","params":{"threadId":"child-exact","turn":{"id":"turn","status":"failed","error":{"message":"fixture"},"items":[]}}})).unwrap();
    assert!(failed.events.iter().any(|event| matches!(event, Event::Subagent { id, status:Some(ToolStatus::Failed), .. } if id == "child-exact")));
    adapter.receive(approval(72)).unwrap();
    let closed = adapter
        .receive(json!({"method":"thread/closed","params":{"threadId":"child-exact"}}))
        .unwrap();
    assert!(
        closed
            .events
            .iter()
            .any(|event| matches!(event, Event::RequestResolved { id } if id == "72"))
    );
    assert!(adapter.requests.is_empty());
}

#[test]
fn permission_picker_updates_both_codex_policies() {
    let mut adapter = adapter();
    assert_eq!(
        adapter.capabilities().permission_modes,
        ["readOnly", "workspaceWrite", "yolo"]
    );
    for (mode, approval, sandbox) in [
        ("yolo", "never", "dangerFullAccess"),
        ("workspaceWrite", "on-request", "workspaceWrite"),
        ("readOnly", "on-request", "readOnly"),
    ] {
        let step = adapter
            .command(Command::SetPermissionMode { mode: mode.into() })
            .unwrap();
        assert_eq!(step.writes[0]["method"], "thread/settings/update");
        assert_eq!(step.writes[0]["params"]["approvalPolicy"], approval);
        assert_eq!(step.writes[0]["params"]["sandboxPolicy"]["type"], sandbox);
        let acknowledged = adapter
            .receive(json!({"id":step.writes[0]["id"], "result":{}}))
            .unwrap();
        assert!(
            !acknowledged
                .events
                .iter()
                .any(|event| matches!(event, Event::PermissionMode(_)))
        );
        let confirmed = adapter
            .receive(json!({"method":"thread/settings/updated","params":{
                "threadId":"thread-exact",
                "threadSettings": {"approvalPolicy":approval,"sandboxPolicy":{"type":sandbox}}
            }}))
            .unwrap();
        assert!(
            confirmed
                .events
                .contains(&Event::PermissionMode(Some(mode.into())))
        );
    }
    let custom = adapter.receive(json!({"method":"thread/settings/updated","params":{
        "threadId":"thread-exact",
        "threadSettings": {"approvalPolicy":"on-failure","sandboxPolicy":{"type":"workspaceWrite"}}
    }})).unwrap();
    assert!(custom.events.contains(&Event::PermissionMode(None)));
    adapter.busy = true;
    assert!(
        adapter
            .command(Command::SetPermissionMode {
                mode: "yolo".into()
            })
            .is_err()
    );
}

#[test]
fn accepted_run_waits_for_its_exact_turn_even_when_completion_precedes_ack() {
    use crate::agent_execution::ProviderEvent;
    let mut adapter = adapter();
    let run = crate::model::RunId::new();
    let prompt = adapter
        .command_for_run(
            run,
            Command::Prompt {
                text: "owned prompt".into(),
                attachments: Vec::new(),
                mode: PromptMode::Send,
            },
        )
        .unwrap();
    let request = prompt
        .writes
        .iter()
        .find(|frame| frame["method"] == "turn/start")
        .unwrap()["id"]
        .clone();
    let complete = |id: &str, answer: &str| {
        json!({"method":"turn/completed","params":{
            "threadId":"thread-exact","turn":{"id":id,"status":"completed","items":[
                {"id":format!("{id}-answer"),"type":"agentMessage","text":answer}
            ]}
        }})
    };
    for frame in [
        complete("another-client", "Unrelated answer"),
        complete("owned", "Owned answer"),
    ] {
        let observed = adapter.receive(frame).unwrap();
        assert!(
            !observed
                .events
                .iter()
                .any(|event| matches!(event, Event::Execution { .. }))
        );
    }
    let acknowledged = adapter
        .receive(json!({"id":request,"result":{"turn":{"id":"owned"}}}))
        .unwrap();
    assert!(acknowledged.events.iter().any(|event| matches!(event,
        Event::Execution { run: actual, event: ProviderEvent::Finished { outcome: crate::notifications::Kind::Succeeded } } if *actual == run)));
    let texts = acknowledged
        .events
        .iter()
        .filter_map(|event| match event {
            Event::Item(Item {
                role: Role::Assistant,
                content: ItemContent::Text { text, .. },
                ..
            }) => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(texts, ["Owned answer"]);
    assert!(!adapter.busy);
    assert!(adapter.execution.is_none());
}

#[test]
fn initial_scalar_activation_before_early_completion_keeps_exact_ack_authority() {
    for managed in [false, true] {
        let mut adapter = adapter();
        let run = crate::model::RunId::new();
        let command = Command::Prompt {
            text: "Owned prompt".into(),
            attachments: vec![],
            mode: PromptMode::Send,
        };
        let submitted = if managed {
            adapter.command_for_run(run, command).unwrap()
        } else {
            adapter.command(command).unwrap()
        };
        adapter.receive(json!({"method":"thread/status/changed","params":{"threadId":"thread-exact","status":{"type":"active"}}})).unwrap();
        adapter.receive(json!({"method":"turn/completed","params":{"threadId":"thread-exact","turn":{"id":"owned","status":"completed","items":[{"id":"answer","type":"agentMessage","text":"Owned answer"}]}}})).unwrap();
        adapter.receive(json!({"method":"thread/status/changed","params":{"threadId":"thread-exact","status":{"type":"idle"}}})).unwrap();
        let acknowledged = adapter
            .receive(json!({"id":submitted.writes[0]["id"],"result":{"turn":{"id":"owned"}}}))
            .unwrap();
        assert!(!acknowledged.suppress_current_notification);
        assert!(acknowledged.events.iter().any(|event| matches!(event,
            Event::TurnFinished { key, outcome: crate::notifications::Kind::Succeeded } if key.as_deref() == Some("codex:thread-exact:owned")
        )));
        assert!(!adapter.busy);
        assert_eq!(acknowledged.events.iter().filter(|event| matches!(event,
            Event::Execution { run: actual, event: crate::agent_execution::ProviderEvent::Finished { outcome: crate::notifications::Kind::Succeeded } } if *actual == run
        )).count(), usize::from(managed));
    }
}

#[test]
fn superseded_early_completion_cannot_reclaim_notification_authority_after_idle() {
    for supersession in ["active", "metadata-active", "systemError", "closed"] {
        for managed in [false, true] {
            let mut adapter = adapter();
            let run = crate::model::RunId::new();
            let command = Command::Prompt {
                text: "Old prompt".into(),
                attachments: vec![],
                mode: PromptMode::Send,
            };
            let submitted = if managed {
                adapter.command_for_run(run, command).unwrap()
            } else {
                adapter.command(command).unwrap()
            };
            let request = submitted.writes[0]["id"].clone();
            adapter.receive(json!({"method":"turn/completed","params":{"threadId":"thread-exact","turn":{"id":"old","status":"completed","items":[{"id":"old-answer","type":"agentMessage","text":"Old answer"}]}}})).unwrap();
            if supersession == "closed" {
                adapter
                    .receive(json!({"method":"thread/closed","params":{"threadId":"thread-exact"}}))
                    .unwrap();
            } else if supersession == "metadata-active" {
                let refresh = adapter.command(Command::Refresh).unwrap();
                let read = refresh
                    .writes
                    .iter()
                    .find(|frame| frame["method"] == "thread/read")
                    .unwrap();
                adapter.receive(json!({"id":read["id"],"result":{"thread":{"id":"thread-exact","status":{"type":"active"}}}})).unwrap();
            } else {
                adapter.receive(json!({"method":"thread/status/changed","params":{"threadId":"thread-exact","status":{"type":supersession}}})).unwrap();
            }
            if matches!(supersession, "active" | "metadata-active") {
                adapter.receive(json!({"method":"thread/status/changed","params":{"threadId":"thread-exact","status":{"type":"idle"}}})).unwrap();
            }
            let revision = adapter.lifecycle_revision;
            let acknowledged = adapter
                .receive(json!({"id":request,"result":{"turn":{"id":"old"}}}))
                .unwrap();
            assert!(
                acknowledged.suppress_current_notification,
                "{supersession}, managed={managed}"
            );
            assert!(
                acknowledged.notification_preview.is_none(),
                "{supersession}, managed={managed}"
            );
            assert!(
                !acknowledged.events.iter().any(|event| matches!(
                    event,
                    Event::TurnFinished { .. } | Event::TurnStarted | Event::TurnSettled
                )),
                "{supersession}, managed={managed}: {:?}",
                acknowledged.events
            );
            assert_eq!(adapter.lifecycle_revision, revision);
            assert!(adapter.notification_turn.is_none());
            assert!(!adapter.busy);
            assert_eq!(acknowledged.events.iter().filter(|event| matches!(event,
                Event::Execution { run: actual, event: crate::agent_execution::ProviderEvent::Finished { outcome: crate::notifications::Kind::Succeeded } } if *actual == run
            )).count(), usize::from(managed));
            if managed {
                assert!(acknowledged.events.iter().any(|event| matches!(event,
                    Event::Item(Item { content: ItemContent::Text { text, .. }, .. }) if text == "Old answer"
                )));
            }
        }
    }
}

#[test]
fn managed_completion_publishes_final_content_once_per_step_in_either_ack_order() {
    for completion_first in [false, true] {
        let mut adapter = adapter();
        let run = crate::model::RunId::new();
        let sent = adapter
            .command_for_run(
                run,
                Command::Prompt {
                    text: "Owned prompt".into(),
                    attachments: vec![],
                    mode: PromptMode::Send,
                },
            )
            .unwrap();
        let request = sent.writes[0]["id"].clone();
        let ack = json!({"id":request,"result":{"turn":{"id":"owned"}}});
        let completion = json!({"method":"turn/completed","params":{"threadId":"thread-exact","turn":{"id":"owned","status":"failed","error":{"message":"Exact failure"},"items":[{"id":"answer","type":"agentMessage","text":"Partial answer"}]}}});
        let final_step = if completion_first {
            adapter.receive(completion).unwrap();
            adapter.receive(ack).unwrap()
        } else {
            adapter.receive(ack).unwrap();
            adapter.receive(completion).unwrap()
        };
        assert_eq!(final_step.events.iter().filter(|event| matches!(event,
            Event::Item(Item { content: ItemContent::Text { text, .. }, .. }) if text == "Partial answer"
        )).count(), 1, "{:?}", final_step.events);
        assert_eq!(
            final_step
                .events
                .iter()
                .filter(|event| matches!(event,
                    Event::Error { message, .. } if message == "Exact failure"
                ))
                .count(),
            1,
            "{:?}",
            final_step.events
        );
        assert_eq!(final_step.events.iter().filter(|event| matches!(event,
            Event::Execution { run: actual, event: crate::agent_execution::ProviderEvent::Finished { outcome: crate::notifications::Kind::Failed } } if *actual == run
        )).count(), 1, "{:?}", final_step.events);
        assert!(adapter.execution.is_none());
        assert!(!adapter.busy);
    }
}

#[test]
fn managed_late_ack_settles_its_run_without_claiming_new_unidentified_work() {
    let mut adapter = adapter();
    let run = crate::model::RunId::new();
    let sent = adapter
        .command_for_run(
            run,
            Command::Prompt {
                text: "Owned prompt".into(),
                attachments: vec![],
                mode: PromptMode::Send,
            },
        )
        .unwrap();
    adapter.receive(json!({"method":"turn/completed","params":{"threadId":"thread-exact","turn":{"id":"owned","status":"completed","items":[{"id":"answer","type":"agentMessage","text":"Owned answer"}]}}})).unwrap();
    adapter.receive(json!({"method":"thread/status/changed","params":{"threadId":"thread-exact","status":{"type":"active","activeFlags":["waitingOnApproval"]}}})).unwrap();
    let revision = adapter.lifecycle_revision;
    let ack = adapter
        .receive(json!({"id":sent.writes[0]["id"],"result":{"turn":{"id":"owned"}}}))
        .unwrap();
    assert!(ack.events.iter().any(|event| matches!(event,
        Event::Execution { run: actual, event: crate::agent_execution::ProviderEvent::Finished { outcome: crate::notifications::Kind::Succeeded } } if *actual == run
    )));
    assert!(ack.suppress_current_notification);
    assert!(ack.notification_preview.is_none());
    assert!(!ack.events.iter().any(|event| matches!(
        event,
        Event::TurnFinished { .. } | Event::Status { busy: false, .. }
    )));
    assert!(adapter.execution.is_none());
    assert!(adapter.busy && adapter.subagent_waiting);
    assert_eq!(adapter.lifecycle_revision, revision);
    assert!(adapter.turn.is_none() && adapter.notification_turn.is_none());
    adapter.receive(json!({"method":"turn/started","params":{"threadId":"thread-exact","turn":{"id":"new","status":"inProgress","items":[]}}})).unwrap();
    let finished = adapter.receive(json!({"method":"turn/completed","params":{"threadId":"thread-exact","turn":{"id":"new","status":"completed","items":[]}}})).unwrap();
    assert!(!adapter.busy);
    assert!(finished.events.iter().any(|event| matches!(
        event,
        Event::TurnFinished {
            outcome: crate::notifications::Kind::Succeeded,
            ..
        }
    )));
}

#[test]
fn child_buffered_new_turn_supersedes_its_initial_active_history_binding() {
    let mut parent = adapter();
    let mut discovered = Step::default();
    parent
        .discover_child("child", &json!({}), &mut discovered)
        .unwrap();
    parent.receive(json!({"method":"turn/started","params":{"threadId":"child","turn":{"id":"new","status":"inProgress","items":[]}}})).unwrap();
    parent.receive(json!({"id":discovered.writes[0]["id"],"result":{"thread":{"id":"child","status":{"type":"active"}},"initialTurnsPage":{"data":[{"id":"old","status":"inProgress","items":[]}],"nextCursor":null}}})).unwrap();
    assert_eq!(
        parent.children["child"].notification_turn.as_deref(),
        Some("new")
    );
    parent.receive(json!({"method":"turn/completed","params":{"threadId":"child","turn":{"id":"old","status":"failed","items":[]}}})).unwrap();
    assert!(parent.children["child"].busy);
    let completed = parent.receive(json!({"method":"turn/completed","params":{"threadId":"child","turn":{"id":"new","status":"completed","items":[]}}})).unwrap();
    assert!(!parent.children["child"].busy);
    assert!(completed.events.iter().any(|event| matches!(event, Event::Subagent { id, status: Some(ToolStatus::Succeeded), .. } if id == "child")));
}

fn account_fixture(name: &str) -> Value {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("fixtures/accounts")
        .join(name);
    serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap()
}

/// Account notifications name no thread and are forwarded as account
/// reports rather than dropped with the other auxiliary notifications. A
/// sparse rate-limit update is forwarded as what it names.
#[test]
fn account_notifications_are_forwarded_as_reports() {
    let mut adapter = adapter();
    let limits = account_fixture("codex-rate-limits-updated.json");
    let expected = account::codex::parse_update(&limits["params"]).unwrap();
    assert_eq!(expected.windows.len(), 1, "the null window is not cleared");
    assert_eq!(
        adapter.receive(limits).unwrap().events,
        [Event::AccountUsage(LiveReport::CodexLimits(expected))]
    );
    assert_eq!(
        adapter
            .receive(account_fixture("codex-account-updated.json"))
            .unwrap()
            .events,
        [Event::AccountUsage(LiveReport::CodexLogin(
            account::codex::LoginUpdate {
                chatgpt: true,
                plan: Some("prolite".into()),
            }
        ))]
    );
    let empty = json!({"method": "account/rateLimits/updated", "params": {"rateLimits": null}});
    assert!(adapter.receive(empty).unwrap().events.is_empty());
}

#[test]
fn joins_exact_thread_without_settings_or_history_overrides() {
    let directory = tempfile::tempdir().unwrap();
    let socket = directory.path().join("shared.sock");
    let _server = std::os::unix::net::UnixListener::bind(&socket).unwrap();
    let launch = Launch {
        provider: crate::agent::Provider::Codex,
        session_id: Some("thread-exact".into()),
        recipe: crate::model::LaunchRecipe {
            unset_environment: Default::default(),
            program: "codex".into(),
            arguments: [
                "resume",
                "thread-exact",
                "--remote",
                &format!("unix://{}", socket.display()),
                "--profile",
                "work",
                "--config",
                "model=\"other-model\"",
                "--model",
                "other-model",
                "--search",
                "--dangerously-bypass-approvals-and-sandbox",
            ]
            .into_iter()
            .map(str::to_owned)
            .collect(),
            working_directory: directory.path().into(),
            environment: BTreeMap::from([(
                "CODEX_HOME".into(),
                directory.path().to_str().unwrap().into(),
            )]),
        },
    };
    let (spec, mut adapter) = create(&launch).unwrap();
    assert!(matches!(
        spec.transport,
        TransportSpec::UnixWebSocket { .. }
    ));
    let initialize = adapter.initialize().unwrap();
    let step = adapter.receive(json!({"id":initialize.writes[0]["id"],"result":{"userAgent":"codex","codexHome":directory.path()}})).unwrap();
    let resume = step
        .writes
        .iter()
        .find(|frame| frame["method"] == "thread/resume")
        .unwrap();
    assert_eq!(
        resume["params"],
        json!({"threadId":"thread-exact","excludeTurns":true,"initialTurnsPage":{"limit":20,"sortDirection":"desc","itemsView":"full"}})
    );
    let wrong =
        adapter.receive(json!({"id":resume["id"],"result":{"thread":{"id":"tree-id-not-thread"}}}));
    assert!(wrong.is_err());
}

#[test]
fn a_new_thread_carries_the_terminal_bypass_into_its_own_settings() {
    let directory = tempfile::tempdir().unwrap();
    let launch = Launch {
        provider: crate::agent::Provider::Codex,
        session_id: None,
        recipe: crate::model::LaunchRecipe {
            unset_environment: Default::default(),
            program: "codex".into(),
            arguments: ["--dangerously-bypass-approvals-and-sandbox"]
                .into_iter()
                .map(str::to_owned)
                .collect(),
            working_directory: directory.path().into(),
            environment: BTreeMap::from([(
                "CODEX_HOME".into(),
                directory.path().to_str().unwrap().into(),
            )]),
        },
    };
    let (spec, mut adapter) = create(&launch).unwrap();
    assert_eq!(spec.arguments, vec!["app-server"]);
    let initialize = adapter.initialize().unwrap();
    let step = adapter.receive(json!({"id":initialize.writes[0]["id"],"result":{"userAgent":"codex","codexHome":directory.path()}})).unwrap();
    let start = step
        .writes
        .iter()
        .find(|frame| frame["method"] == "thread/start")
        .unwrap();
    assert_eq!(start["params"]["sandbox"], json!("danger-full-access"));
    assert_eq!(start["params"]["approvalPolicy"], json!("never"));
}

#[test]
fn a_new_thread_takes_its_reasoning_effort_as_a_start_override() {
    let directory = tempfile::tempdir().unwrap();
    let launch = |arguments: &[&str]| Launch {
        provider: crate::agent::Provider::Codex,
        session_id: None,
        recipe: crate::model::LaunchRecipe {
            unset_environment: Default::default(),
            program: "codex".into(),
            arguments: arguments.iter().map(|&argument| argument.into()).collect(),
            working_directory: directory.path().into(),
            environment: BTreeMap::from([(
                "CODEX_HOME".into(),
                directory.path().to_str().unwrap().into(),
            )]),
        },
    };
    for (arguments, effort) in [
        (&["--config", "model_reasoning_effort=low"][..], "low"),
        (&["-c", "model_reasoning_effort=\"xhigh\""][..], "xhigh"),
    ] {
        let (spec, mut adapter) = create(&launch(arguments)).unwrap();
        // The override is a thread setting, never a server-wide flag.
        assert_eq!(spec.arguments, vec!["app-server"]);
        let initialize = adapter.initialize().unwrap();
        let step = adapter.receive(json!({"id":initialize.writes[0]["id"],"result":{"userAgent":"codex","codexHome":directory.path()}})).unwrap();
        let start = step
            .writes
            .iter()
            .find(|frame| frame["method"] == "thread/start")
            .unwrap();
        assert_eq!(
            start["params"]["config"],
            json!({REASONING_EFFORT_KEY: effort})
        );
    }
    for arguments in [
        &["--config", "model=\"other\""][..],
        &["-c", "model_reasoning_effort=--yolo"][..],
        &["-c", "model_reasoning_effort_extra=low"][..],
        &["--config"][..],
    ] {
        assert!(create(&launch(arguments)).is_err(), "{arguments:?}");
    }
}

/// The completion list is bounded whatever the account's skill tree looks
/// like. A catalog arrives grouped by directory, so a cap that stopped only
/// the walk within one group grew by one skill per directory.
#[test]
fn a_catalog_split_across_directories_still_stops_at_the_cap() {
    let mut adapter = adapter();
    let initialize = adapter.initialize().unwrap();
    let setup = adapter
        .receive(json!({"id":initialize.writes[0]["id"],"result":{"userAgent":"codex","codexHome":"/tmp"}}))
        .unwrap();
    let list = setup
        .writes
        .iter()
        .find(|frame| frame["method"] == "skills/list")
        .unwrap();
    let data = (0..MAX_SKILLS * 2)
        .map(|group| json!({"skills":[{"name":format!("skill-{group}")}]}))
        .collect::<Vec<_>>();
    let answered = adapter
        .receive(json!({"id":list["id"],"result":{"data":data}}))
        .unwrap();
    let skills = answered
        .events
        .iter()
        .find_map(|event| match event {
            Event::Skills(skills) => Some(skills),
            _ => None,
        })
        .unwrap();
    assert_eq!(skills.len(), MAX_SKILLS);
    assert_eq!(skills[0].invocation, "$skill-0");
}

/// The skills a catalog lists, answered to a fresh adapter.
fn listed(adapter: &mut Codex, data: Value) -> Vec<Skill> {
    let initialize = adapter.initialize().unwrap();
    let setup = adapter
        .receive(json!({"id":initialize.writes[0]["id"],"result":{"userAgent":"codex","codexHome":"/tmp"}}))
        .unwrap();
    let list = setup
        .writes
        .iter()
        .find(|frame| frame["method"] == "skills/list")
        .unwrap();
    adapter
        .receive(json!({"id":list["id"],"result":{"data":data}}))
        .unwrap()
        .events
        .into_iter()
        .find_map(|event| match event {
            Event::Skills(skills) => Some(skills),
            _ => None,
        })
        .unwrap()
}

/// Codex loads a skill a message names by `$name` only when no other
/// skill has that name, so two sharing one are each named by a link to
/// their own file, and every skill a message names goes with it as a
/// `skill` input carrying its path, as Codex's own composer sends it.
#[test]
fn named_skills_go_with_the_message_by_path() {
    let mut adapter = adapter();
    let skills = listed(
        &mut adapter,
        json!([
            {"cwd":"/repo","skills":[
                {"name":"commits","description":"Write commits","path":"/u/commits/SKILL.md","scope":"user","enabled":true},
                {"name":"lint","description":"Lint","path":"/u/lint/SKILL.md","scope":"user","enabled":true},
                {"name":"lint","description":"Lint here","path":"/repo/lint/SKILL.md","scope":"repo","enabled":true},
                {"name":"off","path":"/u/off/SKILL.md","enabled":false}
            ]},
            // The same skill reached from another directory is one skill.
            {"cwd":"/repo/sub","skills":[
                {"name":"commits","description":"Write commits","path":"/u/commits/SKILL.md","scope":"user","enabled":true}
            ]}
        ]),
    );
    let shown = skills
        .iter()
        .map(|skill| (skill.invocation.as_str(), skill.description.as_str()))
        .collect::<Vec<_>>();
    assert_eq!(
        shown,
        [
            ("$commits", "Write commits"),
            ("[$lint](/u/lint/SKILL.md)", "user · Lint"),
            ("[$lint](/repo/lint/SKILL.md)", "repo · Lint here"),
        ]
    );

    let text = "$commits then [$lint](/repo/lint/SKILL.md), $commits again, $lint, $off and $HOME";
    let input = adapter.input(text.into(), Vec::new()).unwrap();
    assert_eq!(
        input,
        [
            json!({"type":"text","text":text}),
            json!({"type":"skill","name":"commits","path":"/u/commits/SKILL.md"}),
            json!({"type":"skill","name":"lint","path":"/repo/lint/SKILL.md"}),
        ],
        "a bare shared name, a disabled skill and a variable name nothing"
    );
    assert_eq!(
        adapter.input("no skills here".into(), Vec::new()).unwrap(),
        [json!({"type":"text","text":"no skills here"})]
    );
}

#[test]
fn a_symlinked_skill_is_one_named_skill() {
    use std::{fs, os::unix::fs::symlink};

    let directory = tempfile::tempdir().unwrap();
    let actual = directory.path().join("dotfiles/skills/review/SKILL.md");
    fs::create_dir_all(actual.parent().unwrap()).unwrap();
    fs::write(&actual, "# Review").unwrap();
    let link = directory.path().join("agents-skills");
    symlink(directory.path().join("dotfiles/skills"), &link).unwrap();
    let alias = link.join("review/SKILL.md");
    let mut adapter = adapter();
    let skills = listed(
        &mut adapter,
        json!([{"skills":[
            {"name":"review","description":"Review changes","path":actual},
            {"name":"review","description":"Review changes","path":alias}
        ]}]),
    );
    assert_eq!(skills.len(), 1);
    assert_eq!(skills[0].invocation, "$review");
    assert_eq!(
        adapter.input("$review".into(), Vec::new()).unwrap(),
        [
            json!({"type":"text","text":"$review"}),
            json!({"type":"skill","name":"review","path":actual})
        ]
    );
}

#[test]
fn a_link_to_a_symlink_target_uses_the_providers_listed_path() {
    use std::{fs, os::unix::fs::symlink};

    let directory = tempfile::tempdir().unwrap();
    let actual = directory.path().join("skills/review/SKILL.md");
    fs::create_dir_all(actual.parent().unwrap()).unwrap();
    fs::write(&actual, "# Review").unwrap();
    let alias = directory.path().join("alias.md");
    symlink(&actual, &alias).unwrap();
    let mut adapter = adapter();
    listed(
        &mut adapter,
        json!([{"skills":[{"name":"review","path":alias}]}]),
    );
    let message = format!("[$review]({})", actual.display());
    assert_eq!(
        adapter.input(message.clone(), Vec::new()).unwrap(),
        [
            json!({"type":"text","text":message}),
            json!({"type":"skill","name":"review","path":alias})
        ]
    );
}

#[test]
fn distinct_same_scope_skills_show_different_sources() {
    let mut adapter = adapter();
    let skills = listed(
        &mut adapter,
        json!([{"skills":[
            {"name":"review","description":"Review changes","path":"/home/user/.agents/skills/review/SKILL.md","scope":"user"},
            {"name":"review","description":"Review changes","path":"/repo/.agents/skills/review/SKILL.md","scope":"user"}
        ]}]),
    );
    assert_eq!(skills.len(), 2);
    assert_eq!(
        skills[0].description,
        "user · user/.agents/skills · Review changes"
    );
    assert_eq!(
        skills[1].description,
        "user · repo/.agents/skills · Review changes"
    );
}

#[test]
fn source_labels_distinguish_skill_directories_with_the_same_declared_name() {
    assert_eq!(
        skill_source(
            "/repo/skills/a/SKILL.md",
            ["/repo/skills/b/SKILL.md"].into_iter(),
        ),
        "a"
    );
    assert_eq!(
        skill_source(
            "/repo/skills/b/SKILL.md",
            ["/repo/skills/a/SKILL.md"].into_iter(),
        ),
        "b"
    );
}

#[test]
fn mentions_are_read_the_way_codex_reads_them() {
    assert_eq!(
        skill_mentions("[$a-b:c](/x/SKILL.md) and $d_e.$PATH $ [$f] (/y) cost$9 [$g](/z/SKILL.md"),
        [
            Mention::Linked("/x/SKILL.md"),
            Mention::Named("d_e"),
            Mention::Linked("/y"),
            Mention::Named("9"),
            Mention::Named("g"),
        ]
    );
    assert!(skill_mentions("naïve $ é [$HOME](/h)").is_empty());
    assert_eq!(
        skill_mentions("[$x]( ) [$y]\u{a0}(/p)"),
        [Mention::Named("x"), Mention::Named("y"),]
    );
}

/// A skill part in an echoed message names what its text already names.
#[test]
fn skill_parts_of_a_user_message_add_nothing_to_show() {
    let echoed = json!({"id":"1","type":"userMessage","content":[
        {"type":"text","text":"$commits please"},
        {"type":"skill","name":"commits","path":"/u/commits/SKILL.md"},
        {"type":"mention","name":"x","path":"app://x"}
    ]});
    let items = parse_item(&echoed, true, "turn").unwrap();
    assert_eq!(items.len(), 1);
    assert!(matches!(&items[0].content,
        ItemContent::Text { text, .. } if text == "$commits please"));
}

#[test]
fn first_prompt_waits_for_skill_paths_in_either_response_order() {
    const PROMPT: &str = "[$review](/skills/review/SKILL.md) this change";
    for skills_first in [false, true] {
        for catalog_supported in [false, true] {
            let mut adapter = adapter();
            adapter.thread = None;
            adapter.materialized = false;
            let initialize = adapter.initialize().unwrap();
            let setup = adapter.receive(json!({"id":initialize.writes[0]["id"],"result":{"userAgent":"codex","codexHome":"/tmp"}})).unwrap();
            let id = |method| {
                setup
                    .writes
                    .iter()
                    .find(|frame| frame["method"] == method)
                    .unwrap()["id"]
                    .clone()
            };
            let attach = json!({"id":id("thread/start"),"result":{"thread":{"id":"fresh","canAcceptDirectInput":true,"status":{"type":"idle"}},"model":"model"}});
            let skills = if catalog_supported {
                json!({"id":id("skills/list"),"result":{"data":[{"skills":[
                    {"name":"review","path":"/skills/review/SKILL.md"},
                    {"name":"review","path":"/plugins/review/SKILL.md"}
                ]}]}})
            } else {
                json!({"id":id("skills/list"),"error":{"code":-32601,"message":"Method not found"}})
            };
            let responses = if skills_first {
                [skills, attach]
            } else {
                [attach, skills]
            };
            let first = adapter.receive(responses[0].clone()).unwrap();
            assert!(!first.events.contains(&Event::Ready(true)));
            let prompt = || Command::Prompt {
                text: PROMPT.into(),
                attachments: Vec::new(),
                mode: PromptMode::Send,
            };
            assert!(adapter.command(prompt()).is_err());
            let second = adapter.receive(responses[1].clone()).unwrap();
            assert!(second.events.contains(&Event::Ready(true)));
            let sent = adapter.command(prompt()).unwrap();
            let input = sent.writes[0]["params"]["input"].as_array().unwrap();
            assert_eq!(input[0], json!({"type":"text","text":PROMPT}));
            if catalog_supported {
                assert_eq!(
                    input[1],
                    json!({"type":"skill","name":"review","path":"/skills/review/SKILL.md"})
                );
            } else {
                assert_eq!(input.len(), 1);
            }
            let refresh = adapter
                .receive(json!({"method":"skills/changed","params":{}}))
                .unwrap();
            assert!(
                refresh
                    .writes
                    .iter()
                    .any(|frame| frame["method"] == "skills/list")
            );
            assert!(adapter.ready);
        }
    }
}

#[test]
fn new_thread_defers_history_until_its_first_turn_materializes() {
    let mut adapter = adapter();
    adapter.thread = None;
    adapter.materialized = false;
    let initialize = adapter.initialize().unwrap();
    let setup = adapter
        .receive(json!({"id":initialize.writes[0]["id"],"result":{"userAgent":"codex","codexHome":"/tmp"}}))
        .unwrap();
    let start = setup
        .writes
        .iter()
        .find(|frame| frame["method"] == "thread/start")
        .unwrap();
    let attached = adapter.receive(json!({"id":start["id"],"result":{"thread":{"id":"fresh","canAcceptDirectInput":true,"status":{"type":"idle"}},"model":"gpt-test"}})).unwrap();
    // The first sender also waits for the skill catalog.
    assert!(!attached.events.contains(&Event::Ready(true)));
    assert!(
        attached
            .events
            .iter()
            .any(|event| matches!(event, Event::Session { id, .. } if id == "fresh"))
    );
    assert!(
        !attached
            .writes
            .iter()
            .any(|frame| frame["method"] == "thread/turns/list")
    );
    let skills = setup
        .writes
        .iter()
        .find(|frame| frame["method"] == "skills/list")
        .unwrap();
    let catalog = adapter
        .receive(json!({"id":skills["id"],"result":{"data":[]}}))
        .unwrap();
    assert!(catalog.events.contains(&Event::Ready(true)));
    adapter.receive(json!({"method":"turn/started","params":{"threadId":"fresh","turn":{"id":"turn","status":"inProgress","items":[]}}})).unwrap();
    let refreshed = adapter.command(Command::Refresh).unwrap();
    assert!(
        refreshed
            .writes
            .iter()
            .any(|frame| frame["method"] == "thread/turns/list")
    );
}

/// Exercise the real actor dispatch without a provider subprocess. The
/// history transport records protocol writes in memory instead of sending
/// them, while retaining Codex's actual pending-decision state machine.
#[test]
fn settled_indexed_declines_are_checked_on_the_actor_without_authorizing_work() {
    use crate::chat::runtime::{Handle, preflight};
    use std::sync::{Arc, Mutex};

    struct Recorded {
        adapter: Codex,
        writes: Arc<Mutex<Vec<Value>>>,
    }
    impl Adapter for Recorded {
        fn initialize(&mut self) -> Result<Step, String> {
            unreachable!()
        }
        fn answer_declines(&self, id: &str, action: &str) -> bool {
            self.adapter.answer_declines(id, action)
        }
        fn command(&mut self, command: Command) -> Result<Step, String> {
            let mut step = self.adapter.command(command)?;
            self.writes.lock().unwrap().append(&mut step.writes);
            Ok(step)
        }
        fn receive(&mut self, frame: Value) -> Result<Step, String> {
            self.adapter.receive(frame)
        }
    }
    for decisions in [json!(["accept", "decline"]), json!(["cancel", "accept"])] {
        let mut adapter = adapter();
        let run = crate::model::RunId::new();
        let prompt = adapter
            .command_for_run(
                run,
                Command::Prompt {
                    text: "owned prompt".into(),
                    attachments: vec![],
                    mode: PromptMode::Send,
                },
            )
            .unwrap();
        let start = prompt
            .writes
            .iter()
            .find(|frame| frame["method"] == "turn/start")
            .unwrap();
        adapter.receive(json!({"id":start["id"],"result":{"turn":{"id":"owned","status":"inProgress","items":[]}}})).unwrap();
        let mut child = self::adapter();
        child.thread = Some("child".into());
        child.is_child = true;
        adapter.children.insert("child".into(), Box::new(child));
        let request = adapter.receive(json!({"id":"pending","method":"item/commandExecution/requestApproval","params":{
            "threadId":"child","turnId":"child-turn","itemId":"tool","availableDecisions":decisions,"command":"requested"
        }})).unwrap();
        let completed = adapter
            .receive(json!({"method":"turn/completed","params":{
                "threadId":"thread-exact","turn":{"id":"owned","status":"completed","items":[]}
            }}))
            .unwrap();
        assert!(completed.events.iter().any(|event| matches!(event,
            Event::Execution { run: actual, event: crate::agent_execution::ProviderEvent::Finished { outcome: crate::notifications::Kind::Succeeded }} if *actual == run)));
        let id = rpc_id(&json!("pending")).unwrap();
        let decline_index = decisions
            .as_array()
            .unwrap()
            .iter()
            .position(|v| matches!(v.as_str(), Some("decline" | "cancel")))
            .unwrap();
        let decline = format!("decision-{decline_index}");
        let accept = format!("decision-{}", 1 - decline_index);
        let directory = tempfile::tempdir().unwrap();
        let cwd = directory.path().join("approved");
        std::fs::create_dir(&cwd).unwrap();
        let context =
            super::super::ExecutionContext::new(Some(Arc::new(std::fs::File::open(&cwd).unwrap())));
        let spec = ProcessSpec {
            stdout_noise: None,
            execution: Some(Arc::new(context)),
            messages: None,
            transport: TransportSpec::History,
            program: "not-launched".into(),
            arguments: vec![],
            working_directory: cwd.clone(),
            environment: BTreeMap::new(),
            unset_environment: Default::default(),
        };
        let mut initial = Step {
            writes: vec![],
            events: vec![Event::Ready(true)],
            ..Step::default()
        };
        initial.events.extend(prompt.events);
        initial.events.extend(request.events);
        initial.events.extend(completed.events);
        let store = preflight(&spec, &mut initial).unwrap();
        let writes = Arc::new(Mutex::new(Vec::new()));
        let handle = Handle::start(
            spec,
            Box::new(Recorded {
                adapter,
                writes: writes.clone(),
            }),
            initial,
            store,
            None,
            |_| {},
        )
        .unwrap();
        // Command delivery is serialized after the initial snapshot.
        let answer = |action: String| Command::Answer {
            request_id: id.clone(),
            action,
            answers: vec![],
        };
        let refused = handle.decline_answer(answer(accept.clone())).unwrap_err();
        assert!(refused.contains("only decline"), "{refused}");
        assert!(
            handle.decline_answer(answer("decline".into())).is_err(),
            "literal action was never offered"
        );
        assert!(writes.lock().unwrap().is_empty());
        std::fs::remove_dir(&cwd).unwrap();
        assert!(
            handle.command(answer(accept)).is_err(),
            "authorization still requires its approved directory"
        );
        handle.decline_answer(answer(decline.clone())).unwrap();
        assert!(handle.requests().is_empty());
        assert_eq!(
            *writes.lock().unwrap(),
            vec![json!({"id":"pending","result":{"decision":decisions[decline_index]}})]
        );
        assert!(
            handle.decline_answer(answer(decline)).is_err(),
            "resolved requests cannot be answered twice"
        );
        handle.close().unwrap();
    }
}

#[test]
fn request_ids_and_resolution_prevent_wrong_or_stale_approval() {
    let mut adapter = adapter();
    for id in [json!(7), json!("7")] {
        let step = adapter.receive(json!({"id":id,"method":"item/commandExecution/requestApproval","params":{"threadId":"thread-exact","turnId":"turn","itemId":"tool","availableDecisions":["accept","decline"],"command":"echo requested"}})).unwrap();
        assert!(step.writes.is_empty());
    }
    assert_ne!(rpc_id(&json!(7)).unwrap(), rpc_id(&json!("7")).unwrap());
    adapter.receive(json!({"method":"serverRequest/resolved","params":{"threadId":"thread-exact","requestId":7}})).unwrap();
    assert!(
        adapter
            .command(Command::Answer {
                request_id: rpc_id(&json!(7)).unwrap(),
                action: "decision-0".into(),
                answers: vec![]
            })
            .is_err()
    );
    let answered = adapter
        .command(Command::Answer {
            request_id: rpc_id(&json!("7")).unwrap(),
            action: "decision-1".into(),
            answers: vec![],
        })
        .unwrap();
    assert_eq!(
        answered.writes,
        json!([{"id":"7","result":{"decision":"decline"}}])
            .as_array()
            .unwrap()
            .clone()
    );
}

/// A command writes for a terminal, and Codex forwards what it wrote: a
/// colour cut in two by the end of a delta must not reach the transcript
/// as its second half, and the output reported whole at the end shows a
/// redrawn line as it was last drawn.
#[test]
fn command_output_reaches_the_transcript_without_terminal_escapes() {
    let mut adapter = adapter();
    adapter.busy = true;
    let item = json!({"id":"0","type":"commandExecution","command":"cargo build","cwd":"/tmp","status":"inProgress","aggregatedOutput":"\u{1b}[1mStart\u{1b}[0m\n"});
    let mut page = Step::default();
    adapter.page(&json!({"data":[{"id":"turn-a","status":"inProgress","items":[item]}],"nextCursor":"older"}),&mut page, false).unwrap();
    let mut delta = |text: &str| {
        adapter.receive(json!({"method":"item/commandExecution/outputDelta","params":{"threadId":"thread-exact","turnId":"turn-a","itemId":"0","delta":text}})).unwrap().events
    };
    // The introducer alone appends nothing, so there is nothing to send.
    assert!(delta("\u{1b}[1;3").is_empty());
    let events = delta("2m   Compiling\u{1b}[0m a\n 1/2\r");
    assert!(
        matches!(&events[0], Event::OutputDelta { offset: 6, delta, .. } if delta == "   Compiling a\n 1/2"),
        "{events:?}"
    );
    let events = delta(" 2/2\r\n");
    assert!(
        matches!(&events[0], Event::OutputDelta { offset: 25, delta, .. } if delta == "\n 2/2\n"),
        "{events:?}"
    );
    let completed = json!({"id":"0","type":"commandExecution","command":"cargo build","cwd":"/tmp","status":"completed","exitCode":0,"aggregatedOutput":"\u{1b}[1mStart\u{1b}[0m\n\u{1b}[1;32m   Compiling\u{1b}[0m a\n 1/2\r 2/2\r\n"});
    let done = adapter.receive(json!({"method":"item/completed","params":{"threadId":"thread-exact","turnId":"turn-a","item":completed}})).unwrap();
    assert!(done.events.iter().any(|event| matches!(event,
        Event::Item(Item { content: ItemContent::Command { output, .. }, .. })
        if output == "Start\n   Compiling a\n 2/2\n")));
    assert!(adapter.output_streams.is_empty());
}

#[test]
fn resumed_step_and_final_history_use_same_turn_scoped_identity() {
    let mut adapter = adapter();
    adapter.busy = true;
    let item = json!({"id":"0","type":"commandExecution","command":"echo hello","cwd":"/tmp","status":"inProgress","aggregatedOutput":"hel"});
    let mut page = Step::default();
    adapter.page(&json!({"data":[{"id":"turn-a","status":"inProgress","items":[item]}],"nextCursor":"older"}),&mut page, false).unwrap();
    let delta = adapter.receive(json!({"method":"item/commandExecution/outputDelta","params":{"threadId":"thread-exact","turnId":"turn-a","itemId":"0","delta":"lo"}})).unwrap();
    assert!(
        matches!(&delta.events[0],Event::OutputDelta { id, offset: 3, delta } if id == "turn-a/0" && delta == "lo")
    );
    let completed = json!({"id":"0","type":"agentMessage","text":"answer"});
    let one = parse_item(&completed, true, "turn-a").unwrap();
    let two = parse_item(&completed, true, "turn-b").unwrap();
    assert_ne!(one[0].id, two[0].id);
    let live = adapter.receive(json!({"method":"item/agentMessage/delta","params":{"threadId":"thread-exact","turnId":"turn-a","itemId":"0","delta":"answer"}})).unwrap();
    assert!(matches!(&live.events[0],Event::TextDelta { id,.. } if id == &one[0].id));
}

#[test]
fn long_active_turn_history_tracks_only_running_tools() {
    let mut adapter = adapter();
    adapter.busy = true;
    let mut items: Vec<_> = (0..MAX_ACTIVE_ITEMS + 32)
        .map(|index| json!({"id":index.to_string(),"type":"commandExecution","command":"echo done","cwd":"/repo","status":"completed","aggregatedOutput":"done"}))
        .collect();
    items.push(json!({"id":"running","type":"commandExecution","command":"echo live","cwd":"/repo","status":"inProgress","aggregatedOutput":"start"}));
    let turn = json!({"id":"long","status":"inProgress","items":items});
    let mut page = Step::default();
    adapter
        .page(
            &json!({"data":[turn.clone()],"nextCursor":null}),
            &mut page,
            false,
        )
        .unwrap();
    assert_eq!(adapter.active_items.len(), 1);
    assert!(
        page.events.iter().any(|event| matches!(event, Event::LatestHistory { items, .. } if items.len() == MAX_ACTIVE_ITEMS + 33))
    );
    let started = adapter
        .receive(json!({"method":"turn/started","params":{"threadId":"thread-exact","turn":turn}}))
        .unwrap();
    assert_eq!(adapter.active_items.len(), 1);
    assert_eq!(
        started
            .events
            .iter()
            .filter(|event| matches!(event, Event::Item(_)))
            .count(),
        MAX_ACTIVE_ITEMS + 33
    );
    let delta = adapter.receive(json!({"method":"item/commandExecution/outputDelta","params":{"threadId":"thread-exact","turnId":"long","itemId":"running","delta":" tail"}})).unwrap();
    assert!(
        matches!(&delta.events[0], Event::OutputDelta { offset: 5, delta, .. } if delta == " tail")
    );
    let completed = json!({"id":"running","type":"commandExecution","command":"echo live","cwd":"/repo","status":"completed","aggregatedOutput":"start tail"});
    adapter.receive(json!({"method":"item/completed","params":{"threadId":"thread-exact","turnId":"long","item":completed}})).unwrap();
    assert!(adapter.active_items.is_empty());
}

#[test]
fn orchestrating_turn_outlives_the_live_tool_bound() {
    // `sleep` and `subAgentActivity` state no status, so each reads as
    // running until its turn ends, whether it arrives in the turn's
    // history or as it starts.
    let auxiliary = |index: usize| {
        if index.is_multiple_of(2) {
            json!({"id":format!("aux-{index}"),"type":"sleep","durationMs":1000})
        } else {
            json!({"id":format!("aux-{index}"),"type":"subAgentActivity","kind":"started","agentThreadId":"child","agentPath":"/root/review"})
        }
    };
    let mut adapter = adapter();
    adapter.busy = true;
    let mut items: Vec<_> = (0..MAX_ACTIVE_ITEMS * 2).map(auxiliary).collect();
    items.push(json!({"id":"running","type":"commandExecution","command":"echo live","cwd":"/repo","status":"inProgress","aggregatedOutput":"start"}));
    let turn = json!({"id":"long","status":"inProgress","items":items});
    let mut page = Step::default();
    adapter
        .page(
            &json!({"data":[turn.clone()],"nextCursor":null}),
            &mut page,
            false,
        )
        .unwrap();
    assert_eq!(adapter.active_items.len(), 1);
    adapter
        .receive(json!({"method":"turn/started","params":{"threadId":"thread-exact","turn":turn}}))
        .unwrap();
    for index in 0..MAX_ACTIVE_ITEMS * 2 {
        adapter.receive(json!({"method":"item/started","params":{"threadId":"thread-exact","turnId":"long","item":auxiliary(index)}})).unwrap();
    }
    assert_eq!(adapter.active_items.len(), 1);
    let delta = adapter.receive(json!({"method":"item/commandExecution/outputDelta","params":{"threadId":"thread-exact","turnId":"long","itemId":"running","delta":" tail"}})).unwrap();
    assert!(
        matches!(&delta.events[0], Event::OutputDelta { offset: 5, delta, .. } if delta == " tail")
    );
}

#[test]
fn running_tools_past_the_bound_go_unheld_without_ending_the_session() {
    let mut adapter = adapter();
    adapter.busy = true;
    let command = |index: usize, status: &str| json!({"id":index.to_string(),"type":"commandExecution","command":"sleep 1","cwd":"/repo","status":status,"aggregatedOutput":""});
    for index in 0..=MAX_ACTIVE_ITEMS {
        let started = adapter.receive(json!({"method":"item/started","params":{"threadId":"thread-exact","turnId":"turn","item":command(index, "inProgress")}})).unwrap();
        assert!(matches!(&started.events[0], Event::Item(_)));
    }
    assert_eq!(adapter.active_items.len(), MAX_ACTIVE_ITEMS);
    let unheld = adapter.receive(json!({"method":"item/commandExecution/outputDelta","params":{"threadId":"thread-exact","turnId":"turn","itemId":MAX_ACTIVE_ITEMS.to_string(),"delta":"lost"}})).unwrap();
    assert!(unheld.events.is_empty());
    let completed = adapter.receive(json!({"method":"item/completed","params":{"threadId":"thread-exact","turnId":"turn","item":command(MAX_ACTIVE_ITEMS, "completed")}})).unwrap();
    assert!(matches!(
        &completed.events[0],
        Event::Item(Item {
            content: ItemContent::Command {
                status: ToolStatus::Succeeded,
                ..
            },
            ..
        })
    ));
}

#[test]
fn active_turn_history_releases_tools_that_completed_unseen() {
    let mut adapter = adapter();
    adapter.busy = true;
    let command = |status: &str| json!({"id":"cmd","type":"commandExecution","command":"echo hi","cwd":"/repo","status":status,"aggregatedOutput":"hi"});
    adapter.receive(json!({"method":"item/started","params":{"threadId":"thread-exact","turnId":"turn","item":command("inProgress")}})).unwrap();
    adapter.receive(json!({"method":"item/commandExecution/outputDelta","params":{"threadId":"thread-exact","turnId":"turn","itemId":"cmd","delta":"\u{1b}["}})).unwrap();
    assert_eq!(adapter.active_items.len(), 1);
    assert_eq!(adapter.output_streams.len(), 1);
    let mut page = Step::default();
    adapter
        .page(
            &json!({"data":[{"id":"turn","status":"inProgress","items":[command("completed")]}],"nextCursor":null}),
            &mut page,
            false,
        )
        .unwrap();
    assert!(adapter.active_items.is_empty());
    assert!(adapter.output_streams.is_empty());
}

#[test]
fn oversized_tool_output_keeps_stream_and_completion_alive() {
    let mut adapter = adapter();
    adapter.receive(json!({"method":"turn/started","params":{"threadId":"thread-exact","turn":{"id":"turn","status":"inProgress","items":[]}}})).unwrap();
    let mut item = json!({"id":"large","type":"commandExecution","command":"cat data","cwd":"/tmp","status":"inProgress","aggregatedOutput":""});
    adapter.receive(json!({"method":"item/started","params":{"threadId":"thread-exact","turnId":"turn","item":item}})).unwrap();
    let delta = "🦀".repeat(MAX_ITEM_BYTES);
    let streamed = adapter.receive(json!({"method":"item/commandExecution/outputDelta","params":{"threadId":"thread-exact","turnId":"turn","itemId":"large","delta":delta}})).unwrap();
    let Event::Item(streamed) = &streamed.events[0] else {
        panic!("missing bounded output window")
    };
    let ItemContent::Command { output: suffix, .. } = &streamed.content else {
        panic!("missing command")
    };
    assert!(suffix.len() <= MAX_ITEM_BYTES);
    assert!(suffix.contains("bytes omitted"));
    assert!(suffix.ends_with("🦀"));
    item["status"] = json!("completed");
    item["aggregatedOutput"] = json!(delta);
    let completed = adapter.receive(json!({"method":"item/completed","params":{"threadId":"thread-exact","turnId":"turn","item":item}})).unwrap();
    let Event::Item(completed) = &completed.events[0] else {
        panic!("missing completed item")
    };
    assert!(completed.content.byte_len() <= MAX_ITEM_BYTES);
    assert!(matches!(
        &completed.content,
        ItemContent::Command {
            status: ToolStatus::Succeeded,
            ..
        }
    ));
    assert!(adapter.active_items.is_empty());
    adapter.receive(json!({"method":"turn/completed","params":{"threadId":"thread-exact","turn":{"id":"turn","status":"completed","items":[]}}})).unwrap();
    assert!(!adapter.busy);
}

#[test]
fn file_change_rename_points_at_the_destination_and_keeps_the_source() {
    let item = json!({"id":"0","type":"fileChange","status":"completed","changes":[
        {"path":"src/old.rs","kind":{"type":"update","move_path":"src/new.rs"},"diff":"@@ -1 +1 @@\n-old\n+new\n"},
        {"path":"src/kept.rs","kind":{"type":"update","move_path":null},"diff":"@@ -2 +2 @@\n"},
        {"path":"src/gone.rs","kind":{"type":"delete"},"diff":"-gone\n"},
        {"path":"src/fresh.rs","kind":{"type":"add"},"diff":"+fresh\n"}
    ]});
    let items = parse_item(&item, true, "turn").unwrap();
    let ItemContent::Changes { changes, status } = &items[0].content else {
        panic!("A Codex fileChange must carry its patch, not a rendering of it");
    };
    assert_eq!(*status, ToolStatus::Succeeded);
    assert_eq!(
        changes[0],
        FileChange {
            path: "src/new.rs".into(),
            kind: ChangeKind::Renamed {
                from: "src/old.rs".into()
            },
            diff: "@@ -1 +1 @@\n-old\n+new\n".into(),
        }
    );
    assert_eq!(changes[1].kind, ChangeKind::Updated);
    assert_eq!(changes[2].kind, ChangeKind::Deleted);
    assert_eq!(changes[3].kind, ChangeKind::Added);
    let unknown = parse_item(&json!({"id":"0","type":"fileChange","status":"completed","changes":[{"path":"p","kind":{"type":"chmod"},"diff":"@@ -1 +1 @@\n"}]}), true, "turn").unwrap();
    let ItemContent::Changes { changes, .. } = &unknown[0].content else {
        panic!("A file change kind this build does not know is still a file change");
    };
    assert_eq!(changes[0].kind, ChangeKind::Updated);
}

fn only_tool(value: Value, done: bool) -> (String, String, String, ToolStatus) {
    match &parse_item(&value, done, "turn").unwrap()[..] {
        [
            Item {
                content:
                    ItemContent::Tool {
                        name,
                        input,
                        output,
                        status,
                    },
                ..
            },
        ] => (name.clone(), input.clone(), output.clone(), *status),
        items => panic!("one tool row, not {items:?}"),
    }
}

/// A tool row carries what a reader can use: the app's name for an MCP
/// call, no arguments when there were none, the text a tool returned
/// rather than the content parts carrying it, and the error's message.
#[test]
fn tool_rows_carry_readable_names_arguments_results_and_errors() {
    let called = json!({"id":"m","type":"mcpToolCall","server":"codex_apps","tool":"linear_get_issue",
        "appContext":{"connectorId":"c","appName":"Linear","actionName":"get_issue"},
        "status":"completed","arguments":null,
        "result":{"content":[{"type":"text","text":"ENG-42: Tool rows"},{"type":"text","text":"Status: open"}],"structuredContent":null}});
    assert_eq!(
        only_tool(called, true),
        (
            "Linear / get_issue".into(),
            String::new(),
            "ENG-42: Tool rows\n\nStatus: open".into(),
            ToolStatus::Succeeded
        )
    );
    let structured = json!({"id":"m","type":"mcpToolCall","server":"docs","tool":"search","status":"completed",
        "arguments":{"query":"hover"},"result":{"content":[],"structuredContent":{"hits":1}}});
    let (name, input, output, _) = only_tool(structured, true);
    assert_eq!(
        (name.as_str(), input.as_str()),
        ("docs / search", r#"{"query":"hover"}"#)
    );
    assert_eq!(output, "{\n  \"hits\": 1\n}");
    let failed = json!({"id":"m","type":"mcpToolCall","server":"docs","tool":"search","status":"failed",
        "arguments":{},"error":{"message":"Connector is not authorized"}});
    assert_eq!(
        only_tool(failed, true),
        (
            "docs / search".into(),
            String::new(),
            "Connector is not authorized".into(),
            ToolStatus::Failed
        )
    );
    for output in [
        json!("plain text"),
        json!([{"type":"input_text","text":"plain text"}]),
    ] {
        let item = json!({"id":"f","type":"functionCallOutput","name":"lookup","output":output});
        assert_eq!(only_tool(item, true).2, "plain text");
    }
    let interrupted = json!({"id":"c","type":"collabAgentToolCall","tool":"interruptAgent","status":"interrupted",
        "senderThreadId":"thread-exact","receiverThreadIds":["child"],"agentsStates":{}});
    let (_, _, states, status) = only_tool(interrupted, true);
    assert_eq!(status, ToolStatus::Stopped);
    // The receiver is named even before Codex reports its state.
    assert_eq!(states, r#"{"child":{}}"#);
    let reported = json!({"id":"c","type":"collabAgentToolCall","tool":"sendMessage","status":"completed",
        "senderThreadId":"thread-exact","receiverThreadIds":["child"],
        "agentsStates":{"child":{"status":"running"}}});
    assert_eq!(
        only_tool(reported, true).2,
        r#"{"child":{"status":"running"}}"#
    );
}

/// Codex's own events read as events: an image being generated says
/// nothing until it has a result, compaction says it is under way until
/// it is done, a hook's prompt is its text, a pause is a pause, and a
/// subagent's lifecycle adds no rows of its own.
#[test]
fn codex_events_read_as_what_happened() {
    let generating = json!({"id":"g","type":"imageGeneration","status":"inProgress","inputImages":[],"revisedPrompt":"A fox"});
    assert_eq!(
        only_tool(generating, false),
        (
            "Image generation".into(),
            "A fox".into(),
            String::new(),
            ToolStatus::Running
        )
    );
    let limited = json!({"id":"g","type":"imageGeneration","status":"failed","inputImages":[],
        "failure":{"type":"usageLimitExceeded","limitId":"images"}});
    let (_, _, output, status) = only_tool(limited, true);
    assert_eq!(
        (output.as_str(), status),
        (
            "Image generation has reached its usage limit.",
            ToolStatus::Failed
        )
    );
    let viewed = parse_item(
        &json!({"id":"v","type":"imageView","path":"/repo/shots/after.png"}),
        true,
        "turn",
    )
    .unwrap();
    assert!(matches!(&viewed[0].content,
        ItemContent::Image { description, .. } if description == "after.png"));
    let notice = |value: Value, done: bool| match &parse_item(&value, done, "turn").unwrap()[..] {
        [
            Item {
                role: Role::System,
                content:
                    ItemContent::Delivery {
                        kind: chat::DeliveryKind::Notice,
                        summary,
                        body,
                        ..
                    },
                ..
            },
        ] => format!("{summary}: {body}"),
        items => panic!("one notice, not {items:?}"),
    };
    // A pause or a compaction keeps its name; its status says how far it
    // got, so a replayed or abandoned one never reads as still going.
    let compaction = json!({"id":"k","type":"contextCompaction"});
    let running = only_tool(compaction.clone(), false);
    assert_eq!(
        (running.0.as_str(), running.3),
        ("Compact context", ToolStatus::Running)
    );
    let done = only_tool(compaction, true);
    assert_eq!(
        (done.0.as_str(), done.3),
        ("Compact context", ToolStatus::Succeeded)
    );
    let empty =
        json!({"id":"h0","type":"hookPrompt","fragments":[{"hookRunId":"run-0","text":"  "}]});
    assert!(parse_item(&empty, true, "turn").unwrap().is_empty());
    let hook = json!({"id":"h","type":"hookPrompt","fragments":[
        {"hookRunId":"run-1","text":"Remember the style guide."},{"hookRunId":"run-2","text":"Run the tests."}]});
    assert_eq!(
        notice(hook, true),
        "Context from a hook: Remember the style guide.\n\nRun the tests."
    );
    let sleep = json!({"id":"s","type":"sleep","durationMs":90_000});
    let running = only_tool(sleep.clone(), false);
    assert_eq!(
        (running.0.as_str(), running.3),
        ("Wait 1m 30s", ToolStatus::Running)
    );
    let done = only_tool(sleep, true);
    assert_eq!(
        (done.0.as_str(), done.3),
        ("Wait 1m 30s", ToolStatus::Succeeded)
    );
    let activity = json!({"id":"a","type":"subAgentActivity","kind":"started","agentThreadId":"child","agentPath":"/root/review"});
    assert!(parse_item(&activity, true, "turn").unwrap().is_empty());
    let mut adapter = adapter();
    let diff = adapter
        .receive(json!({"method":"turn/diff/updated","params":{"threadId":"thread-exact","turnId":"turn","diff":"@@ -1 +1 @@\n-a\n+b\n"}}))
        .unwrap();
    assert!(diff.events.is_empty());
}

/// A tool result's structured content follows its text when it says
/// more than the text, and is not repeated when the text is that JSON.
#[test]
fn a_tool_result_keeps_structured_content_its_text_does_not_carry() {
    let both = json!({"content":[{"type":"text","text":"Found two issues"}],
        "structuredContent":{"issues":["ENG-1","ENG-2"]}});
    assert_eq!(
        images::readable(&both),
        "Found two issues\n\n{\n  \"issues\": [\n    \"ENG-1\",\n    \"ENG-2\"\n  ]\n}"
    );
    let same = json!({"content":[{"type":"text","text":"{\"issues\":[\"ENG-1\"]}"}],
        "structuredContent":{"issues":["ENG-1"]}});
    assert_eq!(images::readable(&same), "{\"issues\":[\"ENG-1\"]}");
}

/// A turn's diff is its own change row only for the files none of its
/// file-change items edited, such as one a shell command rewrote, and
/// that row goes once items account for every file.
#[test]
fn a_turn_diff_shows_only_the_files_no_item_changed() {
    let diff = "diff --git a/src/lib.rs b/src/lib.rs\nindex 1..2 100644\n--- a/src/lib.rs\n+++ b/src/lib.rs\n@@ -1 +1 @@\n-a\n+b\n\
diff --git a/README.md b/README.md\nindex 3..4 100644\n--- a/README.md\n+++ b/README.md\n@@ -1 +1,2 @@\n # Title\n+More\n\
diff --git a/new.txt b/new.txt\nnew file mode 100644\nindex 0..5\n--- /dev/null\n+++ b/new.txt\n@@ -0,0 +1 @@\n+hi\n\
diff --git a/old.txt b/old.txt\ndeleted file mode 100644\nindex 6..0\n--- a/old.txt\n+++ /dev/null\n@@ -1 +0,0 @@\n-bye\n\
diff --git a/a.txt b/b.txt\nsimilarity index 100%\nrename from a.txt\nrename to b.txt\n\
diff --git a/run.sh b/run.sh\nold mode 100644\nnew mode 100755\n\
diff --git a/empty.txt b/empty.txt\nnew file mode 100644\nindex 0000000..e69de29\n\
diff --git a/logo.png b/logo.png\ndeleted file mode 100644\nindex 1..0\nBinary files a/logo.png and /dev/null differ\n";
    let changes = turn_diff_changes(diff);
    assert_eq!(
        changes
            .iter()
            .map(|change| (change.path.as_str(), change.kind.clone()))
            .collect::<Vec<_>>(),
        [
            ("src/lib.rs", ChangeKind::Updated),
            ("README.md", ChangeKind::Updated),
            ("new.txt", ChangeKind::Added),
            ("old.txt", ChangeKind::Deleted),
            (
                "b.txt",
                ChangeKind::Renamed {
                    from: "a.txt".into()
                }
            ),
            ("run.sh", ChangeKind::Updated),
            ("empty.txt", ChangeKind::Added),
            ("logo.png", ChangeKind::Deleted),
        ]
    );
    assert_eq!(changes[1].diff, "@@ -1 +1,2 @@\n # Title\n+More\n");
    assert_eq!(changes[5].diff, "old mode 100644\nnew mode 100755\n");

    let mut adapter = adapter();
    let file_change = |id: &str, path: &str, diff: &str| {
        json!({"method":"item/completed","params":{"threadId":"thread-exact","turnId":"turn","item":
            {"id":id,"type":"fileChange","status":"completed","changes":[{"path":path,"kind":{"type":"update"},"diff":diff}]}}})
    };
    // A patch the agent made, and one to a different file of the same name.
    adapter
        .receive(file_change(
            "lib",
            "/tmp/src/lib.rs",
            "@@ -1 +1 @@\n-a\n+b\n",
        ))
        .unwrap();
    adapter
        .receive(file_change(
            "docs",
            "/tmp/docs/README.md",
            "@@ -1 +1,2 @@\n # Title\n+More\n",
        ))
        .unwrap();
    // The README a shell command edited, and a second line a shell
    // command added to the patched file.
    let diff = "diff --git a/src/lib.rs b/src/lib.rs\n--- a/src/lib.rs\n+++ b/src/lib.rs\n@@ -1,2 +1,3 @@\n-a\n+b\n c\n+d\n\
diff --git a/README.md b/README.md\n--- a/README.md\n+++ b/README.md\n@@ -1 +1,2 @@\n # Title\n+More\n";
    let shown = adapter
        .receive(json!({"method":"turn/diff/updated","params":{"threadId":"thread-exact","turnId":"turn","diff":diff}}))
        .unwrap();
    let paths = |events: &[Event]| match events {
        [
            Event::Item(Item {
                id,
                content:
                    ItemContent::Changes {
                        changes,
                        status: ToolStatus::Succeeded,
                    },
                ..
            }),
        ] if id == "turn/diff" => changes
            .iter()
            .map(|change| change.path.clone())
            .collect::<Vec<_>>(),
        events => panic!("one change row, not {events:?}"),
    };
    assert_eq!(paths(&shown.events), ["/tmp/src/lib.rs", "/tmp/README.md"]);
    // Items that account for every changed line retract the row.
    let partly = adapter
        .receive(file_change(
            "readme",
            "/tmp/README.md",
            "@@ -1 +1,2 @@\n # Title\n+More\n",
        ))
        .unwrap();
    assert_eq!(paths(&partly.events[..1]), ["/tmp/src/lib.rs"]);
    let covered = adapter
        .receive(file_change(
            "lib2",
            "/tmp/src/lib.rs",
            "@@ -1,2 +1,3 @@\n b\n c\n+d\n",
        ))
        .unwrap();
    assert!(matches!(&covered.events[0], Event::ItemsRemoved { ids } if ids == &["turn/diff"]));
    let unchanged = adapter
        .receive(json!({"method":"turn/diff/updated","params":{"threadId":"thread-exact","turnId":"turn","diff":diff}}))
        .unwrap();
    assert!(unchanged.events.is_empty());

    // An item reported again counts once: a shell's second `-a/+b` in
    // the same file stays shown when the patch is replayed.
    let mut adapter = super::tests::adapter();
    adapter
        .receive(file_change(
            "lib",
            "/tmp/src/lib.rs",
            "@@ -1 +1 @@\n-a\n+b\n",
        ))
        .unwrap();
    let twice = "diff --git a/src/lib.rs b/src/lib.rs\n--- a/src/lib.rs\n+++ b/src/lib.rs\n@@ -1,3 +1,3 @@\n-a\n+b\n c\n-a\n+b\n";
    let diff_update = |diff: &str| json!({"method":"turn/diff/updated","params":{"threadId":"thread-exact","turnId":"turn","diff":diff}});
    assert_eq!(
        paths(&adapter.receive(diff_update(twice)).unwrap().events),
        ["/tmp/src/lib.rs"]
    );
    let replayed = adapter
        .receive(file_change(
            "lib",
            "/tmp/src/lib.rs",
            "@@ -1 +1 @@\n-a\n+b\n",
        ))
        .unwrap();
    assert_eq!(paths(&replayed.events[..1]), ["/tmp/src/lib.rs"]);
    // A line whose text starts `++` is a change like any other.
    let plus = "diff --git a/src/lib.rs b/src/lib.rs\n--- a/src/lib.rs\n+++ b/src/lib.rs\n@@ -1 +1,2 @@\n-a\n+b\n+++ note\n";
    assert_eq!(
        paths(&adapter.receive(diff_update(plus)).unwrap().events),
        ["/tmp/src/lib.rs"]
    );
    // A mode change no item made stays, with the lines an item did make.
    let mode = "diff --git a/src/lib.rs b/src/lib.rs\nold mode 100644\nnew mode 100755\n--- a/src/lib.rs\n+++ b/src/lib.rs\n@@ -1 +1 @@\n-a\n+b\n";
    let moded = adapter.receive(diff_update(mode)).unwrap();
    assert!(
        matches!(&moded.events[..], [Event::Item(Item { content: ItemContent::Changes { changes, .. }, .. })]
        if changes[0].diff == "old mode 100644\nnew mode 100755\n@@ -1 +1 @@\n-a\n+b\n")
    );
    // An empty diff retracts what an earlier one showed.
    let empty = adapter.receive(diff_update("")).unwrap();
    assert!(matches!(&empty.events[..], [Event::ItemsRemoved { ids }] if ids == &["turn/diff"]));
}

#[test]
fn an_unrecognized_item_part_or_untracked_patch_cannot_end_the_session() {
    let future = json!({"id":"0","type":"quantumThought","status":"completed","why":"new"});
    let items = parse_item(&future, true, "turn").unwrap();
    assert!(matches!(&items[0].content,
        ItemContent::Tool { name, output, status: ToolStatus::Succeeded, .. }
        if name == "quantumThought" && output.contains("quantumThought")));
    let part = json!({"id":"1","type":"userMessage","content":[{"type":"hologram","n":3}]});
    let items = parse_item(&part, true, "turn").unwrap();
    assert!(matches!(&items[0].content,
        ItemContent::Text { text, .. } if text.contains("hologram")));
    assert_eq!(items[0].role, Role::User);
    let mut adapter = adapter();
    let ignored = adapter.receive(json!({"method":"item/fileChange/patchUpdated","params":{"threadId":"thread-exact","turnId":"turn-a","itemId":"0","changes":"not an array"}})).unwrap();
    assert!(ignored.events.is_empty());
}

#[test]
fn command_actions_classify_the_command_and_name_its_target() {
    let read = json!({"id":"0","type":"commandExecution","command":"sed -n '1,20p' src/app.rs","cwd":"/repo","status":"completed","exitCode":0,"durationMs":12,"aggregatedOutput":"fn main() {}","commandActions":[{"type":"read","command":"sed -n '1,20p' src/app.rs","name":"app.rs","path":"src/app.rs"}]});
    let items = parse_item(&read, true, "turn").unwrap();
    let ItemContent::Command {
        label,
        command,
        working_directory,
        action,
        exit_code,
        duration_ms,
        output,
        ..
    } = &items[0].content
    else {
        panic!("A Codex commandExecution must map to a command");
    };
    assert_eq!(*action, CommandAction::Read);
    assert_eq!(label, "src/app.rs");
    assert_eq!(command, "sed -n '1,20p' src/app.rs");
    assert_eq!(working_directory, "/repo");
    assert_eq!(*exit_code, Some(0));
    assert_eq!(*duration_ms, Some(12));
    assert_eq!(output, "fn main() {}");
    let unclassified = json!({"id":"1","type":"commandExecution","command":"make release","cwd":"/repo","status":"inProgress","commandActions":[{"type":"unknown","command":"make release"}]});
    let items = parse_item(&unclassified, false, "turn").unwrap();
    let ItemContent::Command {
        label,
        action,
        status,
        exit_code,
        duration_ms,
        ..
    } = &items[0].content
    else {
        panic!("A Codex commandExecution must map to a command");
    };
    assert_eq!(*action, CommandAction::Run);
    assert!(label.is_empty());
    assert_eq!(*status, ToolStatus::Running);
    assert_eq!(*exit_code, None);
    assert_eq!(*duration_ms, None);
}

/// A compound command is classified by what all of it does, never by the
/// one part Codex could name. These are the shapes Codex reports for the
/// compound commands agents run.
#[test]
fn a_compound_command_is_classified_only_when_every_part_does_the_same() {
    let intent = |command: &str, actions: Value| {
        let item = json!({"id":"0","type":"commandExecution","command":command,"cwd":"/repo","status":"completed","exitCode":0,"commandActions":actions});
        let items = parse_item(&item, true, "turn").unwrap();
        let ItemContent::Command { action, label, .. } = &items[0].content else {
            panic!("A Codex commandExecution must map to a command");
        };
        (*action, label.clone())
    };
    let ran = (CommandAction::Run, String::new());
    // Listing, then reading: neither is what the command is.
    assert_eq!(
        intent(
            "ls docs && sed -n '1,120p' docs/development.md",
            json!([
                {"type":"listFiles","command":"ls docs","path":"docs"},
                {"type":"read","command":"sed -n '1,120p' docs/development.md","name":"development.md","path":"docs/development.md"}
            ])
        ),
        ran
    );
    // Two files read is not a read of the first.
    assert_eq!(
        intent(
            "cat CLAUDE.md; cat docs/development.md",
            json!([
                {"type":"read","command":"cat CLAUDE.md","name":"CLAUDE.md","path":"CLAUDE.md"},
                {"type":"read","command":"cat docs/development.md","name":"development.md","path":"docs/development.md"}
            ])
        ),
        ran
    );
    // A part Codex could not name, here the issue's `cagents … sessions`.
    assert_eq!(
        intent(
            "ls ~/.config/cagents/; timeout 20 cagents --device fw16 --no-start sessions --json",
            json!([
                {"type":"listFiles","command":"ls ~/.config/cagents/","path":"cagents"},
                {"type":"unknown","command":"timeout 20 cagents --device fw16 --no-start sessions --json"}
            ])
        ),
        ran
    );
    // Two views of one file are still a read of it.
    assert_eq!(
        intent(
            "sed -n '1,40p' src/app.rs && sed -n '200,240p' src/app.rs",
            json!([
                {"type":"read","command":"sed -n '1,40p' src/app.rs","name":"app.rs","path":"src/app.rs"},
                {"type":"read","command":"sed -n '200,240p' src/app.rs","name":"app.rs","path":"src/app.rs"}
            ])
        ),
        (CommandAction::Read, "src/app.rs".to_owned())
    );
    assert_eq!(
        intent(
            "rg -n 'fn main' src",
            json!([{"type":"search","command":"rg -n 'fn main' src","query":"fn main","path":"src"}])
        ),
        (CommandAction::Search, "fn main in src".to_owned())
    );
}

#[test]
fn a_non_zero_exit_code_is_a_result_rather_than_a_failure() {
    let no_matches = json!({"id":"0","type":"commandExecution","command":"grep -n todo src","cwd":"/repo","status":"completed","exitCode":1,"durationMs":8,"aggregatedOutput":""});
    let items = parse_item(&no_matches, true, "turn").unwrap();
    let ItemContent::Command {
        status, exit_code, ..
    } = &items[0].content
    else {
        panic!("A Codex commandExecution must map to a command");
    };
    assert_eq!(
        *status,
        ToolStatus::Succeeded,
        "a grep with no matches exits 1 and has not failed at anything"
    );
    assert_eq!(*exit_code, Some(1), "the code itself stays visible");
    let reported = json!({"id":"1","type":"commandExecution","command":"cargo build","cwd":"/repo","status":"failed","exitCode":101});
    let items = parse_item(&reported, true, "turn").unwrap();
    let ItemContent::Command { status, .. } = &items[0].content else {
        panic!("A Codex commandExecution must map to a command");
    };
    assert_eq!(
        *status,
        ToolStatus::Failed,
        "what Codex itself calls a failure is still a failure"
    );
}

/// A model listing that comes in pages is reported once it is whole: a
/// model on a later page is one Codex offers, not one it lacks.
#[test]
fn a_paged_model_listing_is_reported_whole() {
    let mut adapter = adapter();
    let mut listing = Step::default();
    adapter
        .call(
            &mut listing,
            "model/list",
            json!({"limit":100}),
            Pending::Models,
        )
        .unwrap();
    let first = adapter
        .receive(json!({"id":listing.writes[0]["id"],"result":{"data":[{"model":"gpt-a"}],"nextCursor":"more"}}))
        .unwrap();
    assert!(
        !first
            .events
            .iter()
            .any(|event| matches!(event, Event::Models { .. })),
        "part of a listing is not reported"
    );
    assert_eq!(first.writes[0]["params"]["cursor"], "more");
    let last = adapter
        .receive(json!({"id":first.writes[0]["id"],"result":{"data":[{"model":"gpt-b"}]}}))
        .unwrap();
    let reported = last.events.iter().find_map(|event| match event {
        Event::Models { models, .. } => Some(
            models
                .iter()
                .map(|model| model.id.as_str())
                .collect::<Vec<_>>(),
        ),
        _ => None,
    });
    assert_eq!(reported, Some(vec!["gpt-a", "gpt-b"]));

    // A later page that fails leaves the pages that arrived.
    let mut second = self::adapter();
    let mut listing = Step::default();
    second
        .call(
            &mut listing,
            "model/list",
            json!({"limit":100}),
            Pending::Models,
        )
        .unwrap();
    let first = second
        .receive(json!({"id":listing.writes[0]["id"],"result":{"data":[{"model":"gpt-a"}],"nextCursor":"more"}}))
        .unwrap();
    let failed = second
        .receive(json!({"id":first.writes[0]["id"],"error":{"message":"busy"}}))
        .unwrap();
    assert!(failed.events.iter().any(|event| matches!(
        event,
        Event::Models { models, .. } if models.len() == 1 && models[0].id == "gpt-a"
    )));
}

fn active_model_turn_fixture() -> (Codex, Value) {
    let mut adapter = adapter();
    adapter.models.push(Model {
        id: "override-model".into(),
        label: "Override model".into(),
        reasoning: vec!["high".into()],
    });
    adapter.effective_model = Some("model".into());
    adapter.effective_reasoning = Some("medium".into());
    adapter.running_model = adapter.effective_model.clone();
    adapter.running_reasoning = adapter.effective_reasoning.clone();
    adapter
        .command(Command::SetModel {
            model: "override-model".into(),
            reasoning: Some("high".into()),
        })
        .unwrap();
    let sent = adapter
        .command(Command::Prompt {
            text: "Use the override".into(),
            attachments: vec![],
            mode: PromptMode::Send,
        })
        .unwrap();
    let request = sent
        .writes
        .iter()
        .find(|frame| frame["method"] == "turn/start")
        .unwrap()["id"]
        .clone();
    (adapter, request)
}

fn active_model_turn_started(id: &str) -> Value {
    json!({"method":"turn/started","params":{"threadId":"thread-exact",
        "turn":{"id":id,"status":"inProgress","items":[]}}})
}

fn active_model_turn_completed(id: &str) -> Value {
    json!({"method":"turn/completed","params":{"threadId":"thread-exact",
        "turn":{"id":id,"status":"completed","items":[]}}})
}

fn assert_active_model_selection(adapter: &Codex, model: Option<&str>, effort: Option<&str>) {
    let mut step = Step::default();
    adapter.active_model_event(&mut step);
    assert!(
        step.events.iter().any(|event| match event {
            Event::ActiveModel(Some(actual)) =>
                Some(actual.id.as_str()) == model && actual.reasoning.as_deref() == effort,
            Event::ActiveModel(None) => model.is_none() && effort.is_none(),
            _ => false,
        }),
        "wrong active model status: {:?}",
        step.events
    );
}

#[test]
fn active_model_turn_ack_then_started_keeps_both_overrides() {
    let (mut adapter, request) = active_model_turn_fixture();
    adapter
        .receive(json!({"id":request,"result":{"turn":{"id":"chosen"}}}))
        .unwrap();
    assert_active_model_selection(&adapter, Some("override-model"), Some("high"));
    adapter
        .receive(active_model_turn_started("chosen"))
        .unwrap();
    assert_active_model_selection(&adapter, Some("override-model"), Some("high"));
    adapter
        .receive(json!({"method":"thread/settings/updated","params":{
            "threadId":"thread-exact","threadSettings":{"model":"new-default","effort":"low"}
        }}))
        .unwrap();
    adapter
        .receive(active_model_turn_started("chosen"))
        .unwrap();
    assert_active_model_selection(&adapter, Some("override-model"), Some("high"));
}

#[test]
fn active_model_turn_default_reset_sends_null_for_the_next_accepted_turn() {
    let (mut adapter, request) = active_model_turn_fixture();
    adapter
        .receive(json!({"id":request,"result":{"turn":{"id":"high-turn"}}}))
        .unwrap();
    adapter
        .receive(active_model_turn_started("high-turn"))
        .unwrap();
    let cleared = adapter
        .command(Command::SetModel {
            model: "override-model".into(),
            reasoning: None,
        })
        .unwrap();
    assert!(cleared.events.iter().any(|event| matches!(event,
        Event::Models { selected, reasoning: None, .. }
            if selected.as_deref() == Some("override-model"))));
    assert_active_model_selection(&adapter, Some("override-model"), Some("high"));
    adapter
        .receive(active_model_turn_completed("high-turn"))
        .unwrap();
    let sent = adapter
        .command(Command::Prompt {
            text: "Use the default effort".into(),
            attachments: vec![],
            mode: PromptMode::Send,
        })
        .unwrap();
    let request = sent
        .writes
        .iter()
        .find(|frame| frame["method"] == "turn/start")
        .unwrap();
    assert_eq!(request["params"]["model"], "override-model");
    assert_eq!(request["params"].get("effort"), Some(&Value::Null));
    assert_active_model_selection(&adapter, Some("override-model"), Some("high"));
    let accepted = adapter
        .receive(json!({"id":request["id"],"result":{"turn":{"id":"default-turn"}}}))
        .unwrap();
    assert!(accepted.events.iter().any(|event| matches!(event,
        Event::ActiveModel(Some(model))
            if model.id == "override-model" && model.reasoning.is_none())));
    adapter
        .receive(active_model_turn_started("default-turn"))
        .unwrap();
    assert_active_model_selection(&adapter, Some("override-model"), None);
    assert!(adapter.reasoning.is_none());
}

#[test]
fn active_model_turn_started_then_ack_does_not_guess_the_pending_selection() {
    let (mut adapter, request) = active_model_turn_fixture();
    adapter
        .receive(active_model_turn_started("chosen"))
        .unwrap();
    assert_active_model_selection(&adapter, None, None);
    assert!(
        adapter
            .turn_pricing
            .iter()
            .any(|(turn, model, _)| turn == "chosen" && model.as_deref() == Some("override-model"))
    );
    adapter
        .receive(json!({"id":request,"result":{"turn":{"id":"chosen"}}}))
        .unwrap();
    assert_active_model_selection(&adapter, Some("override-model"), Some("high"));
    adapter
        .receive(active_model_turn_started("chosen"))
        .unwrap();
    assert_active_model_selection(&adapter, Some("override-model"), Some("high"));
}

#[test]
fn active_model_turn_unrelated_start_and_late_ack_cannot_borrow_overrides() {
    let (mut adapter, request) = active_model_turn_fixture();
    adapter
        .receive(active_model_turn_started("foreign"))
        .unwrap();
    assert_active_model_selection(&adapter, None, None);
    let late = adapter
        .receive(json!({"id":request,"result":{"turn":{"id":"requested"}}}))
        .unwrap();
    assert!(late.suppress_current_notification);
    assert_active_model_selection(&adapter, None, None);
    assert_eq!(adapter.notification_turn.as_deref(), Some("foreign"));
    adapter
        .receive(active_model_turn_completed("foreign"))
        .unwrap();
    adapter
        .receive(active_model_turn_started("genuinely-new"))
        .unwrap();
    assert_active_model_selection(&adapter, Some("model"), Some("medium"));
    adapter
        .receive(active_model_turn_started("requested"))
        .unwrap();
    assert_active_model_selection(&adapter, Some("model"), Some("medium"));
    assert_eq!(adapter.notification_turn.as_deref(), Some("genuinely-new"));
}

#[test]
fn active_model_turn_old_duplicate_cannot_replace_a_newer_turn() {
    let (mut adapter, request) = active_model_turn_fixture();
    adapter
        .receive(json!({"id":request,"result":{"turn":{"id":"chosen"}}}))
        .unwrap();
    adapter
        .receive(active_model_turn_started("chosen"))
        .unwrap();
    adapter
        .receive(active_model_turn_completed("chosen"))
        .unwrap();
    adapter.receive(active_model_turn_started("newer")).unwrap();
    assert_active_model_selection(&adapter, Some("model"), Some("medium"));
    adapter
        .receive(active_model_turn_started("chosen"))
        .unwrap();
    assert_active_model_selection(&adapter, Some("model"), Some("medium"));
    assert_eq!(adapter.notification_turn.as_deref(), Some("newer"));
}

#[test]
fn active_model_turn_completed_before_ack_is_confirmed_without_restarting() {
    let (mut adapter, request) = active_model_turn_fixture();
    adapter
        .receive(active_model_turn_started("chosen"))
        .unwrap();
    adapter
        .receive(active_model_turn_completed("chosen"))
        .unwrap();
    assert_active_model_selection(&adapter, None, None);
    adapter
        .receive(json!({"id":request,"result":{"turn":{"id":"chosen"}}}))
        .unwrap();
    assert_active_model_selection(&adapter, Some("override-model"), Some("high"));
    assert!(!adapter.busy);
    assert!(adapter.turn.is_none());
    assert!(adapter.notification_turn.is_none());
    let duplicate = adapter
        .receive(active_model_turn_started("chosen"))
        .unwrap();
    assert!(duplicate.suppress_current_notification);
    assert!(!adapter.busy);
}

#[test]
fn active_model_turn_settled_duplicate_cannot_claim_a_new_scalar_lifecycle() {
    let (mut adapter, request) = active_model_turn_fixture();
    adapter
        .receive(json!({"id":request,"result":{"turn":{"id":"chosen"}}}))
        .unwrap();
    adapter
        .receive(active_model_turn_completed("chosen"))
        .unwrap();
    adapter
        .receive(json!({"method":"thread/status/changed","params":{
            "threadId":"thread-exact","status":{"type":"active","activeFlags":[]}
        }}))
        .unwrap();
    assert!(adapter.busy);
    let lifecycle = adapter.lifecycle_revision;
    let notifications = adapter.notification_revision;
    let stale = adapter
        .receive(active_model_turn_started("chosen"))
        .unwrap();
    assert!(stale.suppress_current_notification);
    assert_eq!(adapter.lifecycle_revision, lifecycle);
    assert_eq!(adapter.notification_revision, notifications);
    assert!(adapter.notification_turn.is_none());
    assert!(adapter.turn.is_none());
    assert_active_model_selection(&adapter, Some("override-model"), Some("high"));
}

#[test]
fn active_model_stays_visible_when_a_different_model_is_staged() {
    let mut adapter = adapter();
    adapter.models.push(Model {
        id: "next-model".into(),
        label: "Next model".into(),
        reasoning: vec!["high".into()],
    });
    adapter.effective_model = Some("model".into());
    adapter.effective_reasoning = Some("future-effort".into());
    adapter
        .receive(json!({"method":"turn/started","params":{
            "threadId":"thread-exact","turn":{"id":"active","status":"inProgress","items":[]}
        }}))
        .unwrap();
    let change = adapter
        .command(Command::SetModel {
            model: "next-model".into(),
            reasoning: Some("high".into()),
        })
        .unwrap();
    assert!(change.events.iter().any(|event| matches!(event,
        Event::ActiveModel(Some(model))
            if model.id == "model" && model.reasoning.as_deref() == Some("future-effort"))));
    assert_eq!(adapter.selected.as_deref(), Some("next-model"));
    let completed = adapter
        .receive(json!({"method":"turn/completed","params":{
            "threadId":"thread-exact","turn":{"id":"active","status":"completed","items":[]}
        }}))
        .unwrap();
    assert!(completed.events.iter().any(|event| matches!(event,
        Event::ActiveModel(Some(model)) if model.id == "model")));
    assert_eq!(adapter.effective_model.as_deref(), Some("model"));
    let sent = adapter
        .command(Command::Prompt {
            text: "next turn".into(),
            attachments: vec![],
            mode: PromptMode::Send,
        })
        .unwrap();
    let started = adapter
        .receive(json!({"id":sent.writes[0]["id"],"result":{
            "turn":{"id":"next","status":"inProgress","items":[]}
        }}))
        .unwrap();
    assert!(started.events.iter().any(|event| matches!(event,
        Event::ActiveModel(Some(model))
            if model.id == "next-model" && model.reasoning.as_deref() == Some("high"))));
    assert_eq!(adapter.running_model.as_deref(), Some("next-model"));
}

#[test]
fn active_model_does_not_change_when_thread_defaults_change_during_a_turn() {
    let mut adapter = adapter();
    adapter.effective_model = Some("model".into());
    adapter.effective_reasoning = Some("future-effort".into());
    adapter
        .receive(json!({"method":"turn/started","params":{
            "threadId":"thread-exact","turn":{"id":"active","status":"inProgress","items":[]}
        }}))
        .unwrap();
    let change = adapter
        .receive(json!({"method":"thread/settings/updated","params":{
            "threadId":"thread-exact","threadSettings":{"model":"next-model","effort":"high"}
        }}))
        .unwrap();
    assert_eq!(adapter.selected.as_deref(), Some("next-model"));
    assert_eq!(adapter.effective_model.as_deref(), Some("next-model"));
    assert!(change.events.iter().any(|event| matches!(event,
        Event::ActiveModel(Some(model)) if model.id == "model"
            && model.reasoning.as_deref() == Some("future-effort"))));
    adapter
        .receive(json!({"method":"turn/completed","params":{
            "threadId":"thread-exact","turn":{"id":"active","status":"completed","items":[]}
        }}))
        .unwrap();
    let started = adapter
        .receive(json!({"method":"turn/started","params":{
            "threadId":"thread-exact","turn":{"id":"next","status":"inProgress","items":[]}
        }}))
        .unwrap();
    assert!(started.events.iter().any(|event| matches!(event,
        Event::ActiveModel(Some(model)) if model.id == "next-model"
            && model.reasoning.as_deref() == Some("high"))));
}

#[test]
fn catalog_default_reasoning_is_reported_separately_from_selected_effort() {
    let mut adapter = adapter();
    let mut listing = Step::default();
    adapter
        .call(
            &mut listing,
            "model/list",
            json!({"limit":100}),
            Pending::Models,
        )
        .unwrap();
    adapter.reasoning = Some("medium".into());
    adapter.effective_model = Some("model".into());
    adapter.effective_reasoning = Some("medium".into());
    adapter.running_model = adapter.effective_model.clone();
    adapter.running_reasoning = adapter.effective_reasoning.clone();
    let catalog = adapter.receive(json!({"id":listing.writes[0]["id"],"result":{
        "data":[{"model":"model","displayName":"Model", "defaultReasoningEffort":"medium",
            "supportedReasoningEfforts":[{"reasoningEffort":"medium"},{"reasoningEffort":"high"}]}],
        "nextCursor":null
    }})).unwrap();
    assert!(catalog.events.iter().any(|event| matches!(event,
        Event::ActiveModel(Some(model)) if model.id == "model"
            && model.reasoning.as_deref() == Some("medium")
            && model.default_reasoning.as_deref() == Some("medium"))));
    assert_eq!(adapter.reasoning.as_deref(), Some("medium"));
    let high = adapter
        .command(Command::SetModel {
            model: "model".into(),
            reasoning: Some("high".into()),
        })
        .unwrap();
    assert!(high.events.iter().any(|event| matches!(event,
        Event::Models {reasoning: Some(effort), ..} if effort == "high")));
}

#[test]
fn changing_model_never_sends_control_to_a_running_turn() {
    let mut adapter = adapter();
    adapter.busy = true;
    adapter.turn = Some("active".into());
    let change = adapter
        .command(Command::SetModel {
            model: "model".into(),
            reasoning: Some("future-effort".into()),
        })
        .unwrap();
    assert!(change.writes.is_empty());
    let steer = adapter
        .command(Command::Prompt {
            text: "follow up".into(),
            attachments: vec![],
            mode: PromptMode::Steer,
        })
        .unwrap();
    assert_eq!(
        steer.writes[0]["params"],
        json!({"threadId":"thread-exact","expectedTurnId":"active","input":[{"type":"text","text":"follow up"}]})
    );
    assert!(
        adapter
            .command(Command::Prompt {
                text: "new".into(),
                attachments: vec![],
                mode: PromptMode::Send
            })
            .is_err()
    );
}

#[test]
fn question_and_elicitation_kinds_preserve_their_exact_reply_contracts() {
    let mut adapter = adapter();
    let questions = adapter.receive(json!({"id":"question-kind","method":"item/tool/requestUserInput","params":{"threadId":"thread-exact","turnId":"turn","itemId":"question","questions":[{"id":"scope","header":"Scope","question":"Which?","isOther":false,"options":[{"label":"One"}]}]}})).unwrap();
    assert!(
        matches!(&questions.events[0], Event::Request(request) if request.kind == chat::RequestKind::Question && request.actions[0].id == "submit")
    );
    let submitted = adapter
        .command(Command::Answer {
            request_id: rpc_id(&json!("question-kind")).unwrap(),
            action: "submit".into(),
            answers: vec![Answer {
                field: "scope".into(),
                values: vec!["One".into()],
            }],
        })
        .unwrap();
    assert_eq!(
        submitted.writes[0]["result"],
        json!({"answers":{"scope":{"answers":["One"]}}})
    );
    for (id, action) in [("consent-decline", "decline"), ("consent-cancel", "cancel")] {
        let consent = adapter.receive(json!({"id":id,"method":"mcpServer/elicitation/request","params":{"threadId":"thread-exact","turnId":"turn","mode":"form","message":"Supply input","requestedSchema":{"type":"object","properties":{"name":{"type":"string"}},"required":["name"]}}})).unwrap();
        assert!(consent.events.iter().any(|event| matches!(event, Event::Request(request) if request.kind == chat::RequestKind::Consent && !request.fields.is_empty())));
        let response = adapter
            .command(Command::Answer {
                request_id: rpc_id(&json!(id)).unwrap(),
                action: action.into(),
                answers: vec![],
            })
            .unwrap();
        assert_eq!(response.writes[0]["result"], json!({"action":action}));
    }
}

#[test]
fn optional_questions_survive_turn_completion_until_resolved() {
    let mut adapter = adapter();
    adapter.receive(json!({"id":"q","method":"item/tool/requestUserInput","params":{"threadId":"thread-exact","turnId":"turn","itemId":"question","isBlocking":false,"questions":[{"id":"scope","header":"Scope","question":"Which?","isOther":false,"options":[{"label":"One","description":"One file"}]}]}})).unwrap();
    adapter.receive(json!({"method":"turn/completed","params":{"threadId":"thread-exact","turn":{"id":"turn","status":"completed","items":[]}}})).unwrap();
    let invalid = adapter.command(Command::Answer {
        request_id: rpc_id(&json!("q")).unwrap(),
        action: "submit".into(),
        answers: vec![Answer {
            field: "scope".into(),
            values: vec!["All".into()],
        }],
    });
    assert!(invalid.is_err());
    let answer = adapter
        .command(Command::Answer {
            request_id: rpc_id(&json!("q")).unwrap(),
            action: "submit".into(),
            answers: vec![Answer {
                field: "scope".into(),
                values: vec!["One".into()],
            }],
        })
        .unwrap();
    assert_eq!(
        answer.writes[0]["result"],
        json!({"answers":{"scope":{"answers":["One"]}}})
    );
}

/// Compaction's only visible effect is the context figure the reader
/// pressed the control to move. Nothing on the wire announces that it
/// moved, so the acknowledgement has to re-read the thread; without this
/// the gauge kept its old reading until some later frame happened to
/// carry a new one.
#[test]
fn a_compaction_acknowledgement_re_reads_the_thread() {
    let mut adapter = adapter();
    let requested = adapter.command(Command::Compact).unwrap();
    let start = requested
        .writes
        .iter()
        .find(|frame| frame["method"] == "thread/compact/start")
        .expect("compaction is requested");
    let acknowledged = adapter
        .receive(json!({"id":start["id"],"result":{}}))
        .unwrap();
    let methods = acknowledged
        .writes
        .iter()
        .map(|frame| frame["method"].as_str().unwrap_or_default().to_owned())
        .collect::<Vec<_>>();
    assert!(
        methods.contains(&"thread/read".to_owned()),
        "the compacted thread's state is never asked for: {methods:?}"
    );
    assert!(
        methods.contains(&"thread/turns/list".to_owned()),
        "the rewritten history is never asked for: {methods:?}"
    );
}

/// A plan reaches the surface as the steps it is made of. It used to
/// arrive as a transcript message holding the raw JSON of the payload,
/// which is the same information in the one shape nobody can read.
#[test]
fn a_published_plan_arrives_as_its_steps() {
    let mut adapter = adapter();
    let step = adapter
        .receive(json!({"method":"turn/plan/updated","params":{"threadId":"thread-exact","turnId":"turn","plan":{
            "explanation":"why",
            "plan":[
                {"step":"Read the view","status":"completed"},
                {"step":"Split the status line","status":"in_progress"},
                {"step":"   ","status":"pending"},
                {"step":"Pin the plan","status":"pending"}]}}}))
        .unwrap();
    let steps = step
        .events
        .iter()
        .find_map(|event| match event {
            Event::Plan { steps } => Some(steps.clone()),
            _ => None,
        })
        .expect("the plan reaches the surface");
    assert_eq!(
        steps,
        vec![
            PlanStep {
                text: "Read the view".into(),
                status: PlanState::Done
            },
            PlanStep {
                text: "Split the status line".into(),
                status: PlanState::Active
            },
            PlanStep {
                text: "Pin the plan".into(),
                status: PlanState::Pending
            },
        ],
        "a step with no text of its own would pin a blank row"
    );
    assert!(
        !step
            .events
            .iter()
            .any(|event| matches!(event, Event::Item(_))),
        "the raw payload still reaches the transcript"
    );
}
#[test]
fn resume_login_verifies_same_account_without_replaying_or_resetting_thread() {
    let mut adapter = adapter();
    let failure = "Your access token could not be refreshed. Please sign in again.";
    let expected = crate::account::Identity {
        email: Some("expected@example.test".into()),
        organization: None,
        plan: None,
        disputed_by: None,
    };
    let request = adapter
        .command(Command::ResumeAfterLogin {
            failure: failure.into(),
            expected: Some(expected.clone()),
        })
        .unwrap();
    assert_eq!(request.writes.len(), 1);
    assert_eq!(request.writes[0]["method"], "account/read");
    assert_eq!(request.writes[0]["params"], json!({"refreshToken":true}));
    assert!(request.events.is_empty());
    assert!(
        adapter
            .command(Command::ResumeAfterLogin {
                failure: failure.into(),
                expected: None
            })
            .is_err()
    );
    let result = adapter.receive(json!({"id":request.writes[0]["id"],"result":{"account":{"type":"chatgpt","email":"expected@example.test","planType":"pro"},"requiresOpenaiAuth":true}})).unwrap();
    assert_eq!(
        result.events,
        vec![Event::LoginRecovered {
            failure: failure.into()
        }]
    );
    assert!(result.writes.is_empty());
    assert_eq!(adapter.thread.as_deref(), Some("thread-exact"));
    for response in [
        json!({"account":null,"requiresOpenaiAuth":true}),
        json!({"account":{"type":"chatgpt","email":"different@example.test"}}),
        json!({"account":{"type":"apiKey"},"requiresOpenaiAuth":false}),
    ] {
        let request = adapter
            .command(Command::ResumeAfterLogin {
                failure: failure.into(),
                expected: Some(expected.clone()),
            })
            .unwrap();
        let result = adapter
            .receive(json!({"id":request.writes[0]["id"],"result":response}))
            .unwrap();
        assert!(
            matches!(&result.events[..], [Event::LoginVerificationFailed {failure: captured,..}] if captured == failure)
        );
        assert!(result.writes.is_empty());
    }
    let request = adapter
        .command(Command::ResumeAfterLogin {
            failure: failure.into(),
            expected: None,
        })
        .unwrap();
    let failed = adapter.receive(json!({"id":request.writes[0]["id"],"error":{"code":-32000,"message":"refresh token permanently invalid"}})).unwrap();
    assert!(
        matches!(&failed.events[..], [Event::LoginVerificationFailed {failure: captured,detail}] if captured == failure && detail.contains("verification failed"))
    );
    let request = adapter
        .command(Command::ResumeAfterLogin {
            failure: failure.into(),
            expected: None,
        })
        .unwrap();
    adapter.lifecycle_revision += 1;
    let late = adapter.receive(json!({"id":request.writes[0]["id"],"result":{"account":{"type":"chatgpt","email":"expected@example.test"}}})).unwrap();
    assert!(late.events.is_empty());
}
#[test]
fn stale_login_verification_rpc_failure_emits_no_new_error() {
    let mut adapter = adapter();
    let request = adapter
        .command(Command::ResumeAfterLogin {
            failure: "Authentication required. Please sign in again.".into(),
            expected: None,
        })
        .unwrap();
    adapter.lifecycle_revision += 1;
    let late = adapter.receive(json!({"id":request.writes[0]["id"],"error":{"code":-32000,"message":"old refresh failed"}})).unwrap();
    assert!(late.events.is_empty());
}
