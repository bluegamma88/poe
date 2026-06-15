# Two-Regime Viewport

The TUI renders an inline chat interface inside the user's terminal. The
viewport—the rectangle where ratatui draws live content—changes height every
frame depending on what's being displayed. The tricky part is coordinating
those height changes with the history lines being inserted above the viewport
so the user never sees a blank gap or a visual glitch.

The solution splits the viewport's lifetime into two **regimes**. The first
regime handles startup, when the viewport is small and there is empty space
below it. The second handles the steady state, when the viewport is pinned to
the screen bottom and every size change must be reconciled with the scrollback
above it. A one-way latch (`bottom_pinned`) governs the transition.

## Visual model

```
┌─ Screen ──────────────────────────┐
│ (shell prompt, prior output)      │  ← pre-existing content
│                                   │
│ ── history lines ──               │  ← finalized conversation turns
│                                   │
│╭─ Viewport ──────────────────────╮│  ← ratatui-drawn live zone
││  live items (streaming text,    ││
││  tool spinners)                 ││
││  ┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄ ││
││  composer (input box)           ││
││  footer (status bar)            ││
│╰─────────────────────────────────╯│
└───────────────────────────────────┘
```

The viewport height is computed each frame by `AppState::desired_viewport_height`:

```rust
fn desired_viewport_height(&self, width: u16, screen_height: u16) -> u16 {
    let composer_h = self.composer.height(width).max(3);
    let footer_h: u16 = 1;
    let live_h = self.live_content_height(width);
    let max_height = screen_height / 2;
    (live_h + composer_h + footer_h).min(max_height)
}
```

When idle it is just a few rows (composer + footer). When the model is
streaming it grows to accommodate the live content, capped at half the screen.

## Regime 1 — Top-anchored (startup)

**Invariant:** The viewport's top row stays fixed at its initial screen
position and the viewport grows downward.

```
 row 0  ┌────────────────────────────┐
        │ shell prompt               │
 cursor │─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ │  ← viewport_area.y (fixed)
        │╭─ Viewport (small) ───────╮│
        ││ composer + footer        ││
        │╰──────────────────────────╯│
        │                            │
        │ (unused screen space)      │
        └────────────────────────────┘
```

On construction, the viewport starts at zero height, anchored at the cursor
row:

```rust
viewport_area: Rect::new(0, cursor_pos.y, 0, 0)
```

Each frame `draw_with_height` computes the new area and simply extends it
downward. History lines are inserted above the viewport by
`InlineTerminal::flush_history`, which calls `insert_history_lines` using ANSI
scroll regions to push content into the rows above without disturbing the
viewport.

### Transition to Regime 2

The first time the viewport's bottom would exceed the screen height, the
overflow path fires:

```rust
if area.bottom() > size.height {
    let overflow = area.bottom() - size.height;
    scroll_rows_into_scrollback(terminal, area.top(), overflow);
    area.y = size.height - area.height;
    terminal.set_bottom_pinned(true);   // ← latch: never goes back to false
}
```

This scrolls the rows above the viewport into terminal scrollback (pushing
them off the top of the screen), then repositions the viewport flush against
the screen bottom and sets the `bottom_pinned` flag. The flag is a one-way
latch—once set, the session stays in Regime 2 forever.

In practice, the transition happens early: typically on the first turn when
streaming output pushes the viewport height past what fits below the cursor.

## Regime 2 — Bottom-pinned (steady state)

**Invariant:** The viewport's bottom row is always the last row of the screen.
The top moves up and down as the viewport grows and shrinks.

```
 row 0  ┌────────────────────────────┐
        │ finalized history          │  ← above the viewport
        │ …                          │
        │╭─ Viewport ───────────────╮│  ← top is screen_height - height
        ││ live zone                ││
        ││ composer                 ││
        ││ footer                   ││
        │╰──────────────────────────╯│  ← always screen bottom
        └────────────────────────────┘
```

### The coordination problem

In this regime, two things happen simultaneously each frame:

1. **History lines** are ready to be flushed above the viewport.
2. **The viewport height changes** (it may grow or shrink depending on live
   content).

These two operations interact: if the viewport shrinks, its top moves down,
vacating rows. Those vacated rows need to be filled with something—either new
history lines or by pulling existing history down. If the viewport grows, its
top moves up, covering rows that may contain history that needs to be scrolled
into terminal scrollback first.

