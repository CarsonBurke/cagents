//! The main window's macOS conventions.
//!
//! The window keeps AppKit's titlebar, transparent over the client-drawn
//! header, so the traffic lights are the system's own. They show focus,
//! offer tiling on hover and sit where they do in every Mac app; the
//! interface only leaves room for them, measured here rather than assumed,
//! since their size, side and place change between macOS releases, writing
//! directions and full screen, where AppKit hides them with the titlebar.
//!
//! Quit closes the window, as its close button does. One window holds the
//! whole interface, and the daemon keeps every agent and terminal running
//! without it, so closing it ends the process. That close first asks about
//! unsaved editors and drafts and stops language servers, so Quit raises the
//! window and asks it to close. AppKit's own terminate would skip all of it.
//!
//! AppKit's `TerminateLater` would let the close answer the request, but
//! AppKit then waits in its modal-panel run loop mode. winit's wake-ups
//! still run there, while no keyboard or mouse event reaches the window, so
//! nobody could answer the close's question about unsaved work. Each Quit is
//! cancelled instead and the window's close ends the process itself. Logging
//! out, restarting and shutting down are cancelled the same way, so macOS
//! reports that cagents interrupted them; trying again after the window has
//! closed proceeds. A Quit while the process is already ending, stopping its
//! language servers, is cancelled too, so it cannot cut that short. A Quit
//! in the moment before the window first opens finds no delegate yet and
//! ends the process at once.
//!
//! The traffic lights keep AppKit's position at the top of the titlebar
//! rather than centering in the taller header: AppKit offers no supported
//! way to move them.
//!
//! winit's menu bar has only the application menu. A Window menu adds
//! Minimize, Zoom and full screen with their usual key equivalents, and lets
//! AppKit list the window there. A Control-click is a secondary click, as
//! everywhere on the Mac, so context menus open from a one-button trackpad.
use std::{
    cell::Cell,
    ptr::NonNull,
    sync::atomic::{AtomicBool, AtomicU64, Ordering},
};

use cosmic::iced::{self, runtime::window::raw_window_handle::RawWindowHandle, window};
use objc2::{
    DefinedClass, MainThreadMarker, MainThreadOnly, available, define_class, msg_send,
    rc::{Retained, Weak},
    runtime::ProtocolObject,
    sel,
};
use objc2_app_kit::{
    NSApplication, NSApplicationDelegate, NSApplicationTerminateReply, NSEvent,
    NSEventModifierFlags, NSEventType, NSMenu, NSMenuItem, NSView, NSWindow, NSWindowButton,
    NSWindowStyleMask,
};
use objc2_core_graphics::{CGEvent, CGEventField, CGEventFlags, CGEventType};
use objc2_foundation::{NSObject, NSObjectProtocol, NSRect, NSString};

use crate::sidebar::{WindowControls, WindowControlsEdge};

/// Set once the process has begun to exit, and never cleared.
static EXITING: AtomicBool = AtomicBool::new(false);

/// Records that the application has begun to exit, so a further Quit waits
/// for it rather than ending the process before its cleanup finishes.
pub(super) fn exit_began() {
    EXITING.store(true, Ordering::Relaxed);
}

/// Routes Quit through `window`, adds the Window menu and secondary clicks,
/// and measures the traffic lights, once iced has opened the window.
pub(super) fn adopt(window: window::Id) -> iced::Task<Option<WindowControls>> {
    with_window(window, |window, view, main_thread| {
        quit_through(window, main_thread);
        add_window_menu(main_thread);
        secondary_click_with_control();
        controls(window, view)
    })
}

/// How long AppKit's full screen transition takes, with margin. Leaving full
/// screen reports its last size while the window still counts as full
/// screen, and nothing follows the transition's end.
const FULL_SCREEN_TRANSITION: std::time::Duration = std::time::Duration::from_millis(900);

/// Resizes seen, so only the last of a burst measures again once settled.
static RESIZES: AtomicU64 = AtomicU64::new(0);

