use cagents::sidebar::FileListMode;
use std::{
    io::{Read, Write},
    path::Path,
};

pub fn load(settings: &Path) -> Result<FileListMode, String> {
    let path = settings.with_file_name("file-list-mode");
    let mut value = String::new();
    match std::fs::File::open(&path) {
        Ok(file) => {
            file.take(16)
                .read_to_string(&mut value)
                .map_err(|error| error.to_string())?;
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(FileListMode::Tree);
        }
        Err(error) => return Err(format!("{}: {error}", path.display())),
    }
    match value.trim() {
        "tree" => Ok(FileListMode::Tree),
        "flat" => Ok(FileListMode::Flat),
        _ => Err(format!("Invalid file-list mode in {}", path.display())),
    }
}

pub fn save(settings: &Path, mode: FileListMode) -> Result<(), String> {
    let path = settings.with_file_name("file-list-mode");
    let directory = path.parent().ok_or("Settings directory is unavailable")?;
    // The shared settings writer creates and validates this directory first.
    let mut temporary =
        tempfile::NamedTempFile::new_in(directory).map_err(|error| error.to_string())?;
    temporary
        .write_all(match mode {
            FileListMode::Tree => b"tree\n",
            FileListMode::Flat => b"flat\n",
        })
        .and_then(|()| temporary.as_file().sync_all())
        .map_err(|error| error.to_string())?;
    temporary
        .persist(&path)
        .map_err(|error| error.to_string())?;
    std::fs::File::open(directory)
        .and_then(|directory| directory.sync_all())
        .map_err(|error| error.to_string())
}
