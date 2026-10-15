//! Validated configuration operations shared by the CLI and selected daemon.
use super::{AppSettings, ensure_file, load_detailed, save_merged};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::path::Path;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Operation {
    Get { key: String },
    Set { key: String, value: String },
    Path { create: bool },
}

pub fn execute(path: &Path, operation: Operation) -> Result<Value, String> {
    if let Operation::Path { create } = operation {
        if create {
            let settings=load_detailed(path).map_err(|e|e.to_string())?.settings;
            ensure_file(path, &settings).map_err(|e| e.to_string())?;
        }
        return Ok(json!({"path": path}));
    }
    let loaded = load_detailed(path).map_err(|e| e.to_string())?;
    let base = loaded.settings.clone();
    let mut document = serde_json::to_value(&base).map_err(|e| e.to_string())?;
    match operation {
        Operation::Get { key } => {
            let value = lookup(&document, &key).cloned().unwrap_or(Value::Null);
            if value.is_null() {
                let mut probe = document;
                replace(&mut probe, &key, Value::Null)?;
                decode(probe)?;
            }
            Ok(json!({"key": key, "value": value}))
        }
        Operation::Set { key, value } => {
            let diagnostics = loaded.set_aside().collect::<Vec<_>>();
            if !diagnostics.is_empty() {
                return Err(format!("Fix the settings file before changing it: {}", diagnostics.join("; ")));
            }
            if key == "schema_version" || key.starts_with("schema_version.") {
                return Err("schema_version is maintained by cagents".into());
            }
            let value = parse_value(&value)?;
            replace(&mut document, &key, value)?;
            let desired = decode(document)?;
            // The shared save path validates the merged document against the
            // actual current file, including transition-specific policies.
            save_merged(path, &base, &desired).map_err(|e| e.to_string())?;
            let saved = load_detailed(path).map_err(|e| e.to_string())?.settings;
            let saved = serde_json::to_value(saved).map_err(|e| e.to_string())?;
            Ok(json!({"key": key, "value": lookup(&saved, &key), "path": path}))
        }
        Operation::Path { .. } => unreachable!(),
    }
}

fn parse_value(text: &str) -> Result<Value, String> {
    if text == "null" {
        return Ok(Value::Null);
    }
    // TOML preserves the types and spellings used by settings.toml. Unquoted
    // strings are convenient at a shell; malformed typed values remain errors.
    match toml::from_str::<toml::Table>(&format!("value = {text}")) {
        Ok(mut table) if table.len() == 1 => serde_json::to_value(table.remove("value").unwrap()).map_err(|e| e.to_string()),
        Err(_) if !matches!(text.chars().next(), Some('[' | '{' | '"' | '\'')) => Ok(Value::String(text.into())),
        _ => Err("Use a TOML value: a number, true/false, quoted string, array or inline table".into()),
    }
}

fn decode(document: Value) -> Result<AppSettings, String> {
    let mut unknown = Vec::new();
    let settings = serde_ignored::deserialize(document, |path| unknown.push(path.to_string()))
        .map_err(|e| e.to_string())?;
    if !unknown.is_empty() {
        return Err(format!("Unknown setting: {}", unknown.join(", ")));
    }
    Ok(settings)
}

fn lookup<'a>(document: &'a Value, key: &str) -> Option<&'a Value> {
    key.split('.').try_fold(document, |value, key| value.get(key))
}

fn replace(document: &mut Value, key: &str, value: Value) -> Result<(), String> {
    let keys: Vec<_> = key.split('.').collect();
    if keys.iter().any(|key| key.is_empty()) {
        return Err("Give a setting name, using dots for nested keys".into());
    }
    let mut current = document;
    for key in &keys[..keys.len() - 1] {
        let object = current.as_object_mut().ok_or("Setting path crosses a scalar value")?;
        current = object.entry(*key).or_insert_with(|| json!({}));
        if current.is_null() {
            *current = json!({});
        }
    }
    current.as_object_mut().ok_or("Setting path crosses a scalar value")?
        .insert(keys[keys.len() - 1].into(), value);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn settings_open_preserves_a_file_created_after_its_original_snapshot() {
        let root=tempfile::tempdir().unwrap();let path=root.path().join("settings.toml");
        // Deterministic old race: one caller observed defaults while absent,
        // another completed its save before the first caller's create step.
        let stale=load_detailed(&path).unwrap().settings;
        let writer_path=path.clone();
        std::thread::spawn(move ||execute(&writer_path,Operation::Set{key:"terminal_font_size".into(),value:"15".into()}).unwrap()).join().unwrap();
        ensure_file(&path,&stale).unwrap();
        execute(&path,Operation::Path{create:true}).unwrap();
        assert_eq!(super::super::load(&path).unwrap().terminal_font_size,15.0);
    }

    #[test]
    fn settings_open_migrates_legacy_values_before_ensuring_toml_exists() {
        let root=tempfile::tempdir().unwrap();let path=root.path().join("settings.toml");
        let legacy=root.path().join("settings.json");
        std::fs::write(&legacy,br#"{"terminal_font_size":18.0,"default_column_fraction":0.65}"#).unwrap();
        execute(&path,Operation::Path{create:true}).unwrap();
        let settings=super::super::load(&path).unwrap();
        assert_eq!(settings.terminal_font_size,18.0);
        assert_eq!(settings.default_column_fraction,0.65);
        assert!(path.is_file());assert!(!legacy.exists());
    }

    #[test]
    fn settings_commands_share_validation_and_preserve_unrelated_values() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("settings.toml");
        execute(&path, Operation::Path { create: true }).unwrap();
        let result = execute(&path, Operation::Set { key: "terminal_font_size".into(), value: "15".into() }).unwrap();
        assert_eq!(result["value"], 15.0);
        assert_eq!(super::super::load(&path).unwrap().terminal_font_size, 15.0);
        let error = execute(&path, Operation::Set { key: "terminal_font_size".into(), value: "1".into() }).unwrap_err();
        let mut invalid = AppSettings::default(); invalid.terminal_font_size = 1.0;
        assert_eq!(error, invalid.validate().unwrap_err().to_string());
        assert!(execute(&path, Operation::Set { key: "nonexistent".into(), value: "true".into() }).unwrap_err().contains("Unknown setting"));
        assert_eq!(execute(&path, Operation::Get { key: "terminal_font_size".into() }).unwrap()["value"], 15.0);
        assert_eq!(execute(&path, Operation::Get { key: "editor_font_size".into() }).unwrap()["value"], Value::Null);
    }
}
