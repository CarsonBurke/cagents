//! Safe single-file loading and atomic saving for native repository editors.

use std::{
    fs::{self, File, Permissions},
    io::{self, Cursor, Read, Seek, Write},
    path::{Component, Path, PathBuf},
};

use thiserror::Error;

use crate::fs_safety::{self, ResolveError, Root};

/// Rasterizing a page happens in a child process, so the reader's document
/// lives behind the same module boundary either way: `load_file` opens one on
/// its first page, and the view asks for the pages after that.
pub use crate::document_render::{DocumentPage, render_document_page};

/// Files above this size are never loaded into an editable buffer.
pub const MAX_EDITABLE_FILE_BYTES: u64 = 8 * 1024 * 1024;

/// Bound both decoder working memory and the final GPU upload.
pub(crate) const MAX_IMAGE_DIMENSION: u32 = 4096;
pub(crate) const MAX_IMAGE_PIXELS: u64 = 16 * 1024 * 1024;
const MAX_IMAGE_DECODE_BYTES: u64 = 128 * 1024 * 1024;
const MAX_SVG_BYTES: usize = 256 * 1024;

/// A PDF page is rasterized at this many device pixels per PostScript point.
///
/// A point is 1/72 inch, so this puts a US Letter page at roughly 1224 by
/// 1584 pixels: sharp enough to read body text on a high-density display,
/// and well inside the same pixel budget the image preview already enforces.
pub const PDF_PAGE_SCALE: f32 = 1.7;

/// The most pages a document may declare before the preview refuses it.
///
/// What bounds the cost of a page tree is not this number but where the tree
/// is materialised: the helper process parses it, so a tree that cannot be
/// bounded at all costs one helper process rather than the window. This limit
/// is about the reader instead, because a page counter that cannot be
/// navigated is of no use to them. It is applied in the helper the moment the
/// count is known, and again to the frame the helper sends back.
pub(crate) const MAX_PDF_PAGES: usize = 4096;

/// The most bytes a paged document may occupy.
///
/// Separate from `MAX_EDITABLE_FILE_BYTES` because that limit is about the
/// cost of an editable text buffer and its undo history, which a document
/// never becomes. Real documents are far larger than any text file worth
/// editing - a scanned or figure-heavy textbook is routinely tens of
/// megabytes - so holding them to the text limit would reject the ordinary
/// case. None of those bytes stay in this process: the helper re-reads the
/// file for each page it rasterizes, so this bounds how much work a page
/// render may be handed rather than how much memory the window holds.
pub(crate) const MAX_DOCUMENT_BYTES: u64 = 192 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReadOnlyReason {
    Symlink {
        target: PathBuf,
    },
    Binary,
    InvalidUtf8,
    Image(String),
    Document(String),
    /// Past the limit that applies to `kind`, which the refusal has to name:
    /// the two limits differ by a factor of twenty-four, so a sentence about
    /// editable files is plainly false about a refused document.
    Oversize {
        bytes: u64,
        limit: u64,
        kind: OversizeKind,
    },
    NotRegularFile,
}

/// Which of the two size limits a file was held to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OversizeKind {
    /// The file would have been loaded into an editable text buffer.
    Editable,
    /// The file is a paged document, which never becomes editable and is held
    /// to the document limit instead.
    Document,
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
    /// A paged document, rasterized one page at a time in a helper process.
    ///
    /// No document bytes are retained: the helper re-reads the file per page,
    /// which keeps up to `MAX_DOCUMENT_BYTES`, and the page-tree parsing a
    /// crafted document can turn into a stack overflow, out of this process.
    /// The cost is that a page turned to after the file changed on disk comes
    /// from the new bytes, which is a far better trade than an abort that
    /// takes every unsaved buffer in the window with it.
    Pdf {
        pages: usize,
        page: PdfPage,
    },
    ReadOnly(ReadOnlyReason),
}

/// One rasterized PDF page.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PdfPage {
    pub index: usize,
    pub width: u32,
    pub height: u32,
    pub rgba: std::sync::Arc<[u8]>,
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

/// Whether the open file begins with a PDF header, leaving it rewound.
///
/// The header is read through a reference and the descriptor seeked back to
/// the start, so the caller still reads the whole file from byte zero. A short
/// file simply does not match.
fn starts_with_pdf_header(file: &File) -> io::Result<bool> {
    const HEADER: &[u8] = b"%PDF-";
    let mut probe = [0u8; HEADER.len()];
    let mut read = 0;
    while read < probe.len() {
        match (&*file).read(&mut probe[read..]) {
            Ok(0) => break,
            Ok(count) => read += count,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        }
    }
    (&*file).seek(io::SeekFrom::Start(0))?;
    Ok(&probe[..read] == HEADER)
}

