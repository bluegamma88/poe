//! Core agent session engine.

use std::{
    error::Error,
    fmt, fs,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
};

use agent_protocol::{Event, Op, TokenUsage, ToolCall, ToolResult, TranscriptMessage};
use agent_tools::{
    edit_file, edit_file_tool_schema, list_dir, list_dir_tool_schema, read_file,
    read_file_tool_schema, shell, shell_tool_schema, write_file, write_file_tool_schema,
};
use futures_util::StreamExt;
use futures_util::stream::BoxStream;
use reqwest::header::{AUTHORIZATION, CONTENT_TYPE, HeaderMap, HeaderValue};
use serde::{Deserialize, Serialize};
use serde_json::Value;

const MAX_TOOL_ROUNDS: usize = 250;
const SYSTEM_PROMPT: &str = "You are a coding agent.";

pub fn available_tool_definitions() -> Vec<Value> {
    vec![
        read_file_tool_schema(),
        list_dir_tool_schema(),
        shell_tool_schema(),
        edit_file_tool_schema(),
        write_file_tool_schema(),
    ]
}

pub struct AgentSession<M> {
    model: M,
    started: bool,
    /// Set by `Op::Interrupt` to request cancellation of the in-flight turn.
    /// Shared with the running turn stream, which observes it at each loop
    /// boundary and ends the turn with `Event::TurnInterrupted`.
    cancel: Arc<AtomicBool>,
}

impl<M> AgentSession<M> {
    pub fn new(model: M) -> Self {
        Self {
            model,
            started: false,
            cancel: Arc::new(AtomicBool::new(false)),
        }
    }

    pub fn into_model(self) -> M {
        self.model
    }
}

impl<M> AgentSession<M>
where
    M: ModelClient,
{
    pub async fn submit(&mut self, op: Op) -> Result<EventStream, AgentError> {
        match op {
            Op::UserTurn { prompt, cwd } => self.submit_user_turn(prompt, cwd),
            Op::Interrupt => Ok(self.interrupt()),
            Op::Compact => Err(AgentError::unsupported_op("compact")),
            Op::Shutdown => Err(AgentError::unsupported_op("shutdown")),
        }
    }

    /// Signals the in-flight turn to stop. The running turn stream observes the
    /// flag and ends with `Event::TurnInterrupted`; this op itself emits no
    /// events, so the returned stream is empty. A no-op when nothing is running.
    fn interrupt(&mut self) -> EventStream {
        self.cancel.store(true, Ordering::SeqCst);
        Box::pin(futures_util::stream::empty())
    }

    fn submit_user_turn(
        &mut self,
        prompt: String,
        cwd: PathBuf,
    ) -> Result<EventStream, AgentError> {
        let emit_session_started = !self.started;
        self.started = true;

        // Clear any stale interrupt request from a previous turn, then share the
        // flag with the turn stream so `Op::Interrupt` can cancel it.
        self.cancel.store(false, Ordering::SeqCst);
        let cancel = Arc::clone(&self.cancel);

        let tool_cwd = cwd.clone();
        let input = ModelTurnInput { prompt, cwd };
        let mut model = self.model.clone();
        let mut next_stream = model.stream_turn(input);

        Ok(Box::pin(async_stream::stream! {
            if emit_session_started {
                yield Event::SessionStarted;
            }

            for _round in 0..MAX_TOOL_ROUNDS {
                if cancel.load(Ordering::SeqCst) {
                    yield Event::TurnInterrupted;
                    return;
                }

                let mut tool_results = Vec::new();

                match next_stream {
                    Ok(mut model_events) => {
                        while let Some(model_event) = model_events.next().await {
                            if cancel.load(Ordering::SeqCst) {
                                yield Event::TurnInterrupted;
                                return;
                            }
                            match model_event {
                                Ok(ModelEvent::AssistantDelta { text }) => {
                                    yield Event::AssistantDelta { text };
                                }
                                Ok(ModelEvent::ThinkingDelta { text }) => {
                                    yield Event::ThinkingDelta { text };
                                }
                                Ok(ModelEvent::Usage(usage)) => {
                                    yield Event::Usage { usage };
                                }
                                Ok(ModelEvent::ToolCall(tool_call)) => {
                                    yield Event::ToolStarted {
                                        call: ToolCall {
                                            id: tool_call.id.clone(),
                                            name: tool_call.name.clone(),
                                            input: tool_call.input.clone(),
                                        },
                                    };

                                    let output = match run_tool_call(&tool_call, &tool_cwd) {
                                        Ok(output) => output,
                                        Err(error) => agent_tools::ToolOutput {
                                            success: Some(false),
                                            exit_code: None,
                                            content: error.to_string(),
                                        },
                                    };

                                    yield Event::ToolFinished {
                                        id: tool_call.id.clone(),
                                        result: ToolResult {
                                            exit_code: output.exit_code,
                                            summary: output.content.clone(),
                                        },
                                    };

                                    tool_results.push(ModelToolResult {
                                        call_id: tool_call.id,
                                        name: tool_call.name,
                                        content: output.content,
                                    });
                                }
                                Err(error) => {
                                    yield Event::TurnFailed {
                                        message: error.message,
                                    };
                                    return;
                                }
                            }
                        }
                    }
                    Err(error) => {
                        yield Event::TurnFailed {
                            message: error.message,
                        };
                        return;
                    }
                }

                if tool_results.is_empty() {
                    yield Event::TurnComplete;
                    return;
                }

                next_stream = model.stream_tool_results(tool_results);
            }

            yield Event::TurnFailed {
                message: "maximum tool rounds exceeded".to_string(),
            };
        }))
    }
}

pub type EventStream = BoxStream<'static, Event>;
pub type ModelEventStream = BoxStream<'static, Result<ModelEvent, ModelError>>;

pub trait ModelClient: Clone + Send + 'static {
    fn stream_turn(&mut self, input: ModelTurnInput) -> Result<ModelEventStream, ModelError>;

    fn stream_tool_results(
        &mut self,
        results: Vec<ModelToolResult>,
    ) -> Result<ModelEventStream, ModelError>;

    /// Returns the full conversation accumulated so far, in chat order. Used to
    /// persist a session transcript. Implementors that do not track a
    /// conversation return an empty transcript.
    fn transcript(&self) -> Vec<TranscriptMessage> {
        Vec::new()
    }

    /// Returns the tool definitions made available to the model for this
    /// session, in the same JSON shape sent to the model provider.
    fn tool_definitions(&self) -> Vec<Value> {
        Vec::new()
    }

    /// Returns the provider model slug used for this session. Implementors that
    /// do not have a provider-backed model return an empty slug.
    fn model_slug(&self) -> String {
        String::new()
    }
}

