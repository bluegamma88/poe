# Reference Width And Height Resize Report

This report documents how the reference TUI in
`reference/codex/codex-rs/tui/src/` handles terminal width and height resizing,
and how that compares with Poe's current TUI.

The key finding is that the reference separates three related but different
resize problems:

1. **Viewport resizing:** the inline ratatui viewport changes width and height.
2. **Terminal resize events:** the terminal emulator reports a new screen size.
3. **Transcript scrollback reflow:** already-written terminal scrollback is
   cleared and rebuilt from source-backed transcript cells at the new size.

Poe currently implements the first problem well and handles basic resize redraws,
but it does not implement the third problem. That means future frames use the new
width and height, while older rows already written into terminal scrollback keep
their old wrapping.

## Files Inspected

Reference:

| File | Role |
| --- | --- |
| `reference/codex/codex-rs/tui/src/custom_terminal.rs` | Custom ratatui terminal with mutable inline viewport and double buffers. |
| `reference/codex/codex-rs/tui/src/tui.rs` | Draw orchestration, viewport resize, legacy draw path, resize-reflow draw path. |
| `reference/codex/codex-rs/tui/src/tui/event_stream.rs` | Converts `crossterm::Event::Resize` into `TuiEvent::Resize`. |
| `reference/codex/codex-rs/tui/src/app.rs` | Routes `Draw` and `Resize` events into rendering. |
| `reference/codex/codex-rs/tui/src/app/resize_reflow.rs` | Source-backed transcript reflow after terminal resize. |
| `reference/codex/codex-rs/tui/src/transcript_reflow.rs` | Debounced resize-reflow scheduler and stream-time repair state. |
| `reference/codex/codex-rs/tui/src/chatwidget.rs` | Updates active stream wrapping width after terminal resize. |
| `reference/codex/codex-rs/tui/src/streaming/controller.rs` | Re-renders active streaming output when width changes. |
| `reference/codex/codex-rs/tui/src/resize_reflow_cap.rs` | Terminal-specific row caps for scrollback replay. |

Poe:

| File | Role |
| --- | --- |
| `crates/agent-tui/src/custom_terminal.rs` | Poe's simplified custom terminal. |
| `crates/agent-tui/src/lib.rs` | Inline terminal draw loop and two-regime viewport logic. |
| `crates/agent-tui/src/conversation.rs` | Current source of queued history lines and live items. |
| `docs/two-regime-viewport.md` | Existing Poe documentation for dynamic viewport height. |

## Reference Terminal Foundation

The reference does not use ratatui's built-in fixed inline viewport. It carries a
mutable viewport rectangle:

```rust
pub viewport_area: Rect,
pub last_known_screen_size: Size,
pub last_known_cursor_pos: Position,
visible_history_rows: u16,
```

The viewport starts with zero width and height at the cursor row:

```rust
viewport_area: Rect::new(0, cursor_pos.y, 0, 0)
```

The critical primitive is `set_viewport_area(area)`. It resizes both buffers
before updating the viewport:

```rust
self.current_buffer_mut().resize(area);
self.previous_buffer_mut().resize(area);
self.viewport_area = area;
self.visible_history_rows = self.visible_history_rows.min(area.top());
```

Resizing both buffers matters because ratatui's draw model diffs the current and
previous buffers. If only one buffer changed shape, the diff base would be
invalid and could leave stale cells or draw out of bounds.

The custom terminal also runs `autoresize()` before every draw. This only updates
`last_known_screen_size`; it does not choose a new viewport. The higher-level
`Tui` draw path owns viewport placement.

## Legacy Reference Draw Path

The older reference path is `Tui::draw(height, draw_fn)`.

Before entering the synchronized update block, it calls `pending_viewport_area()`.
That function samples the real terminal size and cursor position. If the screen
size changed and the cursor row moved since the last known cursor position, it
offsets the viewport by the cursor delta. The comment calls this a heuristic
that works well in at least iTerm2.

