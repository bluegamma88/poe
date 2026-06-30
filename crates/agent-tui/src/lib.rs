//! Inline interactive terminal frontend for poe.

mod composer;
mod conversation;
mod custom_terminal;
mod history_insert;
mod resize_reflow;

use std::{
    error::Error,
    fmt, io,
    io::Stdout,
    path::{Path, PathBuf},
    thread,
    time::{Duration, Instant},
};

use agent_core::{AgentSession, ModelClient};
use agent_exec::{SessionTrace, save_session_trace};
use agent_protocol::{Event, Op, TokenUsage, ToolCall, ToolResult};
use composer::{COMPOSER_LEFT_PREFIX, Composer};
use conversation::{Conversation, Item, LineKind};
use crossterm::{
    SynchronizedUpdate, cursor,
    event::{
        self, DisableBracketedPaste, EnableBracketedPaste, Event as CrosstermEvent, KeyCode,
        KeyEvent, KeyModifiers, KeyboardEnhancementFlags, PopKeyboardEnhancementFlags,
        PushKeyboardEnhancementFlags,
    },
    execute,
    style::Print,
    terminal::{Clear as TerminalClear, ClearType, disable_raw_mode, enable_raw_mode},
};
use futures_util::{StreamExt, future};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Rect, Size},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph, Wrap},
};
use tokio::{
    sync::mpsc,
    time::{self, MissedTickBehavior},
};

const COMMIT_TICK: Duration = Duration::from_millis(50);
const SPINNER_FRAMES: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TuiOptions {
    pub cwd: PathBuf,
    pub sessions_dir: Option<PathBuf>,
}

pub async fn run_with_model<M>(options: TuiOptions, model: M) -> Result<(), TuiError>
where
    M: ModelClient,
{
    let model_slug = model.model_slug();
    let mut session = AgentSession::new(model);
    let mut terminal = InlineTerminal::enter()?;
    let input_rx = spawn_input_thread();
    let mut app = AppState::new(model_slug.clone(), options.cwd.clone());
    app.set_key_debug(std::env::var_os("POE_TUI_KEY_DEBUG").is_some());
    let mut current_stream: Option<agent_core::EventStream> = None;
    let mut input_rx = input_rx;
    let mut tick = time::interval(COMMIT_TICK);
    tick.set_missed_tick_behavior(MissedTickBehavior::Skip);

    // Source-backed scrollback reflow on terminal resize, on by default. Set
    // POE_TUI_RESIZE_REFLOW=0 to opt out if a terminal mishandles the replay.
    let reflow_enabled = std::env::var("POE_TUI_RESIZE_REFLOW")
        .map(|value| value != "0")
        .unwrap_or(true);
    let reflow_max_rows = resize_reflow::parse_max_rows(
        std::env::var("POE_TUI_RESIZE_REFLOW_MAX_ROWS")
            .ok()
            .as_deref(),
    );
    let mut reflow = resize_reflow::ResizeReflowState::default();
    {
        let size = terminal.screen_size();
        reflow.init_size(size.width, size.height);
    }

    terminal.flush_welcome(&app)?;
    terminal.draw(&mut app)?;

    loop {
        if current_stream.is_some() {
            tokio::select! {
                maybe_input = input_rx.recv() => {
                    if handle_input(maybe_input, &mut app, &mut session, &mut current_stream).await? {
                        break;
                    }
                }
                _ = tick.tick() => {
                    app.log.on_tick();
                }
                maybe_event = next_event(&mut current_stream) => {
                    if let Some(event) = maybe_event {
                        app.apply_event(event);
                    }
                }
            }
        } else {
            tokio::select! {
                maybe_input = input_rx.recv() => {
                    if handle_input(maybe_input, &mut app, &mut session, &mut current_stream).await? {
                        break;
                    }
                }
                _ = tick.tick() => {
                    app.log.on_tick();
                }
            }
        }

        if reflow_enabled {
            let now = Instant::now();
            let size = terminal.screen_size();
            reflow.observe(size.width, size.height, now);
            if let Some((width, _height)) = reflow.take_due(now) {
                terminal.reflow_scrollback(&mut app, width, reflow_max_rows)?;
            }
        }

        terminal.flush_history(&mut app)?;
        terminal.draw(&mut app)?;
    }

    app.log.finalize_all();
    terminal.flush_history(&mut app)?;
    terminal.draw(&mut app)?;
    drop(terminal);

    let model = session.into_model();
    persist_tui_trace(
        options.sessions_dir.as_deref(),
        options.cwd,
        model_slug,
        model.transcript(),
        model.tool_definitions(),
    )?;

    Ok(())
}

async fn next_event(current_stream: &mut Option<agent_core::EventStream>) -> Option<Event> {
    let Some(stream) = current_stream.as_mut() else {
        future::pending::<()>().await;
        return None;
    };

    match stream.next().await {
        Some(event) => Some(event),
        None => {
            *current_stream = None;
            None
        }
    }
}

async fn handle_input<M>(
    maybe_input: Option<InputEvent>,
    app: &mut AppState,
    session: &mut AgentSession<M>,
    current_stream: &mut Option<agent_core::EventStream>,
) -> Result<bool, TuiError>
where
    M: ModelClient,
{
    let Some(input) = maybe_input else {
        return Ok(true);
    };

    match input {
        InputEvent::Key(key) => handle_key(key, app, session, current_stream).await,
        InputEvent::Paste(text) => {
            app.composer.insert_text(&text);
            Ok(false)
        }
        InputEvent::Resize => Ok(false),
    }
}

