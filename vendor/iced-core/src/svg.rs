//! Load and draw vector graphics.
use crate::time::{Duration, Instant};
use crate::{Color, Radians, Rectangle, Size};

use rustc_hash::FxHasher;
use std::borrow::Cow;
use std::hash::{Hash, Hasher as _};
use std::path::PathBuf;
use std::sync::Arc;

/// A raster image that can be drawn.
#[derive(Debug, Clone, PartialEq)]
pub struct Svg<H = Handle> {
    /// The handle of the [`Svg`].
    pub handle: H,

    /// The [`Color`] filter to be applied to the [`Svg`].
    ///
    /// If some [`Color`] is set, the whole [`Svg`] will be
    /// painted with it—ignoring any intrinsic colors.
    ///
    /// This can be useful for coloring icons programmatically
    /// (e.g. with a theme).
    pub color: Option<Color>,

    /// The rotation to be applied to the image; on its center.
    pub rotation: Radians,

    /// The opacity of the [`Svg`].
    ///
    /// 0 means transparent. 1 means opaque.
    pub opacity: f32,

    /// The border radius for the svg
    pub border_radius: [f32; 4],

    /// The turning of the [`Svg`] with time, on top of its `rotation`.
    pub spin: Option<Spin>,
}

/// A steady turning of an [`Svg`] that its renderer works out from the clock
/// whenever it presents a frame.
///
/// A spinning drawing keeps turning when its window presents its last drawing
/// again, as it does for frames requested with a
/// [`window::AnimationRequest`](crate::window::AnimationRequest). A renderer
/// that cannot spin draws the [`Svg`] at its `rotation`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Spin {
    /// The instant at which the [`Svg`] is at its `rotation`.
    pub since: Instant,

    /// How long a full turn takes. A zero period does not turn.
    pub period: Duration,
}

impl Spin {
    /// Returns how far the drawing has turned at `now`, within a single turn.
    ///
    /// The reduction to a single turn happens in `f64`, because a clock that
    /// runs for days accumulates enough seconds that `f32` can no longer
    /// resolve a frame.
    pub fn angle(&self, now: Instant) -> Radians {
        let period = self.period.as_secs_f64();

        if period <= 0.0 {
            return Radians(0.0);
        }

        let elapsed = now.saturating_duration_since(self.since).as_secs_f64();
        let turns = (elapsed % period) / period;

        Radians(turns as f32 * std::f32::consts::TAU)
    }
}

impl Svg<Handle> {
    /// Creates a new [`Svg`] with the given handle.
    pub fn new(handle: impl Into<Handle>) -> Self {
        Self {
            handle: handle.into(),
            color: None,
            rotation: Radians(0.0),
            opacity: 1.0,
            border_radius: [0.0; 4],
            spin: None,
        }
    }

    /// Sets the [`Color`] filter of the [`Svg`].
    pub fn color(mut self, color: impl Into<Color>) -> Self {
        self.color = Some(color.into());
        self
    }

    /// Sets the rotation of the [`Svg`].
    pub fn rotation(mut self, rotation: impl Into<Radians>) -> Self {
        self.rotation = rotation.into();
        self
    }

    /// Sets the opacity of the [`Svg`].
    pub fn opacity(mut self, opacity: impl Into<f32>) -> Self {
        self.opacity = opacity.into();
        self
    }

    /// Turns the [`Svg`] steadily with time; see [`Spin`].
    pub fn spin(mut self, spin: impl Into<Option<Spin>>) -> Self {
        self.spin = spin.into();
        self
    }

    /// Sets the border radius of the [`Svg`]
    pub fn border_radius(mut self, border_radius: impl Into<[f32; 4]>) -> Self {
        self.border_radius = border_radius.into();
        self
    }
}

impl From<&Handle> for Svg {
    fn from(handle: &Handle) -> Self {
        Svg::new(handle.clone())
    }
}

/// A handle of Svg data.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Handle {
    id: u64,
    data: Arc<Data>,
}

impl Handle {
    /// Creates an SVG [`Handle`] pointing to the vector image of the given
    /// path.
    pub fn from_path(path: impl Into<PathBuf>) -> Handle {
        Self::from_data(Data::Path(path.into()))
    }

    /// Creates an SVG [`Handle`] from raw bytes containing either an SVG string
    /// or gzip compressed data.
    ///
    /// This is useful if you already have your SVG data in-memory, maybe
    /// because you downloaded or generated it procedurally.
    pub fn from_memory(bytes: impl Into<Cow<'static, [u8]>>) -> Handle {
        Self::from_data(Data::Bytes(bytes.into()))
    }

    fn from_data(data: Data) -> Handle {
        let mut hasher = FxHasher::default();
        data.hash(&mut hasher);

        Handle {
            id: hasher.finish(),
            data: Arc::new(data),
        }
    }

    /// Returns the unique identifier of the [`Handle`].
    pub fn id(&self) -> u64 {
        self.id
    }

    /// Returns a reference to the SVG [`Data`].
    pub fn data(&self) -> &Data {
        &self.data
    }
}

impl<T> From<T> for Handle
where
    T: Into<PathBuf>,
{
    fn from(path: T) -> Handle {
        Handle::from_path(path.into())
    }
}

impl Hash for Handle {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.id.hash(state);
    }
}

/// The data of a vectorial image.
#[derive(Clone, Hash, PartialEq, Eq)]
pub enum Data {
    /// File data
    Path(PathBuf),

    /// In-memory data
    ///
    /// Can contain an SVG string or a gzip compressed data.
    Bytes(Cow<'static, [u8]>),
}

impl std::fmt::Debug for Data {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Data::Path(path) => write!(f, "Path({path:?})"),
            Data::Bytes(_) => write!(f, "Bytes(...)"),
        }
    }
}

/// A [`Renderer`] that can render vector graphics.
///
/// [renderer]: crate::renderer
pub trait Renderer: crate::Renderer {
    /// Returns the default dimensions of an SVG for the given [`Handle`].
    fn measure_svg(&self, handle: &Handle) -> Size<u32>;

    /// Draws an SVG with the given [`Handle`], an optional [`Color`] filter, and inside the provided `bounds`.
    fn draw_svg(&mut self, svg: Svg, bounds: Rectangle, clip_bounds: Rectangle);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_spin_turns_once_per_period_from_its_instant() {
        let since = Instant::now();
        let spin = Spin {
            since,
            period: Duration::from_millis(1200),
        };
        let quarter = spin.angle(since + Duration::from_millis(300)).0;
        let wrapped = spin.angle(since + Duration::from_millis(1500)).0;

        assert_eq!(spin.angle(since).0, 0.0);
        assert!((quarter - std::f32::consts::FRAC_PI_2).abs() < 1e-4);
        assert!((wrapped - quarter).abs() < 1e-4, "a full turn wraps");
        assert_eq!(
            spin.angle(since - Duration::from_millis(10)).0,
            0.0,
            "an instant before the spin began has not turned"
        );
        assert_eq!(
            Spin { since, period: Duration::ZERO }
                .angle(since + Duration::from_secs(1))
                .0,
            0.0
        );
    }
}
