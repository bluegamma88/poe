# Composer Navigation Plan

The current composer is intentionally small: it stores only a `String` and all
editing happens at the end of the buffer. Supporting left, right, up, and down
arrow keys changes the composer from an append-only prompt into a small text
editor. The important design goal is to keep one source of truth for how text
wraps, where the cursor is, and which rows are visible.

## Current state

`Composer` currently owns only:

```rust
pub(crate) struct Composer {
    text: String,
}
```

That means these operations all assume the cursor is at the end:

- `insert_text` appends to `text`
- `insert_char` appends to `text`
- `backspace` pops the final character
- `backspace_word` trims from the end
- `backspace_to_line_start` truncates the final logical line
- `cursor_position` computes the end position of the whole string

There are also three separate layout approximations:

- `visual_line_count()` estimates composer height.
- `text_end_position()` estimates the cursor position.
- `Paragraph::wrap()` performs the actual render wrapping.

Those must become one shared layout model before vertical cursor movement can
feel correct.

## Persistent state

Persistent state lives on `Composer`. These fields survive across frames and
input events.

```rust
pub(crate) struct Composer {
    text: String,
    cursor: usize,
    desired_col: Option<usize>,
    scroll_row: usize,
}
```

### `text`

The prompt buffer. This remains the only source of truth for prompt content.

The buffer should continue to be UTF-8 text. All cursor offsets stored in the
composer must be byte offsets that are valid UTF-8 character boundaries.

### `cursor`

The insertion point as a byte offset into `text`.

This replaces the current "cursor is always at the end" assumption. All editing
methods should operate at this byte offset:

- `insert_char` inserts at `cursor`, then advances `cursor`.
- `insert_text` inserts at `cursor`, then advances `cursor` by the inserted
  byte length.
- `backspace` deletes the previous scalar value and moves `cursor` left.
- `backspace_word` deletes the word before `cursor`.
- `backspace_to_line_start` deletes from the current logical line start to
  `cursor`.
- `take_prompt` trims/submits the whole prompt and resets `cursor` to `0`.
- `clear` resets `text`, `cursor`, `desired_col`, and `scroll_row`.

### `desired_col`

The target terminal-cell column for repeated vertical movement.

When the user presses Up or Down, the first vertical move records the current
visual column in `desired_col`. Further Up/Down moves try to keep landing on
that column, even when crossing shorter visual rows. Any horizontal movement or
text edit clears `desired_col`.

This is the usual text-editor behavior:

```text
abcdef
xy
123456
```

If the cursor starts after `f`, Down lands after `y`, but another Down should
return to column 6 on the third row rather than column 2.

### `scroll_row`

The first visual row currently shown inside the composer text area.

The composer height is capped, so long input needs composer-local scrolling.
Without this field, the cursor can move below the visible composer and
`cursor_position()` can only clamp it to the last visible row.

`scroll_row` should be adjusted after every edit, movement, width change, or
height change so the cursor is visible:

```rust
fn ensure_cursor_visible(&mut self, layout: &ComposerLayout, visible_rows: usize) {
    if layout.cursor_row < self.scroll_row {
        self.scroll_row = layout.cursor_row;
    } else if layout.cursor_row >= self.scroll_row + visible_rows {
        self.scroll_row = layout.cursor_row + 1 - visible_rows;
    }
}
```

## Derived layout state

Layout state should be computed from `text`, `cursor`, the text area width, and
the visible text height. It should not be stored permanently except for
`scroll_row`, which is user-visible viewport state.

```rust
pub(crate) struct ComposerLayout<'a> {
    rows: Vec<VisualRow<'a>>,
    cursor_row: usize,
    cursor_col: usize,
    visible_start_row: usize,
    visible_end_row: usize,
}
```

`ComposerLayout` is the shared answer to all geometry questions:

- how many visual rows the prompt occupies
- which visual rows should be rendered
- where the cursor appears
- how Up/Down map a row/column back to a byte offset
- how tall the composer should be before clamping

### `VisualRow`

Each visual row represents one rendered row after applying hard newlines and
soft wrapping.

```rust
pub(crate) struct VisualRow<'a> {
    text: &'a str,
    start_byte: usize,
    end_byte: usize,
    next_byte: usize,
    width: usize,
    break_kind: RowBreak,
    checkpoints: Vec<CellCheckpoint>,
}

pub(crate) enum RowBreak {
    Wrap,
    Newline,
    End,
}

pub(crate) struct CellCheckpoint {
    byte: usize,
    col: usize,
}
```

Field meanings:

- `text` is the renderable slice for this row. It never includes a hard
  newline.
