# Custom Terminal Plan

Replace the fixed-height ratatui `Viewport::Inline(6)` with a custom terminal
that supports dynamic viewport resizing, scroll-region history insertion, and
history row tracking. The custom flush/diff engine is explicitly out of scope —
ratatui's stock diff is fine for now.

## Reference

All patterns are lifted from `reference/codex/codex-rs/tui/src/`:
- `custom_terminal.rs` — forked `ratatui::Terminal` with mutable viewport
- `tui.rs` — `Tui::draw(height, draw_fn)` orchestration
- `insert_history.rs` — scroll-region history insertion

## Current state

| File | Role |
|------|------|
| `lib.rs` | `InlineTerminal` wraps `ratatui::Terminal<CrosstermBackend<Stdout>>` with `Viewport::Inline(6)` |
| `lib.rs` | `flush_history` calls `terminal.insert_before(n, \|buf\| …)` |
| `lib.rs` | `draw` calls `terminal.draw(\|frame\| render_app(…))` |
| `lib.rs` | `render_app` computes layout with `Constraint::Min(1)` for live zone, `Constraint::Length(h)` for composer, `Constraint::Length(1)` for footer |
| `conversation.rs` | `take_history_lines(width)` drains `history_queue` into pre-wrapped `Vec<HistoryLine>` |
| `composer.rs` | `height(width)` returns how tall the composer needs to be (3..=8) |

---

## Step 1 — `custom_terminal.rs`: the forked Terminal struct

Create `crates/agent-tui/src/custom_terminal.rs`.

### What to copy from codex

