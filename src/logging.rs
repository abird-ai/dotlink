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

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, ValueEnum)]
pub enum ColorMode {
    #[default]
    Auto,
    Always,
    Never,
}

#[derive(Debug)]
struct LogState {
    verbosity: AtomicU8,
    silent_activity: AtomicBool,
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
    pub fn new(verbose: u8, silent_activity: bool, color_mode: ColorMode) -> Self {
        let color = match color_mode {
            ColorMode::Auto => io::stderr().is_terminal(),
            ColorMode::Always => true,
            ColorMode::Never => false,
        };
        Self {
            state: Arc::new(LogState {
                verbosity: AtomicU8::new(verbose.min(2)),
                silent_activity: AtomicBool::new(silent_activity),
            }),
            color,
        }
    }

    pub fn color_enabled(&self) -> bool {
        self.color
    }

    pub fn activity_enabled(&self) -> bool {
        self.state.verbosity.load(Ordering::Relaxed) >= 1
            && !self.state.silent_activity.load(Ordering::Relaxed)
    }

    pub fn developer_enabled(&self) -> bool {
        self.state.verbosity.load(Ordering::Relaxed) >= 2
    }

    pub fn verbosity_label(&self) -> &'static str {
        let verbosity = self.state.verbosity.load(Ordering::Relaxed);
        let silent = self.state.silent_activity.load(Ordering::Relaxed);
        match (verbosity, silent) {
            (0, _) => "quiet",
            (1, true) => "quiet",
            (1, false) => "TOOL",
            (_, true) => "REQ",
            (_, false) => "TOOL + REQ",
        }
    }

    pub fn cycle_verbosity(&self) -> &'static str {
        self.state.silent_activity.store(false, Ordering::Relaxed);
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
        if !self.developer_enabled() {
            return;
        }
        let elapsed = started
            .map(|started| format!("  {}ms", started.elapsed().as_millis()))
            .unwrap_or_default();
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
        if self.color {
            eprintln!(
                "\x1b[2m[{timestamp}]\x1b[0m \x1b[{color_code}m{level:<4}\x1b[0m \x1b[1m{scope:<10}\x1b[0m {message}"
            );
        } else {
            eprintln!("[{timestamp}] {level:<4} {scope:<10} {message}");
        }
    }
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

        let silent_developer = LogConfig::new(2, true, ColorMode::Never);
        assert!(!silent_developer.activity_enabled());
        assert!(silent_developer.developer_enabled());
        assert_eq!(silent_developer.verbosity_label(), "REQ");
    }

    #[test]
    fn runtime_verbosity_cycles_and_clears_silent_override() {
        let log = LogConfig::new(0, true, ColorMode::Never);
        assert_eq!(log.verbosity_label(), "quiet");
        assert_eq!(log.cycle_verbosity(), "TOOL");
        assert_eq!(log.cycle_verbosity(), "TOOL + REQ");
        assert_eq!(log.cycle_verbosity(), "quiet");
    }
}
