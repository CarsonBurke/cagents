//! A PDF read as one scrolling column of pages.
//!
//! The editor's preview turns one page at a time; a reader who opens a paper
//! in the browser expects what every browser does instead: the whole document
//! in a column they scroll through, with the page they are on named, and a
//! zoom that fits the page to the pane until they choose a size.
//!
//! Pages are rasterized by the same sandboxed helper the editor uses
//! ([`crate::document_render`]), one process per page, so a hostile document
//! costs a helper rather than the window and no parsing happens on the UI
//! thread. Only pages at or near the viewport are rendered, at most
//! [`MAX_RENDERS`] at a time, and rasters far from it are dropped, so the
//! memory held stays a few pages' worth however long the document is.
//!
//! The column itself is [`PageColumn`], a widget that lays the pages out from
//! their printed sizes before any of them is drawn, draws only the ones that
//! intersect the viewport, and reports which pages those are.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::Arc,
};

use cosmic::{
    Element, Renderer, Task, Theme,
    iced::{
        self, Color, Event, Length, Rectangle, Size,
        advanced::{
            Clipboard, Layout, Shell, Widget, layout, mouse, renderer,
            widget::{Tree, tree},
        },
    },
    widget,
};

use crate::{
    design::{self, space},
    editor::{PDF_LOGICAL_SCALE, PageSize, PdfTone},
    ui_controls::{Icon, icon_button},
};

/// How many page renders may run at once.
///
/// Each is a helper process parsing the document, so this is also how many
/// cores a fast scroll can occupy. Two keeps the page being read and the next
/// one moving without the rest of the machine noticing.
const MAX_RENDERS: usize = 2;

/// Pages beyond the visible ones, on each side, that are rendered ahead so a
/// reader scrolling at reading speed rarely meets a blank sheet.
const RENDER_AHEAD: usize = 1;

/// Pages beyond the visible ones, on each side, whose rasters are kept.
/// Anything further is dropped, because a page at reading size is several
/// megabytes of pixels.
const KEEP_AROUND: usize = 3;

/// The gap between two pages and around the column, in logical pixels.
const PAGE_GAP: f32 = space::M;

/// The zoom steps a reader moves through, as fractions of the printed size.
const STEPS: [f32; 11] = [0.5, 0.67, 0.75, 0.9, 1.0, 1.1, 1.25, 1.5, 2.0, 3.0, 4.0];

/// The smallest and largest fraction of the printed size a fitted page may be
/// drawn at, so a narrow pane still shows text and a very wide one does not
/// ask for a poster.
const FIT_BOUNDS: (f32, f32) = (0.25, 4.0);

/// The width assumed before the column has been laid out in a pane.
const FIRST_PANE_WIDTH: f32 = 800.0;

/// How much sharper or softer than wanted a raster may be before the page is
/// rendered again. A small mismatch is drawn scaled, which is invisible,
/// rather than spending a render on every step of a pane resize.
const SHARPNESS_TOLERANCE: (f32, f32) = (0.85, 1.6);

/// How large the pages are drawn.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Zoom {
    /// The widest page fills the pane's width.
    #[default]
    FitWidth,
    /// A fraction of the printed size, indexed into [`STEPS`].
    Step(usize),
}

impl Zoom {
    /// The fraction of the printed size this draws, given the fraction that
    /// would fit the pane.
    fn fraction(self, fitted: f32) -> f32 {
        match self {
            Self::FitWidth => fitted,
            Self::Step(step) => STEPS[step.min(STEPS.len() - 1)],
        }
    }

    /// The next step larger than what is drawn, or `None` at the largest.
    fn larger(self, fitted: f32) -> Option<Self> {
        let current = self.fraction(fitted);
        STEPS
            .iter()
            .position(|step| *step > current + 0.005)
            .map(Self::Step)
    }

    /// The next step smaller than what is drawn, or `None` at the smallest.
    fn smaller(self, fitted: f32) -> Option<Self> {
        let current = self.fraction(fitted);
        STEPS
            .iter()
            .rposition(|step| *step < current - 0.005)
            .map(Self::Step)
    }
}

