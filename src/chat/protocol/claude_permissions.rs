//! Session-scoped Claude settings. Keep the provider's raw flag objects private:
//! shallow top-level settings merges must preserve their unrelated fields.
use crate::chat::{
    PermissionBehavior, PermissionEnvironment, PermissionRule, PermissionSettings,
    PermissionSettingsUpdate,
};
use serde_json::{Map, Value, json};

const MAX_ENTRIES: usize = 512;
const MAX_ENTRY_BYTES: usize = 4096;
const MAX_SETTINGS_BYTES: usize = 128 * 1024;
const FLAG_SOURCE: &str = "flagSettings";

#[derive(Debug, Clone)]
pub(super) struct Snapshot {
    pub settings: PermissionSettings,
    permissions: Map<String, Value>,
    auto_mode: Map<String, Value>,
    initial_environment: Vec<Value>,
}

impl Snapshot {
    pub fn parse(response: &Value) -> Result<Self, String> {
        let effective = response["effective"]
            .as_object()
            .ok_or("Claude did not return effective permission settings")?;
        let sources = response["sources"]
            .as_array()
            .ok_or("Claude did not return permission settings sources")?;
        let mut snapshot = Self {
            settings: PermissionSettings::default(),
            permissions: Map::new(),
            auto_mode: Map::new(),
            initial_environment: effective
                .get("autoMode")
                .and_then(|mode| mode.get("environment"))
                .and_then(Value::as_array)
                .map(|_| Vec::new())
                .unwrap_or_else(|| vec![json!("$defaults")]),
        };
        for source in sources {
            let name = source["source"]
                .as_str()
                .ok_or("Claude settings source has no identity")?;
            if name.is_empty() || name.len() > 128 || name.chars().any(char::is_control) {
                return Err("Claude settings source identity is malformed".into());
            }
            let settings = &source["settings"];
            if name == FLAG_SOURCE {
                snapshot.permissions = settings["permissions"]
                    .as_object()
                    .cloned()
                    .unwrap_or_default();
                snapshot.auto_mode = settings["autoMode"]
                    .as_object()
                    .cloned()
                    .unwrap_or_default();
            }
            for behavior in [
                PermissionBehavior::Allow,
                PermissionBehavior::Ask,
                PermissionBehavior::Deny,
            ] {
                if let Some(rules) = settings["permissions"][behavior.as_str()].as_array() {
                    for rule in rules {
                        let rule = entry(rule)?;
                        if snapshot.settings.rules.iter().any(|existing| {
                            existing.behavior == behavior
                                && existing.rule == rule
                                && existing.source == name
                        }) {
                            continue;
                        }
                        if snapshot.settings.rules.len() >= MAX_ENTRIES {
                            return Err("Claude returned too many permission rules".into());
                        }
                        snapshot.settings.rules.push(PermissionRule {
                            behavior,
                            rule: rule.into(),
                            source: name.into(),
                        });
                    }
                }
            }
            if let Some(entries) = settings["autoMode"]["environment"].as_array() {
                for text in entries {
                    let text = entry(text)?;
                    if text == "$defaults" {
                        continue;
                    }
                    if snapshot
                        .settings
                        .environment
                        .iter()
                        .any(|existing| existing.text == text && existing.source == name)
                    {
                        continue;
                    }
                    if snapshot.settings.environment.len() >= MAX_ENTRIES {
                        return Err("Claude returned too many Auto mode environment entries".into());
                    }
                    snapshot.settings.environment.push(PermissionEnvironment {
                        text: text.into(),
                        source: name.into(),
                    });
                }
            }
        }
        Ok(snapshot)
    }

