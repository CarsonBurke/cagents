//! Rasterizes paged documents in a short-lived child process.
//!
//! A PDF is the one file kind the editor opens whose parser is handed a
//! deeply structured, attacker-shaped input. `hayro-syntax` resolves a page
//! tree by recursing through `/Kids` with no depth counter and no visited
//! set, so a document that nests sixty thousand `/Pages` nodes overflows the
//! parsing thread's stack, and a Rust stack overflow calls `abort`. A panic
//! would have been turned into an error by `spawn_blocking`; an abort takes
//! the whole process, which means every unsaved buffer in every tab of the
//! window. The same tree can instead name one child twice per level, which
//! asks the parser to materialise 2^30 pages and exhausts memory rather than
//! the stack.
//!
//! Neither shape can be screened out ahead of the parser without writing a
//! second PDF parser that agrees with hayro's own recovery for damaged
//! cross-reference tables, so the render runs where a crash costs one page
//! instead of the application. The process boundary also buys a wall-clock
//! bound, an address-space bound and a CPU bound, none of which an in-process
//! design can offer, because the failures here are not panics.

use std::{
    io::{Read, Write},
    os::unix::process::{CommandExt, ExitStatusExt},
    path::{Path, PathBuf},
    process::{Child, Command, ExitCode, ExitStatus, Stdio},
    sync::mpsc,
    time::{Duration, Instant},
};

use crate::editor::{
    MAX_DOCUMENT_BYTES, MAX_IMAGE_DIMENSION, MAX_PDF_PAGES, PDF_PAGE_SCALE, PdfPage, PdfTone,
    PdfZoom, check_image_dimensions,
};

/// The argument that turns this executable into the render helper.
///
/// Named like the other modes cagents spawns itself with, and absent from
/// `USAGE` for the same reason they are: it is a calling convention between
/// two copies of one binary, not a feature anyone should hand-craft.
const RENDER_PAGE_FLAG: &str = "--internal-render-page";

/// The helper's exit code for a document it refused deliberately.
///
/// Worth distinguishing from dying because of what the reader is shown: a
/// refusal is already a sentence about their document, while a crash has only
/// a signal number and whatever the runtime managed to print on its way out.
const EXIT_DOCUMENT_REFUSED: u8 = 3;

/// How long the parent waits for a page before giving up on the helper.
///
/// Generous next to the tens of milliseconds an ordinary page takes, because
/// a figure-dense or scanned page on a loaded machine is legitimately slow
/// and the reader would rather wait than be told no. This bounds the
/// pathological case; it is not a performance target. It runs on a blocking
/// worker, so the window keeps drawing throughout.
const RENDER_TIMEOUT: Duration = Duration::from_secs(15);

/// The helper's own CPU bound, deliberately under the wall-clock one.
///
/// The parent's timeout covers a helper that stops making progress. This
/// covers what the parent cannot see: if the parent's own thread is
/// descheduled or stopped, a runaway helper would otherwise keep a core busy
/// with nobody watching. The heaviest real page measured here, the colour
/// cover of a 24 megabyte, 786 page textbook, costs a third of a second, so
/// twelve seconds is more than thirty times the legitimate worst case and
/// still ends a spinning parser promptly.
const MAX_HELPER_CPU_SECONDS: u64 = 12;

/// The helper's address-space bound.
///
/// A legitimate render needs the document in memory, at most
/// `MAX_DOCUMENT_BYTES`, plus one page of RGBA, at most `MAX_IMAGE_DIMENSION`
/// squared times four, plus hayro's caches for the page being drawn, on top
/// of the mapped executable. Measured: the 24 megabyte, 786 page textbook
/// renders its cover inside 160 megabytes of address space and fails under
/// 128, so the largest file the editor will open at all should sit around
/// twice that. Two gigabytes is several times over the worst legitimate
/// case, which is what makes this a guard against a page tree that
/// multiplies rather than a limit a real document can trip: the fanout tree
/// needs orders of magnitude more and dies here in about a second, ending
/// the helper instead of the host's memory.
const MAX_HELPER_ADDRESS_SPACE_BYTES: u64 = 2 * 1024 * 1024 * 1024;

/// Identifies the frame and the layout of everything after it.
const FRAME_MAGIC: [u8; 8] = *b"CGDOCPAG";

/// Bumped if the header's shape ever changes. Parent and helper are the same
/// build in every supported case, so a mismatch means something other than
/// this protocol is writing to the pipe.
const FRAME_VERSION: u32 = 1;

/// Magic, version, page count, page index, width, height, and RGBA length.
const FRAME_HEADER_BYTES: usize = 8 + 4 + 8 + 8 + 4 + 4 + 8;

/// The largest raster the parent will accept, which is the ceiling the image
/// preview already enforces. Kept independent of the header's own length
/// field, because a rogue helper's word is exactly what must not decide how
/// much the parent allocates.
const MAX_RGBA_BYTES: u64 = MAX_IMAGE_DIMENSION as u64 * MAX_IMAGE_DIMENSION as u64 * 4;

/// Everything the parent will read from the helper's stdout.
const MAX_FRAME_BYTES: u64 = FRAME_HEADER_BYTES as u64 + MAX_RGBA_BYTES;