async fn handle_key<M>(
    key: KeyEvent,
    app: &mut AppState,
    session: &mut AgentSession<M>,
    current_stream: &mut Option<agent_core::EventStream>,
) -> Result<bool, TuiError>
where
    M: ModelClient,
{
    app.record_key_debug(&key);

    match key.code {
        KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            if app.running {
                Ok(false)
            } else {
                Ok(true)
            }
        }
        KeyCode::Esc => {
            if app.running {
                // The running turn stream observes the interrupt and emits
                // Event::TurnInterrupted, which returns the app to idle. Keep
                // polling current_stream; the returned (empty) stream is unused.
                let _ = session.submit(Op::Interrupt).await?;
            } else if !app.composer.is_empty() {
                app.composer.clear();
            }
            Ok(false)
        }
        KeyCode::Enter => {
            if key.modifiers.contains(KeyModifiers::SHIFT) {
                app.composer.insert_char('\n');
                return Ok(false);
            }

            if app.running {
                return Ok(false);
            }

            let Some(prompt) = app.composer.take_prompt() else {
                return Ok(false);
            };

            app.start_turn(&prompt);
            let events = session
                .submit(Op::UserTurn {
                    prompt,
                    cwd: app.cwd.clone(),
                })
                .await?;
            *current_stream = Some(events);
            Ok(false)
        }
        KeyCode::Backspace if key.modifiers.contains(KeyModifiers::SUPER) => {
            app.composer.backspace_to_line_start();
            Ok(false)
        }
        KeyCode::Backspace if key.modifiers.contains(KeyModifiers::ALT) => {
            app.composer.backspace_word();
            Ok(false)
        }
        KeyCode::Backspace => {
            app.composer.backspace();
            Ok(false)
        }
        KeyCode::Left => {
            app.composer.move_left();
            Ok(false)
        }
        KeyCode::Right => {
            app.composer.move_right();
            Ok(false)
        }
        KeyCode::Up => {
            let (text_width, visible_rows) = app.composer_geometry();
            app.composer.move_up(text_width, visible_rows);
            Ok(false)
        }
        KeyCode::Down => {
            let (text_width, visible_rows) = app.composer_geometry();
            app.composer.move_down(text_width, visible_rows);
            Ok(false)
        }
        KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            if app.composer.is_empty() {
                app.scroll_up();
            } else {
                app.composer.backspace_to_line_start();
            }
            Ok(false)
        }
        KeyCode::Char('d') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            if app.running {
                Ok(false)
            } else {
                Ok(true)
            }
        }
        KeyCode::PageUp => {
            app.scroll_up();
            Ok(false)
        }
        KeyCode::PageDown => {
            app.scroll_down();
            Ok(false)
        }
        KeyCode::Char(ch) => {
            if !key.modifiers.contains(KeyModifiers::CONTROL)
                && !key.modifiers.contains(KeyModifiers::ALT)
            {
                app.composer.insert_char(ch);
            }
            Ok(false)
        }
        _ => Ok(false),
    }
}

fn persist_tui_trace(
    sessions_dir: Option<&Path>,
    cwd: PathBuf,
    model: String,
    messages: Vec<agent_protocol::TranscriptMessage>,
    tools: Vec<serde_json::Value>,
) -> Result<(), TuiError> {
    let Some(sessions_dir) = sessions_dir else {
        return Ok(());
    };

    save_session_trace(
        sessions_dir,
        &SessionTrace {
            prompt: String::new(),
            cwd,
            model,
            tools,
            messages,
        },
    )?;
    Ok(())
}

#[derive(Debug)]
enum InputEvent {
    Key(KeyEvent),
    Paste(String),
    Resize,
}

fn spawn_input_thread() -> mpsc::UnboundedReceiver<InputEvent> {
    let (tx, rx) = mpsc::unbounded_channel();

    thread::spawn(move || {
        loop {
            match event::read() {
                Ok(CrosstermEvent::Key(key)) => {
                    if tx.send(InputEvent::Key(key)).is_err() {
                        break;
                    }
                }
                Ok(CrosstermEvent::Paste(text)) => {
                    if tx.send(InputEvent::Paste(text)).is_err() {
                        break;
                    }
                }
                Ok(CrosstermEvent::Resize(_, _)) => {
                    if tx.send(InputEvent::Resize).is_err() {
                        break;
                    }
                }
                Ok(_) => {}
                Err(_) => break,
            }
        }
    });

    rx
}

struct InlineTerminal {
    terminal: custom_terminal::Terminal<CrosstermBackend<Stdout>>,
}

impl InlineTerminal {
    fn enter() -> Result<Self, TuiError> {
        enable_raw_mode()?;
        let mut stdout = io::stdout();
        execute!(
            stdout,
            EnableBracketedPaste,
            PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES),
            cursor::Hide
        )?;
        let backend = CrosstermBackend::new(stdout);
        let terminal = custom_terminal::Terminal::with_options(backend)?;

        Ok(Self { terminal })
    }

    fn flush_history(&mut self, app: &mut AppState) -> Result<(), TuiError> {
        // In the bottom-pinned regime, history insertion is coordinated with the
        // viewport resize inside `draw` so the two cannot open a gap; the queued
        // lines are drained there instead.
        if self.terminal.is_bottom_pinned() {
            return Ok(());
        }
        let width = self.terminal.viewport_area.width.max(1) as usize;
        let lines = app.log.take_history_lines(width);
        if lines.is_empty() {
            return Ok(());
        }
        history_insert::insert_history_lines(&mut self.terminal, &lines)?;
        Ok(())
    }

    /// Write the metadata box into scrollback once, so it stays pinned at the
    /// top of the conversation above all turns.
    fn flush_welcome(&mut self, app: &AppState) -> Result<(), TuiError> {
        let lines = welcome_history_lines(app);
        history_insert::insert_history_lines(&mut self.terminal, &lines)?;
        Ok(())
    }

    /// Current terminal size, or a zero size if the backend cannot be probed.
    fn screen_size(&self) -> Size {
        self.terminal.size().unwrap_or(Size::new(0, 0))
    }

    /// Rebuild owned scrollback from retained conversation history at `width`,
    /// discarding the stale wrapping the terminal applied to already-emitted
    /// rows after a width change and the row shifts a height change causes
    /// around the viewport.
    ///
    /// Handles both regimes. When history still fits above the viewport the
    /// rows are repainted in place and the viewport stays top-anchored;
    /// otherwise the viewport bottom-pins and overflow scrolls into scrollback.
    fn reflow_scrollback(
        &mut self,
        app: &mut AppState,
        width: u16,
        max_rows: Option<usize>,
    ) -> Result<(), TuiError> {
        let width = width.max(1);

        // Replay the persistent welcome box (flushed out-of-band, so not part of
        // the conversation log) followed by retained history, both re-wrapped to
        // the new width.
        let mut lines =
            conversation::render_history_lines(&welcome_history_lines(app), width as usize);
        let mut history = app.log.render_history_lines(width as usize);

        // Cap replayed history rows to roughly what the terminal would retain,
        // dropping the oldest with a marker rather than silently truncating. The
        // welcome box is always kept.
        if let Some(cap) = max_rows.filter(|&cap| history.len() > cap) {
            let dropped = history.len() - cap;
            history.drain(0..dropped);
            history.insert(
                0,
                conversation::HistoryLine::dim(format!("… {dropped} earlier rows not reflowed …")),
            );
        }
        lines.append(&mut history);

        // The replay reproduces every retained line, including those still
        // queued for the incremental drip. Drop the queue so the next
        // `flush_history` does not re-emit them on top of the replay.
        app.log.clear_pending_history_queue();

        // Place the viewport exactly where the next draw will, so the post-reflow
        // draw is a no-op move and does not scroll the freshly replayed rows.
        let size = self.terminal.size()?;
        let height = app
            .desired_viewport_height(width, size.height)
            .min(size.height);

        io::stdout()
            .sync_update(|_| replay_reflow(&mut self.terminal, &lines, width, height))??;
        Ok(())
    }

    fn draw(&mut self, app: &mut AppState) -> Result<(), TuiError> {
        let screen_size = self.terminal.size()?;
        let height = app
            .desired_viewport_height(screen_size.width, screen_size.height)
            .min(screen_size.height);
        self.draw_with_height(height, app)
    }

    fn draw_with_height(&mut self, height: u16, app: &mut AppState) -> Result<(), TuiError> {
        io::stdout().sync_update(|_| {
            let terminal = &mut self.terminal;
            let size = terminal.size()?;

            let mut area = terminal.viewport_area;
            area.height = height.min(size.height);
            area.width = size.width;

            if !terminal.is_bottom_pinned() {
                // Regime 1 — top-anchored: before the screen has filled, the
                // viewport keeps its top fixed and grows downward. The first
                // time it would overflow the bottom, scroll the rows above it
                // into scrollback, pin it to the screen bottom, and latch into
                // the bottom-pinned regime for the rest of the session. History
                // for this frame was already inserted by `flush_history`.
                if area.bottom() > size.height {
                    let overflow = area.bottom() - size.height;
                    scroll_rows_into_scrollback(terminal, area.top(), overflow)?;
                    area.y = size.height - area.height;
                    terminal.set_bottom_pinned(true);
                }
                if area != terminal.viewport_area {
                    terminal.clear_for_viewport_change(area)?;
                    terminal.set_viewport_area(area);
                }
            } else {
                // Regime 2 — bottom-pinned: drain and place this frame's history
                // in lockstep with the viewport move so neither opens a gap.
                reconcile_bottom_pinned(terminal, &mut area, size, app)?;
            }

            app.update_composer_geometry(area.width);
            terminal.draw(|frame| {
                render_app(frame, frame.area(), app);
            })
        })??;
        Ok(())
    }
}