#[derive(Clone)]
pub struct OpenRouterClient {
    http: reqwest::Client,
    endpoint: String,
    model: String,
    api_key: String,
    messages: Arc<Mutex<Vec<ChatMessage>>>,
}

impl OpenRouterClient {
    pub fn new(model: impl Into<String>, api_key: impl Into<String>) -> Self {
        Self {
            http: reqwest::Client::new(),
            endpoint: "https://openrouter.ai/api/v1/chat/completions".to_string(),
            model: model.into(),
            api_key: api_key.into(),
            messages: Arc::new(Mutex::new(Vec::new())),
        }
    }

    pub fn with_endpoint(
        model: impl Into<String>,
        api_key: impl Into<String>,
        endpoint: impl Into<String>,
    ) -> Self {
        Self {
            http: reqwest::Client::new(),
            endpoint: endpoint.into(),
            model: model.into(),
            api_key: api_key.into(),
            messages: Arc::new(Mutex::new(Vec::new())),
        }
    }
}

impl ModelClient for OpenRouterClient {
    fn stream_turn(&mut self, input: ModelTurnInput) -> Result<ModelEventStream, ModelError> {
        let ModelTurnInput { prompt, cwd } = input;

        {
            let mut messages = self
                .messages
                .lock()
                .map_err(|_| ModelError::new("OpenRouter conversation state is unavailable"))?;
            if messages.is_empty() {
                let startup_context = build_startup_context_prompt(&cwd);
                messages.push(ChatMessage::system(SYSTEM_PROMPT));
                messages.push(ChatMessage::user(startup_context));
            }

            messages.push(ChatMessage::user(prompt));
        }

        self.stream_current_messages()
    }

    fn stream_tool_results(
        &mut self,
        results: Vec<ModelToolResult>,
    ) -> Result<ModelEventStream, ModelError> {
        {
            let mut messages = self
                .messages
                .lock()
                .map_err(|_| ModelError::new("OpenRouter conversation state is unavailable"))?;
            messages.extend(results.into_iter().map(ChatMessage::tool_result));
        }

        self.stream_current_messages()
    }

    fn transcript(&self) -> Vec<TranscriptMessage> {
        self.messages
            .lock()
            .map(|messages| messages.iter().map(ChatMessage::to_transcript).collect())
            .unwrap_or_default()
    }

    fn tool_definitions(&self) -> Vec<Value> {
        available_tool_definitions()
    }

    fn model_slug(&self) -> String {
        self.model.clone()
    }
}

impl OpenRouterClient {
    fn stream_current_messages(&mut self) -> Result<ModelEventStream, ModelError> {
        let http = self.http.clone();
        let endpoint = self.endpoint.clone();
        let model = self.model.clone();
        let api_key = self.api_key.clone();
        let mut messages = self
            .messages
            .lock()
            .map_err(|_| ModelError::new("OpenRouter conversation state is unavailable"))?
            .clone();

        // Prompt caching: Anthropic via OpenRouter uses explicit `cache_control`
        // breakpoints. Pin one on the system prompt (stable for the whole
        // session, and reusable across conversations since tools + system form
        // the canonical prefix) and one on the latest message (advances each
        // turn, so the growing transcript prefix keeps getting cached). That is
        // two of the four allowed breakpoints. Non-Anthropic providers ignore
        // the markers, so this is gated to the Anthropic family.
        if model.starts_with("anthropic/") {
            if let Some(system) = messages.first_mut() {
                system.mark_cache_breakpoint();
            }
            if let Some(latest) = messages.last_mut() {
                latest.mark_cache_breakpoint();
            }
        }

        let state = Arc::clone(&self.messages);

        Ok(Box::pin(async_stream::stream! {
            let request = ChatCompletionRequest {
                model,
                messages,
                stream: true,
                tools: available_tool_definitions(),
                reasoning: ReasoningConfig { enabled: true },
            };

            let response = match http
                .post(endpoint)
                .headers(openrouter_headers(&api_key))
                .json(&request)
                .send()
                .await
            {
                Ok(response) => response,
                Err(error) => {
                    yield Err(ModelError::new(format!("OpenRouter request failed: {error}")));
                    return;
                }
            };

            if !response.status().is_success() {
                let status = response.status();
                let message = match response.text().await {
                    Ok(body) => parse_openrouter_error(&body)
                        .unwrap_or_else(|| body.trim().to_string()),
                    Err(error) => format!("failed to read error body: {error}"),
                };

                yield Err(ModelError::new(format!(
                    "OpenRouter request failed with HTTP {status}: {message}"
                )));
                return;
            }

            let mut bytes = response.bytes_stream();
            let mut buffer = String::new();
            let mut tool_calls = ToolCallAccumulator::default();
            let mut assistant_text = String::new();
            let mut reasoning_content = String::new();
            let mut stream_done = false;

            while let Some(chunk) = bytes.next().await {
                let chunk = match chunk {
                    Ok(chunk) => chunk,
                    Err(error) => {
                        yield Err(ModelError::new(format!("OpenRouter stream failed: {error}")));
                        return;
                    }
                };

                buffer.push_str(&String::from_utf8_lossy(&chunk));

                while let Some(newline) = buffer.find('\n') {
                    let line = buffer[..newline].trim_end_matches('\r').to_string();
                    buffer.drain(..=newline);

                    match parse_sse_line(&line) {
                        SseLine::Ignore => {}
                        SseLine::Done => {
                            stream_done = true;
                            break;
                        }
                        SseLine::Data(data) => match parse_chat_chunk(&data) {
                            Ok(ChatChunkEvent::AssistantDelta(text)) => {
                                assistant_text.push_str(&text);
                                yield Ok(ModelEvent::AssistantDelta { text });
                            }
                            Ok(ChatChunkEvent::ThinkingDelta(text)) => {
                                reasoning_content.push_str(&text);
                                yield Ok(ModelEvent::ThinkingDelta { text });
                            }
                            Ok(ChatChunkEvent::ToolCallDelta(delta)) => {
                                tool_calls.apply(delta);
                            }
                            Ok(ChatChunkEvent::Usage(usage)) => {
                                yield Ok(ModelEvent::Usage(usage));
                            }
                            Ok(ChatChunkEvent::None) => {}
                            Err(error) => {
                                yield Err(error);
                                return;
                            }
                        },
                    }
                }

                if stream_done {
                    break;
                }
            }

            let tool_calls = match tool_calls.finish() {
                Ok(tool_calls) => tool_calls,
                Err(error) => {
                    yield Err(error);
                    return;
                }
            };

            if (!assistant_text.is_empty() || !reasoning_content.is_empty() || !tool_calls.is_empty())
                && let Err(error) =
                    push_assistant_message(&state, assistant_text, reasoning_content, &tool_calls)
            {
                yield Err(error);
                return;
            }

            for tool_call in tool_calls {
                yield Ok(ModelEvent::ToolCall(tool_call));
            }
        }))
    }
}