    /// Build a bounded patch from a fresh provider snapshot. Inherited rules
    /// remain owned by their source; a session cannot remove policy or disk rules.
    pub fn update(&self, update: &PermissionSettingsUpdate) -> Result<Value, String> {
        let patch = match update {
            PermissionSettingsUpdate::AddRule { behavior, rule }
            | PermissionSettingsUpdate::RemoveRule { behavior, rule } => {
                validate_entry(rule)?;
                let mut permissions = self.permissions.clone();
                let rules = permissions
                    .entry(behavior.as_str())
                    .or_insert_with(|| json!([]));
                let rules = rules
                    .as_array_mut()
                    .ok_or("Claude flag permission rules are malformed")?;
                mutate(
                    rules,
                    rule,
                    matches!(update, PermissionSettingsUpdate::RemoveRule { .. }),
                )?;
                json!({"permissions": permissions})
            }
            PermissionSettingsUpdate::AddEnvironment { text }
            | PermissionSettingsUpdate::RemoveEnvironment { text } => {
                validate_entry(text)?;
                if text == "$defaults" {
                    return Err("Built-in Auto mode context cannot be changed here".into());
                }
                let remove = matches!(update, PermissionSettingsUpdate::RemoveEnvironment { .. });
                let mut auto_mode = self.auto_mode.clone();
                // Claude merges arrays across settings sources. Preserve
                // implicit defaults only when no source supplied environment.
                let entries = auto_mode
                    .entry("environment")
                    .or_insert_with(|| Value::Array(self.initial_environment.clone()));
                let entries = entries
                    .as_array_mut()
                    .ok_or("Claude Auto mode context is malformed")?;
                if remove
                    && !self
                        .settings
                        .environment
                        .iter()
                        .any(|entry| entry.source == FLAG_SOURCE && entry.text == *text)
                {
                    return Err("Only this conversation's Auto mode context can be removed".into());
                }
                mutate(entries, text, remove)?;
                json!({"autoMode": auto_mode})
            }
        };
        if patch.to_string().len() > MAX_SETTINGS_BYTES {
            return Err("Claude permission settings exceed the native transport limit".into());
        }
        Ok(patch)
    }

    /// An apply acknowledgement can succeed even when Claude drops invalid
    /// entries. Check the refreshed source, rather than the merged settings:
    /// matching inherited entries cannot prove that a session edit succeeded.
    pub fn confirm(&self, update: &PermissionSettingsUpdate) -> Result<(), String> {
        let (present, should_be_present, description) = match update {
            PermissionSettingsUpdate::AddRule { behavior, rule }
            | PermissionSettingsUpdate::RemoveRule { behavior, rule } => (
                self.settings.rules.iter().any(|entry| {
                    entry.source == FLAG_SOURCE
                        && entry.behavior == *behavior
                        && entry.rule == *rule
                }),
                matches!(update, PermissionSettingsUpdate::AddRule { .. }),
                "permission rule",
            ),
            PermissionSettingsUpdate::AddEnvironment { text }
            | PermissionSettingsUpdate::RemoveEnvironment { text } => (
                self.settings
                    .environment
                    .iter()
                    .any(|entry| entry.source == FLAG_SOURCE && entry.text == *text),
                matches!(update, PermissionSettingsUpdate::AddEnvironment { .. }),
                "Auto context entry",
            ),
        };
        if present == should_be_present {
            Ok(())
        } else if should_be_present {
            Err(format!(
                "Claude did not apply the {description}. Check the entry's syntax and Claude's policy, then try again. The panel shows Claude's actual settings."
            ))
        } else {
            Err(format!(
                "Claude did not remove the {description}. Refresh the settings and try again. The panel shows Claude's actual settings."
            ))
        }
    }
}

fn entry(value: &Value) -> Result<&str, String> {
    let text = value
        .as_str()
        .ok_or("Claude permission setting is not text")?;
    validate_entry(text)?;
    Ok(text)
}

fn validate_entry(text: &str) -> Result<(), String> {
    if text.trim().is_empty() || text.len() > MAX_ENTRY_BYTES || text.contains('\0') {
        return Err(
            "Permission entries must contain 1–4096 bytes of text without NUL characters".into(),
        );
    }
    Ok(())
}

