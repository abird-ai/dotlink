use std::{
    collections::HashSet,
    io::{self, IsTerminal},
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU8, Ordering},
    },
    time::Instant,
};

use chrono::Local;
use clap::ValueEnum;
use serde_json::{Map, Value};

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, ValueEnum)]
pub enum ColorMode {
    #[default]
    Auto,
    Always,
    Never,
}

impl ColorMode {
    pub fn stdout_enabled(self) -> bool {
        match self {
            Self::Auto => io::stdout().is_terminal(),
            Self::Always => true,
            Self::Never => false,
        }
    }

    fn stderr_enabled(self) -> bool {
        match self {
            Self::Auto => io::stderr().is_terminal(),
            Self::Always => true,
            Self::Never => false,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum OutcomeTone {
    Success,
    Neutral,
    Error,
}

/// Typed completion outcome shared by TOOL and REQ logs. Callers provide the
/// semantic class rather than relying on label parsing; HTTP status classes are
/// classified centrally so every transport renders them consistently.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum LogOutcome<'a> {
    Success(&'a str),
    Neutral(&'a str),
    Error(&'a str),
    Http(u16),
}

impl<'a> LogOutcome<'a> {
    pub(crate) const fn success(label: &'a str) -> Self {
        Self::Success(label)
    }

    pub(crate) const fn neutral(label: &'a str) -> Self {
        Self::Neutral(label)
    }

    pub(crate) const fn error(label: &'a str) -> Self {
        Self::Error(label)
    }

    pub(crate) const fn http(status: u16) -> Self {
        Self::Http(status)
    }

    fn tone(self) -> OutcomeTone {
        match self {
            Self::Success(_) | Self::Http(200..=299) => OutcomeTone::Success,
            Self::Error(_) | Self::Http(400..=599) => OutcomeTone::Error,
            Self::Neutral(_) | Self::Http(_) => OutcomeTone::Neutral,
        }
    }
}

#[derive(Debug)]
struct LogState {
    verbosity: AtomicU8,
    quiet_activity: AtomicBool,
    interactive: AtomicBool,
    full_commands: AtomicBool,
}

#[derive(Clone, Debug)]
pub struct LogConfig {
    state: Arc<LogState>,
    color: bool,
}

impl Default for LogConfig {
    fn default() -> Self {
        Self::new(0, false, ColorMode::Never)
    }
}

impl LogConfig {
    pub fn new(verbose: u8, quiet_activity: bool, color_mode: ColorMode) -> Self {
        let color = color_mode.stderr_enabled();
        Self {
            state: Arc::new(LogState {
                verbosity: AtomicU8::new(verbose.min(2)),
                quiet_activity: AtomicBool::new(quiet_activity),
                interactive: AtomicBool::new(false),
                full_commands: AtomicBool::new(false),
            }),
            color,
        }
    }

    pub fn color_enabled(&self) -> bool {
        self.color
    }

    pub fn activity_enabled(&self) -> bool {
        self.state.verbosity.load(Ordering::Relaxed) >= 1
            && !self.state.quiet_activity.load(Ordering::Relaxed)
    }

    pub fn developer_enabled(&self) -> bool {
        self.state.verbosity.load(Ordering::Relaxed) >= 2
    }

    pub fn verbosity_label(&self) -> &'static str {
        let verbosity = self.state.verbosity.load(Ordering::Relaxed);
        let quiet = self.state.quiet_activity.load(Ordering::Relaxed);
        match (verbosity, quiet) {
            (0, _) => "quiet",
            (1, true) => "quiet",
            (1, false) => "TOOL",
            (_, true) => "REQ",
            (_, false) => "TOOL + REQ",
        }
    }

    /// RuntimeControls alone enables this after acquiring interactive stdin.
    /// Disabling it resets full command logging for services and restarts.
    pub(crate) fn set_interactive(&self, active: bool) {
        if !active {
            self.state.full_commands.store(false, Ordering::Relaxed);
        }
        self.state.interactive.store(active, Ordering::Relaxed);
    }

    pub(crate) fn toggle_full_commands(&self) -> Option<bool> {
        if !self.state.interactive.load(Ordering::Relaxed) || !self.activity_enabled() {
            return None;
        }
        Some(!self.state.full_commands.fetch_xor(true, Ordering::Relaxed))
    }

    fn full_commands_enabled(&self) -> bool {
        self.activity_enabled()
            && self.state.interactive.load(Ordering::Relaxed)
            && self.state.full_commands.load(Ordering::Relaxed)
    }

    fn highlight_fields(&self) -> bool {
        self.color && self.state.interactive.load(Ordering::Relaxed)
    }

    /// Shared TOOL summary for stdio, HTTP and OpenAI. Only interactive
    /// terminal controls can choose an uncapped command excerpt.
    pub(crate) fn summarize_arguments<'a>(
        &self,
        tool: &str,
        arguments: &Map<String, Value>,
        keys: impl IntoIterator<Item = &'a str>,
        cwd: Option<&Path>,
    ) -> String {
        summarize_arguments_with_options(
            tool,
            arguments,
            keys,
            cwd,
            self.full_commands_enabled(),
            self.highlight_fields(),
        )
    }

    pub fn cycle_verbosity(&self) -> &'static str {
        self.state.quiet_activity.store(false, Ordering::Relaxed);
        let mut current = self.state.verbosity.load(Ordering::Relaxed).min(2);
        loop {
            let next = (current + 1) % 3;
            match self.state.verbosity.compare_exchange_weak(
                current,
                next,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => {
                    if next == 0 {
                        // Don't silently resume full command logging when TOOL
                        // verbosity is re-enabled later.
                        self.state.full_commands.store(false, Ordering::Relaxed);
                    }
                    return self.verbosity_label();
                }
                Err(actual) => current = actual.min(2),
            }
        }
    }

    pub fn notice(&self, scope: &str, message: impl AsRef<str>) {
        self.line("LOG", scope, message.as_ref(), "33");
    }

    pub fn developer(&self, scope: &str, message: impl AsRef<str>) {
        if self.developer_enabled() {
            self.line("DBG", scope, message.as_ref(), "90");
        }
    }

    pub fn activity_start(&self, tool: &str, detail: impl AsRef<str>) -> Option<Instant> {
        if !self.activity_enabled() {
            return None;
        }
        let detail = detail.as_ref();
        let arrow = if self.highlight_fields() {
            "\x1b[2m→\x1b[0m"
        } else {
            "→"
        };
        let message = if detail.is_empty() {
            arrow.to_owned()
        } else {
            format!("{arrow} {detail}")
        };
        self.line("TOOL", tool, &message, "36");
        Some(Instant::now())
    }

    pub fn activity_done(&self, tool: &str, started: Option<Instant>, outcome: LogOutcome<'_>) {
        let Some(started) = started else {
            return;
        };
        let message = format_log_outcome(
            outcome,
            started.elapsed().as_millis(),
            self.highlight_fields(),
        );
        self.line("TOOL", tool, &message, "36");
    }

    pub fn request(&self, transport: &str, method: &str) -> Option<Instant> {
        if !self.developer_enabled() {
            return None;
        }
        let message = if self.highlight_fields() {
            format!("\x1b[2m→\x1b[0m \x1b[35m{method}\x1b[0m")
        } else {
            format!("→ {method}")
        };
        self.line("REQ", transport, &message, "35");
        Some(Instant::now())
    }

    pub fn request_done(
        &self,
        transport: &str,
        method: &str,
        started: Option<Instant>,
        outcome: LogOutcome<'_>,
    ) {
        let Some(started) = started else {
            return;
        };
        let colored = self.highlight_fields();
        let elapsed = started.elapsed().as_millis();
        let arrow = if colored { "\x1b[2m←\x1b[0m" } else { "←" };
        let method = if colored {
            format!("\x1b[35m{method}\x1b[0m")
        } else {
            method.to_owned()
        };
        let message = format!(
            "{arrow} {method}  {}",
            outcome_label(outcome, elapsed, colored)
        );
        self.line("REQ", transport, &message, "35");
    }

    pub fn style(&self, code: &str, text: impl AsRef<str>) -> String {
        if self.color {
            format!("\x1b[{code}m{}\x1b[0m", text.as_ref())
        } else {
            text.as_ref().to_owned()
        }
    }

    fn line(&self, level: &str, scope: &str, message: &str, color_code: &str) {
        let timestamp = Local::now().format("%H:%M:%S%.3f");
        let scope = sanitize_log_scope(scope);
        if self.color {
            eprintln!(
                "\x1b[2m[{timestamp}]\x1b[0m \x1b[{color_code}m{level:<4}\x1b[0m \x1b[1m{scope:<10}\x1b[0m {message}"
            );
        } else {
            eprintln!("[{timestamp}] {level:<4} {scope:<10} {message}");
        }
    }
}

/// Shared TOOL/REQ completion formatting. Outcome class is structured at the
/// call site (or derived from an HTTP code) rather than guessed from display
/// text, so transports and tools cannot silently drift in status coloring.
fn outcome_label(outcome: LogOutcome<'_>, elapsed_ms: u128, colored: bool) -> String {
    let label = match outcome {
        LogOutcome::Success(label) | LogOutcome::Neutral(label) | LogOutcome::Error(label) => {
            label.to_owned()
        }
        LogOutcome::Http(status) => status.to_string(),
    };
    if !colored {
        return format!("{label}  {elapsed_ms}ms");
    }
    let tone = match outcome.tone() {
        OutcomeTone::Success => "92",
        OutcomeTone::Neutral => "93",
        OutcomeTone::Error => "91",
    };
    format!("\x1b[1;{tone}m{label}\x1b[0m  \x1b[2m{elapsed_ms}ms\x1b[0m")
}

fn format_log_outcome(outcome: LogOutcome<'_>, elapsed_ms: u128, colored: bool) -> String {
    let arrow = if colored { "\x1b[2m←\x1b[0m" } else { "←" };
    format!("{arrow} {}", outcome_label(outcome, elapsed_ms, colored))
}

const LOG_SCOPE_CHARS: usize = 48;

/// Log scopes include MCP tool names. Keep every caller on one strict,
/// single-line, bounded representation before terminal styling is applied.
fn sanitize_log_scope(scope: &str) -> String {
    let mut chars = scope.chars();
    let mut sanitized: String = chars
        .by_ref()
        .take(LOG_SCOPE_CHARS)
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '/' | '-' | '_' | '.') {
                character
            } else {
                '�'
            }
        })
        .collect();
    if chars.next().is_some() {
        sanitized.pop();
        sanitized.push('…');
    }
    sanitized
}

