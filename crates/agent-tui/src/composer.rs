//! The prompt input buffer: text editing plus the geometry used to lay the
//! text out inside the composer box and place the cursor.

use ratatui::layout::{Position, Rect};
use unicode_width::UnicodeWidthChar;

/// Left gutter rendered before composer text.
pub(crate) const COMPOSER_LEFT_PREFIX: &str = "> ";

/// The editable prompt buffer and the layout math for rendering it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Composer {
    text: String,
    cursor: usize,
    desired_col: Option<usize>,
    scroll_row: usize,
}

impl Composer {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    #[cfg(test)]
    pub(crate) fn text(&self) -> &str {
        &self.text
    }

    #[cfg(test)]
    pub(crate) fn cursor(&self) -> usize {
        self.cursor
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    pub(crate) fn clear(&mut self) {
        self.text.clear();
        self.cursor = 0;
        self.desired_col = None;
        self.scroll_row = 0;
    }

    pub(crate) fn insert_text(&mut self, text: &str) {
        self.text.insert_str(self.cursor, text);
        self.cursor += text.len();
        self.after_edit();
    }

    pub(crate) fn insert_char(&mut self, ch: char) {
        self.text.insert(self.cursor, ch);
        self.cursor += ch.len_utf8();
        self.after_edit();
    }

    pub(crate) fn backspace(&mut self) {
        let Some(previous) = previous_boundary(&self.text, self.cursor) else {
            return;
        };
        self.text.replace_range(previous..self.cursor, "");
        self.cursor = previous;
        self.after_edit();
    }

    pub(crate) fn backspace_word(&mut self) {
        let mut start = self.cursor;
        while let Some((previous, ch)) = previous_char(&self.text, start) {
            if !ch.is_whitespace() {
                break;
            }
            start = previous;
        }
        while let Some((previous, ch)) = previous_char(&self.text, start) {
            if ch.is_whitespace() {
                break;
            }
            start = previous;
        }
        if start != self.cursor {
            self.text.replace_range(start..self.cursor, "");
            self.cursor = start;
            self.after_edit();
        }
    }

    pub(crate) fn backspace_to_line_start(&mut self) {
        let line_start = self.text[..self.cursor]
            .rfind('\n')
            .map(|idx| idx + '\n'.len_utf8())
            .unwrap_or(0);
        if line_start != self.cursor {
            self.text.replace_range(line_start..self.cursor, "");
            self.cursor = line_start;
            self.after_edit();
        }
    }

    pub(crate) fn move_left(&mut self) {
        if let Some(previous) = previous_boundary(&self.text, self.cursor) {
            self.cursor = previous;
        }
        self.desired_col = None;
    }

    pub(crate) fn move_right(&mut self) {
        if let Some(ch) = self.text[self.cursor..].chars().next() {
            self.cursor += ch.len_utf8();
        }
        self.desired_col = None;
    }

    pub(crate) fn move_up(&mut self, text_width: u16, visible_rows: u16) {
        self.move_vertically(text_width, visible_rows, VerticalDirection::Up);
    }

    pub(crate) fn move_down(&mut self, text_width: u16, visible_rows: u16) {
        self.move_vertically(text_width, visible_rows, VerticalDirection::Down);
    }

    /// Take the trimmed prompt and clear the buffer, or `None` when blank.
    pub(crate) fn take_prompt(&mut self) -> Option<String> {
        let prompt = self.text.trim().to_string();
        if prompt.is_empty() {
            return None;
        }

        self.clear();
        Some(prompt)
    }

    /// Height of the composer box (borders included) for a given outer width.
    pub(crate) fn height(&self, width: u16) -> u16 {
        let inner_width = self.text_width(width).max(1);
        (self.layout(inner_width, u16::MAX).rows.len() as u16 + 2).clamp(3, 8)
    }

    /// Cursor position within `area`.
    pub(crate) fn cursor_position(&self, area: Rect) -> Position {
        let text_area = self.text_area(area);
        let text_width = text_area.width.max(1);
        let text_height = text_area.height.max(1);
        let layout = self.layout(text_width, text_height);
        let row = layout
            .cursor_row
            .saturating_sub(layout.visible_start_row)
            .min(text_height.saturating_sub(1) as usize);
        let col = layout.cursor_col.min(text_width.saturating_sub(1) as usize);

        Position {
            x: text_area.x + col as u16,
            y: text_area.y + row as u16,
        }
    }

    /// The inner text rect, below the top rule and after the left prefix
    /// gutter. The composer has no side borders, so text starts flush left.
    pub(crate) fn text_area(&self, area: Rect) -> Rect {
        let prefix_width = COMPOSER_LEFT_PREFIX.chars().count() as u16;
        Rect::new(
            area.x + prefix_width.min(area.width),
            area.y + 1.min(area.height),
            area.width.saturating_sub(prefix_width),
            area.height.saturating_sub(2),
        )
    }

    pub(crate) fn text_width(&self, outer_width: u16) -> u16 {
        let prefix_width = COMPOSER_LEFT_PREFIX.chars().count() as u16;
        outer_width.saturating_sub(prefix_width)
    }

    pub(crate) fn layout(&self, text_width: u16, visible_rows: u16) -> ComposerLayout<'_> {
        build_layout(
            &self.text,
            self.cursor,
            text_width.max(1) as usize,
            self.scroll_row,
            visible_rows.max(1) as usize,
        )
    }

