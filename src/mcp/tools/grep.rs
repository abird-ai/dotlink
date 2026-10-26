//! `grep`: descriptor-rooted search with one glob grammar and bounded output.

use std::{
    collections::BinaryHeap,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};
use rmcp::{
    handler::server::wrapper::Parameters, model::CallToolResult, schemars, tool, tool_router,
};
use serde::Deserialize;

use super::{
    GlobFilter, LocalMachine, WalkFilter, dot, marker_bytes, marker_files, marker_hits, ok_text,
    retain_best, tool_error, trim_to_ceiling,
};
use crate::mcp::AccessNeed;

const MAX_GREP_HITS: usize = 200;
const MAX_GREP_FILE_BYTES: usize = 1_048_576;
const MAX_GREP_PATTERN: usize = 1024;
const MAX_GREP_PER_FILE: usize = 50;
const MAX_GREP_CONTEXT: usize = 8;
const MAX_GREP_FILES: usize = 65_536;
/// Rendered-size estimate for one JSON array element (braces, keys, quotes).
const JSON_ENTRY_OVERHEAD: usize = 48;

/// Why a page stopped. Keeping the cause explicit makes the prose marker and
/// the paging contract agree instead of inferring a cause from counters.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum StopReason {
    PageFull,
    ByteBudget,
    PerFileCap,
    CandidateCap,
    ScanBudget,
}

/// Suffix that names the per-file cap as the reason a content page stopped, so a
/// reader does not mistake it for the call's own `limit`.
const PER_FILE_CAP_SUFFIX: &str = " (per-file cap)";

