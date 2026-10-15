//! Lossless selected-file paths, with legacy JSON strings and binary framing.
//!
//! Human-readable snapshots preserve ordinary strings and use an explicit
//! `unix_bytes` object only when UTF-8 cannot represent the selected filename.
//! Binary paths remain an option followed by length-prefixed bytes, identical
//! to the existing UTF-8 string framing. Protocol 59 permits undecodable bytes.
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::{
    ffi::OsString,
    os::unix::ffi::{OsStrExt, OsStringExt},
    path::{Path, PathBuf},
};

pub(crate) fn serialize<S: Serializer>(
    path: &Option<PathBuf>,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    match path {
        Some(path) => serializer.serialize_some(&PathRef(path)),
        None => serializer.serialize_none(),
    }
}

pub(crate) fn deserialize<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<PathBuf>, D::Error> {
    Option::<OwnedPath>::deserialize(deserializer).map(|path| path.map(|path| path.0))
}

/// Required paths retain the same byte-preserving framing without an option.
pub(crate) mod required {
    use super::*;
    pub(crate) fn serialize<S:Serializer>(path:&PathBuf,serializer:S)->Result<S::Ok,S::Error> {
        PathRef(path).serialize(serializer)
    }
    pub(crate) fn deserialize<'de,D:Deserializer<'de>>(deserializer:D)->Result<PathBuf,D::Error> {
        OwnedPath::deserialize(deserializer).map(|path|path.0)
    }
}

struct PathRef<'a>(&'a Path);
impl Serialize for PathRef<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        if self.0.as_os_str().as_bytes().contains(&0) {
            return Err(serde::ser::Error::custom(
                "selected file path contains a NUL byte",
            ));
        }
        if serializer.is_human_readable() {
            if let Some(path) = self.0.to_str() {
                return serializer.serialize_str(path);
            }
            #[derive(Serialize)]
            struct UnixPath<'a> {
                unix_bytes: &'a serde_bytes::Bytes,
            }
            UnixPath {
                unix_bytes: serde_bytes::Bytes::new(self.0.as_os_str().as_bytes()),
            }
            .serialize(serializer)
        } else {
            serde_bytes::Bytes::new(self.0.as_os_str().as_bytes()).serialize(serializer)
        }
    }
}

struct OwnedPath(PathBuf);
impl<'de> Deserialize<'de> for OwnedPath {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let bytes = if deserializer.is_human_readable() {
            #[derive(Deserialize)]
            #[serde(deny_unknown_fields)]
            struct UnixPath {
                unix_bytes: serde_bytes::ByteBuf,
            }
            #[derive(Deserialize)]
            #[serde(untagged)]
            enum SavedPath {
                Text(String),
                Unix(UnixPath),
            }
            match SavedPath::deserialize(deserializer)? {
                SavedPath::Text(path) => path.into_bytes(),
                SavedPath::Unix(path) => path.unix_bytes.into_vec(),
            }
        } else {
            serde_bytes::ByteBuf::deserialize(deserializer)?.into_vec()
        };
        if bytes.contains(&0) {
            return Err(serde::de::Error::custom(
                "selected file path contains a NUL byte",
            ));
        }
        Ok(Self(PathBuf::from(OsString::from_vec(bytes))))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[derive(Debug, Serialize, Deserialize, PartialEq)]
    struct Selection {
        #[serde(default, with = "super")]
        selected_file: Option<PathBuf>,
    }
    #[derive(Serialize, Deserialize)]
    struct Legacy {
        selected_file: Option<PathBuf>,
    }