/// Default maximum visible command value (including quotes and escaping).
/// Paths are always logged in full; only interactive `V` removes this cap.
const LOG_ARGUMENT_CHARS: usize = 256;
/// Unrelated scalar strings longer than this are still summarized as bytes.
const SUMMARY_VALUE_CHARS: usize = 120;

/// Quote and escape a log value without formatting an unbounded string.
/// Accounting for escaped characters prevents a long script of newlines or
/// control bytes from exceeding the visible limit or spanning journal lines.
fn quoted_log_argument(value: &str, limit: usize) -> String {
    debug_assert!(limit >= 3);
    let mut result = String::with_capacity(limit + 16);
    let mut chars = value.chars().peekable();
    let mut rendered = 1_usize; // opening quote
    result.push('"');
    while let Some(character) = chars.next() {
        // Rust's str Debug leaves single quotes alone, unlike char Debug.
        let escaped = character.escape_debug();
        let count = if character == '\'' {
            1
        } else {
            escaped.clone().count()
        };
        let reserve = 1 + usize::from(chars.peek().is_some()); // quote + ellipsis
        if rendered + count + reserve > limit {
            result.push('…');
            break;
        }
        if character == '\'' {
            result.push(character);
        } else {
            result.extend(escaped);
        }
        rendered += count;
    }
    result.push('"');
    result
}