/// Enough of the helper's stderr for a sentence or a runtime's dying words,
/// and not enough for a wedged helper to fill the parent's memory.
const MAX_STDERR_BYTES: u64 = 4096;

/// The gap between the helper's last write and its exit.
///
/// Both pipes are at end of file by the time this applies, so the helper has
/// already said everything it is going to; only its exit is outstanding.
const REAP_GRACE: Duration = Duration::from_millis(500);

/// How often the exit status is polled while waiting out `REAP_GRACE`.
const REAP_POLL: Duration = Duration::from_millis(1);

/// One page raster plus how many pages the document has.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DocumentPage {
    pub pages: usize,
    pub page: PdfPage,
}

/// Performs the render and exits, when this process was spawned as the
/// helper.
///
/// `None` means an ordinary launch, so the caller falls through to its real
/// `main`. This must run before diagnostics, the window lease, the session
/// store and the launcher handshake: the helper's stdout carries a binary
/// frame, and anything else that writes a line there corrupts the page.
pub fn hook() -> Option<ExitCode> {
    let mut arguments = std::env::args_os().skip(1);
    if arguments.next()? != RENDER_PAGE_FLAG {
        return None;
    }
    let path = arguments.next().map(PathBuf::from);
    let index = arguments.next();
    let tone = arguments.next();
    let zoom = arguments.next();
    if arguments.next().is_some() {
        return Some(refuse("The document render request was malformed."));
    }
    let Some((((path, index), tone), zoom)) = path.zip(index).zip(tone).zip(zoom) else {
        return Some(refuse("The document render request was incomplete."));
    };
    let Some(((index, tone), zoom)) = index
        .to_str()
        .and_then(|index| index.parse::<usize>().ok())
        .zip(tone.to_str().and_then(parse_tone))
        .zip(zoom.to_str().and_then(parse_zoom))
    else {
        return Some(refuse("The document render request was not understood."));
    };
    // Fail closed. These limits are the reason the work is out here at all,
    // so a helper that cannot install them must not open the document.
    if let Err(error) = restrict_helper_resources() {
        return Some(refuse(&format!(
            "This document could not be rendered safely: {error}."
        )));
    }
    Some(
        match render_page(&path, index, tone, zoom).and_then(|page| write_frame(&page)) {
            Ok(()) => ExitCode::SUCCESS,
            Err(message) => refuse(&message),
        },
    )
}

/// Reports a refusal on stderr, where the parent reads it, and exits with
/// the code that marks the message as one already phrased for the reader.
fn refuse(message: &str) -> ExitCode {
    eprintln!("{message}");
    ExitCode::from(EXIT_DOCUMENT_REFUSED)
}

/// Renders one page of the document at `path` in a child process.
///
/// The document itself never crosses the boundary: the helper re-reads the
/// file, so turning a page costs a file read instead of keeping up to
/// `MAX_DOCUMENT_BYTES` resident in the process that holds the reader's
/// unsaved work. The name was already resolved beneath the worktree root by
/// the caller, and the helper only reads it.
pub fn render_document_page(
    path: &Path,
    index: usize,
    tone: PdfTone,
    zoom: PdfZoom,
) -> Result<DocumentPage, String> {
    let helper = std::env::current_exe()
        .map_err(|error| format!("This document could not be rendered: {error}."))?;
    render_with_helper(&helper, path, index, tone, zoom, RENDER_TIMEOUT)
}

fn render_with_helper(
    helper: &Path,
    path: &Path,
    index: usize,
    tone: PdfTone,
    zoom: PdfZoom,
    timeout: Duration,
) -> Result<DocumentPage, String> {
    let mut command = Command::new(helper);
    command
        .arg(RENDER_PAGE_FLAG)
        .arg(path)
        .arg(index.to_string())
        .arg(tone_argument(tone))
        .arg(format!("{:.4}", zoom.factor()))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // This process may itself have been started with the UI launch
        // handshake set. Inheriting it would let the helper put a readiness
        // line on the stdout the frame is read from.
        .env_remove(crate::instance::UI_LAUNCH_HANDSHAKE_ENV);
    // SAFETY: the hook performs only the close_range syscall before exec.
    unsafe {
        command.pre_exec(crate::daemon::seal_inherited_descriptors);
    }
    let mut child = command
        .spawn()
        .map_err(|error| format!("This document could not be rendered: {error}."))?;
    let Some(output) = collect_output(&mut child, timeout) else {
        // The helper is still holding a pipe open with nothing to say, or is
        // busy in a loop the reader will never see the end of.
        kill_and_reap(&mut child);
        return Err("This document could not be rendered in time.".to_owned());
    };
    // A whole frame is proof the render finished, so how the helper exited
    // afterwards cannot unmake the page. The status decides only what the
    // reader is told when no page arrived.
    let decoded = output
        .frame
        .map(|frame| decode_frame(&frame, index))
        .transpose();
    let status = reap(&mut child);
    match decoded {
        Ok(Some(page)) => Ok(page),
        Ok(None) => Err(helper_failure(status, &output.stderr, None)),
        Err(problem) => Err(helper_failure(status, &output.stderr, Some(problem))),
    }
}