/// Measures the traffic lights again after the window changed size, at once
/// and once more when a full screen transition would have ended.
pub(super) fn measure_after_resize(window: window::Id) -> iced::Task<Option<WindowControls>> {
    let resize = RESIZES.fetch_add(1, Ordering::Relaxed) + 1;
    let settled = iced::Task::future(async move {
        tokio::time::sleep(FULL_SCREEN_TRANSITION).await;
        RESIZES.load(Ordering::Relaxed) == resize
    })
    .then(move |latest| {
        if latest {
            measure(window)
        } else {
            iced::Task::none()
        }
    });
    iced::Task::batch([measure(window), settled])
}

fn measure(window: window::Id) -> iced::Task<Option<WindowControls>> {
    with_window(window, |window, view, _| controls(window, view))
}

fn with_window(
    window: window::Id,
    act: impl FnOnce(&NSWindow, &NSView, MainThreadMarker) -> Option<WindowControls> + Send + 'static,
) -> iced::Task<Option<WindowControls>> {
    window::run_with_handle(window, |handle| {
        let RawWindowHandle::AppKit(handle) = handle.as_raw() else {
            return None;
        };
        // iced runs window callbacks on the event loop, AppKit's main thread.
        let main_thread = MainThreadMarker::new()?;
        // SAFETY: an AppKit window handle points at the window's NSView,
        // which winit keeps alive while this callback borrows the handle.
        let view = unsafe { handle.ns_view.cast::<NSView>().as_ref() };
        let window = view.window()?;
        act(&window, view, main_thread)
    })
}

struct Ivars {
    window: Weak<NSWindow>,
}

define_class!(
    // SAFETY: NSObject has no subclassing requirements, and this class
    // implements no Drop.
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "CagentsApplicationDelegate"]
    #[ivars = Ivars]
    struct Delegate;

    // SAFETY: NSObjectProtocol has no extra requirements.
    unsafe impl NSObjectProtocol for Delegate {}

    // SAFETY: the method below has the signature the protocol declares.
    unsafe impl NSApplicationDelegate for Delegate {
        #[unsafe(method(applicationShouldTerminate:))]
        fn should_terminate(&self, application: &NSApplication) -> NSApplicationTerminateReply {
            if EXITING.load(Ordering::Relaxed) {
                return NSApplicationTerminateReply::TerminateCancel;
            }
            let Some(window) = self.ivars().window.load() else {
                return NSApplicationTerminateReply::TerminateNow;
            };
            // Whatever the close asks about unsaved work has to be seen.
            application.unhide(None);
            if window.isMiniaturized() {
                window.deminiaturize(None);
            }
            window.makeKeyAndOrderFront(None);
            if available!(macos = 14.0) {
                application.activate();
            } else {
                #[allow(deprecated)]
                application.activateIgnoringOtherApps(true);
            }
            // winit answers the close button's request with a close request
            // event and leaves the window open; the application's close then
            // decides whether to exit.
            window.performClose(None);
            NSApplicationTerminateReply::TerminateCancel
        }
    }
);

/// Makes every terminate request, from the app menu, the Dock or another
/// application, a request to close `window`. winit registers no application
/// delegate and leaves this one to applications.
fn quit_through(window: &NSWindow, main_thread: MainThreadMarker) {
    let delegate = Delegate::alloc(main_thread).set_ivars(Ivars {
        window: Weak::from(window),
    });
    // SAFETY: NSObject's init takes no arguments and returns the object.
    let delegate: Retained<Delegate> = unsafe { msg_send![super(delegate), init] };
    let application = NSApplication::sharedApplication(main_thread);
    application.setDelegate(Some(ProtocolObject::from_ref(&*delegate)));
    // The application holds its delegate weakly, and needs it until exit.
    std::mem::forget(delegate);
}