impl Drop for InlineTerminal {
    fn drop(&mut self) {
        // Move cursor below the viewport so the shell prompt appears after it,
        // leaving the whole viewport — including the footer on its last row — on
        // screen.
        let bottom = self.terminal.viewport_area.bottom();
        let screen_height = self.terminal.size().map(|s| s.height).unwrap_or(bottom);
        let backend = self.terminal.backend_mut();
        if bottom >= screen_height {
            // Bottom-pinned: the footer occupies the last visible row and there
            // is no row beneath it. Scroll the screen up by one line from the
            // last row so the footer is preserved in scrollback, opening a fresh
            // line below it for the shell prompt.
            let _ = execute!(
                backend,
                crossterm::cursor::MoveTo(0, screen_height.saturating_sub(1)),
                Print("\r\n")
            );
        } else {
            // Top-anchored: there is a free row below the viewport, so the
            // prompt can land there directly.
            let _ = execute!(backend, crossterm::cursor::MoveTo(0, bottom));
        }
        let _ = execute!(
            backend,
            cursor::Show,
            PopKeyboardEnhancementFlags,
            DisableBracketedPaste,
            TerminalClear(ClearType::UntilNewLine)
        );
        let _ = disable_raw_mode();
    }
}

/// Purge owned scrollback and replay `lines` at the new size. The
/// byte-emitting core of [`InlineTerminal::reflow_scrollback`], split out so it
/// can be exercised against a capturing backend in tests.
///
/// When history still fits above the viewport the rows are painted in place and
/// the viewport stays top-anchored; otherwise the viewport bottom-pins and the
/// insert path scrolls overflow into terminal scrollback.
fn replay_reflow<B>(
    terminal: &mut custom_terminal::Terminal<B>,
    lines: &[conversation::HistoryLine],
    width: u16,
    height: u16,
) -> io::Result<()>
where
    B: ratatui::backend::Backend + io::Write,
{
    let size = terminal.size()?;
    let rows = lines.len() as u16;
    // Stay top-anchored while history still fits above the viewport and the
    // session has not already latched into the bottom-pinned regime.
    let top_anchored = !terminal.is_bottom_pinned() && rows.saturating_add(height) <= size.height;

    // Take ownership of the screen: purge scrollback and clear the visible
    // screen, throwing away the terminal's own reflow of the old rows in favour
    // of our source-backed replay.
    crossterm::queue!(
        terminal.backend_mut(),
        crossterm::cursor::MoveTo(0, 0),
        TerminalClear(ClearType::Purge),
        TerminalClear(ClearType::All),
    )?;

    if top_anchored {
        let area = Rect::new(0, rows, width, height);
        history_insert::write_history_block(terminal.backend_mut(), 0, lines, width)?;
        terminal.reset_for_reflow(area, false, rows);
    } else {
        let area = Rect::new(0, size.height.saturating_sub(height), width, height);
        terminal.reset_for_reflow(area, true, 0);
        // With no rows above the viewport there is nowhere to replay into.
        if area.top() > 0 {
            history_insert::insert_history_lines(terminal, lines)?;
        }
    }
    Ok(())
}

/// Scroll `count` rows immediately above `viewport_top` up into terminal
/// scrollback, leaving the viewport itself untouched. We set a scroll region
/// covering the rows above the viewport, park the cursor on the bottom row of
/// that region, and emit newlines to push its content up. No-op when there is
/// nothing above the viewport or nothing to scroll.
fn scroll_rows_into_scrollback<B>(
    terminal: &mut custom_terminal::Terminal<B>,
    viewport_top: u16,
    count: u16,
) -> io::Result<()>
where
    B: ratatui::backend::Backend + io::Write,
{
    if count == 0 || viewport_top == 0 {
        return Ok(());
    }
    let writer = terminal.backend_mut();
    crossterm::queue!(writer, history_insert::ScrollRegionCmd(1..viewport_top))?;
    crossterm::queue!(
        writer,
        crossterm::cursor::MoveTo(0, viewport_top.saturating_sub(1))
    )?;
    for _ in 0..count {
        crossterm::queue!(writer, crossterm::style::Print("\n"))?;
    }
    crossterm::queue!(writer, history_insert::ResetScrollRegionCmd)?;
    Ok(())
}