/// Where the column sits in its pane, as the widget last laid it out.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Visible {
    /// The first and last pages that intersect the viewport.
    pub first: usize,
    pub last: usize,
    /// The page the reader is on: the one under the viewport's upper third.
    pub current: usize,
    /// How far into `current` that line falls, as a fraction of its height,
    /// so a zoom can keep the same line in place.
    pub within: f32,
    /// The pane's width in logical pixels, which `Zoom::FitWidth` fills.
    pub pane_width: f32,
}

#[derive(Debug, Clone)]
pub enum PdfAction {
    Visible(Visible),
    Rendered {
        page: usize,
        request: u64,
        result: Result<crate::editor::DocumentPage, String>,
    },
    ZoomIn,
    ZoomOut,
    FitWidth,
    ToggleTone,
    PreviousPage,
    NextPage,
}

#[derive(Debug, Clone)]
struct Raster {
    handle: iced::widget::image::Handle,
    pixels_per_point: f32,
    tone: PdfTone,
}

#[derive(Debug, Clone, Copy)]
struct Render {
    request: u64,
    pixels_per_point: f32,
    tone: PdfTone,
}

/// One open document and how it is being read.
#[derive(Debug)]
pub struct PdfDocument {
    path: PathBuf,
    /// Every page's printed size, learned from the first page rendered.
    sizes: Arc<[PageSize]>,
    zoom: Zoom,
    tone: PdfTone,
    /// Whether the reader chose the tone, which a theme change then leaves
    /// alone.
    tone_chosen: bool,
    /// Device pixels per logical pixel, so a raster is sharp on the display
    /// it is drawn on.
    scale_factor: f32,
    rasters: HashMap<usize, Raster>,
    renders: HashMap<usize, Render>,
    /// Pages whose render failed at the current size and tone, with why.
    failed: HashMap<usize, String>,
    visible: Option<Visible>,
    next_request: u64,
    scroll: widget::Id,
}

