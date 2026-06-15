//! Insert finalized history lines into terminal scrollback above the viewport.
//!
//! Uses ANSI scroll-region sequences so that only the area above the viewport
//! scrolls, keeping the live viewport in place.

use std::fmt;
use std::io;
use std::io::Write;
use std::ops::Range;

use crossterm::Command;
use crossterm::cursor::MoveTo;
use crossterm::queue;
use crossterm::style::Colors;
use crossterm::style::Print;
use crossterm::style::SetAttribute;
use crossterm::style::SetBackgroundColor;
use crossterm::style::SetColors;
use crossterm::style::SetForegroundColor;
use crossterm::terminal::Clear;
use crossterm::terminal::ClearType;
use ratatui::backend::Backend;
use ratatui::style::Color;

use crate::conversation::{HistoryLine, LineKind};
use crate::custom_terminal::Terminal;

// ---------------------------------------------------------------------------
// Scroll-region escape helpers
// ---------------------------------------------------------------------------

/// `ESC [ <top> ; <bottom> r` — set the scrolling region (1-indexed).
pub(crate) struct ScrollRegionCmd(pub(crate) Range<u16>);

impl Command for ScrollRegionCmd {
    fn write_ansi(&self, f: &mut impl fmt::Write) -> fmt::Result {
        write!(f, "\x1b[{};{}r", self.0.start, self.0.end)
    }

    #[cfg(windows)]
    fn execute_winapi(&self) -> io::Result<()> {
        Err(io::Error::other(
            "SetScrollRegion is only available via ANSI",
        ))
    }

    #[cfg(windows)]
    fn is_ansi_code_supported(&self) -> bool {
        true
    }
}

/// `ESC [ r` — reset the scrolling region to the full screen.
pub(crate) struct ResetScrollRegionCmd;

impl Command for ResetScrollRegionCmd {
    fn write_ansi(&self, f: &mut impl fmt::Write) -> fmt::Result {
        write!(f, "\x1b[r")
    }

    #[cfg(windows)]
    fn execute_winapi(&self) -> io::Result<()> {
        Err(io::Error::other(
            "ResetScrollRegion is only available via ANSI",
        ))
    }

