//! A terminal's screen as bytes that draw it again on a new terminal, so a
//! successor daemon shows exactly what its predecessor did.

use libghostty_vt::{
    Terminal,
    fmt::{Format, Formatter, FormatterOptions},
    screen::Screen,
    selection::Selection,
    terminal::{Mode, Point, PointCoordinate},
};

/// Draws `terminal`'s screen again on a new terminal of the same size: its
/// scrollback, as much of it as fits in `budget` bytes, then its active area
/// with the cursor, modes and working directory, then its alternate screen if
/// that is showing, then its title.
///
/// Leaves `terminal` on its primary screen: this is its last use.
pub(super) fn screen(terminal: &mut Terminal<'_, '_>, budget: usize) -> Result<Vec<u8>, String> {
    let failed = |error: libghostty_vt::Error| format!("format terminal screen: {error}");
    let alternate = match terminal.active_screen().map_err(failed)? {
        Screen::Primary => None,
        Screen::Alternate => {
            // Entered the way the program entered it, which on the new
            // terminal saves the primary screen's cursor for the program's
            // exit to restore, and leaves the cursor where the alternate
            // screen's content starts. Entering it again is then a no-op.
            let mode = if terminal.mode(Mode::ALT_SCREEN_SAVE).map_err(failed)? {
                1049
            } else if terminal.mode(Mode::ALT_SCREEN).map_err(failed)? {
                1047
            } else {
                47
            };
            let mut alternate = format!("\x1b[?{mode}h").into_bytes();
            alternate.extend_from_slice(HOME);
            alternate.extend_from_slice(&active(terminal).map_err(failed)?);
            terminal.vt_write(format!("\x1b[?{mode}l").as_bytes());
            Some(alternate)
        }
    };
    let primary = active(terminal).map_err(failed)?;
    let title = title(terminal);
    let drawn = primary.len() + alternate.as_ref().map_or(0, Vec::len) + title.len();
    let rows = terminal.rows().map_err(failed)?;
    // Scrolling the history out of the active area costs a line feed a row.
    let padding = RESET.len() + 2 * usize::from(rows) + HOME.len();
    let mut screen = history(terminal, budget.saturating_sub(drawn + padding)).map_err(failed)?;
    if !screen.is_empty() {
        // Line feeds that scroll the history into scrollback, onto rows
        // that take no colour from the history's last style.
        screen.extend_from_slice(RESET);
        for _ in 0..rows {
            screen.extend_from_slice(b"\r\n");
        }
        screen.extend_from_slice(HOME);
    }
    screen.extend_from_slice(&primary);
    if let Some(alternate) = alternate {
        screen.extend_from_slice(&alternate);
    }
    screen.extend_from_slice(&title);
    if screen.len() > budget {
        return Err("the terminal's screen is too large to carry".into());
    }
    Ok(screen)
}

const RESET: &[u8] = b"\x1b[0m";
const HOME: &[u8] = b"\x1b[H";

/// The active area with everything that is not content: modes, scrolling
/// region, tab stops, keyboard protocols, working directory, cursor style
/// and position. Drawn from the top left.
fn active(terminal: &Terminal<'_, '_>) -> Result<Vec<u8>, libghostty_vt::Error> {
    let last = PointCoordinate {
        x: terminal.cols()?.saturating_sub(1),
        y: u32::from(terminal.rows()?.saturating_sub(1)),
    };
    let start = terminal.grid_ref(Point::Active(PointCoordinate { x: 0, y: 0 }))?;
    let end = terminal.grid_ref(Point::Active(last))?;
    let selection = Selection::new(start, end, false);
    let options = FormatterOptions::new()
        .with_format(Format::Vt)
        .with_selection(&selection)
        .with_style(true)
        .with_hyperlink(true)
        .with_protection(true)
        .with_charsets(true)
        .with_modes(true)
        .with_keyboard(true)
        .with_kitty_keyboard(true)
        .with_scrolling_region(true)
        .with_tabstops(true)
        .with_pwd(true)
        .with_cursor(true);
    let mut active = Formatter::new(terminal, options)?
        .format_alloc(None)?
        .to_vec();
    // The formatter places the cursor before it sets the scrolling region
    // and tab stops, both of which move it. With origin mode on, a position
    // would be read within the scrolling region; the formatter's is kept.
    if !terminal.mode(Mode::ORIGIN)? {
        let (x, y) = (terminal.cursor_x()?, terminal.cursor_y()?);
        active.extend_from_slice(format!("\x1b[{};{}H", y + 1, x + 1).as_bytes());
    }
    Ok(active)
}