fn push_assistant_message(
    state: &Arc<Mutex<Vec<ChatMessage>>>,
    content: String,
    reasoning_content: String,
    tool_calls: &[ModelToolCall],
) -> Result<(), ModelError> {
    let mut messages = state
        .lock()
        .map_err(|_| ModelError::new("OpenRouter conversation state is unavailable"))?;
    messages.push(ChatMessage::assistant(
        content,
        reasoning_content,
        tool_calls,
    ));
    Ok(())
}

fn openrouter_headers(api_key: &str) -> HeaderMap {
    let mut headers = HeaderMap::new();
    let authorization = format!("Bearer {api_key}");

    if let Ok(value) = HeaderValue::from_str(&authorization) {
        headers.insert(AUTHORIZATION, value);
    }

    headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    headers
}

enum SseLine {
    Data(String),
    Done,
    Ignore,
}

fn parse_sse_line(line: &str) -> SseLine {
    let line = line.trim();

    if line.is_empty() || line.starts_with(':') {
        return SseLine::Ignore;
    }

    let Some(data) = line.strip_prefix("data:") else {
        return SseLine::Ignore;
    };

    let data = data.trim();

    if data == "[DONE]" {
        SseLine::Done
    } else {
        SseLine::Data(data.to_string())
    }
}

fn parse_chat_chunk(data: &str) -> Result<ChatChunkEvent, ModelError> {
    let chunk = serde_json::from_str::<ChatCompletionChunk>(data)
        .map_err(|error| ModelError::new(format!("failed to parse OpenRouter stream: {error}")))?;

    if let Some(error) = chunk.error {
        return Err(ModelError::new(format!(
            "OpenRouter stream error: {}",
            error.message
        )));
    }

    // The final chunk carries usage accounting and typically has no choices.
    if let Some(usage) = chunk.usage {
        return Ok(ChatChunkEvent::Usage(usage.to_token_usage()));
    }

    for choice in chunk.choices {
        if let Some(reasoning) = choice.delta.reasoning.or(choice.delta.reasoning_content)
            && !reasoning.is_empty()
        {
            return Ok(ChatChunkEvent::ThinkingDelta(reasoning));
        }

        if let Some(content) = choice.delta.content
            && !content.is_empty()
        {
            return Ok(ChatChunkEvent::AssistantDelta(content));
        }

        if let Some(tool_calls) = choice.delta.tool_calls
            && let Some(tool_call) = tool_calls.into_iter().next()
        {
            return Ok(ChatChunkEvent::ToolCallDelta(tool_call));
        }
    }

    Ok(ChatChunkEvent::None)
}

fn parse_openrouter_error(body: &str) -> Option<String> {
    serde_json::from_str::<OpenRouterErrorResponse>(body)
        .ok()
        .map(|response| response.error.message)
}

#[derive(Debug, Serialize)]
struct ChatCompletionRequest {
    model: String,
    messages: Vec<ChatMessage>,
    stream: bool,
    tools: Vec<Value>,
    reasoning: ReasoningConfig,
}

/// Requests model reasoning/thinking tokens. Models that do not support
/// reasoning ignore this field.
#[derive(Debug, Clone, Copy, Serialize)]
struct ReasoningConfig {
    enabled: bool,
}

/// Marks a content block as a prompt-cache breakpoint. Anthropic caches the
/// entire prefix up to and including a marked block; OpenRouter forwards this
/// using Anthropic's explicit-caching syntax. Providers without explicit
/// caching ignore the field.
#[derive(Debug, Clone, Copy, Serialize)]
struct CacheControl {
    #[serde(rename = "type")]
    kind: &'static str,
}

const EPHEMERAL_CACHE: CacheControl = CacheControl { kind: "ephemeral" };

/// A single text content block, optionally tagged as a cache breakpoint.
#[derive(Debug, Clone, Serialize)]
struct TextPart {
    #[serde(rename = "type")]
    kind: &'static str,
    text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    cache_control: Option<CacheControl>,
}

impl TextPart {
    fn new(text: String) -> Self {
        Self {
            kind: "text",
            text,
            cache_control: None,
        }
    }
}

/// Message content sent to OpenRouter. Plain strings serialize as a bare JSON
/// string; once a cache breakpoint is attached the content is promoted to the
/// content-parts array form that Anthropic requires for `cache_control`.
#[derive(Debug, Clone, Serialize)]
#[serde(untagged)]
enum MessageContent {
    Text(String),
    Parts(Vec<TextPart>),
}

impl MessageContent {
    fn text(value: impl Into<String>) -> Self {
        Self::Text(value.into())
    }

    fn as_text(&self) -> String {
        match self {
            Self::Text(text) => text.clone(),
            Self::Parts(parts) => parts.iter().map(|part| part.text.as_str()).collect(),
        }
    }

    /// Attaches a cache breakpoint to the final block, promoting a plain string
    /// to the parts form if needed.
    fn mark_cache_breakpoint(&mut self) {
        let mut parts = match std::mem::replace(self, Self::Parts(Vec::new())) {
            Self::Text(text) => vec![TextPart::new(text)],
            Self::Parts(parts) => parts,
        };
        if let Some(last) = parts.last_mut() {
            last.cache_control = Some(EPHEMERAL_CACHE);
        }
        *self = Self::Parts(parts);
    }
}

#[derive(Debug, Clone, Serialize)]
struct ChatMessage {
    role: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    content: Option<MessageContent>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reasoning_content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_calls: Option<Vec<OpenRouterToolCall>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_call_id: Option<String>,
}

impl ChatMessage {
    fn system(content: impl Into<String>) -> Self {
        Self {
            role: "system".to_string(),
            content: Some(MessageContent::text(content)),
            reasoning_content: None,
            tool_calls: None,
            tool_call_id: None,
        }
    }

    fn user(content: String) -> Self {
        Self {
            role: "user".to_string(),
            content: Some(MessageContent::text(content)),
            reasoning_content: None,
            tool_calls: None,
            tool_call_id: None,
        }
    }