fn tone_argument(tone: PdfTone) -> String {
    match tone {
        PdfTone::Original => "original".to_owned(),
        PdfTone::Inverted {
            paper: [red, green, blue],
        } => format!("inverted:{red},{green},{blue}"),
    }
}

fn parse_tone(argument: &str) -> Option<PdfTone> {
    if argument == "original" {
        return Some(PdfTone::Original);
    }
    let mut channels = argument.strip_prefix("inverted:")?.split(',');
    let mut paper = [0u8; 3];
    for channel in &mut paper {
        *channel = channels.next()?.parse().ok()?;
    }
    channels
        .next()
        .is_none()
        .then_some(PdfTone::Inverted { paper })
}

/// The zoom step the parent asked for, as a plain multiplier.
///
/// The helper is told a number rather than a step, because which steps a
/// reader can move through is the viewer's business and the helper only has
/// to rasterize at a scale. The bounds are wider than the viewer's own steps
/// and exist so a malformed argument cannot ask for a scale of zero, an
/// infinity, or a page nothing can hold.
fn parse_zoom(argument: &str) -> Option<f32> {
    let zoom = argument.parse::<f32>().ok()?;
    (zoom.is_finite() && (0.05..=8.0).contains(&zoom)).then_some(zoom)
}

struct HelperOutput {
    /// `None` when stdout could not be read to its end, which is a different
    /// failure from a helper that wrote nothing.
    frame: Option<Vec<u8>>,
    stderr: Vec<u8>,
}

enum Pipe {
    Stdout(Option<Vec<u8>>),
    Stderr(Vec<u8>),
}

/// Reads both pipes to end of file, or gives up at `timeout`.
///
/// Reading happens on two threads because the helper can fill either pipe and
/// block on the other, and because a blocking read is the only kind std
/// offers. `None` is the timeout: the caller kills the helper, which closes
/// the pipes and lets both threads finish.
fn collect_output(child: &mut Child, timeout: Duration) -> Option<HelperOutput> {
    let deadline = Instant::now() + timeout;
    let (sender, receiver) = mpsc::channel();
    let stderr_sender = sender.clone();
    if let Some(mut stdout) = child.stdout.take() {
        std::thread::spawn(move || {
            let mut frame = Vec::new();
            let read = (&mut stdout)
                .take(MAX_FRAME_BYTES + 1)
                .read_to_end(&mut frame);
            let _ = sender.send(Pipe::Stdout(read.ok().map(|_| frame)));
        });
    } else {
        let _ = sender.send(Pipe::Stdout(None));
    }
    if let Some(mut stderr) = child.stderr.take() {
        std::thread::spawn(move || {
            let mut text = Vec::new();
            let _ = (&mut stderr).take(MAX_STDERR_BYTES).read_to_end(&mut text);
            let _ = stderr_sender.send(Pipe::Stderr(text));
        });
    } else {
        let _ = stderr_sender.send(Pipe::Stderr(Vec::new()));
    }
    let mut output = HelperOutput {
        frame: None,
        stderr: Vec::new(),
    };
    for _ in 0..2 {
        match receiver.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
            Ok(Pipe::Stdout(frame)) => output.frame = frame,
            Ok(Pipe::Stderr(text)) => output.stderr = text,
            // Disconnection means a reader thread died without answering,
            // which is as unusable as a timeout but needs no kill.
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
            Err(mpsc::RecvTimeoutError::Timeout) => return None,
        }
    }
    // A helper that wrote more than the ceiling is not trusted for the part
    // that did fit: the length it declared cannot describe what it sent.
    if output
        .frame
        .as_ref()
        .is_some_and(|frame| frame.len() as u64 > MAX_FRAME_BYTES)
    {
        output.frame = None;
    }
    Some(output)
}

/// Collects the helper's exit status once both its pipes are closed.
fn reap(child: &mut Child) -> Option<ExitStatus> {
    let deadline = Instant::now() + REAP_GRACE;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Some(status),
            Err(_) => return None,
            Ok(None) if Instant::now() >= deadline => {
                kill_and_reap(child);
                return None;
            }
            Ok(None) => std::thread::sleep(REAP_POLL),
        }
    }
}

/// Ends the helper and waits for it, so nothing is left to inherit the pipes
/// or to sit in the process table.
fn kill_and_reap(child: &mut Child) {
    let _ = child.kill();
    let _ = child.wait();
}

fn helper_failure(
    status: Option<ExitStatus>,
    stderr: &[u8],
    frame_problem: Option<String>,
) -> String {
    let refused =
        status.is_some_and(|status| status.code() == Some(i32::from(EXIT_DOCUMENT_REFUSED)));
    let reported = reported_line(stderr);
    // A deliberate refusal has already phrased itself for the reader, so it
    // is passed through instead of being wrapped in an exit status.
    if let (true, Some(reported)) = (refused, reported.as_deref()) {
        return reported.to_owned();
    }
    let outcome = match status {
        Some(status) => match (status.code(), status.signal()) {
            (Some(code), _) => format!("exit status {code}"),
            (None, Some(signal)) => format!("killed by signal {signal}"),
            (None, None) => "stopped".to_owned(),
        },
        None => "never exited".to_owned(),
    };
    match reported.or(frame_problem) {
        Some(detail) => format!("This document could not be rendered ({outcome}): {detail}"),
        None => format!("This document could not be rendered ({outcome})."),
    }
}

