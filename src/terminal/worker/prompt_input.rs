//! Protect an unfinished command using the emulator's shell integration
//! metadata, rather than guessing editing state from raw terminal keys.
use libghostty_vt::{
    Terminal,
    screen::{CellSemanticContent, RowSemanticPrompt, Screen},
    terminal::{Point, PointCoordinate},
};

/// Query the active screen, independently of the user's scrollback position.
/// A primary prompt anchors the current command; continuations and cells to
/// the right of a cursor moved back into the line must also be inspected.
pub(super) fn has_prompt_input(
    terminal: &Terminal<'_, '_>,
) -> Result<Option<bool>, libghostty_vt::Error> {
    if terminal.active_screen()? == Screen::Alternate {
        return Ok(None);
    }
    let columns = terminal.cols()?;
    let rows = terminal.rows()?;
    let cursor_y = terminal.cursor_y()?;
    let mut start = None;
    for y in (0..=cursor_y).rev() {
        let reference = terminal.grid_ref(Point::Active(PointCoordinate {
            x: 0,
            y: u32::from(y),
        }))?;
        if reference.row()?.semantic_prompt()? == RowSemanticPrompt::Prompt {
            start = Some(y);
            break;
        }
    }
    // A wrapped command can scroll its primary prompt out of the active
    // screen. Input semantics on its remaining cells still protect it.
    let integrated = start.is_some();
    let start = start.unwrap_or(0);
    for y in start..rows {
        let reference = terminal.grid_ref(Point::Active(PointCoordinate {
            x: 0,
            y: u32::from(y),
        }))?;
        if y > start && reference.row()?.semantic_prompt()? == RowSemanticPrompt::Prompt {
            break;
        }
        for x in 0..columns {
            let cell = terminal
                .grid_ref(Point::Active(PointCoordinate { x, y: u32::from(y) }))?
                .cell()?;
            if cell.semantic_content()? == CellSemanticContent::Input && cell.has_text()? {
                return Ok(Some(true));
            }
        }
    }
    Ok(integrated.then_some(false))
}

/// Runs after fish's configuration, preserving the user's prompt functions
/// and status. Nothing is installed in HOME or persisted into launch recipes.
pub(super) const FISH_INTEGRATION: &str = include_str!("prompt_input.fish");

#[cfg(test)]
mod tests {
    use super::*;
    use libghostty_vt::TerminalOptions;

    #[test]
    fn fish_markers_preserve_both_prompts_pipeline_status_and_exact_output() {
        let script = format!(
            r#"
function fish_prompt
    printf 'status=%s,pipeline=%s\n\n' $status "$pipestatus"
    set -g __cagents_probe_state yes
    return 7
end
function fish_right_prompt
    printf 'right=%s,pipeline=%s' $status "$pipestatus"
    return 9
end
{FISH_INTEGRATION}
false | true
fish_prompt
printf 'left-return=%s,state=%s\n' $status "$__cagents_probe_state"
false | true
fish_right_prompt
printf 'right-return=%s\n' $status
"#
        );
        let result = match std::process::Command::new("fish")
            .args(["--no-config", "-c", &script])
            .output()
        {
            Ok(output) => output,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return,
            Err(error) => panic!("Run fish prompt fixture: {error}"),
        };
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        assert_eq!(result.stdout, b"\x1b]133;A\x07status=0,pipeline=1 0\n\n\x1b]133;B\x07left-return=7,state=yes\n\x1b]133;P;k=r\x07right=0,pipeline=1 0\x1b]133;B\x07right-return=9\n");
    }

    #[test]
    fn right_prompt_markers_keep_the_cursor_on_the_prompt_row() {
        let mut terminal = Terminal::new(TerminalOptions {
            cols: 40,
            rows: 6,
            max_scrollback: 100,
        })
        .unwrap();
        terminal.vt_write(b"\x1b]133;A\x07> \x1b]133;B\x07");
        let prompt_row = terminal.cursor_y().unwrap();
        terminal.vt_write(b"\x1b[30G\x1b]133;P;k=r\x07right\x1b]133;B\x07");
        assert_eq!(terminal.cursor_y().unwrap(), prompt_row);
        assert_eq!(terminal.cursor_x().unwrap(), 34);
        assert_eq!(has_prompt_input(&terminal).unwrap(), Some(false));
        terminal.vt_write(b"\x1b[3Gunfinished");
        assert_eq!(has_prompt_input(&terminal).unwrap(), Some(true));
        terminal.vt_write(b"\r\x1b[2K\x1b]133;A\x07> \x1b]133;B\x07\x1b[30G\x1b]133;P;k=r\x07right\x1b]133;B\x07\x1b[3G");
        assert_eq!(terminal.cursor_y().unwrap(), prompt_row);
        assert_eq!(has_prompt_input(&terminal).unwrap(), Some(false));
    }

    #[test]
    fn a_screen_without_prompt_markers_does_not_prove_empty_input() {
        let mut terminal = Terminal::new(TerminalOptions {
            cols: 40,
            rows: 6,
            max_scrollback: 100,
        })
        .unwrap();
        terminal.vt_write(b"prompt> ");
        assert_eq!(has_prompt_input(&terminal).unwrap(), None);
        terminal.vt_write(b"unfinished");
        assert_eq!(has_prompt_input(&terminal).unwrap(), None);
    }

    #[test]
    fn a_command_taller_than_the_screen_keeps_input_protection() {
        let mut terminal = Terminal::new(TerminalOptions {
            cols: 8,
            rows: 3,
            max_scrollback: 100,
        })
        .unwrap();
        terminal.vt_write(b"\x1b]133;A\x07> \x1b]133;B\x07a long wrapped unsent command that scrolls the prompt away");
        assert_eq!(has_prompt_input(&terminal).unwrap(), Some(true));
    }

    #[test]
    fn semantic_prompt_protects_input_and_releases_a_cleared_line() {
        let mut terminal = Terminal::new(TerminalOptions {
            cols: 40,
            rows: 6,
            max_scrollback: 100,
        })
        .unwrap();
        terminal.vt_write(b"\x1b]133;A\x07prompt> \x1b]133;B\x07");
        assert_eq!(has_prompt_input(&terminal).unwrap(), Some(false));
        terminal.vt_write(b"echo valuable\x1b[5D");
        assert_eq!(has_prompt_input(&terminal).unwrap(), Some(true));
        terminal.vt_write(b"\r\x1b[2K\x1b]133;A\x07prompt> \x1b]133;B\x07");
        assert_eq!(has_prompt_input(&terminal).unwrap(), Some(false));
    }

    #[test]
    fn semantic_prompt_handles_wrapping_and_ignores_an_old_command() {
        let mut terminal = Terminal::new(TerminalOptions {
            cols: 12,
            rows: 8,
            max_scrollback: 100,
        })
        .unwrap();
        terminal.vt_write(b"\x1b]133;A\x07> \x1b]133;B\x07long wrapped command");
        assert_eq!(has_prompt_input(&terminal).unwrap(), Some(true));
        terminal.vt_write(
            b"\x1b]133;C\x07\r\noutput\r\n\x1b]133;D;0\x07\x1b]133;A\x07> \x1b]133;B\x07",
        );
        assert_eq!(has_prompt_input(&terminal).unwrap(), Some(false));
    }
}
