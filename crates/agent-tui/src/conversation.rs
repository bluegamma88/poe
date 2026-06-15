//! The on-screen conversation log and its live → pending → scrollback
//! pipeline.
//!
//! Output arrives as a stream of [`Item`]s. While an item can still change it
//! stays "live" and is redrawn each frame; once finalized it is promoted into
//! `pending_lines`, then dripped one line per tick into `history_queue`, which
//! the terminal drains into permanent scrollback.
//!
//! Responsibility split with `InlineTerminal`:
//!
//! - `Conversation` is the model. It owns conversation state and the line
//!   pipeline with zero knowledge of the terminal: it ingests agent events into
//!   live items, sheds/promotes them into queued `HistoryLine`s, and paces their
//!   release. Data in, data out — fully testable without a screen.
//! - `InlineTerminal` is the device. It owns the real terminal and does I/O:
//!   it pulls lines out via [`Conversation::take_history_lines`] and writes them
//!   to scrollback, and redraws the live zone each frame. No conversation logic.
//!
//! In short: `Conversation` decides *what* text exists and when it is ready;
//! `InlineTerminal` decides *how* it reaches the screen. The run loop wires the
//! two together.

use agent_protocol::{ToolCall, ToolResult};

use crate::{describe_tool_call, describe_tool_result};

const TOOL_OUTPUT_TAIL_LINES: usize = 3;

/// Visual classification of a scrollback line, used to style it on flush.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LineKind {
    Normal,
    Thinking,
    Dim,
    /// The echoed user prompt. Rendered on a filled background, padded to the
    /// full width, to set it apart from assistant output.
    User,
}

/// A line destined for terminal scrollback, tagged with how it should render.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct HistoryLine {
    pub(crate) text: String,
    pub(crate) kind: LineKind,
}

impl HistoryLine {
    pub(crate) fn normal(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            kind: LineKind::Normal,
        }
    }

    pub(crate) fn thinking(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            kind: LineKind::Thinking,
        }
    }

    pub(crate) fn dim(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            kind: LineKind::Dim,
        }
    }

    pub(crate) fn user(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            kind: LineKind::User,
        }
    }
}

/// A logical unit of conversation output. While an item is "live" it renders in
/// the transient zone (redrawn every frame); once finalized it is promoted into
/// terminal scrollback as a single unit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Item {
    /// Streaming assistant reply. Holds only the trailing partial line —
    /// completed lines are shed into `pending_lines` as they arrive.
    Message(String),
    /// Streaming model reasoning, with the same shedding behavior as `Message`.
    Thinking(String),
    /// A tool call. `result` is `None` while running (renders as a spinner) and
    /// `Some` once finished (promoted with a collapsed output tail).
    Tool {
        call: ToolCall,
        output: String,
        result: Option<ToolResult>,
    },
    /// A one-shot line that is born finalized (patches, errors, markers).
    Notice { text: String, kind: LineKind },
}

impl Item {
    /// Whether this item is finalized and may be promoted out of the live zone.
    /// Streaming text items shed their content eagerly and are removed by
    /// `finalize_trailing_text`, so they never reach the promotion path.
    fn is_done(&self) -> bool {
        match self {
            Item::Tool { result, .. } => result.is_some(),
            Item::Notice { .. } => true,
            Item::Message(_) | Item::Thinking(_) => false,
        }
    }
}

/// The conversation log: live items plus the staged scrollback pipeline.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Conversation {
    /// Ordered active items, rendered in the transient zone.
    pub(crate) live: Vec<Item>,
    /// Finalized lines awaiting the paced drip into `history_queue`.
    pub(crate) pending_lines: Vec<HistoryLine>,
    /// Lines ready to flush into terminal scrollback.
    pub(crate) history_queue: Vec<HistoryLine>,
    /// Animation frame for the running-tool spinner.
    pub(crate) spinner_frame: usize,
}

impl Conversation {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Reset the transient zone for a new turn and echo the user's prompt.
    pub(crate) fn start_turn(&mut self, prompt: &str) {
        self.live.clear();
        self.pending_lines.clear();
        self.history_queue.extend(user_prompt_history_lines(prompt));
    }

    pub(crate) fn push_assistant_delta(&mut self, text: &str) {
        // Reasoning always precedes assistant content, so finalize any trailing
        // thinking item before the reply begins.
        if !matches!(self.live.last(), Some(Item::Message(_))) {
            self.finalize_trailing_text();
            self.live.push(Item::Message(String::new()));
        }
        self.append_streaming_text(text, LineKind::Normal);
    }