/// The first thing the helper said, which is where both its own refusals and
/// the runtime's dying words land.
///
/// The first line rather than the last, because a runtime that aborts says
/// what happened first and then advises a developer to set `RUST_BACKTRACE`;
/// the cause is what belongs in a sentence about the reader's document. It is
/// truncated because the helper's output is not a place to put a paragraph.
fn reported_line(stderr: &[u8]) -> Option<String> {
    const MAX_REPORTED_CHARS: usize = 240;
    let text = String::from_utf8_lossy(stderr);
    let line = text
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty() && !line.starts_with("note:"))?;
    let end = line
        .char_indices()
        .nth(MAX_REPORTED_CHARS)
        .map_or(line.len(), |(end, _)| end);
    Some(line[..end].to_owned())
}

fn restrict_helper_resources() -> Result<(), String> {
    lower_limit(libc::RLIMIT_AS, MAX_HELPER_ADDRESS_SPACE_BYTES)?;
    lower_limit(libc::RLIMIT_CPU, MAX_HELPER_CPU_SECONDS)?;
    // A helper that aborts on a document built to overflow the parser's stack
    // should not also hand the system a multi-gigabyte core file to write.
    lower_limit(libc::RLIMIT_CORE, 0)
}

fn lower_limit(resource: libc::__rlimit_resource_t, value: libc::rlim_t) -> Result<(), String> {
    let mut limit = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: getrlimit writes one rlimit through the pointer it is given.
    if unsafe { libc::getrlimit(resource, &mut limit) } != 0 {
        return Err(format!(
            "resource limit {resource} could not be read ({})",
            std::io::Error::last_os_error()
        ));
    }
    // Only ever tighten. A parent already running under a stricter limit
    // keeps it, and raising a hard limit needs a privilege this process must
    // not have.
    limit.rlim_cur = limit.rlim_cur.min(value);
    limit.rlim_max = limit.rlim_max.min(value);
    // SAFETY: setrlimit reads one rlimit through the pointer it is given.
    if unsafe { libc::setrlimit(resource, &limit) } != 0 {
        return Err(format!(
            "resource limit {resource} could not be set ({})",
            std::io::Error::last_os_error()
        ));
    }
    Ok(())
}

/// Rasterizes one page, in the helper process.
///
/// Everything here runs behind the limits `restrict_helper_resources`
/// installed, which is what makes reading a hostile page tree survivable.
fn render_page(
    path: &Path,
    index: usize,
    tone: PdfTone,
    zoom: f32,
) -> Result<DocumentPage, String> {
    let bytes = read_document(path)?;
    let pdf = hayro::hayro_interpret::hayro_syntax::Pdf::new(std::sync::Arc::new(bytes))
        .map_err(|error| format!("This PDF could not be read: {error:?}."))?;
    let pages = pdf.pages();
    let count = pages.len();
    if count == 0 {
        return Err("This PDF declares no pages.".into());
    }
    // Materialising the page tree is what bounds this, and it has just
    // happened, here, where a tree that refuses to be bounded costs one
    // helper process.
    if count > MAX_PDF_PAGES {
        return Err(format!(
            "This PDF declares {count} pages, past the {MAX_PDF_PAGES} page preview limit."
        ));
    }
    let page = pages
        .get(index)
        .ok_or_else(|| format!("This PDF has no page {}.", index + 1))?;
    // The reader's zoom raises the raster's resolution along with the size
    // the page is drawn at, so text stays as sharp magnified as it was at the
    // pane's width. What the buffer actually measures is asked of the pixmap
    // afterwards, because a declared size that disagrees with the buffer by
    // even one column makes the upload silently draw nothing.
    let (points_wide, points_high) = page.render_dimensions();
    let scale = render_scale(points_wide, points_high, PDF_PAGE_SCALE * zoom)
        .ok_or_else(|| format!("Page {} has no size to render.", index + 1))?;
    let pixmap = hayro::render(
        page,
        &hayro::RenderCache::new(),
        &hayro::hayro_interpret::InterpreterSettings::default(),
        &hayro::RenderSettings {
            x_scale: scale,
            y_scale: scale,
            width: None,
            height: None,
            // A page describes ink on paper and says nothing about what is
            // behind it, so the paper has to be supplied. White is what the
            // document was authored against; `PdfTone::Inverted` then turns
            // the rendered page over, paper included.
            bg_color: hayro::vello_cpu::color::palette::css::WHITE,
        },
    );
    // The background is opaque, so every pixel's alpha is 255 and the
    // premultiplied buffer is already straight alpha. That is why this is one
    // copy rather than the per-pixel unpremultiply `take_unpremultiplied`
    // would do.
    let mut rgba = pixmap.data_as_u8_slice().to_vec();
    let width = u32::from(pixmap.width());
    let height = u32::from(pixmap.height());
    if let PdfTone::Inverted { paper } = tone {
        invert_lightness(&mut rgba, paper);
    }
    Ok(DocumentPage {
        pages: count,
        page: PdfPage {
            index,
            width,
            height,
            rgba: rgba.into(),
        },
    })
}

