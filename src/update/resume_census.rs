//! Optional private details associated with the update's conversation census.
//!
//! Earlier builds reserialize the recovery census to verify the same
//! rollback proof. Keep these details in a separate job artifact so those
//! builds can still return to the exact original checkpoint.
use super::in_flight::{InFlight, QueuedText, ResumeData};
use super::{UpdateError, retention, write_state_file_through};
use crate::chat::{QueuedMessage, Snapshot};
use crate::model::TerminalId;
use serde::{Deserialize, Serialize};
use std::{fs, io, path::Path};

const FILE: &str = "resume-census.json";
// Capture holds the worker lease. A fixed path lets the next writer remove
// any private text a killed writer left before the atomic rename.
const TEMPORARY: &str = "resume-census.tmp";
const LIMIT: usize = 512 * 1024;

#[derive(Serialize, Deserialize)]
struct Entry {
    id: TerminalId,
    queued: Vec<String>,
    details: ResumeData,
}

#[derive(Serialize, Deserialize)]
struct Saved {
    job: String,
    entries: Vec<Entry>,
}

pub(super) struct Capture {
    entries: Vec<Entry>,
    remaining: usize,
}

impl Capture {
    pub(super) fn new() -> Self {
        Self {
            entries: Vec::new(),
            // The outer object includes one canonical UUID and the array.
            remaining: LIMIT - 128,
        }
    }

    pub(super) fn observe(&mut self, work: &InFlight, snapshot: &Snapshot) {
        if work.queued.is_empty() {
            return;
        }
        let mut entry = Entry {
            id: work.id,
            queued: work.queued.clone(),
            details: ResumeData::default(),
        };
        let Ok(base) = serde_json::to_vec(&entry) else {
            return;
        };
        let overhead = base.len() + r#""queued_text":[],"#.len() + 1;
        let Some(mut remaining) = self.remaining.checked_sub(overhead) else {
            return;
        };
        // The census already bounded the queue IDs. Walk that same ordered
        // prefix for the corresponding payloads.
        let mut listed = work.queued.iter().peekable();
        for message in &snapshot.queued_messages {
            if listed.peek().is_none() {
                break;
            }
            if listed.next_if(|id| **id == message.id).is_none()
                || !message.editable
                || !message.attachments.is_empty()
                || message.text.is_empty()
                || message.text.len() > QueuedMessage::PREVIEW_BYTES
            {
                continue;
            }
            let text = QueuedText {
                id: message.id.clone(),
                text: message.text.clone(),
            };
            let Ok(encoded) = serde_json::to_vec(&text) else {
                continue;
            };
            if let Some(left) = remaining.checked_sub(encoded.len() + 1) {
                remaining = left;
                entry.details.queued_text.push(text);
            }
        }
        if !entry.details.queued_text.is_empty() {
            self.remaining = remaining;
            self.entries.push(entry);
        }
    }

    pub(super) fn save(self, state: &Path, job: &str) -> Result<(), UpdateError> {
        if !retention::canonical_id(job) {
            return Err(super::invalid("invalid update job identifier"));
        }
        clear(state)?;
        if self.entries.is_empty() {
            return Ok(());
        }
        write_state_file_through(
            state,
            FILE,
            &state.join(TEMPORARY),
            &Saved {
                job: job.to_owned(),
                entries: self.entries,
            },
        )
    }
}