impl PdfDocument {
    /// Opens the document at `path`, starting the render of its first page.
    ///
    /// The helper reports every page's size with that page, so until it
    /// answers the column has nothing to lay out and says it is opening.
    pub fn open(path: PathBuf, tone: PdfTone, scale_factor: f32) -> (Self, Task<PdfAction>) {
        let mut document = Self {
            path,
            sizes: Arc::from([]),
            zoom: Zoom::default(),
            tone,
            tone_chosen: false,
            scale_factor: sanitize_scale(scale_factor),
            rasters: HashMap::new(),
            renders: HashMap::new(),
            failed: HashMap::new(),
            visible: None,
            next_request: 0,
            scroll: widget::Id::unique(),
        };
        let pixels_per_point = document.wanted_pixels_per_point(FIRST_PANE_WIDTH);
        let task = document.start_render(0, pixels_per_point);
        (document, task)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn page_count(&self) -> usize {
        self.sizes.len()
    }

    /// Why the document could not be shown at all, once its first page has
    /// failed with nothing laid out.
    pub fn failure(&self) -> Option<&str> {
        self.sizes
            .is_empty()
            .then(|| self.failed.get(&0).map(String::as_str))
            .flatten()
    }

    /// The device scale changed, as when the window moved to another display.
    pub fn set_scale_factor(&mut self, scale_factor: f32) -> Task<PdfAction> {
        let scale_factor = sanitize_scale(scale_factor);
        if (scale_factor - self.scale_factor).abs() < f32::EPSILON {
            return Task::none();
        }
        self.scale_factor = scale_factor;
        self.schedule()
    }

    /// The shell's theme changed. A tone the reader chose is theirs to keep.
    pub fn set_theme_tone(&mut self, tone: PdfTone) -> Task<PdfAction> {
        if self.tone_chosen || self.tone == tone {
            return Task::none();
        }
        self.tone = tone;
        self.failed.clear();
        self.schedule()
    }

    pub fn update(&mut self, action: PdfAction) -> Task<PdfAction> {
        match action {
            PdfAction::Visible(visible) => {
                if self.visible == Some(visible) {
                    return Task::none();
                }
                let width_changed = self
                    .visible
                    .is_none_or(|previous| previous.pane_width != visible.pane_width);
                self.visible = Some(visible);
                if width_changed {
                    self.failed.clear();
                }
                self.schedule()
            }
            PdfAction::Rendered {
                page,
                request,
                result,
            } => {
                let Some(render) = self.renders.get(&page).copied() else {
                    return self.schedule();
                };
                if render.request != request {
                    return self.schedule();
                }
                self.renders.remove(&page);
                match result {
                    Ok(rendered) => {
                        if self.sizes.is_empty() || self.sizes.len() != rendered.pages {
                            self.sizes = rendered.sizes;
                        }
                        self.failed.remove(&page);
                        let raster = rendered.page;
                        self.rasters.insert(
                            page,
                            Raster {
                                handle: iced::widget::image::Handle::from_rgba(
                                    raster.width,
                                    raster.height,
                                    raster.rgba.to_vec(),
                                ),
                                pixels_per_point: raster.width as f32 / raster.points_wide,
                                tone: render.tone,
                            },
                        );
                    }
                    Err(message) => {
                        self.failed.insert(page, message);
                    }
                }
                self.schedule()
            }
            PdfAction::ZoomIn => self.zoom_to(self.zoom.larger(self.fitted())),
            PdfAction::ZoomOut => self.zoom_to(self.zoom.smaller(self.fitted())),
            PdfAction::FitWidth => self.zoom_to(Some(Zoom::FitWidth)),
            PdfAction::ToggleTone => {
                self.tone_chosen = true;
                self.tone = match self.tone {
                    PdfTone::Original => {
                        crate::repository_widget::pdf_tone_inverted(&cosmic::theme::active())
                    }
                    PdfTone::Inverted { .. } => PdfTone::Original,
                };
                self.failed.clear();
                self.schedule()
            }
            PdfAction::PreviousPage => {
                let current = self.current_page();
                self.go_to(current.saturating_sub(1))
            }
            PdfAction::NextPage => {
                let current = self.current_page();
                self.go_to((current + 1).min(self.sizes.len().saturating_sub(1)))
            }
        }
    }

    /// The page the reader is on.
    pub fn current_page(&self) -> usize {
        self.visible.map_or(0, |visible| visible.current)
    }

    pub fn zoom(&self) -> Zoom {
        self.zoom
    }

    /// The fraction of the printed size the pages are drawn at.
    pub fn zoom_fraction(&self) -> f32 {
        self.zoom.fraction(self.fitted())
    }

    pub fn tone(&self) -> PdfTone {
        self.tone
    }

    /// The fraction of the printed size that fits the widest page to the
    /// pane.
    fn fitted(&self) -> f32 {
        let pane = self
            .visible
            .map_or(FIRST_PANE_WIDTH, |visible| visible.pane_width);
        fitted_fraction(&self.sizes, pane)
    }

    fn zoom_to(&mut self, zoom: Option<Zoom>) -> Task<PdfAction> {
        let Some(zoom) = zoom else {
            return Task::none();
        };
        if zoom == self.zoom {
            return Task::none();
        }
        self.zoom = zoom;
        self.failed.clear();
        // Keep the line the reader was on where it was: the column's length
        // changes with the zoom, so the scroll offset that showed it does not
        // show it any more.
        let anchor = self
            .visible
            .map(|visible| (visible.current, visible.within));
        let scroll = anchor.map_or_else(Task::none, |(page, within)| {
            let geometry = self.geometry();
            let top = geometry.page_top(page) + within * geometry.page_height(page);
            // The line sat a third of the way down the viewport, which the
            // widget does not report; the page's own top is close enough and
            // never scrolls the page out of view.
            self.scroll_to_offset(top - geometry.gap)
        });
        Task::batch([scroll, self.schedule()])
    }

    fn go_to(&mut self, page: usize) -> Task<PdfAction> {
        if self.sizes.is_empty() {
            return Task::none();
        }
        let geometry = self.geometry();
        self.scroll_to_offset(geometry.page_top(page) - geometry.gap)
    }

    fn scroll_to_offset(&self, y: f32) -> Task<PdfAction> {
        iced::widget::operation::scroll_to(
            self.scroll.clone(),
            iced::widget::operation::AbsoluteOffset {
                x: None,
                y: Some(y.max(0.0)),
            },
        )
    }

    fn geometry(&self) -> Geometry {
        let pane = self
            .visible
            .map_or(FIRST_PANE_WIDTH, |visible| visible.pane_width);
        Geometry::new(&self.sizes, self.zoom, pane)
    }

    fn wanted_pixels_per_point(&self, pane_width: f32) -> f32 {
        let fraction = self.zoom.fraction(fitted_fraction(&self.sizes, pane_width));
        quantize(fraction * PDF_LOGICAL_SCALE * self.scale_factor)
    }

    /// Starts the renders the viewport needs, nearest first, and drops the
    /// rasters it no longer does.
    fn schedule(&mut self) -> Task<PdfAction> {
        if self.sizes.is_empty() {
            // Still waiting for the first page, which brings the layout.
            return Task::none();
        }
        let visible = self.visible.unwrap_or(Visible {
            first: 0,
            last: 0,
            current: 0,
            within: 0.0,
            pane_width: FIRST_PANE_WIDTH,
        });
        let last_page = self.sizes.len() - 1;
        let first = visible.first.min(last_page);
        let last = visible.last.clamp(first, last_page);
        let keep = first.saturating_sub(KEEP_AROUND)..=(last + KEEP_AROUND).min(last_page);
        self.rasters.retain(|page, _| keep.contains(page));
        let wanted = self.wanted_pixels_per_point(visible.pane_width);
        // The current page first, then the rest on screen downward, then the
        // ones just past either edge.
        let ahead = first.saturating_sub(RENDER_AHEAD)..=(last + RENDER_AHEAD).min(last_page);
        let current = visible.current.clamp(first, last);
        let mut order = vec![current];
        order.extend((first..=last).filter(|page| *page != current));
        order.extend(ahead.filter(|page| *page < first || *page > last));
        let mut tasks = Vec::new();
        for page in order {
            if self.renders.len() >= MAX_RENDERS {
                break;
            }
            if self.failed.contains_key(&page) || !self.needs_render(page, wanted) {
                continue;
            }
            tasks.push(self.start_render(page, wanted));
        }
        Task::batch(tasks)
    }

    fn needs_render(&self, page: usize, wanted: f32) -> bool {
        if let Some(render) = self.renders.get(&page) {
            return !adequate(render.pixels_per_point, render.tone, wanted, self.tone);
        }
        self.rasters
            .get(&page)
            .is_none_or(|raster| !adequate(raster.pixels_per_point, raster.tone, wanted, self.tone))
    }

    fn start_render(&mut self, page: usize, pixels_per_point: f32) -> Task<PdfAction> {
        self.next_request = self.next_request.wrapping_add(1);
        let request = self.next_request;
        let tone = self.tone;
        self.renders.insert(
            page,
            Render {
                request,
                pixels_per_point,
                tone,
            },
        );
        let path = self.path.clone();
        cosmic::task::future(async move {
            let result = tokio::task::spawn_blocking(move || {
                crate::editor::render_document_page_at(&path, page, tone, pixels_per_point)
            })
            .await
            .unwrap_or_else(|error| Err(format!("The page worker failed: {error}.")));
            PdfAction::Rendered {
                page,
                request,
                result,
            }
        })
    }

    /// The zoom and page controls, for the host to place in its toolbar
    /// beside its own.
    pub fn controls(&self) -> Vec<Element<'_, PdfAction>> {
        let pages = self.sizes.len();
        let current = self.current_page();
        let fitted = self.fitted();
        let mut controls: Vec<Element<'_, PdfAction>> = Vec::with_capacity(8);
        if pages > 1 {
            controls.push(
                icon_button(Icon::ChevronUp, "Previous page")
                    .on_press_maybe((current > 0).then_some(PdfAction::PreviousPage))
                    .into(),
            );
            controls.push(
                icon_button(Icon::ChevronDown, "Next page")
                    .on_press_maybe((current + 1 < pages).then_some(PdfAction::NextPage))
                    .into(),
            );
        }
        if pages > 0 {
            controls.push(
                design::body(format!("Page {} of {pages}", current + 1))
                    .wrapping(iced::widget::text::Wrapping::None)
                    .into(),
            );
        }
        controls.push(widget::Space::new().width(space::S).into());
        controls.push(
            icon_button(Icon::ZoomOut, "Zoom out")
                .on_press_maybe(
                    (pages > 0 && self.zoom.smaller(fitted).is_some())
                        .then_some(PdfAction::ZoomOut),
                )
                .into(),
        );
        controls.push(
            widget::container(
                design::body(format!(
                    "{}%",
                    (self.zoom.fraction(fitted) * 100.0).round() as u32
                ))
                .width(Length::Fill)
                .center(),
            )
            .width(Length::Fixed(ZOOM_LABEL_WIDTH))
            .into(),
        );
        controls.push(
            icon_button(Icon::ZoomIn, "Zoom in")
                .on_press_maybe(
                    (pages > 0 && self.zoom.larger(fitted).is_some()).then_some(PdfAction::ZoomIn),
                )
                .into(),
        );
        controls.push(
            icon_button(Icon::ZoomFit, "Fit to width")
                .selected(self.zoom == Zoom::FitWidth)
                .on_press(PdfAction::FitWidth)
                .into(),
        );
        let inverted = matches!(self.tone, PdfTone::Inverted { .. });
        controls.push(
            icon_button(
                if inverted {
                    Icon::Brightness
                } else {
                    Icon::Night
                },
                if inverted {
                    "Show original colors"
                } else {
                    "Use dark colors"
                },
            )
            .selected(inverted)
            .on_press(PdfAction::ToggleTone)
            .into(),
        );
        controls
    }