/// Payloads other than executable commands remain byte counts. TOOL logging
/// is opt-in (`-v` / `logging.verbosity = "tools"`); executable commands are
/// short, escaped excerpts rather than unbounded journal entries.
const PAYLOAD_ARGUMENTS: &[&str] = &[
    "content", "stdin", "data", "old_text", "new_text", "patch", "pattern",
];

/// Render a requested path relative to the launch directory *only* when its
/// resolved target is inside it. Follow existing symlinks (including ancestors
/// of not-yet-created files) for accurate audit labels, without authorizing
/// any access. If resolution is unavailable, fall back to lexical spelling;
/// the tool itself still performs its independent authority checks.
fn display_path(input: &str, cwd: Option<&Path>) -> String {
    let Some(cwd) = cwd else {
        return input.to_owned();
    };
    let request = Path::new(input);
    let absolute = crate::mcp::resolve_policy_path(cwd, request).unwrap_or_else(|_| {
        crate::mcp::lexical_normalize(&if request.is_absolute() {
            request.to_path_buf()
        } else {
            cwd.join(request)
        })
    });
    let cwd = crate::mcp::resolve_policy_path(cwd, Path::new("."))
        .unwrap_or_else(|_| crate::mcp::lexical_normalize(cwd));
    let value = match absolute.strip_prefix(&cwd) {
        Ok(relative) if relative.as_os_str().is_empty() => "./".to_owned(),
        Ok(relative) => format!("./{}", relative.display()),
        Err(_) => absolute.display().to_string(),
    };
    if cfg!(windows) {
        value.replace('\\', "/")
    } else {
        value
    }
}

/// Enumerate only work that `edit` can accept. Show every distinct path in
/// a valid call (up to MAX_EDIT_FILES) without truncating its spelling. When
/// the request itself exceeds the tool ceilings, report an overflow instead
/// of scanning unbounded or doing quadratic work before tool validation.
fn edit_paths(arguments: &Map<String, Value>, cwd: Option<&Path>) -> Option<String> {
    use crate::mcp::{MAX_EDIT_FILES, MAX_EDIT_OPS};

    let edits = arguments.get("edits")?.as_array()?;
    let fallback = arguments.get("path").and_then(Value::as_str);
    let mut seen = HashSet::with_capacity(MAX_EDIT_FILES);
    let mut paths = Vec::with_capacity(MAX_EDIT_FILES);
    let mut exceeded_files = false;
    for edit in edits.iter().take(MAX_EDIT_OPS) {
        let Some(path) = edit.get("path").and_then(Value::as_str).or(fallback) else {
            continue;
        };
        if !seen.insert(path) {
            continue;
        }
        if seen.len() > MAX_EDIT_FILES {
            exceeded_files = true;
            break;
        }
        paths.push(format!("{:?}", display_path(path, cwd)));
    }
    if paths.is_empty() && !exceeded_files {
        return None;
    }
    let mut preview = format!("paths=[{}]", paths.join(", "));
    if exceeded_files {
        preview.push_str(&format!(" files_above_limit={MAX_EDIT_FILES}"));
    }
    let unscanned = edits.len().saturating_sub(MAX_EDIT_OPS);
    if unscanned > 0 {
        preview.push_str(&format!(" unscanned_edits={unscanned}"));
    }
    Some(preview)
}

/// A color-safe field is assembled *before* the summary is joined: quoted
/// user values can contain text such as ` p=` without becoming new fields.
#[derive(Clone, Copy)]
enum FieldTone {
    Path,
    Directory,
    Command,
    Numeric,
    Boolean,
    Text,
}

fn log_field(key: &str, value: impl AsRef<str>, tone: FieldTone, colored: bool) -> String {
    let value = value.as_ref();
    if !colored {
        return format!("{key}={value}");
    }
    let (key_color, value_color) = match tone {
        FieldTone::Path => ("1;36", "96"),
        FieldTone::Directory => ("1;34", "94"),
        FieldTone::Command => ("1;33", "93"),
        FieldTone::Numeric => ("1;35", "95"),
        FieldTone::Boolean => ("1;36", if value == "true" { "92" } else { "90" }),
        FieldTone::Text => ("1;36", "37"),
    };
    format!("\x1b[{key_color}m{key}\x1b[2m=\x1b[{value_color}m{value}\x1b[0m")
}

/// Stable, operator-friendly argument order. The JSON schema currently stores
/// properties in a sorted map, which otherwise puts offset/limit before path.
/// Reuse this order for local HTTP, stdio, and OpenAI diagnostic summaries.
#[cfg(test)]
pub fn summarize_arguments<'a>(
    tool: &str,
    arguments: &Map<String, Value>,
    keys: impl IntoIterator<Item = &'a str>,
    cwd: Option<&Path>,
) -> String {
    summarize_arguments_with_options(tool, arguments, keys, cwd, false, false)
}

