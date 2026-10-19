//! Daemon-owned resident admission preference, separate from window settings.
pub const DEFAULT_LIMIT: u32 = 128;
pub const MIN_LIMIT: u32 = 1;
/// An operator-selected safety bound, not a promise that 4096 providers fit in RAM.
pub const MAX_LIMIT: u32 = 4096;

pub fn validate(limit: u32) -> Result<(), String> {
    if (MIN_LIMIT..=MAX_LIMIT).contains(&limit) {
        Ok(())
    } else {
        Err(format!(
            "native chat capacity must be {MIN_LIMIT}..={MAX_LIMIT}"
        ))
    }
}

#[cfg(feature = "host")]
mod persistence {
    use super::*;
    use crate::fs_safety::{self, Root};
    use std::{
        io::{self, Write},
        path::{Path, PathBuf},
    };

    const FILE_NAME: &str = "native-chat-capacity.toml";
    const MAX_BYTES: u64 = 1024;

    #[derive(serde::Serialize, serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Configuration {
        limit: u32,
    }

    fn directory() -> io::Result<PathBuf> {
        crate::instance::project_dirs()
            .map(|dirs| dirs.config_dir().to_path_buf())
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::NotFound,
                    "configuration directory unavailable",
                )
            })
    }

    pub fn load() -> io::Result<u32> {
        load_from(&directory()?)
    }

    pub fn save(limit: u32) -> io::Result<()> {
        save_to(&directory()?, limit)
    }

    fn load_from(directory: &Path) -> io::Result<u32> {
        crate::secure_file::reject_symlink_components(directory)?;
        let root = match Root::open(directory) {
            Ok(root) => root,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(DEFAULT_LIMIT),
            Err(error) => return Err(error),
        };
        let mut file = match fs_safety::open_beneath(&root, Path::new(FILE_NAME)) {
            Ok(file) => file,
            Err(error) if error.is_not_found() => return Ok(DEFAULT_LIMIT),
            Err(error) => return Err(io::Error::other(error)),
        };
        crate::secure_file::validate_and_privatize_open_file(&directory.join(FILE_NAME), &file)?;
        let bytes = fs_safety::read_bounded(&mut file, MAX_BYTES)?;
        let text = std::str::from_utf8(&bytes)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        let config: Configuration =
            toml::from_str(text).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        validate(config.limit).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        Ok(config.limit)
    }

    fn save_to(directory: &Path, limit: u32) -> io::Result<()> {
        validate(limit).map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
        crate::secure_file::prepare_private_directory(directory)?;
        let root = Root::open(directory)?;
        let document = toml::to_string(&Configuration { limit }).map_err(io::Error::other)?;
        let (mut file, name) =
            fs_safety::create_temporary_in(&root, 0o600).map_err(io::Error::other)?;
        let result = (|| {
            file.write_all(document.as_bytes())?;
            file.sync_all()?;
            fs_safety::rename_within(&root, &name, FILE_NAME.as_ref()).map_err(io::Error::other)?;
            fs_safety::sync_directory(&root)
        })();
        if result.is_err() {
            let _ = fs_safety::unlink_within(&root, &name);
        }
        result
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        #[test]
        fn capacity_persists_bounds_and_rejects_invalid_without_overwrite() {
            let temp = tempfile::tempdir().unwrap();
            let path = temp.path().join("configuration");
            assert_eq!(load_from(&path).unwrap(), DEFAULT_LIMIT);
            for limit in [MIN_LIMIT, MAX_LIMIT, 17] {
                save_to(&path, limit).unwrap();
                assert_eq!(load_from(&path).unwrap(), limit);
            }
            for limit in [0, MAX_LIMIT + 1, u32::MAX] {
                assert!(save_to(&path, limit).is_err());
                assert_eq!(load_from(&path).unwrap(), 17);
            }
        }
        #[test]
        fn unsafe_or_malformed_capacity_file_is_not_followed_or_defaulted() {
            use std::os::unix::fs::symlink;
            let temp = tempfile::tempdir().unwrap();
            let path = temp.path().join(FILE_NAME);
            let outside = temp.path().join("outside");
            std::fs::write(&outside, "limit = 33\n").unwrap();
            symlink(&outside, &path).unwrap();
            assert!(load_from(temp.path()).is_err());
            std::fs::remove_file(path).unwrap();
            for text in [
                "limit = 0",
                "limit = 4097",
                "limit = '128'",
                "limit = 17\nunknown = 1",
            ] {
                std::fs::write(temp.path().join(FILE_NAME), text).unwrap();
                assert!(load_from(temp.path()).is_err());
            }
        }
    }
}
#[cfg(feature = "host")]
pub(crate) use persistence::{load, save};