    /// The scrolling column of pages.
    pub fn view(&self) -> Element<'_, PdfAction> {
        let paper = match self.tone {
            PdfTone::Original => Color::WHITE,
            PdfTone::Inverted { paper: [r, g, b] } => Color::from_rgb8(r, g, b),
        };
        let column = PageColumn {
            sizes: &self.sizes,
            zoom: self.zoom,
            rasters: &self.rasters,
            paper,
            on_visible: Box::new(PdfAction::Visible),
        };
        widget::scrollable(column)
            .id(self.scroll.clone())
            .direction(iced::widget::scrollable::Direction::Both {
                vertical: design::scrollbar(),
                horizontal: design::scrollbar(),
            })
            .width(Length::Fill)
            .height(Length::Fill)
            .into()
    }
}

/// The tone a page is read in on `theme`: the document's own colors on a
/// light theme, and adapted for a dark one.
pub fn theme_tone(theme: &Theme) -> PdfTone {
    crate::repository_widget::pdf_tone(theme)
}

/// The zoom label's width, fixed because "75%" and "100%" differ and the
/// label sits between the two controls a reader presses repeatedly.
const ZOOM_LABEL_WIDTH: f32 = 48.0;

fn sanitize_scale(scale_factor: f32) -> f32 {
    if scale_factor.is_finite() && scale_factor > 0.0 {
        scale_factor.clamp(0.25, 8.0)
    } else {
        1.0
    }
}

