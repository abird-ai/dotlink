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
    /// sections, then `*** End Patch`.
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
    lines: Vec<PatchLine<'a>>,
    /// The Codex `*** End of File` marker: this hunk must end at the last line.
    at_end_of_file: bool,
}

#[derive(Debug)]
enum PatchLine<'a> {
    Context(&'a str),
    Removed(&'a str),
    Added(&'a str),
}

/// Borrow an unchanged source line or a patch addition. The only extra
/// representation needed by CRLF normalization is a single trailing CR bit;
/// adding CR never allocates a String per line.
#[derive(Clone, Copy)]
struct WorkingLine<'a> {
    text: &'a str,
    append_cr: bool,
}

impl<'a> WorkingLine<'a> {
    fn source(text: &'a str) -> Self {
        Self {
            text,
            append_cr: false,
        }
    }

    fn added(text: &'a str, crlf: bool) -> Self {
        match (crlf, text.ends_with('\r')) {
            (true, false) => Self {
                text,
                append_cr: true,
            },
            (false, true) => Self::source(text.strip_suffix('\r').unwrap()),
            _ => Self::source(text),
        }
    }

    fn len(self) -> usize {
        self.text.len() + usize::from(self.append_cr)
    }

    fn is_empty(self) -> bool {
        self.text.is_empty() && !self.append_cr
    }

    fn ends_with_cr(self) -> bool {
        self.append_cr || self.text.ends_with('\r')
    }

    fn contains(self, needle: &str) -> bool {
        self.text.contains(needle)
            || (self.append_cr
                && needle
                    .strip_suffix('\r')
                    .is_some_and(|prefix| self.text.ends_with(prefix)))
    }

    fn equals(self, patch: &str) -> bool {
        if self.append_cr {
            patch.strip_suffix('\r') == Some(self.text)
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

/// Patch lines, split on `\n` only: a CRLF patch keeps its `\r`, and
/// `apply_patch_hunks` normalizes added lines to the target file's terminator.
fn patch_lines(text: &str) -> impl Iterator<Item = &str> {
    text.split_inclusive('\n')
        .map(|line| line.strip_suffix('\n').unwrap_or(line))
}

fn parse_patch(text: &str) -> Result<Vec<PatchOp<'_>>> {
    // All file names, hunk anchors and lines borrow the original request
    // buffer. Even million-line patches retain only short slice descriptors.
    let mut ops: Vec<PatchOp<'_>> = Vec::new();
    let mut saw_begin = false;
    let mut saw_end = false;
    let mut hunks = 0_usize;
    let mut cursor = 0_usize;

    for (index, line) in patch_lines(text).enumerate() {
        let number = index + 1;
        // The iterator strips only LF, not CR; the cursor keeps the exact
        // raw byte range so add-file bodies can be borrowed in one piece.
        cursor +=
            line.len() + usize::from(text.as_bytes().get(cursor + line.len()) == Some(&b'\n'));
        if !saw_begin {
            if line.trim().is_empty() {
                continue;
            }
            if line.trim_end() != "*** Begin Patch" {
                bail!("patch: missing '*** Begin Patch' header");
            }
            saw_begin = true;
            continue;
        }
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
                lines: Vec::new(),
                at_end_of_file: false,
            });
            continue;
        }

        match current {
            PatchOp::Update { hunks: list, .. } => {
                let Some(hunk) = list.last_mut() else {
                    bail!("patch: line {number}: text outside a hunk");
                };
                match line.as_bytes().first() {
                    Some(b' ') => hunk.lines.push(PatchLine::Context(&line[1..])),
                    Some(b'-') => hunk.lines.push(PatchLine::Removed(&line[1..])),
                    Some(b'+') => hunk.lines.push(PatchLine::Added(&line[1..])),
                    _ if line.trim().is_empty() => hunk.lines.push(PatchLine::Context("")),
                    _ => {
                        bail!("patch: line {number}: unexpected text '{line}'");
                    }
                }
            }
            PatchOp::Add {
                body, lines, bytes, ..
            } => {
                let added_bytes = match line.as_bytes().first() {
                    Some(b'+') => line.len(),         // remove '+' and append LF
                    _ if line.trim().is_empty() => 1, // blank line becomes LF
                    _ => bail!("patch: line {number}: unexpected text '{line}'"),
                };
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
    // One borrowed slice per source line: unchanged text is never cloned.
    // Keep this scratch index separate from the output-file byte ceiling.
    // Patches add owned text only when line-ending normalization needs it.
    let line_count = text
        .bytes()
        .filter(|byte| *byte == b'\n')
        .count()
        .saturating_add(1);
    let index_bytes = line_count.saturating_mul(std::mem::size_of::<WorkingLine<'_>>());
    if index_bytes > scratch_limit {
        bail!("patch source line index exceeds the remaining mutation plan budget");
    }
    let mut lines: Vec<WorkingLine<'_>> = Vec::with_capacity(line_count);
    lines.extend(text.split('\n').map(WorkingLine::source));
    let mut content_bytes = text.len() - lines.len().saturating_sub(1);
    let mut from_line = 0_usize;
    let mut added = 0_usize;
    let mut removed = 0_usize;

    for (offset, hunk) in hunks.iter().enumerate() {
        let position = first_hunk + offset + 1;
        let block = hunk
            .lines
            .iter()
            .filter_map(|line| match line {
                PatchLine::Context(text) => Some(*text),
                PatchLine::Removed(text) => Some(*text),
                PatchLine::Added(_) => None,
            })
            .collect::<Vec<_>>();
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

        if block.is_empty() {
            bail!("hunk {position}/{total_hunks} ({display}): context not found near line {near}");
        }

        // Count ambiguity without retaining one usize per candidate line.
        // A repetitive file can contain millions of matching lines; only a
        // small diagnostic prefix is useful to the caller.
        let (mut exact_count, mut lenient_count) = (0_usize, 0_usize);
        let (mut exact_first, mut lenient_first) = (None, None);
        let mut exact_preview = Vec::with_capacity(MAX_AMBIGUOUS_LINES);
        let mut lenient_preview = Vec::with_capacity(MAX_AMBIGUOUS_LINES);
        for start in search_start..lines.len() {
            if start + block.len() > lines.len() {
                break;
            }
            if lines[start..start + block.len()]
                .iter()
                .zip(&block)
                .all(|(file, patch)| file.equals(patch))
            {
                exact_count += 1;
                exact_first.get_or_insert(start);
                if exact_preview.len() < MAX_AMBIGUOUS_LINES {
                    exact_preview.push(start + 1);
                }
            } else if lines[start..start + block.len()]
                .iter()
                .zip(&block)
                .all(|(file, patch)| file.equals_trimmed(patch))
            {
                lenient_count += 1;
                lenient_first.get_or_insert(start);
                if lenient_preview.len() < MAX_AMBIGUOUS_LINES {
                    lenient_preview.push(start + 1);
                }
            }
        }
        let (first, matches, preview) = if exact_count > 0 {
            (exact_first, exact_count, exact_preview)
        } else {
            (lenient_first, lenient_count, lenient_preview)
        };
        let Some(start) = first else {
            bail!("hunk {position}/{total_hunks} ({display}): context not found near line {near}");
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

        if hunk.at_end_of_file {
            // `text.split('\n')` yields one empty tail for a trailing newline;
            // it is not a line the marker can refer to.
            let file_lines =
                lines.len() - usize::from(lines.last().is_some_and(|line| line.is_empty()));
            if start + block.len() != file_lines {
                bail!(
                    "hunk {position}/{total_hunks} ({display}): '*** End of File' requires the hunk to end at the end of the file (line {file_lines})"
                );
            }
        }

        // Before allocating replacement lines or expanding the index, compute
        // both the resulting file size and scratch usage. Very dense added
        // lines can make the Cow index expensive even for a tiny source.
        let file_crlf = lines.get(start).is_some_and(|line| line.ends_with_cr());
        let replacement_count = hunk
            .lines
            .iter()
            .filter(|line| !matches!(line, PatchLine::Removed(_)))
            .count();
        let next_line_count = lines
            .len()
            .checked_sub(block.len())
            .and_then(|n| n.checked_add(replacement_count))
            .ok_or_else(|| anyhow!("patch line count overflow"))?;
        let removed_bytes = lines[start..start + block.len()]
            .iter()
            .try_fold(0_usize, |total, line| total.checked_add(line.len()))
            .ok_or_else(|| anyhow!("patch line size overflow"))?;
        let mut replacement_bytes = 0_usize;
        let mut original_line = 0_usize;
        for line in &hunk.lines {
            let size = match line {
                PatchLine::Context(_) => {
                    let size = lines[start + original_line].len();
                    original_line += 1;
                    size
                }
                PatchLine::Removed(_) => {
                    original_line += 1;
                    0
                }
                PatchLine::Added(text) => WorkingLine::added(text, file_crlf).len(),
            };
            replacement_bytes = replacement_bytes
                .checked_add(size)
                .ok_or_else(|| anyhow!("patch replacement size overflow"))?;
        }
        let next_content = content_bytes
            .checked_sub(removed_bytes)
            .and_then(|size| size.checked_add(replacement_bytes))
            .ok_or_else(|| anyhow!("patch replacement size overflow"))?;
        let next_size = next_content
            .checked_add(next_line_count.saturating_sub(1))
            .ok_or_else(|| anyhow!("patch result size overflow"))?;
        if next_size > output_limit {
            bail!(
                "patch result would be {next_size} bytes, above the {output_limit}-byte write limit"
            );
        }
        let index_bytes = next_line_count
            .checked_mul(std::mem::size_of::<WorkingLine<'_>>())
            .ok_or_else(|| anyhow!("patch line index size overflow"))?;
        // The per-hunk replacement Vec exists briefly beside the main line
        // index, so charge its Cow slots before allocating it as well.
        let replacement_slots = replacement_count
            .checked_mul(std::mem::size_of::<WorkingLine<'_>>())
            .ok_or_else(|| anyhow!("patch replacement index size overflow"))?;
        if index_bytes
            .saturating_add(next_size)
            .saturating_add(replacement_slots)
            > scratch_limit
        {
            bail!("patch working buffers exceed the remaining mutation plan budget");
        }

        let mut replacement = Vec::with_capacity(replacement_count);
        // Added lines inherit the terminator of the file they land in. The
        // patch itself often carries LF because numbered `read` strips `\r`,
        // so preserving only the patch's bytes would produce mixed endings in
        // a CRLF file.
        // Cursor into the consumed block: only context/removed lines consume
        // existing file lines.
        let mut consumed_at = 0_usize;
        for line in &hunk.lines {
            match line {
                // Keep the file's own bytes for context lines, so a lenient
                // match never rewrites trailing whitespace it did not touch.
                PatchLine::Context(_) => {
                    replacement.push(lines[start + consumed_at]);
                    consumed_at += 1;
                }
                PatchLine::Removed(_) => {
                    removed += 1;
                    consumed_at += 1;
                }
                PatchLine::Added(text) => {
                    added += 1;
                    replacement.push(WorkingLine::added(text, file_crlf));
                }
            }
        }
        let replacement_len = replacement.len();
        lines.splice(start..start + block.len(), replacement);
        content_bytes = next_content;
        from_line = start + replacement_len;
    }

    // Inspect the exact projected join size before allocating the final
    // result. Retained source and output must fit the shared plan budget.
    let bytes = lines
        .iter()
        .try_fold(0_usize, |sum, line| sum.checked_add(line.len()))
        .and_then(|sum| sum.checked_add(lines.len().saturating_sub(1)))
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
    let mut result = String::with_capacity(bytes);
    for (index, line) in lines.iter().enumerate() {
        if index != 0 {
            result.push('\n');
        }
        line.append_to(&mut result);
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
        // commit sequence. Shard indices are acquired in sorted order, so a
        // multi-file patch cannot deadlock and independent files stay
        // concurrent.
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
                            if let Some(content) = line.strip_prefix('+') {
                                text.push_str(content);
                            }
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
        description = "Apply a Codex-grammar patch with per-file atomic replacement. Supports \"*** Update File:\" hunks and \"*** Add File:\" only; the Codex \"*** End of File\" marker is enforced (the hunk must end at the last line), while Delete and Move are rejected. At most 32 files and 64 hunks per call. Hunks are located by their context lines (exact first, then trailing-whitespace-tolerant), an ambiguous match fails, and hunks apply top-down within a file. `*** Add File` is create-only and a path claimed by two sections is refused. Every file is planned and re-checked before any commit; each file replacement is atomic, but the multi-file commit sequence is not a transaction, so a commit failure can leave earlier files replaced. best_effort writes the files that planned cleanly and reports the rest. Returns a receipt with per-file hunks/added/removed and mode (strict or best_effort).",
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
    fn crlf_adjustment_is_borrowed_and_comparable_between_hunks() {
        let added = WorkingLine::added("hello", true);
        assert_eq!(added.len(), 6);
        assert!(added.ends_with_cr());
        assert!(added.equals("hello\r"));
        assert!(added.equals_trimmed("hello"));
        assert!(added.contains("lo\r"));
        let mut output = String::new();
        added.append_to(&mut output);
        assert_eq!(output, "hello\r");
        let normalized = WorkingLine::added("hello\r", false);
        assert!(normalized.equals("hello"));
        assert!(!normalized.ends_with_cr());
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
    fn line_index_growth_is_rejected_before_expanding_temp_buffers() {
        let input = format!(
            "*** Begin Patch\n*** Update File: file.txt\n@@\n-old\n{}*** End Patch\n",
            "+x\n".repeat(250)
        );
        let patch = parse_patch(&input).unwrap();
        let PatchOp::Update { hunks, .. } = &patch[0] else {
            panic!("expected update")
        };
        let error = apply_patch_hunks("old\n", hunks, "file.txt", 0, 1, 4096, 1024)
            .unwrap_err()
            .to_string();
        assert!(error.contains("working buffers"), "{error}");
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
                assert!(hunks[0].lines.iter().all(|line| match line {
                    PatchLine::Context(s) | PatchLine::Removed(s) | PatchLine::Added(s) =>
                        inside(s),
                }));
            }
            _ => panic!("expected update"),
        }
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
            .map(|line| {
                let content = line.strip_prefix('+').unwrap_or("");
                format!("{content}\n")
            })
            .collect::<String>();
        assert_eq!(actual, "hello\r\n\n\r\n");
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
                assert_eq!(hunks[0].lines.len(), 2, "removed + added");
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
