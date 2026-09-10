//! Safe single-file loading and atomic saving for native repository editors.

use std::{
    fs::{self, File, Permissions},
    io::{self, Cursor, Read, Write},
    path::{Component, Path, PathBuf},
};

use thiserror::Error;

/// Files above this size are never loaded into an editable buffer.
pub const MAX_EDITABLE_FILE_BYTES: u64 = 8 * 1024 * 1024;

/// Bound both decoder working memory and the final GPU upload.
const MAX_IMAGE_DIMENSION: u32 = 4096;
const MAX_IMAGE_PIXELS: u64 = 16 * 1024 * 1024;
const MAX_IMAGE_DECODE_BYTES: u64 = 128 * 1024 * 1024;
const MAX_SVG_BYTES: usize = 256 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReadOnlyReason {
    Symlink { target: PathBuf },
    Binary,
    InvalidUtf8,
    Image(String),
    Oversize { bytes: u64, limit: u64 },
    NotRegularFile,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileContent {
    Editable {
        text: String,
        content_hash: blake3::Hash,
        markdown: bool,
    },
    /// Already decoded off the UI thread; never enters the text/save pipeline.
    Image {
        width: u32,
        height: u32,
        rgba: std::sync::Arc<[u8]>,
    },
    ReadOnly(ReadOnlyReason),
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum FileIoError {
    #[error("repository path must be a non-empty relative path without traversal")]
    InvalidRepositoryPath,
    #[error("repository root is unavailable: {0}")]
    InvalidRepositoryRoot(String),
    #[error("path escapes the repository worktree")]
    OutsideWorktree,
    #[error("file operation failed: {0}")]
    Io(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SaveOutcome {
    Saved {
        content_hash: blake3::Hash,
        durability: SaveDurability,
    },
    Conflict {
        disk_hash: blake3::Hash,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SaveDurability {
    /// The replacement and its parent-directory entry were synced.
    Durable,
    /// The destination was replaced, but syncing its parent directory failed.
    /// The new bytes are the live disk baseline even though crash durability is
    /// not guaranteed.
    Unconfirmed { error: String },
}

pub fn validate_repo_path(path: &Path) -> Result<(), FileIoError> {
    if path.as_os_str().is_empty() || path.is_absolute() {
        return Err(FileIoError::InvalidRepositoryPath);
    }
    if path
        .components()
        .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(FileIoError::InvalidRepositoryPath);
    }
    Ok(())
}

pub fn load_file(worktree: &Path, relative_path: &Path) -> Result<FileContent, FileIoError> {
    let path = checked_path(worktree, relative_path)?;
    let metadata = fs::symlink_metadata(&path).map_err(io_error)?;
    if metadata.file_type().is_symlink() {
        return fs::read_link(&path)
            .map(|target| FileContent::ReadOnly(ReadOnlyReason::Symlink { target }))
            .map_err(io_error);
    }
    if !metadata.is_file() {
        return Ok(FileContent::ReadOnly(ReadOnlyReason::NotRegularFile));
    }
    if metadata.len() > MAX_EDITABLE_FILE_BYTES {
        return Ok(FileContent::ReadOnly(ReadOnlyReason::Oversize {
            bytes: metadata.len(),
            limit: MAX_EDITABLE_FILE_BYTES,
        }));
    }

    // A file can grow after metadata was read. Bound the read itself, not only
    // its result, so a concurrently growing file cannot exhaust memory.
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    File::open(&path)
        .map_err(io_error)?
        .take(MAX_EDITABLE_FILE_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(io_error)?;
    if bytes.len() as u64 > MAX_EDITABLE_FILE_BYTES {
        return Ok(FileContent::ReadOnly(ReadOnlyReason::Oversize {
            bytes: bytes.len() as u64,
            limit: MAX_EDITABLE_FILE_BYTES,
        }));
    }
    if relative_path
        .extension()
        .is_some_and(|extension| extension.eq_ignore_ascii_case("svg"))
    {
        return Ok(decode_svg(&bytes)
            .unwrap_or_else(|reason| FileContent::ReadOnly(ReadOnlyReason::Image(reason))));
    }
    let format = image::guess_format(&bytes)
        .ok()
        .or_else(|| image::ImageFormat::from_path(relative_path).ok());
    if let Some(format) = format {
        return Ok(decode_raster(&bytes, format)
            .unwrap_or_else(|reason| FileContent::ReadOnly(ReadOnlyReason::Image(reason))));
    }
    if bytes.contains(&0) {
        return Ok(FileContent::ReadOnly(ReadOnlyReason::Binary));
    }
    let text = match String::from_utf8(bytes) {
        Ok(text) => text,
        Err(_) => return Ok(FileContent::ReadOnly(ReadOnlyReason::InvalidUtf8)),
    };
    let markdown = relative_path
        .extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| {
            matches!(extension.to_ascii_lowercase().as_str(), "md" | "markdown")
        });
    Ok(FileContent::Editable {
        content_hash: blake3::hash(text.as_bytes()),
        text,
        markdown,
    })
}

fn check_image_dimensions(width: u32, height: u32) -> Result<(), String> {
    if width == 0
        || height == 0
        || width > MAX_IMAGE_DIMENSION
        || height > MAX_IMAGE_DIMENSION
        || u64::from(width) * u64::from(height) > MAX_IMAGE_PIXELS
    {
        return Err(format!(
            "Image exceeds the preview limit of {MAX_IMAGE_DIMENSION} × {MAX_IMAGE_DIMENSION} pixels."
        ));
    }
    Ok(())
}

fn decode_raster(bytes: &[u8], format: image::ImageFormat) -> Result<FileContent, String> {
    use image::ImageDecoder;
    let mut reader = image::ImageReader::with_format(Cursor::new(bytes), format);
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(MAX_IMAGE_DIMENSION);
    limits.max_image_height = Some(MAX_IMAGE_DIMENSION);
    limits.max_alloc = Some(MAX_IMAGE_DECODE_BYTES);
    reader.limits(limits);
    let decoder = reader.into_decoder().map_err(|error| error.to_string())?;
    let (width, height) = decoder.dimensions();
    check_image_dimensions(width, height)?;
    let rgba = image::DynamicImage::from_decoder(decoder)
        .map_err(|error| error.to_string())?
        .into_rgba8()
        .into_raw();
    Ok(FileContent::Image {
        width,
        height,
        rgba: rgba.into(),
    })
}

fn decode_svg(bytes: &[u8]) -> Result<FileContent, String> {
    if bytes.len() > MAX_SVG_BYTES {
        return Err("SVG exceeds the 256 KiB safe preview limit.".into());
    }
    let text = std::str::from_utf8(bytes).map_err(|error| error.to_string())?;
    // Parse with DTDs disabled before usvg. Bound reference expansion before
    // conversion, then bound intermediate surfaces in the resolved render tree.
    let xml = roxmltree::Document::parse(text).map_err(|error| error.to_string())?;
    for (index, node) in xml.descendants().enumerate() {
        if index >= 4096 || node.ancestors().take(33).count() > 32 {
            return Err("SVG exceeds the safe preview complexity limit.".into());
        }
        if !node.is_element() {
            continue;
        }
        if matches!(
            node.tag_name().name(),
            "image" | "filter" | "mask" | "pattern" | "foreignObject"
        ) {
            return Err("This SVG uses embedded resources or effects that exceed the safe native preview contract. Open it externally to view the full image.".into());
        }
    }
    let ids: std::collections::HashMap<_, _> = xml
        .descendants()
        .filter_map(|node| node.attribute("id").map(|id| (id, node)))
        .collect();
    check_svg_expansion(
        xml.root_element(),
        &ids,
        0,
        &mut 4096,
        &mut (MAX_EDITABLE_FILE_BYTES as usize),
    )?;
    let options = resvg::usvg::Options {
        image_href_resolver: resvg::usvg::ImageHrefResolver {
            resolve_data: Box::new(|_, _, _| None),
            resolve_string: Box::new(|_, _| None),
        },
        // Reuse the application's already-discovered fonts, without rescanning
        // the filesystem or resolving any SVG-provided font file.
        fontdb: std::sync::Arc::new(
            cosmic::iced::advanced::graphics::text::font_system()
                .read()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .db()
                .clone(),
        ),
        ..Default::default()
    };
    let tree = resvg::usvg::Tree::from_str(text, &options).map_err(|error| error.to_string())?;
    let size = tree.size().to_int_size();
    check_image_dimensions(size.width(), size.height())?;
    let mut layer_bytes = u64::from(size.width()) * u64::from(size.height()) * 4;
    check_svg_layers(tree.root(), 0, &mut layer_bytes, &mut 4096)?;
    let mut pixmap = resvg::tiny_skia::Pixmap::new(size.width(), size.height())
        .ok_or_else(|| "Could not allocate the bounded SVG preview.".to_string())?;
    resvg::render(
        &tree,
        resvg::tiny_skia::Transform::identity(),
        &mut pixmap.as_mut(),
    );
    // tiny-skia stores premultiplied alpha; Iced's RGBA handle expects straight
    // alpha. Unpremultiply in place, preserving transparent pixels.
    for pixel in pixmap.data_mut().chunks_exact_mut(4) {
        let alpha = u32::from(pixel[3]);
        if alpha != 0 {
            for channel in &mut pixel[..3] {
                *channel = ((u32::from(*channel) * 255 + alpha / 2) / alpha).min(255) as u8;
            }
        }
    }
    Ok(FileContent::Image {
        width: size.width(),
        height: size.height(),
        rgba: pixmap.take().into(),
    })
}

fn check_svg_expansion<'a, 'input>(
    node: roxmltree::Node<'a, 'input>,
    ids: &std::collections::HashMap<&'a str, roxmltree::Node<'a, 'input>>,
    depth: usize,
    remaining: &mut usize,
    remaining_bytes: &mut usize,
) -> Result<(), String> {
    if depth > 32 || *remaining == 0 {
        return Err("SVG exceeds the safe preview reference/complexity limit.".into());
    }
    *remaining -= 1;
    // A small number of references can still repeat one enormous path. Charge
    // its source bytes on every expansion as well as charging element count.
    *remaining_bytes = remaining_bytes
        .checked_sub(node.range().len())
        .ok_or_else(|| "SVG references exceed the safe preview expansion limit.".to_string())?;
    if node.has_tag_name("use") {
        let href = node
            .attributes()
            .find(|attribute| attribute.name() == "href")
            .map(|attribute| attribute.value())
            .unwrap_or("");
        if let Some(id) = href.strip_prefix('#') {
            if let Some(target) = ids.get(id) {
                check_svg_expansion(*target, ids, depth + 1, remaining, remaining_bytes)?;
            }
        } else if !href.is_empty() {
            return Err("External SVG references are not supported by the safe preview.".into());
        }
    }
    for child in node.children() {
        check_svg_expansion(child, ids, depth + 1, remaining, remaining_bytes)?;
    }
    Ok(())
}

fn check_svg_layers(
    group: &resvg::usvg::Group,
    depth: usize,
    bytes: &mut u64,
    remaining: &mut usize,
) -> Result<(), String> {
    if depth > 32 || *remaining == 0 {
        return Err("SVG exceeds the safe preview render complexity limit.".into());
    }
    *remaining -= 1;
    if !group.filters().is_empty() || group.mask().is_some() {
        return Err("SVG filters and masks are not supported by the bounded preview.".into());
    }
    if group.should_isolate() {
        let bounds = group.abs_layer_bounding_box();
        let width = bounds.width().ceil() as u32;
        let height = bounds.height().ceil() as u32;
        check_image_dimensions(width, height)?;
        // Include rounding around the layer and a second clipping surface.
        *bytes = bytes.saturating_add((u64::from(width) + 2) * (u64::from(height) + 2) * 8);
        if *bytes > MAX_IMAGE_DECODE_BYTES {
            return Err("SVG intermediate layers exceed the safe preview memory limit.".into());
        }
    }
    for node in group.children() {
        match node {
            resvg::usvg::Node::Group(child) => {
                check_svg_layers(child, depth + 1, bytes, remaining)?
            }
            resvg::usvg::Node::Text(text) => {
                check_svg_layers(text.flattened(), depth + 1, bytes, remaining)?
            }
            _ => {
                if *remaining == 0 {
                    return Err("SVG exceeds the safe preview render complexity limit.".into());
                }
                *remaining -= 1;
            }
        }
    }
    Ok(())
}

pub fn save_file(
    worktree: &Path,
    relative_path: &Path,
    text: &str,
    expected_disk_hash: blake3::Hash,
    overwrite: bool,
) -> Result<SaveOutcome, FileIoError> {
    save_file_with_atomic_replace(
        worktree,
        relative_path,
        text,
        expected_disk_hash,
        overwrite,
        atomic_replace,
    )
}

fn save_file_with_atomic_replace(
    worktree: &Path,
    relative_path: &Path,
    text: &str,
    expected_disk_hash: blake3::Hash,
    overwrite: bool,
    replace: impl FnOnce(&Path, &[u8], Permissions) -> Result<SaveDurability, FileIoError>,
) -> Result<SaveOutcome, FileIoError> {
    let path = checked_path(worktree, relative_path)?;
    let metadata = fs::symlink_metadata(&path).map_err(io_error)?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(FileIoError::Io(
            "the destination is no longer a regular file".into(),
        ));
    }

    let disk = fs::read(&path).map_err(io_error)?;
    let disk_hash = blake3::hash(&disk);
    if !overwrite && disk_hash != expected_disk_hash {
        return Ok(SaveOutcome::Conflict { disk_hash });
    }

    let durability = replace(&path, text.as_bytes(), metadata.permissions())?;
    Ok(SaveOutcome::Saved {
        content_hash: blake3::hash(text.as_bytes()),
        durability,
    })
}

fn checked_path(worktree: &Path, relative_path: &Path) -> Result<PathBuf, FileIoError> {
    validate_repo_path(relative_path)?;
    let root = fs::canonicalize(worktree)
        .map_err(|error| FileIoError::InvalidRepositoryRoot(error.to_string()))?;
    let path = root.join(relative_path);
    let parent = path.parent().ok_or(FileIoError::InvalidRepositoryPath)?;
    let canonical_parent = fs::canonicalize(parent).map_err(io_error)?;
    if !canonical_parent.starts_with(&root) {
        return Err(FileIoError::OutsideWorktree);
    }
    Ok(path)
}

fn atomic_replace(
    path: &Path,
    bytes: &[u8],
    permissions: Permissions,
) -> Result<SaveDurability, FileIoError> {
    atomic_replace_with_hooks(
        path,
        bytes,
        permissions,
        || Ok(()),
        |parent| File::open(parent).and_then(|directory| directory.sync_all()),
    )
}

fn atomic_replace_with_hooks(
    path: &Path,
    bytes: &[u8],
    permissions: Permissions,
    before_persist: impl FnOnce() -> io::Result<()>,
    sync_parent: impl FnOnce(&Path) -> io::Result<()>,
) -> Result<SaveDurability, FileIoError> {
    let parent = path.parent().ok_or(FileIoError::InvalidRepositoryPath)?;
    let mut temporary = tempfile::NamedTempFile::new_in(parent).map_err(io_error)?;
    temporary
        .as_file_mut()
        .set_permissions(permissions)
        .map_err(io_error)?;
    temporary.write_all(bytes).map_err(io_error)?;
    temporary.flush().map_err(io_error)?;
    temporary.as_file().sync_all().map_err(io_error)?;
    before_persist().map_err(io_error)?;
    temporary
        .persist(path)
        .map_err(|error| io_error(error.error))?;
    // `persist` has replaced the destination. From this point onward, an I/O
    // failure must be reported as a successful replacement with unconfirmed
    // durability; returning `Err` would falsely tell callers the old baseline
    // is still on disk.
    Ok(match sync_parent(parent) {
        Ok(()) => SaveDurability::Durable,
        Err(error) => SaveDurability::Unconfirmed {
            error: error.to_string(),
        },
    })
}

fn io_error(error: io::Error) -> FileIoError {
    FileIoError::Io(error.to_string())
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::{PermissionsExt, symlink};

    use tempfile::tempdir;

    use super::*;

    #[test]
    fn raster_file_load_preserves_pixels_without_creating_editable_text() {
        let root = tempdir().unwrap();
        let path = root.path().join("opaque.dat");
        let image = image::RgbaImage::from_pixel(2, 1, image::Rgba([31, 127, 220, 128]));
        image
            .save_with_format(&path, image::ImageFormat::Png)
            .unwrap();
        let original = fs::read(&path).unwrap();
        let FileContent::Image {
            width,
            height,
            rgba,
        } = load_file(root.path(), Path::new("opaque.dat")).unwrap()
        else {
            panic!("image magic must win over the filename extension");
        };
        assert_eq!((width, height), (2, 1));
        assert_eq!(&*rgba, &[31, 127, 220, 128, 31, 127, 220, 128]);
        assert_eq!(fs::read(&path).unwrap(), original);
    }

    #[test]
    fn image_headers_cannot_bypass_pixel_or_file_limits() {
        let root = tempdir().unwrap();
        let path = root.path().join("wide.png");
        image::RgbaImage::new(MAX_IMAGE_DIMENSION + 1, 1)
            .save_with_format(&path, image::ImageFormat::Png)
            .unwrap();
        assert!(matches!(
            load_file(root.path(), Path::new("wide.png")).unwrap(),
            FileContent::ReadOnly(ReadOnlyReason::Image(_))
        ));
        let file = File::create(root.path().join("huge.png")).unwrap();
        file.set_len(MAX_EDITABLE_FILE_BYTES + 1).unwrap();
        assert!(matches!(
            load_file(root.path(), Path::new("huge.png")).unwrap(),
            FileContent::ReadOnly(ReadOnlyReason::Oversize { .. })
        ));
        fs::write(root.path().join("broken.png"), b"not a valid image").unwrap();
        assert!(matches!(
            load_file(root.path(), Path::new("broken.png")).unwrap(),
            FileContent::ReadOnly(ReadOnlyReason::Image(_))
        ));
    }

    #[test]
    fn svg_styles_and_internal_references_render_with_straight_alpha() {
        let svg = br##"<svg xmlns="http://www.w3.org/2000/svg" width="2" height="2">
            <style>.color { fill: #ff0000; fill-opacity: 0.5; }</style>
            <defs><rect id="square" class="color" width="2" height="2"/></defs>
            <use href="#square"/>
        </svg>"##;
        let FileContent::Image {
            width,
            height,
            rgba,
        } = decode_svg(svg).unwrap()
        else {
            panic!("safe vector SVG should render");
        };
        assert_eq!((width, height), (2, 2));
        assert_eq!(&rgba[..4], &[255, 0, 0, 128]);
    }

    #[test]
    fn svg_rejects_resource_expansion_and_oversized_render_surfaces() {
        for svg in [
            r##"<svg xmlns="http://www.w3.org/2000/svg"><defs><g id="loop"><use href="#loop"/></g></defs><use href="#loop"/></svg>"##,
            r#"<svg xmlns="http://www.w3.org/2000/svg" width="999999" height="999999"/>"#,
            r#"<svg xmlns="http://www.w3.org/2000/svg"><image href="file:///etc/passwd"/></svg>"#,
            r#"<!DOCTYPE svg [<!ENTITY x "expanded">]><svg xmlns="http://www.w3.org/2000/svg"><text>&x;</text></svg>"#,
            r#"<svg xmlns="http://www.w3.org/2000/svg"><rect width="10" height="10" style="filter:blur(2px)"/></svg>"#,
        ] {
            assert!(
                decode_svg(svg.as_bytes()).is_err(),
                "unsafe SVG was accepted: {svg}"
            );
        }
    }

    #[test]
    fn classifies_unsafe_file_inputs_without_partial_text() {
        let root = tempdir().unwrap();
        fs::write(root.path().join("binary.bin"), b"hello\0world").unwrap();
        fs::write(root.path().join("invalid.txt"), [0xff, 0xfe]).unwrap();
        symlink("binary.bin", root.path().join("link.txt")).unwrap();

        assert_eq!(
            load_file(root.path(), Path::new("binary.bin")).unwrap(),
            FileContent::ReadOnly(ReadOnlyReason::Binary)
        );
        assert_eq!(
            load_file(root.path(), Path::new("invalid.txt")).unwrap(),
            FileContent::ReadOnly(ReadOnlyReason::InvalidUtf8)
        );
        assert_eq!(
            load_file(root.path(), Path::new("link.txt")).unwrap(),
            FileContent::ReadOnly(ReadOnlyReason::Symlink {
                target: PathBuf::from("binary.bin")
            })
        );
    }

    #[test]
    fn atomic_save_preserves_mode_and_detects_external_changes() {
        let root = tempdir().unwrap();
        let path = root.path().join("notes.md");
        fs::write(&path, "before\n").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o640)).unwrap();
        let expected = blake3::hash(b"before\n");

        fs::write(&path, "external\n").unwrap();
        assert!(matches!(
            save_file(
                root.path(),
                Path::new("notes.md"),
                "mine\n",
                expected,
                false
            )
            .unwrap(),
            SaveOutcome::Conflict { .. }
        ));
        assert_eq!(fs::read_to_string(&path).unwrap(), "external\n");

        let outcome =
            save_file(root.path(), Path::new("notes.md"), "mine\n", expected, true).unwrap();
        assert!(matches!(
            outcome,
            SaveOutcome::Saved {
                durability: SaveDurability::Durable,
                ..
            }
        ));
        assert_eq!(fs::read_to_string(&path).unwrap(), "mine\n");
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o640
        );
    }

    #[test]
    fn failure_before_persist_leaves_destination_unchanged_and_returns_error() {
        let root = tempdir().unwrap();
        let path = root.path().join("notes.txt");
        fs::write(&path, "before\n").unwrap();
        let expected = blake3::hash(b"before\n");

        let result = save_file_with_atomic_replace(
            root.path(),
            Path::new("notes.txt"),
            "after\n",
            expected,
            false,
            |path, bytes, permissions| {
                atomic_replace_with_hooks(
                    path,
                    bytes,
                    permissions,
                    || Err(io::Error::other("injected failure before persist")),
                    |_| panic!("parent sync must not run when persist was not attempted"),
                )
            },
        );

        assert!(matches!(
            result,
            Err(FileIoError::Io(error)) if error.contains("injected failure before persist")
        ));
        assert_eq!(fs::read_to_string(path).unwrap(), "before\n");
    }

    #[test]
    fn parent_sync_failure_reports_replaced_but_unconfirmed_destination() {
        let root = tempdir().unwrap();
        let path = root.path().join("notes.txt");
        fs::write(&path, "before\n").unwrap();
        let expected = blake3::hash(b"before\n");

        let result = save_file_with_atomic_replace(
            root.path(),
            Path::new("notes.txt"),
            "after\n",
            expected,
            false,
            |path, bytes, permissions| {
                atomic_replace_with_hooks(
                    path,
                    bytes,
                    permissions,
                    || Ok(()),
                    |_| Err(io::Error::other("injected parent sync failure")),
                )
            },
        )
        .unwrap();

        assert!(matches!(
            result,
            SaveOutcome::Saved {
                content_hash,
                durability: SaveDurability::Unconfirmed { error },
            } if content_hash == blake3::hash(b"after\n")
                && error.contains("injected parent sync failure")
        ));
        assert_eq!(fs::read_to_string(path).unwrap(), "after\n");
    }

    #[test]
    fn rejects_traversal() {
        let root = tempdir().unwrap();
        assert_eq!(
            load_file(root.path(), Path::new("../outside")),
            Err(FileIoError::InvalidRepositoryPath)
        );
    }
}