/// The worker lease serializes capture and publication of a newer job.
/// Both clear any optional payload from an earlier capture.
pub(super) fn clear(state: &Path) -> Result<(), UpdateError> {
    let mut removed = false;
    for name in [TEMPORARY, FILE] {
        match fs::remove_file(state.join(name)) {
            Ok(()) => removed = true,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    if removed {
        fs::File::open(state)?.sync_all()?;
    }
    Ok(())
}

/// Only the UI's view of a terminal job needs access to the private data.
/// An absent, unsafe or outdated file leaves that job usable as before.
#[cfg(any(feature = "ui", test))]
pub(super) fn enrich(state: &Path, job: &mut super::UpdateJob) {
    if !job.phase.is_terminal()
        || !job
            .in_flight
            .iter()
            .any(|outcome| outcome.lost() && !outcome.work.queued.is_empty())
    {
        return;
    }
    let Ok(bytes) = super::read_private(&state.join(FILE), LIMIT as u64) else {
        return;
    };
    let Ok(saved) = serde_json::from_slice::<Saved>(&bytes) else {
        return;
    };
    if saved.job != job.id {
        return;
    }
    let mut entries: std::collections::HashMap<_, _> = saved
        .entries
        .into_iter()
        .map(|entry| (entry.id, entry))
        .collect();
    for outcome in &mut job.in_flight {
        if outcome.lost()
            && let Some(entry) = entries.remove(&outcome.work.id)
            && outcome.work.queued == entry.queued
        {
            outcome.resume = entry.details;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::Provider;
    use crate::update::in_flight::{Fate, Outcome};
    use crate::update::{UpdateJob, UpdatePhase};
    use crate::upgrade::recovery::Census;
    use sha2::{Digest, Sha256};
    use std::ffi::OsString;
    use std::os::unix::{ffi::OsStringExt, fs::PermissionsExt};

    fn job(work: InFlight) -> UpdateJob {
        UpdateJob {
            id: uuid::Uuid::new_v4().to_string(),
            version: "1.2.3".into(),
            phase: UpdatePhase::Complete,
            message: String::new(),
            log_path: Default::default(),
            confirmation: None,
            refusal: None,
            in_flight: vec![Outcome {
                work,
                resume: ResumeData::default(),
                label: String::new(),
                fate: Fate::Restarted,
            }],
            recorded_by: None,
        }
    }

    #[test]
    fn a_private_resume_census_keeps_the_legacy_census_and_status_bytes() {
        let directory = crate::test_fixture::tempdir().unwrap();
        let id = TerminalId::new();
        let private = "and then push\nwhen the tests pass";
        let snapshot = Snapshot {
            queued_messages: vec![QueuedMessage::new("q".into(), private.into())],
            ..Snapshot::default()
        };
        let work = InFlight::observe(id, Provider::Claude, &snapshot);
        let census = Census {
            in_flight: vec![work.clone()],
            ..Census::default()
        };
        // The previous launcher's census shape and field order. Its Ready
        // return fingerprint hashes this census as part of the reservation.
        let legacy = format!(
            r#"{{"terminals":[],"chats":[],"snapshots":[],"aliases":[],"presentations":[],"in_flight":[{{"id":"{id}","queued":["q"]}}]}}"#
        );
        let legacy_digest = Sha256::digest(legacy.as_bytes());
        let mut capture = Capture::new();
        capture.observe(&work, &snapshot);

        let mut other_job = job(work.clone());
        let mut job = job(work);
        let before = serde_json::to_vec(&job).unwrap();
        capture.save(directory.path(), &job.id).unwrap();
        let path = directory.path().join(FILE);
        let bytes = std::fs::read(&path).unwrap();
        assert!(String::from_utf8(bytes).unwrap().contains("and then push"));
        assert_eq!(path.metadata().unwrap().permissions().mode() & 0o777, 0o600);

        enrich(directory.path(), &mut job);
        assert_eq!(job.in_flight[0].resume.queued_text[0].text, private);
        assert!(!format!("{job:?}").contains("and then push"));
        assert_eq!(serde_json::to_vec(&job).unwrap(), before);
        let wire_census = serde_json::to_vec(&census).unwrap();
        assert_eq!(wire_census, legacy.as_bytes());
        assert_eq!(Sha256::digest(&wire_census), legacy_digest);
        assert_eq!(
            serde_json::to_value(&census).unwrap()["in_flight"],
            serde_json::json!([{ "id": id, "queued": ["q"] }]),
        );

        enrich(directory.path(), &mut other_job);
        assert!(other_job.in_flight[0].resume.queued_text.is_empty());

        job.in_flight[0].resume = ResumeData::default();
        job.in_flight[0].work.queued.push("later".into());
        enrich(directory.path(), &mut job);
        assert!(job.in_flight[0].resume.queued_text.is_empty());
    }

    #[test]
    fn a_failed_current_job_keeps_its_text_until_an_empty_recapture_or_new_job() {
        let directory = crate::test_fixture::tempdir().unwrap();
        let state = directory.path();
        let snapshot = Snapshot {
            queued_messages: vec![QueuedMessage::new("q".into(), "keep my request".into())],
            ..Snapshot::default()
        };
        let work = InFlight::observe(TerminalId::new(), Provider::Claude, &snapshot);
        let mut failed = job(work.clone());
        failed.phase = UpdatePhase::Failed;
        let capture = || {
            let mut capture = Capture::new();
            capture.observe(&work, &snapshot);
            capture.save(state, &failed.id).unwrap();
        };

        capture();
        super::super::write_job(state, &failed).unwrap();
        let mut current = super::super::read_job(state).unwrap().unwrap();
        enrich(state, &mut current);
        assert_eq!(
            current.in_flight[0].resume.queued_text[0].text,
            "keep my request"
        );

        let write_interrupted_capture = || {
            use std::io::Write;
            let mut file = super::super::private_create(&state.join(TEMPORARY)).unwrap();
            file.write_all(b"interrupted capture").unwrap();
        };

        write_interrupted_capture();
        Capture::new().save(state, &failed.id).unwrap();
        assert!(!state.join(FILE).exists());
        assert!(!state.join(TEMPORARY).exists());

        for phase in [UpdatePhase::Starting, UpdatePhase::Installing] {
            super::super::write_job(state, &failed).unwrap();
            capture();
            write_interrupted_capture();
            let mut superseding = job(work.clone());
            superseding.phase = phase;
            // Serializing a non-UTF-8 path fails before replacing status.json.
            superseding.log_path = OsString::from_vec(vec![0xff]).into();
            assert!(super::super::write_new_job(state, &superseding).is_err());
            let mut current = super::super::read_job(state).unwrap().unwrap();
            assert_eq!(current.id, failed.id);
            enrich(state, &mut current);
            assert_eq!(
                current.in_flight[0].resume.queued_text[0].text,
                "keep my request"
            );

            superseding.log_path = state.join("superseding.log");
            super::super::write_new_job(state, &superseding).unwrap();
            assert!(!state.join(FILE).exists());
            assert!(!state.join(TEMPORARY).exists());
            assert_eq!(
                super::super::read_job(state).unwrap().unwrap().id,
                superseding.id
            );
        }
    }

    #[test]
    fn the_resume_census_keeps_only_complete_text_inside_its_bound() {
        let mut truncated = QueuedMessage::new("truncated".into(), "excerpt".into());
        truncated.editable = false;
        let snapshot = Snapshot {
            queued_messages: vec![
                QueuedMessage::new("first".into(), "\u{1}".repeat(QueuedMessage::PREVIEW_BYTES)),
                truncated,
                QueuedMessage::new("last".into(), "end 🙂".into()),
            ],
            ..Snapshot::default()
        };
        let mut capture = Capture::new();
        for _ in 0..250 {
            let work = InFlight::observe(TerminalId::new(), Provider::Claude, &snapshot);
            capture.observe(&work, &snapshot);
        }

        assert!(!capture.entries.is_empty());
        assert!(capture.entries.len() < 250);
        let first = &capture.entries[0].details.queued_text;
        assert_eq!(first[0].text, snapshot.queued_messages[0].text);
        assert_eq!(first[1].text, "end 🙂");
        assert_eq!(first.len(), 2);
        let bytes = serde_json::to_vec(&Saved {
            job: uuid::Uuid::new_v4().to_string(),
            entries: capture.entries,
        })
        .unwrap();
        assert!(bytes.len() < LIMIT);
    }
}
