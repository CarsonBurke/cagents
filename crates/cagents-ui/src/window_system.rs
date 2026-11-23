//! Explains how the window system ended a window that was already running.

use cosmic::iced;
use iced_winit::winit::error::EventLoopError;

/// Why the event loop stopped a window that had opened, as a clause a person
/// can read after "stopped: ". Anything other than an event loop error keeps
/// its own message.
pub fn stopped_reason(error: &(dyn std::error::Error + 'static)) -> String {
    let Some(iced::Error::EventLoop(inner)) = error.downcast_ref::<iced::Error>() else {
        return error.to_string();
    };
    match inner.downcast_ref::<EventLoopError>() {
        // The application asks winit to exit with status 0, so a failing
        // status comes from the backend: it exits when the display connection
        // fails, with the failed call's errno when it has one and 1 otherwise.
        Some(EventLoopError::ExitFailure(code)) => {
            let lost = "the connection to the display server was lost";
            if *code > 1 {
                format!("{lost} ({})", std::io::Error::from_raw_os_error(*code))
            } else {
                lost.to_owned()
            }
        }
        _ => format!("the window system's event loop failed: {inner}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event_loop_error(error: EventLoopError) -> iced::Error {
        iced::Error::EventLoop(Box::new(error))
    }

    #[test]
    fn a_closed_display_connection_reads_as_lost_with_its_cause() {
        let reason = stopped_reason(&event_loop_error(EventLoopError::ExitFailure(libc::EPIPE)));
        assert_eq!(
            reason,
            format!(
                "the connection to the display server was lost ({})",
                std::io::Error::from_raw_os_error(libc::EPIPE)
            )
        );
    }

    #[test]
    fn a_failure_without_an_errno_does_not_invent_one() {
        let reason = stopped_reason(&event_loop_error(EventLoopError::ExitFailure(1)));
        assert_eq!(reason, "the connection to the display server was lost");
    }

    #[test]
    fn other_event_loop_errors_keep_their_detail() {
        let reason = stopped_reason(&event_loop_error(EventLoopError::RecreationAttempt));
        assert!(
            reason
                .starts_with("the window system's event loop failed: EventLoop can't be recreated"),
            "{reason}"
        );
    }

    #[test]
    fn errors_outside_the_event_loop_keep_their_message() {
        let error = std::io::Error::other("the daemon refused the window");
        assert_eq!(stopped_reason(&error), "the daemon refused the window");
    }
}
