//! Custom terminal with dynamic viewport resizing.
//!
//! A simplified fork of `ratatui::Terminal` that exposes the viewport area as a
//! mutable field, allowing the viewport height to change every frame. Uses
//! ratatui's stock `Buffer::diff` + `Backend::draw` for flushing.

use std::io;

use ratatui::backend::Backend;
use ratatui::backend::ClearType;
use ratatui::buffer::Buffer;
use ratatui::layout::Position;
use ratatui::layout::Rect;
use ratatui::layout::Size;
use ratatui::widgets::Widget;

// ---------------------------------------------------------------------------
// Frame
// ---------------------------------------------------------------------------

pub struct Frame<'a> {
    pub(crate) cursor_position: Option<Position>,
    pub(crate) viewport_area: Rect,
    pub(crate) buffer: &'a mut Buffer,
}

impl Frame<'_> {
    /// The renderable area of this frame (identical to the current viewport).
    pub const fn area(&self) -> Rect {
        self.viewport_area
    }

    /// Render any [`Widget`] into `area`.
    pub fn render_widget<W: Widget>(&mut self, widget: W, area: Rect) {
        widget.render(area, self.buffer);
    }

    /// Request the cursor to be shown at `position` after this frame is drawn.
    pub fn set_cursor_position<P: Into<Position>>(&mut self, position: P) {
        self.cursor_position = Some(position.into());
    }

    #[allow(dead_code)]
    pub fn buffer_mut(&mut self) -> &mut Buffer {
        self.buffer
    }
}

// ---------------------------------------------------------------------------
// Terminal
// ---------------------------------------------------------------------------

pub struct Terminal<B> {
    backend: B,
    /// Double-buffered rendering: current and previous frame.
    buffers: [Buffer; 2],
    /// Index of the *current* buffer (the one being drawn into).
    current: usize,
    pub hidden_cursor: bool,
    /// The viewport rectangle – publicly mutable via [`set_viewport_area`].
    pub viewport_area: Rect,
    pub last_known_screen_size: Size,
    pub last_known_cursor_pos: Position,
    /// Number of visible history rows above the viewport.
    visible_history_rows: u16,
    /// Latches `true` the first time the viewport overflows the screen bottom.
    /// Once set, the viewport is pinned to the bottom of the screen every frame
    /// instead of growing downward from a fixed top.
    bottom_pinned: bool,
}

impl<B: Backend> Terminal<B> {
    /// Create a new terminal, probing the backend for screen size and cursor
    /// position.  The viewport starts at height&nbsp;0, anchored at the
    /// current cursor row.
    pub fn with_options(mut backend: B) -> io::Result<Self> {
        let screen_size = backend.size()?;
        let cursor_pos = backend
            .get_cursor_position()
            .unwrap_or(Position { x: 0, y: 0 });
        Ok(Self {
            backend,
            buffers: [Buffer::empty(Rect::ZERO), Buffer::empty(Rect::ZERO)],
            current: 0,
            hidden_cursor: false,
            viewport_area: Rect::new(0, cursor_pos.y, 0, 0),
            last_known_screen_size: screen_size,
            last_known_cursor_pos: cursor_pos,
            visible_history_rows: 0,
            bottom_pinned: false,
        })
    }

    /// Whether the viewport is pinned to the bottom of the screen. Latches on
    /// the first overflow and stays set for the rest of the session.
    pub fn is_bottom_pinned(&self) -> bool {
        self.bottom_pinned
    }

    /// Latch the viewport into the bottom-pinned regime.
    pub fn set_bottom_pinned(&mut self, pinned: bool) {
        self.bottom_pinned = pinned;
    }

    // -- viewport -----------------------------------------------------------

    /// Resize both double-buffers to match `area` and update the viewport.
    pub fn set_viewport_area(&mut self, area: Rect) {
        self.buffers[self.current].resize(area);
        self.buffers[1 - self.current].resize(area);
        self.viewport_area = area;
        self.visible_history_rows = self.visible_history_rows.min(area.top());
    }

    /// Reset viewport bookkeeping to `area` with no visible history above it,
    /// after the screen and scrollback have been purged for resize reflow.
    /// Pins the viewport to the bottom regime and forces the next draw to fully
    /// repaint the viewport.
    pub fn reset_for_reflow(&mut self, area: Rect) {
        self.bottom_pinned = true;
        self.set_viewport_area(area);
        self.visible_history_rows = 0;
        self.invalidate_viewport();
    }