/// Pull the history above the viewport down by `count` rows using reverse-index
/// inside a scroll region covering rows `[1, region_bottom)`. The bottom `count`
/// rows of that region (the stale rows the viewport just vacated) fall away and
/// `count` blank rows open at the very top. Used by the bottom-pinned regime
/// when the viewport shrinks by more rows than there are new history lines.
fn pull_history_down<B>(
    terminal: &mut custom_terminal::Terminal<B>,
    region_bottom: u16,
    count: u16,
) -> io::Result<()>
where
    B: ratatui::backend::Backend + io::Write,
{
    if count == 0 || region_bottom == 0 {
        return Ok(());
    }
    let writer = terminal.backend_mut();
    crossterm::queue!(writer, history_insert::ScrollRegionCmd(1..region_bottom))?;
    crossterm::queue!(writer, crossterm::cursor::MoveTo(0, 0))?;
    for _ in 0..count {
        // Reverse Index — scrolls the region content down one row.
        crossterm::queue!(writer, crossterm::style::Print("\x1bM"))?;
    }
    crossterm::queue!(writer, history_insert::ResetScrollRegionCmd)?;
    Ok(())
}

/// Drain this frame's queued history and place it while moving the bottom-pinned
/// viewport to its new height, coordinating the two so no blank gap is ever
/// opened above the live region. `area` is updated to the new viewport rect.
fn reconcile_bottom_pinned<B>(
    terminal: &mut custom_terminal::Terminal<B>,
    area: &mut Rect,
    size: Size,
    app: &mut AppState,
) -> io::Result<()>
where
    B: ratatui::backend::Backend + io::Write,
{
    let old_top = area.top();
    let new_top = size.height.saturating_sub(area.height);
    let width = area.width.max(1);
    let mut lines = app.log.take_history_lines(width as usize);
    area.y = new_top;

    if new_top < old_top {
        // GROW — the viewport top rises. Insert any queued history above the
        // current top, then scroll the rows the viewport now covers into
        // scrollback to preserve them.
        if !lines.is_empty() {
            history_insert::insert_history_lines(terminal, &lines)?;
        }
        scroll_rows_into_scrollback(terminal, old_top, old_top - new_top)?;
        if *area != terminal.viewport_area {
            terminal.clear_for_viewport_change(*area)?;
            terminal.set_viewport_area(*area);
        }
    } else {
        // SHRINK — the viewport top descends. Only `new_top` history rows fit
        // above the new viewport. When more lines were flushed at once (e.g.
        // finalizing a whole turn produces a backlog larger than the rows above
        // the shrunken viewport), push the oldest overflow into scrollback
        // first — bottom-aligned to the *old* top, where the region above the
        // viewport is still full of history — so the remaining lines can be laid
        // flush against the new top below. The previous bulk-shrink shortcut
        // instead inserted everything above the old top and then moved the
        // viewport down, leaving a blank gap above the composer.
        let n = lines.len() as u16;
        if n > new_top {
            let overflow = (n - new_top) as usize;
            history_insert::insert_history_lines(terminal, &lines[..overflow])?;
            lines.drain(..overflow);
        }

        // At most `new_top` lines remain. Lay them into the rows the shrink
        // vacates, scrolling older on-screen history into scrollback for any
        // overflow above, and pulling history down to fill rows the shrink frees
        // beyond the new lines. History already on screen above is left
        // untouched, so no gap appears.
        let n = lines.len() as u16;
        let shrink = new_top - old_top;
        let above = n.saturating_sub(shrink);
        let pulldown = shrink.saturating_sub(n);
        scroll_rows_into_scrollback(terminal, old_top, above)?;
        pull_history_down(terminal, new_top, pulldown)?;
        if n > 0 {
            let start = new_top - n;
            history_insert::write_history_block(terminal.backend_mut(), start, &lines, width)?;
        }
        // Only resize (and force a full repaint) when the viewport actually
        // moved. When the height is unchanged the diff base is still valid and
        // the steady-state frame stays a minimal diff. History written above is
        // untouched by the viewport repaint either way. Clear the new viewport
        // first: after invalidating the diff base, blank cells in the next
        // frame would otherwise compare as blank-vs-blank and leave stale
        // terminal glyphs behind.
        if *area != terminal.viewport_area {
            terminal.clear_after_position(area.as_position())?;
            terminal.set_viewport_area(*area);
            terminal.invalidate_viewport();
        }
    }
    Ok(())
}

fn render_app(frame: &mut custom_terminal::Frame<'_>, area: Rect, app: &AppState) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(0),
            Constraint::Length(app.status_height()),
            Constraint::Length(app.composer.height(area.width).max(3)),
            Constraint::Length(1),
        ])
        .split(area);

    frame.render_widget(Clear, area);
    render_live(frame, chunks[0], app);
    render_status(frame, chunks[1], app);
    render_composer(frame, chunks[2], app);
    render_footer(frame, chunks[3], app);
    frame.set_cursor_position(app.composer.cursor_position(chunks[2]));
}

/// Render the spinning "Thinking..." indicator that sits directly above the
/// composer while a turn is in flight. Renders nothing when idle.
fn render_status(frame: &mut custom_terminal::Frame<'_>, area: Rect, app: &AppState) {
    if !app.running {
        return;
    }
    let spinner = SPINNER_FRAMES[app.log.spinner_frame % SPINNER_FRAMES.len()];
    let line = Line::from(vec![
        Span::styled(format!("{spinner} "), Style::default().fg(Color::Cyan)),
        Span::styled("Thinking...", Style::default().fg(Color::Gray)),
    ]);
    frame.render_widget(Paragraph::new(line), area);
}

fn history_style(kind: LineKind) -> Style {
    match kind {
        LineKind::Normal => Style::default().fg(Color::White),
        LineKind::Thinking => thinking_style(),
        LineKind::Dim => Style::default().fg(Color::DarkGray),
        LineKind::User => Style::default().fg(Color::White).bg(Color::Indexed(238)),
    }
}

fn thinking_style() -> Style {
    Style::default()
        .fg(Color::Gray)
        .add_modifier(Modifier::ITALIC)
}

