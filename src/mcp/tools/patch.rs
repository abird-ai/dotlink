//! `patch`: Codex grammar with strict preflight and per-file atomic commits.

use std::{
    collections::BTreeSet,
    ops::Range,
    path::{Path, PathBuf},
};

use anyhow::{Result, anyhow, bail};
use rmcp::{
    handler::server::wrapper::Parameters, model::CallToolResult, schemars, tool, tool_router,
};
use serde::Deserialize;
use tokio::fs;

use super::{
    LocalMachine, Snapshot, dot,
    mutation::{MutationReceipt, MutationTarget, PlanBudget, PlannedMutation, stage_and_commit},
    ok_text, tool_error,
};
use crate::mcp::AccessNeed;
use crate::mcp::lexical_normalize;

const MAX_PATCH_FILES: usize = 32;
const MAX_PATCH_HUNKS: usize = 64;
/// The ambiguity message lists at most this many line numbers.
const MAX_AMBIGUOUS_LINES: usize = 10;

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub(super) struct PatchArgs {
    /// Base directory for the patch's relative paths. Relative paths resolve
    /// from the directory where dotlink was launched.
    #[serde(default = "dot")]
    dir: String,

    /// Codex patch text: `*** Begin Patch`, `*** Update File:` / `*** Add File:`
    /// sections, then `*** End Patch`. Addition-only update hunks require
    /// an explicit `*** End of File` marker to anchor their insertion.
    patch: String,

    /// Write the files that planned cleanly and report the rest instead of
    /// failing the whole call.
    #[serde(default)]
    best_effort: bool,
}

/// Codex `*** Begin Patch` grammar, v1: `*** Update File:` and
/// `*** Add File:` only. Delete and Move are rejected explicitly rather than
/// silently ignored.
#[derive(Debug)]
enum PatchOp<'a> {
    Update {
        path: &'a str,
        hunks: Vec<PatchHunk<'a>>,
    },
    Add {
        path: &'a str,
        // Add-file source is contiguous in the original request; one byte
        // range replaces a per-line Vec even for million-line additions.
        body: Range<usize>,
        lines: usize,
        bytes: usize,
    },
}

#[derive(Debug)]
struct PatchHunk<'a> {
    anchor: Option<&'a str>,
    /// Exact hunk body borrowed from the original request. Update hunks stay
    /// compact regardless of line count; lines are decoded lazily when matching
    /// or applying, after mutation scratch limits are known.
    body: &'a str,
    body_start: usize,
    patch_crlf: bool,
    /// The Codex `*** End of File` marker: this hunk must end at the last line.
    at_end_of_file: bool,
}

#[derive(Clone, Copy, Debug)]
enum PatchLine<'a> {
    Context(&'a str),
    Removed(&'a str),
    Added(&'a str),
}

impl<'a> PatchLine<'a> {
    /// Only context and removed lines consume source lines. Added lines never
    /// enter the search pattern.
    fn source_text(self) -> Option<&'a str> {
        match self {
            Self::Context(text) | Self::Removed(text) => Some(text),
            Self::Added(_) => None,
        }
    }
}

impl<'a> PatchHunk<'a> {
    /// Re-decode borrowed hunk lines on demand instead of retaining one enum
    /// descriptor per request line. The iterator is cloneable so edit groups
    /// can be pre-scanned without buffering dense additions.
    fn lines(&self) -> impl Iterator<Item = PatchLine<'a>> + Clone {
        let patch_crlf = self.patch_crlf;
        patch_lines(self.body).map(move |raw_line| {
            let line = if patch_crlf {
                raw_line.strip_suffix('\r').unwrap_or(raw_line)
            } else {
                raw_line
            };
            decode_hunk_line(line).expect("validated hunk lines stay valid")
        })
    }
}

/// Borrow an unchanged source line or a patch addition. The only extra
/// representation needed by CRLF normalization is a single trailing CR bit;
/// adding CR never allocates a String per line.
#[derive(Clone, Copy)]
struct WorkingLine<'a> {
    text: &'a str,
    append_cr: bool,
    // A CR belonging to a formerly terminated CRLF line must not be left
    // dangling if this line becomes the un-terminated end of the edited file.
    delimiter_cr: bool,
    terminated: bool,
}

impl<'a> WorkingLine<'a> {
    fn source(text: &'a str) -> Self {
        Self {
            text,
            append_cr: false,
            delimiter_cr: false,
            terminated: false,
        }
    }

    fn source_line(text: &'a str, terminated: bool) -> Self {
        Self {
            delimiter_cr: terminated && text.ends_with('\r'),
            terminated,
            ..Self::source(text)
        }
    }

    fn added(text: &'a str, crlf: bool) -> Self {
        // The parser removes CRLF *syntax* from patch lines. Anything still in
        // `text`, including a final CR, is literal content and stays intact.
        // If content itself ends with a literal CR, use a separate CR for
        // the new delimiter; otherwise the literal byte would be consumed as
        // CRLF syntax on the next line. The final-EOF path removes the extra
        // delimiter CR without touching content when no LF follows.
        let delimiter_cr = crlf || text.ends_with('\r');
        Self {
            text,
            append_cr: delimiter_cr,
            delimiter_cr,
            terminated: true,
        }
    }

    fn len(self) -> usize {
        self.text.len() + usize::from(self.append_cr)
    }

    fn drop_terminal_delimiter_cr(&mut self) -> bool {
        // Becoming the unterminated last line also removes its *logical* LF
        // separator, even when no CR byte has to be stripped.
        self.terminated = false;
        if !self.delimiter_cr {
            return false;
        }
        if self.append_cr {
            self.append_cr = false;
        } else {
            self.text = self
                .text
                .strip_suffix('\r')
                .expect("CRLF marker is backed by trailing CR");
        }
        self.delimiter_cr = false;
        true
    }

    fn contains(self, needle: &str) -> bool {
        self.text.contains(needle)
            || (self.append_cr
                && needle
                    .strip_suffix('\r')
                    .is_some_and(|prefix| self.text.ends_with(prefix)))
    }

    fn equals(self, patch: &str) -> bool {
        // Compare logical text, not the source line's CRLF delimiter. A CR
        // without a following LF is literal content and is never removed.
        if self.delimiter_cr && !self.append_cr {
            self.text.strip_suffix('\r') == Some(patch)
        } else {
            self.text == patch
        }
    }

    fn equals_trimmed(self, patch: &str) -> bool {
        self.text.trim_end() == patch.trim_end()
    }

    fn append_to(self, output: &mut String) {
        output.push_str(self.text);
        if self.append_cr {
            output.push('\r');
        }
    }
}

/// Use the edited line's delimiter when present. An unterminated final line
/// inherits its predecessor's style when replaced; an append to that line
/// joins with LF, preserving all existing bytes. Literal CR is not a delimiter.
fn crlf_for_edit(lines: &[WorkingLine<'_>], start: usize, trailing_newline: bool) -> bool {
    if let Some(current) = lines.get(start) {
        if start + 1 < lines.len() || trailing_newline {
            current.delimiter_cr
        } else {
            lines
                .get(start.wrapping_sub(1))
                .is_some_and(|line| line.delimiter_cr)
        }
    } else if trailing_newline {
        lines.last().is_some_and(|line| line.delimiter_cr)
    } else {
        lines
            .get(lines.len().wrapping_sub(2))
            .is_some_and(|line| line.delimiter_cr)
    }
}

/// When an unterminated source line gains a successor, a literal final
/// CR must not become the newly inserted LF's delimiter CR. Add a separate
/// CR even in an otherwise LF-style file; this retains the literal content.
fn requires_bridge_cr(line: WorkingLine<'_>, preferred_crlf: bool) -> bool {
    !line.terminated && (preferred_crlf || line.text.ends_with('\r'))
}

/// A borrowed view of source lines indexed by their absolute position in the
/// working file. Earlier hunks snapshot only their consumed span plus the next
/// boundary line, while final/preflight passes can borrow the full index.
#[derive(Clone, Copy)]
struct HunkSource<'s, 'a> {
    lines: &'s [WorkingLine<'a>],
    base: usize,
}

impl<'s, 'a> HunkSource<'s, 'a> {
    fn get(self, index: usize) -> Option<WorkingLine<'a>> {
        index
            .checked_sub(self.base)
            .and_then(|relative| self.lines.get(relative))
            .copied()
    }
}

/// A borrowed hunk action. Source indices are absolute working-file positions
/// so pure insertions use the same boundary semantics whether or not a later
/// context line happens to be present in the patch.
#[derive(Clone, Copy)]
enum HunkAction<'a> {
    Keep {
        source_index: usize,
        line: WorkingLine<'a>,
    },
    Remove,
    Add(WorkingLine<'a>),
}

/// Select an existing line's separator or inherit the nearest usable one at
/// an insertion boundary. Never infer it from an unrelated context line.
fn style_at(source: HunkSource<'_, '_>, at: usize, fallback: bool) -> bool {
    if let Some(line) = source.get(at)
        && line.terminated
    {
        return line.delimiter_cr;
    }
    for distance in [1, 2] {
        if let Some(previous) = at.checked_sub(distance).and_then(|index| source.get(index))
            && previous.terminated
        {
            return previous.delimiter_cr;
        }
    }
    fallback
}

/// Visit each hunk action without retaining a per-line patch representation or
/// a replacement-sized buffer. Each context boundary separates edit groups: a
/// removed line determines the separator for additions in its group, while a
/// pure insertion inspects its absolute source boundary (including the next
/// unconsumed line when present).
fn visit_hunk<'a>(
    source: HunkSource<'_, 'a>,
    source_start: usize,
    hunk: &PatchHunk<'a>,
    fallback_style: bool,
    mut visit: impl FnMut(HunkAction<'a>),
) {
    let mut cursor = source_start;
    let mut lines = hunk.lines().peekable();
    while let Some(line) = lines.peek().copied() {
        if matches!(line, PatchLine::Context(_)) {
            lines.next();
            visit(HunkAction::Keep {
                source_index: cursor,
                line: source
                    .get(cursor)
                    .expect("matched context has a corresponding source line"),
            });
            cursor += 1;
            continue;
        }

        // Pre-scan this edit group using a cloned zero-copy iterator. This is
        // what lets additions listed before removals inherit the same removed
        // line separators without buffering the group.
        let removed = lines
            .clone()
            .take_while(|line| !matches!(line, PatchLine::Context(_)))
            .filter(|line| matches!(line, PatchLine::Removed(_)))
            .count();
        let group_start = cursor;
        let mut added = 0_usize;
        while let Some(line) = lines.peek().copied() {
            if matches!(line, PatchLine::Context(_)) {
                break;
            }
            match lines.next().expect("peeked edit-group line") {
                PatchLine::Added(text) => {
                    let source_index = if removed == 0 {
                        group_start
                    } else {
                        group_start + added.min(removed - 1)
                    };
                    let style = style_at(source, source_index, fallback_style);
                    visit(HunkAction::Add(WorkingLine::added(text, style)));
                    added += 1;
                }
                PatchLine::Removed(_) => {
                    visit(HunkAction::Remove);
                    cursor += 1;
                }
                PatchLine::Context(_) => unreachable!("split on context"),
            }
        }
    }
}

/// Shared single-pass final assembler. Delay emitting one source line until
/// the next is known; when an unterminated line gains a successor, synthesize
/// its separator without mistaking any literal CR for a CRLF delimiter.
struct LineOutput<'a> {
    text: String,
    pending: Option<WorkingLine<'a>>,
    written: usize,
    total: usize,
    trailing_newline: bool,
    new_crlf: bool,
}

impl<'a> LineOutput<'a> {
    fn new(bytes: usize, total: usize, trailing_newline: bool, new_crlf: bool) -> Self {
        Self {
            text: String::with_capacity(bytes),
            pending: None,
            written: 0,
            total,
            trailing_newline,
            new_crlf,
        }
    }

    fn push(&mut self, line: WorkingLine<'a>) {
        if let Some(previous) = self.pending.replace(line) {
            previous.append_to(&mut self.text);
            if self.new_crlf && !previous.terminated {
                self.text.push('\r');
            }
            self.text.push('\n');
        }
        self.written += 1;
    }

    fn finish(mut self) -> String {
        debug_assert_eq!(self.written, self.total);
        if let Some(mut final_line) = self.pending {
            if !self.trailing_newline {
                final_line.drop_terminal_delimiter_cr();
            }
            final_line.append_to(&mut self.text);
        }
        if self.trailing_newline {
            self.text.push('\n');
        }
        self.text
    }
}

/// Patch lines, split on `\n` only: a CRLF patch keeps its `\r`, and
/// `apply_patch_hunks` normalizes added lines to the target file's terminator.
fn patch_lines(text: &str) -> impl Iterator<Item = &str> + Clone {
    text.split_inclusive('\n')
        .map(|line| line.strip_suffix('\n').unwrap_or(line))
}

/// Decode one already-normalized update-hunk line. Parsing and lazy replay use
/// this one grammar helper so validation cannot drift from application.
fn decode_hunk_line(line: &str) -> Option<PatchLine<'_>> {
    match line.as_bytes().first() {
        Some(b' ') => Some(PatchLine::Context(&line[1..])),
        Some(b'-') => Some(PatchLine::Removed(&line[1..])),
        Some(b'+') => Some(PatchLine::Added(&line[1..])),
        _ if line.trim().is_empty() => Some(PatchLine::Context("")),
        _ => None,
    }
}

/// Shared, zero-copy add-file decoder for both byte preflight and output.
/// Bare blank lines are accepted as shorthand for `+`, but their CRLF
/// separator must be preserved just like an explicit `+\r\n` blank line.
fn add_file_line(raw: &str) -> Option<&str> {
    match raw.strip_prefix('+') {
        Some(text) => Some(text),
        None if raw.trim().is_empty() => Some(if raw.ends_with('\r') { "\r" } else { "" }),
        None => None,
    }
}

