# Custom Terminal & History Tracking

This document explains how the TUI renders an inline chat interface using a
custom terminal abstraction with dynamic viewport resizing and scroll-region
history insertion.

## Problem

ratatui's built-in `Viewport::Inline(n)` creates a viewport with a **fixed
height** that cannot change after construction.  A chat interface needs a
viewport that grows when the model is streaming and shrinks when idle, while
completed conversation turns scroll into the terminal's native scrollback where
the user's scroll wheel works naturally.

## Architecture overview

The solution has four cooperating pieces:

```
┌──────────────────────────────────────────────────────────────┐
│  Conversation (model)                                        │
│  conversation.rs                                             │
│  Owns live items, pending lines, history queue.              │
│  Pure data — no terminal I/O.                                │
├──────────────────────────────────────────────────────────────┤
│  AppState                                                    │
│  lib.rs                                                      │
│  Orchestrates events → Conversation, computes desired        │
│  viewport height each frame.                                 │
├──────────────────────────────────────────────────────────────┤
│  InlineTerminal                                              │
│  lib.rs                                                      │
│  Owns the custom Terminal. Calls flush_history (scroll-      │
│  region insertion) then draw_with_height (viewport resize    │
│  + render) each frame.                                       │
├──────────────────────────────────────────────────────────────┤
│  custom_terminal::Terminal          history_insert            │
│  custom_terminal.rs                 history_insert.rs         │
│  Double-buffered viewport with      Scroll-region ANSI       │
│  mutable Rect. Owns diff/flush.     escape insertion.        │
└──────────────────────────────────────────────────────────────┘
```

## The custom terminal (`custom_terminal.rs`)

### Why not use ratatui's Terminal?

ratatui's `Terminal` bundles together the viewport strategy (fullscreen,
inline, fixed) and the rendering pipeline.  Once you create an inline terminal
with `Viewport::Inline(6)`, the viewport is 6 rows forever — you cannot resize
it between frames.  We need a terminal whose viewport area is a plain `Rect`
that can be resized at will.

### Struct layout

```rust
pub struct Terminal<B> {
    backend: B,
    buffers: [Buffer; 2],      // double-buffered: current + previous
    current: usize,             // index of the current buffer (0 or 1)
    pub viewport_area: Rect,    // ← the key field: mutable viewport
    pub last_known_screen_size: Size,
    pub last_known_cursor_pos: Position,
    visible_history_rows: u16,  // history tracking
    pub hidden_cursor: bool,
}
```

### Initialization

```rust
let terminal = Terminal::with_options(backend)?;
```

The constructor probes the backend for the screen size and the current cursor
position.  The viewport starts at **zero width and zero height**, anchored at
the cursor's current row:

```rust
viewport_area: Rect::new(0, cursor_pos.y, 0, 0)
```

This is important: the viewport does not claim any screen space until the first
`draw` call explicitly sizes it.  The cursor row becomes the boundary between
"content above" (shell prompt, previous output) and "content below" (the TUI
viewport).

### Double-buffered rendering

Two `Buffer` instances are maintained.  On each `draw` call:

1. The render callback writes widgets into the **current** buffer.
2. `Buffer::diff` compares the current buffer against the **previous** buffer
   to produce a minimal set of `(x, y, &Cell)` updates.
3. The diff is sent to the backend via `backend.draw(updates)`.
4. The previous buffer is reset and the indices swap.

This is the same algorithm ratatui uses internally — we just manage the buffers
ourselves so we can resize them between frames.

### Viewport resizing

```rust
pub fn set_viewport_area(&mut self, area: Rect) {
    self.buffers[self.current].resize(area);
    self.buffers[1 - self.current].resize(area);
    self.viewport_area = area;
    self.visible_history_rows = self.visible_history_rows.min(area.top());
}
```