fn render_live(frame: &mut custom_terminal::Frame<'_>, area: Rect, app: &AppState) {
    let mut lines = Vec::new();

    // Lines that have been finalized out of the live items but are still
    // draining into scrollback render above the live items (they are older),
    // keeping them visible until the frame they actually land in scrollback.
    for line in &app.log.pending_lines {
        lines.push(Line::from(Span::styled(
            line.text.clone(),
            history_style(line.kind),
        )));
    }

    for item in &app.log.live {
        match item {
            Item::Thinking(text) if !text.is_empty() => {
                lines.push(Line::from(Span::styled(text.clone(), thinking_style())));
            }
            Item::Message(text) if !text.is_empty() => {
                lines.push(Line::from(Span::styled(
                    text.clone(),
                    Style::default().fg(Color::White),
                )));
            }
            Item::Tool { call, .. } => {
                let spinner = SPINNER_FRAMES[app.log.spinner_frame % SPINNER_FRAMES.len()];
                lines.push(Line::from(vec![
                    Span::styled(format!("{spinner} "), Style::default().fg(Color::Cyan)),
                    Span::styled(
                        describe_tool_call(call),
                        Style::default().fg(Color::DarkGray),
                    ),
                ]));
            }
            Item::Notice { text, kind } => {
                lines.push(Line::from(Span::styled(text.clone(), history_style(*kind))));
            }
            Item::Thinking(_) | Item::Message(_) => {}
        }
    }

    let paragraph = Paragraph::new(lines)
        .wrap(Wrap { trim: false })
        .scroll((app.scroll_offset, 0));
    frame.render_widget(paragraph, area);
}

/// Build the welcome box as history lines for scroll-region insertion.
/// Uses box-drawing characters to match the bordered look of the old widget.
fn welcome_history_lines(app: &AppState) -> Vec<conversation::HistoryLine> {
    use conversation::HistoryLine;
    vec![
        HistoryLine::dim("┌ poe agent ──────────────────────"),
        HistoryLine::dim(format!("│ model  {}", app.model)),
        HistoryLine::dim(format!("│ dir    {}", app.cwd.display())),
        HistoryLine::dim("└─────────────────────────────────"),
    ]
}

fn render_composer(frame: &mut custom_terminal::Frame<'_>, area: Rect, app: &AppState) {
    frame.render_widget(
        Block::default().borders(Borders::TOP | Borders::BOTTOM),
        area,
    );

    let text_area = app.composer.text_area(area);
    if text_area.width == 0 || text_area.height == 0 {
        return;
    }

    let layout = app.composer.layout(text_area.width, text_area.height);
    let lines = layout
        .visible_rows()
        .iter()
        .map(|row| Line::from(row.text.to_string()))
        .collect::<Vec<_>>();

    let prefix_width = COMPOSER_LEFT_PREFIX.chars().count() as u16;
    let prefix = Paragraph::new(COMPOSER_LEFT_PREFIX).style(Style::default().fg(Color::Gray));
    frame.render_widget(
        prefix,
        Rect::new(text_area.x - prefix_width, text_area.y, prefix_width, 1),
    );

    let paragraph = Paragraph::new(lines);
    frame.render_widget(paragraph, text_area);
}

fn render_footer(frame: &mut custom_terminal::Frame<'_>, area: Rect, app: &AppState) {
    let text = format!(
        "{} | {} | {} | Enter submit | Ctrl+C quit",
        app.model,
        app.cwd.display(),
        format_usage(&app.session_usage),
    );
    let line = Line::from(Span::styled(text, Style::default().fg(Color::Gray)));
    frame.render_widget(Paragraph::new(line), area);
}

/// Formats session usage totals for the footer, e.g.
/// `$0.0142 · ↑12.4k (8.1k cached, 2.0k write) ↓1.3k`.
fn format_usage(usage: &TokenUsage) -> String {
    format!(
        "${:.4} · ↑{} ({} cached, {} write) ↓{}",
        usage.cost_usd,
        humanize_tokens(usage.input_tokens),
        humanize_tokens(usage.cached_tokens),
        humanize_tokens(usage.cache_write_tokens),
        humanize_tokens(usage.output_tokens),
    )
}

/// Renders a token count compactly: `940`, `12.4k`, `3.0M`.
fn humanize_tokens(count: u64) -> String {
    if count < 1_000 {
        count.to_string()
    } else if count < 1_000_000 {
        format!("{:.1}k", count as f64 / 1_000.0)
    } else {
        format!("{:.1}M", count as f64 / 1_000_000.0)
    }
}

// `session_usage` holds an `f64` cost, so this struct is `PartialEq` but not `Eq`.
#[derive(Debug, Clone, PartialEq)]
pub struct AppState {
    model: String,
    cwd: PathBuf,
    composer: Composer,
    log: Conversation,
    running: bool,
    scroll_offset: u16,
    key_debug: bool,
    session_usage: TokenUsage,
    composer_text_width: u16,
    composer_visible_rows: u16,
}

impl AppState {
    pub fn new(model: String, cwd: PathBuf) -> Self {
        Self {
            model,
            cwd,
            composer: Composer::new(),
            log: Conversation::new(),
            running: false,
            scroll_offset: 0,
            key_debug: false,
            session_usage: TokenUsage::default(),
            composer_text_width: 78,
            composer_visible_rows: 1,
        }
    }

    pub fn apply_event(&mut self, event: Event) {
        match event {
            Event::SessionStarted => {}
            Event::AssistantDelta { text } => self.log.push_assistant_delta(&text),
            Event::ThinkingDelta { text } => self.log.push_thinking_delta(&text),
            Event::ToolStarted { call } => self.log.start_tool(call),
            Event::ToolOutput { id, chunk, .. } => self.log.push_tool_output(&id, &chunk),
            Event::ToolFinished { id, result } => self.log.finish_tool(&id, result),
            Event::PatchProposed { changes, .. } => {
                self.log
                    .push_notice(format!("patch proposed: {} file(s)", changes.len()));
            }
            Event::PatchApplied { changes, .. } => {
                self.log
                    .push_notice(format!("patch applied: {} file(s)", changes.len()));
            }
            Event::PatchFailed { message, .. } => {
                self.log.push_notice(format!("patch failed: {message}"));
            }
            Event::TurnComplete => {
                self.log.finalize_all();
                self.running = false;
            }
            Event::TurnInterrupted => {
                self.log.finalize_all();
                self.log.push_marker("⏹ interrupted");
                self.running = false;
            }
            Event::TurnFailed { message } => {
                self.log.finalize_all();
                self.log.push_marker(format!("turn failed: {message}"));
                self.running = false;
            }
            Event::Usage { usage } => self.session_usage.add(&usage),
        }
    }

    pub fn start_turn(&mut self, prompt: &str) {
        self.running = true;
        self.scroll_offset = 0;
        self.log.start_turn(prompt);
    }