    #[cfg(windows)]
    fn is_ansi_code_supported(&self) -> bool {
        true
    }
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Insert `lines` into terminal scrollback immediately above the viewport.
///
/// The viewport and everything below it stay in place; only the region above
/// scrolls.
pub(crate) fn insert_history_lines<B>(
    terminal: &mut Terminal<B>,
    lines: &[HistoryLine],
) -> io::Result<()>
where
    B: Backend + Write,
{
    if lines.is_empty() {
        return Ok(());
    }

    let screen_size = terminal.size().unwrap_or(ratatui::layout::Size::new(0, 0));
    let mut area = terminal.viewport_area;
    let last_cursor_pos = terminal.last_known_cursor_pos;
    let wrapped_lines = lines.len() as u16;
    let mut should_update_area = false;

    let writer = terminal.backend_mut();

    // If the viewport is not at the bottom of the screen, push it down first
    // to make room for the new history lines above it.  We compute
    // `cursor_top` *before* adjusting `area.y` so that the cursor lands at the
    // top of the newly-opened blank region rather than the bottom.  Placing it
    // at the bottom would cause every subsequent `\r\n` to trigger a scroll,
    // pushing one spurious blank line per history line into terminal scrollback.
    let cursor_top = if area.bottom() < screen_size.height {
        let scroll_amount = wrapped_lines.min(screen_size.height - area.bottom());

        // Scroll the region starting just inside the viewport top downward
        // using reverse-index inside a scroll region.
        let top_1based = area.top() + 1;
        queue!(writer, ScrollRegionCmd(top_1based..screen_size.height))?;
        queue!(writer, MoveTo(0, area.top()))?;
        for _ in 0..scroll_amount {
            // Reverse Index – scrolls the region content down one row.
            queue!(writer, Print("\x1bM"))?;
        }
        queue!(writer, ResetScrollRegionCmd)?;

        // Capture cursor_top from the *original* area.top(), before we shift
        // the viewport down.
        let cursor_top = area.top().saturating_sub(1);
        area.y += scroll_amount;
        should_update_area = true;
        cursor_top
    } else {
        area.top().saturating_sub(1)
    };

    // Now constrain scrolling to the rows above the viewport and write the
    // history lines into the region above it.  Because `cursor_top` points to
    // the top of the region (or just above it), the first N−1 `\r\n`s simply
    // advance the cursor through already-blank rows without scrolling.
    //
    // ┌─Screen───────────────────────┐
    // │┌╌Scroll region╌╌╌╌╌╌╌╌╌╌╌╌╌╌┐│
    // │█╌╌╌╌╌╌╌ cursor here ╌╌╌╌╌╌╌╌┘│
    // │┆                            ┆│
    // │┆  ← blank rows to fill     ┆│
    // │┆                            ┆│
    // │╭─Viewport───────────────────╮│
    // ││                            ││
    // │╰────────────────────────────╯│
    // └──────────────────────────────┘

    queue!(writer, ScrollRegionCmd(1..area.top()))?;
    queue!(writer, MoveTo(0, cursor_top))?;

    let wrap_width = area.width.max(1);
    for line in lines {
        queue!(writer, Print("\r\n"))?;
        write_styled_line(writer, line, wrap_width)?;
    }

    queue!(writer, ResetScrollRegionCmd)?;
    queue!(writer, MoveTo(last_cursor_pos.x, last_cursor_pos.y))?;
    Write::flush(writer)?;

    if should_update_area {
        terminal.set_viewport_area(area);
    }
    if wrapped_lines > 0 {
        terminal.note_history_rows_inserted(wrapped_lines);
    }

    Ok(())
}

/// Write `lines` as a contiguous block starting at screen row `start_row`,
/// each row cleared before its text. Unlike [`insert_history_lines`] this sets
/// no scroll region and does not scroll: it simply paints the rows in place, so
/// the caller must guarantee the block fits above the viewport (the last row
/// written stays above the screen bottom, so no `\r\n` can trigger a scroll).
///
/// Used by the bottom-pinned regime to lay freshly committed history into the
/// rows the viewport vacates as it shrinks, without disturbing the history
/// already on screen above them.
pub(crate) fn write_history_block<W: Write>(
    writer: &mut W,
    start_row: u16,
    lines: &[HistoryLine],
    wrap_width: u16,
) -> io::Result<()> {
    if lines.is_empty() {
        return Ok(());
    }
    queue!(writer, MoveTo(0, start_row))?;
    for (index, line) in lines.iter().enumerate() {
        if index > 0 {
            queue!(writer, Print("\r\n"))?;
        }
        write_styled_line(writer, line, wrap_width)?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Line rendering
// ---------------------------------------------------------------------------

fn style_for_kind(kind: LineKind) -> (Color, Color) {
    match kind {
        LineKind::Normal => (Color::White, Color::Reset),
        LineKind::Thinking => (Color::Gray, Color::Reset),
        LineKind::Dim => (Color::DarkGray, Color::Reset),
        LineKind::User => (Color::White, Color::Indexed(238)),
    }
}

fn write_styled_line<W: Write>(
    writer: &mut W,
    line: &HistoryLine,
    _wrap_width: u16,
) -> io::Result<()> {
    let (fg, bg) = style_for_kind(line.kind);
    queue!(
        writer,
        SetColors(Colors::new(fg.into(), bg.into())),
        Clear(ClearType::UntilNewLine),
        Print(&line.text),
        SetForegroundColor(crossterm::style::Color::Reset),
        SetBackgroundColor(crossterm::style::Color::Reset),
        SetAttribute(crossterm::style::Attribute::Reset),
    )
}