/// Loads one file, already classified and decoded for the view.
///
/// `tone` only reaches the document path. A page is rasterized before the
/// view sees it, so the caller has to say which surface it is destined for;
/// the alternative is rendering every page twice whenever the shell is dark.
pub fn load_file(
    worktree: &Path,
    relative_path: &Path,
    tone: PdfTone,
) -> Result<FileContent, FileIoError> {
    validate_repo_path(relative_path)?;
    let root = worktree_root(worktree)?;
    // A symlink is the one classification that has to be made from the name
    // without opening it, because opening it is exactly what must not happen.
    let metadata = fs_safety::metadata_beneath(&root, relative_path).map_err(resolve_error)?;
    if metadata.file_type().is_symlink() {
        return fs_safety::read_link_beneath(&root, relative_path)
            .map(|target| FileContent::ReadOnly(ReadOnlyReason::Symlink { target }))
            .map_err(resolve_error);
    }
    if !metadata.is_file() {
        return Ok(FileContent::ReadOnly(ReadOnlyReason::NotRegularFile));
    }

    // Everything from here on describes the open descriptor, not the name:
    // `metadata_beneath` answered about a name, and a build, a `git checkout`
    // or a hostile local process can put a different object behind it before
    // the read. The kind and the size are therefore taken again from the file
    // whose bytes are actually about to be read.
    let file = fs_safety::open_beneath(&root, relative_path).map_err(resolve_error)?;
    let metadata = fs_safety::metadata_of(&file).map_err(io_error)?;
    if !metadata.is_file() {
        return Ok(FileContent::ReadOnly(ReadOnlyReason::NotRegularFile));
    }
    // What the file is decides how large it is allowed to be, so the header is
    // read from the open descriptor before any byte limit applies. A document
    // held to the editable-text limit would reject the ordinary case, and
    // raising that limit for everything would let an 8 megabyte text file
    // become a 192 megabyte one.
    let document = starts_with_pdf_header(&file).map_err(io_error)?;
    let kind = if document {
        OversizeKind::Document
    } else {
        OversizeKind::Editable
    };
    let limit = match kind {
        OversizeKind::Document => MAX_DOCUMENT_BYTES,
        OversizeKind::Editable => MAX_EDITABLE_FILE_BYTES,
    };
    if metadata.len() > limit {
        return Ok(FileContent::ReadOnly(ReadOnlyReason::Oversize {
            bytes: metadata.len(),
            limit,
            kind,
        }));
    }
    if document {
        // The document's bytes deliberately never enter this process. The
        // helper re-reads the file by name, which is what keeps both the
        // document and hayro's page-tree parsing - the part a crafted
        // document turns into an abort - outside the process holding the
        // reader's unsaved work. The name was just resolved beneath the
        // worktree root, and pointing the helper at a different file means
        // already having write access to the worktree, which is enough to
        // change the document itself.
        drop(file);
        let path = worktree.join(relative_path);
        // Opening is always at the default zoom, because the reader has not
        // had a chance to ask for anything else yet.
        return Ok(
            match render_document_page(&path, 0, tone, PdfZoom::default()) {
                Ok(DocumentPage { pages, page }) => FileContent::Pdf { pages, page },
                Err(reason) => FileContent::ReadOnly(ReadOnlyReason::Document(reason)),
            },
        );
    }

    // A file can grow after metadata was read. Bound the read itself, not only
    // its result, so a concurrently growing file cannot exhaust memory.
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.take(limit + 1)
        .read_to_end(&mut bytes)
        .map_err(io_error)?;
    if bytes.len() as u64 > limit {
        return Ok(FileContent::ReadOnly(ReadOnlyReason::Oversize {
            bytes: bytes.len() as u64,
            limit,
            kind,
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

pub(crate) fn check_image_dimensions(width: u32, height: u32) -> Result<(), String> {
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

/// How a PDF page's colors are adapted to the surface it is read on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PdfTone {
    /// The page exactly as the document specifies it.
    Original,
    /// The page adapted for reading on a dark surface, with `paper` as the
    /// color the document's own white becomes.
    ///
    /// The color is carried rather than assumed because it is the shell's,
    /// not the viewer's: a page recolored to absolute black is not the same
    /// shade as anything else on screen, and a reader who changed their theme
    /// changed this too.
    Inverted { paper: [u8; 3] },
}

/// How large a page is drawn, as a multiple of the width of the pane reading
/// it.
///
/// The scale is relative to the pane rather than to the page's printed size,
/// because a page on screen has no printed size: the reader sees a sheet in a
/// column, and what they adjust is how much of that column one page takes.
/// `PdfZoom::default()` is exactly the pane's width, which is what the viewer
/// did before it could be changed at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PdfZoom(u8);

impl PdfZoom {
    /// The steps a reader moves through, smallest first.
    ///
    /// Discrete steps rather than a continuous scale, because each change
    /// costs a rasterization in another process, and a reader dragging a
    /// slider would ask for renders they never look at.
    const STEPS: [f32; 7] = [0.5, 0.75, 1.0, 1.25, 1.5, 2.0, 3.0];
    const FIT: u8 = 2;

    /// How much of the pane's width one page takes.
    pub fn factor(self) -> f32 {
        Self::STEPS[usize::from(self.0)]
    }

    /// The step as a reader reads it, where 100% is the pane's width.
    pub fn percent(self) -> u32 {
        (self.factor() * 100.0).round() as u32
    }

    /// The next step in, or `None` at the largest, so a control can be
    /// withheld rather than press to no effect.
    pub fn larger(self) -> Option<Self> {
        (usize::from(self.0) + 1 < Self::STEPS.len()).then_some(Self(self.0 + 1))
    }

    pub fn smaller(self) -> Option<Self> {
        self.0.checked_sub(1).map(Self)
    }

    pub fn is_fit(self) -> bool {
        self.0 == Self::FIT
    }
}

impl Default for PdfZoom {
    fn default() -> Self {
        Self(Self::FIT)
    }
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

/// Replaces a regular file's contents atomically, preserving its permissions,
/// its POSIX access ACL and its content extended attributes.
///
/// The destination is compared against the baseline the buffer was loaded from
/// unless `overwrite` is set, and a mismatch is reported rather than
/// overwritten.
pub fn save_file(
    worktree: &Path,
    relative_path: &Path,
    text: &str,
    expected_disk_hash: blake3::Hash,
    overwrite: bool,
) -> Result<SaveOutcome, FileIoError> {
    save_file_with_hooks(
        worktree,
        relative_path,
        text,
        expected_disk_hash,
        overwrite,
        || Ok(()),
        fs_safety::sync_directory,
    )
}

fn save_file_with_hooks(
    worktree: &Path,
    relative_path: &Path,
    text: &str,
    expected_disk_hash: blake3::Hash,
    overwrite: bool,
    before_publish: impl FnOnce() -> io::Result<()>,
    sync_parent: impl FnOnce(&Root) -> io::Result<()>,
) -> Result<SaveOutcome, FileIoError> {
    validate_repo_path(relative_path)?;
    let root = worktree_root(worktree)?;
    let (parent, name) =
        fs_safety::open_parent_beneath(&root, relative_path).map_err(resolve_error)?;
    let name = Path::new(&name);
    // The destination is classified by name once, only to refuse a symlink
    // without opening it, and then opened; every decision that follows — its
    // mode, its ACL, its attributes and the bytes the conflict check hashes —
    // is taken from that one descriptor, so nothing can be swapped in behind
    // the name between the check and the save.
    let listed = fs_safety::metadata_beneath(&parent, name).map_err(resolve_error)?;
    if listed.file_type().is_symlink() {
        return Err(FileIoError::Io(
            "the destination is no longer a regular file".into(),
        ));
    }
    let mut destination = fs_safety::open_beneath(&parent, name).map_err(resolve_error)?;
    let metadata = fs_safety::metadata_of(&destination).map_err(io_error)?;
    if !metadata.is_file() {
        return Err(FileIoError::Io(
            "the destination is no longer a regular file".into(),
        ));
    }

    // A forced save compares nothing, so it reads nothing. An ordinary save
    // bounds the read the way `load_file` does rather than reading whatever is
    // there now: this worktree is shared with coding agents, so the
    // destination can have been replaced by a file far larger than the editor
    // would ever open. Reading one byte past the limit is enough to answer the
    // only question being asked, because a buffer can never have been loaded
    // from a file that long and the hashes therefore cannot match.
    if !overwrite {
        let disk =
            fs_safety::read_bounded(&mut destination, MAX_EDITABLE_FILE_BYTES).map_err(io_error)?;
        let disk_hash = blake3::hash(&disk);
        if disk_hash != expected_disk_hash {
            return Ok(SaveOutcome::Conflict { disk_hash });
        }
    }

    let durability = publish_replacement(
        &parent,
        name,
        &destination,
        text.as_bytes(),
        metadata.permissions(),
        before_publish,
        sync_parent,
    )?;
    Ok(SaveOutcome::Saved {
        content_hash: blake3::hash(text.as_bytes()),
        durability,
    })
}

/// Writes `bytes` into a temporary sibling that carries the destination's
/// permissions, access ACL and content attributes, then renames it over the
/// destination.
///
/// The replacement is a new inode, so every piece of metadata the old one
/// carried has to be reproduced deliberately — `st_mode` alone leaves an
/// ACL-bearing file readable by the wrong people and silently drops the user's
/// extended attributes.
fn publish_replacement(
    parent: &Root,
    name: &Path,
    destination: &File,
    bytes: &[u8],
    permissions: Permissions,
    before_publish: impl FnOnce() -> io::Result<()>,
    sync_parent: impl FnOnce(&Root) -> io::Result<()>,
) -> Result<SaveDurability, FileIoError> {
    let (mut temporary, temporary_name) =
        fs_safety::create_temporary_in(parent, 0o600).map_err(resolve_error)?;
    let result = (|| {
        // Establish access rules before any sensitive bytes exist: otherwise
        // the final chmod could briefly expose a mode-only view of an ACL.
        fs_safety::copy_access_acl(destination, &temporary).map_err(io_error)?;
        temporary.write_all(bytes).map_err(io_error)?;
        // Set the mode after writing: writing can clear set-ID permission bits.
        temporary.set_permissions(permissions).map_err(io_error)?;
        // ...and reinstate the ACL after it, because the kernel rewrites the
        // ACL's mask from the mode.
        fs_safety::copy_access_acl(destination, &temporary).map_err(io_error)?;
        let lost = fs_safety::copy_extended_attributes(destination, &temporary);
        temporary.sync_all().map_err(io_error)?;
        before_publish().map_err(io_error)?;
        fs_safety::rename_within(parent, &temporary_name, name.as_os_str())
            .map_err(resolve_error)?;
        Ok(lost)
    })();
    let lost = match result {
        Ok(lost) => lost,
        Err(error) => {
            if let Err(cleanup_error) = fs_safety::unlink_within(parent, &temporary_name) {
                return Err(FileIoError::Io(format!(
                    "{error}; could not remove partial save: {cleanup_error}"
                )));
            }
            return Err(error);
        }
    };
    // The bytes are published. Losing an optional attribute is not worth
    // failing a save that already succeeded, but it must not be silent either.
    for attribute in lost {
        tracing::warn!(
            path = %parent.path().join(name).display(),
            %attribute,
            "saved file did not receive one of its extended attributes"
        );
    }
    // The rename has replaced the destination. From this point onward, an I/O
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

/// Moves a regular file to a new repository-relative name without replacing
/// any existing entry. Symlinks in either path are refused.
///
/// An error means publication did not succeed. Once the move succeeds, a
/// directory-sync failure is logged rather than reported as a failed move.
pub fn rename_file(worktree: &Path, source: &Path, destination: &Path) -> Result<(), FileIoError> {
    let root = file_operation_root(worktree, source, destination)?;
    let (source_parent, source_name) =
        fs_safety::open_parent_beneath(&root, source).map_err(resolve_error)?;
    let (destination_parent, destination_name) =
        fs_safety::open_parent_beneath(&root, destination).map_err(resolve_error)?;
    let source_file = regular_source(&source_parent, Path::new(&source_name))?;
    require_new_destination(&destination_parent, Path::new(&destination_name))?;
    source_file.sync_all().map_err(io_error)?;
    fs_safety::rename_new_between(
        &source_parent,
        &source_name,
        &destination_parent,
        &destination_name,
    )
    .map_err(resolve_error)?;
    sync_published_directory(&destination_parent);
    if source.parent() != destination.parent() {
        sync_published_directory(&source_parent);
    }
    Ok(())
}

/// Streams a regular file into a temporary sibling, preserves its permissions
/// and access ACL, syncs the bytes, then atomically publishes the copy without
/// replacing any entry. The source is never modified. Symlinks are refused.
///
/// As with `rename_file`, errors precede publication; directory-sync failures
/// after publication are logged because the new file already exists.
pub fn copy_file(worktree: &Path, source: &Path, destination: &Path) -> Result<(), FileIoError> {
    copy_file_before_publish(worktree, source, destination, || Ok(()))
}

fn copy_file_before_publish(
    worktree: &Path,
    source: &Path,
    destination: &Path,
    before_publish: impl FnOnce() -> io::Result<()>,
) -> Result<(), FileIoError> {
    let root = file_operation_root(worktree, source, destination)?;
    let mut source_file = regular_source(&root, source)?;
    let permissions = source_file.metadata().map_err(io_error)?.permissions();
    let (parent, name) =
        fs_safety::open_parent_beneath(&root, destination).map_err(resolve_error)?;
    require_new_destination(&parent, Path::new(&name))?;
    let (mut temporary, temporary_name) =
        fs_safety::create_temporary_in(&parent, 0o600).map_err(resolve_error)?;
    let result = (|| {
        // Establish access rules before any sensitive bytes exist: otherwise
        // the final chmod could briefly expose a mode-only view of an ACL.
        fs_safety::copy_access_acl(&source_file, &temporary).map_err(io_error)?;
        io::copy(&mut source_file, &mut temporary).map_err(io_error)?;
        // Set the mode after writing: writing can clear set-ID permission bits.
        temporary.set_permissions(permissions).map_err(io_error)?;
        fs_safety::copy_access_acl(&source_file, &temporary).map_err(io_error)?;
        temporary.sync_all().map_err(io_error)?;
        before_publish().map_err(io_error)?;
        fs_safety::rename_new_within(&parent, &temporary_name, &name).map_err(resolve_error)
    })();
    if let Err(error) = result {
        if let Err(cleanup_error) = fs_safety::unlink_within(&parent, &temporary_name) {
            return Err(FileIoError::Io(format!(
                "{error}; could not remove partial copy: {cleanup_error}"
            )));
        }
        return Err(error);
    }
    sync_published_directory(&parent);
    Ok(())
}

fn file_operation_root(
    worktree: &Path,
    source: &Path,
    destination: &Path,
) -> Result<Root, FileIoError> {
    validate_repo_path(source)?;
    validate_repo_path(destination)?;
    if source == destination {
        return Err(FileIoError::Io(
            "source and destination must be different paths".into(),
        ));
    }
    worktree_root(worktree)
}

/// Opens the worktree as the root every repository-relative path is resolved
/// beneath.
///
/// The worktree path is the application's own — the one piece of input here
/// that repository contents cannot influence — so resolving it by name once is
/// exactly what makes everything below it fd-relative. It is canonicalized
/// first because a worktree may legitimately live behind a symbolic link,
/// which `Root::open` refuses for its final component.
fn worktree_root(worktree: &Path) -> Result<Root, FileIoError> {
    let root = fs::canonicalize(worktree)
        .map_err(|error| FileIoError::InvalidRepositoryRoot(error.to_string()))?;
    Root::open(&root).map_err(|error| FileIoError::InvalidRepositoryRoot(error.to_string()))
}

fn regular_source(root: &Root, source: &Path) -> Result<File, FileIoError> {
    let file = fs_safety::open_beneath(root, source).map_err(resolve_error)?;
    if !file.metadata().map_err(io_error)?.is_file() {
        return Err(FileIoError::Io("the source is not a regular file".into()));
    }
    Ok(file)
}

fn require_new_destination(parent: &Root, name: &Path) -> Result<(), FileIoError> {
    match fs_safety::metadata_beneath(parent, name) {
        Ok(_) => Err(FileIoError::Io("the destination already exists".into())),
        Err(error) if error.is_not_found() => Ok(()),
        Err(error) => Err(resolve_error(error)),
    }
}

fn sync_published_directory(parent: &Root) {
    if let Err(error) = fs_safety::sync_directory(parent) {
        tracing::warn!(
            path = %parent.path().display(),
            %error,
            "file operation succeeded but directory durability is unconfirmed"
        );
    }
}

fn resolve_error(error: ResolveError) -> FileIoError {
    match error {
        ResolveError::Escaped { .. } => FileIoError::OutsideWorktree,
        error => FileIoError::Io(error.to_string()),
    }
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
        } = load_file(root.path(), Path::new("opaque.dat"), PdfTone::Original).unwrap()
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
            load_file(root.path(), Path::new("wide.png"), PdfTone::Original).unwrap(),
            FileContent::ReadOnly(ReadOnlyReason::Image(_))
        ));
        let file = File::create(root.path().join("huge.png")).unwrap();
        file.set_len(MAX_EDITABLE_FILE_BYTES + 1).unwrap();
        assert!(matches!(
            load_file(root.path(), Path::new("huge.png"), PdfTone::Original).unwrap(),
            FileContent::ReadOnly(ReadOnlyReason::Oversize { .. })
        ));
        fs::write(root.path().join("broken.png"), b"not a valid image").unwrap();
        assert!(matches!(
            load_file(root.path(), Path::new("broken.png"), PdfTone::Original).unwrap(),
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

    /// A PDF header and nothing else.
    ///
    /// What `load_file` does with a document is pick its size limit and hand
    /// its name to a helper process, so the bytes after the header never
    /// matter on this side. What a rendered page looks like is tested in
    /// `document_render` against the code the helper runs, because the lib
    /// test harness is not one of the binaries that answers the render flag:
    /// here, every document render fails, which is the other half of the
    /// contract and worth pinning too.
    fn document_header(padding: usize) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(padding + 11);
        // A comment is ignored by a PDF parser, so the padding changes the
        // file's size without making it a different document.
        bytes.extend_from_slice(b"%PDF-1.7\n%");
        bytes.extend(std::iter::repeat_n(b'p', padding));
        bytes.push(b'\n');
        bytes
    }

    #[test]
    fn a_pdf_header_decides_the_document_path_and_a_failed_render_is_never_text() {
        let root = tempdir().unwrap();
        // Deliberately not named `.pdf`: the header decides.
        fs::write(root.path().join("report"), document_header(0)).unwrap();
        assert!(
            matches!(
                load_file(root.path(), Path::new("report"), PdfTone::Original).unwrap(),
                FileContent::ReadOnly(ReadOnlyReason::Document(_))
            ),
            "a document whose page cannot be produced is read-only for a document reason"
        );
        // The header decides in both directions: a text file that happens to
        // be named like a document stays editable.
        fs::write(root.path().join("notes.pdf"), "not a document at all\n").unwrap();
        assert!(matches!(
            load_file(root.path(), Path::new("notes.pdf"), PdfTone::Original).unwrap(),
            FileContent::Editable { .. }
        ));
    }

    /// A real document is far larger than any text file worth editing, so the
    /// editable-text limit must not be what decides whether it opens, and a
    /// refusal has to carry which limit it applied: the two differ by a
    /// factor of twenty-four, and the reader is shown the sentence built from
    /// this.
    #[test]
    fn the_size_limit_and_the_refusal_both_follow_the_kind_of_file() {
        let root = tempdir().unwrap();
        fs::write(
            root.path().join("big.pdf"),
            document_header(MAX_EDITABLE_FILE_BYTES as usize + 1),
        )
        .unwrap();
        assert!(
            matches!(
                load_file(root.path(), Path::new("big.pdf"), PdfTone::Original).unwrap(),
                FileContent::ReadOnly(ReadOnlyReason::Document(_))
            ),
            "a document must be held to the document limit, not the text limit"
        );
        let huge = File::create(root.path().join("huge.pdf")).unwrap();
        (&huge).write_all(b"%PDF-1.7\n").unwrap();
        huge.set_len(MAX_DOCUMENT_BYTES + 1).unwrap();
        assert!(matches!(
            load_file(root.path(), Path::new("huge.pdf"), PdfTone::Original).unwrap(),
            FileContent::ReadOnly(ReadOnlyReason::Oversize {
                limit: MAX_DOCUMENT_BYTES,
                kind: OversizeKind::Document,
                ..
            })
        ));
        // A text file that size is still refused, because the larger bound
        // belongs to documents alone.
        fs::write(
            root.path().join("big.txt"),
            "x".repeat(MAX_EDITABLE_FILE_BYTES as usize + 1),
        )
        .unwrap();
        assert!(matches!(
            load_file(root.path(), Path::new("big.txt"), PdfTone::Original).unwrap(),
            FileContent::ReadOnly(ReadOnlyReason::Oversize {
                limit: MAX_EDITABLE_FILE_BYTES,
                kind: OversizeKind::Editable,
                ..
            })
        ));
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
            load_file(root.path(), Path::new("binary.bin"), PdfTone::Original).unwrap(),
            FileContent::ReadOnly(ReadOnlyReason::Binary)
        );
        assert_eq!(
            load_file(root.path(), Path::new("invalid.txt"), PdfTone::Original).unwrap(),
            FileContent::ReadOnly(ReadOnlyReason::InvalidUtf8)
        );
        assert_eq!(
            load_file(root.path(), Path::new("link.txt"), PdfTone::Original).unwrap(),
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
    fn failure_before_publication_leaves_destination_unchanged_and_removes_temporary() {
        let root = tempdir().unwrap();
        let path = root.path().join("notes.txt");
        fs::write(&path, "before\n").unwrap();
        let expected = blake3::hash(b"before\n");

        let result = save_file_with_hooks(
            root.path(),
            Path::new("notes.txt"),
            "after\n",
            expected,
            false,
            || Err(io::Error::other("injected failure before publication")),
            |_| panic!("parent sync must not run when publication was not attempted"),
        );

        assert!(matches!(
            result,
            Err(FileIoError::Io(error)) if error.contains("injected failure before publication")
        ));
        assert_eq!(fs::read_to_string(path).unwrap(), "before\n");
        assert_eq!(
            fs::read_dir(root.path()).unwrap().count(),
            1,
            "a failed save must not leave its temporary file behind"
        );
    }

    #[test]
    fn parent_sync_failure_reports_replaced_but_unconfirmed_destination() {
        let root = tempdir().unwrap();
        let path = root.path().join("notes.txt");
        fs::write(&path, "before\n").unwrap();
        let expected = blake3::hash(b"before\n");

        let result = save_file_with_hooks(
            root.path(),
            Path::new("notes.txt"),
            "after\n",
            expected,
            false,
            || Ok(()),
            |_| Err(io::Error::other("injected parent sync failure")),
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
    fn save_preserves_set_id_bits_extended_attributes_and_reports_oversize_conflict() {
        let root = tempdir().unwrap();
        let path = root.path().join("notes.txt");
        fs::write(&path, "before\n").unwrap();
        // Group-executable plus set-group-ID: installing the mode before the
        // content would have the write clear `S_ISGID` again. The mode is read
        // back rather than assumed, because a kernel is allowed to refuse
        // `S_ISGID` to a process that is not in the file's group.
        fs::set_permissions(&path, Permissions::from_mode(0o2750)).unwrap();
        let installed = fs::metadata(&path).unwrap().permissions().mode() & 0o7777;
        let attribute = c"user.cagents.test";
        let file = File::open(&path).unwrap();
        // SAFETY: the descriptor is live and both the name and the value
        // outlive the call.
        let supported = unsafe {
            libc::fsetxattr(
                std::os::fd::AsRawFd::as_raw_fd(&file),
                attribute.as_ptr(),
                c"kept".as_ptr().cast(),
                4,
                0,
            )
        } == 0;
        drop(file);

        let outcome = save_file(
            root.path(),
            Path::new("notes.txt"),
            "after\n",
            blake3::hash(b"before\n"),
            false,
        )
        .unwrap();
        assert!(matches!(outcome, SaveOutcome::Saved { .. }));
        assert_eq!(fs::read_to_string(&path).unwrap(), "after\n");
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o7777,
            installed,
            "a save must not drop the destination's set-ID bits"
        );
        if supported {
            let file = File::open(&path).unwrap();
            let mut value = [0_u8; 8];
            // SAFETY: as above; the buffer length is passed exactly.
            let read = unsafe {
                libc::fgetxattr(
                    std::os::fd::AsRawFd::as_raw_fd(&file),
                    attribute.as_ptr(),
                    value.as_mut_ptr().cast(),
                    value.len(),
                )
            };
            assert_eq!(read, 4, "a save must carry the user's attributes across");
            assert_eq!(&value[..4], b"kept");
        }

        // A destination replaced by a file past the editable limit can never be
        // the baseline a buffer was loaded from, so it conflicts rather than
        // being read whole.
        let huge = File::options().write(true).open(&path).unwrap();
        huge.set_len(MAX_EDITABLE_FILE_BYTES + 1).unwrap();
        drop(huge);
        assert!(matches!(
            save_file(
                root.path(),
                Path::new("notes.txt"),
                "mine\n",
                blake3::hash(b"after\n"),
                false,
            )
            .unwrap(),
            SaveOutcome::Conflict { .. }
        ));
    }

    #[test]
    fn file_operations_preserve_binary_contents_and_permissions_across_directories() {
        let root = tempdir().unwrap();
        fs::create_dir(root.path().join("copies")).unwrap();
        fs::create_dir(root.path().join("renamed")).unwrap();
        let contents = [0, 255, 1, 128].repeat(65_536);
        fs::write(root.path().join("source.bin"), &contents).unwrap();
        fs::set_permissions(
            root.path().join("source.bin"),
            Permissions::from_mode(0o751),
        )
        .unwrap();

        copy_file(
            root.path(),
            Path::new("source.bin"),
            Path::new("copies/file.bin"),
        )
        .unwrap();
        assert_eq!(fs::read(root.path().join("source.bin")).unwrap(), contents);
        assert_eq!(
            fs::read(root.path().join("copies/file.bin")).unwrap(),
            contents
        );
        rename_file(
            root.path(),
            Path::new("source.bin"),
            Path::new("renamed/file.bin"),
        )
        .unwrap();
        assert!(!root.path().join("source.bin").exists());
        for path in ["copies/file.bin", "renamed/file.bin"] {
            assert_eq!(fs::read(root.path().join(path)).unwrap(), contents);
            assert_eq!(
                fs::metadata(root.path().join(path))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o7777,
                0o751
            );
        }
        fs::write(root.path().join("copies/file.bin"), b"independent").unwrap();
        assert_eq!(
            fs::read(root.path().join("renamed/file.bin")).unwrap(),
            contents
        );
    }

    type FileOperation = fn(&Path, &Path, &Path) -> Result<(), FileIoError>;
    const FILE_OPERATIONS: [FileOperation; 2] = [rename_file, copy_file];

    #[test]
    fn file_operations_never_replace_existing_entries() {
        for operation in FILE_OPERATIONS {
            let root = tempdir().unwrap();
            fs::write(root.path().join("source"), b"source").unwrap();
            fs::write(root.path().join("existing"), b"original destination").unwrap();
            symlink("absent", root.path().join("dangling")).unwrap();
            symlink("existing", root.path().join("link")).unwrap();
            fs::create_dir(root.path().join("directory")).unwrap();
            for destination in ["existing", "dangling", "link", "directory", "source"] {
                assert!(
                    operation(root.path(), Path::new("source"), Path::new(destination)).is_err()
                );
                assert_eq!(fs::read(root.path().join("source")).unwrap(), b"source");
                assert_eq!(
                    fs::read(root.path().join("existing")).unwrap(),
                    b"original destination"
                );
            }
            assert_eq!(
                fs::read_link(root.path().join("dangling")).unwrap(),
                Path::new("absent")
            );
            assert_eq!(
                fs::read_link(root.path().join("link")).unwrap(),
                Path::new("existing")
            );
            assert!(root.path().join("directory").is_dir());
            assert_eq!(fs::read_dir(root.path()).unwrap().count(), 5);
        }
    }

    #[test]
    fn file_operations_reject_traversal_and_symlink_boundaries() {
        for operation in FILE_OPERATIONS {
            let root = tempdir().unwrap();
            let outside = tempdir().unwrap();
            fs::write(root.path().join("source"), b"inside").unwrap();
            fs::write(outside.path().join("source"), b"outside").unwrap();
            fs::create_dir(root.path().join("real")).unwrap();
            symlink(outside.path(), root.path().join("escape")).unwrap();
            symlink("real", root.path().join("internal")).unwrap();
            symlink("source", root.path().join("source-link")).unwrap();
            for path in ["../source", "/source"] {
                assert_eq!(
                    operation(root.path(), Path::new(path), Path::new("destination")),
                    Err(FileIoError::InvalidRepositoryPath)
                );
                assert_eq!(
                    operation(root.path(), Path::new("source"), Path::new(path)),
                    Err(FileIoError::InvalidRepositoryPath)
                );
            }
            for source in ["source-link", "escape/source"] {
                assert_eq!(
                    operation(root.path(), Path::new(source), Path::new("destination")),
                    Err(FileIoError::OutsideWorktree)
                );
            }
            for destination in ["escape/destination", "internal/destination"] {
                assert_eq!(
                    operation(root.path(), Path::new("source"), Path::new(destination)),
                    Err(FileIoError::OutsideWorktree)
                );
            }
            assert_eq!(fs::read(root.path().join("source")).unwrap(), b"inside");
            assert_eq!(fs::read(outside.path().join("source")).unwrap(), b"outside");
            assert!(!outside.path().join("destination").exists());
            assert!(!root.path().join("destination").exists());
            assert!(!root.path().join("real/destination").exists());
        }
    }

    #[test]
    fn file_operations_fail_without_creating_parents_or_moving_non_files() {
        for operation in FILE_OPERATIONS {
            let root = tempdir().unwrap();
            fs::write(root.path().join("source"), b"untouched").unwrap();
            fs::create_dir(root.path().join("directory")).unwrap();
            for (source, destination) in [
                ("missing", "destination"),
                ("directory", "destination"),
                ("source", "missing/destination"),
            ] {
                assert!(operation(root.path(), Path::new(source), Path::new(destination)).is_err());
            }
            assert_eq!(fs::read(root.path().join("source")).unwrap(), b"untouched");
            assert!(root.path().join("directory").is_dir());
            assert!(!root.path().join("missing").exists());
            assert!(!root.path().join("destination").exists());
            assert_eq!(fs::read_dir(root.path()).unwrap().count(), 2);
        }
    }

    #[test]
    fn copy_collision_at_publication_preserves_both_files_and_removes_temporary() {
        let root = tempdir().unwrap();
        fs::write(root.path().join("source"), b"source").unwrap();
        let result = copy_file_before_publish(
            root.path(),
            Path::new("source"),
            Path::new("destination"),
            || fs::write(root.path().join("destination"), b"concurrent creation"),
        );
        assert!(result.is_err());
        assert_eq!(fs::read(root.path().join("source")).unwrap(), b"source");
        assert_eq!(
            fs::read(root.path().join("destination")).unwrap(),
            b"concurrent creation"
        );
        assert_eq!(fs::read_dir(root.path()).unwrap().count(), 2);
    }

    #[test]
    fn copy_failure_after_streaming_removes_partial_output() {
        let root = tempdir().unwrap();
        fs::write(root.path().join("source"), b"source").unwrap();
        let result = copy_file_before_publish(
            root.path(),
            Path::new("source"),
            Path::new("destination"),
            || Err(io::Error::other("injected publication failure")),
        );
        assert!(matches!(result, Err(FileIoError::Io(error)) if error.contains("injected")));
        assert_eq!(fs::read(root.path().join("source")).unwrap(), b"source");
        assert!(!root.path().join("destination").exists());
        assert_eq!(fs::read_dir(root.path()).unwrap().count(), 1);
    }

    #[test]
    fn rename_publication_is_no_replace_between_pinned_parents() {
        let root = tempdir().unwrap();
        let outside = tempdir().unwrap();
        fs::create_dir(root.path().join("destination")).unwrap();
        fs::write(root.path().join("source"), b"source").unwrap();
        fs::write(root.path().join("destination/file"), b"existing").unwrap();
        let source_parent = Root::open(root.path()).unwrap();
        let destination_parent = Root::open(&root.path().join("destination")).unwrap();
        fs::rename(root.path().join("destination"), root.path().join("pinned")).unwrap();
        symlink(outside.path(), root.path().join("destination")).unwrap();

        assert!(
            fs_safety::rename_new_between(
                &source_parent,
                std::ffi::OsStr::new("source"),
                &destination_parent,
                std::ffi::OsStr::new("file"),
            )
            .is_err()
        );
        assert_eq!(fs::read(root.path().join("source")).unwrap(), b"source");
        assert_eq!(
            fs::read(root.path().join("pinned/file")).unwrap(),
            b"existing"
        );
        assert!(!outside.path().join("file").exists());
    }

    #[test]
    fn copy_publication_cannot_follow_a_replaced_parent_symlink() {
        let root = tempdir().unwrap();
        let outside = tempdir().unwrap();
        fs::write(root.path().join("source"), b"source").unwrap();
        fs::create_dir(root.path().join("destination")).unwrap();
        copy_file_before_publish(
            root.path(),
            Path::new("source"),
            Path::new("destination/file"),
            || {
                fs::rename(root.path().join("destination"), root.path().join("pinned"))?;
                symlink(outside.path(), root.path().join("destination"))
            },
        )
        .unwrap();
        assert_eq!(fs::read(root.path().join("source")).unwrap(), b"source");
        assert_eq!(
            fs::read(root.path().join("pinned/file")).unwrap(),
            b"source"
        );
        assert!(!outside.path().join("file").exists());
        assert_eq!(fs::read_dir(root.path().join("pinned")).unwrap().count(), 1);
    }

    #[test]
    fn rejects_traversal() {
        let root = tempdir().unwrap();
        assert_eq!(
            load_file(root.path(), Path::new("../outside"), PdfTone::Original),
            Err(FileIoError::InvalidRepositoryPath)
        );
    }
}