Handling these independently would create a frame where a blank gap is visible
between the history and the viewport. The solution is to handle them together
in a single function.

### Why `flush_history` is a no-op in Regime 2

```rust
fn flush_history(&mut self, app: &mut AppState) -> Result<(), TuiError> {
    if self.terminal.is_bottom_pinned() {
        return Ok(());   // ← history is drained inside draw() instead
    }
    // ... regime 1 path ...
}
```

In Regime 1, history insertion and viewport drawing are independent
operations—`flush_history` runs first, then `draw` adjusts the viewport. This
works because the viewport's top edge is fixed, so inserting lines above it
cannot conflict with a height change.

In Regime 2, the viewport's top edge moves every time the height changes, so
the history insertion must be coordinated with the viewport repositioning. Both
are handled inside `draw_with_height` via `reconcile_bottom_pinned`.

### `reconcile_bottom_pinned`

This is the heart of Regime 2. It receives the terminal, the new viewport area
(computed from the desired height), the screen size, and the app state. It
drains the history queue and adjusts the viewport atomically rather than in two
separate steps.

```rust
fn reconcile_bottom_pinned(
    terminal: &mut Terminal<...>,
    area: &mut Rect,       // new viewport area (y = screen_height - height)
    size: Size,            // current screen size
    app: &mut AppState,
) -> io::Result<()>
```

There are three cases, determined by comparing the old viewport top to the new
one.

#### Case 1: GROW (viewport top rises — `new_top < old_top`)

The viewport is getting taller. Its top edge moves up, covering rows that
previously contained history.

```
 Before:                          After:
 ┌────────────────────────┐       ┌────────────────────────┐
 │ history                │       │ history (some scrolled  │
 │ history ←covered by    │       │         off screen)     │
 │          growth        │       │╭─ Viewport (taller) ──╮│
 │╭─ Viewport ──────────╮│       ││                       ││
 ││                      ││       ││                       ││
 │╰──────────────────────╯│       ││                       ││
 └────────────────────────┘       │╰───────────────────────╯│
                                  └─────────────────────────┘
```

Steps:
1. Insert any queued history lines above the old viewport top (the normal
   scroll-region insertion).
2. Scroll the rows the viewport now covers (from `new_top` to `old_top`) into
   terminal scrollback using `scroll_rows_into_scrollback`.
3. Update the viewport area.

#### Case 2: SHRINK — common case (`new_top >= old_top`, lines fit)

The viewport is getting shorter. Its top edge descends, vacating rows. New
history lines are written directly into the vacated rows; any surplus space is
filled by pulling existing history down.

```
 Before:                          After:
 ┌────────────────────────┐       ┌────────────────────────┐
 │ history                │       │ history                │
 │╭─ Viewport (taller) ──╮│       │ new history lines ←──  │
 ││                      ││       │╭─ Viewport (shorter) ─╮│
 ││                      ││       ││                       ││
 ││                      ││       │╰───────────────────────╯│
 │╰──────────────────────╯│       └─────────────────────────┘
 └────────────────────────┘
```

`shrink` is the number of rows the top descends (`new_top - old_top`). The
vacated rows are partitioned:

```rust
let shrink = new_top - old_top;
let above  = n.saturating_sub(shrink);   // lines that overflow into scrollback
let pulldown = shrink.saturating_sub(n); // empty rows to fill by pulling history down
```

- If there are more history lines than vacated rows (`n > shrink`), the excess
  (`above`) is scrolled into terminal scrollback.
- If there are fewer history lines than vacated rows (`n < shrink`), the
  surplus empty rows (`pulldown`) are filled by pulling existing history down
  using reverse-index scrolling.
- The history lines themselves are painted directly into the rows immediately
  above the new viewport top via `write_history_block`, which writes at
  absolute screen positions without any scroll regions.

The key property: the history already visible on screen above the old viewport
top is never moved or redrawn in the common case, so there is no flicker.

#### Case 3: SHRINK — overflow (`n > new_top`)

A degenerate case where more history lines are queued than there are rows above
the viewport (e.g. when an entire turn is finalized at once). Falls back to the
scroll-region insertion path. A brief visual artifact is acceptable for what is
essentially a bulk dump.

### Timeline of a single frame (Regime 2)

