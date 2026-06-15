//! Tool implementations shared by poe core.

use std::{
    collections::VecDeque,
    error::Error,
    ffi::OsStr,
    fmt, fs,
    io::{self, Read},
    path::{Component, Path, PathBuf},
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};

use serde::Deserialize;
use serde_json::{Value, json};

#[cfg(unix)]
use std::os::unix::process::CommandExt;

const READ_FILE_DEFAULT_OFFSET: usize = 1;
const READ_FILE_DEFAULT_LIMIT: usize = 2000;
const LIST_DIR_DEFAULT_OFFSET: usize = 1;
const LIST_DIR_DEFAULT_LIMIT: usize = 25;
const LIST_DIR_DEFAULT_DEPTH: usize = 2;
const DISPLAY_BYTE_LIMIT: usize = 500;
const SHELL_DEFAULT_TIMEOUT_MS: u64 = 5_000;
const SHELL_MIN_TIMEOUT_MS: u64 = 1;
const SHELL_MAX_TIMEOUT_MS: u64 = 300_000;
const SHELL_OUTPUT_BYTE_LIMIT: usize = 20_000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolOutput {
    pub success: Option<bool>,
    pub exit_code: Option<i32>,
    pub content: String,
}

impl ToolOutput {
    fn success(content: impl Into<String>) -> Self {
        Self {
            success: Some(true),
            exit_code: None,
            content: content.into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolError {
    message: String,
}

impl ToolError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }

    pub fn message(&self) -> &str {
        &self.message
    }
}

impl fmt::Display for ToolError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl Error for ToolError {}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReadFileArgs {
    file_path: PathBuf,
    offset: Option<usize>,
    limit: Option<usize>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ListDirArgs {
    dir_path: PathBuf,
    offset: Option<usize>,
    limit: Option<usize>,
    depth: Option<usize>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ShellArgs {
    command: String,
    cwd: Option<PathBuf>,
    timeout_ms: Option<u64>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct EditFileArgs {
    file_path: PathBuf,
    search: String,
    replace: String,
    expected_replacements: Option<usize>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct WriteFileArgs {
    file_path: PathBuf,
    content: String,
    overwrite: Option<bool>,
}

pub fn read_file(arguments: Value) -> Result<ToolOutput, ToolError> {
    let args = parse_args::<ReadFileArgs>(arguments)?;
    let offset = args.offset.unwrap_or(READ_FILE_DEFAULT_OFFSET);
    let limit = args.limit.unwrap_or(READ_FILE_DEFAULT_LIMIT);

    if offset == 0 {
        return Err(ToolError::new("offset must be greater than zero"));
    }

    if limit == 0 {
        return Err(ToolError::new("limit must be greater than zero"));
    }

    if !args.file_path.is_absolute() {
        return Err(ToolError::new("file_path must be absolute"));
    }

    let bytes = fs::read(&args.file_path)
        .map_err(|error| ToolError::new(format!("failed to read file: {error}")))?;
    let text = String::from_utf8_lossy(&bytes);
    let lines = text.lines().collect::<Vec<_>>();

    if offset > lines.len() {
        return Ok(ToolOutput::success("offset exceeds file length"));
    }

    let content = lines
        .iter()
        .enumerate()
        .skip(offset - 1)
        .take(limit)
        .map(|(index, line)| format!("L{}: {}", index + 1, cap_display(line)))
        .collect::<Vec<_>>()
        .join("\n");

    Ok(ToolOutput::success(content))
}

pub fn list_dir(arguments: Value) -> Result<ToolOutput, ToolError> {
    let args = parse_args::<ListDirArgs>(arguments)?;
    let offset = args.offset.unwrap_or(LIST_DIR_DEFAULT_OFFSET);
    let limit = args.limit.unwrap_or(LIST_DIR_DEFAULT_LIMIT);
    let depth = args.depth.unwrap_or(LIST_DIR_DEFAULT_DEPTH);

    if offset == 0 {
        return Err(ToolError::new("offset must be greater than zero"));
    }

    if limit == 0 {
        return Err(ToolError::new("limit must be greater than zero"));
    }

    if depth == 0 {
        return Err(ToolError::new("depth must be greater than zero"));
    }

    if !args.dir_path.is_absolute() {
        return Err(ToolError::new("dir_path must be absolute"));
    }

    let entries = collect_dir_entries(&args.dir_path, depth)?;

    if entries.is_empty() {
        return Ok(ToolOutput::success(format!(
            "Absolute path: {}",
            args.dir_path.display()
        )));
    }

    if offset > entries.len() {
        return Ok(ToolOutput::success("offset exceeds directory entry count"));
    }

    let mut selected = entries
        .iter()
        .skip(offset - 1)
        .take(limit)
        .cloned()
        .collect::<Vec<_>>();
    selected.sort_by(|left, right| left.sort_key.cmp(&right.sort_key));

    let mut lines = vec![format!("Absolute path: {}", args.dir_path.display())];
    lines.extend(selected.iter().map(format_dir_entry));

    if entries.len() > offset - 1 + limit {
        lines.push(format!("More than {limit} entries found"));
    }

    Ok(ToolOutput::success(lines.join("\n")))
}

pub fn shell(arguments: Value, default_cwd: &Path) -> Result<ToolOutput, ToolError> {
    let args = parse_args::<ShellArgs>(arguments)?;

    if args.command.trim().is_empty() {
        return Err(ToolError::new("command must not be empty"));
    }

    let cwd = args.cwd.unwrap_or_else(|| default_cwd.to_path_buf());
    if !cwd.is_absolute() {
        return Err(ToolError::new("cwd must be absolute"));
    }

    let timeout_ms = args.timeout_ms.unwrap_or(SHELL_DEFAULT_TIMEOUT_MS);
    if !(SHELL_MIN_TIMEOUT_MS..=SHELL_MAX_TIMEOUT_MS).contains(&timeout_ms) {
        return Err(ToolError::new(format!(
            "timeout_ms must be between {SHELL_MIN_TIMEOUT_MS} and {SHELL_MAX_TIMEOUT_MS}"
        )));
    }

    let shell_path = std::env::var_os("SHELL").unwrap_or_else(|| OsStr::new("/bin/sh").to_owned());
    let mut command = Command::new(shell_path);
    command
        .arg("-lc")
        .arg(&args.command)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    #[cfg(unix)]
    command.process_group(0);

    let mut child = command
        .spawn()
        .map_err(|error| ToolError::new(format!("failed to spawn shell command: {error}")))?;

    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| ToolError::new("failed to capture shell stdout"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| ToolError::new("failed to capture shell stderr"))?;

    let stdout_reader = thread::spawn(move || read_pipe(stdout));
    let stderr_reader = thread::spawn(move || read_pipe(stderr));

    let deadline = Instant::now() + Duration::from_millis(timeout_ms);
    let (exit_code, timed_out) = loop {
        match child.try_wait() {
            Ok(Some(status)) => break (status.code(), false),
            Ok(None) => {
                if Instant::now() >= deadline {
                    kill_child(&mut child);
                    let _ = child.wait();
                    break (None, true);
                }
                thread::sleep(Duration::from_millis(10));
            }
            Err(error) => {
                kill_child(&mut child);
                let _ = child.wait();
                return Err(ToolError::new(format!(
                    "failed to wait for shell command: {error}"
                )));
            }
        }
    };

    let stdout = join_pipe_reader(stdout_reader, "stdout")?;
    let stderr = join_pipe_reader(stderr_reader, "stderr")?;
    let content = format_shell_output(exit_code, timed_out, timeout_ms, &stdout, &stderr);

    Ok(ToolOutput {
        success: Some(!timed_out && exit_code == Some(0)),
        exit_code,
        content,
    })
}

pub fn edit_file(arguments: Value, default_cwd: &Path) -> Result<ToolOutput, ToolError> {
    let args = parse_args::<EditFileArgs>(arguments)?;

    if args.search.is_empty() {
        return Err(ToolError::new("search must not be empty"));
    }

    let expected_replacements = args.expected_replacements.unwrap_or(1);
    if expected_replacements == 0 {
        return Err(ToolError::new(
            "expected_replacements must be greater than zero",
        ));
    }

    let file_path = resolve_path_under_cwd(default_cwd, &args.file_path)?;
    let bytes = fs::read(&file_path)
        .map_err(|error| ToolError::new(format!("failed to read file: {error}")))?;
    let content = String::from_utf8(bytes)
        .map_err(|_| ToolError::new("failed to read file as UTF-8 text"))?;
    let actual_replacements = content.matches(&args.search).count();

    if actual_replacements != expected_replacements {
        return Err(ToolError::new(format!(
            "expected {expected_replacements} replacement(s), found {actual_replacements}"
        )));
    }

    let updated = content.replace(&args.search, &args.replace);
    fs::write(&file_path, updated)
        .map_err(|error| ToolError::new(format!("failed to write file: {error}")))?;

    Ok(ToolOutput::success(format!(
        "Edited {}: replaced {actual_replacements} occurrence(s).",
        file_path.display()
    )))
}

pub fn write_file(arguments: Value, default_cwd: &Path) -> Result<ToolOutput, ToolError> {
    let args = parse_args::<WriteFileArgs>(arguments)?;
    let overwrite = args.overwrite.unwrap_or(false);
    let file_path = resolve_path_under_cwd(default_cwd, &args.file_path)?;

    if file_path.exists() && !overwrite {
        return Err(ToolError::new(
            "file already exists; set overwrite to true to replace it",
        ));
    }

    if let Some(parent) = file_path.parent() {
        fs::create_dir_all(parent).map_err(|error| {
            ToolError::new(format!("failed to create parent directories: {error}"))
        })?;
    }

    fs::write(&file_path, &args.content)
        .map_err(|error| ToolError::new(format!("failed to write file: {error}")))?;

    Ok(ToolOutput::success(format!(
        "Wrote {}: {} byte(s).",
        file_path.display(),
        args.content.len()
    )))
}

pub fn read_file_tool_schema() -> Value {
    json!({
        "type": "function",
        "function": {
            "name": "read_file",
            "description": "Reads a local file with 1-indexed line numbers.",
            "parameters": {
                "type": "object",
                "required": ["file_path"],
                "additionalProperties": false,
                "properties": {
                    "file_path": {
                        "type": "string",
                        "description": "Absolute path to the file"
                    },
                    "offset": {
                        "type": "number",
                        "description": "The line number to start reading from. Must be 1 or greater."
                    },
                    "limit": {
                        "type": "number",
                        "description": "The maximum number of lines to return."
                    }
                }
            }
        }
    })
}

pub fn list_dir_tool_schema() -> Value {
    json!({
        "type": "function",
        "function": {
            "name": "list_dir",
            "description": "Lists entries in a local directory with 1-indexed entry numbers and simple type labels.",
            "parameters": {
                "type": "object",
                "required": ["dir_path"],
                "additionalProperties": false,
                "properties": {
                    "dir_path": {
                        "type": "string",
                        "description": "Absolute path to the directory to list."
                    },
                    "offset": {
                        "type": "number",
                        "description": "The entry number to start listing from. Must be 1 or greater."
                    },
                    "limit": {
                        "type": "number",
                        "description": "The maximum number of entries to return."
                    },
                    "depth": {
                        "type": "number",
                        "description": "The maximum directory depth to traverse. Must be 1 or greater."
                    }
                }
            }
        }
    })
}

pub fn shell_tool_schema() -> Value {
    json!({
        "type": "function",
        "function": {
            "name": "shell",
            "description": "Runs an unsandboxed shell command in the local environment. Commands are auto-approved and may read or modify local files.",
            "parameters": {
                "type": "object",
                "required": ["command"],
                "additionalProperties": false,
                "properties": {
                    "command": {
                        "type": "string",
                        "description": "Command string to run via the user's shell."
                    },
                    "cwd": {
                        "type": "string",
                        "description": "Absolute working directory. Defaults to the current user turn directory."
                    },
                    "timeout_ms": {
                        "type": "number",
                        "description": "Maximum runtime in milliseconds. Defaults to 5000."
                    }
                }
            }
        }
    })
}

pub fn edit_file_tool_schema() -> Value {
    json!({
        "type": "function",
        "function": {
            "name": "edit_file",
            "description": "Edits a UTF-8 text file by exact search and replace. The edit is only written when the number of matches equals expected_replacements.",
            "parameters": {
                "type": "object",
                "required": ["file_path", "search", "replace"],
                "additionalProperties": false,
                "properties": {
                    "file_path": {
                        "type": "string",
                        "description": "File path to edit. Relative paths are resolved under the current user turn directory."
                    },
                    "search": {
                        "type": "string",
                        "description": "Exact text to find. Must not be empty."
                    },
                    "replace": {
                        "type": "string",
                        "description": "Replacement text. May be empty."
                    },
                    "expected_replacements": {
                        "type": "number",
                        "description": "Exact number of occurrences that must be found before writing. Defaults to 1."
                    }
                }
            }
        }
    })
}

pub fn write_file_tool_schema() -> Value {
    json!({
        "type": "function",
        "function": {
            "name": "write_file",
            "description": "Creates a UTF-8 text file. Relative paths are resolved under the current user turn directory. Existing files are not overwritten unless overwrite is true.",
            "parameters": {
                "type": "object",
                "required": ["file_path", "content"],
                "additionalProperties": false,
                "properties": {
                    "file_path": {
                        "type": "string",
                        "description": "File path to create. Must be relative to the current user turn directory."
                    },
                    "content": {
                        "type": "string",
                        "description": "Complete UTF-8 text content to write."
                    },
                    "overwrite": {
                        "type": "boolean",
                        "description": "Whether to replace an existing file. Defaults to false."
                    }
                }
            }
        }
    })
}

fn parse_args<T>(arguments: Value) -> Result<T, ToolError>
where
    T: for<'de> Deserialize<'de>,
{
    serde_json::from_value(arguments)
        .map_err(|error| ToolError::new(format!("failed to parse function arguments: {error}")))
}

fn resolve_path_under_cwd(cwd: &Path, path: &Path) -> Result<PathBuf, ToolError> {
    if !cwd.is_absolute() {
        return Err(ToolError::new("cwd must be absolute"));
    }

    if path.as_os_str().is_empty() {
        return Err(ToolError::new("file_path must not be empty"));
    }

    let mut resolved = cwd.to_path_buf();

    for component in path.components() {
        match component {
            Component::Normal(part) => resolved.push(part),
            Component::CurDir => {}
            Component::ParentDir => {
                return Err(ToolError::new("file_path must not escape cwd"));
            }
            Component::RootDir | Component::Prefix(_) => {
                return Err(ToolError::new("file_path must be relative"));
            }
        }
    }

    Ok(resolved)
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct DirEntryInfo {
    name: String,
    sort_key: String,
    depth: usize,
    suffix: &'static str,
}

fn collect_dir_entries(root: &Path, max_depth: usize) -> Result<Vec<DirEntryInfo>, ToolError> {
    let mut entries = Vec::new();
    let mut queue = VecDeque::from([(root.to_path_buf(), PathBuf::new(), 0usize)]);

    while let Some((dir_path, relative_dir, parent_depth)) = queue.pop_front() {
        let mut dir_entries = fs::read_dir(&dir_path)
            .map_err(|error| ToolError::new(format!("failed to read directory: {error}")))?
            .collect::<Result<Vec<_>, io::Error>>()
            .map_err(|error| ToolError::new(format!("failed to inspect entry: {error}")))?;

        dir_entries.sort_by(|left, right| {
            normalized_relative_path(&relative_dir.join(left.file_name())).cmp(
                &normalized_relative_path(&relative_dir.join(right.file_name())),
            )
        });

        for dir_entry in dir_entries {
            let file_type = dir_entry
                .file_type()
                .map_err(|error| ToolError::new(format!("failed to inspect entry: {error}")))?;
            let file_name = dir_entry.file_name();
            let relative_path = relative_dir.join(&file_name);
            let entry_depth = parent_depth + 1;
            let suffix = entry_suffix(&file_type);

            entries.push(DirEntryInfo {
                name: os_str_to_string_lossy(&file_name),
                sort_key: normalized_relative_path(&relative_path),
                depth: entry_depth,
                suffix,
            });

            if file_type.is_dir() && entry_depth < max_depth {
                queue.push_back((dir_entry.path(), relative_path, entry_depth));
            }
        }
    }

    Ok(entries)
}

fn entry_suffix(file_type: &fs::FileType) -> &'static str {
    if file_type.is_symlink() {
        "@"
    } else if file_type.is_dir() {
        "/"
    } else if file_type.is_file() {
        ""
    } else {
        "?"
    }
}

fn format_dir_entry(entry: &DirEntryInfo) -> String {
    let indent = "  ".repeat(entry.depth.saturating_sub(1));
    format!("{indent}{}{}", cap_display(&entry.name), entry.suffix)
}

fn normalized_relative_path(path: &Path) -> String {
    path.components()
        .map(|component| component.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}

fn os_str_to_string_lossy(value: &OsStr) -> String {
    value.to_string_lossy().into_owned()
}

fn cap_display(value: &str) -> &str {
    if value.len() <= DISPLAY_BYTE_LIMIT {
        return value;
    }

    let mut limit = DISPLAY_BYTE_LIMIT;
    while !value.is_char_boundary(limit) {
        limit -= 1;
    }
    &value[..limit]
}

fn read_pipe<R>(mut pipe: R) -> io::Result<Vec<u8>>
where
    R: Read,
{
    let mut bytes = Vec::new();
    pipe.read_to_end(&mut bytes)?;
    Ok(bytes)
}

fn join_pipe_reader(
    reader: thread::JoinHandle<io::Result<Vec<u8>>>,
    stream_name: &str,
) -> Result<Vec<u8>, ToolError> {
    reader
        .join()
        .map_err(|_| ToolError::new(format!("failed to join shell {stream_name} reader")))?
        .map_err(|error| ToolError::new(format!("failed to read shell {stream_name}: {error}")))
}

#[cfg(unix)]
fn kill_child(child: &mut std::process::Child) {
    let pid = child.id() as libc::pid_t;
    // The child starts a new process group, so this also terminates commands
    // launched by the shell that still hold stdout/stderr pipes open.
    unsafe {
        libc::kill(-pid, libc::SIGKILL);
    }
}

#[cfg(not(unix))]
fn kill_child(child: &mut std::process::Child) {
    let _ = child.kill();
}

fn format_shell_output(
    exit_code: Option<i32>,
    timed_out: bool,
    timeout_ms: u64,
    stdout: &[u8],
    stderr: &[u8],
) -> String {
    let mut lines = Vec::new();

    match (timed_out, exit_code) {
        (true, _) => lines.push(format!("Timed out after {timeout_ms}ms")),
        (false, Some(code)) => lines.push(format!("Exit code: {code}")),
        (false, None) => lines.push("Exit code: <terminated by signal>".to_string()),
    }

    if !stdout.is_empty() {
        lines.push("stdout:".to_string());
        lines.push(format_capped_bytes(stdout, SHELL_OUTPUT_BYTE_LIMIT));
    }

    if !stderr.is_empty() {
        lines.push("stderr:".to_string());
        lines.push(format_capped_bytes(stderr, SHELL_OUTPUT_BYTE_LIMIT));
    }

    lines.join("\n")
}

fn format_capped_bytes(bytes: &[u8], byte_limit: usize) -> String {
    let (selected, truncated) = if bytes.len() > byte_limit {
        (&bytes[..byte_limit], true)
    } else {
        (bytes, false)
    };

    let mut output = String::from_utf8_lossy(selected).into_owned();
    if truncated {
        output.push_str(&format!(
            "\n<truncated: showing first {byte_limit} of {} bytes>",
            bytes.len()
        ));
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        env,
        fs::{self, File},
        io::Write,
        process,
        sync::atomic::{AtomicU64, Ordering},
        time::{SystemTime, UNIX_EPOCH},
    };

    static TEMP_DIR_COUNTER: AtomicU64 = AtomicU64::new(0);

    #[test]
    fn read_file_reads_full_file_with_line_numbers() {
        let temp = TempDir::new();
        let file_path = temp.path().join("sample.txt");
        fs::write(&file_path, "alpha\nbeta\ngamma\n").expect("write file");

        let output = read_file(json!({ "file_path": file_path })).expect("read file");

        assert_eq!(output.success, Some(true));
        assert_eq!(output.content, "L1: alpha\nL2: beta\nL3: gamma");
    }

    #[test]
    fn read_file_respects_offset_and_limit() {
        let temp = TempDir::new();
        let file_path = temp.path().join("sample.txt");
        fs::write(&file_path, "alpha\nbeta\ngamma\ndelta\n").expect("write file");

        let output = read_file(json!({
            "file_path": file_path,
            "offset": 2,
            "limit": 2
        }))
        .expect("read file");

        assert_eq!(output.content, "L2: beta\nL3: gamma");
    }

    #[test]
    fn read_file_reports_offset_past_file_length() {
        let temp = TempDir::new();
        let file_path = temp.path().join("sample.txt");
        fs::write(&file_path, "alpha\n").expect("write file");

        let output = read_file(json!({ "file_path": file_path, "offset": 2 })).expect("read file");

        assert_eq!(output.content, "offset exceeds file length");
    }

    #[test]
    fn read_file_rejects_relative_path_and_zero_bounds() {
        assert_eq!(
            read_file(json!({ "file_path": "relative.txt" }))
                .expect_err("relative path")
                .message(),
            "file_path must be absolute"
        );

        let path = absolute_nonexistent_path("sample.txt");
        assert_eq!(
            read_file(json!({ "file_path": path, "offset": 0 }))
                .expect_err("zero offset")
                .message(),
            "offset must be greater than zero"
        );

        let path = absolute_nonexistent_path("sample.txt");
        assert_eq!(
            read_file(json!({ "file_path": path, "limit": 0 }))
                .expect_err("zero limit")
                .message(),
            "limit must be greater than zero"
        );
    }

    #[test]
    fn read_file_handles_crlf_non_utf8_and_display_cap() {
        let temp = TempDir::new();
        let file_path = temp.path().join("sample.txt");
        let mut bytes = b"alpha\r\n".to_vec();
        bytes.extend([0xff, b'\n']);
        bytes.extend("é".repeat(300).as_bytes());
        fs::write(&file_path, bytes).expect("write file");

        let output = read_file(json!({ "file_path": file_path })).expect("read file");
        let lines = output.content.lines().collect::<Vec<_>>();

        assert_eq!(lines[0], "L1: alpha");
        assert_eq!(lines[1], "L2: �");
        assert!(lines[2].starts_with("L3: "));
        assert_eq!(lines[2].strip_prefix("L3: ").expect("prefix").len(), 500);
        assert!(lines[2].is_char_boundary(lines[2].len()));
    }

    #[test]
    fn read_file_reports_parse_and_io_errors() {
        let parse_error =
            read_file(json!({ "file_path": absolute_nonexistent_path("x"), "extra": true }))
                .expect_err("parse error");
        assert!(
            parse_error
                .message()
                .starts_with("failed to parse function arguments: ")
        );

        let io_error = read_file(json!({ "file_path": absolute_nonexistent_path("missing.txt") }))
            .expect_err("io error");
        assert!(io_error.message().starts_with("failed to read file: "));
    }

    #[test]
    fn list_dir_empty_directory_returns_absolute_path_only() {
        let temp = TempDir::new();

        let output = list_dir(json!({ "dir_path": temp.path() })).expect("list dir");

        assert_eq!(
            output.content,
            format!("Absolute path: {}", temp.path().display())
        );
    }

    #[test]
    fn list_dir_lists_files_directories_and_nested_entries() {
        let temp = TempDir::new();
        fs::write(temp.path().join("entry.txt"), "hello").expect("write file");
        fs::create_dir(temp.path().join("nested")).expect("create nested");
        fs::write(temp.path().join("nested").join("child.txt"), "child").expect("write child");
        fs::create_dir(temp.path().join("nested").join("deeper")).expect("create deeper");

        let output = list_dir(json!({
            "dir_path": temp.path(),
            "depth": 2,
            "limit": 10
        }))
        .expect("list dir");

        assert_eq!(
            output.content,
            format!(
                "Absolute path: {}\nentry.txt\nnested/\n  child.txt\n  deeper/",
                temp.path().display()
            )
        );
    }

    #[test]
    fn list_dir_respects_depth_offset_limit_and_more_message() {
        let temp = TempDir::new();
        fs::write(temp.path().join("a.txt"), "a").expect("write a");
        fs::write(temp.path().join("b.txt"), "b").expect("write b");
        fs::write(temp.path().join("c.txt"), "c").expect("write c");
        fs::create_dir(temp.path().join("nested")).expect("create nested");
        fs::write(temp.path().join("nested").join("hidden.txt"), "hidden").expect("write hidden");

        let output = list_dir(json!({
            "dir_path": temp.path(),
            "offset": 2,
            "limit": 2,
            "depth": 1
        }))
        .expect("list dir");

        assert_eq!(
            output.content,
            format!(
                "Absolute path: {}\nb.txt\nc.txt\nMore than 2 entries found",
                temp.path().display()
            )
        );
    }

    #[test]
    fn list_dir_reports_offset_past_entry_count() {
        let temp = TempDir::new();
        fs::write(temp.path().join("a.txt"), "a").expect("write a");

        let output = list_dir(json!({ "dir_path": temp.path(), "offset": 2 })).expect("list dir");

        assert_eq!(output.content, "offset exceeds directory entry count");
    }

    #[test]
    fn list_dir_rejects_relative_path_and_zero_bounds() {
        assert_eq!(
            list_dir(json!({ "dir_path": "relative" }))
                .expect_err("relative path")
                .message(),
            "dir_path must be absolute"
        );

        let path = absolute_nonexistent_path("dir");
        assert_eq!(
            list_dir(json!({ "dir_path": path, "offset": 0 }))
                .expect_err("zero offset")
                .message(),
            "offset must be greater than zero"
        );

        let path = absolute_nonexistent_path("dir");
        assert_eq!(
            list_dir(json!({ "dir_path": path, "limit": 0 }))
                .expect_err("zero limit")
                .message(),
            "limit must be greater than zero"
        );

        let path = absolute_nonexistent_path("dir");
        assert_eq!(
            list_dir(json!({ "dir_path": path, "depth": 0 }))
                .expect_err("zero depth")
                .message(),
            "depth must be greater than zero"
        );
    }

    #[test]
    fn list_dir_caps_long_entry_names() {
        let name = "é".repeat(300);
        let entry = DirEntryInfo {
            name,
            sort_key: "long-name".to_string(),
            depth: 1,
            suffix: "",
        };
        let formatted = format_dir_entry(&entry);

        assert_eq!(formatted.len(), 500);
        assert!(formatted.is_char_boundary(formatted.len()));
    }

    #[test]
    fn list_dir_reports_parse_and_read_errors() {
        let parse_error =
            list_dir(json!({ "dir_path": absolute_nonexistent_path("x"), "extra": true }))
                .expect_err("parse error");
        assert!(
            parse_error
                .message()
                .starts_with("failed to parse function arguments: ")
        );

        let read_error = list_dir(json!({ "dir_path": absolute_nonexistent_path("missing") }))
            .expect_err("read error");
        assert!(
            read_error
                .message()
                .starts_with("failed to read directory: ")
        );
    }

    #[test]
    fn tool_schemas_match_expected_model_shape() {
        let read_schema = read_file_tool_schema();
        assert_eq!(read_schema["type"], "function");
        assert_eq!(read_schema["function"]["name"], "read_file");
        assert_eq!(
            read_schema["function"]["parameters"]["required"],
            json!(["file_path"])
        );
        assert_eq!(
            read_schema["function"]["parameters"]["additionalProperties"],
            false
        );

        let list_schema = list_dir_tool_schema();
        assert_eq!(list_schema["type"], "function");
        assert_eq!(list_schema["function"]["name"], "list_dir");
        assert_eq!(
            list_schema["function"]["parameters"]["required"],
            json!(["dir_path"])
        );
        assert_eq!(
            list_schema["function"]["parameters"]["additionalProperties"],
            false
        );

        let shell_schema = shell_tool_schema();
        assert_eq!(shell_schema["type"], "function");
        assert_eq!(shell_schema["function"]["name"], "shell");
        assert_eq!(
            shell_schema["function"]["parameters"]["required"],
            json!(["command"])
        );
        assert_eq!(
            shell_schema["function"]["parameters"]["additionalProperties"],
            false
        );
        assert!(
            shell_schema["function"]["description"]
                .as_str()
                .expect("description")
                .contains("unsandboxed")
        );

        let edit_schema = edit_file_tool_schema();
        assert_eq!(edit_schema["type"], "function");
        assert_eq!(edit_schema["function"]["name"], "edit_file");
        assert_eq!(
            edit_schema["function"]["parameters"]["required"],
            json!(["file_path", "search", "replace"])
        );
        assert_eq!(
            edit_schema["function"]["parameters"]["additionalProperties"],
            false
        );

        let write_schema = write_file_tool_schema();
        assert_eq!(write_schema["type"], "function");
        assert_eq!(write_schema["function"]["name"], "write_file");
        assert_eq!(
            write_schema["function"]["parameters"]["required"],
            json!(["file_path", "content"])
        );
        assert_eq!(
            write_schema["function"]["parameters"]["additionalProperties"],
            false
        );
    }

    #[test]
    fn edit_file_replaces_one_occurrence_in_relative_path() {
        let temp = TempDir::new();
        let file_path = temp.path().join("sample.txt");
        fs::write(&file_path, "alpha\nbeta\n").expect("write file");

        let output = edit_file(
            json!({
                "file_path": "sample.txt",
                "search": "beta",
                "replace": "gamma"
            }),
            temp.path(),
        )
        .expect("edit file");

        assert_eq!(output.success, Some(true));
        assert_eq!(output.exit_code, None);
        assert!(output.content.contains("replaced 1 occurrence(s)"));
        assert_eq!(
            fs::read_to_string(&file_path).expect("read edited file"),
            "alpha\ngamma\n"
        );
    }

    #[test]
    fn edit_file_supports_multiple_expected_replacements() {
        let temp = TempDir::new();
        let file_path = temp.path().join("sample.txt");
        fs::write(&file_path, "red blue red\n").expect("write file");

        edit_file(
            json!({
                "file_path": "sample.txt",
                "search": "red",
                "replace": "green",
                "expected_replacements": 2
            }),
            temp.path(),
        )
        .expect("edit file");

        assert_eq!(
            fs::read_to_string(&file_path).expect("read edited file"),
            "green blue green\n"
        );
    }

    #[test]
    fn edit_file_rejects_invalid_arguments() {
        let temp = TempDir::new();
        fs::write(temp.path().join("sample.txt"), "alpha\n").expect("write file");

        assert_eq!(
            edit_file(
                json!({
                    "file_path": "sample.txt",
                    "search": "",
                    "replace": "beta"
                }),
                temp.path()
            )
            .expect_err("empty search")
            .message(),
            "search must not be empty"
        );
        assert_eq!(
            edit_file(
                json!({
                    "file_path": "../sample.txt",
                    "search": "alpha",
                    "replace": "beta"
                }),
                temp.path()
            )
            .expect_err("path traversal")
            .message(),
            "file_path must not escape cwd"
        );
        assert_eq!(
            edit_file(
                json!({
                    "file_path": "sample.txt",
                    "search": "alpha",
                    "replace": "beta",
                    "expected_replacements": 0
                }),
                temp.path()
            )
            .expect_err("zero replacements")
            .message(),
            "expected_replacements must be greater than zero"
        );
    }

    #[test]
    fn edit_file_fails_without_writing_when_match_count_differs() {
        let temp = TempDir::new();
        let file_path = temp.path().join("sample.txt");
        fs::write(&file_path, "one two one\n").expect("write file");

        let error = edit_file(
            json!({
                "file_path": "sample.txt",
                "search": "one",
                "replace": "three"
            }),
            temp.path(),
        )
        .expect_err("unexpected match count");

        assert_eq!(error.message(), "expected 1 replacement(s), found 2");
        assert_eq!(
            fs::read_to_string(&file_path).expect("read unchanged file"),
            "one two one\n"
        );
    }

    #[test]
    fn edit_file_rejects_non_utf8_files() {
        let temp = TempDir::new();
        let file_path = temp.path().join("sample.bin");
        fs::write(&file_path, [0xff, 0xfe]).expect("write file");

        let error = edit_file(
            json!({
                "file_path": "sample.bin",
                "search": "x",
                "replace": "y"
            }),
            temp.path(),
        )
        .expect_err("non utf8");

        assert_eq!(error.message(), "failed to read file as UTF-8 text");
    }

    #[test]
    fn write_file_creates_new_relative_file() {
        let temp = TempDir::new();
        let output = write_file(
            json!({
                "file_path": "sample.txt",
                "content": "hello\n"
            }),
            temp.path(),
        )
        .expect("write file");

        assert_eq!(output.success, Some(true));
        assert_eq!(output.exit_code, None);
        assert_eq!(
            output.content,
            format!(
                "Wrote {}: 6 byte(s).",
                temp.path().join("sample.txt").display()
            )
        );
        assert_eq!(
            fs::read_to_string(temp.path().join("sample.txt")).expect("read written file"),
            "hello\n"
        );
    }

    #[test]
    fn write_file_creates_missing_parent_directories() {
        let temp = TempDir::new();

        write_file(
            json!({
                "file_path": "nested/deeper/sample.txt",
                "content": "hello"
            }),
            temp.path(),
        )
        .expect("write nested file");

        assert_eq!(
            fs::read_to_string(temp.path().join("nested/deeper/sample.txt"))
                .expect("read written file"),
            "hello"
        );
    }

    #[test]
    fn write_file_rejects_overwrite_by_default() {
        let temp = TempDir::new();
        let file_path = temp.path().join("sample.txt");
        fs::write(&file_path, "original").expect("write original");

        let error = write_file(
            json!({
                "file_path": "sample.txt",
                "content": "replacement"
            }),
            temp.path(),
        )
        .expect_err("overwrite should fail");

        assert_eq!(
            error.message(),
            "file already exists; set overwrite to true to replace it"
        );
        assert_eq!(
            fs::read_to_string(&file_path).expect("read unchanged file"),
            "original"
        );
    }

    #[test]
    fn write_file_overwrites_when_requested() {
        let temp = TempDir::new();
        let file_path = temp.path().join("sample.txt");
        fs::write(&file_path, "original").expect("write original");

        write_file(
            json!({
                "file_path": "sample.txt",
                "content": "replacement",
                "overwrite": true
            }),
            temp.path(),
        )
        .expect("overwrite file");

        assert_eq!(
            fs::read_to_string(&file_path).expect("read overwritten file"),
            "replacement"
        );
    }

    #[test]
    fn write_file_rejects_invalid_arguments() {
        let temp = TempDir::new();

        assert_eq!(
            write_file(
                json!({
                    "file_path": "../sample.txt",
                    "content": "hello"
                }),
                temp.path()
            )
            .expect_err("path traversal")
            .message(),
            "file_path must not escape cwd"
        );
        assert_eq!(
            write_file(
                json!({
                    "file_path": temp.path().join("sample.txt"),
                    "content": "hello"
                }),
                temp.path()
            )
            .expect_err("absolute path")
            .message(),
            "file_path must be relative"
        );

        let parse_error = write_file(
            json!({
                "file_path": "sample.txt",
                "content": "hello",
                "extra": true
            }),
            temp.path(),
        )
        .expect_err("parse error");
        assert!(
            parse_error
                .message()
                .starts_with("failed to parse function arguments: ")
        );
    }

    #[test]
    fn shell_runs_command_in_default_cwd() {
        let temp = TempDir::new();

        let output = shell(json!({ "command": "pwd" }), temp.path()).expect("run shell");

        assert_eq!(output.success, Some(true));
        assert_eq!(output.exit_code, Some(0));
        assert!(output.content.contains("Exit code: 0"));
        assert!(output.content.contains(&temp.path().display().to_string()));
    }

    #[test]
    fn shell_uses_explicit_cwd() {
        let default_cwd = TempDir::new();
        let explicit_cwd = TempDir::new();

        let output = shell(
            json!({
                "command": "pwd",
                "cwd": explicit_cwd.path()
            }),
            default_cwd.path(),
        )
        .expect("run shell");

        assert!(
            output
                .content
                .contains(&explicit_cwd.path().display().to_string())
        );
        assert!(
            !output
                .content
                .contains(&default_cwd.path().display().to_string())
        );
    }

    #[test]
    fn shell_captures_stderr_and_nonzero_exit() {
        let temp = TempDir::new();

        let output = shell(
            json!({ "command": "printf problem >&2; exit 7" }),
            temp.path(),
        )
        .expect("run shell");

        assert_eq!(output.success, Some(false));
        assert_eq!(output.exit_code, Some(7));
        assert!(output.content.contains("Exit code: 7"));
        assert!(output.content.contains("stderr:\nproblem"));
    }

    #[test]
    fn shell_times_out() {
        let temp = TempDir::new();

        let output = shell(
            json!({
                "command": "sleep 1",
                "timeout_ms": 25
            }),
            temp.path(),
        )
        .expect("run shell");

        assert_eq!(output.success, Some(false));
        assert_eq!(output.exit_code, None);
        assert!(output.content.contains("Timed out after 25ms"));
    }

    #[test]
    fn shell_rejects_invalid_arguments() {
        let temp = TempDir::new();

        assert_eq!(
            shell(json!({ "command": " " }), temp.path())
                .expect_err("empty command")
                .message(),
            "command must not be empty"
        );
        assert_eq!(
            shell(json!({ "command": "pwd", "cwd": "relative" }), temp.path())
                .expect_err("relative cwd")
                .message(),
            "cwd must be absolute"
        );
        assert_eq!(
            shell(json!({ "command": "pwd", "timeout_ms": 0 }), temp.path())
                .expect_err("bad timeout")
                .message(),
            "timeout_ms must be between 1 and 300000"
        );
    }

    #[cfg(unix)]
    #[test]
    fn list_dir_marks_symlinks() {
        use std::os::unix::fs::symlink;

        let temp = TempDir::new();
        fs::write(temp.path().join("target.txt"), "target").expect("write target");
        symlink(temp.path().join("target.txt"), temp.path().join("link")).expect("create symlink");

        let output = list_dir(json!({ "dir_path": temp.path(), "limit": 10 })).expect("list dir");

        assert!(output.content.contains("link@"));
        assert!(output.content.contains("target.txt"));
    }

    fn absolute_nonexistent_path(name: &str) -> PathBuf {
        env::temp_dir().join(format!("poe-agent-nonexistent-{name}"))
    }

    struct TempDir {
        path: PathBuf,
    }

    impl TempDir {
        fn new() -> Self {
            let unique = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("system time")
                .as_nanos();
            let counter = TEMP_DIR_COUNTER.fetch_add(1, Ordering::Relaxed);
            let path = env::temp_dir().join(format!(
                "poe-agent-tools-test-{}-{unique}-{counter}",
                process::id()
            ));
            fs::create_dir(&path).expect("create temp dir");
            Self { path }
        }

        fn path(&self) -> &Path {
            &self.path
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    #[allow(dead_code)]
    fn write_bytes(path: &Path, bytes: &[u8]) {
        let mut file = File::create(path).expect("create file");
        file.write_all(bytes).expect("write bytes");
    }
}
