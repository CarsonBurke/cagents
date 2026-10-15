//! Actual App callback/cache contracts through the real client and a scripted
//! private peer. These do NOT exercise the Registry19 daemon admission issuer.
use super::*;
use crate::{
    edited_root_admission::test_peer::{Event, ScriptedEditedRootPeer},
    fs_safety::Root,
};

fn prepared_git(app: &mut Cagents, project: ProjectId)
    -> (tempfile::TempDir, ScriptedEditedRootPeer, AdmissionTicket, Box<PreparedProjectRepository>) {
    let directory = tempfile::tempdir().unwrap();
    let command = crate::repository::CommandSpec::new("git").cwd(directory.path())
        .args(["init", "--template=", "-b", "main"]);
    crate::git_watch::tests::run_fixture_command(
        crate::git_watch::tests::isolated_fixture_command(command));
    let root = Root::open(directory.path()).unwrap();
    let peer = ScriptedEditedRootPeer::start(&root);
    let cancellation = CancellationToken::new();
    app.repository_generations.insert(project, 1);
    app.refresh_ui.projects.insert(project, cancellation.clone());
    let ticket = app.edited_root_admissions.lock().unwrap().reserve().unwrap();
    let work = ticket.begin_preparation().unwrap();
    let prepared = project_source_admission::prepare_project_repository(directory.path(), project,
        cancellation, Some(ProjectKind::Git), None, Ok(peer.endpoint()), work,
        Instant::now() + Duration::from_secs(3)).unwrap();
    assert!(peer.events().is_empty(), "phase one must not send Prepare/Activate");
    assert_eq!(ticket.phase(), AdmissionPhase::Prepared);
    (directory, peer, ticket, Box::new(prepared))
}

async fn outputs(task: Task<Message>) -> Vec<Message> {
    use cosmic::iced::futures::StreamExt;
    tokio::time::timeout(Duration::from_secs(10), async {
        let mut messages = Vec::new();
        if let Some(mut stream) = cosmic::iced::runtime::task::into_stream(task) {
            while let Some(action) = stream.next().await {
                if let cosmic::iced::runtime::Action::Output(cosmic::Action::App(message)) = action {
                    messages.push(message);
                }
            }
        }
        messages
    }).await.expect("owned admission task must make bounded progress")
}

async fn admitted_callback(app: &mut Cagents, project: ProjectId,
    prepared: Box<PreparedProjectRepository>) -> Message {
    let task = app.update(Message::RepositoryPrepared { project_id: project,
        generation: 1, open_cards: false, result: Ok(prepared) });
    let mut callbacks = outputs(task).await.into_iter()
        .filter(|message| matches!(message, Message::RepositoryAdmitted { .. }));
    let callback = callbacks.next().expect("accepted phase one starts the actual enrollment worker");
    assert!(callbacks.next().is_none(), "one accepted preparation starts one enrollment");
    assert!(matches!(&callback, Message::RepositoryAdmitted { project_id, generation: 1,
        ticket, result: Ok(_), .. } if *project_id == project && ticket.phase() == AdmissionPhase::Active));
    callback
}

async fn settle_original(app: &mut Cagents) {
    let mut tasks = vec![app.update(Message::RootAdmissions(root_admission_ui::Action::Poll))];
    let mut delivered = 0;
    while let Some(task) = tasks.pop() {
        for message in outputs(task).await {
            assert!(matches!(message, Message::RootAdmissions(_)), "cleanup must not publish a new project/source");
            delivered += 1;
            assert!(delivered <= 8, "bounded single-root cleanup must not spin");
            tasks.push(app.update(message));
        }
    }
    assert!(app.edited_root_admissions.lock().unwrap().is_empty(), "original cleanup must settle and collect its same ticket");
}