Both buffers are resized to the new area.  This is critical: if only one buffer
were resized, the diff would compare buffers of different dimensions and
produce garbage.  The `visible_history_rows` counter is clamped so it never
exceeds the viewport's top row (see [History row
tracking](#history-row-tracking) below).

### Viewport change clearing

When the viewport moves (its `y` changes) or appears for the first time, stale
terminal content may be visible behind transparent/space cells.
`clear_for_viewport_change` handles this:

```rust
pub fn clear_for_viewport_change(&mut self, new_area: Rect) -> io::Result<()> {
    let pos = if self.viewport_area.is_empty() {
        new_area.as_position()   // first draw: clear from the new area
    } else {
        self.viewport_area.as_position()  // subsequent: clear from old area
    };
    self.clear_after_position(pos)
}
```

`clear_after_position` moves the cursor and emits a "clear from cursor down"
escape, then resets the previous buffer to force a full repaint on the next
diff.

## Dynamic viewport height

Each frame, `InlineTerminal::draw` asks `AppState` how tall the viewport should
be:

```rust
fn desired_viewport_height(&self, width: u16) -> u16 {
    let composer_h = self.composer.height(width).max(3);
    let footer_h: u16 = 1;
    let live_h: u16 = if self.running { 3 }
                       else if !self.log.live.is_empty() { 2 }
                       else { 1 };
    (live_h + composer_h + footer_h).min(MAX_VIEWPORT_HEIGHT)
}
```

The viewport is small when idle (just the composer + footer + one live row) and
grows to `MAX_VIEWPORT_HEIGHT` when a model turn is active and streaming
content.

### The draw cycle

`draw_with_height` runs inside a **synchronized update** to prevent flicker:

```
stdout.sync_update(|_| {
    1. Read terminal size
    2. Compute new viewport area (same y, new width + height)
    3. If viewport overflows screen bottom → scroll-region scroll
    4. If area changed → clear_for_viewport_change + set_viewport_area
    5. terminal.draw(render_callback)
})
```

#### Step 3: scroll-region overflow handling

When the viewport grows and its bottom would exceed the screen height, we need
to push the content *above* the viewport into terminal scrollback to make room.
This is done with ANSI scroll regions:

```
ESC [ 1 ; {viewport_top} r    ← confine scrolling to rows above viewport
MoveTo(0, viewport_top - 1)   ← cursor to bottom of that region
\n × overflow                  ← push content up into scrollback
ESC [ r                        ← reset scroll region
```

After scrolling, the viewport's `y` is moved down to `screen_height - height`
so it sits flush at the screen bottom.

## History insertion (`history_insert.rs`)

### The pipeline

Conversation content follows a pipeline:

```
Agent events
    → Conversation.live (rendered in the transient viewport)
    → Conversation.pending_lines (staged, paced by on_tick)
    → Conversation.history_queue (ready to flush)
    → InlineTerminal.flush_history → insert_history_lines
    → Terminal scrollback (permanent, scrollable)
```

Once content is finalized (a turn completes, a tool finishes), it moves through
the pipeline and ends up as permanent terminal scrollback that lives *above*
the viewport.

### How scroll-region insertion works

The key insight: we want to write lines above the viewport without disturbing
the viewport itself.  ANSI scroll regions let us confine all scrolling to a
sub-region of the screen.

`insert_history_lines` performs two phases:

**Phase 1: Make room** — if the viewport is not at the screen bottom, push it
down first using reverse-index scrolling:

```
┌─ Screen (before) ────────────┐
│ existing scrollback           │ ← rows 0..viewport_top
│╭─ Viewport ─────────────────╮│ ← viewport_area
││ live content                ││
│╰─────────────────────────────╯│
│ (empty space)                 │ ← rows below viewport
└───────────────────────────────┘
```

```
SetScrollRegion(viewport_top+1 .. screen_height)
MoveTo(0, viewport_top)
Print("\x1bM") × scroll_amount    ← reverse index pushes content DOWN
ResetScrollRegion
viewport_area.y += scroll_amount
```

```
┌─ Screen (after) ─────────────┐
│ existing scrollback           │
│ (new empty rows)              │ ← room for history lines
│╭─ Viewport ─────────────────╮│ ← viewport moved down
││ live content                ││
│╰─────────────────────────────╯│
└───────────────────────────────┘
```

**Phase 2: Write history** — confine scrolling to the rows above the viewport,
position the cursor at the bottom of that region, and write lines:

```
SetScrollRegion(1 .. viewport_top)
MoveTo(0, viewport_top - 1)

For each history line:
    Print("\r\n")              ← scroll existing content UP within the region
    SetColors(fg, bg)          ← style for this LineKind
    Clear(UntilNewLine)        ← wipe stale content on this row
    Print(text)                ← the actual line
    ResetColors

ResetScrollRegion
MoveTo(saved_cursor)          ← restore cursor for the viewport
```

```
┌─ Screen (final) ─────────────┐
│ older scrollback              │ ← pushed up by the insertion
│ newly inserted history lines  │ ← just written
│╭─ Viewport ─────────────────╮│
││ live content                ││
│╰─────────────────────────────╯│
└───────────────────────────────┘
```

The viewport is completely undisturbed — it stays at the same screen position,
same content.  The new lines appear immediately above it.

### Styled line rendering

Each `HistoryLine` has a `LineKind` that maps to colors:

| LineKind   | Foreground   | Background       |
|------------|-------------|------------------|
| `Normal`   | White       | Reset (default)  |
| `Thinking` | Gray        | Reset            |
| `Dim`      | DarkGray    | Reset            |
| `User`     | White       | Color 238 (dark) |

The user prompt echo gets a filled background so it stands out visually from
assistant output.

Lines are written with raw crossterm `queue!` calls rather than ratatui widgets
because scrollback content is permanent and never re-rendered — there's no
reason to go through the double-buffered diff engine for one-shot output.

## History row tracking

The terminal tracks how many history rows have been inserted above the viewport:

```rust
pub fn note_history_rows_inserted(&mut self, inserted_rows: u16) {
    self.visible_history_rows = self
        .visible_history_rows
        .saturating_add(inserted_rows)
        .min(self.viewport_area.top());
}
```

The counter is clamped to `viewport_area.top()` because that's the maximum
number of rows that can physically exist between the top of the screen and the
viewport.  If the terminal scrollback has pushed older rows off-screen, they're
no longer "visible" history rows.

This counter is updated by `insert_history_lines` after each batch and by
`set_viewport_area` (which clamps it when the viewport moves).  It's available
for future use in resize handling — when the terminal size changes, the TUI
needs to know how many rows above the viewport are "ours" versus pre-existing
shell content to correctly reposition the viewport.

## Lifecycle

### Startup

```
1. enable_raw_mode()
2. EnableBracketedPaste, PushKeyboardEnhancementFlags, Hide cursor
3. Terminal::with_options(backend)  → viewport at (0, cursor_y, 0, 0)
4. flush_welcome()                 → 4 history lines inserted via scroll region
5. draw()                          → viewport grows from 0 to desired height
```

### Main loop (each iteration)

```
1. Process input / model events / tick
2. flush_history()   → drain history_queue into scrollback via scroll region
3. draw()            → recompute desired height, resize viewport, render frame
```

### Shutdown

```
1. finalize_all()          → flush remaining live content to scrollback
2. flush_history() + draw() → final render
3. Drop InlineTerminal:
   a. MoveTo(0, viewport_bottom)  → cursor below viewport
   b. Show cursor
   c. PopKeyboardEnhancementFlags, DisableBracketedPaste
   d. Clear(UntilNewLine)
   e. disable_raw_mode()
```

After shutdown, the terminal's normal scrollback contains the full conversation
history and the shell prompt appears below it.

## Synchronized updates

All drawing is wrapped in `crossterm::SynchronizedUpdate::sync_update`, which
brackets the output with DCS sequences (`ESC P =1s ESC \` to begin, `ESC P =2s
ESC \` to end).  Terminals that support this protocol (iTerm2, Kitty, WezTerm,
Ghostty, and others) buffer all intermediate output and flush it atomically,
eliminating visible flicker.  Terminals that don't recognize the sequences
simply ignore them.

## File map

| File | Responsibility |
|------|---------------|
| `custom_terminal.rs` | `Terminal` struct: viewport rect, double-buffered draw, clear, cursor, history counter |
| `history_insert.rs` | `insert_history_lines`: scroll-region ANSI insertion, styled line writing, `ScrollRegionCmd` / `ResetScrollRegionCmd` |
| `conversation.rs` | Data model: live items → pending lines → history queue pipeline (no I/O) |
| `lib.rs` | `InlineTerminal`: wires everything together — `flush_history`, `draw_with_height`, `desired_viewport_height` |
