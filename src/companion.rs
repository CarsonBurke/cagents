//! Fixed executable names within an already selected matched installation.
//! This module constructs paths only: it never searches PATH/current directory
//! or guesses a different installation when a companion is missing.
use std::{
    ffi::OsString,
    io,
    path::{Path, PathBuf},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Product {
    Cli,
    Ui,
    Daemon,
    Editor,
}
impl Product {
    pub fn file_name(self) -> OsString {
        self.file_name_with_suffix(std::env::consts::EXE_SUFFIX)
    }
    fn file_name_with_suffix(self, suffix: &str) -> OsString {
        let mut name = OsString::from(match self {
            Self::Cli => "cagents",
            Self::Ui => "cagents-ui",
            Self::Daemon => "cagentsd",
            Self::Editor => "ceditor",
        });
        name.push(suffix);
        name
    }
    /// Resolve only beside the supplied absolute executable, preserving its
    /// installation even when a staged update changes another directory.
    pub fn beside(self, executable: &Path) -> io::Result<PathBuf> {
        if !executable.is_absolute() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "the selected executable path must be absolute",
            ));
        }
        let directory = executable
            .parent()
            .filter(|_| executable.file_name().is_some())
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::NotFound,
                    "the selected executable has no parent directory",
                )
            })?;
        Ok(directory.join(self.file_name()))
    }
    pub fn beside_current(self) -> io::Result<PathBuf> {
        self.beside(&std::env::current_exe()?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn fixed_names_apply_the_suffix_once() {
        for (product, stem) in [
            (Product::Cli, "cagents"),
            (Product::Ui, "cagents-ui"),
            (Product::Daemon, "cagentsd"),
            (Product::Editor, "ceditor"),
        ] {
            assert_eq!(product.file_name_with_suffix(""), OsString::from(stem));
            assert_eq!(
                product.file_name_with_suffix(".exe"),
                OsString::from(format!("{stem}.exe"))
            );
            assert_eq!(
                product.file_name(),
                OsString::from(format!("{stem}{}", std::env::consts::EXE_SUFFIX))
            );
        }
    }
    #[test]
    fn lookup_stays_with_the_selected_installation_and_does_not_search() {
        let root = tempfile::tempdir().unwrap();
        let old = root.path().join("versions/old/bin");
        let staged = root.path().join("versions/staged/bin");
        std::fs::create_dir_all(&old).unwrap();
        std::fs::create_dir_all(&staged).unwrap();
        std::fs::write(
            staged.join(Product::Ui.file_name()),
            b"different installation",
        )
        .unwrap();
        let old_cli = old.join(Product::Cli.file_name());
        // Even a missing old companion must not silently select the staged UI.
        assert_eq!(
            Product::Ui.beside(&old_cli).unwrap(),
            old.join(Product::Ui.file_name())
        );
        assert!(!Product::Ui.beside(&old_cli).unwrap().exists());
        assert!(Product::Ui.beside(Path::new("relative/cagents")).is_err());
        assert!(
            Product::Ui
                .beside(Path::new(std::path::MAIN_SEPARATOR_STR))
                .is_err()
        );
    }
    #[cfg(unix)]
    #[test]
    fn companion_paths_preserve_non_utf8_parent_bytes() {
        use std::os::unix::ffi::{OsStrExt, OsStringExt};
        let parent = PathBuf::from(OsString::from_vec(b"/installed/\xff space".to_vec()));
        let path = Product::Daemon
            .beside(&parent.join(Product::Cli.file_name()))
            .unwrap();
        assert_eq!(
            path.parent().unwrap().as_os_str().as_bytes(),
            parent.as_os_str().as_bytes()
        );
        assert_eq!(path.file_name().unwrap(), Product::Daemon.file_name());
    }
}