- `start_byte` is the byte offset where the row begins.
- `end_byte` is the byte offset where visible row text ends.
- `next_byte` is the next cursor position after this row. For a hard newline,
  this is after the newline byte; for a wrap or end row, it normally equals
  `end_byte`.
- `width` is the terminal-cell width of `text`.
- `break_kind` records whether the row ended because of soft wrap, hard
  newline, or end of input.
- `checkpoints` map byte offsets to terminal-cell columns within the row.

The distinction between `end_byte` and `next_byte` is important for trailing
newlines. For `text == "hello\n"`, layout should produce:

```text
row 0: "hello", break_kind = Newline, end_byte before '\n', next_byte after '\n'
row 1: "",      break_kind = End,     cursor can appear at col 0
```

This avoids the current mismatch where `str::lines()` drops the trailing empty
line but cursor math sees it.

## Width model

Wrapping and cursor movement should use terminal-cell width, not
`chars().count()`.

The `agent-tui` crate should add a direct dependency on `unicode-width` and use
it when building `VisualRow` checkpoints. Ratatui already depends on
`unicode-width`, but direct use should be represented explicitly in
`crates/agent-tui/Cargo.toml`.

The first implementation can operate on Unicode scalar values while respecting
their terminal width. A later improvement could move to grapheme-cluster
navigation if emoji sequences or combining marks need better behavior.

## Layout builder

The layout builder should be a pure helper that does not mutate `Composer`.

```rust
impl Composer {
    pub(crate) fn layout(&self, text_width: u16, visible_rows: u16) -> ComposerLayout<'_> {
        build_layout(
            &self.text,
            self.cursor,
            text_width.max(1) as usize,
            self.scroll_row,
            visible_rows.max(1) as usize,
        )
    }
}
```

Responsibilities:

1. Split text into visual rows using hard newlines and soft wraps.
2. Preserve empty logical lines and trailing empty lines.
3. Track byte offsets only at valid UTF-8 boundaries.
4. Track terminal-cell columns for each boundary.
5. Map `cursor` to `cursor_row` and `cursor_col`.
6. Clamp `visible_start_row` from `scroll_row`.
7. Compute `visible_end_row` from the visible height.

The builder should guarantee there is always at least one row, even when the
composer is empty.

## Editing operations

Editing operations should mutate persistent state, then normalize it.

Common post-edit steps:

1. Clamp `cursor` to `text.len()`.
2. Ensure `cursor` is on a UTF-8 character boundary.
3. Clear `desired_col`.
4. Recompute layout at the current width during the next render.
5. Adjust `scroll_row` so the cursor is visible.

The composer methods do not currently know the render width. There are two
reasonable options:

- Keep editing methods width-independent and call `ensure_cursor_visible` from
  the render/input layer once the text area width is known.
- Pass the text area width and height into movement methods that need layout,
  such as `move_up`, `move_down`, and a post-edit visibility helper.

The second option keeps visibility updates close to the operations that need
them, but it means `handle_key` must provide composer text-area dimensions.

## Movement operations

### Left and Right

Left and right only need UTF-8 character boundaries.

```rust
pub(crate) fn move_left(&mut self) {
    if let Some((idx, _)) = self.text[..self.cursor].char_indices().next_back() {
        self.cursor = idx;
    }
    self.desired_col = None;
}

pub(crate) fn move_right(&mut self) {
    if self.cursor < self.text.len() {
        let ch = self.text[self.cursor..].chars().next().expect("cursor boundary");
        self.cursor += ch.len_utf8();
    }
    self.desired_col = None;
}
```

### Up and Down

Up and down use `ComposerLayout`.

Algorithm:

1. Build layout at the current text width.
2. If `desired_col` is `None`, set it to `layout.cursor_col`.
3. Pick the target row: `cursor_row - 1` for Up, `cursor_row + 1` for Down.
4. Clamp if the target row does not exist.
5. Convert `(target_row, desired_col)` to the nearest byte offset in that row.
6. Set `cursor` to that byte offset.
7. Update `scroll_row` so the new cursor row is visible.

The row/column to byte mapping should use `VisualRow.checkpoints`. For target
columns beyond the end of a short row, return the row end. For a hard-newline
row, the row end should mean before the newline; Down/Right can still move
after the newline through normal movement.

## Rendering interaction

`render_composer` should render pre-wrapped rows from `ComposerLayout` instead
of passing logical lines to `Paragraph::wrap()`.

Current flow:

```text
Composer.text()
  -> str::lines()
  -> Paragraph::wrap()
```

Target flow:

