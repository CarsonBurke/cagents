//! A prompt for an agent busy with a turn no run owns waits for that turn, and
//! the owner can reach that turn. The turn here is a real private provider
//! process held in place until its fixture releases it.
use super::*;

const HOLD: &str = "HOLD_CLEANUP_BUSY";

struct Busy {
    host: SharedHost,
    owner: Authority,
    agent: AgentId,
    handle: chat::runtime::Handle,
    root: std::path::PathBuf,
}

impl Busy {
    /// An agent whose first run succeeded, now in a turn that run didn't start.
    fn held() -> Self {
        let this = Self::idle();
        this.hold();
        this
    }

    fn hold(&self) {
        let _ = std::fs::remove_file(self.root.join("cleanup-release"));
        let _ = std::fs::remove_file(self.root.join("cleanup-busy"));
        self.handle.try_idle_prompt(HOLD.into()).unwrap();
        wait_for("the held turn", || self.root.join("cleanup-busy").exists());
    }

    fn idle() -> Self {
        Self::started(|_| {})
    }

    fn started(prepare: impl FnOnce(&SharedHost)) -> Self {
        let (host, request, path) = fixture();
        prepare(&host);
        let root = path.parent().unwrap().to_owned();
        let provider = root.join("bin/claude");
        let script = std::fs::read_to_string(&provider).unwrap();
        let reply = r#"      printf '{"type":"control_response","response":{"subtype":"success","request_id":"%s","response":{}}}\n' "$id""#;
        let recording = format!(
            "      case \"$frame\" in *'\"subtype\":\"interrupt\"'*) printf 'interrupt\\n' >> \"$HOME/../interrupts\" ;; esac\n{reply}"
        );
        assert_eq!(script.matches(reply).count(), 1);
        let held = "            [ \"$attempts\" -lt 1000 ] || exit 1\n";
        assert_eq!(script.matches(held).count(), 1);
        let script = script.replace(reply, &recording).replace(
            held,
            "            [ ! -e \"$HOME/../provider-exit\" ] || exit 1\n            [ \"$attempts\" -lt 3000 ] || exit 1\n",
        );
        std::fs::write(provider, script).unwrap();
        let owner = Authority::default();
        let accepted = submit(&host, &owner, request).unwrap();
        assert_eq!(settled(&host, accepted.run.id).state, State::Succeeded);
        let agent = accepted.agent.id;
        let session = registry(&host)
            .unwrap()
            .lock()
            .unwrap()
            .agent(agent)
            .unwrap()
            .session
            .unwrap()
            .id();
        let handle = host.lock().unwrap().chats[&session].handle.clone();
        Self {
            host,
            owner,
            agent,
            handle,
            root,
        }
    }

    fn send(&self, key: &str) -> Result<Accepted, String> {
        submit(
            &self.host,
            &self.owner,
            Submit {
                name: None,
                destination: Destination::Existing(self.agent),
                prompt: format!("Prompt {key}"),
                idempotency_key: key.into(),
            },
        )
    }

    fn queued(&self, key: &str) -> Run {
        let accepted = self.send(key).unwrap();
        wait_for("the prompt to queue", || {
            self.run(accepted.run.id).queued_ms.is_some()
        });
        let run = self.run(accepted.run.id);
        assert_eq!(run.state, State::Accepted);
        assert_eq!(run.started_ms, None);
        run
    }

    fn run(&self, id: RunId) -> Run {
        registry(&self.host)
            .unwrap()
            .lock()
            .unwrap()
            .agent_run(id)
            .unwrap()
    }

    fn interrupt(&self, including_subagents: bool) -> Result<(), String> {
        control(
            &self.host,
            &self.owner,
            self.agent,
            Control::Interrupt {
                including_subagents,
            },
        )
        .map(|_| ())
    }

    /// What the interrupt reported having done.
    fn interrupt_reply(&self) -> Result<(bool, Option<RunId>), String> {
        match control(
            &self.host,
            &self.owner,
            self.agent,
            Control::Interrupt {
                including_subagents: false,
            },
        )? {
            Reply::Interrupted {
                interrupted,
                withdrawn,
                ..
            } => Ok((interrupted, withdrawn)),
            reply => panic!("{reply:?}"),
        }
    }