fn parse_patch(text: &str) -> Result<Vec<PatchOp<'_>>> {
    // File names and anchors borrow the request, while hunk/add-file bodies
    // retain byte ranges into it. Parser storage is therefore O(files+hunks),
    // not O(patch lines); dense additions are decoded only during planning.
    let mut ops: Vec<PatchOp<'_>> = Vec::new();
    let mut saw_begin = false;
    let mut saw_end = false;
    let mut hunks = 0_usize;
    let mut cursor = 0_usize;
    let mut patch_crlf = false;

    for (index, raw_line) in patch_lines(text).enumerate() {
        let number = index + 1;
        // The iterator strips only LF, not CR; the cursor keeps the exact
        // raw byte range so add-file bodies can be borrowed in one piece.
        cursor += raw_line.len()
            + usize::from(text.as_bytes().get(cursor + raw_line.len()) == Some(&b'\n'));
        if !saw_begin {
            let line = raw_line;
            if line.trim().is_empty() {
                continue;
            }
            if line.trim_end() != "*** Begin Patch" {
                bail!("patch: missing '*** Begin Patch' header");
            }
            patch_crlf = raw_line.ends_with('\r');
            saw_begin = true;
            continue;
        }
        // The header fixes the patch format: in CRLF patches, strip the CR
        // belonging to the transport delimiter from update-hunk lines. In LF
        // patches, a trailing CR in a hunk is literal text, not a delimiter.
        let line = if patch_crlf {
            raw_line.strip_suffix('\r').unwrap_or(raw_line)
        } else {
            raw_line
        };
        if saw_end {
            if !line.trim().is_empty() {
                bail!("patch: line {number}: unexpected text after '*** End Patch'");
            }
            continue;
        }

        if let Some(directive) = line.strip_prefix("*** ") {
            let directive = directive.trim_end();
            if directive == "End Patch" {
                saw_end = true;
                continue;
            }
            if directive == "End of File" {
                match ops.last_mut() {
                    Some(PatchOp::Update { hunks, .. }) => match hunks.last_mut() {
                        Some(hunk) if hunk.at_end_of_file => {
                            bail!("patch: line {number}: duplicate '*** End of File' marker")
                        }
                        Some(hunk) => hunk.at_end_of_file = true,
                        None => bail!("patch: line {number}: '*** End of File' needs a hunk"),
                    },
                    _ => {
                        bail!("patch: line {number}: '*** End of File' is only valid in an update")
                    }
                }
                continue;
            }
            if let Some(path) = directive.strip_prefix("Update File:") {
                if ops.len() == MAX_PATCH_FILES {
                    bail!(
                        "patch: {} files, above the {MAX_PATCH_FILES} file limit",
                        ops.len() + 1
                    );
                }
                let path = path.trim();
                if path.is_empty() {
                    bail!("patch: line {number}: '*** Update File' needs a path");
                }
                ops.push(PatchOp::Update {
                    path,
                    hunks: Vec::new(),
                });
                continue;
            }
            if let Some(path) = directive.strip_prefix("Add File:") {
                if ops.len() == MAX_PATCH_FILES {
                    bail!(
                        "patch: {} files, above the {MAX_PATCH_FILES} file limit",
                        ops.len() + 1
                    );
                }
                let path = path.trim();
                if path.is_empty() {
                    bail!("patch: line {number}: '*** Add File' needs a path");
                }
                ops.push(PatchOp::Add {
                    path,
                    body: cursor..cursor,
                    lines: 0,
                    bytes: 0,
                });
                continue;
            }
            let name = directive.split(':').next().unwrap_or(directive);
            bail!("patch: line {number}: unsupported directive '*** {name}'");
        }

        let Some(current) = ops.last_mut() else {
            bail!("patch: line {number}: text outside a file section");
        };

        if let Some(rest) = line.strip_prefix("@@") {
            let PatchOp::Update { hunks: list, .. } = current else {
                bail!("patch: line {number}: '*** Add File' does not take hunks");
            };
            if list.last().is_some_and(|hunk| hunk.at_end_of_file) {
                bail!(
                    "patch: line {number}: no hunk may follow '*** End of File' in the same file"
                );
            }
            hunks += 1;
            if hunks > MAX_PATCH_HUNKS {
                bail!("patch: {hunks} hunks, above the {MAX_PATCH_HUNKS} hunk limit");
            }
            list.push(PatchHunk {
                anchor: parse_patch_anchor(rest),
                body: &text[cursor..cursor],
                body_start: cursor,
                patch_crlf,
                at_end_of_file: false,
            });
            continue;
        }

        match current {
            PatchOp::Update { hunks: list, .. } => {
                let Some(hunk) = list.last_mut() else {
                    bail!("patch: line {number}: text outside a hunk");
                };
                if hunk.at_end_of_file {
                    bail!("patch: line {number}: hunk data may not follow '*** End of File'");
                }
                if decode_hunk_line(line).is_none() {
                    bail!("patch: line {number}: unexpected text '{line}'");
                }
                hunk.body = &text[hunk.body_start..cursor];
            }
            PatchOp::Add {
                body, lines, bytes, ..
            } => {
                let Some(content) = add_file_line(raw_line) else {
                    bail!("patch: line {number}: unexpected text '{line}'");
                };
                let added_bytes = content.len() + 1; // content plus LF
                *bytes = bytes
                    .checked_add(added_bytes)
                    .ok_or_else(|| anyhow!("patch: added file byte count overflow"))?;
                *lines += 1;
                body.end = cursor;
            }
        }
    }

    if !saw_begin {
        bail!("patch: missing '*** Begin Patch' header");
    }
    if !saw_end {
        bail!("patch: missing '*** End Patch' terminator");
    }
    for op in &ops {
        if let PatchOp::Update { path, hunks } = op
            && hunks.is_empty()
        {
            bail!("patch: '*** Update File' for {path} has no hunks");
        }
    }
    Ok(ops)
}

/// `@@` may carry anchor text or unified-diff line numbers. Numbers are
/// accepted and ignored; anchor text narrows where the search starts.
/// `@@` content is either anchor text or unified-diff line numbers. Both may
/// appear (`@@ -5,1 +5,1 @@ fn b()`), so leading `-`/`+` number tokens are
/// dropped and whatever follows them narrows the search.
fn parse_patch_anchor(rest: &str) -> Option<&str> {
    let mut rest = rest.trim();
    loop {
        // `@@ -3,1 +3,1 @@ fn b()`: drop the range tokens, then the closing `@@`.
        let stripped = rest
            .strip_prefix('-')
            .or_else(|| rest.strip_prefix('+'))
            .map(str::trim_start)
            .filter(|stripped| stripped.starts_with(|c: char| c.is_ascii_digit()))
            .map(|stripped| {
                stripped
                    .trim_start_matches(|c: char| c.is_ascii_digit() || c == ',')
                    .trim_start()
            })
            .or_else(|| rest.strip_prefix("@@").map(str::trim_start));
        match stripped {
            Some(after) if after.len() < rest.len() => rest = after,
            _ => break,
        }
    }
    match rest.is_empty() {
        true => None,
        false => Some(rest),
    }
}

