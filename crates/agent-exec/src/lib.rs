//! Non-interactive poe frontend.

use std::{
    error::Error,
    fmt, fs,
    io::{self, Write},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::{SystemTime, UNIX_EPOCH},
};

use agent_core::{
    AgentError, AgentSession, ModelClient, ModelError, ModelEvent, ModelEventStream,
    ModelToolResult, ModelTurnInput, available_tool_definitions,
};
use agent_protocol::{Event, Op, ToolCall, ToolOutputStream, ToolResult, TranscriptMessage};
use futures_util::StreamExt;
use futures_util::stream;
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecOptions {
    pub prompt: String,
    pub cwd: PathBuf,
    pub json: bool,
    /// When set, the session transcript is persisted as JSON in this directory
    /// once the session ends. `None` disables session persistence.
    pub sessions_dir: Option<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecOutput {
    pub stdout: String,
    pub stderr: String,
    pub exit_code: i32,
}

pub async fn run_scripted(options: ExecOptions) -> Result<ExecOutput, ExecError> {
    run_with_model(options, ScriptedModel::default()).await
}

pub async fn run_with_model<M>(options: ExecOptions, model: M) -> Result<ExecOutput, ExecError>
where
    M: ModelClient,
{
    let ExecOptions {
        prompt,
        cwd,
        json,
        sessions_dir,
    } = options;

    let model_slug = model.model_slug();
    let mut session = AgentSession::new(model);
    let mut events = session
        .submit(Op::UserTurn {
            prompt: prompt.clone(),
            cwd: cwd.clone(),
        })
        .await?;

    let output = render_events(&mut events, json, &model_slug, &cwd).await?;

    let model = session.into_model();
    persist_trace(
        sessions_dir.as_deref(),
        prompt,
        cwd,
        model_slug,
        model.transcript(),
        model.tool_definitions(),
    )?;

    Ok(output)
}

pub async fn run_with_model_live<M, W, E>(
    options: ExecOptions,
    model: M,
    stdout: &mut W,
    stderr: &mut E,
) -> Result<i32, ExecError>
where
    M: ModelClient,
    W: Write,
    E: Write,
{
    let ExecOptions {
        prompt,
        cwd,
        json,
        sessions_dir,
    } = options;

    let model_slug = model.model_slug();
    let mut session = AgentSession::new(model);
    let mut events = session
        .submit(Op::UserTurn {
            prompt: prompt.clone(),
            cwd: cwd.clone(),
        })
        .await?;

    let exit_code =
        render_events_live(&mut events, json, &model_slug, &cwd, stdout, stderr).await?;

    let model = session.into_model();
    persist_trace(
        sessions_dir.as_deref(),
        prompt,
        cwd,
        model_slug,
        model.transcript(),
        model.tool_definitions(),
    )?;

    Ok(exit_code)
}

async fn render_events(
    events: &mut agent_core::EventStream,
    json: bool,
    model: &str,
    cwd: &Path,
) -> Result<ExecOutput, ExecError> {
    if json {
        render_json_events(events).await
    } else {
        Ok(render_human_events(events, model, cwd).await)
    }
}

async fn render_human_events(
    events: &mut agent_core::EventStream,
    model: &str,
    cwd: &Path,
) -> ExecOutput {
    let mut stdout = String::new();
    let mut stderr = format_exec_header(model, cwd);
    let mut exit_code = 0;

    while let Some(event) = events.next().await {
        match event {
            Event::AssistantDelta { text } => stdout.push_str(&text),
            Event::ToolStarted { call } => {
                stderr.push_str(&format_tool_started(&call));
                stderr.push('\n');
            }
            Event::ToolOutput { stream, chunk, .. } => {
                stderr.push_str(&format_tool_output(stream, &chunk));
            }
            Event::ToolFinished { id: _, result } => {
                stderr.push_str(&format_tool_finished(&result));
                stderr.push('\n');
            }
            Event::TurnInterrupted => {
                stderr.push_str("interrupted\n");
            }
            Event::TurnFailed { message } => {
                stderr.push_str(&message);
                stderr.push('\n');
                exit_code = 1;
            }
            _ => {}
        }
    }

    ExecOutput {
        stdout,
        stderr,
        exit_code,
    }
}

async fn render_json_events(events: &mut agent_core::EventStream) -> Result<ExecOutput, ExecError> {
    let mut stdout = String::new();
    let mut exit_code = 0;

    while let Some(event) = events.next().await {
        stdout.push_str(&serde_json::to_string(&event)?);
        stdout.push('\n');

        if matches!(event, Event::TurnFailed { .. }) {
            exit_code = 1;
        }
    }

    Ok(ExecOutput {
        stdout,
        stderr: String::new(),
        exit_code,
    })
}

async fn render_events_live<W, E>(
    events: &mut agent_core::EventStream,
    json: bool,
    model: &str,
    cwd: &Path,
    stdout: &mut W,
    stderr: &mut E,
) -> Result<i32, ExecError>
where
    W: Write,
    E: Write,
{
    if json {
        render_json_events_live(events, stdout).await
    } else {
        render_human_events_live(events, model, cwd, stdout, stderr).await
    }
}

async fn render_human_events_live<W, E>(
    events: &mut agent_core::EventStream,
    model: &str,
    cwd: &Path,
    stdout: &mut W,
    stderr: &mut E,
) -> Result<i32, ExecError>
where
    W: Write,
    E: Write,
{
    let mut exit_code = 0;

    stderr.write_all(format_exec_header(model, cwd).as_bytes())?;
    stderr.flush()?;

    while let Some(event) = events.next().await {
        match event {
            Event::AssistantDelta { text } => {
                stdout.write_all(text.as_bytes())?;
                stdout.flush()?;
            }
            Event::ToolStarted { call } => {
                writeln!(stderr, "{}", format_tool_started(&call))?;
                stderr.flush()?;
            }
            Event::ToolOutput { stream, chunk, .. } => {
                stderr.write_all(format_tool_output(stream, &chunk).as_bytes())?;
                stderr.flush()?;
            }
            Event::ToolFinished { id: _, result } => {
                writeln!(stderr, "{}", format_tool_finished(&result))?;
                stderr.flush()?;
            }
            Event::TurnInterrupted => {
                stderr.write_all(b"interrupted\n")?;
                stderr.flush()?;
            }
            Event::TurnFailed { message } => {
                stderr.write_all(message.as_bytes())?;
                stderr.write_all(b"\n")?;
                stderr.flush()?;
                exit_code = 1;
            }
            _ => {}
        }
    }

    Ok(exit_code)
}

fn format_exec_header(model: &str, cwd: &Path) -> String {
    format!(
        "--------\nmodel: {}\nworkdir: {}\n--------\n",
        model,
        cwd.display()
    )
}

fn format_tool_started(call: &ToolCall) -> String {
    format!("[tool started]: {}", describe_tool_call(call))
}

fn format_tool_finished(result: &ToolResult) -> String {
    match result.exit_code {
        Some(code) => format!("[tool finished]: exit {code}"),
        None => "[tool finished]".to_string(),
    }
}

fn format_tool_output(stream: ToolOutputStream, chunk: &str) -> String {
    let label = match stream {
        ToolOutputStream::Stdout => "tool stdout",
        ToolOutputStream::Stderr => "tool stderr",
    };

    chunk
        .lines()
        .map(|line| format!("{label}: {line}\n"))
        .collect()
}

fn describe_tool_call(call: &ToolCall) -> String {
    match call.name.as_str() {
        "shell" => describe_json_string_arg(call, "command")
            .map(|command| format!("shell: {command}"))
            .unwrap_or_else(|| "shell".to_string()),
        "read_file" => describe_json_string_arg(call, "file_path")
            .map(|path| format!("read_file: {path}"))
            .unwrap_or_else(|| "read_file".to_string()),
        "list_dir" => describe_json_string_arg(call, "dir_path")
            .map(|path| format!("list_dir: {path}"))
            .unwrap_or_else(|| "list_dir".to_string()),
        "edit_file" => describe_json_string_arg(call, "file_path")
            .map(|path| format!("edit_file: {path}"))
            .unwrap_or_else(|| "edit_file".to_string()),
        "write_file" => describe_json_string_arg(call, "file_path")
            .map(|path| format!("write_file: {path}"))
            .unwrap_or_else(|| "write_file".to_string()),
        name => name.to_string(),
    }
}

fn describe_json_string_arg(call: &ToolCall, key: &str) -> Option<String> {
    call.input
        .get(key)
        .and_then(Value::as_str)
        .map(ToString::to_string)
}

async fn render_json_events_live<W>(
    events: &mut agent_core::EventStream,
    stdout: &mut W,
) -> Result<i32, ExecError>
where
    W: Write,
{
    let mut exit_code = 0;

    while let Some(event) = events.next().await {
        serde_json::to_writer(&mut *stdout, &event)?;
        stdout.write_all(b"\n")?;
        stdout.flush()?;

        if matches!(event, Event::TurnFailed { .. }) {
            exit_code = 1;
        }
    }

    Ok(exit_code)
}

/// A persisted record of one agent-exec session: the originating request and
/// the full conversation transcript (system prompt, user messages, assistant
/// replies, tool calls, and tool results) the model exchanged.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionTrace {
    pub prompt: String,
    pub cwd: PathBuf,
    #[serde(default)]
    pub model: String,
    #[serde(default)]
    pub tools: Vec<Value>,
    pub messages: Vec<TranscriptMessage>,
}