    pub fn set_key_debug(&mut self, enabled: bool) {
        self.key_debug = enabled;
        if enabled {
            self.log
                .push_marker("key debug enabled; press keys to print crossterm KeyEvent values");
        }
    }

    pub fn record_key_debug(&mut self, key: &KeyEvent) {
        if self.key_debug {
            self.log.push_marker(format!("key: {key:?}"));
        }
    }

    fn scroll_up(&mut self) {
        self.scroll_offset = self.scroll_offset.saturating_add(3);
    }

    fn scroll_down(&mut self) {
        self.scroll_offset = self.scroll_offset.saturating_sub(3);
    }

    fn update_composer_geometry(&mut self, viewport_width: u16) {
        let composer_h = self.composer.height(viewport_width).max(3);
        self.composer_text_width = self.composer.text_width(viewport_width).max(1);
        self.composer_visible_rows = composer_h.saturating_sub(2).max(1);
    }

    fn composer_geometry(&self) -> (u16, u16) {
        (self.composer_text_width, self.composer_visible_rows)
    }

    /// Height of the "Thinking..." status row above the composer: one row while
    /// a turn is running, zero when idle.
    fn status_height(&self) -> u16 {
        u16::from(self.running)
    }

    /// Compute how tall the viewport should be for this frame.
    fn desired_viewport_height(&self, width: u16, screen_height: u16) -> u16 {
        let composer_h = self.composer.height(width).max(3);
        let footer_h: u16 = 1;
        let status_h = self.status_height();
        let live_h = self.live_content_height(width);
        let max_height = screen_height / 2;
        (live_h + status_h + composer_h + footer_h).min(max_height)
    }

    /// Compute the number of terminal rows the live items would occupy at the
    /// given width, mirroring the lines that `render_live` builds.
    fn live_content_height(&self, width: u16) -> u16 {
        let width = width.max(1) as usize;
        let mut rows: u16 = 0;
        // Reserve rows for finalized lines that have left the live items but
        // have not yet dripped into scrollback. Without this the viewport would
        // shrink the instant a line is shed, one frame before the matching
        // scrollback write, producing a down-then-up bounce.
        for line in &self.log.pending_lines {
            rows += wrapped_line_count(&line.text, width);
        }
        for item in &self.log.live {
            match item {
                Item::Thinking(text) if !text.is_empty() => {
                    rows += wrapped_line_count(text, width);
                }
                Item::Message(text) if !text.is_empty() => {
                    rows += wrapped_line_count(text, width);
                }
                Item::Tool { call, .. } => {
                    // "⠋ shell: ls -la" — spinner char + space + description
                    let desc = describe_tool_call(call);
                    let char_count = 2 + desc.chars().count(); // spinner + space
                    rows += char_count.max(1).div_ceil(width) as u16;
                }
                Item::Notice { text, .. } => {
                    rows += wrapped_line_count(text, width);
                }
                Item::Thinking(_) | Item::Message(_) => {}
            }
        }
        rows
    }
}

/// Count the wrapped visual lines a text string occupies at the given width.
/// Mirrors the wrapping behavior of `Paragraph` with `Wrap { trim: false }`.
fn wrapped_line_count(text: &str, width: usize) -> u16 {
    let width = width.max(1);
    if text.is_empty() {
        return 1;
    }
    text.lines()
        .map(|line| {
            let chars = line.chars().count().max(1);
            chars.div_ceil(width) as u16
        })
        .sum()
}

fn describe_tool_call(call: &ToolCall) -> String {
    match call.name.as_str() {
        "shell" => call
            .input
            .get("command")
            .and_then(serde_json::Value::as_str)
            .map(|command| format!("shell: {command}"))
            .unwrap_or_else(|| "shell".to_string()),
        "read_file" => describe_tool_target(call, "file_path", "read_file"),
        "list_dir" => describe_tool_target(call, "dir_path", "list_dir"),
        "edit_file" => describe_tool_target(call, "file_path", "edit_file"),
        "write_file" => describe_tool_target(call, "file_path", "write_file"),
        name => name.to_string(),
    }
}

fn describe_tool_target(call: &ToolCall, key: &str, fallback: &str) -> String {
    call.input
        .get(key)
        .and_then(serde_json::Value::as_str)
        .map(|target| format!("{fallback}: {target}"))
        .unwrap_or_else(|| fallback.to_string())
}

fn describe_tool_result(result: &ToolResult) -> String {
    match result.exit_code {
        Some(code) => format!("exit {code}"),
        None => "done".to_string(),
    }
}

#[derive(Debug)]
pub enum TuiError {
    Agent(agent_core::AgentError),
    Exec(agent_exec::ExecError),
    Io(io::Error),
}

impl fmt::Display for TuiError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Agent(error) => write!(formatter, "{error}"),
            Self::Exec(error) => write!(formatter, "{error}"),
            Self::Io(error) => write!(formatter, "{error}"),
        }
    }
}

impl Error for TuiError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Agent(error) => Some(error),
            Self::Exec(error) => Some(error),
            Self::Io(error) => Some(error),
        }
    }
}

impl From<agent_core::AgentError> for TuiError {
    fn from(error: agent_core::AgentError) -> Self {
        Self::Agent(error)
    }
}

impl From<agent_exec::ExecError> for TuiError {
    fn from(error: agent_exec::ExecError) -> Self {
        Self::Exec(error)
    }
}