/// Rounds a raster resolution up to a quarter pixel per point, so the
/// resolutions asked for while a pane is being resized fall on a few values
/// rather than a new one every frame.
fn quantize(pixels_per_point: f32) -> f32 {
    ((pixels_per_point * 4.0).ceil() / 4.0).max(0.25)
}

fn adequate(have: f32, have_tone: PdfTone, wanted: f32, tone: PdfTone) -> bool {
    have_tone == tone
        && have >= wanted * SHARPNESS_TOLERANCE.0
        && have <= wanted * SHARPNESS_TOLERANCE.1
}

/// The fraction of the printed size at which the widest page fills a pane
/// `pane_width` wide, inside the gaps either side.
fn fitted_fraction(sizes: &[PageSize], pane_width: f32) -> f32 {
    let widest = sizes.iter().map(|size| size.width).fold(0.0_f32, f32::max);
    if widest <= 0.0 {
        return 1.0;
    }
    let usable = (pane_width - 2.0 * PAGE_GAP - design::SCROLLBAR_GUTTER).max(1.0);
    (usable / (widest * PDF_LOGICAL_SCALE)).clamp(FIT_BOUNDS.0, FIT_BOUNDS.1)
}

/// Where every page sits in the column, in logical pixels.
#[derive(Debug, Clone)]
struct Geometry {
    gap: f32,
    /// Each page's top, plus the column's full height last.
    tops: Vec<f32>,
    width: f32,
    pane_width: f32,
    heights: Vec<f32>,
    widths: Vec<f32>,
}