#[tokio::test]
async fn scripted_client_actual_project_callbacks_publish_and_refresh_the_same_registered_pair() {
    if !crate::git_watch::tests::fixture_isolated(
        "app::project_source_ui::contracts::scripted_client_actual_project_callbacks_publish_and_refresh_the_same_registered_pair") { return; }
    let mut app = super::super::tests::context_inspector_test_app(false);
    let project = ProjectId::new();
    let (directory, peer, ticket, prepared) = prepared_git(&mut app, project);
    assert!(app.retained_registered_project_source(project).is_none());
    let callback = admitted_callback(&mut app, project, prepared).await;
    assert!(app.sidebar_model.project(project).is_none(), "worker success is not publication before final ingress");
    drop(app.update(callback));
    let registered = app.retained_registered_project_source(project).expect("actual final ingress publishes the whole pair");
    let actor = registered.source.id();
    let original = registered.editor_root.retained_root();
    assert_eq!(registered.editor_root.retained_admission().unwrap().ticket().id(), ticket.id());
    assert!(Cagents::registered_project_source_is_current(&registered));
    assert_eq!(app.sidebar_model.project(project).unwrap().creation_binding, Some(actor));
    assert_eq!(app.project_creation_generation(project), Some(1));
    assert!(app.cards.is_empty(), "metadata publication must not create a terminal or editor pane");

    app.repository_generations.insert(project, 2);
    let cancellation = CancellationToken::new();
    app.refresh_ui.projects.insert(project, cancellation.clone());
    assert!(app.project_creation_source(project).is_none(), "new intents wait for the current read");
    assert!(app.current_project_creation_source(project, actor), "already admitted source remains current while refreshing");
    let retained = app.retained_registered_project_source(project).unwrap();
    let read_ticket = app.edited_root_admissions.lock().unwrap().reserve().unwrap();
    let refreshed = project_source_admission::prepare_project_repository(directory.path(), project,
        cancellation, Some(ProjectKind::Git), Some(retained),
        Err("changed endpoint must not reroute a retained source".into()),
        read_ticket.begin_preparation().unwrap(), Instant::now() + Duration::from_secs(3)).unwrap();
    drop(app.update(Message::RepositoryPrepared { project_id: project, generation: 2,
        open_cards: false, result: Ok(Box::new(refreshed)) }));
    let refreshed = app.retained_registered_project_source(project).unwrap();
    assert_eq!(refreshed.source.id(), actor);
    assert!(Arc::ptr_eq(&original, &refreshed.editor_root.retained_root()));
    assert_eq!(app.project_creation_generation(project), Some(1), "ordinary read advances no original logical lifetime");
    assert!(app.project_creation_source(project).is_some());
    assert_eq!(peer.events().iter().filter(|event| **event == Event::Prepared).count(), 1);
    app.retire_project_creation_source(project);
    drop(registered);
    drop(refreshed);
    drop(original);
    settle_original(&mut app).await;
    let events = peer.finish();
    assert_eq!(events.iter().filter(|event| **event == Event::Activated).count(), 1);
    assert_eq!(events.iter().filter(|event| **event == Event::Released).count(), 1);
}

#[tokio::test]
async fn scripted_client_stale_or_sealed_project_preparation_never_enrolls() {
    if !crate::git_watch::tests::fixture_isolated(
        "app::project_source_ui::contracts::scripted_client_stale_or_sealed_project_preparation_never_enrolls") { return; }
    for sealed in [false, true] {
        let mut app = super::super::tests::context_inspector_test_app(false);
        let project = ProjectId::new();
        let (_directory, peer, ticket, prepared) = prepared_git(&mut app, project);
        if sealed { app.edited_root_admissions.lock().unwrap().seal_new_admissions(); }
        else { app.repository_generations.insert(project, 2); }
        drop(app.update(Message::RepositoryPrepared { project_id: project, generation: 1,
            open_cards: false, result: Ok(prepared) }));
        assert!(app.sidebar_model.project(project).is_none());
        assert!(app.retained_registered_project_source(project).is_none());
        assert_ne!(ticket.phase(), AdmissionPhase::Active);
        settle_original(&mut app).await;
        assert!(peer.finish().is_empty(), "stale phase one settles without any enrollment/release RPC");
    }
}

#[tokio::test]
async fn scripted_client_late_project_admission_retires_its_original_owner_without_publishing() {
    if !crate::git_watch::tests::fixture_isolated(
        "app::project_source_ui::contracts::scripted_client_late_project_admission_retires_its_original_owner_without_publishing") { return; }
    for disposition in 0..3 {
        let mut app = super::super::tests::context_inspector_test_app(false);
        let project = ProjectId::new();
        let (_directory, peer, ticket, prepared) = prepared_git(&mut app, project);
        let repository = prepared.loaded.project.repository.clone();
        let callback = admitted_callback(&mut app, project, prepared).await;
        match disposition {
            0 => { app.repository_generations.insert(project, 2); }
            1 => { app.project_ui.closed.insert(repository); }
            _ => { app.update_prepared = true; }
        }
        drop(app.update(callback));
        assert!(app.sidebar_model.project(project).is_none());
        assert!(app.retained_registered_project_source(project).is_none());
        assert!(app.cards.is_empty());
        assert_ne!(ticket.phase(), AdmissionPhase::Active);
        settle_original(&mut app).await;
        let events = peer.finish();
        assert_eq!(events.iter().filter(|event| **event == Event::Prepared).count(), 1);
        assert_eq!(events.iter().filter(|event| **event == Event::Activated).count(), 1);
        assert_eq!(events.iter().filter(|event| **event == Event::Released).count(), 1,
            "late/closed result must retire the SAME original issuer owner");
    }
}