/// Writes the session trace into `sessions_dir` when persistence is enabled.
///
/// A `None` directory disables persistence. The directory (and any missing
/// parents) is created on demand.
fn persist_trace(
    sessions_dir: Option<&Path>,
    prompt: String,
    cwd: PathBuf,
    model: String,
    messages: Vec<TranscriptMessage>,
    tools: Vec<Value>,
) -> Result<(), ExecError> {
    let Some(sessions_dir) = sessions_dir else {
        return Ok(());
    };

    let trace = SessionTrace {
        prompt,
        cwd,
        model,
        tools,
        messages,
    };
    save_session_trace(sessions_dir, &trace)?;
    Ok(())
}

/// Serializes `trace` to a uniquely named JSON file inside `sessions_dir`,
/// returning the path that was written.
pub fn save_session_trace(sessions_dir: &Path, trace: &SessionTrace) -> Result<PathBuf, ExecError> {
    fs::create_dir_all(sessions_dir)?;

    let path = sessions_dir.join(session_trace_file_name());
    let contents = serde_json::to_string_pretty(trace)?;
    fs::write(&path, contents)?;

    Ok(path)
}

/// Builds a sortable, collision-resistant trace file name.
///
/// The leading millisecond timestamp keeps files chronologically ordered. The
/// process id disambiguates concurrent processes (two `exec` runs in the same
/// millisecond), and a process-lifetime counter disambiguates repeated saves
/// within one process, so no save ever overwrites an earlier file.
fn session_trace_file_name() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);

    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let seq = COUNTER.fetch_add(1, Ordering::Relaxed);

    format!(
        "session-{:013}-{}-{seq}.json",
        now.as_millis(),
        std::process::id()
    )
}