/// The newest scrollback that fits in `budget` bytes, without a final line
/// break. Each halving of the rows kept costs one more format, and only a
/// history that does not fit is formatted twice.
fn history(terminal: &Terminal<'_, '_>, budget: usize) -> Result<Vec<u8>, libghostty_vt::Error> {
    let total = terminal.scrollback_rows()?;
    let last = terminal.cols()?.saturating_sub(1);
    let mut kept = total;
    while kept > 0 {
        let first = u32::try_from(total - kept).unwrap_or(u32::MAX);
        let end = u32::try_from(total - 1).unwrap_or(u32::MAX);
        let start = terminal.grid_ref(Point::History(PointCoordinate { x: 0, y: first }))?;
        let end = terminal.grid_ref(Point::History(PointCoordinate { x: last, y: end }))?;
        let selection = Selection::new(start, end, false);
        let options = FormatterOptions::new()
            .with_format(Format::Vt)
            .with_selection(&selection)
            .with_style(true)
            .with_hyperlink(true)
            .with_protection(true);
        let history = Formatter::new(terminal, options)?.format_alloc(None)?;
        if history.len() <= budget {
            return Ok(history.to_vec());
        }
        kept /= 2;
    }
    Ok(Vec::new())
}

/// The title as the sequence that sets it, when there is one. Control
/// characters could end or extend the sequence, so they are left out.
fn title(terminal: &Terminal<'_, '_>) -> Vec<u8> {
    let title: String = terminal
        .title()
        .unwrap_or_default()
        .chars()
        .filter(|character| !character.is_control())
        .collect();
    if title.is_empty() {
        return Vec::new();
    }
    [b"\x1b]2;", title.as_bytes(), b"\x1b\\"].concat()
}

#[cfg(test)]
mod tests {
    use super::*;
    use libghostty_vt::TerminalOptions;

    const COLS: u16 = 20;
    const ROWS: u16 = 4;

    fn terminal() -> Terminal<'static, 'static> {
        Terminal::new(TerminalOptions {
            cols: COLS,
            rows: ROWS,
            max_scrollback: crate::terminal::TerminalConfig::default().max_scrollback_bytes,
        })
        .unwrap()
    }

    /// Everything a person could see of a terminal, and where they would type.
    #[derive(Debug, PartialEq, Eq)]
    struct Seen {
        history: Vec<String>,
        active: Vec<String>,
        cursor: (u16, u16),
        screen: Screen,
        title: String,
        pwd: String,
        /// How keys and pastes reach the program.
        modes: Vec<bool>,
        keyboard: String,
    }

    fn row(terminal: &Terminal<'_, '_>, point: impl Fn(u16) -> Point) -> String {
        let mut text = String::new();
        let mut buffer = ['\0'; 16];
        for x in 0..COLS {
            let cell = terminal.grid_ref(point(x)).unwrap();
            match cell.graphemes(&mut buffer).unwrap() {
                0 => text.push(' '),
                count => text.extend(&buffer[..count]),
            }
        }
        text.trim_end().to_owned()
    }

    fn seen(terminal: &Terminal<'_, '_>) -> Seen {
        let history = (0..terminal.scrollback_rows().unwrap() as u32)
            .map(|y| row(terminal, |x| Point::History(PointCoordinate { x, y })))
            .collect();
        let active = (0..u32::from(ROWS))
            .map(|y| row(terminal, |x| Point::Active(PointCoordinate { x, y })))
            .collect();
        Seen {
            history,
            active,
            cursor: (terminal.cursor_x().unwrap(), terminal.cursor_y().unwrap()),
            screen: terminal.active_screen().unwrap(),
            title: terminal.title().unwrap().to_owned(),
            pwd: terminal.pwd().unwrap().to_owned(),
            modes: [
                Mode::BRACKETED_PASTE,
                Mode::DECCKM,
                Mode::CURSOR_VISIBLE,
                Mode::FOCUS_EVENT,
                Mode::NORMAL_MOUSE,
                Mode::SGR_MOUSE,
                Mode::ORIGIN,
            ]
            .into_iter()
            .map(|mode| terminal.mode(mode).unwrap())
            .collect(),
            keyboard: format!("{:?}", terminal.kitty_keyboard_flags().unwrap()),
        }
    }