Inside `stdout().sync_update(...)`, it:

1. Applies any pending viewport offset and clears the old viewport.
2. Reads the current terminal size.
3. Copies `terminal.viewport_area` into `area`.
4. Sets `area.height = height.min(size.height)`.
5. Sets `area.width = size.width`.
6. If `area.bottom() > size.height`, scrolls rows above the viewport into
   scrollback and moves the viewport to `size.height - area.height`.
7. If `area` changed, clears stale content and calls `set_viewport_area(area)`.
8. Flushes pending history lines above the viewport.
9. Draws the ratatui frame into the resized viewport.

This supports width resizing at the viewport level because every draw uses the
current terminal width:

```rust
area.width = size.width;
terminal.set_viewport_area(area);
```

This supports height resizing at the viewport level because every draw clamps the
requested viewport height to the current terminal height:

```rust
area.height = height.min(size.height);
```

If the viewport no longer fits, the legacy path scrolls rows above it into
terminal scrollback and bottom-aligns the viewport:

```rust
if area.bottom() > size.height {
    terminal.backend_mut().scroll_region_up(0..area.top(), area.bottom() - size.height)?;
    area.y = size.height - area.height;
}
```

The legacy path redraws future frames at the new size, but it does not rebuild
old transcript rows already emitted into terminal scrollback. That limitation is
what the newer resize-reflow path addresses.

## Resize Events

The reference event stream maps crossterm terminal resize notifications to a
distinct TUI event:

```rust
Event::Resize(_, _) => Some(TuiEvent::Resize)
```

`TuiEvent::Resize` is separate from `TuiEvent::Draw` so the app can run
resize-sensitive pre-render work only when needed. In practice, the app handles
both `Draw` and `Resize` through the render path, but resize reflow gets a chance
to inspect terminal dimensions before the frame is drawn.

## Resize-Reflow Path

The newer reference path is feature-gated as `TerminalResizeReflow`. When
enabled, the app renders with:

```rust
tui.draw_with_resize_reflow(desired_height, |frame| { ... })
```

instead of the legacy:

```rust
tui.draw(desired_height, |frame| { ... })
```

The key difference is ownership. The legacy path treats terminal scrollback as
already-written output. Resize reflow treats in-memory transcript cells as the
source of truth, clears Codex-owned terminal history, and replays transcript rows
at the current width.

### Resize Detection

Before rendering, `App::handle_draw_pre_render()` samples the current terminal
size and compares it to `terminal.last_known_screen_size`.

`handle_draw_size_change()` records the observed width in `TranscriptReflowState`
and computes:

```rust
let reflow_needed = self.transcript_reflow.reflow_needed_for_width(size.width);
let height_changed = size.height != last_known_screen_size.height;
let should_rebuild_transcript = reflow_needed || height_changed;
```

Both width and height can schedule transcript rebuilds:

- **Width changes** require rebuilds because text wrapping changes.
- **Height changes** can expose, hide, or shift rows around the inline viewport,
  so the reference also rebuilds from source-backed cells.

The first observed width initializes the state without rebuilding. That avoids
an unnecessary replay on the first draw, before any old-width transcript exists.

### Debouncing

Resize reflow is debounced by `TranscriptReflowState` using a 75 ms quiet period:

```rust
pub(crate) const TRANSCRIPT_REFLOW_DEBOUNCE: Duration = Duration::from_millis(75);
```

Repeated resize events push the deadline out, so dragging a terminal edge does
not rebuild scrollback at every intermediate size. The scheduler tracks:

- `last_observed_width`
- `last_reflow_width`
- `pending_reflow_width`
- `pending_until`

The distinction between observed width and reflowed width is important. Some
terminal emulators report intermediate sizes while resizing and settle on the
final size after a repaint. The reference schedules a cheap follow-up frame after
reflow so it can sample the final width and run one more reflow if needed.

### Viewport Update During Resize Reflow