#[derive(Clone)]
pub struct ScriptedModel {
    response: Result<Vec<Result<ModelEvent, ModelError>>, ModelError>,
    transcript: Arc<Mutex<Vec<TranscriptMessage>>>,
}

impl ScriptedModel {
    pub fn success(text: impl Into<String>) -> Self {
        Self::new(Ok(vec![Ok(ModelEvent::AssistantDelta {
            text: text.into(),
        })]))
    }

    pub fn stream(events: Vec<Result<ModelEvent, ModelError>>) -> Self {
        Self::new(Ok(events))
    }

    pub fn failure(message: impl Into<String>) -> Self {
        Self::new(Err(ModelError::new(message)))
    }

    fn new(response: Result<Vec<Result<ModelEvent, ModelError>>, ModelError>) -> Self {
        Self {
            response,
            transcript: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// Records the user turn and the scripted assistant reply into the
    /// transcript, mirroring what a real client accumulates as it streams.
    fn record_turn(&self, prompt: String) {
        let Ok(mut transcript) = self.transcript.lock() else {
            return;
        };

        transcript.push(TranscriptMessage::User { content: prompt });

        let Ok(events) = &self.response else {
            return;
        };

        let mut content = String::new();
        let mut reasoning_content = String::new();
        let mut tool_calls = Vec::new();
        for event in events.iter().flatten() {
            match event {
                ModelEvent::AssistantDelta { text } => content.push_str(text),
                ModelEvent::ThinkingDelta { text } => reasoning_content.push_str(text),
                // Usage accounting is not part of the persisted assistant message.
                ModelEvent::Usage(_) => {}
                ModelEvent::ToolCall(call) => tool_calls.push(ToolCall {
                    id: call.id.clone(),
                    name: call.name.clone(),
                    input: call.input.clone(),
                }),
            }
        }

        if !content.is_empty() || !reasoning_content.is_empty() || !tool_calls.is_empty() {
            transcript.push(TranscriptMessage::Assistant {
                reasoning_content: (!reasoning_content.is_empty()).then_some(reasoning_content),
                content: (!content.is_empty()).then_some(content),
                tool_calls,
            });
        }
    }
}

impl Default for ScriptedModel {
    fn default() -> Self {
        Self::success("hello from scripted model\n")
    }
}

impl ModelClient for ScriptedModel {
    fn stream_turn(&mut self, input: ModelTurnInput) -> Result<ModelEventStream, ModelError> {
        self.record_turn(input.prompt);
        self.response
            .clone()
            .map(|events| stream::iter(events).boxed())
    }

    fn stream_tool_results(
        &mut self,
        _results: Vec<ModelToolResult>,
    ) -> Result<ModelEventStream, ModelError> {
        Ok(stream::iter(Vec::new()).boxed())
    }

    fn transcript(&self) -> Vec<TranscriptMessage> {
        self.transcript
            .lock()
            .map(|transcript| transcript.clone())
            .unwrap_or_default()
    }

    fn tool_definitions(&self) -> Vec<Value> {
        available_tool_definitions()
    }

    fn model_slug(&self) -> String {
        "scripted".to_string()
    }
}

#[derive(Debug)]
pub enum ExecError {
    Agent(AgentError),
    Io(io::Error),
    Json(serde_json::Error),
}

impl fmt::Display for ExecError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Agent(error) => write!(formatter, "{error}"),
            Self::Io(error) => write!(formatter, "{error}"),
            Self::Json(error) => write!(formatter, "{error}"),
        }
    }
}

