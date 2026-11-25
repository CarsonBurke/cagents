//! The main window's macOS conventions.
//!
//! The window keeps AppKit's titlebar, transparent over the client-drawn
//! header, so the traffic lights are the system's own. They show focus,
//! offer tiling on hover and sit where they do in every Mac app; the
//! interface only leaves room for them, measured here rather than assumed,
//! since their size and place change between macOS releases.
//!
//! Quit closes the window, as its close button does. One window holds the
//! whole interface, and the daemon keeps every agent and terminal running
//! without it, so closing it ends the process. That close first asks about
//! unsaved editors and drafts and stops language servers, so Quit asks the
//! same window to close. AppKit's own terminate would skip all of it. A Quit
//! the window can't take, after it has closed, ends the process at once.
//! Logging out cancels the same way, so macOS reports that cagents
//! interrupted it; once the window has closed, logging out again proceeds.
//!
//! The traffic lights keep AppKit's position at the top of the titlebar
//! rather than centering in the taller header: AppKit offers no supported
//! way to move them, and full screen hides them while the room stays kept.
use cosmic::iced::{self, runtime::window::raw_window_handle::RawWindowHandle, window};
use objc2::{
    DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send,
    rc::{Retained, Weak},
    runtime::ProtocolObject,
};
use objc2_app_kit::{
    NSApplication, NSApplicationDelegate, NSApplicationTerminateReply, NSView, NSWindow,
    NSWindowButton,
};
use objc2_foundation::{NSObject, NSObjectProtocol};

use crate::sidebar::WindowControls;

/// Routes Quit through `window` and measures its traffic lights, once iced
/// has opened it.
pub(super) fn adopt(window: window::Id) -> iced::Task<Option<WindowControls>> {
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
        quit_through(&window, main_thread);
        controls(&window, view)
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
        fn should_terminate(&self, _sender: &NSApplication) -> NSApplicationTerminateReply {
            match self.ivars().window.load() {
                Some(window) => {
                    // winit answers the close button's request with a close
                    // request event and leaves the window open; the
                    // application's close then decides whether to exit.
                    window.performClose(None);
                    NSApplicationTerminateReply::TerminateCancel
                }
                None => NSApplicationTerminateReply::TerminateNow,
            }
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

/// The traffic lights' extent from the window's top-left corner, in points,
/// with the margin AppKit gives them at the window edge kept after them too.
fn controls(window: &NSWindow, view: &NSView) -> Option<WindowControls> {
    let frame = |button| {
        let button = window.standardWindowButton(button)?;
        Some(button.convertRect_toView(button.bounds(), Some(view)))
    };
    let close = frame(NSWindowButton::CloseButton)?;
    let zoom = frame(NSWindowButton::ZoomButton)?;
    let top = if view.isFlipped() {
        close.origin.y
    } else {
        view.bounds().size.height - (close.origin.y + close.size.height)
    };
    let extent = |end: f64, margin: f64| (end + margin.max(0.0)) as f32;
    Some(WindowControls {
        width: extent(zoom.origin.x + zoom.size.width, close.origin.x),
        height: extent(top + close.size.height, top),
    })
}