/// The scale a page is actually rasterized at.
///
/// The reader asks for `wanted`, and both ends are bounded. A page large
/// enough that `wanted` would put it past `MAX_IMAGE_DIMENSION` is drawn as
/// sharply as the limit allows instead of being refused, because a reader
/// zooming into a poster-sized plan wants to see it rather than to be told its
/// dimensions. The pixel budget the parent also checks needs no term of its
/// own, because `MAX_IMAGE_PIXELS` is `MAX_IMAGE_DIMENSION` squared and the
/// per-axis clamp therefore implies it. A page under a point tall is scaled
/// up instead, because `hayro` floors its pixmap and a zero-height page is
/// refused by the parent with a message about being too large.
///
/// `None` is a page no scale can render: one with no size, or one whose two
/// axes are so far apart that a single pixel on the short one already puts
/// the long one past the limit.
fn render_scale(points_wide: f32, points_high: f32, wanted: f32) -> Option<f32> {
    if !points_wide.is_finite()
        || !points_high.is_finite()
        || points_wide <= 0.0
        || points_high <= 0.0
        || !wanted.is_finite()
        || wanted <= 0.0
    {
        return None;
    }
    let most = MAX_IMAGE_DIMENSION as f32 / points_wide.max(points_high);
    let least = 1.0 / points_wide.min(points_high);
    (least <= most).then(|| wanted.clamp(least, most))
}

/// Reads the document in the helper, bounded again.
///
/// The parent gated the size it saw through the descriptor it had open, but
/// the helper opens the file by name a moment later, so the bytes it actually
/// reads are the ones that have to be bounded.
fn read_document(path: &Path) -> Result<Vec<u8>, String> {
    let file = std::fs::File::open(path)
        .map_err(|error| format!("This document could not be opened: {error}."))?;
    let length = file
        .metadata()
        .map_err(|error| format!("This document could not be measured: {error}."))?
        .len();
    if length > MAX_DOCUMENT_BYTES {
        return Err(oversize_document(length));
    }
    let mut bytes = Vec::with_capacity(length as usize);
    file.take(MAX_DOCUMENT_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| format!("This document could not be read: {error}."))?;
    if bytes.len() as u64 > MAX_DOCUMENT_BYTES {
        return Err(oversize_document(bytes.len() as u64));
    }
    Ok(bytes)
}

fn oversize_document(bytes: u64) -> String {
    format!("This document is {bytes} bytes, past the {MAX_DOCUMENT_BYTES} byte preview limit.")
}

/// Adapts a page to a dark surface, leaving its hue and saturation alone.
///
/// Negating the channels instead would turn a dark red heading cyan and a
/// blue diagram orange, which is why a plain inversion looks wrong on a
/// document rather than merely dark. Shifting every channel by the same
/// amount, chosen so the pixel's lightness maps to its complement, inverts
/// the greys a page is mostly made of and leaves a fully saturated
/// mid-lightness color where it was.
///
/// The inverted value is then held above `paper`, so the document's white
/// lands on the shell's own surface color instead of absolute black, and its
/// black still reaches white. That is four adds, a multiply and a shift per
/// channel, which matters because it runs over every pixel of every page the
/// reader turns to.
fn invert_lightness(rgba: &mut [u8], paper: [u8; 3]) {
    for pixel in rgba.chunks_exact_mut(4) {
        let high = pixel[0].max(pixel[1]).max(pixel[2]) as i32;
        let low = pixel[0].min(pixel[1]).min(pixel[2]) as i32;
        // `255 - (high + low)` is `255 * (1 - 2L)` for lightness `L`, which is
        // the shift that sends `L` to `1 - L`.
        let shift = 255 - (high + low);
        for (channel, floor) in pixel[..3].iter_mut().zip(paper) {
            let inverted = (*channel as i32 + shift).clamp(0, 255) as u32;
            let floor = floor as u32;
            *channel = (floor + inverted * (255 - floor) / 255) as u8;
        }
    }
}

/// Writes the page to stdout as one frame: the header, then the pixels.
fn write_frame(page: &DocumentPage) -> Result<(), String> {
    let stdout = std::io::stdout();
    let mut stdout = stdout.lock();
    stdout
        .write_all(&frame_header(page))
        .and_then(|()| stdout.write_all(&page.page.rgba))
        .and_then(|()| stdout.flush())
        .map_err(|error| format!("This document's page could not be returned: {error}."))
}

fn frame_header(page: &DocumentPage) -> Vec<u8> {
    let mut header = Vec::with_capacity(FRAME_HEADER_BYTES);
    header.extend_from_slice(&FRAME_MAGIC);
    header.extend_from_slice(&FRAME_VERSION.to_le_bytes());
    header.extend_from_slice(&(page.pages as u64).to_le_bytes());
    header.extend_from_slice(&(page.page.index as u64).to_le_bytes());
    header.extend_from_slice(&page.page.width.to_le_bytes());
    header.extend_from_slice(&page.page.height.to_le_bytes());
    header.extend_from_slice(&(page.page.rgba.len() as u64).to_le_bytes());
    header
}