    fn assistant(content: String, reasoning_content: String, tool_calls: &[ModelToolCall]) -> Self {
        Self {
            role: "assistant".to_string(),
            content: (!content.is_empty()).then(|| MessageContent::text(content)),
            reasoning_content: (!reasoning_content.is_empty()).then_some(reasoning_content),
            tool_calls: (!tool_calls.is_empty())
                .then(|| tool_calls.iter().map(OpenRouterToolCall::from).collect()),
            tool_call_id: None,
        }
    }

    fn tool_result(result: ModelToolResult) -> Self {
        Self {
            role: "tool".to_string(),
            content: Some(MessageContent::text(result.content)),
            reasoning_content: None,
            tool_calls: None,
            tool_call_id: Some(result.call_id),
        }
    }

    /// Tags this message's final content block as a prompt-cache breakpoint.
    /// No-op for messages without content (e.g. an assistant tool-call turn).
    fn mark_cache_breakpoint(&mut self) {
        if let Some(content) = self.content.as_mut() {
            content.mark_cache_breakpoint();
        }
    }

    fn to_transcript(&self) -> TranscriptMessage {
        let content = self.content.as_ref().map(MessageContent::as_text);
        match self.role.as_str() {
            "system" => TranscriptMessage::System {
                content: content.unwrap_or_default(),
            },
            "user" => TranscriptMessage::User {
                content: content.unwrap_or_default(),
            },
            "tool" => TranscriptMessage::Tool {
                tool_call_id: self.tool_call_id.clone().unwrap_or_default(),
                content: content.unwrap_or_default(),
            },
            // Default to assistant for "assistant" and any unexpected role.
            _ => TranscriptMessage::Assistant {
                reasoning_content: self.reasoning_content.clone(),
                content,
                tool_calls: self
                    .tool_calls
                    .as_deref()
                    .unwrap_or_default()
                    .iter()
                    .map(OpenRouterToolCall::to_tool_call)
                    .collect(),
            },
        }
    }
}

fn build_startup_context_prompt(cwd: &Path) -> String {
    let mut entries = match fs::read_dir(cwd) {
        Ok(entries) => entries
            .filter_map(Result::ok)
            .map(|entry| {
                let suffix = match entry.file_type() {
                    Ok(file_type) if file_type.is_dir() => "/",
                    Ok(file_type) if file_type.is_symlink() => "@",
                    Ok(file_type) if file_type.is_file() => "",
                    _ => "?",
                };

                format!("{}{}", entry.file_name().to_string_lossy(), suffix)
            })
            .collect::<Vec<_>>(),
        Err(error) => {
            return format!(
                "You are currently in {}.\nDepth-1 file tree:\n<unable to read directory: {error}>",
                cwd.display()
            );
        }
    };

    entries.sort();

    let mut lines = vec![
        format!("You are currently in {}.", cwd.display()),
        "Depth-1 file tree:".to_string(),
    ];

    if entries.is_empty() {
        lines.push("<empty>".to_string());
    } else {
        lines.extend(entries);
    }

    lines.join("\n")
}

#[derive(Debug, Deserialize)]
struct ChatCompletionChunk {
    #[serde(default)]
    choices: Vec<ChatChoice>,
    error: Option<OpenRouterError>,
    /// Token/cost accounting, present only on the final streamed chunk.
    usage: Option<Usage>,
}

/// Usage accounting block from OpenRouter's final stream chunk. OpenRouter
/// always includes this now; fields default so partial blocks are tolerated.
#[derive(Debug, Deserialize)]
struct Usage {
    #[serde(default)]
    prompt_tokens: u64,
    #[serde(default)]
    completion_tokens: u64,
    #[serde(default)]
    prompt_tokens_details: PromptTokensDetails,
    #[serde(default)]
    cost: f64,
}

#[derive(Debug, Default, Deserialize)]
struct PromptTokensDetails {
    #[serde(default)]
    cached_tokens: u64,
    #[serde(default)]
    cache_write_tokens: u64,
}

impl Usage {
    fn to_token_usage(&self) -> TokenUsage {
        TokenUsage {
            input_tokens: self.prompt_tokens,
            cached_tokens: self.prompt_tokens_details.cached_tokens,
            cache_write_tokens: self.prompt_tokens_details.cache_write_tokens,
            output_tokens: self.completion_tokens,
            cost_usd: self.cost,
        }
    }
}

#[derive(Debug, Deserialize)]
struct ChatChoice {
    delta: ChatDelta,
}

#[derive(Debug, Deserialize)]
struct ChatDelta {
    content: Option<String>,
    reasoning: Option<String>,
    reasoning_content: Option<String>,
    tool_calls: Option<Vec<ToolCallDelta>>,
}

#[derive(Debug)]
enum ChatChunkEvent {
    AssistantDelta(String),
    ThinkingDelta(String),
    ToolCallDelta(ToolCallDelta),
    Usage(TokenUsage),
    None,
}

#[derive(Debug, Clone, Serialize)]
struct OpenRouterToolCall {
    id: String,
    #[serde(rename = "type")]
    kind: String,
    function: OpenRouterToolFunction,
}

impl From<&ModelToolCall> for OpenRouterToolCall {
    fn from(tool_call: &ModelToolCall) -> Self {
        Self {
            id: tool_call.id.clone(),
            kind: "function".to_string(),
            function: OpenRouterToolFunction {
                name: tool_call.name.clone(),
                arguments: tool_call.input.to_string(),
            },
        }
    }
}