    #[test]
    fn selected_paths_preserve_legacy_json_and_binary_golden_bytes() {
        for path in [
            None,
            Some(PathBuf::from("dir/a.txt")),
            Some(PathBuf::from("line\nλ.txt")),
        ] {
            let selection = Selection {
                selected_file: path.clone(),
            };
            let legacy = Legacy {
                selected_file: path,
            };
            let encoded = postcard::to_allocvec(&selection).unwrap();
            assert_eq!(encoded, postcard::to_allocvec(&legacy).unwrap());
            let old: Legacy = postcard::from_bytes(&encoded).unwrap();
            assert_eq!(old.selected_file, selection.selected_file);
            assert_eq!(
                postcard::from_bytes::<Selection>(&encoded).unwrap(),
                selection
            );
            let json = serde_json::to_string(&legacy).unwrap();
            assert_eq!(serde_json::to_string(&selection).unwrap(), json);
            assert_eq!(serde_json::from_str::<Selection>(&json).unwrap(), selection);
        }
        let path = Selection {
            selected_file: Some("dir/a.txt".into()),
        };
        assert_eq!(postcard::to_allocvec(&path).unwrap(), b"\x01\x09dir/a.txt");
        assert_eq!(
            postcard::to_allocvec(&Selection {
                selected_file: None
            })
            .unwrap(),
            b"\x00"
        );
        assert_eq!(
            serde_json::from_str::<Selection>("{}")
                .unwrap()
                .selected_file,
            None
        );
    }

    #[test]
    fn selected_paths_round_trip_raw_unix_bytes_in_json_and_binary() {
        let selection = Selection {
            selected_file: Some(PathBuf::from(OsString::from_vec(b"raw-\xff.txt".to_vec()))),
        };
        let json = serde_json::to_value(&selection).unwrap();
        assert_eq!(
            json["selected_file"]["unix_bytes"],
            serde_json::json!([114, 97, 119, 45, 255, 46, 116, 120, 116])
        );
        assert_eq!(
            serde_json::from_value::<Selection>(json).unwrap(),
            selection
        );
        let encoded = postcard::to_allocvec(&selection).unwrap();
        assert_eq!(encoded, b"\x01\x09raw-\xff.txt");
        assert_eq!(
            postcard::from_bytes::<Selection>(&encoded).unwrap(),
            selection
        );
    }

    #[test]
    fn repository_views_keep_legacy_framing_and_round_trip_byte_paths() {
        use crate::model::{ProjectId, RepositoryView};
        #[derive(Serialize)]
        struct LegacyView {
            project: ProjectId,
            repository: PathBuf,
            worktree: PathBuf,
            #[serde(skip_serializing_if = "Option::is_none")]
            selected_file: Option<PathBuf>,
        }
        for selected_file in [
            None,
            Some("normal.txt".into()),
            Some(PathBuf::from(OsString::from_vec(b"raw-\xff.txt".to_vec()))),
        ] {
            let view = RepositoryView {
                project: ProjectId::new(),
                repository: "/repo/.git".into(),
                worktree: "/repo".into(),
                selected_file,
            };
            let binary = postcard::to_allocvec(&view).unwrap();
            // Direct binary None was never framed by RepositoryView's legacy
            // skip-if-None JSON annotation; shared IPC uses wire_kind's explicit
            // option. Verify the direct binary forms actually carrying a path.
            if view.selected_file.is_some() {
                assert_eq!(
                    postcard::from_bytes::<RepositoryView>(&binary).unwrap(),
                    view
                );
            }
            assert_eq!(
                serde_json::from_str::<RepositoryView>(&serde_json::to_string(&view).unwrap())
                    .unwrap(),
                view
            );
            if view
                .selected_file
                .as_ref()
                .is_none_or(|path| path.to_str().is_some())
            {
                let legacy = LegacyView {
                    project: view.project,
                    repository: view.repository.clone(),
                    worktree: view.worktree.clone(),
                    selected_file: view.selected_file.clone(),
                };
                assert_eq!(binary, postcard::to_allocvec(&legacy).unwrap());
            }
        }
    }

    #[test]
    fn malformed_selected_path_tags_and_nul_are_rejected() {
        for json in [
            r#"{"selected_file":{"unix_bytes":[255],"unexpected":true}}"#,
            r#"{"selected_file":{"unix_bytes":[256]}}"#,
            r#"{"selected_file":{"unix_bytes":[0]}}"#,
            r#"{"selected_file":"bad\u0000path"}"#,
        ] {
            assert!(serde_json::from_str::<Selection>(json).is_err(), "{json}");
        }
        assert!(postcard::from_bytes::<Selection>(b"\x01\x01\x00").is_err());
        let invalid = Selection {
            selected_file: Some(PathBuf::from(OsString::from_vec(b"bad\x00path".to_vec()))),
        };
        assert!(serde_json::to_vec(&invalid).is_err());
        assert!(postcard::to_allocvec(&invalid).is_err());
    }
}