    fn move_vertically(
        &mut self,
        text_width: u16,
        visible_rows: u16,
        direction: VerticalDirection,
    ) {
        let (new_cursor, new_scroll_row, desired_col) = {
            let layout = self.layout(text_width, visible_rows);
            let desired_col = self.desired_col.unwrap_or(layout.cursor_col);
            let (target_row, new_cursor, desired_col) = if let Some(target_row) =
                direction.target_row(layout.cursor_row, layout.rows.len())
            {
                (
                    target_row,
                    layout.byte_for_row_col(target_row, desired_col),
                    Some(desired_col),
                )
            } else {
                (layout.cursor_row, direction.boundary_cursor(&layout), None)
            };
            let new_scroll_row = scroll_row_for_cursor(
                target_row,
                self.scroll_row,
                visible_rows as usize,
                layout.rows.len(),
            );
            if new_cursor == self.cursor && desired_col == self.desired_col {
                return;
            }
            (new_cursor, new_scroll_row, desired_col)
        };

        self.cursor = new_cursor;
        self.scroll_row = new_scroll_row;
        self.desired_col = desired_col;
    }

    fn after_edit(&mut self) {
        self.desired_col = None;
        if self.text.is_empty() {
            self.scroll_row = 0;
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum VerticalDirection {
    Up,
    Down,
}

impl VerticalDirection {
    fn target_row(self, cursor_row: usize, row_count: usize) -> Option<usize> {
        match self {
            Self::Up => cursor_row.checked_sub(1),
            Self::Down => {
                let target = cursor_row + 1;
                (target < row_count).then_some(target)
            }
        }
    }

    fn boundary_cursor(self, layout: &ComposerLayout<'_>) -> usize {
        match self {
            Self::Up => layout
                .rows
                .get(layout.cursor_row)
                .map(|row| row.start_byte)
                .unwrap_or(0),
            Self::Down => layout
                .rows
                .get(layout.cursor_row)
                .map(|row| row.end_byte)
                .unwrap_or(0),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ComposerLayout<'a> {
    pub(crate) rows: Vec<VisualRow<'a>>,
    pub(crate) cursor_row: usize,
    pub(crate) cursor_col: usize,
    pub(crate) visible_start_row: usize,
    pub(crate) visible_end_row: usize,
}

impl<'a> ComposerLayout<'a> {
    pub(crate) fn visible_rows(&self) -> &[VisualRow<'a>] {
        &self.rows[self.visible_start_row..self.visible_end_row]
    }

    fn byte_for_row_col(&self, row_idx: usize, target_col: usize) -> usize {
        self.rows
            .get(row_idx)
            .map(|row| row.byte_for_col(target_col))
            .unwrap_or_else(|| self.rows.last().map(|row| row.end_byte).unwrap_or(0))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct VisualRow<'a> {
    pub(crate) text: &'a str,
    pub(crate) start_byte: usize,
    pub(crate) end_byte: usize,
    pub(crate) next_byte: usize,
    pub(crate) width: usize,
    pub(crate) break_kind: RowBreak,
    checkpoints: Vec<CellCheckpoint>,
}

impl VisualRow<'_> {
    fn col_for_byte(&self, byte: usize) -> usize {
        self.checkpoints
            .iter()
            .find(|checkpoint| checkpoint.byte == byte)
            .map(|checkpoint| checkpoint.col)
            .unwrap_or(self.width)
    }

    fn byte_for_col(&self, target_col: usize) -> usize {
        let mut best = self.start_byte;
        let mut best_distance = usize::MAX;
        for checkpoint in &self.checkpoints {
            let distance = checkpoint.col.abs_diff(target_col);
            if distance <= best_distance {
                best = checkpoint.byte;
                best_distance = distance;
            }
        }
        best.min(self.end_byte)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RowBreak {
    Wrap,
    Newline,
    End,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct CellCheckpoint {
    byte: usize,
    col: usize,
}

fn build_layout<'a>(
    text: &'a str,
    cursor: usize,
    width: usize,
    scroll_row: usize,
    visible_rows: usize,
) -> ComposerLayout<'a> {
    let width = width.max(1);
    let cursor = cursor.min(text.len());
    debug_assert!(text.is_char_boundary(cursor));

    let mut rows = Vec::new();
    let mut row_start = 0usize;
    let mut row_width = 0usize;
    let mut checkpoints = vec![CellCheckpoint { byte: 0, col: 0 }];

    for (idx, ch) in text.char_indices() {
        if ch == '\n' {
            push_row(
                &mut rows,
                text,
                RowSpec {
                    start_byte: row_start,
                    end_byte: idx,
                    next_byte: idx + ch.len_utf8(),
                    width: row_width,
                    break_kind: RowBreak::Newline,
                    checkpoints,
                },
            );
            row_start = idx + ch.len_utf8();
            row_width = 0;
            checkpoints = vec![CellCheckpoint {
                byte: row_start,
                col: 0,
            }];
            continue;
        }

        let ch_width = char_width(ch);
        if row_width > 0 && row_width + ch_width > width {
            push_row(
                &mut rows,
                text,
                RowSpec {
                    start_byte: row_start,
                    end_byte: idx,
                    next_byte: idx,
                    width: row_width,
                    break_kind: RowBreak::Wrap,
                    checkpoints,
                },
            );
            row_start = idx;
            row_width = 0;
            checkpoints = vec![CellCheckpoint { byte: idx, col: 0 }];
        }

        row_width += ch_width;
        checkpoints.push(CellCheckpoint {
            byte: idx + ch.len_utf8(),
            col: row_width,
        });
    }

    push_row(
        &mut rows,
        text,
        RowSpec {
            start_byte: row_start,
            end_byte: text.len(),
            next_byte: text.len(),
            width: row_width,
            break_kind: RowBreak::End,
            checkpoints,
        },
    );

    if rows.is_empty() {
        push_row(
            &mut rows,
            text,
            RowSpec {
                start_byte: 0,
                end_byte: 0,
                next_byte: 0,
                width: 0,
                break_kind: RowBreak::End,
                checkpoints: vec![CellCheckpoint { byte: 0, col: 0 }],
            },
        );
    }

    let (cursor_row, cursor_col) = cursor_row_col(&rows, cursor);
    let visible_start_row =
        visible_start_row(rows.len(), cursor_row, scroll_row, visible_rows.max(1));
    let visible_end_row = (visible_start_row + visible_rows.max(1)).min(rows.len());

    ComposerLayout {
        rows,
        cursor_row,
        cursor_col,
        visible_start_row,
        visible_end_row,
    }
}

struct RowSpec {
    start_byte: usize,
    end_byte: usize,
    next_byte: usize,
    width: usize,
    break_kind: RowBreak,
    checkpoints: Vec<CellCheckpoint>,
}

fn push_row<'a>(rows: &mut Vec<VisualRow<'a>>, text: &'a str, spec: RowSpec) {
    rows.push(VisualRow {
        text: &text[spec.start_byte..spec.end_byte],
        start_byte: spec.start_byte,
        end_byte: spec.end_byte,
        next_byte: spec.next_byte,
        width: spec.width,
        break_kind: spec.break_kind,
        checkpoints: spec.checkpoints,
    });
}

fn cursor_row_col(rows: &[VisualRow<'_>], cursor: usize) -> (usize, usize) {
    for (idx, row) in rows.iter().enumerate() {
        if cursor < row.start_byte {
            break;
        }
        if cursor < row.end_byte {
            return (idx, row.col_for_byte(cursor));
        }
        if cursor == row.end_byte {
            if row.break_kind == RowBreak::Wrap && rows.get(idx + 1).is_some() {
                continue;
            }
            return (idx, row.col_for_byte(cursor));
        }
    }

    rows.last()
        .map(|row| (rows.len() - 1, row.col_for_byte(row.end_byte)))
        .unwrap_or((0, 0))
}

fn visible_start_row(
    row_count: usize,
    cursor_row: usize,
    scroll_row: usize,
    visible_rows: usize,
) -> usize {
    let max_start = row_count.saturating_sub(visible_rows);
    let mut start = scroll_row.min(max_start);
    if cursor_row < start {
        start = cursor_row;
    } else if cursor_row >= start + visible_rows {
        start = cursor_row + 1 - visible_rows;
    }
    start.min(max_start)
}

fn scroll_row_for_cursor(
    cursor_row: usize,
    scroll_row: usize,
    visible_rows: usize,
    row_count: usize,
) -> usize {
    visible_start_row(row_count, cursor_row, scroll_row, visible_rows.max(1))
}

fn previous_boundary(text: &str, cursor: usize) -> Option<usize> {
    text[..cursor]
        .char_indices()
        .next_back()
        .map(|(idx, _)| idx)
}

fn previous_char(text: &str, cursor: usize) -> Option<(usize, char)> {
    text[..cursor].char_indices().next_back()
}

fn char_width(ch: char) -> usize {
    UnicodeWidthChar::width(ch).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn submit_trims_clears_and_rejects_empty_input() {
        let mut composer = Composer::new();

        composer.insert_text("  hello  ");
        assert_eq!(composer.take_prompt(), Some("hello".to_string()));
        assert_eq!(composer.text(), "");
        assert_eq!(composer.cursor(), 0);
        assert_eq!(composer.take_prompt(), None);
    }

    #[test]
    fn insert_char_inserts_at_cursor() {
        let mut composer = Composer::new();

        composer.insert_text("helo");
        composer.move_left();
        composer.insert_char('l');

        assert_eq!(composer.text(), "hello");
        assert_eq!(composer.cursor(), 4);
    }

    #[test]
    fn insert_text_inserts_at_cursor() {
        let mut composer = Composer::new();

        composer.insert_text("hello world");
        for _ in 0..5 {
            composer.move_left();
        }
        composer.insert_text("small ");

        assert_eq!(composer.text(), "hello small world");
    }

    #[test]
    fn backspace_deletes_one_character_before_cursor() {
        let mut composer = Composer::new();

        composer.insert_text("helo");
        composer.move_left();
        composer.backspace();

        assert_eq!(composer.text(), "heo");
        assert_eq!(composer.cursor(), 2);
    }

    #[test]
    fn backspace_word_deletes_trailing_space_and_previous_word() {
        let mut composer = Composer::new();

        composer.insert_text("hello world   ");
        composer.backspace_word();

        assert_eq!(composer.text(), "hello ");
        assert_eq!(composer.cursor(), "hello ".len());
    }

    #[test]
    fn backspace_word_handles_unicode() {
        let mut composer = Composer::new();

        composer.insert_text("hello café");
        composer.backspace_word();

        assert_eq!(composer.text(), "hello ");
        assert_eq!(composer.cursor(), "hello ".len());
    }

    #[test]
    fn backspace_to_line_start_clears_single_line() {
        let mut composer = Composer::new();

        composer.insert_text("hello world");
        composer.backspace_to_line_start();

        assert_eq!(composer.text(), "");
        assert_eq!(composer.cursor(), 0);
    }

    #[test]
    fn backspace_to_line_start_keeps_previous_lines() {
        let mut composer = Composer::new();

        composer.insert_text("first line\nsecond line");
        composer.backspace_to_line_start();

        assert_eq!(composer.text(), "first line\n");
        assert_eq!(composer.cursor(), "first line\n".len());
    }

    #[test]
    fn left_and_right_move_across_unicode_boundaries() {
        let mut composer = Composer::new();

        composer.insert_text("aé文");
        composer.move_left();
        assert_eq!(composer.cursor(), "aé".len());
        composer.move_left();
        assert_eq!(composer.cursor(), "a".len());
        composer.move_right();
        assert_eq!(composer.cursor(), "aé".len());
    }

    #[test]
    fn cursor_tracks_input_end_inside_border() {
        let mut composer = Composer::new();
        let area = Rect::new(10, 20, 12, 5);

        assert_eq!(composer.cursor_position(area), Position { x: 12, y: 21 });

        composer.insert_text("abcd");
        assert_eq!(composer.cursor_position(area), Position { x: 16, y: 21 });

        composer.insert_text("\nef");
        assert_eq!(composer.cursor_position(area), Position { x: 14, y: 22 });
    }

    #[test]
    fn cursor_wraps_and_scrolls_to_visible_area() {
        let mut composer = Composer::new();
        let area = Rect::new(0, 0, 10, 4);

        composer.insert_text("abcdef");
        assert_eq!(composer.cursor_position(area), Position { x: 8, y: 1 });

        composer.insert_text("\nmore\nlines");
        assert_eq!(composer.cursor_position(area), Position { x: 7, y: 2 });
    }

    #[test]
    fn layout_preserves_trailing_newline() {
        let mut composer = Composer::new();

        composer.insert_text("hello\n");
        let layout = composer.layout(20, 10);

        assert_eq!(layout.rows.len(), 2);
        assert_eq!(layout.rows[0].text, "hello");
        assert_eq!(layout.rows[0].break_kind, RowBreak::Newline);
        assert_eq!(layout.rows[1].text, "");
        assert_eq!(layout.cursor_row, 1);
        assert_eq!(layout.cursor_col, 0);
    }

    #[test]
    fn layout_wraps_on_terminal_cell_width() {
        let mut composer = Composer::new();

        composer.insert_text("ab文c");
        let layout = composer.layout(4, 10);

        assert_eq!(
            layout.rows.iter().map(|row| row.text).collect::<Vec<_>>(),
            vec!["ab文", "c"]
        );
    }

    #[test]
    fn move_up_and_down_cross_visual_rows() {
        let mut composer = Composer::new();

        composer.insert_text("abcdefghi");
        composer.move_up(4, 3);
        assert_eq!(composer.cursor(), 5);
        composer.move_down(4, 3);
        assert_eq!(composer.cursor(), 9);
    }

    #[test]
    fn repeated_vertical_movement_preserves_desired_column() {
        let mut composer = Composer::new();

        composer.insert_text("abcdef\nxy\n123456");
        composer.move_up(20, 8);
        assert_eq!(composer.cursor(), "abcdef\nxy".len());
        composer.move_up(20, 8);
        assert_eq!(composer.cursor(), "abcdef".len());
        composer.move_down(20, 8);
        assert_eq!(composer.cursor(), "abcdef\nxy".len());
        composer.move_down(20, 8);
        assert_eq!(composer.cursor(), "abcdef\nxy\n123456".len());
    }

    #[test]
    fn up_on_top_row_moves_to_row_start() {
        let mut composer = Composer::new();

        composer.insert_text("xy\nabcdef");
        composer.move_up(20, 8);
        assert_eq!(composer.cursor(), "xy".len());
        composer.move_up(20, 8);
        assert_eq!(composer.cursor(), 0);

        composer.move_down(20, 8);
        assert_eq!(composer.cursor(), "xy\n".len());
    }

    #[test]
    fn down_on_bottom_row_moves_to_row_end() {
        let mut composer = Composer::new();

        composer.insert_text("abcdef\nxyz");
        composer.move_left();
        composer.move_left();
        assert_eq!(composer.cursor(), "abcdef\nx".len());

        composer.move_down(20, 8);

        assert_eq!(composer.cursor(), "abcdef\nxyz".len());
    }

    #[test]
    fn text_area_accounts_for_prefix() {
        let composer = Composer::new();

        assert_eq!(
            composer.text_area(Rect::new(10, 20, 20, 5)),
            Rect::new(12, 21, 18, 3)
        );
    }
}
