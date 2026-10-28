use std::str::FromStr;

use cosmic::iced::{keyboard, mouse};

use super::{
    BrowserInput, BrowserKeyLocation, BrowserModifiers, BrowserMouseButton, ButtonState,
    DevicePoint, EditingAction, ImeInput, TouchState, WheelUnit,
};

// Servo's paint thread treats wheel deltas as device pixels, including when
// the DOM event advertises DeltaLine. Match servoshell's line-wheel distance
// before dispatch so ordinary mouse notches do not move just one pixel.
const WHEEL_LINE_PIXELS: f64 = 76.0;

pub(crate) fn wheel_pixels(x: f64, y: f64, unit: WheelUnit) -> (f64, f64) {
    // The active winit backend already supplies physical pixel deltas. Only
    // pointer positions use the widget's logical-to-device scale conversion.
    let factor = match unit {
        WheelUnit::Lines => WHEEL_LINE_PIXELS,
        WheelUnit::Pixels => 1.0,
    };
    (x * factor, y * factor)
}

pub(crate) fn normalize_wheel(input: BrowserInput) -> BrowserInput {
    match input {
        BrowserInput::Wheel { x, y, unit, point } => {
            let (x, y) = wheel_pixels(x, y, unit);
            BrowserInput::Wheel {
                x,
                y,
                unit: WheelUnit::Pixels,
                point,
            }
        }
        input => input,
    }
}

pub(crate) fn local_device_point(
    window_x: f32,
    window_y: f32,
    bounds_x: f32,
    bounds_y: f32,
    scale_factor: f32,
) -> DevicePoint {
    let scale = if scale_factor.is_finite() {
        scale_factor.max(0.01)
    } else {
        1.0
    };
    DevicePoint {
        x: f64::from((window_x - bounds_x) * scale),
        y: f64::from((window_y - bounds_y) * scale),
    }
}

pub(crate) fn iced_mouse_button(button: mouse::Button) -> BrowserMouseButton {
    match button {
        mouse::Button::Left => BrowserMouseButton::Left,
        mouse::Button::Middle => BrowserMouseButton::Middle,
        mouse::Button::Right => BrowserMouseButton::Right,
        mouse::Button::Back => BrowserMouseButton::Back,
        mouse::Button::Forward => BrowserMouseButton::Forward,
        mouse::Button::Other(value) => BrowserMouseButton::Other(value),
    }
}

pub(crate) fn iced_modifiers(modifiers: keyboard::Modifiers) -> BrowserModifiers {
    BrowserModifiers {
        alt: modifiers.alt(),
        control: modifiers.control(),
        shift: modifiers.shift(),
        meta: modifiers.logo(),
        caps_lock: false,
        num_lock: false,
    }
}

pub(crate) fn iced_key(
    state: ButtonState,
    key: &keyboard::Key,
    physical: keyboard::key::Physical,
    location: keyboard::Location,
    modifiers: keyboard::Modifiers,
    repeat: bool,
    composing: bool,
) -> BrowserInput {
    let key = match key {
        // Iced follows native platform naming; DOM keyboard events use Meta.
        keyboard::Key::Named(keyboard::key::Named::Super) => "Meta".into(),
        keyboard::Key::Named(named) => format!("{named:?}"),
        keyboard::Key::Character(character) => character.to_string(),
        keyboard::Key::Unidentified => "Unidentified".into(),
    };
    let code = match physical {
        keyboard::key::Physical::Code(keyboard::key::Code::SuperLeft) => "MetaLeft".into(),
        keyboard::key::Physical::Code(keyboard::key::Code::SuperRight) => "MetaRight".into(),
        keyboard::key::Physical::Code(code) => format!("{code:?}"),
        keyboard::key::Physical::Unidentified(_) => "Unidentified".into(),
    };
    BrowserInput::Key {
        state,
        key,
        code,
        location: match location {
            keyboard::Location::Standard => BrowserKeyLocation::Standard,
            keyboard::Location::Left => BrowserKeyLocation::Left,
            keyboard::Location::Right => BrowserKeyLocation::Right,
            keyboard::Location::Numpad => BrowserKeyLocation::Numpad,
        },
        modifiers: iced_modifiers(modifiers),
        repeat,
        composing,
    }
}