/// The longest marker a `content` page can end with. Bytes are reserved for it
/// before a hit is accepted, so a page always ends with whole hit lines.
fn marker_reserve() -> usize {
    marker_bytes(usize::MAX)
        .len()
        .max(marker_files(usize::MAX).len())
        .max(marker_hits(usize::MAX).len() + PER_FILE_CAP_SUFFIX.len())
        .max(format!("… no hits at offset {}", usize::MAX).len())
        .max("… discovery scan incomplete; narrow path or glob".len())
        + 1 // the newline that separates the marker from the last hit
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub(crate) enum GrepOutputMode {
    #[default]
    Content,
    Files,
    Count,
}

#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
pub(crate) struct GrepArgs {
    /// Regular expression, or literal text when `literal` is set. Maximum 1024 printable characters.
    pub(crate) pattern: String,

    /// Directory to search. Relative paths resolve from the directory where dotlink was launched.
    #[serde(default = "dot")]
    pub(super) path: String,

    /// Glob filter over entry paths and names. Same grammar as `ls`.
    #[serde(default)]
    pub(super) glob: Option<String>,

    /// Treat `pattern` as literal text instead of a regular expression.
    #[serde(default)]
    pub(super) literal: bool,

    /// Case-insensitive matching.
    #[serde(default)]
    pub(super) ignore_case: bool,

    /// `content` returns matching lines, `files` returns paths, `count` returns per-file counts.
    #[serde(default)]
    pub(super) output_mode: GrepOutputMode,

    /// Context lines before each hit. Maximum 8.
    #[serde(default)]
    pub(super) before: Option<usize>,

    /// Context lines after each hit. Maximum 8.
    #[serde(default)]
    pub(super) after: Option<usize>,

    /// Prefix content lines with `path:line:`. Defaults to true.
    #[serde(default)]
    pub(super) numbered: Option<bool>,

    /// Hits, or files, to skip before `limit`.
    #[serde(default)]
    pub(super) offset: Option<usize>,

    /// Maximum hits, or files. Defaults to 200 and is capped at 200.
    #[serde(default)]
    pub(super) limit: Option<usize>,

    /// Honour `.gitignore` files below `path`. Off by default.
    #[serde(default)]
    pub(super) gitignore: bool,

    /// Skip entries whose name starts with `.`. Off by default.
    #[serde(default)]
    pub(super) skip_hidden: bool,
}

/// Output budget for one call: stopping at the ceiling bounds what is held in
/// memory, not just what is returned.
#[derive(Default)]
struct OutputBudget {
    limit: usize,
    used: usize,
}

impl OutputBudget {
    fn new(limit: usize) -> Self {
        Self { limit, used: 0 }
    }

    /// Bytes still available in this call's output budget.
    fn remaining(&self) -> usize {
        self.limit.saturating_sub(self.used)
    }

    /// Reserve `bytes` if the payload still fits.
    fn take(&mut self, bytes: usize) -> bool {
        if self.used + bytes > self.limit {
            return false;
        }
        self.used += bytes;
        true
    }
}

impl LocalMachine {
    /// Read one search candidate through the descriptor-rooted reader, so the
    /// file is opened beneath the pinned root with symlinks and nested mounts
    /// refused, one file at a time (the walk itself descends through pinned
    /// directory handles).
    async fn read_search_file(&self, path: &Path) -> Option<String> {
        let mut file = self.open_read_file(path).await.ok()?;
        let bytes = Self::read_bounded(&mut file, MAX_GREP_FILE_BYTES)
            .await
            .ok()?;
        String::from_utf8(bytes).ok()
    }

    async fn grep_impl(&self, args: GrepArgs) -> Result<String> {
        if args.pattern.is_empty() {
            anyhow::bail!("pattern must not be empty");
        }
        if args.pattern.chars().count() > MAX_GREP_PATTERN {
            anyhow::bail!("pattern is longer than {MAX_GREP_PATTERN} characters");
        }
        if args.pattern.chars().any(char::is_control) {
            anyhow::bail!("pattern must be printable text");
        }

        let source = match args.literal {
            true => regex::escape(&args.pattern),
            false => args.pattern.clone(),
        };
        let regex = regex::RegexBuilder::new(&source)
            .case_insensitive(args.ignore_case)
            .size_limit(MAX_GREP_FILE_BYTES)
            .dfa_size_limit(MAX_GREP_FILE_BYTES)
            .build()
            .with_context(|| format!("invalid regex: {}", args.pattern))?;

        let root = self.resolve_existing(&args.path, AccessNeed::Read).await?;
        if !tokio::fs::metadata(&root).await?.is_dir() {
            anyhow::bail!(
                "not a directory: {} (use read for a single file)",
                args.path
            );
        }

        let glob = GlobFilter::optional(args.glob.as_deref())?;
        let before = args.before.unwrap_or(0).min(MAX_GREP_CONTEXT);
        let after = args.after.unwrap_or(0).min(MAX_GREP_CONTEXT);
        let limit = args.limit.unwrap_or(MAX_GREP_HITS).clamp(1, MAX_GREP_HITS);
        let offset = args.offset.unwrap_or(0);
        let numbered = args.numbered.unwrap_or(true);
        let ceiling = self.config.max_output_bytes;

        // A bounded heap retains the lexically first candidate paths over the
        // complete walk, not an arbitrary enumeration-order prefix. Only paths
        // are retained; files are opened individually after the walk.
        let mut candidates = BinaryHeap::<PathBuf>::new();
        let mut candidates_capped = false;
        let progress = self
            .walk_entries_bounded(
                &root,
                usize::MAX,
                WalkFilter {
                    skip_hidden: args.skip_hidden,
                    gitignore: args.gitignore,
                },
                super::MAX_WALK_VISITS,
                |entry| {
                    if entry.kind != "file" || entry.size > MAX_GREP_FILE_BYTES as u64 {
                        return true;
                    }
                    if glob
                        .as_ref()
                        .is_some_and(|glob| !glob.matches_entry(&entry.relative))
                    {
                        return true;
                    }
                    candidates_capped |= retain_best(&mut candidates, entry.path, MAX_GREP_FILES);
                    true
                },
            )
            .await?;
        let candidates = candidates.into_sorted_vec();

        let mut paging = Paging {
            offset,
            limit,
            emitted: 0,
            skipped: 0,
        };
        let mut files: Vec<String> = Vec::new();
        let mut counts: Vec<(String, usize)> = Vec::new();
        let mut content = String::new();
        let reserve = match args.output_mode {
            GrepOutputMode::Content => marker_reserve(),
            GrepOutputMode::Files | GrepOutputMode::Count => 0,
        };
        let mut budget = OutputBudget::new(ceiling.saturating_sub(reserve));
        // Stop scanning at the first refusal in every mode. A later file must
        // not be emitted after a skipped one, or paging would repeat or lose
        // records depending on where the caller resumes.
        let mut stop: Option<StopReason> = None;

        for path in &candidates {
            if stop.is_some() {
                break;
            }
            let Some(text) = self.read_search_file(path).await else {
                continue;
            };
            let display = self.config.access.relative_display(path);
            let lines = text.lines().collect::<Vec<_>>();

            match args.output_mode {
                GrepOutputMode::Content => {
                    let matches = lines
                        .iter()
                        .enumerate()
                        .filter(|(_, line)| regex.is_match(line))
                        .map(|(index, _)| index)
                        .collect::<Vec<_>>();
                    if matches.is_empty() {
                        continue;
                    }
                    let to_skip = paging
                        .offset
                        .saturating_sub(paging.skipped)
                        .min(matches.len());
                    paging.skipped = paging.skipped.saturating_add(to_skip);
                    if to_skip == matches.len() {
                        continue;
                    }
                    let remaining = paging.limit.saturating_sub(paging.emitted);
                    if remaining == 0 {
                        stop = Some(StopReason::PageFull);
                        break;
                    }
                    let take_max = remaining
                        .min(matches.len() - to_skip)
                        .min(MAX_GREP_PER_FILE);
                    // Select the largest contiguous prefix of hits that fits
                    // both the hit limits and the remaining byte budget. A
                    // single oversized batch must not blank the whole page.
                    let mut take = take_max;
                    let rendered = loop {
                        if take == 0 {
                            break None;
                        }
                        let candidate = render_hit_page(
                            &lines,
                            &matches,
                            to_skip,
                            take,
                            HitRender {
                                before,
                                after,
                                numbered,
                                display: &display,
                            },
                        );
                        if candidate.len() <= budget.remaining() {
                            break Some((take, candidate));
                        }
                        take -= 1;
                    };
                    let Some((take, group)) = rendered else {
                        stop = Some(StopReason::ByteBudget);
                        break;
                    };
                    let _ = budget.take(group.len());
                    content.push_str(&group);
                    paging.emit_many(take);
                    let more_in_file = to_skip + take < matches.len();
                    if take < take_max {
                        stop = Some(StopReason::ByteBudget);
                        break;
                    }
                    if paging.full() && more_in_file {
                        stop = Some(StopReason::PageFull);
                        break;
                    }
                    if take == MAX_GREP_PER_FILE && more_in_file {
                        stop = Some(StopReason::PerFileCap);
                        break;
                    }
                }
                GrepOutputMode::Files | GrepOutputMode::Count => {
                    let hits = lines.iter().filter(|line| regex.is_match(line)).count();
                    if hits == 0 {
                        continue;
                    }
                    if !paging.skip() {
                        continue;
                    }
                    if paging.full() {
                        stop = Some(StopReason::PageFull);
                        continue;
                    }
                    if !budget.take(display.len() + JSON_ENTRY_OVERHEAD) {
                        stop = Some(StopReason::ByteBudget);
                        continue;
                    }
                    paging.emit();
                    match args.output_mode {
                        GrepOutputMode::Files => files.push(display),
                        _ => counts.push((display, hits)),
                    }
                }
            }
        }
        if progress.incomplete {
            // In text mode there is no separate metadata field: incomplete
            // discovery takes precedence over a page-full marker.
            stop = Some(StopReason::ScanBudget);
        } else if stop.is_none() && candidates_capped {
            stop = Some(StopReason::CandidateCap);
        }

        match args.output_mode {
            GrepOutputMode::Content => {
                while content.ends_with('\n') {
                    content.pop();
                }
                // The budget reserved the longest marker before accepting hits,
                // so appending it here cannot cut into an accepted line.
                let marker = match stop {
                    Some(StopReason::ByteBudget) => Some(marker_bytes(ceiling)),
                    Some(StopReason::CandidateCap) => Some(marker_files(MAX_GREP_FILES)),
                    Some(StopReason::ScanBudget) => {
                        Some("… discovery scan incomplete; narrow path or glob".to_owned())
                    }
                    Some(StopReason::PerFileCap) => Some(format!(
                        "{}{PER_FILE_CAP_SUFFIX}",
                        marker_hits(paging.emitted)
                    )),
                    Some(StopReason::PageFull) => Some(marker_hits(paging.emitted)),
                    // A page that emitted nothing because the offset is past the
                    // last hit says so instead of answering with silence.
                    None if paging.emitted == 0 && offset > 0 => {
                        Some(format!("… no hits at offset {offset}"))
                    }
                    None => None,
                };
                if let Some(marker) = marker {
                    if !content.is_empty() {
                        content.push('\n');
                    }
                    content.push_str(&marker);
                }
                Ok(content)
            }
            GrepOutputMode::Files => {
                let render = |files: &[String], truncated: bool| {
                    serde_json::json!({
                        "files": files, "truncated": truncated,
                        "scan_incomplete": progress.incomplete,
                    })
                    .to_string()
                };
                let (files, truncated) = trim_to_ceiling(files, ceiling, stop.is_some(), render);
                Ok(serde_json::json!({
                    "files": files, "truncated": truncated,
                    "scan_incomplete": progress.incomplete,
                })
                .to_string())
            }
            GrepOutputMode::Count => {
                let render = |counts: &[(String, usize)], truncated: bool| {
                    let page_total = counts.iter().map(|(_, count)| count).sum::<usize>();
                    serde_json::json!({
                        "counts": counts
                            .iter()
                            .map(|(path, count)| serde_json::json!({"path": path, "count": count}))
                            .collect::<Vec<_>>(),
                        "page_total": page_total,
                        "truncated": truncated,
                        "scan_incomplete": progress.incomplete,
                    })
                    .to_string()
                };
                let (counts, truncated) = trim_to_ceiling(counts, ceiling, stop.is_some(), render);
                Ok(render(&counts, truncated))
            }
        }
    }
}

#[derive(Clone, Copy)]
struct HitRender<'a> {
    before: usize,
    after: usize,
    numbered: bool,
    display: &'a str,
}