    pub(crate) fn push_thinking_delta(&mut self, text: &str) {
        if !matches!(self.live.last(), Some(Item::Thinking(_))) {
            self.finalize_trailing_text();
            self.live.push(Item::Thinking(String::new()));
        }
        self.append_streaming_text(text, LineKind::Thinking);
    }

    pub(crate) fn start_tool(&mut self, call: ToolCall) {
        self.finalize_trailing_text();
        self.live.push(Item::Tool {
            call,
            output: String::new(),
            result: None,
        });
    }

    pub(crate) fn push_tool_output(&mut self, id: &str, chunk: &str) {
        if let Some(Item::Tool { output, .. }) = self.live_tool_mut(id) {
            output.push_str(chunk);
        }
    }

    pub(crate) fn finish_tool(&mut self, id: &str, result: ToolResult) {
        if let Some(Item::Tool { result: slot, .. }) = self.live_tool_mut(id) {
            *slot = Some(result);
        }
        self.promote_done_prefix();
    }

    /// Append a one-shot finalized line (patch notifications and the like).
    pub(crate) fn push_notice(&mut self, text: impl Into<String>) {
        self.live.push(Item::Notice {
            text: text.into(),
            kind: LineKind::Normal,
        });
        self.promote_done_prefix();
    }

    /// Push a line directly into scrollback, bypassing the paced drip. Used for
    /// turn-end markers and debug output that should appear immediately.
    pub(crate) fn push_marker(&mut self, text: impl Into<String>) {
        self.history_queue.push(HistoryLine::normal(text));
    }

    /// Advance the spinner and release one pending line per tick, pacing the
    /// scroll-in of finalized content.
    pub(crate) fn on_tick(&mut self) {
        self.spinner_frame = self.spinner_frame.wrapping_add(1);
        self.commit_one_streaming_chunk();
    }

    pub(crate) fn commit_one_streaming_chunk(&mut self) {
        if !self.pending_lines.is_empty() {
            let line = self.pending_lines.remove(0);
            self.history_queue.push(line);
        }
    }

    /// Finalize every live item and flush all pending lines into scrollback at
    /// once. Used at turn boundaries where pacing would only add latency.
    pub(crate) fn finalize_all(&mut self) {
        self.finalize_trailing_text();
        for item in std::mem::take(&mut self.live) {
            self.pending_lines.extend(item_history_lines(&item));
        }
        while !self.pending_lines.is_empty() {
            self.commit_one_streaming_chunk();
        }
    }

    /// Drain queued scrollback, wrapping each logical line to `width`.
    pub(crate) fn take_history_lines(&mut self, width: usize) -> Vec<HistoryLine> {
        let width = width.max(1);
        let lines = std::mem::take(&mut self.history_queue);
        lines
            .into_iter()
            .flat_map(|line| {
                wrap_line(&line.text, width).into_iter().map(move |text| {
                    // Pad the user prompt so its background fills the whole row.
                    let text = if line.kind == LineKind::User {
                        format!("{text:<width$}")
                    } else {
                        text
                    };
                    HistoryLine {
                        text,
                        kind: line.kind,
                    }
                })
            })
            .collect()
    }

    /// Append streamed text to the trailing text item, shedding every completed
    /// line into `pending_lines` and leaving the partial line live.
    fn append_streaming_text(&mut self, text: &str, kind: LineKind) {
        match self.live.last_mut() {
            Some(Item::Message(buf) | Item::Thinking(buf)) => buf.push_str(text),
            _ => return,
        }

        loop {
            let line = match self.live.last_mut() {
                Some(Item::Message(buf) | Item::Thinking(buf)) => buf.find('\n').map(|newline| {
                    let committed = buf[..newline].to_string();
                    buf.drain(..=newline);
                    committed
                }),
                _ => None,
            };
            match line {
                Some(committed) => self.pending_lines.push(HistoryLine {
                    text: committed,
                    kind,
                }),
                None => break,
            }
        }
    }

    /// Finalize a trailing live text item, flushing its remaining partial line.
    fn finalize_trailing_text(&mut self) {
        if matches!(self.live.last(), Some(Item::Message(_) | Item::Thinking(_))) {
            let item = self.live.pop().expect("trailing item checked above");
            self.pending_lines.extend(item_history_lines(&item));
        }
    }