pub(crate) fn to_servo(input: BrowserInput) -> servo::InputEvent {
    match input {
        BrowserInput::MouseMove { point } => {
            servo::InputEvent::MouseMove(servo::MouseMoveEvent::new(servo_point(point)))
        }
        BrowserInput::MouseLeave => {
            servo::InputEvent::MouseLeftViewport(servo::MouseLeftViewportEvent::default())
        }
        BrowserInput::MouseButton {
            state,
            button,
            point,
        } => servo::InputEvent::MouseButton(servo::MouseButtonEvent::new(
            match state {
                ButtonState::Pressed => servo::MouseButtonAction::Down,
                ButtonState::Released => servo::MouseButtonAction::Up,
            },
            match button {
                BrowserMouseButton::Left => servo::MouseButton::Left,
                BrowserMouseButton::Middle => servo::MouseButton::Middle,
                BrowserMouseButton::Right => servo::MouseButton::Right,
                BrowserMouseButton::Back => servo::MouseButton::Back,
                BrowserMouseButton::Forward => servo::MouseButton::Forward,
                BrowserMouseButton::Other(value) => servo::MouseButton::Other(value),
            },
            servo_point(point),
        )),
        BrowserInput::Wheel { x, y, unit, point } => {
            let (x, y) = wheel_pixels(x, y, unit);
            servo::InputEvent::Wheel(servo::WheelEvent::new(
                servo::WheelDelta {
                    // iced and Servo both define positive Y as revealing content above.
                    x,
                    y,
                    z: 0.0,
                    mode: servo::WheelMode::DeltaPixel,
                },
                servo_point(point),
            ))
        }
        BrowserInput::Touch { state, id, point } => {
            servo::InputEvent::Touch(servo::TouchEvent::new(
                match state {
                    TouchState::Down => servo::TouchEventType::Down,
                    TouchState::Move => servo::TouchEventType::Move,
                    TouchState::Up => servo::TouchEventType::Up,
                    TouchState::Cancel => servo::TouchEventType::Cancel,
                },
                servo::TouchId(id),
                servo_point(point),
            ))
        }
        BrowserInput::Key {
            state,
            key,
            code,
            location,
            modifiers,
            repeat,
            composing,
        } => {
            let mut servo_modifiers = keyboard_types::Modifiers::empty();
            servo_modifiers.set(keyboard_types::Modifiers::ALT, modifiers.alt);
            servo_modifiers.set(keyboard_types::Modifiers::CONTROL, modifiers.control);
            servo_modifiers.set(keyboard_types::Modifiers::SHIFT, modifiers.shift);
            servo_modifiers.set(keyboard_types::Modifiers::META, modifiers.meta);
            servo_modifiers.set(keyboard_types::Modifiers::CAPS_LOCK, modifiers.caps_lock);
            servo_modifiers.set(keyboard_types::Modifiers::NUM_LOCK, modifiers.num_lock);
            servo::InputEvent::Keyboard(servo::KeyboardEvent::new_without_event(
                match state {
                    ButtonState::Pressed => keyboard_types::KeyState::Down,
                    ButtonState::Released => keyboard_types::KeyState::Up,
                },
                keyboard_types::Key::from_str(&key).unwrap_or(keyboard_types::Key::Named(
                    keyboard_types::NamedKey::Unidentified,
                )),
                keyboard_types::Code::from_str(&code).unwrap_or(keyboard_types::Code::Unidentified),
                match location {
                    BrowserKeyLocation::Standard => keyboard_types::Location::Standard,
                    BrowserKeyLocation::Left => keyboard_types::Location::Left,
                    BrowserKeyLocation::Right => keyboard_types::Location::Right,
                    BrowserKeyLocation::Numpad => keyboard_types::Location::Numpad,
                },
                servo_modifiers,
                repeat,
                composing,
            ))
        }
        BrowserInput::Ime(ime) => match ime {
            ImeInput::Opened => servo::InputEvent::Ime(servo::ImeEvent::Composition(
                keyboard_types::CompositionEvent {
                    state: keyboard_types::CompositionState::Start,
                    data: String::new(),
                },
            )),
            ImeInput::Preedit { text, .. } => servo::InputEvent::Ime(servo::ImeEvent::Composition(
                keyboard_types::CompositionEvent {
                    state: keyboard_types::CompositionState::Update,
                    data: text,
                },
            )),
            ImeInput::Commit(text) => servo::InputEvent::Ime(servo::ImeEvent::Composition(
                keyboard_types::CompositionEvent {
                    state: keyboard_types::CompositionState::End,
                    data: text,
                },
            )),
            ImeInput::Closed => servo::InputEvent::Ime(servo::ImeEvent::Dismissed),
        },
        BrowserInput::Editing(action) => servo::InputEvent::EditingAction(match action {
            EditingAction::Copy => servo::EditingActionEvent::Copy,
            EditingAction::Cut => servo::EditingActionEvent::Cut,
            EditingAction::Paste => servo::EditingActionEvent::Paste,
        }),
    }
}