/// Render one page of matching lines with optional context. Selected hits are
/// exactly the page's hit records; context never reaches into an unselected
/// matching line, so a later page cannot repeat a hit as context.
fn render_hit_page(
    lines: &[&str],
    matches: &[usize],
    selected_start: usize,
    selected_len: usize,
    render: HitRender<'_>,
) -> String {
    let HitRender {
        before,
        after,
        numbered,
        display,
    } = render;
    let previous_match = selected_start.checked_sub(1).map(|index| matches[index]);
    let next_match = matches.get(selected_start + selected_len).copied();
    let last_line = lines.len().saturating_sub(1);
    let mut ranges: Vec<(usize, usize)> = Vec::with_capacity(selected_len);
    for &hit in &matches[selected_start..selected_start + selected_len] {
        let start = hit
            .saturating_sub(before)
            .max(previous_match.map_or(0, |line| line + 1));
        let end = hit
            .saturating_add(after)
            .min(next_match.map_or(last_line, |line| line.saturating_sub(1)))
            .min(last_line);
        if start > end {
            continue;
        }
        match ranges.last_mut() {
            Some(last) if start <= last.1 + 1 => last.1 = last.1.max(end),
            _ => ranges.push((start, end)),
        }
    }

    let mut output = String::new();
    for (index, (start, end)) in ranges.iter().enumerate() {
        if index > 0 && (before > 0 || after > 0) {
            output.push_str("--\n");
        }
        for (number, line) in lines.iter().enumerate().take(end + 1).skip(*start) {
            match numbered {
                true => output.push_str(&format!("{display}:{}: {line}\n", number + 1)),
                false => output.push_str(&format!("{display}: {line}\n")),
            }
        }
    }
    output
}