impl OpenRouterToolCall {
    fn to_tool_call(&self) -> ToolCall {
        // Arguments are stored as the JSON string sent on the wire; parse them
        // back to structured input, falling back to the raw string if needed.
        let input = serde_json::from_str(&self.function.arguments)
            .unwrap_or_else(|_| Value::String(self.function.arguments.clone()));

        ToolCall {
            id: self.id.clone(),
            name: self.function.name.clone(),
            input,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
struct OpenRouterToolFunction {
    name: String,
    arguments: String,
}

#[derive(Debug, Deserialize)]
struct ToolCallDelta {
    index: usize,
    id: Option<String>,
    function: Option<ToolFunctionDelta>,
}

#[derive(Debug, Deserialize)]
struct ToolFunctionDelta {
    name: Option<String>,
    arguments: Option<String>,
}

#[derive(Default)]
struct ToolCallAccumulator {
    calls: Vec<PartialToolCall>,
}

impl ToolCallAccumulator {
    fn apply(&mut self, delta: ToolCallDelta) {
        while self.calls.len() <= delta.index {
            self.calls.push(PartialToolCall::default());
        }

        let call = &mut self.calls[delta.index];

        if let Some(id) = delta.id {
            call.id = Some(id);
        }

        if let Some(function) = delta.function {
            if let Some(name) = function.name {
                call.name = Some(name);
            }

            if let Some(arguments) = function.arguments {
                call.arguments.push_str(&arguments);
            }
        }
    }

    fn finish(self) -> Result<Vec<ModelToolCall>, ModelError> {
        self.calls
            .into_iter()
            .filter(|call| call.id.is_some() || call.name.is_some() || !call.arguments.is_empty())
            .map(PartialToolCall::finish)
            .collect()
    }
}

#[derive(Default)]
struct PartialToolCall {
    id: Option<String>,
    name: Option<String>,
    arguments: String,
}

impl PartialToolCall {
    fn finish(self) -> Result<ModelToolCall, ModelError> {
        let id = self
            .id
            .ok_or_else(|| ModelError::new("OpenRouter tool call missing id"))?;
        let name = self
            .name
            .ok_or_else(|| ModelError::new("OpenRouter tool call missing name"))?;
        let input = serde_json::from_str(&self.arguments).map_err(|error| {
            ModelError::new(format!(
                "failed to parse OpenRouter tool arguments: {error}"
            ))
        })?;

        Ok(ModelToolCall { id, name, input })
    }
}

#[derive(Debug, Deserialize)]
struct OpenRouterErrorResponse {
    error: OpenRouterError,
}

#[derive(Debug, Deserialize)]
struct OpenRouterError {
    message: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelTurnInput {
    pub prompt: String,
    pub cwd: PathBuf,
}

// `Usage` carries an `f64` cost, so this enum is `PartialEq` but not `Eq`.
#[derive(Debug, Clone, PartialEq)]
pub enum ModelEvent {
    AssistantDelta { text: String },
    ThinkingDelta { text: String },
    ToolCall(ModelToolCall),
    Usage(TokenUsage),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelToolCall {
    pub id: String,
    pub name: String,
    pub input: Value,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelToolResult {
    pub call_id: String,
    pub name: String,
    pub content: String,
}

fn run_tool_call(
    tool_call: &ModelToolCall,
    default_cwd: &Path,
) -> Result<agent_tools::ToolOutput, ModelError> {
    let result = match tool_call.name.as_str() {
        "read_file" => read_file(tool_call.input.clone()),
        "list_dir" => list_dir(tool_call.input.clone()),
        "shell" => shell(tool_call.input.clone(), default_cwd),
        "edit_file" => edit_file(tool_call.input.clone(), default_cwd),
        "write_file" => write_file(tool_call.input.clone(), default_cwd),
        other => {
            return Err(ModelError::new(format!("unknown tool: {other}")));
        }
    };

    result.map_err(|error| ModelError::new(error.to_string()))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelError {
    message: String,
}

impl ModelError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }

    pub fn message(&self) -> &str {
        &self.message
    }
}

impl fmt::Display for ModelError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl Error for ModelError {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentError {
    UnsupportedOp { op: &'static str },
}

impl AgentError {
    fn unsupported_op(op: &'static str) -> Self {
        Self::UnsupportedOp { op }
    }
}

impl fmt::Display for AgentError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnsupportedOp { op } => write!(formatter, "unsupported op: {op}"),
        }
    }
}

impl Error for AgentError {}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::StreamExt;
    use futures_util::stream;
    use serde_json::json;
    use std::{
        env, fs, process,
        sync::{
            Arc, Mutex,
            atomic::{AtomicU64, Ordering},
        },
        time::{SystemTime, UNIX_EPOCH},
    };

    #[test]
    fn plain_message_content_serializes_as_bare_string() {
        let user = ChatMessage::user("hello".to_string());
        let value = serde_json::to_value(&user).expect("serialize message");
        assert_eq!(value["content"], json!("hello"));
    }

    #[test]
    fn cache_breakpoint_promotes_content_to_parts_with_ephemeral_marker() {
        let mut system = ChatMessage::system(SYSTEM_PROMPT);
        system.mark_cache_breakpoint();
        let value = serde_json::to_value(&system).expect("serialize message");
        assert_eq!(
            value["content"],
            json!([
                {
                    "type": "text",
                    "text": SYSTEM_PROMPT,
                    "cache_control": { "type": "ephemeral" }
                }
            ])
        );
    }

    #[test]
    fn cache_breakpoint_on_empty_content_is_a_no_op() {
        // An assistant tool-call turn carries no text content.
        let mut assistant = ChatMessage::assistant(
            String::new(),
            String::new(),
            &[ModelToolCall {
                id: "call-1".to_string(),
                name: "read_file".to_string(),
                input: json!({}),
            }],
        );
        assistant.mark_cache_breakpoint();
        let value = serde_json::to_value(&assistant).expect("serialize message");
        assert!(value.get("content").is_none());
    }

    #[test]
    fn transcript_flattens_cached_parts_back_to_text() {
        let mut user = ChatMessage::user("hello".to_string());
        user.mark_cache_breakpoint();
        match user.to_transcript() {
            TranscriptMessage::User { content } => assert_eq!(content, "hello"),
            other => panic!("unexpected transcript message: {other:?}"),
        }
    }

    #[test]
    fn transcript_preserves_assistant_reasoning_content() {
        let assistant =
            ChatMessage::assistant("answer".to_string(), "worked through it".to_string(), &[]);

        match assistant.to_transcript() {
            TranscriptMessage::Assistant {
                reasoning_content,
                content,
                tool_calls,
            } => {
                assert_eq!(reasoning_content, Some("worked through it".to_string()));
                assert_eq!(content, Some("answer".to_string()));
                assert!(tool_calls.is_empty());
            }
            other => panic!("unexpected transcript message: {other:?}"),
        }
    }

    #[test]
    fn user_turn_emits_session_start_model_deltas_and_turn_complete() {
        let mut session = AgentSession::new(ScriptedModel::success(vec![
            ModelEvent::AssistantDelta {
                text: "hello".to_string(),
            },
            ModelEvent::AssistantDelta {
                text: " world".to_string(),
            },
        ]));

        let events = submit_events(
            &mut session,
            Op::UserTurn {
                prompt: "say hello".to_string(),
                cwd: PathBuf::from("/tmp/project"),
            },
            "submit user turn",
        );

        assert_eq!(
            events,
            vec![
                Event::SessionStarted,
                Event::AssistantDelta {
                    text: "hello".to_string()
                },
                Event::AssistantDelta {
                    text: " world".to_string()
                },
                Event::TurnComplete
            ]
        );

        let model = session.into_model();
        assert_eq!(
            model.inputs(),
            vec![ModelTurnInput {
                prompt: "say hello".to_string(),
                cwd: PathBuf::from("/tmp/project"),
            }]
        );
    }

    #[test]
    fn model_stream_errors_are_reported_as_turn_failed_events() {
        let mut session = AgentSession::new(ScriptedModel::stream(vec![
            Ok(ModelEvent::AssistantDelta {
                text: "partial".to_string(),
            }),
            Err(ModelError::new("stream interrupted")),
        ]));

        let events = submit_events(
            &mut session,
            Op::UserTurn {
                prompt: "say hello".to_string(),
                cwd: PathBuf::from("/tmp/project"),
            },
            "submit user turn",
        );

        assert_eq!(
            events,
            vec![
                Event::SessionStarted,
                Event::AssistantDelta {
                    text: "partial".to_string()
                },
                Event::TurnFailed {
                    message: "stream interrupted".to_string()
                }
            ]
        );
    }

    #[test]
    fn session_started_is_emitted_only_once() {
        let mut session =
            AgentSession::new(ScriptedModel::success(vec![ModelEvent::AssistantDelta {
                text: "ok".to_string(),
            }]));

        let first_events = submit_events(
            &mut session,
            Op::UserTurn {
                prompt: "first".to_string(),
                cwd: PathBuf::from("/tmp/project"),
            },
            "submit first turn",
        );
        let second_events = submit_events(
            &mut session,
            Op::UserTurn {
                prompt: "second".to_string(),
                cwd: PathBuf::from("/tmp/project"),
            },
            "submit second turn",
        );

        assert_eq!(first_events.first(), Some(&Event::SessionStarted));
        assert!(!second_events.contains(&Event::SessionStarted));
        assert_eq!(
            second_events,
            vec![
                Event::AssistantDelta {
                    text: "ok".to_string()
                },
                Event::TurnComplete
            ]
        );
    }

    #[test]
    fn model_errors_are_reported_as_turn_failed_events() {
        let mut session = AgentSession::new(ScriptedModel::failure("model unavailable"));

        let events = submit_events(
            &mut session,
            Op::UserTurn {
                prompt: "say hello".to_string(),
                cwd: PathBuf::from("/tmp/project"),
            },
            "submit user turn",
        );

        assert_eq!(
            events,
            vec![
                Event::SessionStarted,
                Event::TurnFailed {
                    message: "model unavailable".to_string()
                }
            ]
        );
    }

    #[test]
    fn unsupported_ops_return_clear_errors() {
        let mut session = AgentSession::new(ScriptedModel::success(Vec::new()));

        assert_unsupported(&mut session, Op::Compact, "compact");
        assert_unsupported(&mut session, Op::Shutdown, "shutdown");
    }

    #[test]
    fn interrupt_ends_running_turn_with_interrupted_event() {
        let mut session =
            AgentSession::new(ScriptedModel::success(vec![ModelEvent::AssistantDelta {
                text: "partial".to_string(),
            }]));

        // Build the turn stream, request interrupt before it is polled, then
        // drive it: the turn observes the flag and ends without forwarding the
        // pending assistant delta.
        let turn = futures_executor::block_on(session.submit(Op::UserTurn {
            prompt: "go".to_string(),
            cwd: PathBuf::from("."),
        }))
        .expect("submit user turn");

        let interrupt = futures_executor::block_on(session.submit(Op::Interrupt))
            .expect("interrupt is supported");
        assert!(collect_events(interrupt).is_empty());

        assert_eq!(
            collect_events(turn),
            vec![Event::SessionStarted, Event::TurnInterrupted]
        );
    }

    #[test]
    fn session_runs_normally_after_interrupt() {
        let mut session =
            AgentSession::new(ScriptedModel::success(vec![ModelEvent::AssistantDelta {
                text: "done".to_string(),
            }]));

        let turn = futures_executor::block_on(session.submit(Op::UserTurn {
            prompt: "go".to_string(),
            cwd: PathBuf::from("."),
        }))
        .expect("submit user turn");
        let interrupt =
            futures_executor::block_on(session.submit(Op::Interrupt)).expect("interrupt");
        let _ = collect_events(interrupt);
        let _ = collect_events(turn);

        // A fresh turn clears the stale interrupt flag and completes normally.
        let events = submit_events(
            &mut session,
            Op::UserTurn {
                prompt: "again".to_string(),
                cwd: PathBuf::from("."),
            },
            "second turn",
        );

        assert_eq!(
            events,
            vec![
                Event::AssistantDelta {
                    text: "done".to_string()
                },
                Event::TurnComplete
            ]
        );
    }

    #[test]
    fn tool_calls_execute_and_continue_model_turn() {
        let temp = TempDir::new();
        let file_path = temp.path().join("sample.txt");
        fs::write(&file_path, "alpha\nbeta\n").expect("write file");
        let mut session = AgentSession::new(ScriptedModel::with_tool_response(
            vec![ModelEvent::ToolCall(ModelToolCall {
                id: "call-1".to_string(),
                name: "read_file".to_string(),
                input: json!({ "file_path": file_path }),
            })],
            vec![ModelEvent::AssistantDelta {
                text: "read complete".to_string(),
            }],
        ));

        let events = submit_events(
            &mut session,
            Op::UserTurn {
                prompt: "read it".to_string(),
                cwd: temp.path().to_path_buf(),
            },
            "submit user turn",
        );

        assert_eq!(
            events,
            vec![
                Event::SessionStarted,
                Event::ToolStarted {
                    call: ToolCall {
                        id: "call-1".to_string(),
                        name: "read_file".to_string(),
                        input: json!({ "file_path": temp.path().join("sample.txt") }),
                    }
                },
                Event::ToolFinished {
                    id: "call-1".to_string(),
                    result: ToolResult {
                        exit_code: None,
                        summary: "L1: alpha\nL2: beta".to_string(),
                    }
                },
                Event::AssistantDelta {
                    text: "read complete".to_string()
                },
                Event::TurnComplete
            ]
        );

        let model = session.into_model();
        assert_eq!(
            model.tool_results(),
            vec![vec![ModelToolResult {
                call_id: "call-1".to_string(),
                name: "read_file".to_string(),
                content: "L1: alpha\nL2: beta".to_string(),
            }]]
        );
    }

    #[test]
    fn shell_tool_call_uses_turn_cwd_and_reports_exit_code() {
        let temp = TempDir::new();
        let mut session = AgentSession::new(ScriptedModel::with_tool_response(
            vec![ModelEvent::ToolCall(ModelToolCall {
                id: "call-1".to_string(),
                name: "shell".to_string(),
                input: json!({ "command": "printf shell-ok" }),
            })],
            vec![ModelEvent::AssistantDelta {
                text: "command complete".to_string(),
            }],
        ));

        let events = submit_events(
            &mut session,
            Op::UserTurn {
                prompt: "run it".to_string(),
                cwd: temp.path().to_path_buf(),
            },
            "submit user turn",
        );

        assert_eq!(
            events,
            vec![
                Event::SessionStarted,
                Event::ToolStarted {
                    call: ToolCall {
                        id: "call-1".to_string(),
                        name: "shell".to_string(),
                        input: json!({ "command": "printf shell-ok" }),
                    }
                },
                Event::ToolFinished {
                    id: "call-1".to_string(),
                    result: ToolResult {
                        exit_code: Some(0),
                        summary: "Exit code: 0\nstdout:\nshell-ok".to_string(),
                    }
                },
                Event::AssistantDelta {
                    text: "command complete".to_string()
                },
                Event::TurnComplete
            ]
        );

        let model = session.into_model();
        assert_eq!(
            model.tool_results(),
            vec![vec![ModelToolResult {
                call_id: "call-1".to_string(),
                name: "shell".to_string(),
                content: "Exit code: 0\nstdout:\nshell-ok".to_string(),
            }]]
        );
    }

    #[test]
    fn edit_file_tool_call_uses_turn_cwd() {
        let temp = TempDir::new();
        let file_path = temp.path().join("sample.txt");
        fs::write(&file_path, "before\n").expect("write file");
        let mut session = AgentSession::new(ScriptedModel::with_tool_response(
            vec![ModelEvent::ToolCall(ModelToolCall {
                id: "call-1".to_string(),
                name: "edit_file".to_string(),
                input: json!({
                    "file_path": "sample.txt",
                    "search": "before",
                    "replace": "after"
                }),
            })],
            vec![ModelEvent::AssistantDelta {
                text: "edit complete".to_string(),
            }],
        ));

        let events = submit_events(
            &mut session,
            Op::UserTurn {
                prompt: "edit it".to_string(),
                cwd: temp.path().to_path_buf(),
            },
            "submit user turn",
        );

        assert_eq!(
            events,
            vec![
                Event::SessionStarted,
                Event::ToolStarted {
                    call: ToolCall {
                        id: "call-1".to_string(),
                        name: "edit_file".to_string(),
                        input: json!({
                            "file_path": "sample.txt",
                            "search": "before",
                            "replace": "after"
                        }),
                    }
                },
                Event::ToolFinished {
                    id: "call-1".to_string(),
                    result: ToolResult {
                        exit_code: None,
                        summary: format!(
                            "Edited {}: replaced 1 occurrence(s).",
                            file_path.display()
                        ),
                    }
                },
                Event::AssistantDelta {
                    text: "edit complete".to_string()
                },
                Event::TurnComplete
            ]
        );
        assert_eq!(
            fs::read_to_string(&file_path).expect("read edited file"),
            "after\n"
        );
    }

    #[test]
    fn write_file_tool_call_uses_turn_cwd() {
        let temp = TempDir::new();
        let file_path = temp.path().join("nested").join("sample.txt");
        let mut session = AgentSession::new(ScriptedModel::with_tool_response(
            vec![ModelEvent::ToolCall(ModelToolCall {
                id: "call-1".to_string(),
                name: "write_file".to_string(),
                input: json!({
                    "file_path": "nested/sample.txt",
                    "content": "created\n"
                }),
            })],
            vec![ModelEvent::AssistantDelta {
                text: "write complete".to_string(),
            }],
        ));

        let events = submit_events(
            &mut session,
            Op::UserTurn {
                prompt: "write it".to_string(),
                cwd: temp.path().to_path_buf(),
            },
            "submit user turn",
        );

        assert_eq!(
            events,
            vec![
                Event::SessionStarted,
                Event::ToolStarted {
                    call: ToolCall {
                        id: "call-1".to_string(),
                        name: "write_file".to_string(),
                        input: json!({
                            "file_path": "nested/sample.txt",
                            "content": "created\n"
                        }),
                    }
                },
                Event::ToolFinished {
                    id: "call-1".to_string(),
                    result: ToolResult {
                        exit_code: None,
                        summary: format!("Wrote {}: 8 byte(s).", file_path.display()),
                    }
                },
                Event::AssistantDelta {
                    text: "write complete".to_string()
                },
                Event::TurnComplete
            ]
        );
        assert_eq!(
            fs::read_to_string(&file_path).expect("read written file"),
            "created\n"
        );
    }

    #[test]
    fn available_tool_definitions_include_tools() {
        let tool_names = available_tool_definitions()
            .into_iter()
            .map(|tool| {
                tool["function"]["name"]
                    .as_str()
                    .expect("tool name")
                    .to_string()
            })
            .collect::<Vec<_>>();

        assert_eq!(
            tool_names,
            vec!["read_file", "list_dir", "shell", "edit_file", "write_file"]
        );
    }

    #[test]
    fn startup_context_prompt_lists_depth_one_entries() {
        let temp = TempDir::new();
        fs::write(temp.path().join("alpha.txt"), "alpha").expect("write alpha");
        fs::create_dir(temp.path().join("nested")).expect("create nested");
        fs::write(temp.path().join("nested").join("child.txt"), "child").expect("write child");

        let prompt = build_startup_context_prompt(temp.path());

        assert_eq!(
            prompt,
            format!(
                "You are currently in {}.\nDepth-1 file tree:\nalpha.txt\nnested/",
                temp.path().display()
            )
        );
    }

    #[test]
    fn startup_context_prompt_handles_empty_directories() {
        let temp = TempDir::new();

        let prompt = build_startup_context_prompt(temp.path());

        assert_eq!(
            prompt,
            format!(
                "You are currently in {}.\nDepth-1 file tree:\n<empty>",
                temp.path().display()
            )
        );
    }

    #[test]
    fn parses_openrouter_stream_content_chunks() {
        let event = parse_chat_chunk(r#"{"choices":[{"delta":{"content":"hello"}}]}"#)
            .expect("parse chunk");

        assert!(matches!(event, ChatChunkEvent::AssistantDelta(text) if text == "hello"));
    }

    #[test]
    fn parses_openrouter_reasoning_content_alias_chunks() {
        let event = parse_chat_chunk(r#"{"choices":[{"delta":{"reasoning_content":"think"}}]}"#)
            .expect("parse chunk");

        assert!(matches!(event, ChatChunkEvent::ThinkingDelta(text) if text == "think"));
    }

    #[test]
    fn ignores_openrouter_stream_chunks_without_content() {
        let event =
            parse_chat_chunk(r#"{"choices":[{"delta":{"content":""}}]}"#).expect("parse chunk");

        assert!(matches!(event, ChatChunkEvent::None));
    }

    #[test]
    fn parses_openrouter_usage_chunk() {
        let event = parse_chat_chunk(
            r#"{"choices":[],"usage":{"prompt_tokens":194,"completion_tokens":2,"total_tokens":196,"cost":0.95,"prompt_tokens_details":{"cached_tokens":100,"cache_write_tokens":50}}}"#,
        )
        .expect("parse chunk");

        let ChatChunkEvent::Usage(usage) = event else {
            panic!("expected usage event, got {event:?}");
        };

        assert_eq!(usage.input_tokens, 194);
        assert_eq!(usage.cached_tokens, 100);
        assert_eq!(usage.cache_write_tokens, 50);
        assert_eq!(usage.output_tokens, 2);
        assert!((usage.cost_usd - 0.95).abs() < 1e-9);
    }

    #[test]
    fn parses_openrouter_tool_call_deltas() {
        let event = parse_chat_chunk(
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call-1","type":"function","function":{"name":"read_file","arguments":"{\"file_path\":\"/tmp/a\"}"}}]}}]}"#,
        )
        .expect("parse chunk");

        let ChatChunkEvent::ToolCallDelta(delta) = event else {
            panic!("expected tool call delta");
        };

        let mut accumulator = ToolCallAccumulator::default();
        accumulator.apply(delta);
        assert_eq!(
            accumulator.finish().expect("finish tool call"),
            vec![ModelToolCall {
                id: "call-1".to_string(),
                name: "read_file".to_string(),
                input: json!({ "file_path": "/tmp/a" }),
            }]
        );
    }