impl Geometry {
    fn new(sizes: &[PageSize], zoom: Zoom, pane_width: f32) -> Self {
        let scale = zoom.fraction(fitted_fraction(sizes, pane_width)) * PDF_LOGICAL_SCALE;
        let gap = PAGE_GAP;
        let widths: Vec<f32> = sizes.iter().map(|size| size.width * scale).collect();
        let heights: Vec<f32> = sizes.iter().map(|size| size.height * scale).collect();
        let mut tops = Vec::with_capacity(sizes.len() + 1);
        let mut y = gap;
        for height in &heights {
            tops.push(y);
            y += height + gap;
        }
        tops.push(y);
        let widest = widths.iter().copied().fold(0.0_f32, f32::max);
        let width = (widest + 2.0 * gap + design::SCROLLBAR_GUTTER).max(pane_width);
        Self {
            gap,
            tops,
            width,
            pane_width,
            heights,
            widths,
        }
    }

    fn height(&self) -> f32 {
        self.tops.last().copied().unwrap_or(0.0) + design::SCROLLBAR_GUTTER
    }

    fn page_top(&self, page: usize) -> f32 {
        self.tops
            .get(page.min(self.heights.len().saturating_sub(1)))
            .copied()
            .unwrap_or(0.0)
    }

    fn page_height(&self, page: usize) -> f32 {
        self.heights.get(page).copied().unwrap_or(0.0)
    }

    /// The page's rectangle, relative to the column's origin, centred in the
    /// column's width.
    fn page_rect(&self, page: usize) -> Rectangle {
        let width = self.widths[page];
        let usable = self.width - design::SCROLLBAR_GUTTER;
        Rectangle {
            x: ((usable - width) / 2.0).max(self.gap),
            y: self.tops[page],
            width,
            height: self.heights[page],
        }
    }

    /// The pages a band of the column from `top` to `bottom` touches.
    fn pages_between(&self, top: f32, bottom: f32) -> Option<(usize, usize)> {
        if self.heights.is_empty() {
            return None;
        }
        let first = self.page_at(top);
        let last = self.page_at(bottom).max(first);
        Some((first, last))
    }

    /// The page at height `y`, counting a gap as the page above it.
    fn page_at(&self, y: f32) -> usize {
        let pages = self.heights.len();
        match self.tops[..pages]
            .binary_search_by(|top| top.partial_cmp(&y).unwrap_or(std::cmp::Ordering::Less))
        {
            Ok(page) => page,
            Err(0) => 0,
            Err(next) => (next - 1).min(pages - 1),
        }
    }
}

/// The column of pages: laid out from their sizes, drawn only where seen.
struct PageColumn<'a> {
    sizes: &'a [PageSize],
    zoom: Zoom,
    rasters: &'a HashMap<usize, Raster>,
    paper: Color,
    on_visible: Box<dyn Fn(Visible) -> PdfAction + 'a>,
}

struct ColumnState {
    a11y: iced_accessibility::Id,
    /// The width of the pane the column is scrolled in, learned from the
    /// viewport, which a scrollable on both axes does not pass to layout.
    pane_width: Option<f32>,
    published: Option<Visible>,
    geometry: Option<Geometry>,
}

impl Default for ColumnState {
    fn default() -> Self {
        Self {
            a11y: iced_accessibility::Id::unique(),
            pane_width: None,
            published: None,
            geometry: None,
        }
    }
}

impl PageColumn<'_> {
    fn geometry(&self, state: &ColumnState, limits_width: f32) -> Geometry {
        let pane = state.pane_width.unwrap_or(if limits_width.is_finite() {
            limits_width
        } else {
            FIRST_PANE_WIDTH
        });
        Geometry::new(self.sizes, self.zoom, pane)
    }
}

