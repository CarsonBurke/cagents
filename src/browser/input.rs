use std::str::FromStr;

use cosmic::iced::{keyboard, mouse};

use super::{
    BrowserInput, BrowserKeyLocation, BrowserModifiers, BrowserMouseButton, ButtonState,
    DevicePoint, EditingAction, ImeInput, TouchState, WheelUnit,
};

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
        keyboard::Key::Named(named) => format!("{named:?}"),
        keyboard::Key::Character(character) => character.to_string(),
        keyboard::Key::Unidentified => "Unidentified".into(),
    };
    let code = match physical {
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
            servo::InputEvent::Wheel(servo::WheelEvent::new(
                servo::WheelDelta {
                    // iced and Servo both define positive Y as revealing content above.
                    x,
                    y,
                    z: 0.0,
                    mode: match unit {
                        WheelUnit::Pixels => servo::WheelMode::DeltaPixel,
                        WheelUnit::Lines => servo::WheelMode::DeltaLine,
                    },
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