`Tui::draw_with_resize_reflow()` calls
`update_inline_viewport_for_resize_reflow(terminal, height)`.

That function:

1. Reads the current screen size.
2. Detects whether terminal height shrank or grew.
3. Detects whether the old viewport was bottom-aligned.
4. Sets the new viewport height and width:

   ```rust
   area.height = height.min(size.height);
   area.width = size.width;
   ```

5. If the viewport bottom exceeds the new screen height:
   - If the terminal height did **not** shrink, it scrolls rows above the
     viewport up.
   - If the terminal height **did** shrink, it does not scroll, because resize
     reflow owns rebuilding those rows from transcript source.
   - It then bottom-aligns the viewport.
6. If terminal height grew and the old viewport was bottom-aligned, it keeps the
   viewport bottom-aligned by setting `area.y = size.height - area.height`.
7. If the area changed, it calls `set_viewport_area(area)`, clears from the
   earlier of old/new top rows, and requests a full repaint.

The height-shrink branch is a notable difference from the legacy path. It avoids
scrolling rows during terminal shrink because the transcript replay will soon
clear and rebuild the visible history. Scrolling first would move the viewport
once, then replay history into a mismatched row.

### Transcript Replay

When the debounce deadline arrives, `maybe_run_resize_reflow()` calls
`reflow_transcript_now()`.

The replay steps are:

1. Compute the wrap width:

   ```rust
   let terminal_width = tui.terminal.size()?.width;
   let width = self.chat_widget.history_wrap_width(terminal_width);
   ```

2. Render transcript cells into display lines at that width.
3. Drop pending history inserts that may have old wrapping.
4. Clear Codex-owned terminal output:
   - alternate screen: clear visible screen
   - inline mode: hard-reset scrollback and visible screen
5. Move the viewport to the top if needed.
6. Insert reflowed transcript lines into terminal history.
7. Mark the terminal width that was actually rebuilt.

The important source-of-truth rule is that replay uses `transcript_cells`, not
terminal text. The terminal cannot reliably rewrap lines that have already been
written into normal scrollback.

### Row Caps

The reference does not always replay the entire transcript. It computes a
terminal-specific row cap with `resize_reflow_cap.rs`.

The config type is:

```rust
pub enum TerminalResizeReflowMaxRows {
    Auto,
    Disabled,
    Limit(usize),
}
```

The TOML setting is `tui.terminal_resize_reflow_max_rows`:

- omitted: use terminal-specific auto cap
- `0`: keep all rendered rows
- positive number: keep at most that many rendered rows

The auto caps are conservative and approximate common terminal scrollback
defaults. This avoids spending work replaying rows the terminal would not retain
anyway.

### Active Stream Width Changes

The reference handles active streaming output separately from finalized
scrollback.

On terminal resize, `chat_widget.on_terminal_resize(width)` updates
`last_rendered_width` and calls `set_width(...)` on active stream controllers.
This happens even when terminal resize reflow is disabled, so live stream
wrapping still tracks the viewport.

`StreamCore::set_width()` re-renders the accumulated source at the new width and
rebuilds the stable queue from the number of already-emitted stable lines. The
reference deliberately avoids byte-level remapping. Finalized content is repaired
later by source-backed transcript consolidation and resize reflow.

The reference also tracks whether resize work happened during streaming or while
transient stream cells were still waiting for consolidation. After consolidation,
it forces one final source-backed reflow so scrollback reflects finalized cells,
not transient stream rows.

## Comparison With Poe

Poe already implements the custom-terminal foundation and two-regime viewport
model:

- `custom_terminal::Terminal` owns a mutable `viewport_area`.
- `set_viewport_area()` resizes both buffers.
- `draw()` calls `autoresize()`.
- `InlineTerminal::draw()` samples `terminal.size()` each frame.
- `draw_with_height()` sets `area.width = size.width`.
- `draw_with_height()` sets `area.height = height.min(size.height)`.
- `AppState::desired_viewport_height(width, screen_height)` recomputes desired
  height from current width and current screen height.