fn servo_point(point: DevicePoint) -> servo::WebViewPoint {
    servo::WebViewPoint::Device(servo::DevicePoint::new(point.x as f32, point.y as f32))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn coordinates_are_widget_local_and_scaled() {
        assert_eq!(
            local_device_point(25.0, 50.0, 5.0, 10.0, 1.5),
            DevicePoint { x: 30.0, y: 60.0 }
        );
    }

    #[test]
    fn line_wheels_are_device_pixel_events_at_servoshell_distance() {
        let mapped = to_servo(BrowserInput::Wheel {
            x: 0.5,
            y: -1.0,
            unit: WheelUnit::Lines,
            point: DevicePoint { x: 2.0, y: 4.0 },
        });
        match mapped {
            servo::InputEvent::Wheel(event) => {
                assert_eq!((event.delta.x, event.delta.y), (38.0, -76.0));
                assert_eq!(event.delta.mode, servo::WheelMode::DeltaPixel);
            }
            _ => panic!("wrong Servo event"),
        }
        assert_eq!(wheel_pixels(0.5, -1.0, WheelUnit::Pixels), (0.5, -1.0));
    }

    #[test]
    fn wheel_sign_is_preserved() {
        let mapped = to_servo(BrowserInput::Wheel {
            x: -3.0,
            y: 7.0,
            unit: WheelUnit::Pixels,
            point: DevicePoint { x: 2.0, y: 4.0 },
        });
        match mapped {
            servo::InputEvent::Wheel(event) => {
                assert_eq!(event.delta.x, -3.0);
                assert_eq!(event.delta.y, 7.0);
                assert_eq!(event.delta.mode, servo::WheelMode::DeltaPixel);
            }
            _ => panic!("wrong Servo event"),
        }
    }

    #[test]
    fn native_super_keys_keep_dom_meta_identity_on_press_and_release() {
        use keyboard::key::{Code, Named, Physical};
        for (code, location, expected_code, expected_location) in [
            (
                Code::SuperLeft,
                keyboard::Location::Left,
                keyboard_types::Code::MetaLeft,
                keyboard_types::Location::Left,
            ),
            (
                Code::SuperRight,
                keyboard::Location::Right,
                keyboard_types::Code::MetaRight,
                keyboard_types::Location::Right,
            ),
        ] {
            for (state, modifiers, expected_state) in [
                (
                    ButtonState::Pressed,
                    keyboard::Modifiers::LOGO,
                    keyboard_types::KeyState::Down,
                ),
                (
                    ButtonState::Released,
                    keyboard::Modifiers::empty(),
                    keyboard_types::KeyState::Up,
                ),
            ] {
                let mapped = to_servo(iced_key(
                    state,
                    &keyboard::Key::Named(Named::Super),
                    Physical::Code(code),
                    location,
                    modifiers,
                    false,
                    false,
                ));
                let servo::InputEvent::Keyboard(event) = mapped else {
                    panic!("keyboard event")
                };
                assert_eq!(
                    event.event.key,
                    keyboard_types::Key::Named(keyboard_types::NamedKey::Meta)
                );
                assert_eq!(event.event.code, expected_code);
                assert_eq!(event.event.location, expected_location);
                assert_eq!(event.event.state, expected_state);
                assert_eq!(
                    event
                        .event
                        .modifiers
                        .contains(keyboard_types::Modifiers::META),
                    state == ButtonState::Pressed
                );
            }
        }
    }

    #[test]
    fn keyboard_identity_and_modifiers_survive_mapping() {
        let mapped = to_servo(BrowserInput::Key {
            state: ButtonState::Pressed,
            key: "a".into(),
            code: "KeyA".into(),
            location: BrowserKeyLocation::Standard,
            modifiers: BrowserModifiers {
                control: true,
                shift: true,
                ..BrowserModifiers::default()
            },
            repeat: true,
            composing: false,
        });
        match mapped {
            servo::InputEvent::Keyboard(event) => {
                assert_eq!(event.event.key, keyboard_types::Key::Character("a".into()));
                assert_eq!(event.event.code, keyboard_types::Code::KeyA);
                assert!(
                    event
                        .event
                        .modifiers
                        .contains(keyboard_types::Modifiers::CONTROL)
                );
                assert!(event.event.repeat);
            }
            _ => panic!("wrong Servo event"),
        }
    }
}