    #[test]
    fn maps_openrouter_stream_error_chunks_to_model_errors() {
        let error = parse_chat_chunk(
            r#"{"error":{"code":"server_error","message":"Provider disconnected unexpectedly"},"choices":[{"delta":{"content":""},"finish_reason":"error"}]}"#,
        )
        .expect_err("stream error");

        assert_eq!(
            error.message(),
            "OpenRouter stream error: Provider disconnected unexpectedly"
        );
    }

    #[test]
    fn parses_openrouter_error_response_message() {
        assert_eq!(
            parse_openrouter_error(r#"{"error":{"code":401,"message":"Invalid API key"}}"#),
            Some("Invalid API key".to_string())
        );
    }

    #[test]
    fn parses_sse_data_done_and_comments() {
        assert!(matches!(
            parse_sse_line(": OPENROUTER PROCESSING"),
            SseLine::Ignore
        ));
        assert!(matches!(parse_sse_line("data: [DONE]"), SseLine::Done));
        assert!(matches!(
            parse_sse_line(r#"data: {"choices":[]}"#),
            SseLine::Data(data) if data == r#"{"choices":[]}"#
        ));
    }

    #[derive(Clone)]
    struct ScriptedModel {
        response: Result<Vec<Result<ModelEvent, ModelError>>, ModelError>,
        tool_response: Result<Vec<Result<ModelEvent, ModelError>>, ModelError>,
        inputs: Arc<Mutex<Vec<ModelTurnInput>>>,
        tool_results: Arc<Mutex<Vec<Vec<ModelToolResult>>>>,
    }

    impl ScriptedModel {
        fn success(events: Vec<ModelEvent>) -> Self {
            Self {
                response: Ok(events.into_iter().map(Ok).collect()),
                tool_response: Ok(Vec::new()),
                inputs: Arc::new(Mutex::new(Vec::new())),
                tool_results: Arc::new(Mutex::new(Vec::new())),
            }
        }

        fn stream(events: Vec<Result<ModelEvent, ModelError>>) -> Self {
            Self {
                response: Ok(events),
                tool_response: Ok(Vec::new()),
                inputs: Arc::new(Mutex::new(Vec::new())),
                tool_results: Arc::new(Mutex::new(Vec::new())),
            }
        }

        fn with_tool_response(events: Vec<ModelEvent>, tool_response: Vec<ModelEvent>) -> Self {
            Self {
                response: Ok(events.into_iter().map(Ok).collect()),
                tool_response: Ok(tool_response.into_iter().map(Ok).collect()),
                inputs: Arc::new(Mutex::new(Vec::new())),
                tool_results: Arc::new(Mutex::new(Vec::new())),
            }
        }

        fn failure(message: &str) -> Self {
            Self {
                response: Err(ModelError::new(message)),
                tool_response: Ok(Vec::new()),
                inputs: Arc::new(Mutex::new(Vec::new())),
                tool_results: Arc::new(Mutex::new(Vec::new())),
            }
        }

        fn inputs(&self) -> Vec<ModelTurnInput> {
            self.inputs.lock().expect("inputs lock").clone()
        }

        fn tool_results(&self) -> Vec<Vec<ModelToolResult>> {
            self.tool_results.lock().expect("tool results lock").clone()
        }
    }

    impl ModelClient for ScriptedModel {
        fn stream_turn(&mut self, input: ModelTurnInput) -> Result<ModelEventStream, ModelError> {
            self.inputs.lock().expect("inputs lock").push(input);
            self.response
                .clone()
                .map(|events| stream::iter(events).boxed())
        }

        fn stream_tool_results(
            &mut self,
            results: Vec<ModelToolResult>,
        ) -> Result<ModelEventStream, ModelError> {
            self.tool_results
                .lock()
                .expect("tool results lock")
                .push(results);
            self.tool_response
                .clone()
                .map(|events| stream::iter(events).boxed())
        }
    }

    fn collect_events(stream: EventStream) -> Vec<Event> {
        futures_executor::block_on(stream.collect())
    }

    fn submit_events(
        session: &mut AgentSession<ScriptedModel>,
        op: Op,
        context: &str,
    ) -> Vec<Event> {
        let stream = futures_executor::block_on(session.submit(op)).unwrap_or_else(|error| {
            panic!("{context}: {error}");
        });

        collect_events(stream)
    }

    fn assert_unsupported(
        session: &mut AgentSession<ScriptedModel>,
        op: Op,
        expected: &'static str,
    ) {
        match futures_executor::block_on(session.submit(op)) {
            Err(error) => assert_eq!(error, AgentError::UnsupportedOp { op: expected }),
            Ok(_) => panic!("expected unsupported op error"),
        }
    }

    struct TempDir {
        path: PathBuf,
    }

    impl TempDir {
        fn new() -> Self {
            static COUNTER: AtomicU64 = AtomicU64::new(0);

            let unique = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("system time")
                .as_nanos();
            let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
            let path = env::temp_dir().join(format!(
                "poe-agent-core-test-{}-{unique}-{seq}",
                process::id()
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
}