```
1. Main loop processes input / model events / tick
2. app.log.on_tick() — advances spinner, releases one pending line
3. flush_history() — returns immediately (bottom-pinned no-op)
4. draw() →
   a. Compute desired height → new viewport area
   b. stdout.sync_update(|| {
        reconcile_bottom_pinned(...)  // drain history + adjust viewport atomically
        terminal.draw(render_app)     // ratatui double-buffered diff/flush
      })
```

Everything inside `sync_update` is bracketed by DCS synchronized-update
sequences, so the terminal buffers all intermediate escape codes and presents
them as a single atomic update—no partial frames are ever visible.

## Helper functions

### `scroll_rows_into_scrollback`

Pushes `count` rows immediately above the viewport into terminal scrollback by
creating a scroll region covering those rows and emitting newlines:

```
ESC [ 1 ; {viewport_top} r     ← scroll region: top of screen to viewport top
MoveTo(0, viewport_top - 1)    ← cursor at bottom of the region
\n × count                      ← push content up, off-screen into scrollback
ESC [ r                         ← reset scroll region
```

Used by both regimes—Regime 1 during the overflow transition, and Regime 2
when the viewport grows (covering rows that must be preserved).

### `pull_history_down`

The inverse: pulls history content *down* within the region above the viewport,
filling blank rows that open when the viewport shrinks by more than the number
of new history lines:

```
ESC [ 1 ; {region_bottom} r    ← scroll region: top of screen to new viewport top
MoveTo(0, 0)                   ← cursor at top of the region
ESC M × count                   ← reverse index: push content down
ESC [ r                         ← reset scroll region
```

The bottom `count` rows of the region (stale rows the viewport just vacated)
are pushed off the bottom of the region and discarded. `count` blank rows open
at the top. This is purely cosmetic—it prevents the user from seeing stale
viewport content lingering in the gap between history and viewport.

### `write_history_block`

Paints a block of history lines at absolute screen coordinates without scroll
regions. Used in the common shrink case to fill the exact rows the viewport
vacated:

```rust
pub(crate) fn write_history_block(
    writer: &mut W,
    start_row: u16,
    lines: &[HistoryLine],
    wrap_width: u16,
) -> io::Result<()>
```

Moves the cursor to `start_row`, writes each line with its style, and returns.
No scrolling occurs—the caller guarantees the block fits in the vacated space.

## State machine summary

```
                 ┌──────────────────┐
     start ───→  │  Regime 1        │
                 │  top-anchored    │
                 │                  │
                 │  flush_history   │
                 │  then draw       │
                 └────────┬─────────┘
                          │
              viewport overflows screen bottom
              (bottom_pinned = true, one-way latch)
                          │
                          ▼
                 ┌──────────────────┐
                 │  Regime 2        │
                 │  bottom-pinned   │
                 │                  │
                 │  flush_history   │
                 │  is a no-op;     │
                 │  draw calls      │
                 │  reconcile_...   │
                 └──────────────────┘
                          │
                       (never returns to Regime 1)
```

## Why two regimes?

A single regime would be simpler, but neither regime works well alone:

- **Top-anchored only** breaks when the viewport reaches the screen bottom.
  Without pinning, the viewport would overflow and content would be drawn
  off-screen. You'd need increasingly complex overflow handling every frame.

- **Bottom-pinned only** doesn't work at startup. The viewport starts at the
  cursor position (which may be in the middle of the screen). Immediately
  pinning to the bottom would leave a gap between existing shell content and
  the viewport, or would scroll shell output into scrollback prematurely.

The two-regime approach handles both gracefully: Regime 1 provides a natural
startup where the viewport grows from the cursor position, and Regime 2
provides a stable steady state where the viewport is always at the screen
bottom.

## Key files

| File | Role |
|------|------|
| `custom_terminal.rs` | `Terminal` struct with `bottom_pinned` flag, `set_viewport_area`, double-buffered draw |
| `history_insert.rs` | `insert_history_lines` (scroll-region insertion), `write_history_block` (direct paint) |
| `lib.rs` | `InlineTerminal::draw_with_height` (regime dispatch), `reconcile_bottom_pinned` (Regime 2 core), `scroll_rows_into_scrollback`, `pull_history_down` |
| `conversation.rs` | Data model—produces `HistoryLine`s that the viewport system consumes; no knowledge of regimes |