- `reconcile_bottom_pinned()` coordinates queued history insertion with viewport
  height changes.

So Poe already supports **viewport width and height resizing** for future frames.
If the terminal width changes, the next draw uses the new width. If the terminal
height changes, the next draw computes a new height cap and viewport position.

Poe's current gaps relative to the reference are:

1. **No source-backed transcript scrollback.**
   `ConversationLog::take_history_lines(width)` drains queued strings and wraps
   them once. After they are written into terminal scrollback, Poe has no retained
   source model capable of rebuilding the already-written transcript at a new
   width.

2. **No resize-reflow scheduler.**
   Poe receives `CrosstermEvent::Resize`, but it treats it as a redraw trigger.
   There is no state equivalent to `TranscriptReflowState` tracking observed
   width, reflowed width, debounce deadlines, or stream-time repair flags.

3. **No scrollback clear-and-replay path.**
   Poe has scroll-region insertion for incremental history, but no path that
   clears Poe-owned scrollback and re-inserts all retained transcript rows at the
   current width.

4. **No cursor-position resize heuristic.**
   The reference legacy path can offset the inline viewport if terminal resize
   changes the cursor row. Poe does not currently probe cursor position during
   resize to keep the inline anchor aligned.

5. **No active stream controller rewrap.**
   Poe's live rendering is recalculated at the current frame width, so visible
   live text adapts on redraw. But Poe does not have a source-backed streaming
   controller that can rebuild queued stable output after a width change while
   preserving emitted-versus-pending boundaries.

6. **No row cap policy.**
   Because Poe does not replay scrollback, it also has no need yet for
   terminal-specific row caps. If resize reflow is added, a cap will matter for
   performance and for matching terminal scrollback retention.

## Practical Implementation Lessons For Poe

If Poe wants reference-level width and height resize behavior, the work is larger
than just changing `custom_terminal.rs`. The reference relies on a retained
conversation model that can render history cells at any width.

A minimal path would be:

1. Keep `ConversationLog` source-backed.
   Store finalized items in a retained transcript list instead of only draining
   strings into terminal scrollback.

2. Add a resize-reflow state machine.
   Track last observed width, last reflow width, pending deadline, and whether a
   reflow happened during streaming.

3. On `InputEvent::Resize`, schedule resize-sensitive pre-render work.
   Poe can initially treat `Draw` and `Resize` together, but it needs a
   pre-render phase that samples `terminal.size()` before `terminal.draw()`
   updates `last_known_screen_size`.

4. For width changes, debounce and replay.
   Clear Poe-owned scrollback, render retained transcript source at the new
   width, and insert those rows above the viewport.

5. For height changes, decide whether to replay.
   The reference schedules reflow for height changes too because rows can be
   exposed or shifted around the inline viewport. Poe's two-regime logic handles
   many height changes, but a full source-backed replay is safer after terminal
   height changes once retained transcript source exists.

6. Keep active streams source-backed.
   Streaming output should preserve raw source so it can be re-rendered at a new
   width. Final stream consolidation should schedule one final reflow if resize
   happened mid-stream.

7. Add row caps before enabling large-session replay.
   Without a cap, drag-resizing a terminal during a long session could repeatedly
   format and replay thousands of rows.

## Bottom Line

The reference supports resizing at two levels:

- **Immediate viewport resizing:** every draw resizes the mutable viewport to the
  current terminal width and requested height, resizes both ratatui buffers, and
  bottom-aligns when needed.
- **Source-backed scrollback reflow:** terminal resize events schedule a debounced
  rebuild that clears Codex-owned terminal history and replays transcript cells
  at the new width and height context.

Poe currently has the immediate viewport resizing layer. It does not yet have
the retained transcript and replay machinery needed to make previously emitted
scrollback fully width- and height-resize aware.