    /// Clear stale content when the viewport area changes.
    ///
    /// On the first draw the old viewport is still empty, so the clear starts
    /// from the *new* area instead.
    pub fn clear_for_viewport_change(&mut self, new_area: Rect) -> io::Result<()> {
        let pos = if self.viewport_area.is_empty() {
            new_area.as_position()
        } else {
            self.viewport_area.as_position()
        };
        self.clear_after_position(pos)
    }

    // -- drawing ------------------------------------------------------------

    /// Draw a single frame: render via `render_callback`, diff, flush, swap.
    pub fn draw<F>(&mut self, render_callback: F) -> io::Result<()>
    where
        F: FnOnce(&mut Frame),
    {
        self.autoresize()?;

        // Build a Frame that borrows the current buffer.
        let mut frame = Frame {
            cursor_position: None,
            viewport_area: self.viewport_area,
            buffer: &mut self.buffers[self.current],
        };
        render_callback(&mut frame);
        let cursor_position = frame.cursor_position;
        // `frame` is no longer used – NLL releases the buffer borrow.

        // Diff against the previous buffer and flush to the backend.
        let previous = &self.buffers[1 - self.current];
        let current = &self.buffers[self.current];
        let updates = previous.diff(current);
        if let Some(&(x, y, _)) = updates.last() {
            self.last_known_cursor_pos = Position { x, y };
        }
        self.backend.draw(updates.into_iter())?;

        // Cursor visibility.
        match cursor_position {
            None => self.hide_cursor()?,
            Some(position) => {
                self.show_cursor()?;
                self.set_cursor_position(position)?;
            }
        }

        // Swap buffers and send everything to the terminal.
        self.buffers[1 - self.current].reset();
        self.current = 1 - self.current;
        Backend::flush(&mut self.backend)?;
        Ok(())
    }

    // -- helpers ------------------------------------------------------------

    fn autoresize(&mut self) -> io::Result<()> {
        let screen_size = self.backend.size()?;
        if screen_size != self.last_known_screen_size {
            self.last_known_screen_size = screen_size;
        }
        Ok(())
    }

    pub fn size(&self) -> io::Result<Size> {
        self.backend.size()
    }

    #[allow(dead_code)]
    pub const fn backend(&self) -> &B {
        &self.backend
    }

    pub fn backend_mut(&mut self) -> &mut B {
        &mut self.backend
    }

    // -- cursor -------------------------------------------------------------

    pub fn hide_cursor(&mut self) -> io::Result<()> {
        self.backend.hide_cursor()?;
        self.hidden_cursor = true;
        Ok(())
    }

    pub fn show_cursor(&mut self) -> io::Result<()> {
        self.backend.show_cursor()?;
        self.hidden_cursor = false;
        Ok(())
    }

    pub fn set_cursor_position<P: Into<Position>>(&mut self, position: P) -> io::Result<()> {
        let position = position.into();
        self.backend.set_cursor_position(position)?;
        self.last_known_cursor_pos = position;
        Ok(())
    }

    #[allow(dead_code)]
    pub fn get_cursor_position(&mut self) -> io::Result<Position> {
        self.backend.get_cursor_position()
    }

    // -- clearing -----------------------------------------------------------

    #[allow(dead_code)]
    pub fn clear(&mut self) -> io::Result<()> {
        if self.viewport_area.is_empty() {
            return Ok(());
        }
        self.clear_after_position(self.viewport_area.as_position())
    }

    pub fn clear_after_position(&mut self, position: Position) -> io::Result<()> {
        self.backend.set_cursor_position(position)?;
        self.backend.clear_region(ClearType::AfterCursor)?;
        // Reset the previous buffer so the next diff repaints everything.
        self.buffers[1 - self.current].reset();
        Ok(())
    }

    /// Force the next draw to repaint everything by resetting the diff base.
    pub fn invalidate_viewport(&mut self) {
        self.buffers[1 - self.current].reset();
    }

    // -- history row tracking -----------------------------------------------

    #[allow(dead_code)]
    pub fn visible_history_rows(&self) -> u16 {
        self.visible_history_rows
    }

    pub fn note_history_rows_inserted(&mut self, inserted_rows: u16) {
        self.visible_history_rows = self
            .visible_history_rows
            .saturating_add(inserted_rows)
            .min(self.viewport_area.top());
    }
}

impl<B> Drop for Terminal<B> {
    fn drop(&mut self) {
        // Cursor restoration is handled by InlineTerminal::drop which has
        // access to the concrete backend type.
    }
}