Copy `Terminal<B>` with these fields (drop everything we don't need yet):

```rust
pub struct Terminal<B: Backend + Write> {
    backend: B,
    buffers: [Buffer; 2],
    current: usize,
    pub hidden_cursor: bool,
    pub viewport_area: Rect,
    pub last_known_screen_size: Size,
    pub last_known_cursor_pos: Position,
    visible_history_rows: u16,
}
```

### Public API to implement

| Method | Purpose |
|--------|---------|
| `with_options(backend) -> io::Result<Self>` | Probe cursor position, set viewport to `(0, cursor_y, 0, 0)` |
| `set_viewport_area(area)` | Resize both buffers, update `viewport_area`, clamp `visible_history_rows` |
| `get_frame() -> Frame` | Return a `Frame` referencing the current buffer and `viewport_area` |
| `draw(render_callback)` | Autoresize → get frame → render → flush diff → cursor → swap buffers |
| `flush()` | Use **ratatui's stock diff** (`Buffer::diff`) to emit updates. No custom `DrawCommand` engine. |
| `size()` | Delegate to `backend.size()` |
| `autoresize()` | Query backend size, store in `last_known_screen_size` |
| `clear()` | Clear from `viewport_area` top |
| `clear_after_position(pos)` | Move cursor to `pos`, clear-after-cursor, reset previous buffer |
| `invalidate_viewport()` | Reset previous buffer to force full repaint |
| `backend() / backend_mut()` | Accessors |
| `visible_history_rows()` | Getter |
| `note_history_rows_inserted(n)` | Increment `visible_history_rows`, clamped to `viewport_area.top()` |
| `hide_cursor() / show_cursor() / set_cursor_position()` | Delegate to backend |

### What to skip

- Custom `diff_buffers` / `DrawCommand` / `ClearToEnd` — use ratatui's
  `Buffer::diff` + `backend.draw()` for flushing.
- `display_width` / OSC-aware width — not needed without hyperlinks.
- Per-frame cursor style (`SetCursorStyle`) — not needed yet.

### Frame struct

```rust
pub struct Frame<'a> {
    pub(crate) cursor_position: Option<Position>,
    pub(crate) viewport_area: Rect,
    pub(crate) buffer: &'a mut Buffer,
}
```

Expose: `area()`, `render_widget(w, area)`, `set_cursor_position(pos)`,
`buffer_mut()`.

### Diff/flush strategy

Use ratatui's existing `Buffer::diff` to get the changeset, then call
`backend.draw(diff)` + `backend.flush()`. This is exactly what the stock
`ratatui::Terminal` does internally. The only thing we're changing is that the
*viewport area can change between frames*.

---

## Step 2 — Scroll-region history insertion

Create `crates/agent-tui/src/history_insert.rs`.

### Core function

```rust
pub(crate) fn insert_history_lines<B: Backend + Write>(
    terminal: &mut Terminal<B>,
    lines: Vec<HistoryLine>,
) -> io::Result<()>
```

### Algorithm (ported from codex `insert_history.rs`, standard mode)

1. Read `viewport_area` from the terminal.
2. Pre-wrap each `HistoryLine` to `viewport_area.width` (already done by
   `Conversation::take_history_lines`, so this is mostly a row-count
   calculation).
3. If the viewport bottom is not at the screen bottom, scroll the viewport
   down first to make room:
   ```
   scroll_amount = wrapped_lines.min(screen_height - viewport_bottom)
   SetScrollRegion(viewport_top+1 .. screen_height)
   MoveTo(0, viewport_top); print "\x1bM" × scroll_amount  // reverse-index
   ResetScrollRegion
   viewport_area.y += scroll_amount
   ```
4. Set a scroll region from row 1 to `viewport_area.top()` (the area above
   the viewport). Move cursor to the end of this region.
5. For each line: `Print("\r\n")`, then write the styled line using crossterm
   `queue!` calls (`SetColors`, `Print` for each span, reset).
6. `ResetScrollRegion`. Restore cursor to `last_known_cursor_pos`.
7. If `viewport_area` changed, call `terminal.set_viewport_area(area)`.
8. Call `terminal.note_history_rows_inserted(wrapped_lines)`.

### Helper types

Port the two crossterm `Command` impls from codex:

```rust
struct SetScrollRegion(Range<u16>);   // writes ESC[{start};{end}r
struct ResetScrollRegion;             // writes ESC[r
```

### Styled line writing

Write a `write_history_line(writer, line)` function that:
- Queues `SetColors` for the line's `LineKind` style
- Queues `Clear(UntilNewLine)` to clear stale content
- Queues `Print(text)` for the line content
- Resets colors after

This is simpler than codex's span-level writer because our `HistoryLine` has
a single style per line (determined by `LineKind`), not per-span styling.

---

## Step 3 — Wire up `InlineTerminal`

### 3a. Change the terminal type

Replace:
```rust
struct InlineTerminal {
    terminal: Terminal<CrosstermBackend<Stdout>>,
}
```

With:
```rust
struct InlineTerminal {
    terminal: custom_terminal::Terminal<CrosstermBackend<Stdout>>,
}
```

### 3b. Change `enter()`

Replace the `Viewport::Inline(6)` constructor:

```rust
// Before
let terminal = Terminal::with_options(backend, TerminalOptions {
    viewport: Viewport::Inline(MAX_VIEWPORT_HEIGHT),
})?;

// After
let terminal = custom_terminal::Terminal::with_options(backend)?;
```

The new terminal starts at height 0, anchored at the current cursor Y.

### 3c. Change `draw()` to accept dynamic height

```rust
fn draw(&mut self, app: &AppState) -> Result<(), TuiError> {
    let screen_size = self.terminal.size()?;
    let height = app.desired_viewport_height(screen_size.width)
                     .min(screen_size.height);
    self.draw_with_height(height, |frame| {
        render_app(frame, frame.area(), app);
    })
}
```

The `draw_with_height` method (on `InlineTerminal`) does what codex's
`Tui::draw(height, draw_fn)` does:

1. Read terminal size.
2. Compute new viewport area: same `y`, new `width = screen_width`,
   new `height = min(requested, screen_height)`.
3. If `area.bottom() > screen_height`, scroll content above the viewport up
   with `scroll_region_up(0..area.top(), overflow)` and set
   `area.y = screen_height - area.height`.
4. If area changed, `clear_for_viewport_change` + `set_viewport_area`.
5. Flush pending history lines.
6. Call `terminal.draw(draw_fn)`.

### 3d. Change `flush_history()`

Replace `terminal.insert_before(…)` with:

```rust
fn flush_history(&mut self, app: &mut AppState) -> Result<(), TuiError> {
    let width = self.terminal.viewport_area.width.max(1) as usize;
    let lines = app.log.take_history_lines(width);
    if lines.is_empty() {
        return Ok(());
    }
    history_insert::insert_history_lines(&mut self.terminal, lines)?;
    Ok(())
}
```

### 3e. Change `flush_welcome()`

Same approach — write the welcome box as history lines instead of
`insert_before`. Convert the welcome content to `Vec<HistoryLine>` and pass
through `insert_history_lines`.

### 3f. Add `AppState::desired_viewport_height()`

Compute how tall the viewport should be this frame:

```rust
fn desired_viewport_height(&self, width: u16) -> u16 {
    let live_lines = count_live_lines(&self.log, width);
    let composer_h = self.composer.height(width).max(3);
    let footer_h = 1;
    (live_lines + composer_h + footer_h).min(MAX_VIEWPORT_HEIGHT)
}
```

Where `count_live_lines` estimates how many wrapped rows the live items
produce (mirror the logic in `render_live` but just counting rows, not
building widgets). When idle with no live content, this returns
`0 + 3 + 1 = 4`. When streaming, it grows up to `MAX_VIEWPORT_HEIGHT`.

Alternatively — and simpler for the first pass — just use a fixed formula:

```rust
let composer_h = self.composer.height(width).max(3);
let footer_h: u16 = 1;
let live_h: u16 = if self.running { 3 } else { 1 };
(live_h + composer_h + footer_h).min(MAX_VIEWPORT_HEIGHT)
```

This keeps the viewport small when idle and grows it when a turn is active.
Exact live-content sizing can be refined later.

---

## Step 4 — Update imports and module declarations

In `lib.rs`:

1. Add `mod custom_terminal;` and `mod history_insert;`.
2. Remove `use ratatui::{Terminal, TerminalOptions, Viewport, …}` — replace
   with `use crate::custom_terminal;`.
3. Keep `use ratatui::{Frame, …}` for the `Frame` type (our custom
   `Frame` will be `custom_terminal::Frame`). Update `render_app` and friends
   to accept `&mut custom_terminal::Frame` instead of `&mut ratatui::Frame`.
4. Remove `MAX_VIEWPORT_HEIGHT` constant (or repurpose it as a cap for
   `desired_viewport_height`).

---

## Step 5 — Drop

Update the `InlineTerminal::Drop` impl. The new terminal doesn't use
ratatui's inline viewport machinery, so we don't need `Viewport::Inline`
cleanup. Just:

1. Show cursor.
2. Pop keyboard enhancement.
3. Disable bracketed paste.
4. Move cursor to viewport bottom + 1 so the shell prompt appears below the
   last rendered content.
5. Clear to end of line.
6. Disable raw mode.

---

## Step 6 — Synchronized output (optional, recommended)

Wrap the draw call in `crossterm::SynchronizedUpdate` to eliminate flicker:

```rust
use crossterm::SynchronizedUpdate;

stdout().sync_update(|_| {
    // viewport adjustment + history flush + terminal.draw(...)
})?;
```

This brackets the output with DCS sequences that tell the terminal to buffer
all changes and flush atomically. Supported by iTerm2, Kitty, WezTerm,
Ghostty, and others. Terminals that don't understand the sequence ignore it
harmlessly.

---

## File summary

| File | Action |
|------|--------|
| `crates/agent-tui/src/custom_terminal.rs` | **New** — forked Terminal struct |
| `crates/agent-tui/src/history_insert.rs` | **New** — scroll-region history insertion |
| `crates/agent-tui/src/lib.rs` | **Modify** — swap InlineTerminal internals, dynamic height, new imports |
| `crates/agent-tui/src/conversation.rs` | **No changes** — model layer is untouched |
| `crates/agent-tui/src/composer.rs` | **No changes** |
| `crates/agent-tui/Cargo.toml` | **No changes** — already depends on ratatui 0.29 + crossterm 0.28 |

## Testing strategy

1. **Unit tests for `custom_terminal.rs`**: create a `TestBackend` (in-memory
   backend), construct the terminal, call `set_viewport_area` with different
   sizes, verify buffer dimensions match.

2. **Unit tests for `history_insert.rs`**: use a `TestBackend` that captures
   queued escape sequences. Verify `SetScrollRegion`/`ResetScrollRegion` are
   emitted, verify `note_history_rows_inserted` is called with the right
   count.

3. **Existing `conversation.rs` tests**: unchanged — they test the model
   layer which is not touched.

4. **Manual smoke test**: run the TUI, verify the viewport starts small,
   grows when a turn streams, and history scrolls into native scrollback.
   Verify `Ctrl-C` exits cleanly, scrollback is readable after exit.

## Risks

- **ratatui `Buffer::diff` assumes the buffer area doesn't move between
  frames.** When the viewport shifts (y changes), the previous buffer is for
  the old position. We must call `invalidate_viewport()` (reset the previous
  buffer) after any y-change so the next diff compares against a clean slate.
  Codex does this in their `clear_for_viewport_change` helper.

- **Terminal compatibility for scroll regions.** `ESC[n;mr` is supported by
  all modern terminals (xterm, iTerm2, Kitty, WezTerm, Ghostty, Windows
  Terminal, Alacritty). The only risk is multiplexers like tmux/Zellij that
  may intercept scroll regions. Codex has a Zellij-specific mode; we can skip
  that for now and add it if users report issues.

- **The welcome box.** Currently `flush_welcome` uses `insert_before` to
  render a bordered `Paragraph` widget into scrollback. The new
  `insert_history_lines` writes plain styled text, not rendered widgets. We
  need to either (a) convert the welcome box to styled text lines, or (b)
  render it into a scratch buffer and extract the text. Option (a) is simpler
  and sufficient — the box is just two lines with a border, easy to
  reconstruct with box-drawing characters.
