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
use tokio::io::{AsyncRead, AsyncReadExt};

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
/// Per-request search I/O (not a limit on concurrent HTTP/stdio tool calls).
const MAX_GREP_SCAN_BYTES: usize = 32 * 1024 * 1024;
const MAX_GREP_SCAN_FILES: usize = 4_096;
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
    WorkBudget,
}

/// Suffix that names the per-file cap as the reason a content page stopped, so a
/// reader does not mistake it for the call's own `limit`.
const IGNORE_INCOMPLETE_MARKER: &str =
    "… ignore rules unavailable; results may include or omit matches";
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
        .max("… search work budget exhausted; narrow path or glob".len())
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

enum SearchRead {
    Text(String, usize),
    Skipped(usize),
    Unreadable(usize),
    WorkBudgetExceeded,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
enum SearchSize {
    Read(usize),
    Skip,
    WorkBudgetExceeded,
}

fn classify_search_size(bytes: u64, remaining: usize) -> SearchSize {
    if bytes > MAX_GREP_FILE_BYTES as u64 {
        SearchSize::Skip
    } else if bytes > remaining as u64 {
        SearchSize::WorkBudgetExceeded
    } else {
        SearchSize::Read(bytes as usize)
    }
}

fn verify_search_size(declared: u64, observed: u64, remaining: usize) -> Option<SearchSize> {
    match classify_search_size(observed, remaining) {
        SearchSize::Read(_) if observed != declared => None,
        decision => Some(decision),
    }
}

/// Read only the bytes proved by descriptor metadata. There is deliberately no
/// probe read: callers verify descriptor metadata again before accepting the
/// bytes, so exact ceilings work while growth and uncertainty fail closed.
async fn read_declared_bytes<R: AsyncRead + Unpin>(
    reader: R,
    expected_len: usize,
) -> (Vec<u8>, bool) {
    let mut bytes = Vec::with_capacity(expected_len.min(64 * 1024));
    let result = reader
        .take(expected_len as u64)
        .read_to_end(&mut bytes)
        .await;
    let complete = result.is_ok() && bytes.len() == expected_len;
    (bytes, complete)
}

impl LocalMachine {
    /// Read through the capability-rooted file descriptor, charging *actual*
    /// bytes even when the result is non-text or unreadable. An over-budget
    /// candidate ends the lexically ordered search rather than silently
    /// skipping it and returning misleading later matches.
    async fn read_search_file(&self, path: &Path, remaining: usize) -> SearchRead {
        let Ok(mut file) = self.open_read_file(path).await else {
            return SearchRead::Unreadable(0);
        };
        let before = match file.metadata().await {
            Ok(metadata) if metadata.is_file() => metadata,
            Ok(_) | Err(_) => return SearchRead::Unreadable(0),
        };
        let expected_len = match classify_search_size(before.len(), remaining) {
            SearchSize::Read(bytes) => bytes,
            SearchSize::Skip => return SearchRead::Skipped(0),
            SearchSize::WorkBudgetExceeded => return SearchRead::WorkBudgetExceeded,
        };
        let (bytes, complete) = read_declared_bytes(&mut file, expected_len).await;
        let scanned = bytes.len();
        if !complete {
            return SearchRead::Unreadable(scanned);
        }
        let after = match file.metadata().await {
            Ok(metadata) if metadata.is_file() => metadata,
            Ok(_) | Err(_) => return SearchRead::Unreadable(scanned),
        };
        match verify_search_size(before.len(), after.len(), remaining) {
            Some(SearchSize::Skip) => return SearchRead::Skipped(scanned),
            Some(SearchSize::WorkBudgetExceeded) => {
                return SearchRead::WorkBudgetExceeded;
            }
            None => return SearchRead::Unreadable(scanned),
            Some(SearchSize::Read(_)) => {}
        }
        match super::text_bytes(&bytes) {
            Some(text) => SearchRead::Text(text.to_owned(), scanned),
            None => SearchRead::Skipped(scanned),
        }
    }

    async fn grep_impl(&self, args: GrepArgs) -> Result<String> {
        self.grep_impl_with_budget(args, MAX_GREP_SCAN_BYTES, MAX_GREP_SCAN_FILES)
            .await
    }

