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

/// Payloads other than executable commands remain byte counts. TOOL logging
/// is deliberately opt-in (`-v` / `logging.verbosity = "tools"`): users who
/// enable it explicitly want the complete shell command recorded in journald.
/// Never truncate the shell command or silently substitute its byte length.
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
    let cwd = crate::mcp::lexical_normalize(cwd);
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

/// Enumerate only work that `edit` can accept. The tool will reject a call
/// with more than MAX_EDIT_OPS operations or MAX_EDIT_FILES distinct files, so
/// logging must not perform unbounded quadratic work before that rejection.
/// Preserve encounter order and a compact, constant-width journal preview.
fn edit_paths(arguments: &Map<String, Value>, cwd: Option<&Path>) -> Option<String> {
    use crate::mcp::{MAX_EDIT_FILES, MAX_EDIT_OPS};
    const PREVIEW_PATHS: usize = 16;

    let edits = arguments.get("edits")?.as_array()?;
    let fallback = arguments.get("path").and_then(Value::as_str);
    let mut seen = HashSet::with_capacity(MAX_EDIT_FILES);
    let mut shown = Vec::with_capacity(PREVIEW_PATHS);
    let mut exceeded_files = false;
    for edit in edits.iter().take(MAX_EDIT_OPS) {
        let Some(path) = edit.get("path").and_then(Value::as_str).or(fallback) else {
            continue;
        };
        if seen.contains(path) {
            continue;
        }
        if seen.len() == MAX_EDIT_FILES {
            exceeded_files = true;
            break;
        }
        seen.insert(path);
        if shown.len() < PREVIEW_PATHS {
            shown.push(path);
        }
    }
    if seen.is_empty() && !exceeded_files {
        return None;
    }
    let displayed = shown
        .iter()
        .map(|path| format!("{:?}", display_path(path, cwd)))
        .collect::<Vec<_>>()
        .join(", ");
    let omitted = seen.len().saturating_sub(shown.len());
    let suffix = if omitted > 0 {
        format!(", … +{omitted} files")
    } else {
        String::new()
    };
    let unscanned = edits.len().saturating_sub(MAX_EDIT_OPS);
    let overflow = if exceeded_files {
        format!(" files_above_limit={MAX_EDIT_FILES}")
    } else {
        String::new()
    };
    let overflow = if unscanned > 0 {
        format!("{overflow} unscanned_edits={unscanned}")
    } else {
        overflow
    };
    Some(format!("paths=[{displayed}{suffix}]{overflow}"))
}

/// Stable, operator-friendly argument order. The JSON schema currently stores
/// properties in a sorted map, which otherwise puts offset/limit before path.
/// Reuse this order for local HTTP, stdio, and OpenAI diagnostic summaries.
pub fn summarize_arguments<'a>(
    tool: &str,
    arguments: &Map<String, Value>,
    keys: impl IntoIterator<Item = &'a str>,
    cwd: Option<&Path>,
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
        parts.push(paths);
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
            // Commands are deliberately complete, one-line escaped, never
            // truncated or replaced with their byte count.
            Value::String(value) if key == "command" => parts.push(format!("c={value:?}")),
            Value::String(value) if key == "path" => {
                parts.push(format!("p={:?}", display_path(value, cwd)));
            }
            Value::String(value) if key == "dir" => {
                parts.push(format!("d={:?}", display_path(value, cwd)));
            }
            Value::String(value) if PAYLOAD_ARGUMENTS.contains(&key) => {
                payload_bytes = payload_bytes.saturating_add(value.len());
            }
            Value::String(value) if value.chars().count() > SUMMARY_VALUE_CHARS => {
                payload_bytes = payload_bytes.saturating_add(value.len());
            }
            Value::String(value) => {
                parts.push(format!(
                    "{key}={:?}",
                    truncate(
                        &value.split_whitespace().collect::<Vec<_>>().join(" "),
                        SUMMARY_VALUE_CHARS
                    )
                ));
            }
            Value::Bool(value) => parts.push(format!("{key}={value}")),
            Value::Number(value) => {
                let short = match key {
                    "timeout_secs" => "max_t",
                    "max_output_bytes" | "max_read_bytes" | "max_write_bytes" => "max_b",
                    _ => key,
                };
                parts.push(format!("{short}={value}"));
            }
            Value::Array(items) => parts.push(format!("{key}={}", items.len())),
            Value::Null | Value::Object(_) => {}
        }
    }
    if payload_bytes > 0 {
        parts.push(format!("b={:?}", payload_bytes.to_string()));
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
    fn tool_log_prefers_path_and_keeps_full_escaped_shell_command() {
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
        assert!(summary.contains(&"x".repeat(1024)));
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
    fn log_paths_are_cwd_relative_only_when_inside_after_parent_folding() {
        let cwd = Path::new("/home/pvl/src/dotlink");
        for (input, expected) in [
            (".", "./"),
            ("src/main.rs", "./src/main.rs"),
            ("/home/pvl/src/dotlink/src/main.rs", "./src/main.rs"),
            (
                "/home/pvl/src/dotlink/../another/file",
                "/home/pvl/src/another/file",
            ),
            ("../../secrets", "/home/pvl/secrets"),
        ] {
            assert_eq!(display_path(input, Some(cwd)), expected, "input={input}");
        }
        let expected = std::fs::canonicalize("/etc/hosts").unwrap();
        assert_eq!(
            display_path("/etc/hosts", Some(cwd)),
            expected.to_string_lossy()
        );
        let args = serde_json::json!({
            "path":"/home/pvl/src/dotlink/src/main.rs",
            "dir":"/home/pvl/src/dotlink",
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
            Some(cwd),
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
        assert!(summary.len() < 1000, "{summary}");
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