    /// Promote the leading run of finalized items into `pending_lines`,
    /// preserving order (a still-running item blocks later finished ones).
    fn promote_done_prefix(&mut self) {
        while self.live.first().is_some_and(Item::is_done) {
            let item = self.live.remove(0);
            self.pending_lines.extend(item_history_lines(&item));
        }
    }

    fn live_tool_mut(&mut self, id: &str) -> Option<&mut Item> {
        self.live
            .iter_mut()
            .find(|item| matches!(item, Item::Tool { call, result: None, .. } if call.id == id))
    }
}

/// Frozen scrollback form of a finalized item. This is the permanent record
/// written to terminal scrollback, so it differs from the live rendering (e.g.
/// a running tool shows a spinner live but a call/output/result block here).
fn item_history_lines(item: &Item) -> Vec<HistoryLine> {
    match item {
        Item::Message(text) => {
            if text.is_empty() {
                Vec::new()
            } else {
                vec![HistoryLine::normal(text.clone())]
            }
        }
        Item::Thinking(text) => {
            if text.is_empty() {
                Vec::new()
            } else {
                vec![HistoryLine::thinking(text.clone())]
            }
        }
        Item::Notice { text, kind } => vec![HistoryLine {
            text: text.clone(),
            kind: *kind,
        }],
        Item::Tool {
            call,
            output,
            result,
        } => {
            let mut lines = vec![HistoryLine::normal(format!(
                "● {}",
                describe_tool_call(call)
            ))];
            for line in output_tail(output, TOOL_OUTPUT_TAIL_LINES) {
                lines.push(HistoryLine::dim(format!("  {line}")));
            }
            let status = match result {
                Some(result) => describe_tool_result(result),
                None => "interrupted".to_string(),
            };
            lines.push(HistoryLine::normal(format!("  {status}")));
            lines
        }
    }
}

fn user_prompt_history_lines(prompt: &str) -> Vec<HistoryLine> {
    prompt
        .split('\n')
        .enumerate()
        .map(|(index, line)| {
            let line = line.strip_suffix('\r').unwrap_or(line);
            let prefix = if index == 0 { "> " } else { "  " };
            HistoryLine::user(format!("{prefix}{line}"))
        })
        .collect()
}

/// The last `max` non-blank lines of tool output, for the collapsed tail.
fn output_tail(output: &str, max: usize) -> Vec<String> {
    let lines: Vec<&str> = output
        .lines()
        .filter(|line| !line.trim().is_empty())
        .collect();
    let start = lines.len().saturating_sub(max);
    lines[start..].iter().map(|line| line.to_string()).collect()
}