/// Adds the standard Window menu after winit's application menu. Its actions
/// travel the responder chain to the key window, and AppKit renames the full
/// screen item as the window enters and leaves full screen.
fn add_window_menu(main_thread: MainThreadMarker) {
    let application = NSApplication::sharedApplication(main_thread);
    let Some(menu_bar) = application.mainMenu() else {
        return;
    };
    let item = |title: &str, action, key: &str, modifiers| {
        // SAFETY: each action is a standard NSWindow action, taking a sender.
        let item = unsafe {
            NSMenuItem::initWithTitle_action_keyEquivalent(
                main_thread.alloc(),
                &NSString::from_str(title),
                Some(action),
                &NSString::from_str(key),
            )
        };
        item.setKeyEquivalentModifierMask(modifiers);
        item
    };
    let title = NSString::from_str("Window");
    let menu = NSMenu::initWithTitle(main_thread.alloc(), &title);
    menu.addItem(&item(
        "Minimize",
        sel!(performMiniaturize:),
        "m",
        NSEventModifierFlags::Command,
    ));
    menu.addItem(&item(
        "Zoom",
        sel!(performZoom:),
        "",
        NSEventModifierFlags::empty(),
    ));
    menu.addItem(&NSMenuItem::separatorItem(main_thread));
    menu.addItem(&item(
        "Enter Full Screen",
        sel!(toggleFullScreen:),
        "f",
        NSEventModifierFlags::Command | NSEventModifierFlags::Control,
    ));
    let entry = NSMenuItem::new(main_thread);
    entry.setTitle(&title);
    entry.setSubmenu(Some(&menu));
    menu_bar.addItem(&entry);
    application.setWindowsMenu(Some(&menu));
}

/// Delivers a Control-click, and the drag and release that follow it, as the
/// secondary button, with Control lifted, so every context menu opens from it.
fn secondary_click_with_control() {
    use objc2_app_kit::NSEventMask;
    let converting = Cell::new(false);
    let handler = block2::RcBlock::new(move |event: NonNull<NSEvent>| -> *mut NSEvent {
        // SAFETY: AppKit passes the event it is about to dispatch, valid for
        // the length of this call.
        let event = unsafe { event.as_ref() };
        let flags = event.modifierFlags();
        let unchanged = std::ptr::from_ref(event).cast_mut();
        let secondary = match event.r#type() {
            // Every press decides afresh, so a release that never reached
            // this monitor, taken by a menu's tracking loop, cannot turn the
            // next plain click into a secondary drag.
            NSEventType::LeftMouseDown => {
                let convert = flags.contains(NSEventModifierFlags::Control)
                    && !flags.contains(NSEventModifierFlags::Command);
                converting.set(convert);
                if !convert {
                    return unchanged;
                }
                CGEventType::RightMouseDown
            }
            NSEventType::LeftMouseDragged if converting.get() => CGEventType::RightMouseDragged,
            NSEventType::LeftMouseUp if converting.replace(false) => CGEventType::RightMouseUp,
            _ => return unchanged,
        };
        // AppKit takes the returned event without a reference of its own.
        as_secondary(event, secondary).map_or(unchanged, Retained::autorelease_return)
    });
    // SAFETY: the handler returns the event it was given or a new event kept
    // alive by the enclosing autorelease pool, never a dangling pointer.
    let monitor = unsafe {
        NSEvent::addLocalMonitorForEventsMatchingMask_handler(
            NSEventMask::LeftMouseDown | NSEventMask::LeftMouseDragged | NSEventMask::LeftMouseUp,
            &handler,
        )
    };
    // The monitor lasts as long as the process; removing it is never needed.
    std::mem::forget(monitor);
}