fn summarize_arguments_with_options<'a>(
    tool: &str,
    arguments: &Map<String, Value>,
    keys: impl IntoIterator<Item = &'a str>,
    cwd: Option<&Path>,
    full_commands: bool,
    colored: bool,
) -> String {
    let keys = keys.into_iter().collect::<Vec<_>>();
    let is_edit = tool == "edit";
    let mut ordered = Vec::with_capacity(keys.len());
    for preferred in ["path", "dir", "command"] {
        if keys.contains(&preferred) && !(is_edit && preferred == "path") {
            ordered.push(preferred);
        }
    }
    ordered.extend(
        keys.into_iter()
            .filter(|key| !matches!(*key, "path" | "dir" | "command")),
    );
    let mut parts = Vec::new();
    if is_edit && let Some(paths) = edit_paths(arguments, cwd) {
        // `edit_paths` constructs this trusted, complete valid-file list.
        // Never reinterpret quoted user path values as new log fields.
        let (key, value) = paths.split_once('=').expect("edit paths summary has a key");
        parts.push(log_field(key, value, FieldTone::Path, colored));
    }
    let mut payload_bytes = 0_usize;
    for key in ordered {
        if is_edit && key == "path" {
            continue;
        }
        let Some(value) = arguments.get(key) else {
            continue;
        };
        match value {
            // Keep executable commands visible but capped, with one-line
            // escaping. File contents and other bulk payloads stay byte counts.
            Value::String(value) if key == "command" => {
                let quoted = if full_commands {
                    format!("{value:?}")
                } else {
                    quoted_log_argument(value, LOG_ARGUMENT_CHARS)
                };
                parts.push(log_field("c", quoted, FieldTone::Command, colored));
            }
            Value::String(value) if key == "path" => {
                parts.push(log_field(
                    "p",
                    format!("{:?}", display_path(value, cwd)),
                    FieldTone::Path,
                    colored,
                ));
            }
            Value::String(value) if key == "dir" => {
                parts.push(log_field(
                    "d",
                    format!("{:?}", display_path(value, cwd)),
                    FieldTone::Directory,
                    colored,
                ));
            }
            Value::String(value) if PAYLOAD_ARGUMENTS.contains(&key) => {
                payload_bytes = payload_bytes.saturating_add(value.len());
            }
            Value::String(value) if value.chars().nth(SUMMARY_VALUE_CHARS).is_some() => {
                payload_bytes = payload_bytes.saturating_add(value.len());
            }
            Value::String(value) => {
                let normalized = value.split_whitespace().collect::<Vec<_>>().join(" ");
                parts.push(log_field(
                    key,
                    quoted_log_argument(&normalized, LOG_ARGUMENT_CHARS),
                    FieldTone::Text,
                    colored,
                ));
            }
            Value::Bool(value) => parts.push(log_field(
                key,
                value.to_string(),
                FieldTone::Boolean,
                colored,
            )),
            Value::Number(value) => {
                let short = match key {
                    "timeout_secs" => "max_t",
                    "max_output_bytes" | "max_read_bytes" | "max_write_bytes" => "max_b",
                    _ => key,
                };
                parts.push(log_field(
                    short,
                    value.to_string(),
                    FieldTone::Numeric,
                    colored,
                ));
            }
            Value::Array(items) => parts.push(log_field(
                key,
                items.len().to_string(),
                FieldTone::Numeric,
                colored,
            )),
            Value::Null | Value::Object(_) => {}
        }
    }
    if payload_bytes > 0 {
        parts.push(log_field(
            "b",
            format!("{:?}", payload_bytes.to_string()),
            FieldTone::Numeric,
            colored,
        ));
    }
    parts.join(" ")
}

/// Names from a JSON-RPC envelope are untrusted input even at diagnostic
/// verbosity. Permit only short, ASCII, single-line protocol identifiers.
pub(crate) fn protocol_identifier<'a>(value: Option<&'a str>, fallback: &'static str) -> &'a str {
    value
        .filter(|value| {
            !value.is_empty()
                && value.len() <= 96
                && value.bytes().all(|byte| {
                    byte.is_ascii_alphanumeric() || matches!(byte, b'/' | b'-' | b'_' | b'.')
                })
        })
        .unwrap_or(fallback)
}

pub(crate) fn jsonrpc_request_label(value: &Value, include_tool: bool) -> String {
    let method = protocol_identifier(
        value.get("method").and_then(Value::as_str),
        "unknown JSON-RPC method",
    );
    if include_tool && method == "tools/call" {
        return protocol_identifier(
            value.pointer("/params/name").and_then(Value::as_str),
            "tools/call",
        )
        .to_owned();
    }
    method.to_owned()
}