    fn withdraw_reply(&self) -> Result<(bool, Option<RunId>), String> {
        match control(&self.host, &self.owner, self.agent, Control::WithdrawQueued)? {
            Reply::Interrupted {
                interrupted,
                withdrawn,
                ..
            } => Ok((interrupted, withdrawn)),
            reply => panic!("{reply:?}"),
        }
    }

    fn release(&self) {
        std::fs::write(self.root.join("cleanup-release"), "").unwrap();
    }

    fn count(&self, file: &str) -> usize {
        std::fs::read_to_string(self.root.join(file)).map_or(0, |text| text.lines().count())
    }
}

fn wait_for(what: &str, mut ready: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !ready() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn settled_run(host: &SharedHost, id: RunId) -> Run {
    let mut run = None;
    wait_for("the run to settle", || {
        let current = registry(host)
            .unwrap()
            .lock()
            .unwrap()
            .agent_run(id)
            .unwrap();
        let done = current.state.is_terminal();
        run = Some(current);
        done
    });
    run.unwrap()
}

#[test]
fn a_prompt_sent_during_another_turn_waits_and_then_runs_once() {
    if isolated("host::agent_execution::tests::queued_prompt::a_prompt_sent_during_another_turn_waits_and_then_runs_once").is_some() {
        return;
    }
    let busy = Busy::held();
    let queued = busy.queued("second");
    assert_eq!(busy.count("prompts"), 2, "the queued prompt was not sent");
    // A turn can last hours; update and stop need the gate meanwhile.
    let gate = busy.host.lock().unwrap().control_gate.clone();
    assert!(
        gate.try_write().is_ok(),
        "a queued prompt held the control gate"
    );

    // Interrupting would stop the turn the queued prompt waits for, so the
    // refusal points to withdrawing only the prompt.
    let another = busy.send("third").unwrap_err();
    assert!(another.contains("already has a prompt queued"), "{another}");
    assert!(another.contains("agent wait"), "{another}");
    assert!(another.contains("--prompt-only"), "{another}");

    busy.release();
    let run = settled_run(&busy.host, queued.id);
    assert_eq!(run.state, State::Succeeded, "{:?}", run.result.diagnostic);
    assert_eq!(run.queued_ms, None);
    assert!(run.started_ms.is_some());
    assert_eq!(busy.count("prompts"), 3);
}

#[test]
fn an_interrupt_stops_the_turn_and_withdraws_the_prompt_queued_behind_it() {
    if isolated("host::agent_execution::tests::queued_prompt::an_interrupt_stops_the_turn_and_withdraws_the_prompt_queued_behind_it").is_some() {
        return;
    }
    let busy = Busy::held();
    let queued = busy.queued("second");
    busy.interrupt(false).unwrap();
    let run = settled_run(&busy.host, queued.id);
    assert_eq!(run.state, State::Cancelled);
    assert_eq!(run.queued_ms, None);
    assert!(
        run.result
            .diagnostic
            .as_deref()
            .unwrap()
            .contains("not sent"),
        "{:?}",
        run.result.diagnostic
    );
    busy.release();
    wait_for("the interrupt to reach the provider", || {
        busy.count("interrupts") == 1
    });
    wait_for("the turn to end", || busy.handle.is_idle());
    assert_eq!(busy.count("prompts"), 2, "the withdrawn prompt never went");

    // The agent is usable afterwards; nothing holds it for inspection.
    let next = busy.send("third").unwrap();
    assert_eq!(settled(&busy.host, next.run.id).state, State::Succeeded);
}

#[test]
fn an_owner_interrupts_a_turn_no_run_started_and_is_told_when_nothing_is_running() {
    if isolated("host::agent_execution::tests::queued_prompt::an_owner_interrupts_a_turn_no_run_started_and_is_told_when_nothing_is_running").is_some() {
        return;
    }
    let busy = Busy::idle();
    let idle = busy.interrupt(false).unwrap_err();
    assert!(idle.contains("nothing to interrupt"), "{idle}");
    assert!(idle.contains("Succeeded"), "{idle}");

    busy.handle.try_idle_prompt(HOLD.into()).unwrap();
    wait_for("the held turn", || busy.root.join("cleanup-busy").exists());
    busy.interrupt(false).unwrap();
    busy.release();
    wait_for("the interrupt to reach the provider", || {
        busy.count("interrupts") == 1
    });
    assert!(
        !registry(&busy.host)
            .unwrap()
            .lock()
            .unwrap()
            .agent_is_interactive(busy.agent)
            .unwrap(),
        "the owner keeps the agent"
    );
}

#[test]
fn background_work_needs_explicit_permission_and_a_refusal_keeps_the_queued_prompt() {
    if isolated("host::agent_execution::tests::queued_prompt::background_work_needs_explicit_permission_and_a_refusal_keeps_the_queued_prompt").is_some() {
        return;
    }
    let busy = Busy::idle();
    // The fixture's turn launches a background subagent and holds until released.
    busy.handle
        .try_idle_prompt("COURIER_BACKGROUND".into())
        .unwrap();
    wait_for("the background agent", || {
        !busy.handle.background_subagents().is_empty()
    });
    let queued = busy.queued("second");
    let refused = busy.interrupt(false).unwrap_err();
    assert!(refused.contains("--including-subagents"), "{refused}");
    assert!(
        busy.run(queued.id).queued_ms.is_some(),
        "a refused interrupt withdrew nothing"
    );

    // The main turn ends and the subagent runs on: the prompt goes, and with
    // the run settled a person or parent can still stop what is left.
    std::fs::write(busy.root.join("courier-release"), "").unwrap();
    let run = settled_run(&busy.host, queued.id);
    assert_eq!(run.state, State::Succeeded, "{:?}", run.result.diagnostic);
    assert_eq!(busy.handle.background_subagents().len(), 1);
    let refused = busy.interrupt(false).unwrap_err();
    assert!(refused.contains("--including-subagents"), "{refused}");
    assert_eq!(busy.count("interrupts"), 0);
    busy.interrupt(true).unwrap();
    wait_for("the interrupt to reach the provider", || {
        busy.count("interrupts") == 1
    });
}

/// Another turn can take the provider between the wait ending and the prompt
/// being sent. The actor refuses the prompt unsent, so it goes back to waiting.
/// The agent's gate holds the dispatcher in that gap.
#[test]
fn a_prompt_that_loses_the_provider_to_another_turn_waits_again() {
    if isolated(
        "host::agent_execution::tests::queued_prompt::a_prompt_that_loses_the_provider_to_another_turn_waits_again",
    )
    .is_some()
    {
        return;
    }
    let busy = Busy::held();
    let queued = busy.queued("second");
    let gate = agent_gate(&busy.host, busy.agent);
    let holding = gate.lock().unwrap();
    busy.release();
    wait_for("the turn to end", || busy.handle.is_idle());
    // Longer than the queue's poll, so the dispatcher sees idle and reaches the
    // gate. If it doesn't get there in time, the prompt just keeps waiting.
    std::thread::sleep(Duration::from_millis(500));
    busy.hold();
    drop(holding);

    wait_for("the prompt to wait again", || {
        busy.run(queued.id).queued_ms.is_some()
    });
    assert_eq!(busy.count("prompts"), 3, "the queued prompt was not sent");
    busy.release();
    let run = settled_run(&busy.host, queued.id);
    assert_eq!(run.state, State::Succeeded, "{:?}", run.result.diagnostic);
    assert_eq!(busy.count("prompts"), 4);
}

/// A queued prompt authorizes nothing yet, so an approval can't borrow it.
#[test]
fn a_queued_prompt_lets_the_owner_decline_a_request_but_not_approve_it() {
    if isolated(
        "host::agent_execution::tests::queued_prompt::a_queued_prompt_lets_the_owner_decline_a_request_but_not_approve_it",
    )
    .is_some()
    {
        return;
    }
    let busy = Busy::idle();
    busy.handle
        .try_idle_prompt("BACKGROUND_PERMISSION while the turn ends".into())
        .unwrap();
    let asked = pending_requests(&busy.host, busy.agent);
    let [asked] = asked.as_slice() else {
        panic!("{asked:?}")
    };
    let queued = busy.queued("second");
    let answer = |action: &str| Control::Answer {
        request_id: asked.id.clone(),
        action: action.into(),
        answers: Vec::new(),
    };
    let error = control(&busy.host, &busy.owner, busy.agent, answer("allow")).unwrap_err();
    assert!(error.contains("only decline"), "{error}");
    assert!(
        !busy.root.join("decisions").exists(),
        "nothing was approved"
    );
    control(&busy.host, &busy.owner, busy.agent, answer("deny")).unwrap();
    wait_for("the denial to reach the provider", || {
        std::fs::read_to_string(busy.root.join("decisions")).unwrap_or_default() == "deny\n"
    });
    let run = settled_run(&busy.host, queued.id);
    assert_eq!(run.state, State::Succeeded, "{:?}", run.result.diagnostic);
}

/// Stopping from the agent's console means the same as stopping from its chat.
#[test]
fn a_person_stopping_from_the_console_withdraws_the_queued_prompt() {
    if isolated(
        "host::agent_execution::tests::queued_prompt::a_person_stopping_from_the_console_withdraws_the_queued_prompt",
    )
    .is_some()
    {
        return;
    }
    let busy = Busy::held();
    let queued = busy.queued("behind-console");
    let store = registry(&busy.host).unwrap();
    super::super::stop_for_person(&busy.host, &store, &busy.handle, busy.agent, false).unwrap();
    assert_eq!(settled_run(&busy.host, queued.id).state, State::Cancelled);
    assert!(
        store
            .lock()
            .unwrap()
            .agent_is_interactive(busy.agent)
            .unwrap()
    );
    busy.release();
    wait_for("the interrupt to reach the provider", || {
        busy.count("interrupts") == 1
    });
    wait_for("the turn to end", || busy.handle.is_idle());
    assert_eq!(busy.count("prompts"), 2, "the withdrawn prompt never went");
}

#[test]
fn a_queued_prompt_ends_truthfully_when_its_provider_exits() {
    if isolated("host::agent_execution::tests::queued_prompt::a_queued_prompt_ends_truthfully_when_its_provider_exits").is_some() {
        return;
    }
    let busy = Busy::held();
    let queued = busy.queued("second");
    std::fs::write(busy.root.join("provider-exit"), "").unwrap();
    let run = settled_run(&busy.host, queued.id);
    assert_eq!(run.state, State::Failed);
    assert_eq!(run.queued_ms, None);
    assert!(
        run.result
            .diagnostic
            .as_deref()
            .unwrap()
            .contains("It was not sent"),
        "{:?}",
        run.result.diagnostic
    );
    assert_eq!(busy.count("prompts"), 2);
}

#[test]
fn closing_an_agent_cancels_its_queued_prompt_unsent() {
    if isolated("host::agent_execution::tests::queued_prompt::closing_an_agent_cancels_its_queued_prompt_unsent").is_some() {
        return;
    }
    let busy = Busy::held();
    let queued = busy.queued("second");
    close(&busy.host, &busy.owner, busy.agent).unwrap();
    let run = settled_run(&busy.host, queued.id);
    assert_eq!(run.state, State::Cancelled);
    assert!(
        run.result
            .diagnostic
            .as_deref()
            .unwrap()
            .contains("not sent"),
        "{:?}",
        run.result.diagnostic
    );
    assert_eq!(busy.count("prompts"), 2);
}

#[test]
fn a_restart_reports_a_queued_prompt_as_unsent_and_leaves_the_agent_usable() {
    if isolated("host::agent_execution::tests::queued_prompt::a_restart_reports_a_queued_prompt_as_unsent_and_leaves_the_agent_usable").is_some() {
        return;
    }
    let (host, request, _) = fixture();
    let registry = registry(&host).unwrap();
    let mut registry = registry.lock().unwrap();
    let accepted = registry.submit_agent(&Origin::User, &request, 1).unwrap();
    let queued = registry
        .update_agent_run(accepted.run.id, accepted.run.revision, Update::Queued, 2)
        .unwrap();
    assert_eq!(queued.queued_ms, Some(2));
    registry
        .interrupt_unproven_agent_runs_except(3, &[])
        .unwrap();
    let run = registry.agent_run(accepted.run.id).unwrap();
    assert_eq!(run.state, State::Failed, "nothing about it is uncertain");
    assert_eq!(run.queued_ms, None);
    assert!(run.result.diagnostic.unwrap().contains("not sent"));

    // Dequeued is committed first, so a prompt mid-handover stays uncertain.
    let followup = Submit {
        name: None,
        destination: Destination::Existing(accepted.agent.id),
        prompt: "Follow up".into(),
        idempotency_key: "followup".into(),
    };
    let accepted = registry.submit_agent(&Origin::User, &followup, 4).unwrap();
    let queued = registry
        .update_agent_run(accepted.run.id, accepted.run.revision, Update::Queued, 5)
        .unwrap();
    registry
        .update_agent_run(accepted.run.id, queued.revision, Update::Dequeued, 6)
        .unwrap();
    registry
        .interrupt_unproven_agent_runs_except(7, &[])
        .unwrap();
    let run = registry.agent_run(accepted.run.id).unwrap();
    assert_eq!(run.state, State::Interrupted);
    let stuck = registry
        .submit_agent(
            &Origin::User,
            &Submit {
                idempotency_key: "after-uncertain".into(),
                ..followup
            },
            8,
        )
        .unwrap_err();
    assert!(stuck.contains("uncertain"), "{stuck}");
}

/// The refusal dispatch retries on is the one the actor itself gives, before
/// anything reaches the provider.
#[test]
fn the_actor_refuses_a_run_prompt_in_the_words_dispatch_retries_on() {
    if isolated(
        "host::agent_execution::tests::queued_prompt::the_actor_refuses_a_run_prompt_in_the_words_dispatch_retries_on",
    )
    .is_some()
    {
        return;
    }
    let busy = Busy::held();
    let prompts = busy.count("prompts");
    let refused = busy
        .handle
        .prompt_for_run(RunId::new(), "Second".into())
        .unwrap_err();
    assert!(refused.lost_idle_to_another_turn(), "{refused:?}");
    assert!(refused.proves_unsent(), "{refused:?}");
    assert_eq!(busy.count("prompts"), prompts);
    busy.release();
}

/// The actor reports a failure after it queued a prompt's input the same way
/// it reports a refusal, so only the refusals made before input can say the
/// prompt was not sent.
#[test]
fn only_a_refusal_before_input_proves_a_prompt_unsent() {
    use chat::runtime::Rejection;
    for unsent in [
        "The provider is no longer idle for the accepted run",
        "Chat is closing",
        "Provider process exited; explicitly recover the exact session",
        "Chat command queue unavailable: sending on a full channel",
        crate::chat::failure::delivery::RECONNECTING_INPUT,
        crate::chat::failure::delivery::CROWDED_INPUT,
    ] {
        assert!(
            Rejection::Refused(unsent.into()).proves_unsent(),
            "{unsent}"
        );
    }
    for uncertain in [
        "Provider stdin queue exceeds 64 MiB; no frames discarded",
        "Cannot persist the transcript",
        crate::chat::failure::delivery::UNACKNOWLEDGED_INPUT,
        crate::chat::failure::delivery::STOPPED_INPUT,
    ] {
        assert!(
            !Rejection::Refused(uncertain.into()).proves_unsent(),
            "{uncertain}"
        );
    }
    assert!(!Rejection::Unacknowledged("Chat is closing".into()).proves_unsent());
}

/// A person's Stop in the chat means the same as the owner's interrupt: the
/// prompt behind that turn must not start the moment it ends.
#[test]
fn a_person_stopping_the_turn_withdraws_the_queued_prompt() {
    if isolated(
        "host::agent_execution::tests::queued_prompt::a_person_stopping_the_turn_withdraws_the_queued_prompt",
    )
    .is_some()
    {
        return;
    }
    let busy = Busy::held();
    let queued = busy.queued("behind-person");
    let session = registry(&busy.host)
        .unwrap()
        .lock()
        .unwrap()
        .agent(busy.agent)
        .unwrap()
        .session
        .unwrap()
        .id();
    let client = 7;
    busy.host
        .lock()
        .unwrap()
        .chats
        .get_mut(&session)
        .unwrap()
        .subscribers
        .insert(client, true);
    let prompts = busy.count("prompts");
    let request = crate::terminal_ipc::ClientRequest::ChatCommand {
        chat_id: session,
        command: chat::Command::Interrupt,
    };
    let handle = busy.handle.clone();
    ownership::interactive(&busy.host, client, request, move |_| {
        handle.command(chat::Command::Interrupt).unwrap();
        Ok(crate::terminal_ipc::ServerResponse::Accepted)
    })
    .unwrap();
    assert_eq!(settled_run(&busy.host, queued.id).state, State::Cancelled);
    assert_eq!(
        busy.count("prompts"),
        prompts,
        "the queued prompt was not sent"
    );
}

/// A queued prompt would be lost across a restart, so an update waits for it,
/// and its refusal says how to get it out of the way.
#[test]
fn a_queued_prompt_blocks_an_update_and_says_how_to_withdraw_it() {
    if isolated(
        "host::agent_execution::tests::queued_prompt::a_queued_prompt_blocks_an_update_and_says_how_to_withdraw_it",
    )
    .is_some()
    {
        return;
    }
    let busy = Busy::held();
    let queued = busy.queued("update");
    let registry = registry(&busy.host).unwrap();
    let blockers = || registry.lock().unwrap().agent_update_blockers().unwrap();
    let blocked = blockers();
    assert_eq!(blocked.len(), 1);
    assert!(blocked[0].contains("hasn't been sent"), "{blocked:?}");
    assert!(
        blocked[0].contains("cagents agent interrupt"),
        "{blocked:?}"
    );

    assert_eq!(busy.interrupt_reply(), Ok((true, Some(queued.id))));
    assert_eq!(blockers(), Vec::<String>::new());
    busy.release();
}

#[test]
fn an_owner_withdraws_a_queued_prompt_after_a_person_took_the_turn_over() {
    if isolated(
        "host::agent_execution::tests::queued_prompt::an_owner_withdraws_a_queued_prompt_after_a_person_took_the_turn_over",
    )
    .is_some()
    {
        return;
    }
    let busy = Busy::held();
    let queued = busy.queued("second");
    registry(&busy.host)
        .unwrap()
        .lock()
        .unwrap()
        .claim_agent_interaction(busy.agent, None)
        .unwrap();
    assert_eq!(busy.interrupt_reply(), Ok((false, Some(queued.id))));
    let run = settled_run(&busy.host, queued.id);
    assert_eq!(run.state, State::Cancelled);
    assert!(
        run.result
            .diagnostic
            .as_deref()
            .unwrap()
            .contains("not sent")
    );
    // The turn stays the person's: nothing asked the provider to stop it, and
    // with nothing left to withdraw the owner's interrupt is refused.
    let refused = busy.interrupt_reply().unwrap_err();
    assert!(
        refused.contains("A person took this agent over"),
        "{refused}"
    );
    // Withdrawing again says there is nothing to withdraw, rather than
    // pointing to the command that was just run.
    let refused = busy.withdraw_reply().unwrap_err();
    assert!(refused.contains("no queued prompt"), "{refused}");
    busy.release();
    wait_for("the turn to end", || busy.handle.is_idle());
    assert_eq!(busy.count("interrupts"), 0);
    assert_eq!(busy.count("prompts"), 2, "the withdrawn prompt never went");
}

#[test]
fn an_owner_cannot_withdraw_a_prompt_the_person_who_took_over_sent() {
    if isolated(
        "host::agent_execution::tests::queued_prompt::an_owner_cannot_withdraw_a_prompt_the_person_who_took_over_sent",
    )
    .is_some()
    {
        return;
    }
    let busy = Busy::held();
    let queued = busy.queued("second");
    registry(&busy.host)
        .unwrap()
        .lock()
        .unwrap()
        .claim_agent_interaction(busy.agent, Some(queued.id))
        .unwrap();
    let refused = busy.interrupt_reply().unwrap_err();
    assert!(refused.contains("only they can withdraw it"), "{refused}");
    // Answering the turn's question, as the queue tells them to, is another
    // action of theirs; the prompt they sent stays theirs.
    registry(&busy.host)
        .unwrap()
        .lock()
        .unwrap()
        .claim_agent_interaction(busy.agent, None)
        .unwrap();
    let refused = busy.interrupt_reply().unwrap_err();
    assert!(refused.contains("only they can withdraw it"), "{refused}");
    let refused = busy.withdraw_reply().unwrap_err();
    assert!(refused.contains("only they can withdraw it"), "{refused}");
    let run = registry(&busy.host)
        .unwrap()
        .lock()
        .unwrap()
        .agent_run(queued.id)
        .unwrap();
    assert!(!run.state.is_terminal(), "{run:?}");
    assert!(run.queued_ms.is_some(), "{run:?}");
    busy.release();
    wait_for("the person's prompt to run", || busy.count("prompts") == 2);
}

#[test]
fn withdrawing_only_the_prompt_leaves_the_turn_and_its_background_work_running() {
    if isolated(
        "host::agent_execution::tests::queued_prompt::withdrawing_only_the_prompt_leaves_the_turn_and_its_background_work_running",
    )
    .is_some()
    {
        return;
    }
    let busy = Busy::held();
    let refused = busy.withdraw_reply().unwrap_err();
    assert!(refused.contains("no queued prompt"), "{refused}");
    let queued = busy.queued("second");
    assert_eq!(busy.withdraw_reply(), Ok((false, Some(queued.id))));
    assert_eq!(settled_run(&busy.host, queued.id).state, State::Cancelled);
    assert!(busy.handle.turn_is_busy(), "the turn keeps running");
    busy.release();
    wait_for("the turn to end", || busy.handle.is_idle());
    // The first run and the held turn; the withdrawn prompt never went.
    assert_eq!(busy.count("prompts"), 2);
    // Read once the provider has handled everything sent to it.
    assert_eq!(busy.count("interrupts"), 0);
}

#[test]
fn an_interrupt_withdraws_an_accepted_prompt_that_dispatch_has_not_submitted() {
    if isolated(
        "host::agent_execution::tests::queued_prompt::an_interrupt_withdraws_an_accepted_prompt_that_dispatch_has_not_submitted",
    )
    .is_some()
    {
        return;
    }
    let busy = Busy::idle();
    let origin = origin(&busy.host, &busy.owner).unwrap();
    let accepted = registry(&busy.host)
        .unwrap()
        .lock()
        .unwrap()
        .submit_agent_with_spec(
            &origin,
            &Submit {
                name: None,
                destination: Destination::Existing(busy.agent),
                prompt: "Prompt undispatched".into(),
                idempotency_key: "undispatched".into(),
            },
            None,
            now(),
        )
        .unwrap();
    assert_eq!(busy.interrupt_reply(), Ok((false, Some(accepted.run.id))));
    let run = busy.run(accepted.run.id);
    assert_eq!(run.state, State::Cancelled);
    assert_eq!(busy.count("prompts"), 1);
    assert_eq!(busy.count("interrupts"), 0);
}

/// Production Claude agents run on the retained actor; the tests above use the
/// local one. Its refusals and exit reach dispatch by another route.
#[test]
fn a_retained_actor_queues_releases_withdraws_and_reports_its_exit() {
    const TEST: &str = "host::agent_execution::tests::queued_prompt::a_retained_actor_queues_releases_withdraws_and_reports_its_exit";
    if retained_automation::reran_in_retained_fixture(TEST) {
        return;
    }
    let mut guards = None;
    let busy = Busy::started(|host| {
        let proof = host.lock().unwrap().proof().clone();
        let paths = crate::daemon::RuntimePaths::discover().unwrap();
        let lease = crate::daemon::HostLease::acquire(&paths, &proof).unwrap();
        let notifications = crate::host::notifications::Broker::start(
            host,
            crate::notifications::Application::Cagents,
        )
        .unwrap();
        host.lock().unwrap().notifications = Some(notifications.handle());
        guards = Some((lease, notifications));
    });
    assert!(
        busy.handle.actor_identity().is_some(),
        "this regression must execute the real retained helper"
    );

    busy.hold();
    let queued = busy.queued("released");
    assert_eq!(busy.count("prompts"), 2, "the queued prompt was not sent");
    // The helper answered, and it doesn't hold the prompt still waiting here,
    // so that one can be reported as unsent. An unanswered refresh couldn't say.
    assert!(!super::super::reached_retained_actor(
        &busy.handle,
        queued.id
    ));
    busy.release();
    let run = settled_run(&busy.host, queued.id);
    assert_eq!(run.state, State::Succeeded, "{:?}", run.result.diagnostic);
    assert_eq!(busy.count("prompts"), 3);

    // The other way round: one the helper took must never read as unsent.
    let _ = std::fs::remove_file(busy.root.join("cleanup-release"));
    let _ = std::fs::remove_file(busy.root.join("cleanup-busy"));
    let sent = busy.send(HOLD).unwrap();
    wait_for("the sent turn", || busy.root.join("cleanup-busy").exists());
    assert!(super::super::reached_retained_actor(
        &busy.handle,
        sent.run.id
    ));
    busy.release();
    assert_eq!(settled_run(&busy.host, sent.run.id).state, State::Succeeded);
    wait_for("the turn to end", || busy.handle.is_idle());

    busy.hold();
    let queued = busy.queued("withdrawn");
    assert_eq!(busy.interrupt_reply(), Ok((true, Some(queued.id))));
    assert_eq!(busy.run(queued.id).state, State::Cancelled);
    busy.release();
    wait_for("the interrupt to reach the provider", || {
        busy.count("interrupts") == 1
    });
    wait_for("the turn to end", || busy.handle.is_idle());
    assert_eq!(busy.count("prompts"), 5, "the withdrawn prompt never went");

    busy.hold();
    let queued = busy.queued("exited");
    std::fs::write(busy.root.join("provider-exit"), "").unwrap();
    let run = settled_run(&busy.host, queued.id);
    assert_eq!(run.state, State::Failed, "{:?}", run.result.diagnostic);
    assert!(
        run.result
            .diagnostic
            .as_deref()
            .unwrap()
            .contains("not sent"),
        "{:?}",
        run.result.diagnostic
    );
    assert_eq!(
        busy.count("prompts"),
        6,
        "the exited provider took no prompt"
    );
}

#[test]
fn a_scheduled_firing_still_fails_rather_than_queueing_behind_a_busy_turn() {
    if isolated("host::agent_execution::tests::queued_prompt::a_scheduled_firing_still_fails_rather_than_queueing_behind_a_busy_turn").is_some() {
        return;
    }
    let (host, request, path) = fixture();
    let root = path.parent().unwrap().to_owned();
    let automation = automation_fixture(&host, &request);
    let accepted = claim_fixture(&host, &automation);
    dispatch_accepted(&host, accepted.clone()).unwrap();
    let first = settled(&host, accepted.run.id);
    release_fixture(&host, &first);
    let session = registry(&host)
        .unwrap()
        .lock()
        .unwrap()
        .agent(accepted.agent.id)
        .unwrap()
        .session
        .unwrap()
        .id();
    let handle = host.lock().unwrap().chats[&session].handle.clone();
    handle.try_idle_prompt(HOLD.into()).unwrap();
    wait_for("the held turn", || root.join("cleanup-busy").exists());
    let next = claim_fixture(&host, &automation);
    dispatch_accepted(&host, next.clone()).unwrap();
    let run = settled_run(&host, next.run.id);
    assert_eq!(run.state, State::Failed);
    assert!(run.result.diagnostic.unwrap().contains("not idle"));
    std::fs::write(root.join("cleanup-release"), "").unwrap();
}