fn wrap_line(line: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    if line.is_empty() {
        return vec![String::new()];
    }

    line.chars()
        .collect::<Vec<_>>()
        .chunks(width)
        .map(|chunk| chunk.iter().collect())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn tool_call(id: &str, command: &str) -> ToolCall {
        ToolCall {
            id: id.to_string(),
            name: "shell".to_string(),
            input: json!({ "command": command }),
        }
    }

    #[test]
    fn assistant_deltas_queue_complete_lines_and_keep_partial_live() {
        let mut conv = Conversation::new();

        conv.push_assistant_delta("hello\nwor");
        conv.push_assistant_delta("ld");

        assert_eq!(conv.pending_lines, vec![HistoryLine::normal("hello")]);
        assert_eq!(conv.live, vec![Item::Message("world".to_string())]);

        conv.commit_one_streaming_chunk();
        assert_eq!(conv.history_queue, vec![HistoryLine::normal("hello")]);
    }

    #[test]
    fn thinking_deltas_queue_as_dim_lines_and_keep_partial_live() {
        let mut conv = Conversation::new();

        conv.push_thinking_delta("reason\nmor");
        conv.push_thinking_delta("e");

        assert_eq!(conv.pending_lines, vec![HistoryLine::thinking("reason")]);
        assert_eq!(conv.live, vec![Item::Thinking("more".to_string())]);

        // Assistant content finalizes the trailing thinking item first.
        conv.push_assistant_delta("answer\n");
        assert_eq!(conv.live, vec![Item::Message(String::new())]);
        assert_eq!(
            conv.pending_lines,
            vec![
                HistoryLine::thinking("reason"),
                HistoryLine::thinking("more"),
                HistoryLine::normal("answer"),
            ]
        );
    }

    #[test]
    fn tool_call_renders_live_then_promotes_to_history_as_a_unit() {
        let mut conv = Conversation::new();

        conv.start_tool(tool_call("call-1", "cargo test"));
        // While running the tool lives in the transient zone, not scrollback.
        assert_eq!(conv.live.len(), 1);
        assert!(conv.pending_lines.is_empty());

        conv.push_tool_output("call-1", "line1\nline2\nline3\nline4\n");
        match &conv.live[0] {
            Item::Tool { output, .. } => assert_eq!(output, "line1\nline2\nline3\nline4\n"),
            other => panic!("expected live tool, got {other:?}"),
        }

        conv.finish_tool(
            "call-1",
            ToolResult {
                exit_code: Some(0),
                summary: "ok".to_string(),
            },
        );

        // Finishing promotes the whole tool as a unit: call line, a collapsed
        // output tail (last 3 non-blank lines), then the result line.
        assert!(conv.live.is_empty());
        assert_eq!(
            conv.pending_lines,
            vec![
                HistoryLine::normal("● shell: cargo test"),
                HistoryLine::dim("  line2"),
                HistoryLine::dim("  line3"),
                HistoryLine::dim("  line4"),
                HistoryLine::normal("  exit 0"),
            ]
        );
    }

    #[test]
    fn parallel_tools_promote_in_order_after_the_blocking_one_finishes() {
        let mut conv = Conversation::new();

        conv.start_tool(tool_call("a", "a"));
        conv.start_tool(tool_call("b", "b"));

        // The later tool finishes first, but cannot jump ahead of the earlier
        // still-running one — nothing is promoted yet.
        conv.finish_tool(
            "b",
            ToolResult {
                exit_code: Some(0),
                summary: "ok".to_string(),
            },
        );
        assert!(conv.pending_lines.is_empty());
        assert_eq!(conv.live.len(), 2);

        // Once the blocking tool finishes, both promote in emission order.
        conv.finish_tool(
            "a",
            ToolResult {
                exit_code: Some(1),
                summary: "fail".to_string(),
            },
        );
        assert!(conv.live.is_empty());
        assert_eq!(
            conv.pending_lines,
            vec![
                HistoryLine::normal("● shell: a"),
                HistoryLine::normal("  exit 1"),
                HistoryLine::normal("● shell: b"),
                HistoryLine::normal("  exit 0"),
            ]
        );
    }

    #[test]
    fn finalize_all_flushes_trailing_partial_text() {
        let mut conv = Conversation::new();

        conv.push_assistant_delta("partial");
        conv.finalize_all();

        assert!(conv.live.is_empty());
        assert_eq!(conv.history_queue, vec![HistoryLine::normal("partial")]);
    }

    #[test]
    fn user_prompt_echo_is_tagged_and_padded_to_full_width() {
        let mut conv = Conversation::new();

        conv.start_turn("hi");
        let lines = conv.take_history_lines(10);

        assert_eq!(lines, vec![HistoryLine::user("> hi      ")]);
    }

    #[test]
    fn multiline_user_prompt_echo_splits_explicit_newlines() {
        let mut conv = Conversation::new();

        conv.start_turn("first\nsecond\n\nfourth");

        assert_eq!(
            conv.history_queue,
            vec![
                HistoryLine::user("> first"),
                HistoryLine::user("  second"),
                HistoryLine::user("  "),
                HistoryLine::user("  fourth"),
            ]
        );
        assert!(
            conv.history_queue
                .iter()
                .all(|line| !line.text.contains('\n'))
        );
    }

    #[test]
    fn multiline_user_prompt_echo_normalizes_crlf() {
        let mut conv = Conversation::new();

        conv.start_turn("first\r\nsecond");

        assert_eq!(
            conv.history_queue,
            vec![HistoryLine::user("> first"), HistoryLine::user("  second"),]
        );
        assert!(
            conv.history_queue
                .iter()
                .all(|line| !line.text.contains('\r'))
        );
    }

    #[test]
    fn wrap_line_prepares_lines_for_terminal_scrollback() {
        assert_eq!(
            wrap_line("abcdef", 2),
            vec!["ab".to_string(), "cd".to_string(), "ef".to_string()]
        );
        assert_eq!(wrap_line("", 10), vec!["".to_string()]);
    }
}
