use std::{
    io::{self, IsTerminal},
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

#[derive(Debug)]
struct LogState {
    verbosity: AtomicU8,
    quiet_activity: AtomicBool,
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
                Ok(_) => return self.verbosity_label(),
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
        let message = if detail.is_empty() {
            "→".to_owned()
        } else {
            format!("→ {detail}")
        };
        self.line("TOOL", tool, &message, "36");
        Some(Instant::now())
    }

    pub fn activity_done(&self, tool: &str, started: Option<Instant>, outcome: impl AsRef<str>) {
        let Some(started) = started else {
            return;
        };
        self.line(
            "TOOL",
            tool,
            &format!(
                "← {}  {}ms",
                outcome.as_ref(),
                started.elapsed().as_millis()
            ),
            "36",
        );
    }

    pub fn request(&self, transport: &str, method: &str) -> Option<Instant> {
        if !self.developer_enabled() {
            return None;
        }
        self.line("REQ", transport, &format!("→ {method}"), "35");
        Some(Instant::now())
    }

    pub fn request_done(
        &self,
        transport: &str,
        method: &str,
        started: Option<Instant>,
        outcome: impl AsRef<str>,
    ) {
        let Some(started) = started else {
            return;
        };
        let elapsed = format!("  {}ms", started.elapsed().as_millis());
        self.line(
            "REQ",
            transport,
            &format!("← {method}  {}{elapsed}", outcome.as_ref()),
            "35",
        );
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

/// Widest scalar argument rendered literally in a tool-call summary.
const SUMMARY_VALUE_CHARS: usize = 120;

/// Arguments that carry whole payloads. Their values are always logged as a byte
/// count, never as text: file content, patches, stdin and search patterns must
/// not reach a journal. Shell commands can include inline credentials and are
/// payloads, too. This is a semantic list (which arguments are payloads) that no
/// schema can express, and it is shared by every caller.
const PAYLOAD_ARGUMENTS: &[&str] = &[
    "content", "stdin", "data", "old_text", "new_text", "patch", "command", "pattern",
];

/// One-line rendering of tool-call arguments for the TOOL and REQ logs.
///
/// `keys` is the tool's own argument order, taken from its JSON schema, so a new
/// argument cannot silently go unlogged. Strings wider than a preview become a
/// byte count, which keeps content, commands, patches, patterns and stdin out of logs.
pub fn summarize_arguments<'a>(
    arguments: &Map<String, Value>,
    keys: impl IntoIterator<Item = &'a str>,
) -> String {
    let mut parts = Vec::new();
    for key in keys {
        let Some(value) = arguments.get(key) else {
            continue;
        };
        match value {
            Value::String(value) if PAYLOAD_ARGUMENTS.contains(&key) => {
                parts.push(format!("{key}_bytes={}", value.len()));
            }
            Value::String(value) if value.chars().count() > SUMMARY_VALUE_CHARS => {
                parts.push(format!("{key}_bytes={}", value.len()));
            }
            Value::String(value) => {
                let compact = value.split_whitespace().collect::<Vec<_>>().join(" ");
                parts.push(format!(
                    "{key}={:?}",
                    truncate(&compact, SUMMARY_VALUE_CHARS)
                ));
            }
            Value::Bool(value) => parts.push(format!("{key}={value}")),
            Value::Number(value) => parts.push(format!("{key}={value}")),
            Value::Array(items) => parts.push(format!("{key}={}", items.len())),
            Value::Null | Value::Object(_) => {}
        }
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
            arguments.as_object().unwrap(),
            [
                "path", "command", "content", "old_text", "pattern", "edits", "literal", "limit",
                "absent", "missing",
            ],
        );
        assert!(summary.contains("path=\"src/main.rs\""), "{summary}");
        assert!(summary.contains("command_bytes="), "{summary}");
        assert!(!summary.contains("FAKE_TEST_SECRET"), "{summary}");
        assert!(summary.contains("content_bytes=200"), "{summary}");
        // A payload stays a byte count even when it is short.
        assert!(summary.contains("old_text_bytes=12"), "{summary}");
        assert!(!summary.contains("short secret"), "{summary}");
        assert!(summary.contains("pattern_bytes=16"), "{summary}");
        assert!(!summary.contains("FAKE_GREP_SECRET"), "{summary}");
        assert!(summary.contains("edits=2"), "{summary}");
        assert!(summary.contains("literal=true"), "{summary}");
        assert!(summary.contains("limit=20"), "{summary}");
        assert!(!summary.contains("xxxx"), "{summary}");
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