/// Page position across one call: hits (content) or files (files/count).
#[derive(Default)]
struct Paging {
    offset: usize,
    limit: usize,
    skipped: usize,
    emitted: usize,
}

impl Paging {
    /// Consume one skip; `false` means this record is still before the page.
    fn skip(&mut self) -> bool {
        if self.skipped < self.offset {
            self.skipped += 1;
            return false;
        }
        true
    }

    fn full(&self) -> bool {
        self.emitted >= self.limit
    }

    fn emit(&mut self) {
        self.emitted += 1;
    }

    fn emit_many(&mut self, count: usize) {
        self.emitted = self.emitted.saturating_add(count);
    }
}

#[tool_router(router = "grep_router", vis = "pub(crate)")]
impl LocalMachine {
    #[tool(
        description = "Search UTF-8 files recursively. `pattern` is regex unless `literal`; `glob` shares the `ls` grammar. `output_mode`: content (matches and context), files, count (per-file counts; `page_total` is page-scoped). Paging uses `offset`/`limit` (up to 200); `gitignore`/`skip_hidden` are opt-in. A discovery scan visits up to 100000 entries, then reports partial results: JSON replies contain `scan_incomplete`, text replies an explicit marker; narrow `path`/`glob` for complete results. Hits and JSON replies obey the byte ceiling; a content page uses whole matching lines. Files over 1 MiB, non-text and symlink entries are skipped.",
        annotations(
            title = "Search files",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn grep(&self, Parameters(args): Parameters<GrepArgs>) -> CallToolResult {
        match self.grep_impl(args).await {
            Ok(text) => ok_text(text),
            Err(error) => tool_error(error),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mcp::tools::test_support::{grep_args, grep_tree, test_machine};
    use serde_json::Value;

    #[tokio::test]
    async fn grep_content_mode_renders_numbered_hits_in_path_order() {
        let (_temp, machine) = grep_tree();
        let content = machine.grep_impl(grep_args("needle")).await.unwrap();
        assert_eq!(
            content,
            ".hidden.txt:1: needle hidden\n\
             notes.md:1: needle notes\n\
             src/lib.rs:1: needle two\n\
             src/lib.rs:3: needle three\n\
             src/nested/deep.rs:1: deep needle\n\
             top.txt:2: needle one"
        );

        let mut args = grep_args("needle");
        args.numbered = Some(false);
        let content = machine.grep_impl(args).await.unwrap();
        assert_eq!(
            content,
            ".hidden.txt: needle hidden\n\
             notes.md: needle notes\n\
             src/lib.rs: needle two\n\
             src/lib.rs: needle three\n\
             src/nested/deep.rs: deep needle\n\
             top.txt: needle one"
        );
    }

    #[tokio::test]
    async fn grep_context_windows_group_and_separate() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        std::fs::write(
            root.join("a.txt"),
            "l1\nl2\nhit\nl4\nl5\nl6\nl7\nl8\nhit2\nl10\n",
        )
        .unwrap();
        let machine = test_machine(root);

        let mut args = grep_args("hit");
        args.before = Some(1);
        args.after = Some(1);
        let content = machine.grep_impl(args).await.unwrap();
        assert_eq!(
            content,
            "a.txt:2: l2\n\
             a.txt:3: hit\n\
             a.txt:4: l4\n\
             --\n\
             a.txt:8: l8\n\
             a.txt:9: hit2\n\
             a.txt:10: l10"
        );

        // Context is capped at 8 lines per side.
        let mut args = grep_args("hit");
        args.before = Some(100);
        args.after = Some(100);
        let content = machine.grep_impl(args).await.unwrap();
        assert!(content.starts_with("a.txt:1: l1\n"), "{content}");
        assert!(content.ends_with("a.txt:10: l10"), "{content}");
    }

    #[tokio::test]
    async fn grep_files_and_count_modes_page_files() {
        let (_temp, machine) = grep_tree();

        let mut args = grep_args("needle");
        args.output_mode = GrepOutputMode::Files;
        let files: Value = serde_json::from_str(&machine.grep_impl(args).await.unwrap()).unwrap();
        assert_eq!(
            files["files"],
            serde_json::json!([
                ".hidden.txt",
                "notes.md",
                "src/lib.rs",
                "src/nested/deep.rs",
                "top.txt"
            ])
        );
        assert_eq!(files["truncated"], Value::Bool(false));

        let mut args = grep_args("needle");
        args.output_mode = GrepOutputMode::Files;
        args.offset = Some(1);
        args.limit = Some(2);
        let files: Value = serde_json::from_str(&machine.grep_impl(args).await.unwrap()).unwrap();
        assert_eq!(
            files["files"],
            serde_json::json!(["notes.md", "src/lib.rs"])
        );
        assert_eq!(files["truncated"], Value::Bool(true));

        let mut args = grep_args("needle");
        args.output_mode = GrepOutputMode::Count;
        let counts: Value = serde_json::from_str(&machine.grep_impl(args).await.unwrap()).unwrap();
        assert_eq!(counts["page_total"], serde_json::json!(6));
        assert_eq!(
            counts["counts"][2],
            serde_json::json!({"path": "src/lib.rs", "count": 2})
        );

        // Hit paging in content mode skips hits, not lines.
        let mut args = grep_args("needle");
        args.offset = Some(1);
        args.limit = Some(2);
        let content = machine.grep_impl(args).await.unwrap();
        assert_eq!(
            content,
            "notes.md:1: needle notes\n\
             src/lib.rs:1: needle two\n\
             … truncated at 2 hits"
        );
    }

    #[tokio::test]
    async fn grep_pattern_options_and_glob_filter() {
        let (_temp, machine) = grep_tree();

        let mut args = grep_args("NEEDLE");
        args.ignore_case = true;
        args.output_mode = GrepOutputMode::Files;
        let files: Value = serde_json::from_str(&machine.grep_impl(args).await.unwrap()).unwrap();
        assert_eq!(files["files"].as_array().unwrap().len(), 5);

        // `literal` escapes regex metacharacters.
        let mut args = grep_args("needle .*");
        args.literal = true;
        assert!(machine.grep_impl(args).await.unwrap().is_empty());

        let mut args = grep_args("needle .*");
        args.output_mode = GrepOutputMode::Files;
        let files: Value = serde_json::from_str(&machine.grep_impl(args).await.unwrap()).unwrap();
        assert_eq!(files["files"].as_array().unwrap().len(), 4);

        let mut args = grep_args("needle");
        args.glob = Some("*.rs".to_owned());
        args.output_mode = GrepOutputMode::Files;
        let files: Value = serde_json::from_str(&machine.grep_impl(args).await.unwrap()).unwrap();
        assert_eq!(
            files["files"],
            serde_json::json!(["src/lib.rs", "src/nested/deep.rs"])
        );

        let mut args = grep_args("needle");
        args.glob = Some("src/**/*.rs".to_owned());
        args.output_mode = GrepOutputMode::Files;
        let files: Value = serde_json::from_str(&machine.grep_impl(args).await.unwrap()).unwrap();
        assert_eq!(
            files["files"],
            serde_json::json!(["src/lib.rs", "src/nested/deep.rs"])
        );
    }

    #[tokio::test]
    async fn grep_per_file_cap_and_invalid_patterns() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let many = (0..120)
            .map(|line| format!("hit {line}"))
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(root.join("many.txt"), many).unwrap();
        let machine = test_machine(root);

        // `count` reports exact per-file totals; the 50-hit cap only bounds
        // content mode.
        let mut args = grep_args("hit");
        args.output_mode = GrepOutputMode::Count;
        let counts: Value = serde_json::from_str(&machine.grep_impl(args).await.unwrap()).unwrap();
        assert_eq!(counts["counts"][0]["count"], serde_json::json!(120));
        assert_eq!(counts["page_total"], serde_json::json!(120));
        assert_eq!(counts["truncated"], Value::Bool(false));

        let args = grep_args("hit");
        let content = machine.grep_impl(args.clone()).await.unwrap();
        assert_eq!(content.matches("many.txt:").count(), 50);
        assert!(content.contains("truncated at 50 hits"), "{content}");

        // Pattern bounds and regex errors are tool failures.
        let mut args = grep_args("");
        args.literal = true;
        assert!(machine.grep_impl(args).await.is_err());
        let args = grep_args(&"a".repeat(MAX_GREP_PATTERN + 1));
        assert!(machine.grep_impl(args).await.is_err());
        let args = grep_args("[unclosed");
        assert!(machine.grep_impl(args).await.is_err());
        let args = grep_args("a\tb");
        assert!(
            machine
                .grep_impl(args)
                .await
                .unwrap_err()
                .to_string()
                .contains("printable")
        );
    }

    #[tokio::test]
    async fn grep_gitignore_and_hidden_are_opt_in() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        std::fs::create_dir_all(root.join("build/nested")).unwrap();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join(".gitignore"), "build/\n*.log\n!keep.log\n").unwrap();
        std::fs::write(root.join("build/nested/out.rs"), "needle\n").unwrap();
        std::fs::write(root.join("src/lib.rs"), "needle\n").unwrap();
        std::fs::write(root.join("debug.log"), "needle\n").unwrap();
        std::fs::write(root.join("keep.log"), "needle\n").unwrap();
        std::fs::write(root.join(".secret.rs"), "needle\n").unwrap();
        let machine = test_machine(root.clone());