/// Turns a frame back into a page, or says why it is not one.
///
/// Every field is checked against what the parent asked for and against the
/// preview's own ceilings, because on this side of the pipe the helper is a
/// process that parsed a hostile document and may be saying anything at all.
fn decode_frame(frame: &[u8], requested: usize) -> Result<DocumentPage, String> {
    if frame.len() < FRAME_HEADER_BYTES {
        return Err(format!(
            "the page header was {} bytes instead of {FRAME_HEADER_BYTES}",
            frame.len()
        ));
    }
    let (header, rgba) = frame.split_at(FRAME_HEADER_BYTES);
    if header[..FRAME_MAGIC.len()] != FRAME_MAGIC {
        return Err("the reply was not a page".to_owned());
    }
    let version = u32_at(header, 8);
    if version != FRAME_VERSION {
        return Err(format!("the page used protocol {version}"));
    }
    let pages = u64_at(header, 12);
    let index = u64_at(header, 20);
    let width = u32_at(header, 28);
    let height = u32_at(header, 32);
    let declared = u64_at(header, 36);
    if declared > MAX_RGBA_BYTES {
        return Err(format!(
            "the page declared {declared} bytes, past the {MAX_RGBA_BYTES} byte limit"
        ));
    }
    if declared != rgba.len() as u64 {
        return Err(format!(
            "the page declared {declared} bytes and sent {}",
            rgba.len()
        ));
    }
    check_image_dimensions(width, height).map_err(|_| {
        format!("the page measured {width} by {height} pixels, past the preview limit")
    })?;
    if declared != u64::from(width) * u64::from(height) * 4 {
        return Err(format!(
            "a {width} by {height} page cannot be {declared} bytes"
        ));
    }
    if pages == 0 || pages > MAX_PDF_PAGES as u64 {
        return Err(format!("the document claimed {pages} pages"));
    }
    if index != requested as u64 {
        return Err(format!(
            "page {index} arrived for a request for {requested}"
        ));
    }
    Ok(DocumentPage {
        pages: pages as usize,
        page: PdfPage {
            index: requested,
            width,
            height,
            rgba: rgba.into(),
        },
    })
}

fn u32_at(header: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([header[at], header[at + 1], header[at + 2], header[at + 3]])
}