/// Hunk location is content-based: exact match first, then a per-line
/// `trim_end` comparison. Ambiguity is an error, and hunks within a file apply
/// top-down without re-scanning above the previous match.
fn apply_patch_hunks<'a>(
    text: &'a str,
    hunks: &'a [PatchHunk<'a>],
    display: &str,
    first_hunk: usize,
    total_hunks: usize,
    output_limit: usize,
    scratch_limit: usize,
) -> Result<(String, usize, usize)> {
    // Split on LF without manufacturing a phantom empty line after a
    // terminator. Retain its presence separately for exact output assembly.
    // `str::lines()` would strip CR and lose byte-for-byte source context.
    let newline_count = text.bytes().filter(|byte| *byte == b'\n').count();
    let mut trailing_newline = text.ends_with('\n');
    let line_count = newline_count + usize::from(!trailing_newline && !text.is_empty());
    let index_bytes = line_count.saturating_mul(std::mem::size_of::<WorkingLine<'_>>());
    if index_bytes > scratch_limit {
        bail!("patch source line index exceeds the remaining mutation plan budget");
    }
    let mut lines: Vec<WorkingLine<'_>> = Vec::with_capacity(line_count);
    lines.extend(
        text.split_terminator('\n')
            .enumerate()
            .map(|(index, line)| {
                WorkingLine::source_line(line, index + 1 < line_count || trailing_newline)
            }),
    );
    debug_assert_eq!(lines.len(), line_count);
    let mut content_bytes = text.len() - newline_count;
    let mut from_line = 0_usize;
    let mut added = 0_usize;
    let mut removed = 0_usize;

    for (offset, hunk) in hunks.iter().enumerate() {
        let position = first_hunk + offset + 1;
        // Retain only source-consuming text for KMP, and only after the shared
        // scratch budget is known. Dense pure-addition hunks allocate nothing
        // here; the parser itself retains only the hunk's borrowed byte range.
        let block_len = hunk.lines().filter_map(PatchLine::source_text).count();
        let matcher_bytes = block_len
            .checked_mul(std::mem::size_of::<&str>() + std::mem::size_of::<usize>())
            .ok_or_else(|| anyhow!("patch match index size overflow"))?;
        let live_index = lines
            .capacity()
            .checked_mul(std::mem::size_of::<WorkingLine<'_>>())
            .ok_or_else(|| anyhow!("patch source index size overflow"))?;
        if live_index.saturating_add(matcher_bytes) > scratch_limit {
            bail!("patch context matching exceeds the remaining mutation plan budget");
        }
        let mut block = Vec::with_capacity(block_len);
        block.extend(hunk.lines().filter_map(PatchLine::source_text));
        let mut search_start = from_line;
        if let Some(anchor) = hunk.anchor
            && let Some(at) = lines
                .iter()
                .enumerate()
                .skip(search_start)
                .find(|(_, line)| line.contains(anchor))
                .map(|(index, _)| index)
        {
            search_start = at;
        }
        let near = search_start + 1;

        // Only an explicit EOF marker can locate an insertion without any
        // existing context. An empty hunk itself still has no work to apply.
        if block.is_empty() && (!hunk.at_end_of_file || hunk.lines().next().is_none()) {
            bail!(
                "hunk {position}/{total_hunks} ({display}): context-free insertion requires '*** End of File' and an added line"
            );
        }

        let start = if block.is_empty() {
            lines.len()
        } else {
            // Match context once per policy, counting only positions eligible
            // under an explicit EOF constraint. Exact matches outrank trimmed
            // matches *among eligible positions*, not anywhere in the file.
            let locate = |trimmed: bool| {
                let mut count = 0_usize;
                let mut first = None;
                let mut preview = Vec::with_capacity(MAX_AMBIGUOUS_LINES);
                super::matching::scan_sequence(
                    &block,
                    &lines,
                    search_start,
                    |a, b| match trimmed {
                        true => a.trim_end() == b.trim_end(),
                        false => a == b,
                    },
                    |line, pattern| match trimmed {
                        true => line.equals_trimmed(pattern),
                        false => line.equals(pattern),
                    },
                    |at| {
                        if hunk.at_end_of_file && at + block_len != lines.len() {
                            return;
                        }
                        count += 1;
                        first.get_or_insert(at);
                        if preview.len() < MAX_AMBIGUOUS_LINES {
                            preview.push(at + 1);
                        }
                    },
                );
                (first, count, preview)
            };
            let (mut first, mut matches, mut preview) = locate(false);
            if matches == 0 {
                (first, matches, preview) = locate(true);
            }
            let Some(start) = first else {
                if hunk.at_end_of_file {
                    bail!(
                        "hunk {position}/{total_hunks} ({display}): '*** End of File' requires the hunk to end at the end of the file (line {})",
                        lines.len()
                    );
                }
                bail!(
                    "hunk {position}/{total_hunks} ({display}): context not found near line {near}"
                );
            };
            if matches > 1 {
                let lines_listed = preview
                    .iter()
                    .map(usize::to_string)
                    .collect::<Vec<_>>()
                    .join(", ");
                bail!(
                    "hunk {position}/{total_hunks} ({display}): context matches {matches} locations (lines {lines_listed})"
                );
            }
            start
        };

        // Pure context does not change the working line index. Avoid building
        // and splicing a full duplicate replacement list for a no-op hunk.
        if hunk
            .lines()
            .all(|line| matches!(line, PatchLine::Context(_)))
        {
            from_line = start + block.len();
            continue;
        }

        drop(block);

        // Preflight final bytes and scratch usage before allocating. Dense
        // added lines can need a large index even for a tiny source.
        let file_crlf = crlf_for_edit(&lines, start, trailing_newline);
        let replacement_count = hunk
            .lines()
            .filter(|line| !matches!(line, PatchLine::Removed(_)))
            .count();
        let next_line_count = lines
            .len()
            .checked_sub(block_len)
            .and_then(|n| n.checked_add(replacement_count))
            .ok_or_else(|| anyhow!("patch line count overflow"))?;
        let removed_bytes = lines[start..start + block_len]
            .iter()
            .try_fold(0_usize, |total, line| total.checked_add(line.len()))
            .ok_or_else(|| anyhow!("patch line size overflow"))?;
        // Use the same allocation-free hunk iterator for preflight, direct
        // output and in-place edits. Group-local styles prevent preceding
        // context from changing a replacement line's delimiter.
        let mut replacement_bytes = Some(0_usize);
        let mut produced_lines = start;
        let original_unterminated =
            !trailing_newline && lines.last().is_some_and(|line| !line.terminated);
        let mut original_last_out = (original_unterminated && start == lines.len())
            .then_some(lines.len().saturating_sub(1));
        let mut last_replacement = None;
        visit_hunk(
            HunkSource {
                lines: &lines,
                base: 0,
            },
            start,
            hunk,
            file_crlf,
            |action| {
                let replacement = match action {
                    HunkAction::Keep { source_index, line } => {
                        if original_unterminated && source_index + 1 == lines.len() {
                            original_last_out = Some(produced_lines);
                        }
                        produced_lines += 1;
                        Some(line)
                    }
                    HunkAction::Remove => None,
                    HunkAction::Add(line) => {
                        produced_lines += 1;
                        Some(line)
                    }
                };
                if let Some(line) = replacement {
                    last_replacement = Some(line);
                    replacement_bytes =
                        replacement_bytes.and_then(|sum| sum.checked_add(line.len()));
                }
            },
        );
        let replacement_bytes =
            replacement_bytes.ok_or_else(|| anyhow!("patch replacement size overflow"))?;
        let next_content = content_bytes
            .checked_sub(removed_bytes)
            .and_then(|size| size.checked_add(replacement_bytes))
            .ok_or_else(|| anyhow!("patch replacement size overflow"))?;
        let next_trailing_newline = trailing_newline && next_line_count > 0;
        let bridge_style = crlf_for_edit(&lines, lines.len().saturating_sub(1), trailing_newline);
        let bridge_cr = original_last_out.is_some_and(|position| {
            position + 1 < next_line_count
                && lines
                    .last()
                    .is_some_and(|line| requires_bridge_cr(*line, bridge_style))
        });
        // When deleting an unterminated last line, a preceding CRLF's CR no
        // longer has an LF to precede. Include that one-byte normalization in
        // the *preflight* length, so a result at the exact byte ceiling fits.
        let last_after = if start + block_len < lines.len() {
            lines.last().copied()
        } else {
            last_replacement.or_else(|| start.checked_sub(1).map(|index| lines[index]))
        };
        let drop_final_cr =
            !next_trailing_newline && last_after.is_some_and(|line| line.delimiter_cr);
        let next_size = next_content
            .checked_add(next_line_count.saturating_sub(1))
            .and_then(|size| size.checked_add(usize::from(next_trailing_newline)))
            .and_then(|size| size.checked_sub(usize::from(drop_final_cr)))
            .and_then(|size| size.checked_add(usize::from(bridge_cr)))
            .ok_or_else(|| anyhow!("patch result size overflow"))?;
        if next_size > output_limit {
            bail!(
                "patch result would be {next_size} bytes, above the {output_limit}-byte write limit"
            );
        }
        // A final hunk never needs a new line index. Construct the final text
        // directly from the old index and borrowed patch lines. This is also
        // what makes million-line additions use only their output buffer.
        if offset + 1 == hunks.len() {
            if live_index.saturating_add(next_size) > scratch_limit {
                bail!("patch working buffers exceed the remaining mutation plan budget");
            }
            let mut output =
                LineOutput::new(next_size, next_line_count, next_trailing_newline, bridge_cr);
            for &line in &lines[..start] {
                output.push(line);
            }
            visit_hunk(
                HunkSource {
                    lines: &lines,
                    base: 0,
                },
                start,
                hunk,
                file_crlf,
                |action| match action {
                    HunkAction::Keep { line, .. } => output.push(line),
                    HunkAction::Remove => removed += 1,
                    HunkAction::Add(line) => {
                        added += 1;
                        output.push(line);
                    }
                },
            );
            for &line in &lines[start + block_len..] {
                output.push(line);
            }
            let result = output.finish();
            if result.len() != next_size {
                bail!("patch output size changed during planning");
            }
            return Ok((result, added, removed));
        }

        // Earlier hunks need a live index for subsequent context matching.
        // Copy only the consumed span plus one unconsumed boundary line. The
        // boundary keeps pure-insertion separator selection identical to the
        // full-source path without allocating a replacement-sized Vec.
        let index_bytes = lines
            .capacity()
            .max(next_line_count)
            .checked_mul(std::mem::size_of::<WorkingLine<'_>>())
            .ok_or_else(|| anyhow!("patch line index size overflow"))?;
        let snapshot_lines = block_len + usize::from(start + block_len < lines.len());
        if index_bytes
            .saturating_add(next_size)
            .saturating_add(snapshot_lines.saturating_mul(std::mem::size_of::<WorkingLine<'_>>()))
            > scratch_limit
        {
            bail!("patch working buffers exceed the remaining mutation plan budget");
        }
        let consumed = lines[start..start + snapshot_lines].to_vec();
        let old_len = lines.len();
        if next_line_count > old_len {
            lines.reserve_exact(next_line_count - old_len);
            lines.resize(next_line_count, WorkingLine::source(""));
            lines.copy_within(start + block_len..old_len, start + replacement_count);
        } else if next_line_count < old_len {
            lines.copy_within(start + block_len..old_len, start + replacement_count);
            lines.truncate(next_line_count);
        }
        let mut consumed_at = 0_usize;
        let mut write_at = start;
        visit_hunk(
            HunkSource {
                lines: &consumed,
                base: start,
            },
            start,
            hunk,
            file_crlf,
            |action| match action {
                HunkAction::Keep { line, .. } => {
                    lines[write_at] = line;
                    write_at += 1;
                    consumed_at += 1;
                }
                HunkAction::Remove => {
                    removed += 1;
                    consumed_at += 1;
                }
                HunkAction::Add(line) => {
                    added += 1;
                    lines[write_at] = line;
                    write_at += 1;
                }
            },
        );
        debug_assert_eq!(consumed_at, block_len);
        debug_assert_eq!(write_at, start + replacement_count);
        content_bytes = next_content;
        if bridge_cr {
            let previous = &mut lines[original_last_out.expect("bridge has a source line")];
            previous.append_cr = true;
            previous.terminated = true;
            previous.delimiter_cr = true;
            content_bytes += 1;
        }
        trailing_newline = next_trailing_newline;
        if !trailing_newline
            && lines
                .last_mut()
                .is_some_and(WorkingLine::drop_terminal_delimiter_cr)
        {
            content_bytes -= 1;
            debug_assert!(drop_final_cr);
        } else {
            debug_assert!(!drop_final_cr);
        }
        from_line = start + replacement_count;
    }

    // Inspect the exact projected join size before allocating the final
    // result. Retained source and output must fit the shared plan budget.
    let bytes = lines
        .iter()
        .try_fold(0_usize, |sum, line| sum.checked_add(line.len()))
        .and_then(|sum| sum.checked_add(lines.len().saturating_sub(1)))
        .and_then(|sum| sum.checked_add(usize::from(trailing_newline)))
        .ok_or_else(|| anyhow!("patch result size overflow"))?;
    if bytes > output_limit {
        bail!("patch result would be {bytes} bytes, above the {output_limit}-byte write limit");
    }
    // The source snapshot and borrowed index coexist with the final output.
    // Count their live bytes separately from the output-file write ceiling.
    let final_index = lines
        .len()
        .saturating_mul(std::mem::size_of::<WorkingLine<'_>>());
    if final_index.saturating_add(bytes) > scratch_limit {
        bail!("patch working buffers exceed the remaining mutation plan budget");
    }
    let mut output = LineOutput::new(bytes, lines.len(), trailing_newline, false);
    for line in lines {
        output.push(line);
    }
    let result = output.finish();
    if result.len() != bytes {
        bail!("patch output size changed during planning");
    }
    Ok((result, added, removed))
}

/// Patch paths are relative to the patch's `dir`, not to the launch directory.
fn join_patch_path(dir: &str, path: &str) -> String {
    if Path::new(path).is_absolute() || dir == "." {
        return path.to_owned();
    }
    Path::new(dir).join(path).to_string_lossy().into_owned()
}

/// How a planned file is written. Update and create are different operations
/// with different rules; keeping them distinct beats overloading an `Option`.
enum PlanKind {
    Update(Snapshot),
    Create,
}

struct PlannedFile {
    path: PathBuf,
    display: String,
    kind: PlanKind,
    text: String,
    hunks: usize,
    added: usize,
    removed: usize,
}

impl LocalMachine {
    async fn patch_impl(&self, args: PatchArgs) -> Result<String> {
        super::require_text_bytes(args.patch.as_bytes(), "bwrite/bedit")?;
        if args.patch.len() > self.config.max_write_bytes {
            anyhow::bail!(
                "patch: {} bytes, above the {} byte write limit",
                args.patch.len(),
                self.config.max_write_bytes
            );
        }
        let ops = parse_patch(&args.patch)?;
        let total_hunks = ops
            .iter()
            .map(|op| match op {
                PatchOp::Update { hunks, .. } => hunks.len(),
                PatchOp::Add { .. } => 0,
            })
            .sum::<usize>();

        let mut budget = PlanBudget::default();
        let mut planned: Vec<PlannedFile> = Vec::new();
        let mut failures: Vec<(String, String)> = Vec::new();
        let mut seen: BTreeSet<PathBuf> = BTreeSet::new();
        let mut hunk_index = 0_usize;
        let dir = args.dir.as_str();
        // A section claims its path before it is resolved, so a section that
        // fails to resolve still prevents a later section from writing the same
        // file: the duplicate rule must not depend on resolution order.
        let claim = |path: &str| {
            let joined = join_patch_path(dir, path);
            let target = Path::new(&joined);
            match target.is_absolute() {
                true => lexical_normalize(target),
                false => lexical_normalize(&self.config.access.base_dir.join(target)),
            }
        };

        let claimed_paths = ops
            .iter()
            .map(|op| match op {
                PatchOp::Update { path, .. } | PatchOp::Add { path, .. } => claim(path),
            })
            .collect::<Vec<_>>();
        let _name_admission = self
            .reserve_mutation_names(claimed_paths.iter().map(|path| path.as_path()))
            .await?;

        struct ResolvedSection<'a, 'b> {
            op: &'a PatchOp<'b>,
            path: PathBuf,
            display: String,
            first_hunk: usize,
        }
        let mut resolved_sections: Vec<ResolvedSection<'_, '_>> = Vec::with_capacity(ops.len());

        for op in &ops {
            // Both sections resolve the same way, and both must reject a path
            // that another section already claimed.
            let (path, need, first_hunk) = match op {
                PatchOp::Update { path, hunks } => {
                    let first = hunk_index;
                    hunk_index += hunks.len();
                    (path, AccessNeed::ReadWrite, first)
                }
                PatchOp::Add { path, .. } => (path, AccessNeed::Write, 0),
            };
            let joined = join_patch_path(dir, path);
            let claimed = claim(path);
            let display = self.config.access.relative_display(&claimed);
            if !seen.insert(claimed.clone()) {
                let error = format!("patch: {display} appears in more than one section");
                if !args.best_effort {
                    anyhow::bail!("{error}");
                }
                failures.push((display, error));
                continue;
            }
            let resolved = match need {
                AccessNeed::Write => self.resolve_for_create(&joined, need).await,
                _ => self.resolve_existing(&joined, need).await,
            };
            let resolved = match resolved {
                Ok(resolved) => resolved,
                Err(error) => {
                    if !args.best_effort {
                        return Err(error);
                    }
                    failures.push((display, error.to_string()));
                    continue;
                }
            };
            let display = self.config.access.relative_display(&resolved);
            // The lexical claim above catches duplicates before resolution; the
            // canonical path catches two spellings (symlink aliases, a symlinked
            // parent) that name the same file. Both are needed: a section that
            // fails to resolve must still reserve the name it asked for.
            if resolved != claimed && !seen.insert(resolved.clone()) {
                let error = format!("patch: {display} appears in more than one section");
                if !args.best_effort {
                    anyhow::bail!("{error}");
                }
                failures.push((display, error));
                continue;
            }
            resolved_sections.push(ResolvedSection {
                op,
                path: resolved,
                display,
                first_hunk,
            });
        }

        // Hold every resolved target for the whole plan -> verify -> stage ->
        // commit sequence. Exact path locks are acquired in sorted order, so
        // multi-file patches cannot deadlock or collide on hash shards.
        let _mutation = self
            .lock_mutations(
                resolved_sections
                    .iter()
                    .map(|section| section.path.as_path()),
            )
            .await?;

        for ResolvedSection {
            op,
            path: resolved,
            display,
            first_hunk,
        } in resolved_sections
        {
            match op {
                PatchOp::Update { hunks, .. } => {
                    let outcome = async {
                        let limit = budget.copy_read_limit(self.config.max_read_bytes)?;
                        let bound = if limit < self.config.max_read_bytes {
                            "aggregate mutation plan budget"
                        } else {
                            "configured read limit"
                        };
                        let snapshot =
                            Snapshot::read_within(self, resolved.clone(), display.clone(), limit)
                                .await
                                .map_err(|error| anyhow!("{display}: {bound}: {error}"))?;
                        let scratch_limit =
                            budget.remaining().saturating_sub(snapshot.bytes().len());
                        let (edited, added, removed) = apply_patch_hunks(
                            snapshot.text()?,
                            hunks,
                            &display,
                            first_hunk,
                            total_hunks,
                            self.config.max_write_bytes,
                            scratch_limit,
                        )?;
                        budget.add(snapshot.bytes().len(), edited.len())?;
                        Ok(PlannedFile {
                            path: resolved,
                            display: display.clone(),
                            kind: PlanKind::Update(snapshot),
                            text: edited,
                            hunks: hunks.len(),
                            added,
                            removed,
                        })
                    }
                    .await;
                    match outcome {
                        Ok(file) => planned.push(file),
                        Err(error) => {
                            if !args.best_effort {
                                return Err(error);
                            }
                            failures.push((display, error.to_string()));
                        }
                    }
                }
                PatchOp::Add {
                    body, lines, bytes, ..
                } => {
                    // Create-only twice over: refuse an existing target while
                    // planning, and install exclusively when writing, so a file
                    // that appears in between cannot be clobbered.
                    let outcome = async {
                        match fs::symlink_metadata(&resolved).await {
                            Ok(_) => anyhow::bail!(
                                "{display}: '*** Add File' target already exists"
                            ),
                            Err(error)
                                if error.kind() == std::io::ErrorKind::NotFound => {}
                            Err(error) => return Err(error.into()),
                        }
                        if *bytes > self.config.max_write_bytes {
                            anyhow::bail!(
                                "{display}: added file would be {bytes} bytes, above the {} byte write limit",
                                self.config.max_write_bytes
                            );
                        }
                        budget.add(0, *bytes)?;
                        let mut text = String::with_capacity(*bytes);
                        for line in patch_lines(&args.patch[body.clone()]) {
                            let content = add_file_line(line)
                                .expect("validated add-file lines stay valid");
                            text.push_str(content);
                            text.push('\n');
                        }
                        debug_assert_eq!(text.len(), *bytes);
                        Ok(PlannedFile {
                            path: resolved,
                            display: display.clone(),
                            kind: PlanKind::Create,
                            text,
                            hunks: 0,
                            added: *lines,
                            removed: 0,
                        })
                    }
                    .await;
                    match outcome {
                        Ok(file) => planned.push(file),
                        Err(error) => {
                            if !args.best_effort {
                                return Err(error);
                            }
                            failures.push((display, error.to_string()));
                        }
                    }
                }
            }
        }

        // Verify every update before the first write. Atomic fails the whole
        // call; `best_effort` reports the file and keeps the rest.
        let mut ready: Vec<PlannedMutation> = Vec::with_capacity(planned.len());
        for file in planned {
            let verified = match file.kind {
                PlanKind::Update(snapshot) => match snapshot.verify(self, "patching").await {
                    Ok(verified) => Some(verified),
                    Err(error) if args.best_effort => {
                        failures.push((file.display.clone(), format!("{error}; skipped")));
                        continue;
                    }
                    Err(error) => return Err(anyhow!("{error}; nothing was written")),
                },
                PlanKind::Create => None,
            };
            let record = serde_json::json!({
                "path": file.display,
                "hunks": file.hunks,
                "added": file.added,
                "removed": file.removed,
            });
            ready.push(PlannedMutation {
                target: match verified {
                    Some(verified) => MutationTarget::Update(verified),
                    None => MutationTarget::Create(file.path),
                },
                display: file.display,
                bytes: file.text.into_bytes(),
                record,
            });
        }

        // Construct the reply budget before any staging or commit. The shared
        // receipt always fits, including dynamic best-effort failure strings;
        // an oversized reply must never turn committed mutations into a 502.
        let mode = if args.best_effort {
            "best_effort"
        } else {
            "strict"
        };
        let mut metadata = serde_json::Map::new();
        metadata.insert("mode".into(), serde_json::json!(mode));
        let receipt_plan = MutationReceipt::new(self.config.max_output_bytes, metadata)?;

        let outcome = stage_and_commit(self, "patch", args.best_effort, ready).await?;
        failures.extend(outcome.failures);
        let errors = failures
            .iter()
            .map(|(path, error)| {
                serde_json::json!({
                    "path": path,
                    "error": error,
                })
            })
            .collect::<Vec<_>>();
        Ok(receipt_plan.finish(&outcome.records, &errors))
    }
}

#[tool_router(router = "patch_router", vis = "pub(crate)")]
impl LocalMachine {
    #[tool(
        description = "Apply a Codex-grammar patch with per-file atomic replacement. Supports \"*** Update File:\" hunks and \"*** Add File:\" only; the Codex \"*** End of File\" marker restricts eligible context matches to EOF and can anchor an addition-only append; unanchored context-free hunks, Delete and Move are rejected. At most 32 files and 64 hunks per call. Hunks are located by their context lines (exact first, then trailing-whitespace-tolerant), an ambiguous match fails, and hunks apply top-down within a file. `*** Add File` is create-only and a path claimed by two sections is refused. Every file is planned and re-checked before any commit; each file replacement is atomic, but the multi-file commit sequence is not a transaction, so a commit failure can leave earlier files replaced. best_effort writes the files that planned cleanly and reports the rest. Returns a receipt with per-file hunks/added/removed and mode (strict or best_effort).",
        annotations(
            title = "Apply patch",
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = false,
            open_world_hint = false
        )
    )]
    async fn patch(&self, Parameters(args): Parameters<PatchArgs>) -> CallToolResult {
        match self.patch_impl(args).await {
            Ok(receipt) => ok_text(receipt),
            Err(error) => tool_error(error),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mcp::tools::test_support::test_machine;
    use serde_json::Value;

    fn patch_args(patch: &str) -> PatchArgs {
        PatchArgs {
            dir: dot(),
            patch: patch.to_owned(),
            best_effort: false,
        }
    }

    #[test]
    fn literal_terminal_cr_and_crlf_syntax_are_distinct() {
        // LF patch syntax: a CR in the content is literal, including a file
        // without a final LF. Equal replacement never erases that content.
        let literal =
            "*** Begin Patch\n*** Update File: f.txt\n@@\n-abc\r\n+abc\r\n*** End Patch\n";
        let patch = parse_patch(literal).unwrap();
        let PatchOp::Update { hunks, .. } = &patch[0] else {
            panic!("update")
        };
        for source in ["abc\r", "before\nabc\r", "before\r\nabc\r"] {
            let (got, ..) = apply_patch_hunks(source, hunks, "f.txt", 0, 1, 4096, 4096).unwrap();
            assert_eq!(got, source, "source {source:?}");
        }
        let patch = parse_patch(
            "*** Begin Patch\n*** Update File: f.txt\n@@\n-abc\r\n+abcd\r\n*** End Patch\n",
        )
        .unwrap();
        let PatchOp::Update { hunks, .. } = &patch[0] else {
            panic!("update")
        };
        let (got, ..) =
            apply_patch_hunks("before\nabc\r", hunks, "f.txt", 0, 1, 4096, 4096).unwrap();
        assert_eq!(got, "before\nabcd\r");

        // A CRLF *patch* has CRLF syntax on its header and its data lines.
        // Those syntactic CRs must not leak into an LF target; a doubled CR
        // still unambiguously expresses one literal CR before the delimiter.
        let crlf_patch =
            "*** Begin Patch\r\n*** Update File: f.txt\r\n@@\r\n-abc\r\n+ABC\r\n*** End Patch\r\n";
        let patch = parse_patch(crlf_patch).unwrap();
        let PatchOp::Update { hunks, .. } = &patch[0] else {
            panic!("update")
        };
        for (source, expected) in [("abc\n", "ABC\n"), ("abc\r\n", "ABC\r\n")] {
            let (got, ..) = apply_patch_hunks(source, hunks, "f.txt", 0, 1, 4096, 4096).unwrap();
            assert_eq!(got, expected);
        }
        let patch = parse_patch("*** Begin Patch\r\n*** Update File: f.txt\r\n@@\r\n-abc\r\n+ABC\r\r\n*** End Patch\r\n").unwrap();
        let PatchOp::Update { hunks, .. } = &patch[0] else {
            panic!("update")
        };
        let (got, ..) = apply_patch_hunks("abc\n", hunks, "f.txt", 0, 1, 4096, 4096).unwrap();
        assert_eq!(got, "ABC\r\r\n");
    }

    #[test]
    fn added_line_literal_cr_gets_an_independent_separator() {
        let patch = parse_patch(
            "*** Begin Patch\n*** Update File: f.txt\n@@\n-old\n+new\r\n+next\n*** End Patch\n",
        )
        .unwrap();
        let PatchOp::Update { hunks, .. } = &patch[0] else {
            panic!("update")
        };
        let (text, ..) = apply_patch_hunks("old\n", hunks, "f.txt", 0, 1, 4096, 4096).unwrap();
        assert_eq!(text, "new\r\r\nnext\n");
        assert!(apply_patch_hunks("old\n", hunks, "f.txt", 0, 1, text.len() - 1, 4096).is_err());
        assert_eq!(
            apply_patch_hunks("old\n", hunks, "f.txt", 0, 1, text.len(), 4096)
                .unwrap()
                .0,
            text
        );

        let patch = parse_patch(
            "*** Begin Patch\n*** Update File: f.txt\n@@\n-old\n+new\r\n*** End Patch\n",
        )
        .unwrap();
        let PatchOp::Update { hunks, .. } = &patch[0] else {
            panic!("update")
        };
        assert_eq!(
            apply_patch_hunks("old\n", hunks, "f.txt", 0, 1, 4096, 4096)
                .unwrap()
                .0,
            "new\r\r\n"
        );
        assert_eq!(
            apply_patch_hunks("old", hunks, "f.txt", 0, 1, 4096, 4096)
                .unwrap()
                .0,
            "new\r"
        );
    }

    #[test]
    fn eof_append_inherits_separators_without_rewriting_literal_content() {
        let patch = parse_patch("*** Begin Patch\n*** Update File: f.txt\n@@\n+one\n+two\n*** End of File\n*** End Patch\n").unwrap();
        let PatchOp::Update { hunks, .. } = &patch[0] else {
            panic!("update")
        };
        for (source, expected) in [
            ("a\r\nb", "a\r\nb\r\none\r\ntwo"),
            ("a\r\nb\r", "a\r\nb\r\r\none\r\ntwo"),
            ("a\r", "a\r\r\none\ntwo"),
            ("a\r\nb\r\n", "a\r\nb\r\none\r\ntwo\r\n"),
        ] {
            let (got, ..) = apply_patch_hunks(source, hunks, "f.txt", 0, 1, 4096, 4096).unwrap();
            assert_eq!(got, expected, "source {source:?}");
        }
    }

    #[test]
    fn crlf_adjustment_is_borrowed_and_comparable_between_hunks() {
        let added = WorkingLine::added("hello", true);
        assert_eq!(added.len(), 6);
        assert!(added.delimiter_cr);
        assert!(added.equals("hello"));
        assert!(added.equals_trimmed("hello"));
        assert!(added.contains("lo\r"));
        let mut output = String::new();
        added.append_to(&mut output);
        assert_eq!(output, "hello\r");
        let normalized = WorkingLine::added("hello\r", false);
        assert!(normalized.equals("hello\r"));
        assert!(normalized.delimiter_cr);
        assert_eq!(normalized.len(), "hello\r\r".len());
        let patch = parse_patch("*** Begin Patch\n*** Update File: a.txt\n@@\n-old\n+new\n@@\n-last\n+LAST\n*** End Patch\n").unwrap();
        let PatchOp::Update { hunks, .. } = &patch[0] else {
            panic!("expected update")
        };
        let (text, added, removed) =
            apply_patch_hunks("old\r\nlast\r\n", hunks, "a.txt", 0, 2, 1024, 4096).unwrap();
        assert_eq!((added, removed), (2, 2));
        assert_eq!(text, "new\r\nLAST\r\n");
    }

    #[test]
    fn borrowed_line_index_handles_dense_source_independent_of_write_limit() {
        let mut source = "x\n".repeat(300_000);
        source.push_str("last\n");
        let patch = parse_patch(
            "*** Begin Patch\n*** Update File: dense.txt\n@@\n-last\n+LAST\n*** End Patch\n",
        )
        .unwrap();
        let PatchOp::Update { hunks, .. } = &patch[0] else {
            panic!("expected update")
        };
        let (result, added, removed) = apply_patch_hunks(
            &source,
            hunks,
            "dense.txt",
            0,
            1,
            1024 * 1024,
            super::super::MAX_MUTATION_PLAN_BYTES - source.len(),
        )
        .unwrap();
        assert_eq!(added, 1);
        assert_eq!(removed, 1);
        assert!(result.ends_with("LAST\n"));
        assert_eq!(result.len(), source.len());
    }

    #[test]
    fn final_hunk_avoids_an_unnecessary_replacement_index() {
        let input = format!(
            "*** Begin Patch\n*** Update File: file.txt\n@@\n-old\n{}*** End Patch\n",
            "+x\n".repeat(250)
        );
        let patch = parse_patch(&input).unwrap();
        let PatchOp::Update { hunks, .. } = &patch[0] else {
            panic!("expected update")
        };
        let (output, added, removed) =
            apply_patch_hunks("old\n", hunks, "file.txt", 0, 1, 4096, 1024).unwrap();
        assert_eq!(output, "x\n".repeat(250));
        assert_eq!((added, removed), (250, 1));
        let error = apply_patch_hunks("old\n", hunks, "file.txt", 0, 1, 4096, 250)
            .unwrap_err()
            .to_string();
        assert!(error.contains("working buffers"), "{error}");
    }

    #[test]
    fn dense_early_hunk_uses_in_place_index_splice() {
        let input = format!(
            "*** Begin Patch\n*** Update File: file.txt\n@@\n anchor\n{}@@\n-end\n+END\n*** End Patch\n",
            "+x\n".repeat(350_000)
        );
        let patch = parse_patch(&input).unwrap();
        let PatchOp::Update { hunks, .. } = &patch[0] else {
            panic!("expected update")
        };
        let (output, added, removed) = apply_patch_hunks(
            "anchor\nend\n",
            hunks,
            "file.txt",
            0,
            2,
            2 * 1024 * 1024,
            16 * 1024 * 1024,
        )
        .unwrap();
        assert_eq!((added, removed), (350_001, 1));
        assert_eq!(output.len(), 700_011);
        assert!(output.starts_with("anchor\nx\nx\n"));
        assert!(output.ends_with("x\nEND\n"));
    }

    #[test]
    fn adjacent_replacements_preserve_each_removed_line_separator() {
        let source = "alpha\r\nbeta\nomega\n";
        for additions_first in [false, true] {
            let group = if additions_first {
                "+ALPHA\n+BETA\n-alpha\n-beta\n"
            } else {
                "-alpha\n-beta\n+ALPHA\n+BETA\n"
            };
            for later_hunk in [false, true] {
                let extra = if later_hunk {
                    "@@\n-omega\n+OMEGA\n"
                } else {
                    ""
                };
                let patch = format!(
                    "*** Begin Patch\n*** Update File: f.txt\n@@\n{group}{extra}*** End Patch\n"
                );
                let parsed = parse_patch(&patch).unwrap();
                let PatchOp::Update { hunks, .. } = &parsed[0] else {
                    panic!("update")
                };
                let actual = apply_patch_hunks(source, hunks, "f.txt", 0, hunks.len(), 4096, 8192)
                    .unwrap()
                    .0;
                let expected = if later_hunk {
                    "ALPHA\r\nBETA\nOMEGA\n"
                } else {
                    "ALPHA\r\nBETA\nomega\n"
                };
                assert_eq!(
                    actual, expected,
                    "additions_first={additions_first}, later_hunk={later_hunk}"
                );
            }
        }
    }

    #[test]
    fn unequal_replacement_groups_inherit_last_removed_separator_for_excess_additions() {
        let source = "a\r\nb\nc\n";
        let input = "*** Begin Patch\n*** Update File: f.txt\n@@\n-a\n-b\n+ONE\n+TWO\n+THREE\n*** End Patch\n";
        let parsed = parse_patch(input).unwrap();
        let PatchOp::Update { hunks, .. } = &parsed[0] else {
            panic!("update")
        };
        assert_eq!(
            apply_patch_hunks(source, hunks, "f.txt", 0, 1, 4096, 4096)
                .unwrap()
                .0,
            "ONE\r\nTWO\nTHREE\nc\n"
        );
    }

    #[test]
    fn eof_append_preserves_literal_cr_as_content_across_both_hunk_paths() {
        let append = "+next\n*** End of File\n";
        let cases = [
            ("original\r", "original\r\r\nnext"),
            ("head\noriginal\r", "head\noriginal\r\r\nnext"),
            ("head\r\noriginal\r", "head\r\noriginal\r\r\nnext"),
            ("original", "original\nnext"),
            ("head\r\noriginal", "head\r\noriginal\r\nnext"),
        ];
        for (source, expected) in cases {
            let input =
                format!("*** Begin Patch\n*** Update File: f.txt\n@@\n{append}*** End Patch\n");
            let parsed = parse_patch(&input).unwrap();
            let PatchOp::Update { hunks, .. } = &parsed[0] else {
                panic!("update")
            };
            let (result, added, removed) =
                apply_patch_hunks(source, hunks, "f.txt", 0, 1, expected.len(), 4096).unwrap();
            assert_eq!(result, expected, "source={source:?}");
            assert_eq!((added, removed), (1, 0));
            // Preflight rejects the byte immediately below the exact result;
            // no unsafe allocation or output can occur on a too-small plan.
            if expected.len() > 1 {
                assert!(
                    apply_patch_hunks(source, hunks, "f.txt", 0, 1, expected.len() - 1, 4096)
                        .is_err()
                );
            }
            // First a matching no-op hunk, then EOF append. The final hunk's
            // source may have passed through earlier planning unaffected.
            if source.starts_with("head") {
                let multi = format!(
                    "*** Begin Patch\n*** Update File: f.txt\n@@\n head\n@@\n{append}*** End Patch\n"
                );
                let parsed = parse_patch(&multi).unwrap();
                let PatchOp::Update { hunks, .. } = &parsed[0] else {
                    panic!("update")
                };
                let got =
                    apply_patch_hunks(source, hunks, "f.txt", 0, hunks.len(), expected.len(), 4096)
                        .unwrap()
                        .0;
                assert_eq!(got, expected, "multi-hunk source={source:?}");
            }
        }
    }

    #[test]
    fn literal_cr_on_both_sides_of_an_eof_append_preserves_all_content() {
        let patch = parse_patch("*** Begin Patch\n*** Update File: f.txt\n@@\n+one\r\n+two\r\n*** End of File\n*** End Patch\n").unwrap();
        let PatchOp::Update { hunks, .. } = &patch[0] else {
            panic!("update")
        };
        let expected = "original\r\r\none\r\r\ntwo\r";
        let (got, added, removed) =
            apply_patch_hunks("original\r", hunks, "f.txt", 0, 1, expected.len(), 4096).unwrap();
        assert_eq!((added, removed), (2, 0));
        assert_eq!(got, expected);
        assert!(
            apply_patch_hunks("original\r", hunks, "f.txt", 0, 1, expected.len() - 1, 4096)
                .is_err()
        );
    }

    #[test]
    fn transitioning_to_unterminated_last_line_updates_line_metadata() {
        let mut crlf = WorkingLine::source_line("foo\r", true);
        assert!(crlf.drop_terminal_delimiter_cr());
        assert!(!crlf.terminated);
        assert_eq!(crlf.text, "foo");
        let mut lf = WorkingLine::source_line("foo", true);
        assert!(!lf.drop_terminal_delimiter_cr());
        assert!(!lf.terminated);
    }

    #[test]
    fn separator_selection_uses_replaced_lines_not_preceding_context() {
        let scenarios = [
            ("context\r\ntarget\nend\n", "context\r\nchanged\nend\n"),
            ("context\ntarget\r\nend\n", "context\nchanged\r\nend\n"),
            ("context\r\ntarget\nend", "context\r\nchanged\nend"),
        ];
        for (source, expected) in scenarios {
            for with_context in [false, true] {
                let context = if with_context { " context\n" } else { "" };
                let input = format!(
                    "*** Begin Patch\n*** Update File: f.txt\n@@\n{context}-target\n+changed\n*** End Patch\n"
                );
                let parsed = parse_patch(&input).unwrap();
                let PatchOp::Update { hunks, .. } = &parsed[0] else {
                    panic!("update")
                };
                let (actual, added, removed) =
                    apply_patch_hunks(source, hunks, "f.txt", 0, 1, 4096, 4096).unwrap();
                assert_eq!(
                    actual, expected,
                    "source={source:?}, context={with_context}"
                );
                assert_eq!((added, removed), (1, 1));
            }
        }
    }

    #[test]
    fn group_local_styles_remain_correct_in_both_multi_hunk_and_final_hunk_paths() {
        let original = "anchor\r\nfirst\ncontext\ntarget\r\nlast\n";
        // Both groups are in one hunk: the first replacement is LF and the
        // second CRLF, even though their context lines use the opposite style.
        let combined = parse_patch("*** Begin Patch\n*** Update File: f.txt\n@@\n anchor\n-first\n+FIRST\n context\n-target\n+TARGET\n*** End Patch\n").unwrap();
        let PatchOp::Update { hunks, .. } = &combined[0] else {
            panic!("update")
        };
        let expected = "anchor\r\nFIRST\ncontext\nTARGET\r\nlast\n";
        assert_eq!(
            apply_patch_hunks(original, hunks, "f.txt", 0, 1, 4096, 8192)
                .unwrap()
                .0,
            expected
        );

        // The same replacements with a later hunk exercise the in-place
        // index path before the shared final-output assembler takes over.
        let multiple = parse_patch("*** Begin Patch\n*** Update File: f.txt\n@@\n anchor\n-first\n+FIRST\n context\n-target\n+TARGET\n@@\n-last\n+LAST\n*** End Patch\n").unwrap();
        let PatchOp::Update { hunks, .. } = &multiple[0] else {
            panic!("update")
        };
        assert_eq!(
            apply_patch_hunks(original, hunks, "f.txt", 0, 2, 4096, 8192)
                .unwrap()
                .0,
            "anchor\r\nFIRST\ncontext\nTARGET\r\nLAST\n"
        );
    }

    #[test]
    fn insertion_groups_use_actual_insertion_site_style() {
        let original = "context\r\nnext\nend\n";
        let patch = parse_patch("*** Begin Patch\n*** Update File: f.txt\n@@\n context\n+insert\n next\n*** End Patch\n").unwrap();
        let PatchOp::Update { hunks, .. } = &patch[0] else {
            panic!("update")
        };
        assert_eq!(
            apply_patch_hunks(original, hunks, "f.txt", 0, 1, 4096, 4096)
                .unwrap()
                .0,
            "context\r\ninsert\nnext\nend\n"
        );
    }

    #[test]
    fn trailing_pure_insertion_style_does_not_depend_on_optional_context() {
        let original = "anchor\nnext\r\nend\n";
        let expected = "anchor\ninsert\r\nnext\r\nend\n";
        for trailing_context in ["", " next\n"] {
            let input = format!(
                "*** Begin Patch\n*** Update File: f.txt\n@@\n anchor\n+insert\n{trailing_context}*** End Patch\n"
            );
            let patch = parse_patch(&input).unwrap();
            let PatchOp::Update { hunks, .. } = &patch[0] else {
                panic!("update")
            };
            assert_eq!(
                apply_patch_hunks(original, hunks, "f.txt", 0, 1, 4096, 4096)
                    .unwrap()
                    .0,
                expected,
                "trailing context={trailing_context:?}"
            );
        }

        // The same boundary lookup must survive the in-place path used when a
        // later hunk still needs the working line index.
        let input = "*** Begin Patch\n*** Update File: f.txt\n@@\n anchor\n+insert\n@@\n-end\n+END\n*** End Patch\n";
        let patch = parse_patch(input).unwrap();
        let PatchOp::Update { hunks, .. } = &patch[0] else {
            panic!("update")
        };
        assert_eq!(
            apply_patch_hunks(original, hunks, "f.txt", 0, 2, 4096, 8192)
                .unwrap()
                .0,
            "anchor\ninsert\r\nnext\r\nEND\n"
        );
    }

    #[test]
    fn eof_constraints_select_only_eligible_context_candidates() {
        let input = "*** Begin Patch\n*** Update File: file.txt\n@@\n-foo\n+BAR\n*** End of File\n*** End Patch\n";
        let patch = parse_patch(input).unwrap();
        let PatchOp::Update { hunks, .. } = &patch[0] else {
            panic!("expected update")
        };
        for (source, expected) in [
            ("foo\nfoo\n", "foo\nBAR\n"),
            ("foo\nfoo  \n", "foo\nBAR\n"),
            ("foo  \nfoo\n", "foo  \nBAR\n"),
            ("foo\r\nfoo\r\n", "foo\r\nBAR\r\n"),
        ] {
            let (actual, added, removed) =
                apply_patch_hunks(source, hunks, "file.txt", 0, 1, 4096, 4096).unwrap();
            assert_eq!(actual, expected, "source={source:?}");
            assert_eq!((added, removed), (1, 1));
        }
        let error = apply_patch_hunks("foo\nbar\n", hunks, "file.txt", 0, 1, 4096, 4096)
            .unwrap_err()
            .to_string();
        assert!(error.contains("requires the hunk to end"), "{error}");
    }

    #[test]
    fn eof_only_insertions_append_without_inventing_source_context() {
        let patch = parse_patch("*** Begin Patch\n*** Update File: file.txt\n@@\n+hello\n*** End of File\n*** End Patch\n").unwrap();
        let PatchOp::Update { hunks, .. } = &patch[0] else {
            panic!("expected update")
        };
        for (source, expected) in [
            ("", "hello"),
            ("foo", "foo\nhello"),
            ("foo\n", "foo\nhello\n"),
            ("foo\r\n", "foo\r\nhello\r\n"),
            ("foo\r\nbar", "foo\r\nbar\r\nhello"),
        ] {
            let (actual, added, removed) =
                apply_patch_hunks(source, hunks, "file.txt", 0, 1, 4096, 4096).unwrap();
            assert_eq!(actual, expected, "source={source:?}");
            assert_eq!((added, removed), (1, 0));
        }
        let unanchored =
            parse_patch("*** Begin Patch\n*** Update File: file.txt\n@@\n+hello\n*** End Patch\n")
                .unwrap();
        let PatchOp::Update { hunks, .. } = &unanchored[0] else {
            panic!("expected update")
        };
        let error = apply_patch_hunks("foo\n", hunks, "file.txt", 0, 1, 4096, 4096)
            .unwrap_err()
            .to_string();
        assert!(error.contains("requires '*** End of File'"), "{error}");
    }

    #[test]
    fn parsed_patch_borrows_paths_and_content_from_the_input_buffer() {
        let input = "*** Begin Patch\n*** Add File: dir/new.txt\n+hello\n+world\n*** Update File: dir/old.txt\n@@ fn test\n-before\n+after\n*** End Patch\n";
        let ops = parse_patch(input).unwrap();
        let inside = |value: &str| {
            let start = input.as_ptr() as usize;
            let end = start + input.len();
            let addr = value.as_ptr() as usize;
            addr >= start && addr + value.len() <= end
        };
        match &ops[0] {
            PatchOp::Add {
                path,
                body,
                lines,
                bytes,
            } => {
                assert!(inside(path));
                assert_eq!(*lines, 2);
                assert_eq!(*bytes, "hello\nworld\n".len());
                assert_eq!(&input[body.clone()], "+hello\n+world\n");
            }
            _ => panic!("expected add"),
        }
        match &ops[1] {
            PatchOp::Update { path, hunks } => {
                assert!(inside(path));
                assert!(inside(hunks[0].anchor.unwrap()));
                assert!(hunks[0].lines().all(|line| match line {
                    PatchLine::Context(s) | PatchLine::Removed(s) | PatchLine::Added(s) =>
                        inside(s),
                }));
                assert_eq!(hunks[0].body, "-before\n+after\n");
            }
            _ => panic!("expected update"),
        }
    }

    #[test]
    fn add_file_blank_forms_share_exact_byte_preflight_and_output() {
        let forms = [
            "*** Begin Patch\r\n*** Add File: f.txt\r\n+first\r\n\r\n+last\r\n*** End Patch\r\n",
            "*** Begin Patch\r\n*** Add File: f.txt\r\n+first\r\n+\r\n+last\r\n*** End Patch\r\n",
            "*** Begin Patch\r\n*** Add File: f.txt\r\n+first\r\n  \r\n+last\r\n*** End Patch\r\n",
        ];
        for input in forms {
            let parsed = parse_patch(input).unwrap();
            let PatchOp::Add {
                body, lines, bytes, ..
            } = &parsed[0]
            else {
                panic!("add")
            };
            let actual = patch_lines(&input[body.clone()])
                .map(|line| format!("{}\n", add_file_line(line).unwrap()))
                .collect::<String>();
            assert_eq!(actual, "first\r\n\r\nlast\r\n");
            assert_eq!(*bytes, actual.len());
            assert_eq!(*lines, 3);
        }
        for input in ["", "\r", " ", "  \t\r", "+", "+\r", "+a\r"] {
            assert!(add_file_line(input).is_some(), "{input:?}");
        }
        assert_eq!(add_file_line("-invalid"), None);
    }

    #[test]
    fn add_body_range_preserves_crlf_and_blank_line_semantics() {
        let input =
            "*** Begin Patch\r\n*** Add File: path.txt\r\n+hello\r\n\r\n+\r\n*** End Patch\r\n";
        let ops = parse_patch(input).unwrap();
        let PatchOp::Add {
            body, lines, bytes, ..
        } = &ops[0]
        else {
            panic!("expected add")
        };
        assert_eq!(*lines, 3);
        let actual = patch_lines(&input[body.clone()])
            .map(|line| format!("{}\n", add_file_line(line).unwrap()))
            .collect::<String>();
        assert_eq!(actual, "hello\r\n\r\n\r\n");
        assert_eq!(*bytes, actual.len());
    }

    #[test]
    fn parser_rejects_excess_sections_before_processing_later_invalid_lines() {
        let mut patch = String::from("*** Begin Patch\n");
        for index in 0..=MAX_PATCH_FILES {
            patch.push_str(&format!("*** Add File: f{index}.txt\n+x\n"));
        }
        patch.push_str("*** UNKNOWN INVALID DIRECTIVE\n");
        let error = parse_patch(&patch).unwrap_err().to_string();
        assert!(error.contains("file limit"), "{error}");
    }

    #[test]
    fn dense_update_additions_remain_a_borrowed_hunk_body() {
        let additions = 100_000;
        let mut input = String::from("*** Begin Patch\n*** Update File: f.txt\n@@\n-old\n");
        input.push_str(&"+x\n".repeat(additions));
        input.push_str("*** End Patch\n");
        let patch = parse_patch(&input).unwrap();
        let PatchOp::Update { hunks, .. } = &patch[0] else {
            panic!("update")
        };
        assert_eq!(hunks.len(), 1);
        assert_eq!(hunks[0].lines().count(), additions + 1);
        assert_eq!(
            hunks[0].body,
            &input[input.find("-old\n").unwrap()..input.find("*** End Patch").unwrap()]
        );
    }

    #[tokio::test]
    async fn patch_receipt_bounds_post_commit_result_and_preserves_counts() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let mut dir = root.clone();
        for _ in 0..4 {
            dir = dir.join("x".repeat(205));
            std::fs::create_dir(&dir).unwrap();
        }
        let mut input = String::from("*** Begin Patch\n");
        for file in 0..MAX_PATCH_FILES {
            let relative = dir
                .strip_prefix(&root)
                .unwrap()
                .join(format!("new-{file:03}.txt"));
            input.push_str(&format!("*** Add File: {}\n+hello\n", relative.display()));
        }
        input.push_str("*** End Patch\n");
        let mut machine = test_machine(root);
        machine.config.max_write_bytes = 64 * 1024;
        machine.config.max_output_bytes = 1024;
        let receipt = machine.patch_impl(patch_args(&input)).await.unwrap();
        assert!(receipt.len() <= 1024, "receipt={} bytes", receipt.len());
        let json: Value = serde_json::from_str(&receipt).unwrap();
        let displayed = json["files"].as_array().unwrap().len();
        assert_eq!(json["mode"], "strict");
        assert_eq!(json["truncated"], true);
        assert_eq!(
            json["omitted_files"].as_u64().unwrap() as usize + displayed,
            MAX_PATCH_FILES
        );
        assert_eq!(std::fs::read_dir(dir).unwrap().count(), MAX_PATCH_FILES);
    }

    #[tokio::test]
    async fn patch_aggregate_budget_rejects_before_any_strict_commit() {
        use super::super::MAX_MUTATION_PLAN_BYTES;
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let mut patch = String::from("*** Begin Patch\n");
        let file_bytes = MAX_MUTATION_PLAN_BYTES / 5;
        for file in 0..3 {
            let path = root.join(format!("large-{file}.txt"));
            let mut bytes = Vec::with_capacity(file_bytes);
            bytes.extend_from_slice(b"before\n");
            bytes.resize(file_bytes, b'x');
            std::fs::write(path, bytes).unwrap();
            patch.push_str(&format!(
                "*** Update File: large-{file}.txt\n@@\n-before\n+after\n"
            ));
        }
        patch.push_str("*** End Patch\n");
        let mut machine = test_machine(root.clone());
        machine.config.max_read_bytes = file_bytes + 1024;
        machine.config.max_write_bytes = file_bytes + 1024;
        let error = machine
            .patch_impl(patch_args(&patch))
            .await
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("aggregate") || error.contains("budget"),
            "{error}"
        );
        for file in 0..3 {
            assert!(
                std::fs::read(root.join(format!("large-{file}.txt")))
                    .unwrap()
                    .starts_with(b"before\n")
            );
        }
    }

    #[tokio::test]
    async fn patch_applies_update_and_add_sections_atomically() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        std::fs::create_dir_all(root.join("crates/x/src")).unwrap();
        std::fs::write(
            root.join("crates/x/src/lib.rs"),
            "fn main() {\n    one();\n    two();\n}\n",
        )
        .unwrap();
        let machine = test_machine(root.clone());

        let patch = r#"*** Begin Patch
*** Update File: crates/x/src/lib.rs
@@ fn main() {
 fn main() {
-    one();
+    uno();
*** Add File: crates/new/src/lib.rs
+pub fn fresh() {}
+
*** End Patch"#;
        let receipt = machine.patch_impl(patch_args(patch)).await.unwrap();
        let receipt: Value = serde_json::from_str(&receipt).unwrap();
        assert_eq!(receipt["mode"], "strict");
        assert_eq!(
            receipt["files"],
            serde_json::json!([
                {"path": "crates/x/src/lib.rs", "hunks": 1, "added": 1, "removed": 1},
                {"path": "crates/new/src/lib.rs", "hunks": 0, "added": 2, "removed": 0}
            ])
        );
        assert_eq!(
            std::fs::read_to_string(root.join("crates/x/src/lib.rs")).unwrap(),
            "fn main() {\n    uno();\n    two();\n}\n"
        );
        assert_eq!(
            std::fs::read_to_string(root.join("crates/new/src/lib.rs")).unwrap(),
            "pub fn fresh() {}\n\n"
        );
    }

    #[tokio::test]
    async fn patch_parser_rejects_missing_header_terminator_and_directives() {
        let machine = test_machine(tempfile::tempdir().unwrap().path().canonicalize().unwrap());

        let error = machine
            .patch_impl(patch_args("*** Update File: a.txt\n*** End Patch"))
            .await
            .unwrap_err()
            .to_string();
        assert_eq!(error, "patch: missing '*** Begin Patch' header");

        let error = machine
            .patch_impl(patch_args("*** Begin Patch\n*** Update File: a.txt"))
            .await
            .unwrap_err()
            .to_string();
        assert_eq!(error, "patch: missing '*** End Patch' terminator");

        let error = machine
            .patch_impl(patch_args(
                "*** Begin Patch\n*** Delete File: a.txt\n*** End Patch",
            ))
            .await
            .unwrap_err()
            .to_string();
        assert_eq!(
            error,
            "patch: line 2: unsupported directive '*** Delete File'"
        );

        let error = machine
            .patch_impl(patch_args(
                "*** Begin Patch\n*** Update File: a.txt\n@@\n x\n*** Move to: b.txt\n*** End Patch",
            ))
            .await
            .unwrap_err()
            .to_string();
        assert_eq!(error, "patch: line 5: unsupported directive '*** Move to'");

        let error = machine
            .patch_impl(patch_args(
                "*** Begin Patch\n*** Update File: a.txt\n@@\n x\nstray text\n*** End Patch",
            ))
            .await
            .unwrap_err()
            .to_string();
        assert_eq!(error, "patch: line 5: unexpected text 'stray text'");

        let error = machine
            .patch_impl(patch_args(
                "*** Begin Patch\n*** Update File: a.txt\n@@\n-old\n+new\n*** End of File\n-danger\n+safe\n*** End Patch",
            ))
            .await
            .unwrap_err()
            .to_string();
        assert_eq!(
            error,
            "patch: line 7: hunk data may not follow '*** End of File'"
        );

        let error = machine
            .patch_impl(patch_args(
                "*** Begin Patch\n*** Update File: a.txt\n@@\n-old\n+new\n*** End of File\n*** End of File\n*** End Patch",
            ))
            .await
            .unwrap_err()
            .to_string();
        assert_eq!(error, "patch: line 7: duplicate '*** End of File' marker");
    }

    #[tokio::test]
    async fn patch_hunk_location_reports_missing_and_ambiguous_context() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        std::fs::write(root.join("a.txt"), "same\nsame\nsame\n").unwrap();
        std::fs::write(root.join("b.txt"), "alpha\nbeta\n").unwrap();
        let machine = test_machine(root.clone());

        let error = machine
            .patch_impl(patch_args(
                "*** Begin Patch\n*** Update File: b.txt\n@@\n-nope\n+yep\n*** End Patch",
            ))
            .await
            .unwrap_err()
            .to_string();
        assert_eq!(error, "hunk 1/1 (b.txt): context not found near line 1");

        let error = machine
            .patch_impl(patch_args(
                "*** Begin Patch\n*** Update File: a.txt\n@@\n-same\n+other\n*** End Patch",
            ))
            .await
            .unwrap_err()
            .to_string();
        assert_eq!(
            error,
            "hunk 1/1 (a.txt): context matches 3 locations (lines 1, 2, 3)"
        );

        // Nothing was written by either failure.
        assert_eq!(
            std::fs::read_to_string(root.join("a.txt")).unwrap(),
            "same\nsame\nsame\n"
        );
    }

    #[test]
    fn sequence_matching_agrees_with_brute_force_on_crlf_and_whitespace() {
        let alphabet = ["x", "x ", "x\r", "é", "é\r", "\t", "🙂", "", "y"];
        for seed in 0_u32..420 {
            let mut state = seed + 1;
            let mut next = || {
                state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                state
            };
            let source_len = (next() % 65) as usize;
            let pattern_len = (next() % 12 + 1) as usize;
            let source = (0..source_len)
                .map(|_| {
                    let text = alphabet[(next() as usize) % alphabet.len()];
                    WorkingLine::added(text, next() % 3 == 0)
                })
                .collect::<Vec<_>>();
            let pattern = (0..pattern_len)
                .map(|_| alphabet[(next() as usize) % alphabet.len()])
                .collect::<Vec<_>>();
            let start = (next() % 72) as usize;
            for trimmed in [false, true] {
                let mut actual = Vec::new();
                super::super::matching::scan_sequence(
                    &pattern,
                    &source,
                    start,
                    |a, b| {
                        if trimmed {
                            a.trim_end() == b.trim_end()
                        } else {
                            a == b
                        }
                    },
                    |line, pat| {
                        if trimmed {
                            line.equals_trimmed(pat)
                        } else {
                            line.equals(pat)
                        }
                    },
                    |at| actual.push(at),
                );
                let mut expected = Vec::new();
                if start < source.len() && pattern.len() <= source.len() - start {
                    for at in start..=source.len() - pattern.len() {
                        if source[at..at + pattern.len()]
                            .iter()
                            .zip(&pattern)
                            .all(|(line, pat)| {
                                if trimmed {
                                    line.equals_trimmed(pat)
                                } else {
                                    line.equals(pat)
                                }
                            })
                        {
                            expected.push(at);
                        }
                    }
                }
                assert_eq!(actual, expected, "seed={seed} trimmed={trimmed}");
            }
        }
    }

    #[test]
    fn unterminated_crlf_sources_preserve_neighboring_style_without_leaking_cr() {
        let delete_last =
            parse_patch("*** Begin Patch\n*** Update File: file.txt\n@@\n-b\n*** End Patch\n")
                .unwrap();
        let PatchOp::Update { hunks, .. } = &delete_last[0] else {
            panic!("expected update")
        };
        let (actual, ..) =
            apply_patch_hunks("a\r\nb", hunks, "file.txt", 0, 1, 1024, 4096).unwrap();
        assert_eq!(actual, "a", "removed terminal LF must not strand a CR");
        let (actual, ..) = apply_patch_hunks("a\r\nb", hunks, "file.txt", 0, 1, 1, 4096).unwrap();
        assert_eq!(
            actual, "a",
            "the exact normalized byte ceiling must be accepted"
        );
        let (actual, ..) = apply_patch_hunks("a\nb", hunks, "file.txt", 0, 1, 1024, 4096).unwrap();
        assert_eq!(actual, "a");

        let replace_last = parse_patch(
            "*** Begin Patch\n*** Update File: file.txt\n@@\n-b\n+one\n+two\n*** End Patch\n",
        )
        .unwrap();
        let PatchOp::Update { hunks, .. } = &replace_last[0] else {
            panic!("expected update")
        };
        let (actual, ..) =
            apply_patch_hunks("a\r\nb", hunks, "file.txt", 0, 1, 1024, 4096).unwrap();
        assert_eq!(actual, "a\r\none\r\ntwo");
        let expected = "a\r\none\r\ntwo";
        let (actual, ..) =
            apply_patch_hunks("a\r\nb", hunks, "file.txt", 0, 1, expected.len(), 4096).unwrap();
        assert_eq!(actual, expected);
        let (actual, ..) = apply_patch_hunks("a\nb", hunks, "file.txt", 0, 1, 1024, 4096).unwrap();
        assert_eq!(actual, "a\none\ntwo");
        let (actual, ..) = apply_patch_hunks("b", hunks, "file.txt", 0, 1, 1024, 4096).unwrap();
        assert_eq!(
            actual, "one\ntwo",
            "a file without any line terminator defaults to LF"
        );

        // A literal trailing CR with no LF is part of the source text, not
        // a delimiter to strip during a context-only hunk.
        let unchanged =
            parse_patch("*** Begin Patch\n*** Update File: file.txt\n@@\n a\n*** End Patch\n")
                .unwrap();
        let PatchOp::Update { hunks, .. } = &unchanged[0] else {
            panic!("expected update")
        };
        let (actual, ..) = apply_patch_hunks("a\r", hunks, "file.txt", 0, 1, 1024, 4096).unwrap();
        assert_eq!(actual, "a\r");
    }

    #[test]
    fn matcher_scratch_is_rejected_before_allocating_the_context_tables() {
        let input = format!(
            "*** Begin Patch\n*** Update File: f.txt\n@@\n{}*** End Patch\n",
            " x\n".repeat(10)
        );
        let patch = parse_patch(&input).unwrap();
        let PatchOp::Update { hunks, .. } = &patch[0] else {
            panic!("expected update")
        };
        let source = "x\n".repeat(10);
        let line_index = 10 * std::mem::size_of::<WorkingLine<'_>>();
        let matcher_index = 10 * (std::mem::size_of::<&str>() + std::mem::size_of::<usize>());
        let error = apply_patch_hunks(
            &source,
            hunks,
            "f.txt",
            0,
            1,
            1024,
            line_index + matcher_index - 1,
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("context matching"), "{error}");
        let (actual, ..) = apply_patch_hunks(&source, hunks, "f.txt", 0, 1, 1024, 4096).unwrap();
        assert_eq!(actual, source);
    }

    #[test]
    fn trailing_terminator_never_matches_an_invented_blank_line() {
        let input = "*** Begin Patch\n*** Update File: file.txt\n@@\n \n+tail\n*** End Patch\n";
        let patch = parse_patch(input).unwrap();
        let PatchOp::Update { hunks, .. } = &patch[0] else {
            panic!("expected update")
        };
        for original in ["", "a", "a\n", "a\r\n"] {
            let error = apply_patch_hunks(original, hunks, "file.txt", 0, 1, 1024, 4096)
                .unwrap_err()
                .to_string();
            assert!(error.contains("context not found"), "{original:?}: {error}");
        }

        for (original, expected) in [
            ("\n", "\ntail\n"),
            ("a\n\n", "a\n\ntail\n"),
            ("a\r\n\r\n", "a\r\n\r\ntail\r\n"),
        ] {
            let (actual, added, removed) =
                apply_patch_hunks(original, hunks, "file.txt", 0, 1, 1024, 4096).unwrap();
            assert_eq!(actual, expected, "original={original:?}");
            assert_eq!((added, removed), (1, 0));
        }
    }

    #[test]
    fn trailing_terminators_survive_updates_but_not_deletion_of_every_line() {
        let replace =
            parse_patch("*** Begin Patch\n*** Update File: f.txt\n@@\n-a\n+A\n*** End Patch\n")
                .unwrap();
        let PatchOp::Update { hunks, .. } = &replace[0] else {
            panic!("expected update")
        };
        for (original, expected) in [
            ("a", "A"),
            ("a\n", "A\n"),
            ("a\n\n", "A\n\n"),
            ("a\r\n", "A\r\n"),
            ("a\r\n\r\n", "A\r\n\r\n"),
        ] {
            let (actual, ..) =
                apply_patch_hunks(original, hunks, "f.txt", 0, 1, 1024, 4096).unwrap();
            assert_eq!(actual, expected, "original={original:?}");
        }

        let delete =
            parse_patch("*** Begin Patch\n*** Update File: f.txt\n@@\n-a\n*** End Patch\n")
                .unwrap();
        let PatchOp::Update { hunks, .. } = &delete[0] else {
            panic!("expected update")
        };
        for original in ["a", "a\n", "a\r\n"] {
            let (actual, ..) =
                apply_patch_hunks(original, hunks, "f.txt", 0, 1, 1024, 4096).unwrap();
            assert_eq!(actual, "", "original={original:?}");
        }
        let delete_blank =
            parse_patch("*** Begin Patch\n*** Update File: f.txt\n@@\n-\n*** End Patch\n").unwrap();
        let PatchOp::Update { hunks, .. } = &delete_blank[0] else {
            panic!("expected update")
        };
        let (actual, ..) = apply_patch_hunks("\n", hunks, "f.txt", 0, 1, 1024, 4096).unwrap();
        assert_eq!(actual, "");
    }

    #[test]
    fn end_of_file_marker_refers_to_last_real_line() {
        let input = "*** Begin Patch\n*** Update File: file.txt\n@@\n \n+tail\n*** End of File\n*** End Patch\n";
        let patch = parse_patch(input).unwrap();
        let PatchOp::Update { hunks, .. } = &patch[0] else {
            panic!("expected update")
        };
        let (actual, ..) = apply_patch_hunks("a\n\n", hunks, "file.txt", 0, 1, 1024, 4096).unwrap();
        assert_eq!(actual, "a\n\ntail\n");
        assert!(apply_patch_hunks("a\n", hunks, "file.txt", 0, 1, 1024, 4096).is_err());
    }

    #[test]
    fn context_only_hunks_leave_source_unchanged_without_replacing_line_index() {
        let original = "x\n".repeat(100_000);
        let input = format!(
            "*** Begin Patch\n*** Update File: file.txt\n@@\n{}*** End Patch\n",
            " x\n".repeat(100_000)
        );
        let patch = parse_patch(&input).unwrap();
        let PatchOp::Update { hunks, .. } = &patch[0] else {
            panic!("expected update")
        };
        let (actual, added, removed) = apply_patch_hunks(
            &original,
            hunks,
            "file.txt",
            0,
            1,
            1024 * 1024,
            super::super::MAX_MUTATION_PLAN_BYTES / 2,
        )
        .unwrap();
        assert_eq!(actual, original);
        assert_eq!((added, removed), (0, 0));
    }

    #[test]
    fn hunk_matching_preserves_exact_precedence_and_trimmed_ambiguity() {
        let exact_precedence = parse_patch(
            "*** Begin Patch\n*** Update File: file.txt\n@@\n-first\n+FIRST\n*** End Patch\n",
        )
        .unwrap();
        let PatchOp::Update { hunks, .. } = &exact_precedence[0] else {
            panic!("update")
        };
        // Both lines match after trimming, but only one is exactly `first`.
        let (updated, added, removed) =
            apply_patch_hunks("first   \nfirst\n", hunks, "file.txt", 0, 1, 1024, 4096).unwrap();
        assert_eq!(updated, "first   \nFIRST\n");
        assert_eq!((added, removed), (1, 1));

        let fallback = parse_patch(
            "*** Begin Patch\n*** Update File: file.txt\n@@\n-first\n+FIRST\n*** End Patch\n",
        )
        .unwrap();
        let PatchOp::Update { hunks, .. } = &fallback[0] else {
            panic!("update")
        };
        let error = apply_patch_hunks("first   \nfirst \n", hunks, "file.txt", 0, 1, 1024, 4096)
            .unwrap_err()
            .to_string();
        assert_eq!(
            error,
            "hunk 1/1 (file.txt): context matches 2 locations (lines 1, 2)"
        );
    }

    #[tokio::test]
    async fn patch_falls_back_to_trim_end_and_applies_hunks_top_down() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        std::fs::write(root.join("a.txt"), "first  \nsecond\nthird\nfourth\n").unwrap();
        let machine = test_machine(root.clone());

        machine
            .patch_impl(patch_args(
                "*** Begin Patch\n\
                 *** Update File: a.txt\n\
                 @@\n\
                 -first\n\
                 +FIRST\n\
                 @@\n\
                 -third\n\
                 +THIRD\n\
                 *** End Patch",
            ))
            .await
            .unwrap();
        assert_eq!(
            std::fs::read_to_string(root.join("a.txt")).unwrap(),
            "FIRST\nsecond\nTHIRD\nfourth\n"
        );
    }

    #[tokio::test]
    async fn patch_add_file_refuses_existing_targets() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        std::fs::write(root.join("a.txt"), "present\n").unwrap();
        let machine = test_machine(root.clone());

        let error = machine
            .patch_impl(patch_args(
                "*** Begin Patch\n*** Add File: a.txt\n+new\n*** End Patch",
            ))
            .await
            .unwrap_err()
            .to_string();
        assert_eq!(error, "a.txt: '*** Add File' target already exists");
        assert_eq!(
            std::fs::read_to_string(root.join("a.txt")).unwrap(),
            "present\n"
        );
    }

    #[tokio::test]
    async fn patch_best_effort_writes_what_planned_cleanly() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        std::fs::write(root.join("good.txt"), "old\n").unwrap();
        std::fs::write(root.join("exists.txt"), "keep\n").unwrap();
        let machine = test_machine(root.clone());

        let patch = "*** Begin Patch\n\
                     *** Update File: good.txt\n\
                     @@\n\
                     -old\n\
                     +new\n\
                     *** Update File: missing.txt\n\
                     @@\n\
                     -x\n\
                     +y\n\
                     *** Add File: exists.txt\n\
                     +nope\n\
                     *** End Patch";
        // Atomic mode fails the whole call.
        let error = machine.patch_impl(patch_args(patch)).await.unwrap_err();
        assert!(error.to_string().contains("missing.txt"), "{error}");
        assert_eq!(
            std::fs::read_to_string(root.join("good.txt")).unwrap(),
            "old\n"
        );

        let mut args = patch_args(patch);
        args.best_effort = true;
        let receipt: Value =
            serde_json::from_str(&machine.patch_impl(args).await.unwrap()).unwrap();
        assert_eq!(receipt["mode"], "best_effort");
        assert_eq!(receipt["files"][0]["path"], "good.txt");
        assert_eq!(receipt["failures"].as_array().unwrap().len(), 2);
        assert_eq!(
            std::fs::read_to_string(root.join("good.txt")).unwrap(),
            "new\n"
        );
        assert_eq!(
            std::fs::read_to_string(root.join("exists.txt")).unwrap(),
            "keep\n"
        );
    }

    #[test]
    fn patch_parser_bounds_hunks_and_files() {
        let mut patch = String::from("*** Begin Patch\n*** Update File: a.txt\n");
        for _ in 0..=MAX_PATCH_HUNKS {
            patch.push_str("@@\n x\n");
        }
        patch.push_str("*** End Patch\n");
        let error = parse_patch(&patch).unwrap_err().to_string();
        assert!(error.contains("hunk limit"), "{error}");

        let mut patch = String::from("*** Begin Patch\n");
        for index in 0..=MAX_PATCH_FILES {
            patch.push_str(&format!("*** Add File: f{index}.txt\n+x\n"));
        }
        patch.push_str("*** End Patch\n");
        let error = parse_patch(&patch).unwrap_err().to_string();
        assert!(error.contains("file limit"), "{error}");

        assert_eq!(
            parse_patch(
                "*** Begin Patch\n*** Update File: a.txt\n@@ -12,3 +12,4 @@\n-x\n+y\n*** End Patch"
            )
            .unwrap()
            .len(),
            1
        );
        let ops = parse_patch(
            "*** Begin Patch\n*** Update File: a.txt\n@@\n-old\n+new\n*** End of File\n*** End Patch",
        )
        .unwrap();
        match &ops[0] {
            PatchOp::Update { hunks, .. } => assert!(hunks[0].at_end_of_file),
            other => panic!("{other:?}"),
        }
        assert!(
            parse_patch("*** Begin Patch\n*** Add File: a.txt\n+one\n two\n*** End Patch").is_err()
        );
    }

    #[tokio::test]
    async fn patch_enforces_the_end_of_file_marker() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        std::fs::write(root.join("a.txt"), "one\ntwo\nthree\n").unwrap();
        let machine = test_machine(root.clone());

        // The block is at the end of the file: accepted.
        let receipt: Value = serde_json::from_str(
            &machine
                .patch_impl(patch_args(
                    "*** Begin Patch\n*** Update File: a.txt\n@@\n-three\n+THREE\n*** End of File\n*** End Patch",
                ))
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(receipt["files"][0]["path"], "a.txt");
        assert_eq!(
            std::fs::read_to_string(root.join("a.txt")).unwrap(),
            "one\ntwo\nTHREE\n"
        );

        // The same block in the middle of the file is refused, and nothing is
        // written: the marker is a claim the patch must satisfy.
        std::fs::write(root.join("a.txt"), "one\ntwo\nthree\n").unwrap();
        let error = machine
            .patch_impl(patch_args(
                "*** Begin Patch\n*** Update File: a.txt\n@@\n-one\n+ONE\n*** End of File\n*** End Patch",
            ))
            .await
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("requires the hunk to end at the end of the file"),
            "{error}"
        );
        assert_eq!(
            std::fs::read_to_string(root.join("a.txt")).unwrap(),
            "one\ntwo\nthree\n"
        );

        // A hunk after the marker is contradictory.
        let error = machine
            .patch_impl(patch_args(
                "*** Begin Patch\n*** Update File: a.txt\n@@\n-three\n+THREE\n*** End of File\n@@\n-one\n+ONE\n*** End Patch",
            ))
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("no hunk may follow"), "{error}");
    }

    #[tokio::test]
    async fn patch_reports_a_snapshot_that_changed_during_planning() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        std::fs::create_dir_all(root.join("crates/y/src")).unwrap();
        std::fs::write(root.join("crates/y/src/lib.rs"), "changed\n").unwrap();
        let machine = test_machine(root.clone());
        let stale = Snapshot {
            path: root.join("crates/y/src/lib.rs"),
            display: "crates/y/src/lib.rs".to_owned(),
            bytes: b"planned".to_vec(),
        };
        let error = stale
            .verify(&machine, "patching")
            .await
            .unwrap_err()
            .to_string();
        assert_eq!(error, "crates/y/src/lib.rs changed while patching");
    }

    #[test]
    fn the_advertised_description_states_the_enforced_limits() {
        let tool = LocalMachine::all_tools().get("patch").cloned().unwrap();
        let description = tool.description.clone().unwrap_or_default();
        for limit in [MAX_PATCH_FILES, MAX_PATCH_HUNKS] {
            assert!(
                description.contains(&limit.to_string()),
                "the patch description must advertise its {limit} limit: {description}"
            );
        }
    }

    #[test]
    fn patch_anchor_text_is_kept_and_line_numbers_are_ignored() {
        assert_eq!(parse_patch_anchor(" fn main() {"), Some("fn main() {"));
        assert_eq!(parse_patch_anchor(" -12,3 +12,4"), None);
        assert_eq!(parse_patch_anchor(" +12,4"), None);
        assert_eq!(parse_patch_anchor(" -3,1 +3,1 @@ fn b()"), Some("fn b()"));
        let ops = parse_patch(
            "*** Begin Patch\n*** Update File: a.rs\n@@ -3,1 +3,1 @@ fn b()\n-let x = 1;\n+let x = 2;\n*** End Patch",
        )
        .unwrap();
        match &ops[0] {
            PatchOp::Update { hunks, .. } => {
                assert_eq!(hunks[0].anchor, Some("fn b()"));
                assert_eq!(hunks[0].lines().count(), 2, "removed + added");
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(parse_patch_anchor(""), None);
    }

    #[tokio::test]
    async fn patch_best_effort_reports_a_create_failure_and_keeps_the_rest() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        std::fs::write(root.join("present.txt"), "present\n").unwrap();
        let machine = test_machine(root.clone());

        let mut args = patch_args(
            "*** Begin Patch\n\
             *** Add File: present.txt\n\
             +nope\n\
             *** Add File: fresh.txt\n\
             +yes\n\
             *** End Patch",
        );
        args.best_effort = true;
        let receipt: Value =
            serde_json::from_str(&machine.patch_impl(args).await.unwrap()).unwrap();
        assert_eq!(receipt["files"][0]["path"], "fresh.txt");
        assert_eq!(receipt["failures"][0]["path"], "present.txt");
        assert_eq!(
            std::fs::read_to_string(root.join("fresh.txt")).unwrap(),
            "yes\n"
        );
        assert_eq!(
            std::fs::read_to_string(root.join("present.txt")).unwrap(),
            "present\n"
        );
    }

    #[tokio::test]
    async fn patch_create_only_write_refuses_an_existing_target() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        std::fs::write(root.join("present.txt"), "present\n").unwrap();
        let machine = test_machine(root.clone());

        let error = machine
            .stage_create_write(&root.join("present.txt"), b"new")
            .await
            .unwrap_err();
        assert!(error.to_string().contains("already exists"), "{error}");
        assert_eq!(
            std::fs::read_to_string(root.join("present.txt")).unwrap(),
            "present\n"
        );

        // A create-only stage does not create the target until commit.
        let staged = machine
            .stage_create_write(&root.join("fresh.txt"), b"fresh")
            .await
            .unwrap();
        assert!(!root.join("fresh.txt").exists());
        machine.commit_write(staged).await.unwrap();
        assert_eq!(
            std::fs::read_to_string(root.join("fresh.txt")).unwrap(),
            "fresh"
        );
    }

    #[tokio::test]
    async fn concurrent_patches_serialize_per_path() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let original = (0..8)
            .map(|index| format!("token{index}"))
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(root.join("f.txt"), format!("{original}\n")).unwrap();
        let machine = std::sync::Arc::new(test_machine(root.clone()));
        let mut tasks = tokio::task::JoinSet::new();
        for index in 0..8 {
            let machine = machine.clone();
            tasks.spawn(async move {
                let patch = format!(
                    "*** Begin Patch\n*** Update File: f.txt\n@@\n-token{index}\n+CHANGED{index}\n*** End Patch"
                );
                machine.patch_impl(patch_args(&patch)).await
            });
        }
        while let Some(result) = tasks.join_next().await {
            result.unwrap().unwrap();
        }
        let text = std::fs::read_to_string(root.join("f.txt")).unwrap();
        for index in 0..8 {
            assert!(text.contains(&format!("CHANGED{index}")), "{text}");
        }
    }

    #[tokio::test]
    async fn patch_rewrites_nothing_when_a_hunk_reproduces_the_file() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let path = root.join("a.txt");
        std::fs::write(&path, "same\n").unwrap();
        let before = std::fs::metadata(&path).unwrap().modified().unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        let machine = test_machine(root);

        let receipt: Value = serde_json::from_str(
            &machine
                .patch_impl(patch_args(
                    "*** Begin Patch\n*** Update File: a.txt\n@@\n same\n*** End Patch",
                ))
                .await
                .unwrap(),
        )
        .unwrap();
        // A no-op patch writes nothing and stays out of the receipt.
        assert_eq!(receipt["files"], serde_json::json!([]));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "same\n");
        assert_eq!(
            std::fs::metadata(&path).unwrap().modified().unwrap(),
            before,
            "an unchanged patch must not touch the file"
        );
    }

    #[tokio::test]
    async fn patch_refuses_a_path_claimed_by_two_sections() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        std::fs::write(root.join("existing.txt"), "old\n").unwrap();
        let machine = test_machine(root.clone());

        // Two Add sections for the same new path: refused while planning, so
        // strict mode writes nothing at all.
        let error = machine
            .patch_impl(patch_args(
                "*** Begin Patch\n*** Add File: dup.txt\n+one\n*** Add File: dup.txt\n+two\n*** End Patch",
            ))
            .await
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("dup.txt appears in more than one section"),
            "{error}"
        );
        assert!(!root.join("dup.txt").exists(), "nothing may be written");

        // An Update and an Add for the same path are refused too, in either
        // order.
        for patch in [
            "*** Begin Patch\n*** Update File: existing.txt\n@@\n-old\n+new\n*** Add File: existing.txt\n+x\n*** End Patch",
            "*** Begin Patch\n*** Add File: existing.txt\n+x\n*** Update File: existing.txt\n@@\n-old\n+new\n*** End Patch",
        ] {
            assert!(
                machine.patch_impl(patch_args(patch)).await.is_err(),
                "{patch}"
            );
        }
        assert_eq!(
            std::fs::read_to_string(root.join("existing.txt")).unwrap(),
            "old\n"
        );

        // best_effort reports the conflict instead of writing the second copy.
        let receipt: Value = serde_json::from_str(
            &machine
                .patch_impl(PatchArgs {
                    dir: ".".to_owned(),
                    patch: "*** Begin Patch\n*** Add File: best.txt\n+one\n*** Add File: best.txt\n+two\n*** End Patch".to_owned(),
                    best_effort: true,
                })
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(receipt["files"].as_array().unwrap().len(), 1);
        assert!(
            receipt["failures"][0]["error"]
                .as_str()
                .unwrap()
                .contains("appears in more than one section")
        );
        assert_eq!(
            std::fs::read_to_string(root.join("best.txt")).unwrap(),
            "one\n"
        );
    }

    #[tokio::test]
    async fn patch_add_treats_only_a_missing_target_as_absent() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        // A directory where the file should go: `mkdir` of a file target is an
        // error, not "absent", so planning must fail.
        std::fs::create_dir(root.join("taken")).unwrap();
        let machine = test_machine(root.clone());
        let error = machine
            .patch_impl(patch_args(
                "*** Begin Patch\n*** Add File: taken\n+x\n*** End Patch",
            ))
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("already exists"), "{error}");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn patch_write_failure_leaves_every_file_unchanged() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        std::fs::write(root.join("good.txt"), "old\n").unwrap();
        std::fs::create_dir(root.join("locked")).unwrap();
        std::fs::set_permissions(
            root.join("locked"),
            std::os::unix::fs::PermissionsExt::from_mode(0o555),
        )
        .unwrap();
        let machine = test_machine(root.clone());

        // The Add cannot be written, so the Update staged before it must not be
        // visible either: staging happens for every file before any rename.
        let error = machine
            .patch_impl(patch_args(
                "*** Begin Patch\n*** Update File: good.txt\n@@\n-old\n+new\n*** Add File: locked/new.txt\n+x\n*** End Patch",
            ))
            .await
            .unwrap_err()
            .to_string();
        std::fs::set_permissions(
            root.join("locked"),
            std::os::unix::fs::PermissionsExt::from_mode(0o755),
        )
        .unwrap();
        assert!(error.contains("locked/new.txt"), "{error}");
        assert_eq!(
            std::fs::read_to_string(root.join("good.txt")).unwrap(),
            "old\n",
            "a failed write leaves earlier files untouched"
        );
        assert!(!root.join("locked/new.txt").exists());
        // No staged temporary survives the failure.
        assert!(
            std::fs::read_dir(&root)
                .unwrap()
                .filter_map(|entry| entry.ok())
                .all(|entry| !entry.file_name().to_string_lossy().contains(".dotlink-"))
        );
    }

    #[tokio::test]
    async fn patch_inherits_crlf_for_added_lines() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        std::fs::write(root.join("crlf.txt"), "foo\r\nbar\r\n").unwrap();
        let machine = test_machine(root.clone());

        // A model reading numbered output sees LF; the patch has no CR. The
        // added line must still use the file's CRLF terminator.
        machine
            .patch_impl(patch_args(
                "*** Begin Patch\n*** Update File: crlf.txt\n@@\n-bar\n+baz\n*** End Patch",
            ))
            .await
            .unwrap();
        assert_eq!(
            std::fs::read(root.join("crlf.txt")).unwrap(),
            b"foo\r\nbaz\r\n"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn patch_refuses_two_spellings_of_one_symlinked_file() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        std::fs::write(root.join("real.txt"), "old\n").unwrap();
        symlink("real.txt", root.join("alias.txt")).unwrap();
        let machine = test_machine(root.clone());

        let error = machine
            .patch_impl(patch_args(
                "*** Begin Patch\n*** Update File: real.txt\n@@\n-old\n+one\n*** Update File: alias.txt\n@@\n-old\n+two\n*** End Patch",
            ))
            .await
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("appears in more than one section"),
            "{error}"
        );
        assert_eq!(
            std::fs::read_to_string(root.join("real.txt")).unwrap(),
            "old\n"
        );

        let mut args = patch_args(
            "*** Begin Patch\n*** Update File: real.txt\n@@\n-old\n+one\n*** Update File: alias.txt\n@@\n-old\n+two\n*** End Patch",
        );
        args.best_effort = true;
        let receipt: Value =
            serde_json::from_str(&machine.patch_impl(args).await.unwrap()).unwrap();
        assert_eq!(receipt["files"][0]["path"], "real.txt");
        assert!(
            receipt["failures"][0]["error"]
                .as_str()
                .unwrap()
                .contains("appears in more than one section")
        );
        assert_eq!(
            std::fs::read_to_string(root.join("real.txt")).unwrap(),
            "one\n"
        );
    }

    #[tokio::test]
    async fn patch_claims_a_path_before_it_resolves_it() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let machine = test_machine(root.clone());

        // The first section names a missing file, so it fails to resolve. The
        // Add still claims the same path, so `best_effort` must report the
        // duplicate instead of writing the file the first section named.
        let receipt: Value = serde_json::from_str(
            &machine
                .patch_impl(PatchArgs {
                    dir: ".".to_owned(),
                    patch: "*** Begin Patch\n*** Update File: new.txt\n@@\n-x\n+y\n*** Add File: new.txt\n+z\n*** End Patch".to_owned(),
                    best_effort: true,
                })
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(receipt["files"], serde_json::json!([]));
        assert_eq!(receipt["failures"].as_array().unwrap().len(), 2);
        assert!(
            receipt["failures"][1]["error"]
                .as_str()
                .unwrap()
                .contains("appears in more than one section"),
            "{receipt}"
        );
        assert!(!root.join("new.txt").exists());
    }

    #[tokio::test]
    async fn patch_anchor_accepts_numbers_and_text_together() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        std::fs::write(root.join("a.rs"), "let x = 1;\nfn b() {}\nlet x = 1;\n").unwrap();
        let machine = test_machine(root.clone());

        // Without an anchor the block is ambiguous.
        let error = machine
            .patch_impl(patch_args(
                "*** Begin Patch\n*** Update File: a.rs\n@@\n-let x = 1;\n+let x = 2;\n*** End Patch",
            ))
            .await
            .unwrap_err()
            .to_string();
        assert_eq!(
            error,
            "hunk 1/1 (a.rs): context matches 2 locations (lines 1, 3)"
        );

        // An anchor carrying both range numbers and text: the numbers are
        // ignored and `fn b()` moves the search past the first `let x`.
        machine
            .patch_impl(patch_args(
                "*** Begin Patch\n*** Update File: a.rs\n@@ -3,1 +3,1 @@ fn b()\n-let x = 1;\n+let x = 2;\n*** End Patch",
            ))
            .await
            .unwrap();
        assert_eq!(
            std::fs::read_to_string(root.join("a.rs")).unwrap(),
            "let x = 1;\nfn b() {}\nlet x = 2;\n"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn patch_undoes_a_claimed_add_when_a_later_file_fails() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        std::fs::create_dir(root.join("locked")).unwrap();
        std::fs::write(root.join("locked/ro.txt"), "old\n").unwrap();
        std::fs::set_permissions(
            root.join("locked"),
            std::os::unix::fs::PermissionsExt::from_mode(0o555),
        )
        .unwrap();
        let machine = test_machine(root.clone());

        // The Add succeeds first; the Update then cannot be staged. Atomic mode
        // must leave neither the new file nor a changed target behind.
        let error = machine
            .patch_impl(patch_args(
                "*** Begin Patch\n*** Add File: claimed.txt\n+x\n*** Update File: locked/ro.txt\n@@\n-old\n+new\n*** End Patch",
            ))
            .await
            .unwrap_err()
            .to_string();
        std::fs::set_permissions(
            root.join("locked"),
            std::os::unix::fs::PermissionsExt::from_mode(0o755),
        )
        .unwrap();
        assert!(error.contains("locked/ro.txt"), "{error}");
        assert!(!root.join("claimed.txt").exists(), "the claim is undone");
        assert_eq!(
            std::fs::read_to_string(root.join("locked/ro.txt")).unwrap(),
            "old\n"
        );
        assert!(
            std::fs::read_dir(&root)
                .unwrap()
                .filter_map(|entry| entry.ok())
                .all(|entry| !entry.file_name().to_string_lossy().contains(".dotlink-"))
        );
    }
}