/// `event`, a primary button event, as the secondary button's `kind`, with
/// Control lifted. winit reads the button from the event's button number,
/// which AppKit takes from the Quartz event; an event AppKit creates from its
/// type alone keeps button 0. So this edits a copy of the Quartz event, which
/// also keeps its window, location, time, click count and pressure.
fn as_secondary(event: &NSEvent, kind: CGEventType) -> Option<Retained<NSEvent>> {
    let copy = CGEvent::new_copy(event.CGEvent().as_deref())?;
    let secondary = Some(&*copy);
    CGEvent::set_type(secondary, kind);
    CGEvent::set_integer_value_field(
        secondary,
        CGEventField::MouseEventButtonNumber,
        SECONDARY_BUTTON,
    );
    CGEvent::set_flags(
        secondary,
        CGEvent::flags(secondary).difference(CGEventFlags::MaskControl),
    );
    NSEvent::eventWithCGEvent(&copy)
}

/// Quartz's number for the secondary (right) mouse button.
const SECONDARY_BUTTON: i64 = 1;

/// The traffic lights' extent from the window's top corner on their side, in
/// points, with the margin AppKit gives them at the window edge kept after
/// them too. `None` in full screen, where AppKit hides them with the titlebar.
fn controls(window: &NSWindow, view: &NSView) -> Option<WindowControls> {
    if window.styleMask().contains(NSWindowStyleMask::FullScreen) {
        return None;
    }
    let frames: Vec<NSRect> = [
        NSWindowButton::CloseButton,
        NSWindowButton::MiniaturizeButton,
        NSWindowButton::ZoomButton,
    ]
    .into_iter()
    .filter_map(|button| window.standardWindowButton(button))
    .filter(|button| !button.isHidden())
    .map(|button| button.convertRect_toView(button.bounds(), Some(view)))
    .collect();
    let start = frames.iter().map(|frame| frame.origin.x).reduce(f64::min)?;
    let end = frames
        .iter()
        .map(|frame| frame.origin.x + frame.size.width)
        .reduce(f64::max)?;
    let first = frames[0];
    let bounds = view.bounds();
    let top = if view.isFlipped() {
        first.origin.y
    } else {
        bounds.size.height - (first.origin.y + first.size.height)
    };
    let extent = |reach: f64, margin: f64| (reach + margin.max(0.0)) as f32;
    // A right-to-left system puts the lights in the top-right corner.
    let (edge, width) = if start > bounds.size.width / 2.0 {
        let from_right = bounds.size.width - start;
        (
            WindowControlsEdge::Trailing,
            extent(from_right, bounds.size.width - end),
        )
    } else {
        (WindowControlsEdge::Leading, extent(end, start))
    };
    Some(WindowControls {
        width,
        height: extent(top + first.size.height, top),
        edge,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use objc2_foundation::NSPoint;

    /// winit names the button from `buttonNumber`, so a Control-click is a
    /// secondary click only if AppKit numbers the converted event so.
    #[test]
    fn a_control_click_becomes_a_secondary_press_without_control() {
        let press = NSEvent::mouseEventWithType_location_modifierFlags_timestamp_windowNumber_context_eventNumber_clickCount_pressure(
            NSEventType::LeftMouseDown,
            NSPoint::new(10.0, 20.0),
            NSEventModifierFlags::Control | NSEventModifierFlags::Shift,
            1.0,
            0,
            None,
            7,
            1,
            1.0,
        )
        .unwrap();
        assert_eq!(press.buttonNumber(), 0);
        for (quartz, kind) in [
            (CGEventType::RightMouseDown, NSEventType::RightMouseDown),
            (
                CGEventType::RightMouseDragged,
                NSEventType::RightMouseDragged,
            ),
            (CGEventType::RightMouseUp, NSEventType::RightMouseUp),
        ] {
            let secondary = as_secondary(&press, quartz).unwrap();
            assert_eq!(secondary.r#type(), kind);
            assert_eq!(secondary.buttonNumber(), 1, "{kind:?}");
            assert_eq!(
                secondary.modifierFlags() & NSEventModifierFlags::DeviceIndependentFlagsMask,
                NSEventModifierFlags::Shift
            );
            assert_eq!(secondary.locationInWindow(), NSPoint::new(10.0, 20.0));
            assert_eq!(secondary.clickCount(), 1);
            assert_eq!(secondary.timestamp(), 1.0);
        }
    }
}