pub fn truncate(value: &str, max_chars: usize) -> String {
    let mut chars = value.chars();
    let prefix: String = chars.by_ref().take(max_chars).collect();
    if chars.next().is_some() {
        format!("{prefix}…")
    } else {
        prefix
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jsonrpc_identifiers_are_bounded_and_sanitized_for_every_transport() {
        use serde_json::json;
        assert_eq!(
            jsonrpc_request_label(
                &json!({"method":"tools/call","params":{"name":"read"}}),
                true
            ),
            "read"
        );
        assert_eq!(
            jsonrpc_request_label(
                &json!({"method":"tools/call","params":{"name":"read"}}),
                false
            ),
            "tools/call"
        );
        for value in [
            "read\ninjected",
            "read\rESC",
            "read\u{1b}[31mred",
            "a".repeat(97).as_str(),
            "access_token:secret",
        ] {
            let envelope = json!({"method":"tools/call","params":{"name":value}});
            assert_eq!(jsonrpc_request_label(&envelope, true), "tools/call");
            let envelope = json!({"method":value});
            assert_eq!(
                jsonrpc_request_label(&envelope, false),
                "unknown JSON-RPC method"
            );
        }
        assert_eq!(
            jsonrpc_request_label(&json!({"params":{"method":"payload-secret"}}), false),
            "unknown JSON-RPC method"
        );
    }

    #[test]
    fn argument_summaries_redact_bulk_values_and_keep_scalars() {
        let arguments = serde_json::json!({
            "path": "src/main.rs",
            "command": "curl -H 'Authorization: Bearer FAKE_TEST_SECRET' https://example.org",
            "content": "x".repeat(200),
            "old_text": "short secret",
            "pattern": "FAKE_GREP_SECRET",
            "edits": [{"old_text": "a"}, {"old_text": "b"}],
            "literal": true,
            "limit": 20,
            "absent": null,
        });
        let summary = summarize_arguments(
            "write",
            arguments.as_object().unwrap(),
            [
                "path", "command", "content", "old_text", "pattern", "edits", "literal", "limit",
                "absent", "missing",
            ],
            None,
        );
        assert!(summary.contains("p=\"src/main.rs\""), "{summary}");
        assert!(summary.contains("c=\"curl -H"), "{summary}");
        assert!(summary.contains("FAKE_TEST_SECRET"), "{summary}");
        assert!(summary.starts_with("p=\"src/main.rs\" "), "{summary}");
        // Three payloads share a single byte summary; raw payloads stay out.
        assert!(summary.contains("b=\"228\""), "{summary}");
        assert!(!summary.contains("short secret") && !summary.contains("FAKE_GREP_SECRET"));
        assert!(summary.contains("edits=2"), "{summary}");
        assert!(summary.contains("literal=true"), "{summary}");
        assert!(summary.contains("limit=20"), "{summary}");
        assert!(!summary.contains("xxxx"), "{summary}");
    }

    #[test]
    fn tool_log_prefers_path_and_bounds_escaped_shell_command() {
        let long_script = format!("printf 'hello'\n{}\u{1b}[31m", "x".repeat(1024));
        let arguments = serde_json::json!({
            "limit": 30, "offset": 10, "path": "/project/first.rs",
            "command": long_script,
        });
        let summary = summarize_arguments(
            "write",
            arguments.as_object().unwrap(),
            ["limit", "command", "offset", "path"],
            None,
        );
        assert!(
            summary.starts_with("p=\"/project/first.rs\" c=\"printf"),
            "{summary}"
        );
        let command = summary
            .split(" c=")
            .nth(1)
            .unwrap()
            .split(" limit=")
            .next()
            .unwrap()
            .split(" offset=")
            .next()
            .unwrap();
        assert_eq!(command.chars().count(), LOG_ARGUMENT_CHARS);
        assert!(command.ends_with("…\""), "{command}");
        assert!(!summary.contains(&"x".repeat(1024)));
        assert!(summary.contains("\\n"));
        assert!(!summary.contains('\n'));
        assert!(!summary.contains('\u{1b}'));
        assert!(!summary.contains("command_bytes="));
        assert!(summary.contains("limit=30") && summary.contains("offset=10"));

        let shell =
            serde_json::json!({"command":"echo hello", "dir":"/project", "timeout_secs":30});
        let logged = summarize_arguments(
            "bash",
            shell.as_object().unwrap(),
            ["command", "timeout_secs", "dir"],
            None,
        );
        assert!(
            logged.starts_with("d=\"/project\" c=\"echo hello\" max_t=30"),
            "{logged}"
        );
    }

    #[test]
    fn quoted_log_arguments_have_a_hard_visible_ceiling_and_keep_escapes_whole() {
        let repeated = "\\\\\"\n\u{1b}".repeat(90);
        let samples = [
            "simple text",
            "it's quoted \" hello \" \\ done",
            "\n\r\t\0\u{1b}\u{200b}\u{2028}é🙂",
            repeated.as_str(),
        ];
        for value in samples {
            if format!("{value:?}").chars().count() <= LOG_ARGUMENT_CHARS {
                assert_eq!(
                    quoted_log_argument(value, LOG_ARGUMENT_CHARS),
                    format!("{value:?}")
                );
            }
            let encoded = quoted_log_argument(value, LOG_ARGUMENT_CHARS);
            assert!(encoded.chars().count() <= LOG_ARGUMENT_CHARS, "{encoded}");
            assert!(
                encoded.starts_with('"') && encoded.ends_with('"'),
                "{encoded}"
            );
            assert!(
                !encoded.contains('\n') && !encoded.contains('\r') && !encoded.contains('\u{1b}')
            );
        }
        let exactly = "x".repeat(LOG_ARGUMENT_CHARS - 2);
        assert_eq!(
            quoted_log_argument(&exactly, LOG_ARGUMENT_CHARS),
            format!("\"{exactly}\"")
        );
        let longer = "x".repeat(LOG_ARGUMENT_CHARS + 1024);
        let encoded = quoted_log_argument(&longer, LOG_ARGUMENT_CHARS);
        assert_eq!(encoded.chars().count(), LOG_ARGUMENT_CHARS);
        assert!(encoded.ends_with("…\""));
        assert!(!encoded.contains(&"x".repeat(LOG_ARGUMENT_CHARS)));

        let escaped = quoted_log_argument(&"\n".repeat(1024), LOG_ARGUMENT_CHARS);
        assert!(escaped.chars().count() <= LOG_ARGUMENT_CHARS);
        assert!(escaped.ends_with("\\n…\""), "{escaped}");
        assert_eq!(escaped.matches("\\n").count(), (LOG_ARGUMENT_CHARS - 4) / 2);
        let emoji = quoted_log_argument(&"🙂".repeat(500), LOG_ARGUMENT_CHARS);
        assert_eq!(emoji.chars().count(), LOG_ARGUMENT_CHARS);
        assert!(emoji.ends_with("🙂…\""));
    }

    #[test]
    fn quoted_arguments_preserve_rust_escaping_under_random_unicode() {
        let alphabet = [
            'a', ' ', '\n', '\r', '\t', '\0', '\u{1b}', '\u{202e}', '\u{200b}', '\u{0301}',
            '\u{2028}', '\u{2066}', '\u{2069}', '\'', '"', '\\', '🙂', 'é', '漢', 'λ', '=', ';',
            '#', '\u{7f}', '\u{85}',
        ];
        let mut state = 1337_u32;
        for n in 0..900_usize {
            let size = (n * 31) % 400;
            let input: String = (0..size)
                .map(|_| {
                    state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                    alphabet[(state as usize) % alphabet.len()]
                })
                .collect();
            let encoded = quoted_log_argument(&input, LOG_ARGUMENT_CHARS);
            let complete = format!("{input:?}");
            if complete.chars().count() <= LOG_ARGUMENT_CHARS {
                assert_eq!(encoded, complete, "sample={n}");
            } else {
                assert!(encoded.ends_with("…\""), "sample={n} {encoded}");
                assert!(encoded.chars().count() <= LOG_ARGUMENT_CHARS);
            }
            assert!(!encoded.contains(['\n', '\r', '\u{1b}', '\u{202e}']));
        }
    }

    #[test]
    fn tool_log_caps_commands_but_keeps_complete_paths() {
        let path = "é".repeat(1024);
        let dir = "a\nb".repeat(500);
        let args = serde_json::json!({
            "path": path,
            "dir": dir,
            "command": "printf hi; ".repeat(150),
            "timeout_secs": 30,
        });
        let summary = summarize_arguments(
            "bash",
            args.as_object().unwrap(),
            ["path", "dir", "command", "timeout_secs"],
            None,
        );
        let actual_path = summary
            .split("p=")
            .nth(1)
            .unwrap()
            .split(" d=")
            .next()
            .unwrap();
        let actual_dir = summary
            .split(" d=")
            .nth(1)
            .unwrap()
            .split(" c=")
            .next()
            .unwrap();
        let actual_command = summary
            .split(" c=")
            .nth(1)
            .unwrap()
            .split(" max_t=")
            .next()
            .unwrap();
        assert_eq!(actual_path, format!("{:?}", path));
        assert_eq!(actual_dir, format!("{:?}", dir));
        assert!(actual_path.chars().count() > LOG_ARGUMENT_CHARS);
        assert!(actual_dir.chars().count() > LOG_ARGUMENT_CHARS);
        assert!(!actual_path.contains('…') && !actual_dir.contains('…'));
        assert!(actual_command.chars().count() <= LOG_ARGUMENT_CHARS);
        assert!(actual_command.ends_with("…\""), "{actual_command}");
        assert!(!summary.contains('\n') && summary.contains(" max_t=30"));

        // All 64 valid distinct file names are logged in full, not just a
        // preview subset, even when each path is long.
        let paths = (0..crate::mcp::MAX_EDIT_FILES)
            .map(|i| format!("src/{i}/{}", "x".repeat(130)))
            .collect::<Vec<_>>();
        let edits = serde_json::json!({"edits": paths.iter()
            .map(|path| serde_json::json!({"path":path}))
            .collect::<Vec<_>>()});
        let summary = summarize_arguments("edit", edits.as_object().unwrap(), ["edits"], None);
        let preview = summary.split(" edits=").next().unwrap();
        assert!(preview.starts_with("paths=[") && preview.ends_with(']'));
        for path in &paths {
            assert!(preview.contains(&format!("{path:?}")), "missing {path}");
        }
        assert!(!preview.contains("…") && !preview.contains("files_above_limit"));
        assert!(summary.ends_with(&format!("edits={}", paths.len())));
    }

    #[test]
    fn interactive_command_toggle_requires_terminal_controls_and_resets() {
        let log = LogConfig::new(1, false, ColorMode::Never);
        let command = format!("echo start\n{}", "x".repeat(2000));
        let args = serde_json::json!({"command":command});
        let summarize =
            || log.summarize_arguments("bash", args.as_object().unwrap(), ["command"], None);
        assert_eq!(log.toggle_full_commands(), None);
        assert!(summarize().ends_with("…\""));

        log.set_interactive(true);
        assert_eq!(log.toggle_full_commands(), Some(true));
        let full = summarize();
        assert_eq!(full, format!("c={:?}", command));
        assert!(full.contains(&"x".repeat(2000)));
        assert!(!full.contains('\n'));
        assert_eq!(log.toggle_full_commands(), Some(false));
        assert!(summarize().ends_with("…\""));
        assert_eq!(log.toggle_full_commands(), Some(true));
        log.set_interactive(false);
        assert_eq!(log.toggle_full_commands(), None);
        assert!(summarize().ends_with("…\""));
        log.set_interactive(true);
        assert!(summarize().ends_with("…\""), "full command mode must reset");
        log.set_interactive(false);
    }

    #[test]
    fn full_commands_require_tool_logging_even_with_active_interactive_controls() {
        let log = LogConfig::new(0, false, ColorMode::Never);
        let args = serde_json::json!({"command": "x".repeat(900)});
        let summarize =
            || log.summarize_arguments("bash", args.as_object().unwrap(), ["command"], None);
        log.set_interactive(true);
        assert_eq!(log.toggle_full_commands(), None);
        assert!(summarize().ends_with("…\""));
        assert_eq!(log.cycle_verbosity(), "TOOL");
        assert_eq!(log.toggle_full_commands(), Some(true));
        assert!(summarize().contains(&"x".repeat(900)));
        assert_eq!(log.cycle_verbosity(), "TOOL + REQ");
        assert!(summarize().contains(&"x".repeat(900)));
        assert_eq!(log.cycle_verbosity(), "quiet");
        assert_eq!(log.toggle_full_commands(), None);
        assert!(summarize().ends_with("…\""));
        assert_eq!(log.cycle_verbosity(), "TOOL");
        assert!(summarize().ends_with("…\""));
        log.set_interactive(false);

        let req_only = LogConfig::new(2, true, ColorMode::Never);
        req_only.set_interactive(true);
        assert_eq!(req_only.verbosity_label(), "REQ");
        assert_eq!(req_only.toggle_full_commands(), None);
        req_only.set_interactive(false);
    }

    #[test]
    fn interactive_arg_colors_distinguish_keys_values_and_outcomes() {
        let log = LogConfig::new(1, false, ColorMode::Always);
        let data = serde_json::json!({
            "path":"src/p=anything\n\u{1b}[31m.rs",
            "dir":".",
            "command":"echo 'd=malicious'",
            "limit":5,
            "literal":true,
            "stdin":"PRIVATE_CONTENT",
        });
        let keys = ["path", "dir", "command", "limit", "literal", "stdin"];
        let plain = log.summarize_arguments("bash", data.as_object().unwrap(), keys, None);
        assert!(!plain.contains('\u{1b}'));
        log.set_interactive(true);
        let colored = log.summarize_arguments("bash", data.as_object().unwrap(), keys, None);
        assert!(colored.contains("\x1b[1;36mp\x1b[2m=\x1b[96m"));
        assert!(colored.contains("\x1b[1;34md\x1b[2m=\x1b[94m"));
        assert!(colored.contains("\x1b[1;33mc\x1b[2m=\x1b[93m"));
        assert!(colored.contains("\x1b[1;35mlimit\x1b[2m=\x1b[95m"));
        assert!(colored.contains("\x1b[1;36mliteral\x1b[2m=\x1b[92mtrue"));
        assert!(!colored.contains("PRIVATE_CONTENT"));
        assert!(!colored.contains('\n'));
        assert!(colored.contains("\\n\\u{1b}"));
        assert_eq!(strip_ansi_for_test(&colored), plain);
        assert_eq!(
            format_log_outcome(LogOutcome::error("error"), 4, false),
            "← error  4ms"
        );
        assert!(format_log_outcome(LogOutcome::success("ok"), 4, true).contains("\x1b[1;92mok"));
        assert!(
            format_log_outcome(LogOutcome::error("error"), 4, true).contains("\x1b[1;91merror")
        );
        assert!(outcome_label(LogOutcome::http(204), 4, true).contains("\x1b[1;92m204"));
        assert!(outcome_label(LogOutcome::http(302), 4, true).contains("\x1b[1;93m302"));
        assert!(outcome_label(LogOutcome::http(404), 4, true).contains("\x1b[1;91m404"));
        assert!(outcome_label(LogOutcome::http(503), 4, true).contains("\x1b[1;91m503"));
        log.set_interactive(false);
        assert_eq!(
            log.summarize_arguments("bash", data.as_object().unwrap(), keys, None),
            plain
        );
    }

    fn strip_ansi_for_test(text: &str) -> String {
        let mut output = String::new();
        let mut chars = text.chars().peekable();
        while let Some(ch) = chars.next() {
            if ch == '\u{1b}' && chars.next_if_eq(&'[').is_some() {
                for code in chars.by_ref() {
                    if code == 'm' {
                        break;
                    }
                }
            } else {
                output.push(ch);
            }
        }
        output
    }

    #[test]
    fn log_paths_are_cwd_relative_only_when_inside_after_parent_folding() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let cwd = root.join("dotlink");
        let inside = cwd.join("src/main.rs");
        let outside = root.join("another/file");
        std::fs::create_dir_all(inside.parent().unwrap()).unwrap();
        std::fs::create_dir_all(outside.parent().unwrap()).unwrap();
        std::fs::write(&inside, "fn main() {}\n").unwrap();
        std::fs::write(&outside, "outside\n").unwrap();

        let outside_display = std::fs::canonicalize(&outside)
            .unwrap()
            .display()
            .to_string()
            .replace('\\', "/");
        let folded = cwd.join("../another/file");
        for (input, expected) in [
            (".".to_owned(), "./".to_owned()),
            ("src/main.rs".to_owned(), "./src/main.rs".to_owned()),
            (
                inside.to_string_lossy().into_owned(),
                "./src/main.rs".to_owned(),
            ),
            (folded.to_string_lossy().into_owned(), outside_display),
        ] {
            assert_eq!(display_path(&input, Some(&cwd)), expected, "input={input}");
        }

        let args = serde_json::json!({
            "path":inside,
            "dir":cwd,
            "command":"echo one\necho two",
            "stdin":"\u{2603}",
            "timeout_secs":60,
            "max_output_bytes":30000,
            "offset":5
        });
        let summary = summarize_arguments(
            "bash",
            args.as_object().unwrap(),
            [
                "offset",
                "max_output_bytes",
                "command",
                "dir",
                "timeout_secs",
                "stdin",
                "path",
            ],
            Some(&cwd),
        );
        assert!(
            summary.starts_with("p=\"./src/main.rs\" d=\"./\" c=\"echo one\\necho two\""),
            "{summary}"
        );
        assert!(
            summary.contains("max_b=30000")
                && summary.contains("max_t=60")
                && summary.contains("b=\"3\""),
            "{summary}"
        );
        assert!(!summary.contains("command_bytes="));
    }

    #[cfg(unix)]
    #[test]
    fn log_resolves_symlinked_outside_targets_even_for_missing_leaf() {
        use std::os::unix::fs::symlink;
        let temp = tempfile::tempdir().unwrap();
        let cwd = temp.path().join("project");
        let outside = temp.path().join("outside");
        std::fs::create_dir(&cwd).unwrap();
        std::fs::create_dir(&outside).unwrap();
        let outside = std::fs::canonicalize(&outside).unwrap();
        std::fs::write(outside.join("secret.txt"), b"sensitive").unwrap();
        symlink(&outside, cwd.join("alias")).unwrap();
        for request in ["alias/secret.txt", "alias/new-file.txt"] {
            let target = outside.join(request.rsplit('/').next().unwrap());
            assert_eq!(display_path(request, Some(&cwd)), target.to_string_lossy());
        }
        assert_eq!(display_path("new-file.txt", Some(&cwd)), "./new-file.txt");
    }

    #[test]
    fn multi_file_edit_paths_come_before_operation_counts_without_payloads() {
        let args = serde_json::json!({
            "edits": [
                {"path":"src/a.rs", "old_text":"SENSITIVE_OLD", "new_text":"SENSITIVE_NEW"},
                {"path":"src/b.rs", "old_text":"x", "new_text":"y"},
                {"path":"src/a.rs", "old_text":"z", "new_text":"w"},
            ]
        });
        let text = summarize_arguments("edit", args.as_object().unwrap(), ["edits", "path"], None);
        assert!(
            text.starts_with("paths=[\"src/a.rs\", \"src/b.rs\"] edits=3"),
            "{text}"
        );
        assert!(!text.contains("SENSITIVE"));
    }

    #[test]
    fn invalid_bulk_edit_summary_stays_bounded_before_tool_preflight() {
        use crate::mcp::{MAX_EDIT_FILES, MAX_EDIT_OPS};
        let edits = (0..(MAX_EDIT_OPS * 48))
            .map(|i| serde_json::json!({"path":format!("file-{i}.txt"), "old_text":"secret", "new_text":"private"}))
            .collect::<Vec<_>>();
        let args = serde_json::json!({"edits":edits});
        let summary = summarize_arguments("edit", args.as_object().unwrap(), ["edits"], None);
        assert!(
            summary.starts_with("paths=[\"file-0.txt\", \"file-1.txt\""),
            "{summary}"
        );
        assert!(
            summary.contains(&format!("files_above_limit={MAX_EDIT_FILES}")),
            "{summary}"
        );
        assert!(
            summary.contains(&format!("unscanned_edits={}", MAX_EDIT_OPS * 47)),
            "{summary}"
        );
        assert!(!summary.contains("file-128.txt"), "{summary}");
        assert!(!summary.contains("secret"));
        assert!(
            summary.len() < 2000,
            "unexpected unbounded number of edit paths"
        );
    }

    #[test]
    fn edit_uses_only_paths_even_with_legacy_top_level_fallback() {
        let args = serde_json::json!({
            "path": "fallback.txt",
            "edits": [
                {"old_text":"one", "new_text":"two"},
                {"path":"another.txt", "old_text":"alpha", "new_text":"beta"},
                {"path":"fallback.txt", "old_text":"two", "new_text":"three"}
            ]
        });
        let summary =
            summarize_arguments("edit", args.as_object().unwrap(), ["edits", "path"], None);
        assert!(
            summary.starts_with("paths=[\"fallback.txt\", \"another.txt\"] edits=3"),
            "{summary}"
        );
        assert!(!summary.contains(" path="), "{summary}");
        assert!(!summary.contains("old_text"), "{summary}");
    }

    #[test]
    fn generic_log_scopes_are_single_line_strict_and_bounded() {
        assert_eq!(sanitize_log_scope("grep"), "grep");
        let malicious = format!("tool\n\r\u{1b}[31m/{}", "x".repeat(200));
        let sanitized = sanitize_log_scope(&malicious);
        assert!(sanitized.chars().count() <= LOG_SCOPE_CHARS);
        assert!(sanitized.ends_with('…'));
        assert!(!sanitized.contains('\n'));
        assert!(!sanitized.contains('\r'));
        assert!(!sanitized.contains('\u{1b}'));
        assert!(sanitized.chars().all(|character| {
            character.is_ascii_alphanumeric()
                || matches!(character, '/' | '-' | '_' | '.' | '�' | '…')
        }));
    }

    #[test]
    fn truncate_marks_long_values() {
        assert_eq!(truncate("abc", 10), "abc");
        assert_eq!(truncate("abcdef", 3), "abc…");
    }

    #[test]
    fn color_mode_is_explicit_when_requested() {
        assert!(LogConfig::new(0, false, ColorMode::Always).color_enabled());
        assert!(!LogConfig::new(0, false, ColorMode::Never).color_enabled());
    }

    #[test]
    fn verbosity_levels_map_to_tool_and_request_logging() {
        let quiet = LogConfig::new(0, false, ColorMode::Never);
        assert!(!quiet.activity_enabled());
        assert!(!quiet.developer_enabled());

        let tools = LogConfig::new(1, false, ColorMode::Never);
        assert!(tools.activity_enabled());
        assert!(!tools.developer_enabled());

        let developer = LogConfig::new(2, false, ColorMode::Never);
        assert!(developer.activity_enabled());
        assert!(developer.developer_enabled());

        let quiet_developer = LogConfig::new(2, true, ColorMode::Never);
        assert!(!quiet_developer.activity_enabled());
        assert!(quiet_developer.developer_enabled());
        assert_eq!(quiet_developer.verbosity_label(), "REQ");
    }

    #[test]
    fn runtime_verbosity_cycles_and_clears_quiet_override() {
        let log = LogConfig::new(0, true, ColorMode::Never);
        assert_eq!(log.verbosity_label(), "quiet");
        assert_eq!(log.cycle_verbosity(), "TOOL");
        assert_eq!(log.cycle_verbosity(), "TOOL + REQ");
        assert_eq!(log.cycle_verbosity(), "quiet");
    }
}