        let mut args = grep_args("needle");
        args.output_mode = GrepOutputMode::Files;
        let files: Value = serde_json::from_str(&machine.grep_impl(args).await.unwrap()).unwrap();
        assert_eq!(files["files"].as_array().unwrap().len(), 5, "{files}");

        let mut args = grep_args("needle");
        args.output_mode = GrepOutputMode::Files;
        args.gitignore = true;
        let files: Value = serde_json::from_str(&machine.grep_impl(args).await.unwrap()).unwrap();
        assert_eq!(
            files["files"],
            serde_json::json!([".secret.rs", "keep.log", "src/lib.rs"])
        );

        let mut args = grep_args("needle");
        args.output_mode = GrepOutputMode::Files;
        args.skip_hidden = true;
        let files: Value = serde_json::from_str(&machine.grep_impl(args).await.unwrap()).unwrap();
        assert_eq!(
            files["files"],
            serde_json::json!(["build/nested/out.rs", "debug.log", "keep.log", "src/lib.rs"])
        );

        let mut args = grep_args("needle");
        args.output_mode = GrepOutputMode::Files;
        args.skip_hidden = true;
        args.gitignore = true;
        let files: Value = serde_json::from_str(&machine.grep_impl(args).await.unwrap()).unwrap();
        assert_eq!(
            files["files"],
            serde_json::json!(["keep.log", "src/lib.rs"])
        );

