use cctk::sctk::{
    activation::{ActivationHandler, RequestData, RequestDataExt},
    delegate_activation,
    reexports::client::protocol::{wl_seat::WlSeat, wl_surface::WlSurface},
};
use std::time::Instant;

use crate::platform_specific::wayland::event_loop::state::SctkState;

#[derive(Debug, Clone, Copy)]
pub(crate) enum ActivationInputKind {
    Keyboard,
    Pointer,
}

/// Preserve the serial's originating surface and local receipt order. Cached
/// compositor timestamps can wrap while an inactive seat keeps old input.
#[derive(Debug)]
pub(crate) struct ActivationInput {
    pub(crate) received_at: Instant,
    pub(crate) surface: WlSurface,
    pub(crate) serial: u32,
    pub(crate) kind: ActivationInputKind,
}

pub(crate) fn newest_activation_input<T>(
    inputs: impl IntoIterator<Item = (Instant, T)>,
) -> Option<T> {
    inputs
        .into_iter()
        .max_by_key(|(received_at, _)| *received_at)
        .map(|(_, input)| input)
}

pub struct IcedRequestData {
    id: u32,
    data: RequestData,
}

impl IcedRequestData {
    pub fn new(data: RequestData, id: u32) -> IcedRequestData {
        IcedRequestData { data, id }
    }
}

impl RequestDataExt for IcedRequestData {
    fn app_id(&self) -> Option<&str> {
        self.data.app_id()
    }

    fn seat_and_serial(&self) -> Option<(&WlSeat, u32)> {
        self.data.seat_and_serial()
    }

    fn surface(&self) -> Option<&WlSurface> {
        self.data.surface()
    }
}

impl ActivationHandler for SctkState {
    type RequestData = IcedRequestData;

    fn new_token(&mut self, token: String, data: &Self::RequestData) {
        if let Some(tx) = self.token_senders.remove(&data.id) {
            _ = tx.send(Some(token));
        } else {
            log::error!("Missing activation request Id.");
        }
    }
}

delegate_activation!(SctkState, IcedRequestData);

#[cfg(test)]
mod tests {
    use super::newest_activation_input;
    use std::time::{Duration, Instant};

    #[test]
    fn activation_uses_a_mouse_press_without_keyboard_history() {
        assert_eq!(newest_activation_input([(Instant::now(), 17)]), Some(17));
        assert_eq!(newest_activation_input::<u32>([]), None);
    }

    #[test]
    fn activation_uses_the_latest_focused_input_across_devices_and_seats() {
        let first = Instant::now();
        let second = first + Duration::from_millis(1);
        let third = second + Duration::from_millis(1);
        assert_eq!(
            newest_activation_input([(first, 11), (second, 12)]),
            Some(12)
        );
        assert_eq!(
            newest_activation_input([(second, 11), (first, 12)]),
            Some(11)
        );
        assert_eq!(
            newest_activation_input([(first, 11), (second, 12), (third, 21)]),
            Some(21)
        );
    }

    #[test]
    fn stale_keyboard_history_cannot_override_a_fresh_pointer_serial() {
        let keyboard_received = Instant::now();
        let pointer_received =
            keyboard_received + Duration::from_secs(25 * 24 * 60 * 60);
        // Receipt ordering stays unambiguous beyond half of the compositor's
        // timestamp range and keeps the chosen serial unchanged, even at wrap.
        assert_eq!(
            newest_activation_input([
                (keyboard_received, u32::MAX),
                (pointer_received, 3)
            ]),
            Some(3)
        );
        assert_eq!(
            newest_activation_input([
                (pointer_received, 3),
                (keyboard_received, u32::MAX)
            ]),
            Some(3)
        );
    }
}