fn mutate(entries: &mut Vec<Value>, text: &str, remove: bool) -> Result<(), String> {
    if remove {
        if !entries.iter().any(|entry| entry.as_str() == Some(text)) {
            return Err("Only this conversation's permission entries can be removed".into());
        }
        entries.retain(|entry| entry.as_str() != Some(text));
    } else if !entries.iter().any(|entry| entry.as_str() == Some(text)) {
        if entries.len() >= MAX_ENTRIES {
            return Err("This conversation has too many permission entries".into());
        }
        entries.push(json!(text));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot() -> Snapshot {
        Snapshot::parse(&json!({
            "effective":{"autoMode":{"environment":["$defaults","Inherited context"]}},
            "sources":[
                {"source":"userSettings","settings":{"permissions":{"deny":["Read(.env)"]},"autoMode":{"environment":["$defaults","Inherited context"]}}},
                {"source":"flagSettings","settings":{"permissions":{"additionalDirectories":["/work"],"ask":["Bash(git push *)"]},"autoMode":{"hard_deny":["$defaults"]}}}
            ]
        })).unwrap()
    }

    #[test]
    fn rule_patch_preserves_existing_flag_fields_without_copying_inherited_rules() {
        let patch = snapshot()
            .update(&PermissionSettingsUpdate::AddRule {
                behavior: PermissionBehavior::Allow,
                rule: "Read(./docs/**)".into(),
            })
            .unwrap();
        assert_eq!(
            patch,
            json!({"permissions":{"additionalDirectories":["/work"],"ask":["Bash(git push *)"],"allow":["Read(./docs/**)"]}})
        );
        assert!(
            snapshot()
                .update(&PermissionSettingsUpdate::RemoveRule {
                    behavior: PermissionBehavior::Deny,
                    rule: "Read(.env)".into(),
                })
                .is_err()
        );
    }

    #[test]
    fn auto_context_preserves_inherited_context_defaults_and_other_sections() {
        let patch = snapshot()
            .update(&PermissionSettingsUpdate::AddEnvironment {
                text: "This checkout is disposable.".into(),
            })
            .unwrap();
        assert_eq!(
            patch,
            json!({"autoMode":{"hard_deny":["$defaults"],"environment":["This checkout is disposable."]}})
        );
        assert!(
            snapshot()
                .update(&PermissionSettingsUpdate::RemoveEnvironment {
                    text: "Inherited context".into()
                })
                .is_err()
        );
        assert!(
            snapshot()
                .update(&PermissionSettingsUpdate::AddEnvironment {
                    text: "$defaults".into()
                })
                .is_err()
        );
    }

    #[test]
    fn provider_metadata_does_not_expose_unrelated_settings() {
        let snapshot = Snapshot::parse(&json!({"effective":{},"sources":[{"source":"userSettings","settings":{"env":{"SECRET":"hidden"},"permissions":{"allow":["Read"]}}}]})).unwrap();
        let metadata = serde_json::to_string(&snapshot.settings).unwrap();
        assert!(!metadata.contains("SECRET"));
        assert!(!metadata.contains("hidden"));
        assert_eq!(snapshot.settings.rules[0].source, "userSettings");
    }

    #[test]
    fn adding_auto_context_preserves_implicit_defaults_without_reintroducing_removed_defaults() {
        for (effective, expected) in [
            (json!({}), json!(["$defaults", "Local checkout"])),
            (
                json!({"autoMode":{"environment":["Custom environment"]}}),
                json!(["Local checkout"]),
            ),
        ] {
            let snapshot = Snapshot::parse(&json!({"effective":effective,"sources":[]})).unwrap();
            let patch = snapshot
                .update(&PermissionSettingsUpdate::AddEnvironment {
                    text: "Local checkout".into(),
                })
                .unwrap();
            assert_eq!(patch["autoMode"]["environment"], expected);
        }
    }

    #[test]
    fn repeated_source_entries_are_deduplicated_without_merging_different_sources() {
        let snapshot = Snapshot::parse(&json!({"effective":{},"sources":[
            {"source":"userSettings","settings":{"permissions":{"allow":["Read","Read"]},"autoMode":{"environment":["Local","Local"]}}},
            {"source":"flagSettings","settings":{"permissions":{"allow":["Read"]},"autoMode":{"environment":["Local"]}}},
        ]})).unwrap();
        assert_eq!(snapshot.settings.rules.len(), 2);
        assert_eq!(snapshot.settings.environment.len(), 2);
    }

    #[test]
    fn settings_source_identifiers_are_bounded_before_copying_per_entry() {
        assert!(
            Snapshot::parse(&json!({"effective":{},"sources":[{
                "source":"x".repeat(129),"settings":{"permissions":{"allow":["Read"]}}
            }]}))
            .is_err()
        );
    }
}