        // An invalid gitignore pattern fails closed.
        std::fs::write(root.join(".gitignore"), "{unclosed\n").unwrap();
        let mut args = grep_args("needle");
        args.gitignore = true;
        let error = machine.grep_impl(args).await.unwrap_err().to_string();
        assert!(error.contains("invalid .gitignore pattern"), "{error}");
    }
}

#[cfg(test)]
mod walk_tests {
    use super::*;
    use crate::mcp::tools::ls::ListArgs;
    use crate::mcp::tools::test_support::{grep_args, test_machine};
    use serde_json::Value;

    #[tokio::test]
    async fn grep_does_not_read_a_denied_gitignore() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        std::fs::create_dir_all(root.join("build")).unwrap();
        std::fs::write(root.join(".gitignore"), "build/\n").unwrap();
        std::fs::write(root.join("build/out.rs"), "needle\n").unwrap();
        std::fs::write(root.join("src.rs"), "needle\n").unwrap();
        let mut machine = test_machine(root.clone());
        // A deny on the ignore file itself must win over `gitignore`.
        machine.config.access.deny_read_roots = vec![root.join(".gitignore")];

        let mut args = grep_args("needle");
        args.output_mode = GrepOutputMode::Files;
        args.gitignore = true;
        let files: Value = serde_json::from_str(&machine.grep_impl(args).await.unwrap()).unwrap();
        assert_eq!(
            files["files"],
            serde_json::json!(["build/out.rs", "src.rs"])
        );
    }

    /// `ls` and `grep` share the walk, so one probe covers both.
    #[tokio::test]
    async fn unreadable_named_root_is_an_error_but_unreadable_children_are_skipped() {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        std::fs::create_dir(root.join("locked")).unwrap();
        std::fs::set_permissions(root.join("locked"), std::fs::Permissions::from_mode(0o000))
            .unwrap();
        let locked_readable = std::fs::read_dir(root.join("locked")).is_ok();
        let machine = test_machine(root.clone());

        let listed = machine
            .ls(Parameters(ListArgs {
                path: "locked".to_owned(),
                ..ListArgs::default()
            }))
            .await;
        std::fs::set_permissions(root.join("locked"), std::fs::Permissions::from_mode(0o755))
            .unwrap();

        // A privileged user can read it; everyone else gets a real error tool
        // result rather than a silently empty listing.
        assert_eq!(
            listed.is_error.unwrap_or(false),
            !locked_readable,
            "{listed:?}"
        );
    }

    #[tokio::test]
    async fn walk_skips_an_unreadable_directory_instead_of_failing() {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        std::fs::create_dir_all(root.join("locked")).unwrap();
        std::fs::write(root.join("locked/secret.rs"), "needle\n").unwrap();
        std::fs::write(root.join("open.rs"), "needle\n").unwrap();
        std::fs::set_permissions(root.join("locked"), std::fs::Permissions::from_mode(0o000))
            .unwrap();
        // Root (or a privileged CI user) can still enumerate it; the point is
        // that an unreadable directory never fails the search.
        let locked_readable = std::fs::read_dir(root.join("locked")).is_ok();
        let machine = test_machine(root.clone());
        let mut args = grep_args("needle");
        args.output_mode = GrepOutputMode::Files;
        let outcome = machine.grep_impl(args).await;
        std::fs::set_permissions(root.join("locked"), std::fs::Permissions::from_mode(0o755))
            .unwrap();

        let files: Value = serde_json::from_str(&outcome.unwrap()).unwrap();
        let expected = match locked_readable {
            true => serde_json::json!(["locked/secret.rs", "open.rs"]),
            false => serde_json::json!(["open.rs"]),
        };
        assert_eq!(files["files"], expected);
    }

    #[tokio::test]
    async fn grep_output_stops_at_the_profile_byte_ceiling() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        for index in 0..20 {
            let line = format!("needle {}", "x".repeat(200));
            std::fs::write(root.join(format!("f{index}.txt")), format!("{line}\n")).unwrap();
        }
        let machine = crate::mcp::tools::test_support::test_machine_with(root, |config| {
            config.max_output_bytes = 512
        });

        let args = grep_args("needle");
        let content = machine.grep_impl(args).await.unwrap();
        assert!(content.len() <= 512, "{} bytes", content.len());
        assert!(content.contains("truncated"), "{content}");

        // The JSON modes respect the same ceiling instead of serializing every
        // path they found.
        let mut args = grep_args("needle");
        args.output_mode = GrepOutputMode::Files;
        let files: Value = serde_json::from_str(&machine.grep_impl(args).await.unwrap()).unwrap();
        assert_eq!(files["truncated"], Value::Bool(true));
        assert!(files["files"].as_array().unwrap().len() < 20);
    }

    /// In `files`/`count` mode the per-file hit cap is about scan cost, not
    /// truncation: extra matches inside one file never mark the page short.
    #[tokio::test]
    async fn grep_files_mode_does_not_report_truncation_for_extra_hits() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        std::fs::write(root.join("many.txt"), "hit\nhit\nhit\n").unwrap();
        let machine = test_machine(root);

        let mut args = grep_args("hit");
        args.output_mode = GrepOutputMode::Files;
        let files: Value = serde_json::from_str(&machine.grep_impl(args).await.unwrap()).unwrap();
        assert_eq!(files["files"], serde_json::json!(["many.txt"]));
        assert_eq!(files["truncated"], Value::Bool(false));
    }

    #[tokio::test]
    async fn grep_content_pages_past_the_per_file_cap() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let lines = (0..60)
            .map(|index| format!("hit {index}"))
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(root.join("many.txt"), format!("{lines}\n")).unwrap();
        let machine = test_machine(root);

        // The 50-hit cap bounds one page, not the paging sequence: hit 51 must
        // still be reachable.
        let mut args = grep_args("hit");
        args.offset = Some(50);
        args.limit = Some(1);
        let content = machine.grep_impl(args).await.unwrap();
        assert_eq!(content, "many.txt:51: hit 50\n… truncated at 1 hits");
    }

    #[tokio::test]
    async fn grep_per_file_cap_keeps_pages_contiguous_across_files() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let many = (0..60)
            .map(|line| format!("hit {line}"))
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(root.join("a.txt"), format!("{many}\n")).unwrap();
        std::fs::write(root.join("b.txt"), "hit b\n").unwrap();
        let machine = test_machine(root);

        let first = machine.grep_impl(grep_args("hit")).await.unwrap();
        assert_eq!(first.matches("a.txt:").count(), 50);
        assert!(!first.contains("b.txt:"), "{first}");

        let mut args = grep_args("hit");
        args.offset = Some(50);
        let second = machine.grep_impl(args).await.unwrap();
        assert_eq!(second.matches("a.txt:").count(), 10);
        assert_eq!(second.matches("b.txt:").count(), 1, "{second}");
    }

    #[tokio::test]
    async fn grep_reports_byte_truncation_when_no_hit_fits() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        std::fs::write(root.join("wide.txt"), format!("hit {}\n", "x".repeat(2000))).unwrap();
        let machine = crate::mcp::tools::test_support::test_machine_with(root, |config| {
            config.max_output_bytes = 1024
        });
        let content = machine.grep_impl(grep_args("hit")).await.unwrap();
        assert_eq!(content, "… truncated at 1024 bytes");
    }

    /// A byte-limited page must end with whole hit lines: the marker is
    /// budgeted before a hit is accepted, never trimmed into it afterwards.
    #[tokio::test]
    async fn grep_byte_limited_pages_contain_only_complete_hits() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let wide = format!("hit {}", "x".repeat(495));
        std::fs::write(root.join("a.txt"), format!("{wide}\n{wide}\n")).unwrap();
        std::fs::write(root.join("b.txt"), "hit b\n").unwrap();
        let machine = crate::mcp::tools::test_support::test_machine_with(root, |config| {
            config.max_output_bytes = 1024
        });

        let content = machine.grep_impl(grep_args("hit")).await.unwrap();
        assert!(content.len() <= 1024, "{} bytes", content.len());

        let mut lines = content.lines().collect::<Vec<_>>();
        let marker = lines.pop().expect("a marker line");
        assert!(marker.starts_with("… truncated at "), "{marker}");
        assert!(
            !lines.is_empty(),
            "the page must carry whole hits: {content}"
        );
        for line in &lines {
            assert!(
                line.starts_with("a.txt:") || line.starts_with("b.txt:"),
                "{line}"
            );
            assert_eq!(
                line.len(),
                "a.txt:1: ".len() + wide.len(),
                "a returned hit line was cut short: {line:?}"
            );
            assert!(line.ends_with('x'), "{line:?}");
        }
    }

    #[tokio::test]
    async fn grep_names_the_per_file_cap_and_an_offset_past_the_last_hit() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let many = (0..60)
            .map(|line| format!("hit {line}"))
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(root.join("a.txt"), format!("{many}\n")).unwrap();
        std::fs::write(root.join("b.txt"), "hit b\n").unwrap();
        let machine = test_machine(root);

        // A page stopped by the per-file cap says so, so the count is not
        // mistaken for the call's own `limit`.
        let first = machine.grep_impl(grep_args("hit")).await.unwrap();
        assert!(
            first.contains("… truncated at 50 hits (per-file cap)"),
            "{first}"
        );

        // An offset past the last hit answers with a marker instead of silence.
        let mut args = grep_args("hit");
        args.offset = Some(1_000);
        let content = machine.grep_impl(args).await.unwrap();
        assert_eq!(content, "… no hits at offset 1000");
    }

    #[tokio::test]
    async fn grep_context_paging_does_not_repeat_matching_lines() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        std::fs::write(
            root.join("a.txt"),
            (0..60).map(|_| "hit").collect::<Vec<_>>().join("\n") + "\n",
        )
        .unwrap();
        let machine = test_machine(root);

        let mut args = grep_args("hit");
        args.after = Some(8);
        let first = machine.grep_impl(args).await.unwrap();
        assert_eq!(first.matches("a.txt:").count(), 50, "{first}");
        assert!(first.contains("truncated at 50 hits"), "{first}");
        assert!(!first.contains("a.txt:51:"), "{first}");

        let mut args = grep_args("hit");
        args.after = Some(8);
        args.offset = Some(50);
        let second = machine.grep_impl(args).await.unwrap();
        assert_eq!(second.matches("a.txt:").count(), 10, "{second}");
        assert!(second.contains("a.txt:51: hit"), "{second}");
        assert!(second.contains("a.txt:60: hit"), "{second}");
    }

    #[tokio::test]
    async fn grep_byte_budget_selects_a_prefix_instead_of_blanking_the_page() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let line = format!("hit {}", "x".repeat(200));
        std::fs::write(
            root.join("wide.txt"),
            (0..100)
                .map(|_| line.as_str())
                .collect::<Vec<_>>()
                .join("\n")
                + "\n",
        )
        .unwrap();
        let machine = crate::mcp::tools::test_support::test_machine_with(root, |config| {
            config.max_output_bytes = 4096
        });
        let content = machine.grep_impl(grep_args("hit")).await.unwrap();
        assert!(content.matches("wide.txt:").count() > 0, "{content}");
        assert!(content.matches("wide.txt:").count() < 50, "{content}");
        assert!(content.contains("truncated at 4096 bytes"), "{content}");
    }

    #[tokio::test]
    async fn grep_byte_stop_reports_bytes_even_after_a_hit_was_emitted() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let line = format!("hit {}", "x".repeat(600));
        std::fs::write(root.join("wide.txt"), format!("{line}\n{line}\n")).unwrap();
        let machine = crate::mcp::tools::test_support::test_machine_with(root, |config| {
            config.max_output_bytes = 1024
        });
        let content = machine.grep_impl(grep_args("hit")).await.unwrap();
        assert!(content.contains("truncated at 1024 bytes"), "{content}");
        assert!(!content.contains("truncated at 1 hits"), "{content}");
    }

    #[tokio::test]
    async fn grep_json_modes_stay_inside_the_byte_ceiling() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        for index in 0..5 {
            // Escaped control characters make the rendered path much longer
            // than its byte length, which is what used to break the ceiling.
            let name = format!("e{index}{}", "\u{1}".repeat(150));
            std::fs::write(root.join(&name), "hit\n").unwrap();
        }
        let machine = crate::mcp::tools::test_support::test_machine_with(root, |config| {
            config.max_output_bytes = 1024
        });
        for mode in [GrepOutputMode::Files, GrepOutputMode::Count] {
            let mut args = grep_args("hit");
            args.output_mode = mode;
            let rendered = machine.grep_impl(args).await.unwrap();
            assert!(rendered.len() <= 1024, "{} bytes", rendered.len());
            let value: Value = serde_json::from_str(&rendered).unwrap();
            assert_eq!(value["truncated"], Value::Bool(true));
        }
    }
}
