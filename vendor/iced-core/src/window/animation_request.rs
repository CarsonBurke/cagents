use crate::time::Duration;

/// A request to keep presenting a window's last drawing while its renderer
/// animates it.
///
/// Some drawings change with time alone, such as an [`Svg`] that spins: the
/// renderer works out their appearance from the clock whenever it presents a
/// frame. A widget that draws one asks for frames with this request instead of
/// a [`RedrawRequest`], and the shell then presents the window's retained
/// drawing again at the requested cadence, without updating or drawing its
/// widgets. Any other reason to redraw, such as an event, a message or a
/// widget's [`RedrawRequest`], still draws the whole window, and the widgets
/// renew their requests on that frame's [`Event::RedrawRequested`].
///
/// Requests combine to the most frequent one.
///
/// [`Svg`]: crate::Svg
/// [`RedrawRequest`]: super::RedrawRequest
/// [`Event::RedrawRequested`]: super::Event::RedrawRequested
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub enum AnimationRequest {
    /// Present a frame for every frame of the display.
    EveryFrame,

    /// Present a frame once per given interval.
    Every(Duration),

    /// Nothing animates.
    #[default]
    None,
}

impl AnimationRequest {
    /// Returns whether a drawing animates at all.
    pub fn is_animating(self) -> bool {
        self != Self::None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_combine_to_the_most_frequent() {
        let fast = AnimationRequest::Every(Duration::from_millis(100));
        let slow = AnimationRequest::Every(Duration::from_millis(500));

        assert_eq!(fast.min(slow), fast);
        assert_eq!(slow.min(AnimationRequest::EveryFrame), AnimationRequest::EveryFrame);
        assert_eq!(AnimationRequest::None.min(slow), slow);
        assert!(!AnimationRequest::default().is_animating());
    }
}