    async fn grep_impl_with_budget(
        &self,
        args: GrepArgs,
        max_scanned_bytes: usize,
        max_scanned_files: usize,
    ) -> Result<String> {
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
            GrepOutputMode::Content => {
                marker_reserve()
                    + IGNORE_INCOMPLETE_MARKER.len()
                    + 1
                    + "… unreadable entries; search results incomplete".len()
                    + 1
            }
            GrepOutputMode::Files | GrepOutputMode::Count => 0,
        };
        let mut budget = OutputBudget::new(ceiling.saturating_sub(reserve));
        // Stop scanning at the first refusal in every mode. A later file must
        // not be emitted after a skipped one, or paging would repeat or lose
        // records depending on where the caller resumes.
        let mut stop: Option<StopReason> = None;
        let mut remaining_bytes = max_scanned_bytes;
        let mut remaining_files = max_scanned_files;
        let mut unreadable_files = false;

        for path in &candidates {
            if stop.is_some() {
                break;
            }
            if remaining_bytes == 0 || remaining_files == 0 {
                stop = Some(StopReason::WorkBudget);
                break;
            }
            remaining_files -= 1;
            let text = match self.read_search_file(path, remaining_bytes).await {
                SearchRead::Text(text, scanned) => {
                    remaining_bytes -= scanned;
                    text
                }
                SearchRead::Skipped(scanned) => {
                    remaining_bytes -= scanned;
                    continue;
                }
                SearchRead::Unreadable(scanned) => {
                    remaining_bytes -= scanned;
                    unreadable_files = true;
                    continue;
                }
                SearchRead::WorkBudgetExceeded => {
                    stop = Some(StopReason::WorkBudget);
                    break;
                }
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
        let work_incomplete = stop == Some(StopReason::WorkBudget);
        let mut incomplete_reasons = Vec::new();
        if work_incomplete {
            incomplete_reasons.push("content_budget");
        }
        incomplete_reasons.extend(progress.incomplete_reasons());
        if unreadable_files {
            incomplete_reasons.push("unreadable_files");
        }
        let scan_incomplete = !incomplete_reasons.is_empty();
        // Keep the established priority for clients that only understand the
        // singular field; the array retains every concurrent cause.
        let incomplete_reason = incomplete_reasons.first().copied();
        if progress.incomplete && !work_incomplete {
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
                    Some(StopReason::WorkBudget) => {
                        Some("… search work budget exhausted; narrow path or glob".to_owned())
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
                if progress.ignore_incomplete {
                    if !content.is_empty() {
                        content.push('\n');
                    }
                    content.push_str(IGNORE_INCOMPLETE_MARKER);
                }
                if progress.traversal_incomplete || unreadable_files {
                    if !content.is_empty() {
                        content.push('\n');
                    }
                    content.push_str("… unreadable entries; search results incomplete");
                }
                Ok(content)
            }
            GrepOutputMode::Files => {
                let render = |files: &[String], truncated: bool| {
                    serde_json::json!({
                        "files": files, "truncated": truncated,
                        "scan_incomplete": scan_incomplete,
                        "incomplete_reason": incomplete_reason,
                        "incomplete_reasons": incomplete_reasons,
                    })
                    .to_string()
                };
                let (files, truncated) = trim_to_ceiling(files, ceiling, stop.is_some(), render);
                Ok(serde_json::json!({
                    "files": files, "truncated": truncated,
                    "scan_incomplete": scan_incomplete,
                    "incomplete_reason": incomplete_reason,
                    "incomplete_reasons": incomplete_reasons,
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
                        "scan_incomplete": scan_incomplete,
                        "incomplete_reason": incomplete_reason,
                        "incomplete_reasons": incomplete_reasons,
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
        description = "Search UTF-8 text files recursively. `pattern` is regex unless `literal`; `glob` shares the `ls` grammar. `output_mode`: content (matches and context), files, count (per-file counts; `page_total` is page-scoped). Paging uses `offset`/`limit` (up to 200); `gitignore`/`skip_hidden` are opt-in. Directory discovery visits up to 100000 entries; per-call file scanning reads at most 32 MiB across up to 4096 candidates. Incomplete results carry explicit text markers or JSON `scan_incomplete`, compatibility `incomplete_reason`, and `incomplete_reasons` with every concurrent cause (directory_budget/content_budget/traversal_error/ignore_rules/unreadable_files). Narrow `path`/`glob` for complete results. Replies obey the profile output byte ceiling and only contain whole hit lines. Files over 1 MiB, files with binary control bytes, invalid UTF-8 files and symlinks are skipped.",
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
    async fn grep_source_reads_never_probe_past_declared_or_aggregate_limits() {
        use std::{
            pin::Pin,
            sync::{
                Arc,
                atomic::{AtomicUsize, Ordering},
            },
            task::{Context, Poll},
        };
        use tokio::io::ReadBuf;

        struct CountingReader {
            bytes: &'static [u8],
            position: usize,
            read: Arc<AtomicUsize>,
        }

        impl AsyncRead for CountingReader {
            fn poll_read(
                mut self: Pin<&mut Self>,
                _cx: &mut Context<'_>,
                buffer: &mut ReadBuf<'_>,
            ) -> Poll<std::io::Result<()>> {
                let count = (self.bytes.len() - self.position).min(buffer.remaining());
                if count > 0 {
                    buffer.put_slice(&self.bytes[self.position..self.position + count]);
                    self.position += count;
                    self.read.fetch_add(count, Ordering::Relaxed);
                }
                Poll::Ready(Ok(()))
            }
        }

        let read = Arc::new(AtomicUsize::new(0));
        let (bytes, complete) = read_declared_bytes(
            CountingReader {
                bytes: b"abcdprobe",
                position: 0,
                read: read.clone(),
            },
            4,
        )
        .await;
        assert!(complete);
        assert_eq!(bytes, b"abcd");
        assert_eq!(read.load(Ordering::Relaxed), 4);

        struct PartialThenError(bool);

        impl AsyncRead for PartialThenError {
            fn poll_read(
                mut self: Pin<&mut Self>,
                _cx: &mut Context<'_>,
                buffer: &mut ReadBuf<'_>,
            ) -> Poll<std::io::Result<()>> {
                if self.0 {
                    Poll::Ready(Err(std::io::Error::other("injected read failure")))
                } else {
                    self.0 = true;
                    buffer.put_slice(b"abc");
                    Poll::Ready(Ok(()))
                }
            }
        }

        let (bytes, complete) = read_declared_bytes(PartialThenError(false), 4).await;
        assert!(!complete);
        assert_eq!(bytes, b"abc", "partial bytes remain available to charge");

        assert_eq!(classify_search_size(4, 4), SearchSize::Read(4));
        assert_eq!(classify_search_size(5, 4), SearchSize::WorkBudgetExceeded);
        assert_eq!(verify_search_size(4, 4, 4), Some(SearchSize::Read(4)));
        assert_eq!(verify_search_size(4, 5, 10), None, "growth is uncertain");
        assert_eq!(verify_search_size(4, 3, 10), None, "shrink is uncertain");
        assert_eq!(
            classify_search_size(MAX_GREP_FILE_BYTES as u64, MAX_GREP_FILE_BYTES),
            SearchSize::Read(MAX_GREP_FILE_BYTES)
        );
        assert_eq!(
            classify_search_size(MAX_GREP_FILE_BYTES as u64 + 1, usize::MAX),
            SearchSize::Skip
        );

        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let path = root.join("exact.txt");
        std::fs::write(&path, "needle\n").unwrap();
        let machine = test_machine(root);
        match machine.read_search_file(&path, 7).await {
            SearchRead::Text(text, scanned) => {
                assert_eq!(text, "needle\n");
                assert_eq!(scanned, 7);
            }
            _ => panic!("a stable regular file exactly at the aggregate limit must be accepted"),
        }
        assert!(matches!(
            machine.read_search_file(&path, 6).await,
            SearchRead::WorkBudgetExceeded
        ));
    }

    #[tokio::test]
    async fn grep_work_budgets_report_incomplete_ordered_results() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        for name in ["a.txt", "b.txt", "c.txt"] {
            std::fs::write(root.join(name), "needle\n").unwrap();
        }
        let machine = test_machine(root);
        for mode in [
            GrepOutputMode::Content,
            GrepOutputMode::Files,
            GrepOutputMode::Count,
        ] {
            let mut args = grep_args("needle");
            args.output_mode = mode;
            let output = machine.grep_impl_with_budget(args, 8, 10).await.unwrap();
            match mode {
                GrepOutputMode::Content => {
                    assert!(output.contains("a.txt:1: needle"), "{output}");
                    assert!(!output.contains("b.txt"), "{output}");
                    assert!(output.contains("search work budget exhausted"), "{output}");
                }
                GrepOutputMode::Files | GrepOutputMode::Count => {
                    let value: Value = serde_json::from_str(&output).unwrap();
                    assert_eq!(value["scan_incomplete"], true);
                    assert_eq!(value["incomplete_reason"], "content_budget");
                    assert_eq!(
                        value["incomplete_reasons"],
                        serde_json::json!(["content_budget"])
                    );
                    assert_eq!(value["truncated"], true);
                    assert!(!output.contains("b.txt"), "{output}");
                }
            }
        }
        let mut args = grep_args("needle");
        args.output_mode = GrepOutputMode::Files;
        let value: Value =
            serde_json::from_str(&machine.grep_impl_with_budget(args, 1024, 1).await.unwrap())
                .unwrap();
        assert_eq!(value["scan_incomplete"], true);
        assert_eq!(value["incomplete_reason"], "content_budget");
        assert_eq!(
            value["incomplete_reasons"],
            serde_json::json!(["content_budget"])
        );
        assert_eq!(value["files"], serde_json::json!(["a.txt"]));
    }

    #[tokio::test]
    async fn grep_ignores_control_byte_binary_files_in_every_mode() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        std::fs::write(root.join("a.bin"), b"before\0needle\0after\n").unwrap();
        std::fs::write(root.join("b.txt"), b"needle here\n").unwrap();
        let machine = test_machine(root);
        for mode in [
            GrepOutputMode::Content,
            GrepOutputMode::Files,
            GrepOutputMode::Count,
        ] {
            let mut args = grep_args("needle");
            args.output_mode = mode;
            let output = machine.grep_impl(args).await.unwrap();
            assert!(output.contains("b.txt"), "{output}");
            assert!(!output.contains("a.bin"), "{output}");
        }
    }

    #[tokio::test]
    async fn grep_accepts_exact_per_file_limit_and_skips_one_byte_over() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let mut exact = vec![b'x'; MAX_GREP_FILE_BYTES];
        exact[..7].copy_from_slice(b"needle\n");
        std::fs::write(root.join("exact.txt"), exact).unwrap();
        let mut over = vec![b'x'; MAX_GREP_FILE_BYTES + 1];
        over[..7].copy_from_slice(b"needle\n");
        std::fs::write(root.join("over.txt"), over).unwrap();
        let machine = test_machine(root);
        let mut args = grep_args("needle");
        args.output_mode = GrepOutputMode::Files;
        let files: Value = serde_json::from_str(&machine.grep_impl(args).await.unwrap()).unwrap();
        assert_eq!(files["files"], serde_json::json!(["exact.txt"]));
    }

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
        assert_eq!(files["incomplete_reason"], Value::Null);
        assert_eq!(files["incomplete_reasons"], serde_json::json!([]));

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
        assert_eq!(counts["incomplete_reason"], Value::Null);
        assert_eq!(counts["incomplete_reasons"], serde_json::json!([]));
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
    #[cfg(unix)]
    use crate::mcp::tools::ls::ListArgs;
    use crate::mcp::tools::test_support::{grep_args, test_machine};
    use serde_json::Value;

    #[tokio::test]
    async fn oversized_gitignore_never_silently_widens_search() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        std::fs::write(root.join("match.txt"), "needle\n").unwrap();
        let machine = test_machine(root.clone());
        let mut args = grep_args("needle");
        args.output_mode = GrepOutputMode::Files;
        args.gitignore = true;

        std::fs::write(root.join(".gitignore"), "match.txt\n").unwrap();
        let clean: Value =
            serde_json::from_str(&machine.grep_impl(args.clone()).await.unwrap()).unwrap();
        assert_eq!(clean["files"], serde_json::json!([]));

        std::fs::write(
            root.join(".gitignore"),
            format!(
                "#{}",
                "x".repeat(crate::mcp::tools::GITIGNORE_MAX_BYTES - 1)
            ),
        )
        .unwrap();
        let exact: Value =
            serde_json::from_str(&machine.grep_impl(args.clone()).await.unwrap()).unwrap();
        assert_eq!(exact["files"], serde_json::json!(["match.txt"]));

        std::fs::write(
            root.join(".gitignore"),
            format!("{}match.txt\n", "unused-pattern\n".repeat(1024)),
        )
        .unwrap();
        let error = machine
            .grep_impl(args.clone())
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("1024 line .gitignore limit"), "{error}");

        std::fs::write(root.join(".gitignore"), "x".repeat(64 * 1024 + 1)).unwrap();
        let error = machine.grep_impl(args).await.unwrap_err().to_string();
        assert!(error.contains("byte .gitignore limit"), "{error}");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn unreadable_tree_and_candidates_report_incomplete_searches() {
        use std::os::unix::fs::PermissionsExt as _;
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let locked = root.join("locked");
        std::fs::create_dir(&locked).unwrap();
        std::fs::write(locked.join("secret.txt"), "needle\n").unwrap();
        std::fs::write(root.join("readable.txt"), "needle\n").unwrap();
        let machine = test_machine(root.clone());
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
        let actually_unreadable = std::fs::read_dir(&locked).is_err();
        let mut args = grep_args("needle");
        args.output_mode = GrepOutputMode::Files;
        let files: Value =
            serde_json::from_str(&machine.grep_impl(args.clone()).await.unwrap()).unwrap();
        args.output_mode = GrepOutputMode::Content;
        let content = machine.grep_impl(args).await.unwrap();
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o700)).unwrap();
        if actually_unreadable {
            assert_eq!(files["incomplete_reason"], "traversal_error");
            assert_eq!(
                files["incomplete_reasons"],
                serde_json::json!(["traversal_error"])
            );
            assert_eq!(files["scan_incomplete"], true);
            assert!(content.contains("unreadable entries; search results incomplete"));
        }

        let restricted = root.join("restricted.txt");
        std::fs::write(&restricted, "needle\n").unwrap();
        std::fs::set_permissions(&restricted, std::fs::Permissions::from_mode(0o000)).unwrap();
        let unreadable_file = std::fs::File::open(&restricted).is_err();
        let mut args = grep_args("needle");
        args.output_mode = GrepOutputMode::Files;
        let files: Value = serde_json::from_str(&machine.grep_impl(args).await.unwrap()).unwrap();
        std::fs::set_permissions(&restricted, std::fs::Permissions::from_mode(0o600)).unwrap();
        if unreadable_file {
            assert_eq!(files["scan_incomplete"], true);
            assert_eq!(files["incomplete_reason"], "unreadable_files");
            assert_eq!(
                files["incomplete_reasons"],
                serde_json::json!(["unreadable_files"])
            );
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn ignore_unavailability_marks_json_and_text_results() {
        use std::os::unix::fs::PermissionsExt as _;
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let ignore = root.join(".gitignore");
        std::fs::write(&ignore, "match.txt\n").unwrap();
        std::fs::write(root.join("match.txt"), "needle\n").unwrap();
        let machine = test_machine(root);
        std::fs::set_permissions(&ignore, std::fs::Permissions::from_mode(0o000)).unwrap();
        let unreadable = std::fs::File::open(&ignore).is_err();
        let mut args = grep_args("needle");
        args.gitignore = true;
        args.output_mode = GrepOutputMode::Files;
        let files: Value =
            serde_json::from_str(&machine.grep_impl(args.clone()).await.unwrap()).unwrap();
        args.output_mode = GrepOutputMode::Content;
        let content = machine.grep_impl(args).await.unwrap();
        std::fs::set_permissions(&ignore, std::fs::Permissions::from_mode(0o600)).unwrap();
        if unreadable {
            assert_eq!(files["scan_incomplete"], true);
            assert_eq!(files["incomplete_reason"], "ignore_rules");
            assert_eq!(
                files["incomplete_reasons"],
                serde_json::json!(["ignore_rules", "unreadable_files"])
            );
            assert!(
                content.contains("ignore rules unavailable; results may include or omit matches"),
                "{content}"
            );
            assert!(
                content.contains("unreadable entries; search results incomplete"),
                "{content}"
            );
        }
    }

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
        assert_eq!(files["scan_incomplete"], true);
        assert_eq!(files["incomplete_reason"], "ignore_rules");
        assert_eq!(
            files["incomplete_reasons"],
            serde_json::json!(["ignore_rules"])
        );

        let mut args = grep_args("needle");
        args.output_mode = GrepOutputMode::Files;
        args.gitignore = true;
        let concurrent: Value = serde_json::from_str(
            &machine
                .grep_impl_with_budget(args, 8, MAX_GREP_SCAN_FILES)
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(concurrent["incomplete_reason"], "content_budget");
        assert_eq!(
            concurrent["incomplete_reasons"],
            serde_json::json!(["content_budget", "ignore_rules"])
        );
    }

    /// `ls` and `grep` share the walk, so one probe covers both.
    #[cfg(unix)]
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

    #[cfg(unix)]
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
        // Escaped characters make the rendered path much longer than its byte
        // length, which is what used to break the ceiling. Windows rejects a
        // control byte in a file name, so use the quote it must still escape.
        #[cfg(not(windows))]
        let padding = "\u{1}";
        #[cfg(windows)]
        let padding = "\"";
        for index in 0..5 {
            let name = format!("e{index}{}", padding.repeat(150));
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