impl From<io::Error> for TuiError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use conversation::HistoryLine;
    use ratatui::backend::{Backend, ClearType, WindowSize};
    use ratatui::buffer::Cell;
    use ratatui::layout::Position;
    use std::io::Write;

    #[derive(Clone)]
    struct NoopModel;

    impl ModelClient for NoopModel {
        fn stream_turn(
            &mut self,
            _input: agent_core::ModelTurnInput,
        ) -> Result<agent_core::ModelEventStream, agent_core::ModelError> {
            unreachable!("control-u tests should not submit turns")
        }

        fn stream_tool_results(
            &mut self,
            _results: Vec<agent_core::ModelToolResult>,
        ) -> Result<agent_core::ModelEventStream, agent_core::ModelError> {
            unreachable!("control-u tests should not submit tool results")
        }
    }

    #[test]
    fn usage_events_accumulate_into_session_totals() {
        let mut app = test_app();

        app.apply_event(Event::Usage {
            usage: TokenUsage {
                input_tokens: 100,
                cached_tokens: 40,
                cache_write_tokens: 60,
                output_tokens: 10,
                cost_usd: 0.01,
            },
        });
        app.apply_event(Event::Usage {
            usage: TokenUsage {
                input_tokens: 200,
                cached_tokens: 150,
                cache_write_tokens: 0,
                output_tokens: 20,
                cost_usd: 0.02,
            },
        });

        assert_eq!(app.session_usage.input_tokens, 300);
        assert_eq!(app.session_usage.cached_tokens, 190);
        assert_eq!(app.session_usage.cache_write_tokens, 60);
        assert_eq!(app.session_usage.output_tokens, 30);
        assert!((app.session_usage.cost_usd - 0.03).abs() < 1e-9);
    }

    #[test]
    fn format_usage_renders_cost_and_humanized_tokens() {
        let usage = TokenUsage {
            input_tokens: 12_400,
            cached_tokens: 8_100,
            cache_write_tokens: 2_000,
            output_tokens: 940,
            cost_usd: 0.0142,
        };

        assert_eq!(
            format_usage(&usage),
            "$0.0142 · ↑12.4k (8.1k cached, 2.0k write) ↓940"
        );
    }

    #[test]
    fn humanize_tokens_scales_units() {
        assert_eq!(humanize_tokens(0), "0");
        assert_eq!(humanize_tokens(940), "940");
        assert_eq!(humanize_tokens(12_400), "12.4k");
        assert_eq!(humanize_tokens(3_000_000), "3.0M");
    }

    #[test]
    fn turn_complete_finalizes_the_log_and_returns_idle() {
        let mut app = test_app();
        app.running = true;

        app.apply_event(Event::AssistantDelta {
            text: "partial".to_string(),
        });
        app.apply_event(Event::TurnComplete);

        assert!(!app.running);
        assert!(app.log.live.is_empty());
        assert_eq!(app.log.history_queue, vec![HistoryLine::normal("partial")]);
    }

    #[test]
    fn failed_turn_commits_error_and_returns_idle() {
        let mut app = test_app();
        app.running = true;

        app.apply_event(Event::TurnFailed {
            message: "model failed".to_string(),
        });

        assert!(!app.running);
        assert_eq!(
            app.log.history_queue,
            vec![HistoryLine::normal("turn failed: model failed")]
        );
    }

    #[test]
    fn interrupted_turn_commits_partial_and_marks_idle() {
        let mut app = test_app();
        app.running = true;

        app.apply_event(Event::AssistantDelta {
            text: "partial".to_string(),
        });
        app.apply_event(Event::TurnInterrupted);

        assert!(!app.running);
        assert!(app.log.live.is_empty());
        assert_eq!(
            app.log.history_queue,
            vec![
                HistoryLine::normal("partial"),
                HistoryLine::normal("⏹ interrupted"),
            ]
        );
    }

    #[test]
    fn key_debug_records_key_events_when_enabled() {
        let mut app = test_app();
        app.set_key_debug(true);

        app.record_key_debug(&KeyEvent::new(KeyCode::Backspace, KeyModifiers::SUPER));

        assert_eq!(
            app.log.history_queue,
            vec![
                HistoryLine::normal(
                    "key debug enabled; press keys to print crossterm KeyEvent values"
                ),
                HistoryLine::normal(
                    "key: KeyEvent { code: Backspace, modifiers: KeyModifiers(SUPER), kind: Press, state: KeyEventState(0x0) }"
                ),
            ]
        );
    }

    #[tokio::test]
    async fn control_u_deletes_to_line_start_when_composer_has_text() {
        let mut app = test_app();
        let mut session = AgentSession::new(NoopModel);
        let mut current_stream = None;

        app.composer.insert_text("first line\nsecond line");
        let should_quit = handle_key(
            KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL),
            &mut app,
            &mut session,
            &mut current_stream,
        )
        .await
        .expect("handle key");

        assert!(!should_quit);
        assert_eq!(app.composer.text(), "first line\n");
        assert_eq!(app.scroll_offset, 0);
    }

    #[tokio::test]
    async fn control_u_scrolls_up_when_composer_is_empty() {
        let mut app = test_app();
        let mut session = AgentSession::new(NoopModel);
        let mut current_stream = None;

        let should_quit = handle_key(
            KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL),
            &mut app,
            &mut session,
            &mut current_stream,
        )
        .await
        .expect("handle key");

        assert!(!should_quit);
        assert_eq!(app.composer.text(), "");
        assert_eq!(app.scroll_offset, 3);
    }

    #[tokio::test]
    async fn left_arrow_moves_composer_cursor_for_insertion() {
        let mut app = test_app();
        let mut session = AgentSession::new(NoopModel);
        let mut current_stream = None;

        app.composer.insert_text("helo");
        let should_quit = handle_key(
            KeyEvent::new(KeyCode::Left, KeyModifiers::NONE),
            &mut app,
            &mut session,
            &mut current_stream,
        )
        .await
        .expect("handle key");
        assert!(!should_quit);

        let should_quit = handle_key(
            KeyEvent::new(KeyCode::Char('l'), KeyModifiers::NONE),
            &mut app,
            &mut session,
            &mut current_stream,
        )
        .await
        .expect("handle key");

        assert!(!should_quit);
        assert_eq!(app.composer.text(), "hello");
    }

    #[tokio::test]
    async fn up_and_down_arrows_use_composer_geometry() {
        let mut app = test_app();
        let mut session = AgentSession::new(NoopModel);
        let mut current_stream = None;

        app.composer_text_width = 4;
        app.composer_visible_rows = 3;
        app.composer.insert_text("abcdefghi");

        let should_quit = handle_key(
            KeyEvent::new(KeyCode::Up, KeyModifiers::NONE),
            &mut app,
            &mut session,
            &mut current_stream,
        )
        .await
        .expect("handle key");
        assert!(!should_quit);
        assert_eq!(app.composer.cursor(), 5);

        let should_quit = handle_key(
            KeyEvent::new(KeyCode::Down, KeyModifiers::NONE),
            &mut app,
            &mut session,
            &mut current_stream,
        )
        .await
        .expect("handle key");

        assert!(!should_quit);
        assert_eq!(app.composer.cursor(), 9);
    }

    #[test]
    fn scroll_bounds_do_not_underflow() {
        let mut app = test_app();

        app.scroll_down();
        assert_eq!(app.scroll_offset, 0);
        app.scroll_up();
        app.scroll_down();
        assert_eq!(app.scroll_offset, 0);
    }

    #[test]
    fn bottom_pinned_shrink_clears_new_viewport() {
        let backend = RecordingBackend::new(Size::new(20, 8));
        let mut terminal = custom_terminal::Terminal::with_options(backend).expect("terminal");
        terminal.set_bottom_pinned(true);
        terminal.set_viewport_area(Rect::new(0, 3, 20, 5));

        let mut app = test_app();
        app.log.start_turn("hello");

        let mut area = terminal.viewport_area;
        area.height = 4;
        reconcile_bottom_pinned(&mut terminal, &mut area, Size::new(20, 8), &mut app)
            .expect("reconcile");

        assert_eq!(terminal.viewport_area, Rect::new(0, 4, 20, 4));
        assert_eq!(
            terminal.backend().clear_calls,
            vec![(Position { x: 0, y: 4 }, ClearType::AfterCursor)]
        );
    }

    #[test]
    fn replay_reflow_bottom_pins_and_purges_when_history_overflows() {
        let backend = RecordingBackend::new(Size::new(20, 8));
        let mut terminal = custom_terminal::Terminal::with_options(backend).expect("terminal");
        terminal.set_bottom_pinned(true);
        terminal.set_viewport_area(Rect::new(0, 3, 20, 5));

        // Ten history rows cannot fit above a 5-row viewport on an 8-row screen,
        // so the replay must bottom-pin and scroll overflow into scrollback.
        let lines: Vec<HistoryLine> = (0..10)
            .map(|i| HistoryLine::normal(format!("row{i:02}")))
            .collect();

        replay_reflow(&mut terminal, &lines, 20, 5).expect("reflow");

        assert!(terminal.is_bottom_pinned());
        assert_eq!(terminal.viewport_area, Rect::new(0, 3, 20, 5));

        let out = String::from_utf8_lossy(&terminal.backend().writes);
        assert!(out.contains("\x1b[3J"), "purges scrollback");
        assert!(out.contains("\x1b[2J"), "clears the screen");
        assert!(
            out.contains("row00") && out.contains("row09"),
            "replays rows"
        );
    }

    #[test]
    fn replay_reflow_stays_top_anchored_when_history_fits() {
        let backend = RecordingBackend::new(Size::new(20, 12));
        let mut terminal = custom_terminal::Terminal::with_options(backend).expect("terminal");
        terminal.set_viewport_area(Rect::new(0, 2, 20, 3));

        // Three history rows fit above a 3-row viewport on a 12-row screen, so
        // the replay paints them in place and keeps the viewport top-anchored
        // just below them.
        let lines: Vec<HistoryLine> = (0..3)
            .map(|i| HistoryLine::normal(format!("line{i}")))
            .collect();

        replay_reflow(&mut terminal, &lines, 20, 3).expect("reflow");

        assert!(!terminal.is_bottom_pinned());
        assert_eq!(terminal.viewport_area, Rect::new(0, 3, 20, 3));
        assert_eq!(terminal.visible_history_rows(), 3);

        let out = String::from_utf8_lossy(&terminal.backend().writes);
        assert!(out.contains("\x1b[3J") && out.contains("\x1b[2J"));
        assert!(out.contains("line0") && out.contains("line2"));
    }

    #[test]
    fn bottom_pinned_bulk_shrink_bottom_aligns_history() {
        // A whole turn finalized at once floods the queue with more lines than
        // fit above the shrunken viewport (n > new_top). The history must end
        // flush against the new viewport top, leaving no blank gap above the
        // composer — the new viewport is cleared at its own top (y = 8), not at
        // the old top (y = 2).
        let backend = RecordingBackend::new(Size::new(20, 12));
        let mut terminal = custom_terminal::Terminal::with_options(backend).expect("terminal");
        terminal.set_bottom_pinned(true);
        terminal.set_viewport_area(Rect::new(0, 2, 20, 10));

        let mut app = test_app();
        app.log.start_turn("hello"); // one user line: "> hello"
        let body: String = (0..14).map(|i| format!("line{i}\n")).collect();
        app.log.push_assistant_delta(&body); // 14 finalized lines
        app.log.finalize_all(); // 15 lines queued, > new_top (8)

        let mut area = terminal.viewport_area;
        area.height = 4; // shrink: new_top = 12 - 4 = 8
        reconcile_bottom_pinned(&mut terminal, &mut area, Size::new(20, 12), &mut app)
            .expect("reconcile");

        assert_eq!(terminal.viewport_area, Rect::new(0, 8, 20, 4));
        assert_eq!(
            terminal.backend().clear_calls,
            vec![(Position { x: 0, y: 8 }, ClearType::AfterCursor)]
        );
        // The newest line is replayed (the block is written flush to the new top)
        // and the oldest overflow lines still reach scrollback.
        let out = String::from_utf8_lossy(&terminal.backend().writes);
        assert!(out.contains("line13"), "newest line painted");
        assert!(out.contains("line0"), "oldest overflow still emitted");
    }

    fn test_app() -> AppState {
        AppState::new("test-model".to_string(), PathBuf::from("/tmp/project"))
    }

    #[derive(Debug)]
    struct RecordingBackend {
        size: Size,
        cursor: Position,
        clear_calls: Vec<(Position, ClearType)>,
        writes: Vec<u8>,
    }

    impl RecordingBackend {
        fn new(size: Size) -> Self {
            Self {
                size,
                cursor: Position { x: 0, y: 0 },
                clear_calls: Vec::new(),
                writes: Vec::new(),
            }
        }
    }

    impl Backend for RecordingBackend {
        fn draw<'a, I>(&mut self, content: I) -> io::Result<()>
        where
            I: Iterator<Item = (u16, u16, &'a Cell)>,
        {
            for _ in content {}
            Ok(())
        }

        fn hide_cursor(&mut self) -> io::Result<()> {
            Ok(())
        }

        fn show_cursor(&mut self) -> io::Result<()> {
            Ok(())
        }

        fn get_cursor_position(&mut self) -> io::Result<Position> {
            Ok(self.cursor)
        }

        fn set_cursor_position<P: Into<Position>>(&mut self, position: P) -> io::Result<()> {
            self.cursor = position.into();
            Ok(())
        }

        fn clear(&mut self) -> io::Result<()> {
            self.clear_calls.push((self.cursor, ClearType::All));
            Ok(())
        }

        fn clear_region(&mut self, clear_type: ClearType) -> io::Result<()> {
            self.clear_calls.push((self.cursor, clear_type));
            Ok(())
        }

        fn size(&self) -> io::Result<Size> {
            Ok(self.size)
        }

        fn window_size(&mut self) -> io::Result<WindowSize> {
            Ok(WindowSize {
                columns_rows: self.size,
                pixels: Size::new(0, 0),
            })
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl Write for RecordingBackend {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.writes.extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
}