impl Error for ExecError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Agent(error) => Some(error),
            Self::Io(error) => Some(error),
            Self::Json(error) => Some(error),
        }
    }
}

impl From<AgentError> for ExecError {
    fn from(error: AgentError) -> Self {
        Self::Agent(error)
    }
}

impl From<io::Error> for ExecError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<serde_json::Error> for ExecError {
    fn from(error: serde_json::Error) -> Self {
        Self::Json(error)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_core::ModelToolCall;
    use std::future::Future;

    #[test]
    fn human_mode_prints_final_assistant_text_to_stdout() {
        let output = run(run_with_model(
            options("say hello", false),
            ScriptedModel::success("hello\n"),
        ))
        .expect("run exec");

        assert_eq!(
            output,
            ExecOutput {
                stdout: "hello\n".to_string(),
                stderr: expected_exec_header(Path::new("/tmp/project")),
                exit_code: 0,
            }
        );
    }

    #[test]
    fn human_mode_preserves_streamed_delta_order() {
        let output = run(run_with_model(
            options("say hello", false),
            ScriptedModel::stream(vec![
                Ok(ModelEvent::AssistantDelta {
                    text: "hello".to_string(),
                }),
                Ok(ModelEvent::AssistantDelta {
                    text: " async\n".to_string(),
                }),
            ]),
        ))
        .expect("run exec");

        assert_eq!(
            output,
            ExecOutput {
                stdout: "hello async\n".to_string(),
                stderr: expected_exec_header(Path::new("/tmp/project")),
                exit_code: 0,
            }
        );
    }

    #[test]
    fn human_mode_prints_stream_failures_to_stderr() {
        let output = run(run_with_model(
            options("say hello", false),
            ScriptedModel::stream(vec![
                Ok(ModelEvent::AssistantDelta {
                    text: "partial".to_string(),
                }),
                Err(ModelError::new("stream failed")),
            ]),
        ))
        .expect("run exec");

        assert_eq!(
            output,
            ExecOutput {
                stdout: "partial".to_string(),
                stderr: format!(
                    "{}stream failed\n",
                    expected_exec_header(Path::new("/tmp/project"))
                ),
                exit_code: 1,
            }
        );
    }

    #[test]
    fn human_mode_prints_tool_activity_to_stderr() {
        let temp = TempDir::new();
        let output = run(run_with_model(
            ExecOptions {
                prompt: "run command".to_string(),
                cwd: temp.path().to_path_buf(),
                json: false,
                sessions_dir: None,
            },
            ScriptedModel::stream(vec![Ok(ModelEvent::ToolCall(ModelToolCall {
                id: "call-1".to_string(),
                name: "shell".to_string(),
                input: serde_json::json!({ "command": "printf ok" }),
            }))]),
        ))
        .expect("run exec");

        assert_eq!(
            output,
            ExecOutput {
                stdout: String::new(),
                stderr: format!(
                    "{}[tool started]: shell: printf ok\n[tool finished]: exit 0\n",
                    expected_exec_header(temp.path())
                ),
                exit_code: 0,
            }
        );
    }

    #[test]
    fn human_mode_names_file_tool_targets() {
        let temp = TempDir::new();
        let file_path = temp.path().join("sample.txt");
        fs::write(&file_path, "hello\n").expect("write file");

        let output = run(run_with_model(
            ExecOptions {
                prompt: "read file".to_string(),
                cwd: temp.path().to_path_buf(),
                json: false,
                sessions_dir: None,
            },
            ScriptedModel::stream(vec![Ok(ModelEvent::ToolCall(ModelToolCall {
                id: "call-1".to_string(),
                name: "read_file".to_string(),
                input: serde_json::json!({ "file_path": file_path }),
            }))]),
        ))
        .expect("run exec");

        assert_eq!(output.stdout, "");
        assert_eq!(output.stderr.lines().count(), 6);
        assert!(
            output
                .stderr
                .starts_with(&expected_exec_header(temp.path()))
        );
        assert!(output.stderr.contains(&format!(
            "[tool started]: read_file: {}",
            file_path.display()
        )));
        assert!(output.stderr.ends_with("[tool finished]\n"));
        assert_eq!(output.exit_code, 0);
    }

    #[test]
    fn human_mode_names_write_file_target() {
        let temp = TempDir::new();
        let output = run(run_with_model(
            ExecOptions {
                prompt: "write file".to_string(),
                cwd: temp.path().to_path_buf(),
                json: false,
                sessions_dir: None,
            },
            ScriptedModel::stream(vec![Ok(ModelEvent::ToolCall(ModelToolCall {
                id: "call-1".to_string(),
                name: "write_file".to_string(),
                input: serde_json::json!({
                    "file_path": "sample.txt",
                    "content": "hello\n"
                }),
            }))]),
        ))
        .expect("run exec");

        assert_eq!(output.stdout, "");
        assert_eq!(
            output.stderr,
            format!(
                "{}[tool started]: write_file: sample.txt\n[tool finished]\n",
                expected_exec_header(temp.path())
            )
        );
        assert_eq!(output.exit_code, 0);
        assert_eq!(
            fs::read_to_string(temp.path().join("sample.txt")).expect("read written file"),
            "hello\n"
        );
    }

    #[test]
    fn human_mode_prints_initial_model_failures_to_stderr() {
        let output = run(run_with_model(
            options("say hello", false),
            ScriptedModel::failure("model unavailable"),
        ))
        .expect("run exec");

        assert_eq!(
            output,
            ExecOutput {
                stdout: String::new(),
                stderr: format!(
                    "{}model unavailable\n",
                    expected_exec_header(Path::new("/tmp/project"))
                ),
                exit_code: 1,
            }
        );
    }

    #[test]
    fn json_mode_prints_one_event_per_stdout_line() {
        let output = run(run_with_model(
            options("say hello", true),
            ScriptedModel::success("hello\n"),
        ))
        .expect("run exec");

        let lines = output.stdout.lines().collect::<Vec<_>>();

        assert_eq!(lines.len(), 3);
        assert_eq!(output.stderr, "");
        assert_eq!(output.exit_code, 0);
        assert_eq!(
            serde_json::from_str::<Event>(lines[0]).expect("session started event"),
            Event::SessionStarted
        );
        assert_eq!(
            serde_json::from_str::<Event>(lines[1]).expect("assistant delta event"),
            Event::AssistantDelta {
                text: "hello\n".to_string()
            }
        );
        assert_eq!(
            serde_json::from_str::<Event>(lines[2]).expect("turn complete event"),
            Event::TurnComplete
        );
    }

    #[test]
    fn json_mode_stream_failures_exit_nonzero_without_stderr_text() {
        let output = run(run_with_model(
            options("say hello", true),
            ScriptedModel::stream(vec![Err(ModelError::new("stream failed"))]),
        ))
        .expect("run exec");

        let lines = output.stdout.lines().collect::<Vec<_>>();

        assert_eq!(lines.len(), 2);
        assert_eq!(output.stderr, "");
        assert_eq!(output.exit_code, 1);
        assert_eq!(
            serde_json::from_str::<Event>(lines[0]).expect("session started event"),
            Event::SessionStarted
        );
        assert_eq!(
            serde_json::from_str::<Event>(lines[1]).expect("turn failed event"),
            Event::TurnFailed {
                message: "stream failed".to_string()
            }
        );
    }

    #[test]
    fn live_human_mode_writes_deltas_to_stdout_and_failures_to_stderr() {
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();

        let exit_code = run(run_with_model_live(
            options("say hello", false),
            ScriptedModel::stream(vec![
                Ok(ModelEvent::AssistantDelta {
                    text: "partial".to_string(),
                }),
                Err(ModelError::new("stream failed")),
            ]),
            &mut stdout,
            &mut stderr,
        ))
        .expect("run live exec");

        assert_eq!(exit_code, 1);
        assert_eq!(String::from_utf8(stdout).expect("stdout utf8"), "partial");
        assert_eq!(
            String::from_utf8(stderr).expect("stderr utf8"),
            format!(
                "{}stream failed\n",
                expected_exec_header(Path::new("/tmp/project"))
            )
        );
    }

    #[test]
    fn live_human_mode_writes_tool_activity_to_stderr() {
        let temp = TempDir::new();
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();

        let exit_code = run(run_with_model_live(
            ExecOptions {
                prompt: "run command".to_string(),
                cwd: temp.path().to_path_buf(),
                json: false,
                sessions_dir: None,
            },
            ScriptedModel::stream(vec![Ok(ModelEvent::ToolCall(ModelToolCall {
                id: "call-1".to_string(),
                name: "shell".to_string(),
                input: serde_json::json!({ "command": "printf ok" }),
            }))]),
            &mut stdout,
            &mut stderr,
        ))
        .expect("run live exec");

        assert_eq!(exit_code, 0);
        assert_eq!(stdout, Vec::<u8>::new());
        assert_eq!(
            String::from_utf8(stderr).expect("stderr utf8"),
            format!(
                "{}[tool started]: shell: printf ok\n[tool finished]: exit 0\n",
                expected_exec_header(temp.path())
            )
        );
    }

    #[test]
    fn live_json_mode_writes_events_to_stdout_only() {
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();

        let exit_code = run(run_with_model_live(
            options("say hello", true),
            ScriptedModel::success("hello\n"),
            &mut stdout,
            &mut stderr,
        ))
        .expect("run live exec");

        let stdout = String::from_utf8(stdout).expect("stdout utf8");
        let lines = stdout.lines().collect::<Vec<_>>();

        assert_eq!(exit_code, 0);
        assert_eq!(stderr, Vec::<u8>::new());
        assert_eq!(lines.len(), 3);
        assert_eq!(
            serde_json::from_str::<Event>(lines[0]).expect("session started event"),
            Event::SessionStarted
        );
        assert_eq!(
            serde_json::from_str::<Event>(lines[1]).expect("assistant delta event"),
            Event::AssistantDelta {
                text: "hello\n".to_string()
            }
        );
        assert_eq!(
            serde_json::from_str::<Event>(lines[2]).expect("turn complete event"),
            Event::TurnComplete
        );
    }

    #[test]
    fn session_trace_persists_conversation_transcript_not_events() {
        let temp = TempDir::new();
        let sessions_dir = temp.path().join("sessions");

        let exit_code = run(run_with_model_live(
            ExecOptions {
                prompt: "say hello".to_string(),
                cwd: PathBuf::from("/tmp/project"),
                json: false,
                sessions_dir: Some(sessions_dir.clone()),
            },
            ScriptedModel::success("hello\n"),
            &mut Vec::new(),
            &mut Vec::new(),
        ))
        .expect("run live exec");

        assert_eq!(exit_code, 0);

        let traces = read_traces(&sessions_dir);
        assert_eq!(traces.len(), 1, "expected exactly one trace file");

        let trace = &traces[0];
        assert_eq!(trace.prompt, "say hello");
        assert_eq!(trace.cwd, PathBuf::from("/tmp/project"));
        assert_eq!(trace.model, "scripted");
        assert_eq!(
            tool_names(&trace.tools),
            vec!["read_file", "list_dir", "shell", "edit_file", "write_file"]
        );
        assert_eq!(
            trace.messages,
            vec![
                TranscriptMessage::User {
                    content: "say hello".to_string()
                },
                TranscriptMessage::Assistant {
                    reasoning_content: None,
                    content: Some("hello\n".to_string()),
                    tool_calls: Vec::new(),
                },
            ]
        );
    }

    #[test]
    fn session_trace_persists_reasoning_content() {
        let temp = TempDir::new();
        let sessions_dir = temp.path().join("sessions");

        let exit_code = run(run_with_model_live(
            ExecOptions {
                prompt: "think then answer".to_string(),
                cwd: PathBuf::from("/tmp/project"),
                json: false,
                sessions_dir: Some(sessions_dir.clone()),
            },
            ScriptedModel::stream(vec![
                Ok(ModelEvent::ThinkingDelta {
                    text: "first ".to_string(),
                }),
                Ok(ModelEvent::ThinkingDelta {
                    text: "reason".to_string(),
                }),
                Ok(ModelEvent::AssistantDelta {
                    text: "answer\n".to_string(),
                }),
            ]),
            &mut Vec::new(),
            &mut Vec::new(),
        ))
        .expect("run live exec");

        assert_eq!(exit_code, 0);

        let traces = read_traces(&sessions_dir);
        assert_eq!(traces.len(), 1, "expected exactly one trace file");
        assert_eq!(
            traces[0].messages,
            vec![
                TranscriptMessage::User {
                    content: "think then answer".to_string()
                },
                TranscriptMessage::Assistant {
                    reasoning_content: Some("first reason".to_string()),
                    content: Some("answer\n".to_string()),
                    tool_calls: Vec::new(),
                },
            ]
        );
    }

    #[test]
    fn session_trace_is_written_even_when_the_turn_fails() {
        let temp = TempDir::new();
        let sessions_dir = temp.path().join("sessions");

        let exit_code = run(run_with_model_live(
            ExecOptions {
                prompt: "explode".to_string(),
                cwd: PathBuf::from("/tmp/project"),
                json: true,
                sessions_dir: Some(sessions_dir.clone()),
            },
            ScriptedModel::stream(vec![Err(ModelError::new("stream failed"))]),
            &mut Vec::new(),
            &mut Vec::new(),
        ))
        .expect("run live exec");

        assert_eq!(exit_code, 1);

        let traces = read_traces(&sessions_dir);
        assert_eq!(traces.len(), 1, "trace should be saved on failure too");
        assert_eq!(traces[0].model, "scripted");
        assert_eq!(
            tool_names(&traces[0].tools),
            vec!["read_file", "list_dir", "shell", "edit_file", "write_file"]
        );
        // A failed turn produced no assistant reply, only the user message.
        assert_eq!(
            traces[0].messages,
            vec![TranscriptMessage::User {
                content: "explode".to_string()
            }]
        );
    }

    #[test]
    fn no_trace_is_written_when_sessions_dir_is_none() {
        let temp = TempDir::new();
        let sessions_dir = temp.path().join("sessions");

        run(run_with_model(
            ExecOptions {
                prompt: "say hello".to_string(),
                cwd: PathBuf::from("/tmp/project"),
                json: false,
                sessions_dir: None,
            },
            ScriptedModel::success("hello\n"),
        ))
        .expect("run exec");

        assert!(!sessions_dir.exists(), "sessions dir should not be created");
    }

    #[test]
    fn session_trace_deserializes_without_tools_for_older_files() {
        let trace = serde_json::from_value::<SessionTrace>(serde_json::json!({
            "prompt": "say hello",
            "cwd": "/tmp/project",
            "messages": [
                {
                    "role": "user",
                    "content": "say hello"
                }
            ]
        }))
        .expect("deserialize old trace");

        assert_eq!(trace.tools, Vec::<serde_json::Value>::new());
        assert_eq!(trace.model, "");
        assert_eq!(trace.prompt, "say hello");
    }

    fn options(prompt: &str, json: bool) -> ExecOptions {
        ExecOptions {
            prompt: prompt.to_string(),
            cwd: PathBuf::from("/tmp/project"),
            json,
            sessions_dir: None,
        }
    }

    fn expected_exec_header(cwd: &Path) -> String {
        format_exec_header("scripted", cwd)
    }

    fn read_traces(sessions_dir: &Path) -> Vec<SessionTrace> {
        let mut traces = fs::read_dir(sessions_dir)
            .expect("read sessions dir")
            .map(|entry| entry.expect("dir entry").path())
            .filter(|path| path.extension().is_some_and(|ext| ext == "json"))
            .map(|path| {
                let contents = fs::read_to_string(&path).expect("read trace file");
                serde_json::from_str::<SessionTrace>(&contents).expect("parse trace")
            })
            .collect::<Vec<_>>();
        traces.sort_by(|a, b| a.prompt.cmp(&b.prompt));
        traces
    }

    fn tool_names(tools: &[Value]) -> Vec<&str> {
        tools
            .iter()
            .map(|tool| {
                tool["function"]["name"]
                    .as_str()
                    .expect("tool function name")
            })
            .collect()
    }

    struct TempDir {
        path: PathBuf,
    }

    impl TempDir {
        fn new() -> Self {
            use std::sync::atomic::{AtomicU64, Ordering};

            static COUNTER: AtomicU64 = AtomicU64::new(0);

            let unique = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("system time")
                .as_nanos();
            let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "poe-agent-exec-test-{}-{unique}-{seq}",
                std::process::id()
            ));
            fs::create_dir(&path).expect("create temp dir");
            Self { path }
        }

        fn path(&self) -> &PathBuf {
            &self.path
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    fn run<T>(future: impl Future<Output = T>) -> T {
        futures_executor::block_on(future)
    }
}