impl Widget<PdfAction, Theme, Renderer> for PageColumn<'_> {
    fn tag(&self) -> tree::Tag {
        tree::Tag::of::<ColumnState>()
    }

    fn state(&self) -> tree::State {
        tree::State::new(ColumnState::default())
    }

    fn size(&self) -> Size<Length> {
        Size::new(Length::Shrink, Length::Shrink)
    }

    fn layout(
        &mut self,
        tree: &mut Tree,
        _renderer: &Renderer,
        limits: &layout::Limits,
    ) -> layout::Node {
        let state = tree.state.downcast_mut::<ColumnState>();
        let geometry = self.geometry(state, limits.max().width);
        let size = Size::new(geometry.width, geometry.height());
        state.geometry = Some(geometry);
        layout::Node::new(size)
    }

    fn update(
        &mut self,
        tree: &mut Tree,
        _event: &Event,
        layout: Layout<'_>,
        _cursor: mouse::Cursor,
        _renderer: &Renderer,
        _clipboard: &mut dyn Clipboard,
        shell: &mut Shell<'_, PdfAction>,
        viewport: &Rectangle,
    ) {
        let state = tree.state.downcast_mut::<ColumnState>();
        if state.pane_width != Some(viewport.width) && viewport.width > 0.0 {
            state.pane_width = Some(viewport.width);
            shell.invalidate_layout();
            shell.request_redraw();
            return;
        }
        let Some(geometry) = &state.geometry else {
            return;
        };
        let bounds = layout.bounds();
        let top = viewport.y - bounds.y;
        let bottom = top + viewport.height;
        let Some((first, last)) = geometry.pages_between(top, bottom) else {
            return;
        };
        let reading = top + viewport.height / 3.0;
        let current = geometry.page_at(reading);
        let height = geometry.page_height(current).max(1.0);
        let within = ((reading - geometry.page_top(current)) / height).clamp(0.0, 1.0);
        let visible = Visible {
            first,
            last,
            current,
            // Coarse on purpose: this is only where a zoom re-anchors, and a
            // finer value would send a message on every scrolled pixel.
            within: (within * 50.0).round() / 50.0,
            pane_width: geometry.pane_width,
        };
        if state.published != Some(visible) {
            state.published = Some(visible);
            shell.publish((self.on_visible)(visible));
        }
    }

    fn draw(
        &self,
        tree: &Tree,
        renderer: &mut Renderer,
        theme: &Theme,
        _style: &renderer::Style,
        layout: Layout<'_>,
        _cursor: mouse::Cursor,
        viewport: &Rectangle,
    ) {
        use iced::advanced::{Renderer as _, image::Renderer as _};
        let state = tree.state.downcast_ref::<ColumnState>();
        let Some(geometry) = &state.geometry else {
            return;
        };
        let bounds = layout.bounds();
        let top = viewport.y - bounds.y;
        let Some((first, last)) = geometry.pages_between(top, top + viewport.height) else {
            return;
        };
        let edge = design::rule(theme);
        for page in first..=last {
            let rect = geometry.page_rect(page);
            let rect = Rectangle {
                x: bounds.x + rect.x,
                y: bounds.y + rect.y,
                ..rect
            };
            if !rect.intersects(viewport) {
                continue;
            }
            // The sheet first, so a page still rendering is paper with an
            // edge rather than a hole in the column.
            renderer.fill_quad(
                renderer::Quad {
                    bounds: rect,
                    border: iced::Border {
                        color: edge,
                        width: design::BORDER_WIDTH,
                        radius: 0.0.into(),
                    },
                    ..renderer::Quad::default()
                },
                self.paper,
            );
            if let Some(raster) = self.rasters.get(&page) {
                renderer.draw_image(
                    iced::advanced::image::Image {
                        handle: raster.handle.clone(),
                        filter_method: iced::advanced::image::FilterMethod::Linear,
                        rotation: iced::Radians(0.0),
                        border_radius: 0.0.into(),
                        opacity: 1.0,
                        snap: true,
                    },
                    rect,
                    *viewport,
                );
            }
        }
    }

    fn a11y_nodes(
        &self,
        layout: Layout<'_>,
        tree: &Tree,
        _cursor: mouse::Cursor,
    ) -> iced_accessibility::A11yTree {
        use iced_accessibility::accesskit::{Node, Rect, Role};
        let bounds = layout.bounds() - layout.virtual_offset();
        let mut node = Node::new(Role::Document);
        node.set_bounds(Rect::new(
            f64::from(bounds.x),
            f64::from(bounds.y),
            f64::from(bounds.x + bounds.width),
            f64::from(bounds.y + bounds.height),
        ));
        node.set_label(match self.sizes.len() {
            1 => "PDF document, 1 page".to_owned(),
            pages => format!("PDF document, {pages} pages"),
        });
        iced_accessibility::A11yTree::leaf(
            node,
            tree.state.downcast_ref::<ColumnState>().a11y.clone(),
        )
    }
}

