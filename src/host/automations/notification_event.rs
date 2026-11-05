//! Lifecycle identity never contains an automation's user-authored content.
use crate::{
    automations::{LifecycleActor, LifecycleCause, LifecycleEvent},
    host::model::{Registry, automation_notifications::Entry},
    notifications::{Content, Event, Evidence, Kind, Target, presentation},
};

pub(super) fn build(registry: &Registry, entry: &Entry) -> Result<Event, String> {
    // A removed/unavailable definition must not hold the oldest receipt at the
    // head of delivery. Its exact historical identity still opens as history.
    let name = registry
        .automation(entry.event().automation)
        .ok()
        .map(|automation| automation.definition.name);
    event(entry.event(), entry.evidence_key(), name.as_deref())
}

fn event(
    lifecycle: &LifecycleEvent,
    evidence_key: String,
    name: Option<&str>,
) -> Result<Event, String> {
    if lifecycle.automation.is_nil()
        || lifecycle.sequence == 0
        || lifecycle.sequence > i64::MAX as u64
        || lifecycle.actor != LifecycleActor::System
        || lifecycle.enabled
        || !matches!(
            lifecycle.cause,
            LifecycleCause::ProjectClosed
                | LifecycleCause::StartPaused
                | LifecycleCause::ProjectUnavailable
                | LifecycleCause::PrecheckInterrupted
                | LifecycleCause::PrecheckPaused
        )
        || evidence_key
            != format!(
                "automation:{}:lifecycle:{}",
                lifecycle.automation, lifecycle.sequence
            )
    {
        return Err("Automation attention requires an exact system pause receipt".into());
    }
    let content = name
        .map(presentation::title)
        .filter(|name| !name.is_empty())
        .map(|conversation| Content {
            conversation,
            // This is a fixed cause, never the precheck's raw diagnostic,
            // command, prompt, account, or execution directory.
            preview: presentation::plain_preview(lifecycle.cause.label()),
            request: None,
        });
    let event = Event {
        target: Target::Automation {
            id: lifecycle.automation,
            sequence: lifecycle.sequence,
        },
        source_session: None,
        provider: None,
        directory: None,
        destination: None,
        evidence: Evidence {
            key: evidence_key,
            kind: if lifecycle.blocked {
                Kind::AutomationBlocked
            } else {
                Kind::AutomationPaused
            },
        },
        content,
    };
    event.validate()?;
    Ok(event)
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    fn receipt(cause: LifecycleCause, blocked: bool) -> LifecycleEvent {
        LifecycleEvent {
            sequence: 17,
            automation: Uuid::new_v4(),
            revision: 3,
            occurred_ms: 100,
            cause,
            actor: LifecycleActor::System,
            enabled: false,
            blocked,
            firing: None,
        }
    }

    fn key(lifecycle: &LifecycleEvent) -> String {
        format!(
            "automation:{}:lifecycle:{}",
            lifecycle.automation, lifecycle.sequence
        )
    }

    #[test]
    fn no_run_attention_keeps_exact_history_identity_and_only_opt_in_bounded_content() {
        for (cause, blocked) in [
            (LifecycleCause::ProjectClosed, false),
            (LifecycleCause::StartPaused, false),
            (LifecycleCause::ProjectUnavailable, false),
            (LifecycleCause::PrecheckInterrupted, true),
            (LifecycleCause::PrecheckPaused, false),
        ] {
            let lifecycle = receipt(cause, blocked);
            let name = format!("Private fixture\u{1b}\n{}", "長".repeat(300));
            let built = event(&lifecycle, key(&lifecycle), Some(&name)).unwrap();
            assert_eq!(
                built.target,
                Target::Automation {
                    id: lifecycle.automation,
                    sequence: lifecycle.sequence,
                }
            );
            assert_eq!(built.evidence.key, key(&lifecycle));
            assert_eq!(
                built.evidence.kind,
                if blocked {
                    Kind::AutomationBlocked
                } else {
                    Kind::AutomationPaused
                }
            );
            assert!(
                built.source_session.is_none()
                    && built.provider.is_none()
                    && built.directory.is_none()
                    && built.destination.is_none()
            );
            let content = built.content.as_ref().unwrap();
            assert!(content.conversation.chars().count() <= presentation::TITLE_LIMIT);
            assert!(!content.conversation.chars().any(char::is_control));
            assert_eq!(content.preview, cause.label());
            assert!(content.request.is_none());
            // Mirror the existing journal preview opt-out boundary. This is a
            // payload privacy oracle, not a desktop-delivery assertion.
            let mut private = built;
            private.content = None;
            let serialized = serde_json::to_string(&private).unwrap();
            assert!(!serialized.contains("Private fixture") && !serialized.contains('長'));
            assert_eq!(
                event(&lifecycle, key(&lifecycle), None).unwrap(),
                private,
                "missing definition names still produce generic exact history attention"
            );
        }
    }

    #[test]
    fn management_run_receipts_and_mismatched_or_unbounded_identity_cannot_become_no_run_attention()
    {
        let valid = receipt(LifecycleCause::ProjectClosed, false);
        for cause in [
            LifecycleCause::Disabled,
            LifecycleCause::Enabled,
            LifecycleCause::Removed,
            LifecycleCause::RunInterrupted,
            LifecycleCause::ReleaseBlocked,
            LifecycleCause::PrecheckRecovered,
        ] {
            let lifecycle = LifecycleEvent {
                cause,
                actor: cause.actor(),
                ..valid.clone()
            };
            assert!(event(&lifecycle, key(&lifecycle), None).is_err());
        }
        for lifecycle in [
            LifecycleEvent {
                enabled: true,
                ..valid.clone()
            },
            LifecycleEvent {
                actor: LifecycleActor::UserManagement,
                ..valid.clone()
            },
            LifecycleEvent {
                automation: Uuid::nil(),
                ..valid.clone()
            },
            LifecycleEvent {
                sequence: 0,
                ..valid.clone()
            },
            LifecycleEvent {
                sequence: u64::MAX,
                ..valid.clone()
            },
        ] {
            assert!(event(&lifecycle, key(&lifecycle), None).is_err());
        }
        assert!(event(&valid, "another receipt".into(), None).is_err());
        let next = LifecycleEvent {
            sequence: valid.sequence + 1,
            ..valid.clone()
        };
        let before = event(&valid, key(&valid), None).unwrap();
        let after = event(&next, key(&next), None).unwrap();
        assert_ne!(before.target, after.target);
        assert_ne!(before.evidence, after.evidence);
    }
}