    /// Carries a terminal that was sent `output`, and returns what each side
    /// shows, the successor's after the same `after`.
    fn carried(output: &[u8], after: &[u8]) -> (Seen, Seen, Vec<u8>) {
        let mut original = terminal();
        original.vt_write(output);
        let expected = {
            let mut reference = terminal();
            reference.vt_write(output);
            reference.vt_write(after);
            seen(&reference)
        };
        let screen = screen(&mut original, 1 << 20).unwrap();
        let mut successor = terminal();
        successor.vt_write(&screen);
        successor.vt_write(after);
        (expected, seen(&successor), screen)
    }

    fn assert_carried(output: &[u8], after: &[u8]) {
        let (expected, actual, screen) = carried(output, after);
        assert_eq!(expected, actual, "{:?}", String::from_utf8_lossy(&screen));
    }

    #[test]
    fn a_primary_screen_keeps_its_history_apart_from_what_is_showing() {
        assert_carried(
            b"\x1b[31mone\x1b[0m\r\ntwo\r\nthree\r\nfour\r\nfive\r\n$ ",
            b"typed",
        );
        // Cleared: the history stays history, not the top of the screen.
        assert_carried(
            b"one\r\ntwo\r\nthree\r\nfour\r\nfive\r\n\x1b[H\x1b[2J$ ",
            b"typed",
        );
        assert_carried(b"$ ", b"typed");
        assert_carried(b"", b"");
        // Blank rows above the prompt, and a line long enough to wrap.
        assert_carried(
            b"one\r\ntwo\r\nthree\r\nfour\r\n\r\n\r\n$ 0123456789abcdefghijklmnopqrstuvwxyz",
            b"!",
        );
    }

    #[test]
    fn modes_scrolling_regions_and_the_working_directory_carry_over() {
        assert_carried(
            b"\x1b]7;file://host/tmp/here\x07\x1b[?2004h\x1b[?1h\x1b[2;3r\x1b[3;1Hin region",
            b"\r\nnext\r\nand next",
        );
        // Mouse and focus reporting, and the kitty keyboard protocol, as a
        // full-screen agent leaves them.
        assert_carried(
            b"\x1b[?1049h\x1b[?1000h\x1b[?1006h\x1b[?1004h\x1b[?25l\x1b[>5u\x1b[3;4Hagent",
            b"",
        );
        // A custom tab stop still stops a tab, and the cursor stays put.
        assert_carried(b"\x1b[3g\x1b[5G\x1bH\r$ ", b"\tafter tab");
    }

    #[test]
    fn an_alternate_screen_returns_to_the_primary_screen_it_covered() {
        let output = b"one\r\ntwo\r\nthree\r\nfour\r\nfive\r\n$ vim\x1b[?1049h\x1b[2;3Hediting";
        assert_carried(output, b"");
        // Quitting shows the primary screen, cursor and all, as it was.
        assert_carried(output, b"\x1b[?1049l typed");
        assert_carried(b"$ less\x1b[?47h\x1b[Hpage", b"\x1b[?47l");
    }

    #[test]
    fn the_title_carries_over_without_its_control_characters() {
        assert_carried(b"\x1b]2;working\x07$ ", b"");
        let mut original = terminal();
        original.vt_write(b"\x1b]2;a\x1b\\b\x07");
        let screen = screen(&mut original, 1 << 20).unwrap();
        assert!(!screen.contains(&0x07), "{screen:?}");
    }

    #[test]
    fn history_past_the_budget_keeps_its_newest_rows() {
        let mut output = Vec::new();
        for line in 0..1000 {
            output.extend_from_slice(format!("line {line}\r\n").as_bytes());
        }
        let mut original = terminal();
        original.vt_write(&output);
        let screen = screen(&mut original, 4096).unwrap();
        assert!(screen.len() <= 4096);
        let mut successor = terminal();
        successor.vt_write(&screen);
        let seen = seen(&successor);
        assert!(!seen.history.is_empty());
        assert!(seen.history.len() < 1000);
        assert_eq!(seen.history.last().unwrap(), "line 996");
        assert_eq!(seen.active, seen_of(&output).active);
        // A budget too small even for the screen itself refuses.
        assert!(screen_of(&output, 8).is_err());
    }

    fn seen_of(output: &[u8]) -> Seen {
        let mut terminal = terminal();
        terminal.vt_write(output);
        seen(&terminal)
    }

    fn screen_of(output: &[u8], budget: usize) -> Result<Vec<u8>, String> {
        let mut terminal = terminal();
        terminal.vt_write(output);
        screen(&mut terminal, budget)
    }
}