impl<'a> From<PageColumn<'a>> for Element<'a, PdfAction> {
    fn from(column: PageColumn<'a>) -> Self {
        Element::new(column)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The printed size's place in [`STEPS`].
    const PRINTED_STEP: usize = 4;

    fn letter(pages: usize) -> Vec<PageSize> {
        vec![
            PageSize {
                width: 612.0,
                height: 792.0,
            };
            pages
        ]
    }

    #[test]
    fn a_fitted_page_fills_the_pane_inside_its_gaps() {
        let sizes = letter(3);
        let geometry = Geometry::new(&sizes, Zoom::FitWidth, 900.0);
        let rect = geometry.page_rect(0);
        let usable = 900.0 - 2.0 * PAGE_GAP - design::SCROLLBAR_GUTTER;
        assert!(
            (rect.width - usable).abs() < 0.5,
            "{} against {usable}",
            rect.width
        );
        assert!((rect.x - PAGE_GAP).abs() < 0.5);
        assert_eq!(
            geometry.width, 900.0,
            "a fitted column never scrolls sideways"
        );
    }

    #[test]
    fn pages_stack_with_a_gap_and_are_found_by_height() {
        let sizes = letter(4);
        let geometry = Geometry::new(&sizes, Zoom::Step(PRINTED_STEP), 2000.0);
        let page_height = 792.0 * PDF_LOGICAL_SCALE;
        assert!((geometry.page_height(1) - page_height).abs() < 0.01);
        assert!(
            (geometry.page_top(1) - (PAGE_GAP * 2.0 + page_height)).abs() < 0.01,
            "the second page starts one page and two gaps down"
        );
        assert_eq!(geometry.page_at(0.0), 0);
        assert_eq!(geometry.page_at(geometry.page_top(2) + 1.0), 2);
        // A gap belongs to the page above it.
        assert_eq!(geometry.page_at(geometry.page_top(2) - 1.0), 1);
        assert_eq!(geometry.page_at(f32::MAX), 3);
        assert_eq!(
            geometry.pages_between(geometry.page_top(1) + 5.0, geometry.page_top(3) - 5.0),
            Some((1, 2))
        );
        // A narrow page is centred in a column sized for the widest.
        let mut mixed = letter(2);
        mixed[1].width = 306.0;
        let geometry = Geometry::new(&mixed, Zoom::Step(PRINTED_STEP), 600.0);
        let wide = geometry.page_rect(0);
        let narrow = geometry.page_rect(1);
        assert!(
            geometry.width > 600.0,
            "a page wider than the pane scrolls sideways"
        );
        assert!(
            (narrow.center_x() - wide.center_x()).abs() < 0.5,
            "pages share one centre line"
        );
    }

    #[test]
    fn zoom_steps_move_away_from_the_fitted_size_in_the_direction_asked() {
        // A pane where the page fits at 1.18 of its printed size.
        let fitted = 1.18;
        assert_eq!(Zoom::FitWidth.larger(fitted), Some(Zoom::Step(6)));
        assert_eq!(Zoom::FitWidth.smaller(fitted), Some(Zoom::Step(5)));
        assert_eq!(Zoom::Step(STEPS.len() - 1).larger(fitted), None);
        assert_eq!(Zoom::Step(0).smaller(fitted), None);
        assert_eq!(Zoom::Step(PRINTED_STEP).larger(fitted), Some(Zoom::Step(5)));
    }

    #[test]
    fn a_raster_is_kept_until_it_is_noticeably_too_soft_or_too_large() {
        let tone = PdfTone::Original;
        assert!(adequate(2.0, tone, 2.0, tone));
        assert!(
            adequate(1.8, tone, 2.0, tone),
            "a slightly soft page is drawn scaled"
        );
        assert!(!adequate(1.5, tone, 2.0, tone));
        assert!(
            !adequate(4.0, tone, 2.0, tone),
            "an oversized raster wastes memory"
        );
        assert!(
            !adequate(2.0, tone, 2.0, PdfTone::Inverted { paper: [0, 0, 0] }),
            "a page in the other tone is rendered again"
        );
        assert_eq!(quantize(1.01), 1.25);
        assert_eq!(quantize(1.25), 1.25);
    }
}