fn u64_at(header: &[u8], at: usize) -> u64 {
    u64::from_le_bytes([
        header[at],
        header[at + 1],
        header[at + 2],
        header[at + 3],
        header[at + 4],
        header[at + 5],
        header[at + 6],
        header[at + 7],
    ])
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use tempfile::tempdir;

    use super::*;

    /// A minimal one-page PDF whose page is a black rectangle on white paper.
    fn one_page_pdf() -> Vec<u8> {
        let objects: [&str; 5] = [
            "<< /Type /Catalog /Pages 2 0 R >>",
            "<< /Type /Pages /Kids [3 0 R] /Count 1 >>",
            // 21 by 11 points deliberately: at the render scale both axes land
            // between whole pixels, which is the case where a declared size
            // computed independently of the renderer's own rounding would
            // disagree with the buffer and silently draw nothing.
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 21 11] /Contents 4 0 R >>",
            // The filled rectangle spans the page's full height and its left
            // half, so every row of the raster is ink on the left and paper on
            // the right whatever the rounding does.
            "<< /Length 43 >>\nstream\n0 0 0 rg 0 0 10 11 re f\n1 1 1 rg\nendstream",
            "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>",
        ];
        let mut pdf = String::from("%PDF-1.7\n");
        let mut offsets = Vec::new();
        for (index, body) in objects.iter().enumerate() {
            offsets.push(pdf.len());
            pdf.push_str(&format!("{} 0 obj\n{body}\nendobj\n", index + 1));
        }
        let start = pdf.len();
        pdf.push_str(&format!(
            "xref\n0 {}\n0000000000 65535 f \n",
            objects.len() + 1
        ));
        for offset in &offsets {
            pdf.push_str(&format!("{offset:010} 00000 n \n"));
        }
        pdf.push_str(&format!(
            "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{start}\n%%EOF\n",
            objects.len() + 1
        ));
        pdf.into_bytes()
    }

    fn helper(directory: &Path, name: &str, body: &str) -> PathBuf {
        let path = directory.join(name);
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    fn frame(page: &DocumentPage) -> Vec<u8> {
        let mut frame = frame_header(page);
        frame.extend_from_slice(&page.page.rgba);
        frame
    }

    #[test]
    fn a_page_renders_on_its_declared_dimensions_and_inverts_without_moving_hue() {
        let root = tempdir().unwrap();
        let path = root.path().join("report.pdf");
        std::fs::write(&path, one_page_pdf()).unwrap();
        let rendered = render_page(&path, 0, PdfTone::Original, 1.0).unwrap();
        assert_eq!(rendered.pages, 1);
        assert_eq!(rendered.page.index, 0);
        let width = rendered.page.width as usize;
        assert_eq!(
            rendered.page.rgba.len(),
            width * rendered.page.height as usize * 4
        );
        // The left half of the page is the filled black rectangle and the
        // right half is bare paper, so an untouched render is dark on white.
        assert_eq!(&rendered.page.rgba[..4], &[0, 0, 0, 255]);
        assert_eq!(
            &rendered.page.rgba[(width - 1) * 4..][..4],
            &[255, 255, 255, 255]
        );

        let inverted = render_page(&path, 0, PdfTone::Inverted { paper: [0, 0, 0] }, 1.0).unwrap();
        assert_eq!(
            (inverted.page.width, inverted.page.height),
            (rendered.page.width, rendered.page.height),
            "tone must not change the geometry of the page"
        );
        assert_eq!(&inverted.page.rgba[..4], &[255, 255, 255, 255]);
        assert_eq!(&inverted.page.rgba[(width - 1) * 4..][..4], &[0, 0, 0, 255]);

        assert!(render_page(&path, 1, PdfTone::Original, 1.0).is_err());
        std::fs::write(&path, b"%PDF-1.7\nnonsense").unwrap();
        assert!(render_page(&path, 0, PdfTone::Original, 1.0).is_err());
    }

    #[test]
    fn a_zoomed_page_is_rasterized_larger_and_never_past_the_preview_limits() {
        let root = tempdir().unwrap();
        let path = root.path().join("report.pdf");
        std::fs::write(&path, one_page_pdf()).unwrap();
        let fit = render_page(&path, 0, PdfTone::Original, 1.0).unwrap();
        let zoomed = render_page(&path, 0, PdfTone::Original, 2.0).unwrap();
        assert!(
            zoomed.page.width > fit.page.width && zoomed.page.height > fit.page.height,
            "zooming in has to raise the raster's resolution, not just its layout size: {} by {} \
             against {} by {}",
            zoomed.page.width,
            zoomed.page.height,
            fit.page.width,
            fit.page.height
        );
        assert_eq!(
            zoomed.page.rgba.len(),
            zoomed.page.width as usize * zoomed.page.height as usize * 4
        );

        // A page this size at the largest zoom would be far past the limit,
        // and a reader zooming into a poster wants to see it rather than to be
        // told its dimensions.
        let poster = render_scale(2400.0, 3600.0, PDF_PAGE_SCALE * 3.0).unwrap();
        let (width, height) = (2400.0 * poster, 3600.0 * poster);
        assert!(
            width <= MAX_IMAGE_DIMENSION as f32 && height <= MAX_IMAGE_DIMENSION as f32,
            "{width} by {height} is past the per-axis limit the parent enforces"
        );
        // A page under a point tall is scaled up instead, because hayro floors
        // its pixmap and the parent refuses a zero-height page with a message
        // about it being too large.
        let sliver = render_scale(600.0, 0.4, PDF_PAGE_SCALE * 0.5).unwrap();
        assert!(
            (0.4 * sliver) as u32 >= 1,
            "a {}-pixel axis would be refused as oversize",
            0.4 * sliver
        );
        assert!(600.0 * sliver <= MAX_IMAGE_DIMENSION as f32);
        assert_eq!(render_scale(0.0, 10.0, 1.0), None);
        assert_eq!(render_scale(10.0, 10.0, f32::INFINITY), None);
        assert_eq!(
            render_scale(f32::MAX, 0.000_001, 1.0),
            None,
            "a page no scale can fit in the limit has to be refused rather than clamped"
        );
    }

    #[test]
    fn inverting_tone_turns_lightness_over_without_moving_hue() {
        let pixels = || {
            vec![
                255, 255, 255, 255, // paper
                0, 0, 0, 255, // ink
                230, 230, 230, 255, // a faint rule
                200, 40, 40, 255, // a saturated mid-lightness red
            ]
        };
        let mut absolute = pixels();
        invert_lightness(&mut absolute, [0, 0, 0]);
        assert_eq!(&absolute[..4], &[0, 0, 0, 255]);
        assert_eq!(&absolute[4..8], &[255, 255, 255, 255]);
        assert_eq!(&absolute[8..12], &[25, 25, 25, 255]);
        // Red stays red: its channel order is unchanged and its lightness,
        // already near the middle, barely moves. Negating the channels
        // instead would have turned it cyan.
        assert_eq!(&absolute[12..16], &[215, 55, 55, 255]);

        // The shell's own surface color is where the document's white lands,
        // so a page reads as a sheet on the shell and not a black hole in it.
        // The ink still reaches white, because that is the contrast the
        // reader is there for.
        let paper = [40, 42, 48];
        let mut themed = pixels();
        invert_lightness(&mut themed, paper);
        assert_eq!(&themed[..3], &paper);
        assert_eq!(&themed[4..8], &[255, 255, 255, 255]);
        assert!(
            themed[8..11]
                .iter()
                .zip(paper)
                .all(|(value, floor)| *value > floor && *value < floor + 40),
            "a faint rule stays just off the paper it is printed on"
        );
    }

    /// The helper is a process that just parsed a hostile document, so the
    /// frame it writes is input, not a value the parent may believe.
    #[test]
    fn a_frame_is_refused_unless_it_carries_exactly_what_it_declared() {
        let page = DocumentPage {
            pages: 3,
            page: PdfPage {
                index: 1,
                width: 2,
                height: 1,
                rgba: vec![1, 2, 3, 4, 5, 6, 7, 8].into(),
            },
        };
        let whole = frame(&page);
        assert_eq!(decode_frame(&whole, 1).unwrap(), page);

        let truncated = &whole[..whole.len() - 1];
        assert!(
            decode_frame(truncated, 1)
                .unwrap_err()
                .contains("declared 8 bytes and sent 7")
        );
        assert!(decode_frame(&whole[..FRAME_HEADER_BYTES - 1], 1).is_err());
        let mut padded = whole.clone();
        padded.push(0);
        assert!(decode_frame(&padded, 1).is_err());

        // A length past the preview ceiling is refused on the header alone,
        // before the parent could be talked into allocating for it.
        let mut enormous = whole.clone();
        enormous[36..44].copy_from_slice(&(MAX_RGBA_BYTES + 1).to_le_bytes());
        assert!(decode_frame(&enormous, 1).unwrap_err().contains("past the"));
        let mut wide = whole.clone();
        wide[28..32].copy_from_slice(&(MAX_IMAGE_DIMENSION + 1).to_le_bytes());
        assert!(decode_frame(&wide, 1).is_err());

        // Dimensions and length have to agree, or the upload draws nothing.
        let mut mismatched = whole.clone();
        mismatched[32..36].copy_from_slice(&2u32.to_le_bytes());
        assert!(decode_frame(&mismatched, 1).is_err());

        // The page that arrives has to be the page that was asked for, and a
        // page count outside the preview's own bound is not usable either.
        assert!(decode_frame(&whole, 0).is_err());
        let mut uncountable = whole.clone();
        uncountable[12..20].copy_from_slice(&0u64.to_le_bytes());
        assert!(decode_frame(&uncountable, 1).is_err());

        let mut foreign = whole.clone();
        foreign[8..12].copy_from_slice(&(FRAME_VERSION + 1).to_le_bytes());
        assert!(decode_frame(&foreign, 1).is_err());
        let mut alien = whole.clone();
        alien[0] = b'X';
        assert!(decode_frame(&alien, 1).is_err());
    }

    #[test]
    fn a_helper_that_fails_is_reported_as_its_status_and_its_last_words() {
        // Long enough that a shell script's own startup cannot be mistaken
        // for the helper hanging, and short enough to fail fast if it is.
        let patience = Duration::from_secs(5);
        let root = tempdir().unwrap();
        let document = root.path().join("report.pdf");
        std::fs::write(&document, one_page_pdf()).unwrap();

        // A deliberate refusal reaches the reader as the helper phrased it.
        let refusing = helper(
            root.path(),
            "refuse",
            &format!("echo 'This PDF declares no pages.' >&2\nexit {EXIT_DOCUMENT_REFUSED}"),
        );
        assert_eq!(
            render_with_helper(
                &refusing,
                &document,
                0,
                PdfTone::Original,
                PdfZoom::default(),
                patience
            ),
            Err("This PDF declares no pages.".to_owned())
        );

        // A helper that dies carries no sentence of its own, so the status is
        // what the reader gets, with whatever the runtime printed.
        let aborting = helper(
            root.path(),
            "abort",
            "echo 'fatal runtime error: stack overflow, aborting' >&2\nkill -ABRT $$",
        );
        let message = render_with_helper(
            &aborting,
            &document,
            0,
            PdfTone::Original,
            PdfZoom::default(),
            patience,
        )
        .unwrap_err();
        assert!(
            message.contains("killed by signal 6") && message.contains("stack overflow"),
            "a crashed helper must report its signal and its last words: {message}"
        );

        // Success with nothing usable on stdout is still a failure, and the
        // reason the frame was unusable is what there is to say about it. The
        // nonsense is header-length on purpose, so it is refused for what it
        // says rather than for being short.
        let babbling = helper(
            root.path(),
            "babble",
            "printf 'not a frame not a frame not a frame not a frame'",
        );
        let message = render_with_helper(
            &babbling,
            &document,
            0,
            PdfTone::Original,
            PdfZoom::default(),
            patience,
        )
        .unwrap_err();
        assert!(
            message.contains("exit status 0") && message.contains("not a page"),
            "a helper that answers with nonsense must say so: {message}"
        );
    }

    #[test]
    fn a_helper_that_never_answers_is_killed_rather_than_waited_on() {
        let root = tempdir().unwrap();
        let document = root.path().join("report.pdf");
        std::fs::write(&document, one_page_pdf()).unwrap();
        // Holding stdout open with nothing on it is the shape a blocking read
        // would never return from.
        let silent = helper(root.path(), "silent", "sleep 120");
        let started = Instant::now();
        assert_eq!(
            render_with_helper(
                &silent,
                &document,
                0,
                PdfTone::Original,
                PdfZoom::default(),
                Duration::from_millis(250)
            ),
            Err("This document could not be rendered in time.".to_owned())
        );
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "the timeout must not wait on the helper it gave up on"
        );
    }
}