```text
Composer.layout(width, visible_height)
  -> visible VisualRow.text slices
  -> Paragraph without wrap
```

This keeps rendering, cursor placement, and vertical movement on the same
model. `Paragraph::wrap()` should not perform a second independent wrap pass.

## Height interaction

`Composer::height(width)` should also use the shared layout builder.

```rust
pub(crate) fn height(&self, width: u16) -> u16 {
    let text_width = self.text_width(width).max(1);
    let layout = self.layout(text_width, u16::MAX);
    (layout.rows.len() as u16 + 2).clamp(3, 8)
}
```

The visible text height is then:

```rust
let visible_rows = composer.height(area.width).saturating_sub(2);
```

Those `visible_rows` should be passed back into layout for rendering and cursor
placement.

## Input interaction

`handle_key` should route arrow keys into composer movement methods.

```rust
KeyCode::Left => {
    app.composer.move_left();
    Ok(false)
}
KeyCode::Right => {
    app.composer.move_right();
    Ok(false)
}
KeyCode::Up => {
    app.composer.move_up(text_width, visible_rows);
    Ok(false)
}
KeyCode::Down => {
    app.composer.move_down(text_width, visible_rows);
    Ok(false)
}
```

`handle_key` currently does not receive the current render area. To support
width-aware Up/Down, the app should keep enough terminal geometry to compute
the composer text width for input handling, or the input handler should receive
the latest viewport area from `InlineTerminal`.

The simplest data flow is:

```text
InlineTerminal::draw
  -> records latest viewport width/height on AppState

handle_key
  -> AppState computes latest composer text width/visible rows
  -> Composer movement uses those dimensions
```

## Ownership boundaries

Keep the responsibilities separated:

| Owner | State | Responsibility |
| --- | --- | --- |
| `Composer` | `text`, `cursor`, `desired_col`, `scroll_row` | Persistent editor state and editing commands |
| `ComposerLayout` | visual rows, cursor row/col, visible range | Derived geometry for one width/height |
| `VisualRow` | byte ranges and cell checkpoints | Mapping between text offsets and screen columns |
| `AppState` | latest viewport dimensions | Supplies width/height context to composer movement |
| `render_composer` | no persistent state | Renders the visible rows produced by layout |
| `InlineTerminal` | terminal viewport rect | Supplies actual terminal geometry and draws frames |

`Composer` should not know about terminal scrollback, live output, or footer
layout. `InlineTerminal` should not know how text wrapping works inside the
composer. The only shared contract is the composer text-area rectangle.

## Invariants

The implementation should preserve these invariants:

- `cursor <= text.len()`.
- `cursor` is always a UTF-8 character boundary.
- `scroll_row` is always less than the number of visual rows, unless the
  composer is empty, where it should be `0`.
- Layout always returns at least one visual row.
- Rendering uses the same visual rows as cursor positioning.
- Up/Down operate on visual rows, not only hard-newline logical lines.
- Any edit or horizontal move clears `desired_col`.
- Repeated Up/Down preserves `desired_col`.
- The cursor is always inside the visible composer text area after
  `ensure_cursor_visible`.

## Suggested implementation order

1. Add `cursor` to `Composer` and update insert/backspace operations to use it.
2. Add left/right movement and tests for ASCII and Unicode boundaries.
3. Introduce `VisualRow` and `ComposerLayout` while preserving current render
   behavior.
4. Switch `height()` and `cursor_position()` to use `ComposerLayout`.
5. Switch `render_composer()` to render pre-wrapped visible rows without
   `Paragraph::wrap()`.
6. Add `desired_col` plus Up/Down movement over visual rows.
7. Add `scroll_row` and cursor-visible behavior for inputs longer than the
   composer height cap.
8. Add direct `unicode-width` dependency and use terminal-cell width in row
   building.

## Test coverage

Tests should cover the state model directly before relying on terminal
snapshots.

Core composer tests:

- insert at beginning, middle, and end
- backspace at beginning, middle, and after Unicode text
- left/right across ASCII and multi-byte characters
- hard newline preservation, including trailing newline
- visual wrapping at exact width boundaries
- height and cursor position from the same layout
- Up/Down across hard newlines
- Up/Down across soft-wrapped rows
- repeated Up/Down preserves `desired_col`
- edit after vertical movement clears `desired_col`
- `scroll_row` keeps cursor visible when visual row count exceeds height cap

Integration tests:

- `handle_key` routes arrow keys without submitting the prompt
- rendering does not double-wrap pre-wrapped composer rows
- resizing recomputes layout and keeps the cursor visible
