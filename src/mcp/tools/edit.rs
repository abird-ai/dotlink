//! `edit`: ordered multi-file exact-text replacement with per-file atomic commits.

use anyhow::Result;
use rmcp::{
    handler::server::wrapper::Parameters,
    model::{CallToolResult, Tool},
    schemars, tool, tool_router,
};
use serde::{Deserialize, de::Error as _};
use serde_json::Value;
use std::{collections::HashSet, path::PathBuf};

use super::{
    LocalMachine, MAX_EDIT_FILES, MAX_EDIT_OPS, Snapshot, has_line_number_prefix, line_count,
    line_number,
    mutation::{MutationReceipt, MutationTarget, PlanBudget, PlannedMutation, stage_and_commit},
    ok_text, tool_error,
};
use crate::mcp::AccessNeed;

/// The ambiguity message lists at most this many line numbers, so a pathological
/// `old_text` cannot produce an unbounded error string.
const MAX_AMBIGUOUS_LINES: usize = 10;

/// The public surface has exactly one source of paths: each operation.
/// Older requests with a top-level default path are upgraded only while
/// deserializing; the planner never carries both representations.
#[derive(Debug, schemars::JsonSchema)]
pub(super) struct EditArgs {
    /// Ordered edits. Multiple entries for the same path see prior edits.
    edits: Vec<EditOp>,
}

#[derive(Debug, schemars::JsonSchema)]
pub(super) struct EditOp {
    /// Required path for this operation. Relative paths use the launch directory.
    path: String,
    /// Exact text to find. Never include the `N\t` prefix that `read` adds.
    old_text: String,
    /// Replacement text. May be empty.
    new_text: String,
    /// Replace every exact occurrence. Without this the match must be unique.
    #[schemars(default)]
    replace_all: bool,
}

/// Compatibility belongs solely at the wire boundary. The schema advertises
/// only EditArgs/EditOp, so new clients cannot inherit an ambiguous default.
#[derive(Deserialize)]
struct LegacyEditArgs {
    #[serde(default)]
    path: Option<String>,
    #[serde(deserialize_with = "bounded_edit_ops")]
    edits: Vec<LegacyEditOp>,
}

#[derive(Deserialize)]
struct LegacyEditOp {
    #[serde(default)]
    path: Option<String>,
    old_text: String,
    new_text: String,
    #[serde(default)]
    replace_all: bool,
}

/// The bound is enforced while parsing, not after the complete wire array.
fn bounded_edit_ops<'de, D: serde::Deserializer<'de>>(
    de: D,
) -> std::result::Result<Vec<LegacyEditOp>, D::Error> {
    super::mutation::bounded_array::<D, LegacyEditOp, MAX_EDIT_OPS>(de, "edit", "edits")
}

/// Advertise the same limits the parser, planner, and logger enforce.
pub(crate) fn with_edit_limits(tool: &mut Tool) {
    let current = tool.description.take().unwrap_or_default();
    tool.description = Some(
        format!(
            "{current} At most {MAX_EDIT_FILES} distinct files and {MAX_EDIT_OPS} edits per call."
        )
        .into(),
    );
    super::mutation::set_array_max_items(tool, "edits", MAX_EDIT_OPS);
}

impl<'de> Deserialize<'de> for EditArgs {
    fn deserialize<D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<Self, D::Error> {
        let wire = LegacyEditArgs::deserialize(deserializer)?;
        let total = wire.edits.len();
        let mut edits = Vec::with_capacity(total);
        for (index, op) in wire.edits.into_iter().enumerate() {
            let path = op
                .path
                .or_else(|| wire.path.clone())
                .filter(|path| !path.is_empty())
                .ok_or_else(|| {
                    D::Error::custom(format!(
                        "edit {}/{}: edits[].path is required",
                        index + 1,
                        total
                    ))
                })?;
            edits.push(EditOp {
                path,
                old_text: op.old_text,
                new_text: op.new_text,
                replace_all: op.replace_all,
            });
        }
        Ok(Self { edits })
    }
}

/// Planning state for one file: the snapshot every entry is applied against and
/// the resulting text.
struct FilePlan<'a> {
    snapshot: Snapshot,
    text: String,
    edits: usize,
    /// Replacement texts applied so far, for the substring guard.
    new_texts: Vec<&'a str>,
}

/// A changed file that passed verification and is ready to be written.
struct Changed {
    verified: super::Verified,
    text: String,
    edits: usize,
}

impl LocalMachine {
    async fn edit_impl(&self, args: EditArgs) -> Result<String> {
        let total = args.edits.len();
        if total == 0 {
            anyhow::bail!("edit: edits must not be empty");
        }
        if total > MAX_EDIT_OPS {
            anyhow::bail!("edit: {total} edits, above the {MAX_EDIT_OPS} edit limit");
        }

        let mut combined_bytes = 0_usize;
        let mut resolved_paths = Vec::with_capacity(total);
        let mut distinct = HashSet::<PathBuf>::new();
        for (index, op) in args.edits.iter().enumerate() {
            let position = index + 1;
            super::require_text_bytes(op.old_text.as_bytes(), "bedit")?;
            super::require_text_bytes(op.new_text.as_bytes(), "bedit")?;
            combined_bytes = combined_bytes
                .saturating_add(op.old_text.len())
                .saturating_add(op.new_text.len());
            if combined_bytes > self.config.max_write_bytes {
                anyhow::bail!(
                    "edit {position}/{total}: edits are {combined_bytes} bytes, above the {} byte write limit",
                    self.config.max_write_bytes
                );
            }

            if op.path.is_empty() {
                anyhow::bail!("edit {position}/{total}: edits[].path is required");
            }
            let target = &op.path;
            let resolved = self.resolve_existing(target, AccessNeed::ReadWrite).await?;
            if distinct.insert(resolved.clone()) && distinct.len() > MAX_EDIT_FILES {
                anyhow::bail!(
                    "edit {position}/{total}: {} files, above the {MAX_EDIT_FILES} file limit",
                    distinct.len()
                );
            }
            resolved_paths.push(resolved);
        }

        // Hold every target for the whole plan -> verify -> stage -> commit
        // sequence. Exact resolved-path locks are acquired in sorted order, so
        // multi-file edits cannot deadlock and unrelated files stay concurrent.
        let _mutation = self
            .lock_mutations(resolved_paths.iter().map(|path| path.as_path()))
            .await?;

        let mut files: Vec<FilePlan<'_>> = Vec::new();
        // Bound all simultaneously retained source snapshots and current text
        // buffers, not just the size of individual files or request payload.
        let mut budget = PlanBudget::default();
        for (index, (op, path)) in args.edits.iter().zip(resolved_paths).enumerate() {
            let position = index + 1;
            let slot = match files.iter().position(|file| file.snapshot.path == path) {
                Some(slot) => slot,
                None => {
                    let display = self.config.access.relative_display(&path);
                    let limit = budget.copy_read_limit(self.config.max_read_bytes)?;
                    let bound = if limit < self.config.max_read_bytes {
                        "aggregate plan budget"
                    } else {
                        "configured read limit"
                    };
                    let snapshot = Snapshot::read_within(self, path, display, limit)
                        .await
                        .map_err(|error| {
                            anyhow::anyhow!(
                                "edit {position}/{total}: cannot read within {bound}: {error}"
                            )
                        })?;
                    let text = snapshot
                        .text()
                        .map_err(|error| anyhow::anyhow!("edit {position}/{total}: {error}"))?
                        .to_owned();
                    budget.add(snapshot.bytes().len(), text.len())?;
                    files.push(FilePlan {
                        snapshot,
                        text,
                        edits: 0,
                        new_texts: Vec::new(),
                    });
                    files.len() - 1
                }
            };
            let file = &mut files[slot];
            if op.old_text.is_empty() {
                anyhow::bail!(
                    "edit {position}/{total} ({}): old_text must not be empty",
                    file.snapshot.display()
                );
            }
            if file
                .new_texts
                .iter()
                .any(|new_text| new_text.contains(&op.old_text))
            {
                anyhow::bail!(
                    "edit {position}/{total} ({}): old_text is a substring of this edit's predecessor's new_text in the same file",
                    file.snapshot.display()
                );
            }

            // A single-character needle may match millions of times. Keep
            // only the count and the first diagnostic lines, never a Vec of
            // every offset. replace_all uses the same allocation-free scan.
            let mut matches = 0_usize;
            let mut first_lines = Vec::with_capacity(MAX_AMBIGUOUS_LINES);
            for (at, _) in file.text.match_indices(&op.old_text) {
                matches += 1;
                if !op.replace_all && first_lines.len() < MAX_AMBIGUOUS_LINES {
                    first_lines.push(line_number(&file.text, at).to_string());
                }
            }
            if matches == 0 {
                if has_line_number_prefix(&op.old_text) {
                    anyhow::bail!(
                        "edit {position}/{total} ({}): old_text appears to include a read line-number prefix (digits then tab); strip it",
                        file.snapshot.display()
                    );
                }
                anyhow::bail!(
                    "edit {position}/{total} ({}): old_text does not occur at the point that edit applies",
                    file.snapshot.display()
                );
            }
            if !op.replace_all && matches > 1 {
                let lines = first_lines.join(", ");
                anyhow::bail!(
                    "edit {position}/{total} ({}): old_text occurs {matches} times at lines {lines}; make it unique or set replace_all",
                    file.snapshot.display(),
                );
            }

            let count = if op.replace_all { matches } else { 1 };
            let next_len = file
                .text
                .len()
                .checked_sub(op.old_text.len() * count)
                .and_then(|bytes| bytes.checked_add(op.new_text.len().checked_mul(count)?))
                .ok_or_else(|| {
                    anyhow::anyhow!("edit {position}/{total}: replacement size overflow")
                })?;
            if next_len > self.config.max_write_bytes {
                anyhow::bail!(
                    "edit {position}/{total} ({}): edited file would be {next_len} bytes, above the {} byte write limit",
                    file.snapshot.display(),
                    self.config.max_write_bytes
                );
            }
            budget.replace_work(file.text.len(), next_len)?;
            if count == 1 {
                // Most edits are single matches. Replace directly rather than
                // retaining a second full file-sized String during planning.
                let at = file
                    .text
                    .find(&op.old_text)
                    .expect("match was counted above");
                file.text
                    .replace_range(at..at + op.old_text.len(), &op.new_text);
            } else if op.new_text.len() <= op.old_text.len() {
                // Shrinking or byte-equal replace_all can compact in place,
                // even when there are millions of matches.
                super::mutation::replace_literal_in_place(
                    &mut file.text,
                    &op.old_text,
                    &op.new_text,
                );
            } else {
                // Growing multi-match replacements use the standard library's
                // optimized builder instead of a complicated in-place scheme.
                file.text = file.text.replace(&op.old_text, &op.new_text);
            }
            file.edits += 1;
            file.new_texts.push(&op.new_text);
        }

        // Verify every file before the first write: a file that changed since it
        // was read fails the whole call, and a no-op file is never touched.
        let mut changed: Vec<Changed> = Vec::new();
        for file in files {
            if file.text.as_bytes() == file.snapshot.bytes() {
                continue;
            }
            let verified = file
                .snapshot
                .verify(self, "editing")
                .await
                .map_err(|error| anyhow::anyhow!("{error}; nothing was written"))?;
            changed.push(Changed {
                verified,
                text: file.text,
                edits: file.edits,
            });
        }

        // Receipt generation precedes *every* commit. The embedded OpenAI
        // transport bounds responses after tool execution, so returning an
        // oversized success receipt would report a 502 for already-written
        // files. A compact, explicit prefix fits the configured output ceiling.
        let changed_files = changed.len();
        let records = changed
            .iter()
            .map(|change| {
                serde_json::json!({
                    "path": change.verified.display(),
                    "edits": change.edits,
                    "line_delta": line_count(&change.text) as i64
                        - line_count(&String::from_utf8_lossy(change.verified.bytes())) as i64,
                })
            })
            .collect::<Vec<_>>();
        let mut metadata = serde_json::Map::new();
        metadata.insert("edits_applied".into(), Value::from(total));
        metadata.insert("changed_files".into(), Value::from(changed_files));
        let receipt_plan = MutationReceipt::new(self.config.max_output_bytes, metadata)?;
        let receipt = receipt_plan.finish(&records, &[]);

        let plans = changed
            .into_iter()
            .zip(records.iter().cloned())
            .map(|(change, record)| PlannedMutation {
                display: change.verified.display().to_owned(),
                target: MutationTarget::Update(change.verified),
                bytes: change.text.into_bytes(),
                record,
            })
            .collect::<Vec<_>>();
        let outcome = stage_and_commit(self, "edit", false, plans).await?;
        debug_assert!(outcome.failures.is_empty());
        debug_assert_eq!(outcome.records.len(), records.len());
        Ok(receipt.to_string())
    }
}

#[tool_router(router = "edit_router", vis = "pub(crate)")]
impl LocalMachine {
    #[tool(
        description = "Replace exact text in one or more existing UTF-8 files. `edits` is an ordered array of {path, old_text, new_text, replace_all?} (bounded per call). Every edit carries its own required path; repeating it edits the same file's evolving buffer. Each old_text must be unique unless replace_all is set, and must not be a substring of an earlier edit's new_text in the same file. `read` prefixes lines with a line number and a tab; never include that prefix in old_text. Every file is planned and re-checked before anything is written, and each file replacement is atomic; the multi-file commit sequence is not a transaction, so a commit failure can leave earlier files replaced. Returns a receipt with per-file path, edits and line_delta, plus edits_applied and changed_files.",
        annotations(
            title = "Edit text files",
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = false,
            open_world_hint = false
        )
    )]
    async fn edit(&self, Parameters(args): Parameters<EditArgs>) -> CallToolResult {
        match self.edit_impl(args).await {
            Ok(receipt) => ok_text(receipt),
            Err(error) => tool_error(error),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mcp::tools::MAX_MUTATION_PLAN_BYTES;
    use crate::mcp::tools::test_support::test_machine;
    use serde_json::Value;

    fn edit_args(path: Option<&str>, mut edits: Vec<EditOp>) -> EditArgs {
        for op in &mut edits {
            if op.path.is_empty() {
                op.path = path.unwrap_or_default().to_owned();
            }
        }
        EditArgs { edits }
    }

    fn edit_op(path: Option<&str>, old_text: &str, new_text: &str) -> EditOp {
        EditOp {
            path: path.unwrap_or_default().to_owned(),
            old_text: old_text.to_owned(),
            new_text: new_text.to_owned(),
            replace_all: false,
        }
    }

    #[test]
    fn edit_wire_limit_rejects_before_materializing_unbounded_ops() {
        let edits = (0..=MAX_EDIT_OPS)
            .map(|_| {
                serde_json::json!({
                    "path": "a.txt", "old_text":"x", "new_text":"y"
                })
            })
            .collect::<Vec<_>>();
        let error = serde_json::from_value::<EditArgs>(serde_json::json!({"edits":edits}))
            .unwrap_err()
            .to_string();
        assert!(
            error.contains(&format!("at most {MAX_EDIT_OPS} edits")),
            "{error}"
        );
        let valid = (0..MAX_EDIT_OPS)
            .map(|_| {
                serde_json::json!({
                    "path":"a.txt", "old_text":"x", "new_text":"y"
                })
            })
            .collect::<Vec<_>>();
        assert_eq!(
            serde_json::from_value::<EditArgs>(serde_json::json!({"edits":valid}))
                .unwrap()
                .edits
                .len(),
            MAX_EDIT_OPS
        );
    }

    #[test]
    fn edit_wire_accepts_legacy_default_but_requires_paths_in_new_schema() {
        let legacy: EditArgs = serde_json::from_value(serde_json::json!({
            "path": "src/main.rs", "edits": [{"old_text":"first", "new_text":"second"}]
        }))
        .unwrap();
        assert_eq!(legacy.edits[0].path, "src/main.rs");
        let modern: EditArgs = serde_json::from_value(serde_json::json!({
            "edits": [{"path":"src/a.rs", "old_text":"one", "new_text":"two"}]
        }))
        .unwrap();
        assert_eq!(modern.edits[0].path, "src/a.rs");
        let error = serde_json::from_value::<EditArgs>(serde_json::json!({
            "edits": [{"old_text":"x", "new_text":"y"}]
        }))
        .unwrap_err()
        .to_string();
        assert!(error.contains("edits[].path is required"), "{error}");
        let schema = super::LocalMachine::all_tools()
            .get("edit")
            .unwrap()
            .input_schema
            .clone();
        assert!(schema["properties"].get("path").is_none(), "{schema:?}");
        assert_eq!(
            schema["properties"]["edits"]["maxItems"],
            serde_json::json!(MAX_EDIT_OPS)
        );
        let op = &schema["$defs"]["EditOp"];
        assert!(
            op["required"]
                .as_array()
                .unwrap()
                .iter()
                .any(|key| key == "path"),
            "{op}"
        );
    }

    #[tokio::test]
    async fn edit_applies_ordered_multi_file_edits_with_inherited_path() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        std::fs::create_dir_all(root.join("crates/x/src")).unwrap();
        std::fs::create_dir_all(root.join("crates/y/src")).unwrap();
        std::fs::write(root.join("crates/x/src/lib.rs"), "fn a() {}\nfn b() {}\n").unwrap();
        std::fs::write(root.join("crates/y/src/lib.rs"), "alpha\n").unwrap();
        let machine = test_machine(root.clone());

        let receipt = machine
            .edit_impl(edit_args(
                Some("crates/x/src/lib.rs"),
                vec![
                    edit_op(None, "fn a() {}", "fn a() { one() }"),
                    // Same file, later in the array: applies to the buffer the
                    // first edit produced.
                    edit_op(None, "fn b() {}", "fn b() { two() }"),
                    edit_op(Some("crates/y/src/lib.rs"), "alpha", "beta\ngamma"),
                ],
            ))
            .await
            .unwrap();
        let receipt: Value = serde_json::from_str(&receipt).unwrap();
        assert_eq!(receipt["edits_applied"], serde_json::json!(3));
        assert_eq!(receipt["changed_files"], serde_json::json!(2));
        assert_eq!(
            receipt["files"][0],
            serde_json::json!({
                "path": "crates/x/src/lib.rs",
                "edits": 2,
                "line_delta": 0
            })
        );
        assert_eq!(receipt["files"][1]["line_delta"], serde_json::json!(1));
        assert_eq!(
            std::fs::read_to_string(root.join("crates/x/src/lib.rs")).unwrap(),
            "fn a() { one() }\nfn b() { two() }\n"
        );
        assert_eq!(
            std::fs::read_to_string(root.join("crates/y/src/lib.rs")).unwrap(),
            "beta\ngamma\n"
        );
    }

    #[tokio::test]
    async fn edit_rejects_failures_with_contract_texts() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        std::fs::create_dir_all(root.join("crates/x/src")).unwrap();
        std::fs::write(root.join("crates/x/src/lib.rs"), "one\ntwo\ntwo\nthree\n").unwrap();
        std::fs::write(root.join("plain.txt"), "plain\n").unwrap();
        let machine = test_machine(root.clone());
        let target = "crates/x/src/lib.rs";

        // Missing path.
        let error = machine
            .edit_impl(edit_args(None, vec![edit_op(None, "one", "1")]))
            .await
            .unwrap_err()
            .to_string();
        assert_eq!(error, "edit 1/1: edits[].path is required");

        // Not found.
        let error = machine
            .edit_impl(edit_args(Some(target), vec![edit_op(None, "nope", "x")]))
            .await
            .unwrap_err()
            .to_string();
        assert_eq!(
            error,
            "edit 1/1 (crates/x/src/lib.rs): old_text does not occur at the point that edit applies"
        );

        // Ambiguity lists the matching lines.
        let error = machine
            .edit_impl(edit_args(
                Some(target),
                vec![edit_op(None, "one", "1"), edit_op(None, "two", "2")],
            ))
            .await
            .unwrap_err()
            .to_string();
        assert_eq!(
            error,
            "edit 2/2 (crates/x/src/lib.rs): old_text occurs 2 times at lines 2, 3; make it unique or set replace_all"
        );

        // The read line-number prefix hint.
        let error = machine
            .edit_impl(edit_args(Some(target), vec![edit_op(None, "2\ttwo", "2")]))
            .await
            .unwrap_err()
            .to_string();
        assert_eq!(
            error,
            "edit 1/1 (crates/x/src/lib.rs): old_text appears to include a read line-number prefix (digits then tab); strip it"
        );

        // old_text that is a substring of the predecessor's new_text.
        let error = machine
            .edit_impl(edit_args(
                Some(target),
                vec![
                    edit_op(None, "one", "one and two"),
                    edit_op(None, "two", "three"),
                ],
            ))
            .await
            .unwrap_err()
            .to_string();
        assert_eq!(
            error,
            "edit 2/2 (crates/x/src/lib.rs): old_text is a substring of this edit's predecessor's new_text in the same file"
        );

        // Nothing was written by any failed plan.
        assert_eq!(
            std::fs::read_to_string(root.join(target)).unwrap(),
            "one\ntwo\ntwo\nthree\n"
        );

        // A snapshot that no longer matches disk fails before any write.
        let stale = Snapshot {
            path: root.join(target),
            display: target.to_owned(),
            bytes: b"planned".to_vec(),
        };
        let error = stale
            .verify(&machine, "editing")
            .await
            .unwrap_err()
            .to_string();
        assert_eq!(error, "crates/x/src/lib.rs changed while editing");
    }

    #[tokio::test]
    async fn edit_replace_all_and_bounds() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        std::fs::write(root.join("a.txt"), "x\ny\nx\n").unwrap();
        let machine = test_machine(root.clone());

        let mut op = edit_op(None, "x", "z");
        op.replace_all = true;
        let receipt = machine
            .edit_impl(edit_args(Some("a.txt"), vec![op]))
            .await
            .unwrap();
        let receipt: Value = serde_json::from_str(&receipt).unwrap();
        assert_eq!(receipt["files"][0]["edits"], serde_json::json!(1));
        assert_eq!(
            std::fs::read_to_string(root.join("a.txt")).unwrap(),
            "z\ny\nz\n"
        );

        let error = machine
            .edit_impl(edit_args(Some("a.txt"), Vec::new()))
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("edits must not be empty"), "{error}");

        let too_many = (0..=MAX_EDIT_OPS)
            .map(|index| edit_op(None, &format!("m{index}"), "n"))
            .collect::<Vec<_>>();
        let error = machine
            .edit_impl(edit_args(Some("a.txt"), too_many))
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("edit limit"), "{error}");
    }

    #[tokio::test]
    async fn edit_global_caps_allow_512_ops_across_64_files_and_reject_65_files() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let mut ops = Vec::new();
        for file in 0..MAX_EDIT_FILES {
            let path = format!("f{file:03}.txt");
            std::fs::write(
                root.join(&path),
                (0..(MAX_EDIT_OPS / MAX_EDIT_FILES))
                    .map(|index| format!("a{index}"))
                    .collect::<Vec<_>>()
                    .join(" "),
            )
            .unwrap();
            for index in 0..(MAX_EDIT_OPS / MAX_EDIT_FILES) {
                ops.push(edit_op(
                    Some(&path),
                    &format!("a{index}"),
                    &format!("b{index}"),
                ));
            }
        }
        assert_eq!(ops.len(), MAX_EDIT_OPS);
        let machine = test_machine(root.clone());
        let result: Value =
            serde_json::from_str(&machine.edit_impl(EditArgs { edits: ops }).await.unwrap())
                .unwrap();
        assert_eq!(result["edits_applied"], serde_json::json!(MAX_EDIT_OPS));
        assert_eq!(result["changed_files"], serde_json::json!(MAX_EDIT_FILES));
        assert_eq!(
            std::fs::read_to_string(root.join("f000.txt")).unwrap(),
            (0..(MAX_EDIT_OPS / MAX_EDIT_FILES))
                .map(|index| format!("b{index}"))
                .collect::<Vec<_>>()
                .join(" ")
        );
        let too_many_files = (0..=MAX_EDIT_FILES)
            .map(|file| {
                let path = format!("f{file:03}.txt");
                std::fs::write(root.join(&path), "a0").unwrap();
                edit_op(Some(&path), "a0", "b0")
            })
            .collect::<Vec<_>>();
        let error = machine
            .edit_impl(EditArgs {
                edits: too_many_files,
            })
            .await
            .unwrap_err()
            .to_string();
        assert!(
            error.contains(&format!(
                "{} files, above the {MAX_EDIT_FILES} file limit",
                MAX_EDIT_FILES + 1
            )),
            "{error}"
        );
        assert_eq!(
            std::fs::read_to_string(root.join("f000.txt")).unwrap(),
            "a0"
        );
    }

    #[test]
    fn edit_helpers_detect_line_prefixes_and_numbers() {
        assert!(has_line_number_prefix("12\tfn main()"));
        assert!(!has_line_number_prefix("fn main()"));
        assert!(!has_line_number_prefix("\tfn main()"));
        assert_eq!(line_number("a\nb\nc", 0), 1);
        assert_eq!(line_number("a\nb\nc", 2), 2);
        assert_eq!(line_number("a\nb\nc", 4), 3);
    }

    #[tokio::test]
    async fn edit_line_delta_is_signed_when_a_file_shrinks() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        std::fs::write(root.join("a.txt"), "one\ntwo\nthree\n").unwrap();
        let machine = test_machine(root.clone());

        let receipt = machine
            .edit_impl(edit_args(Some("a.txt"), vec![edit_op(None, "two\n", "")]))
            .await
            .unwrap();
        let receipt: Value = serde_json::from_str(&receipt).unwrap();
        assert_eq!(receipt["files"][0]["line_delta"], serde_json::json!(-1));
        assert_eq!(
            std::fs::read_to_string(root.join("a.txt")).unwrap(),
            "one\nthree\n"
        );
    }

    #[tokio::test]
    async fn concurrent_edits_serialize_per_path() {
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
                let old = format!("token{index}");
                let new = format!("CHANGED{index}");
                machine
                    .edit_impl(edit_args(Some("f.txt"), vec![edit_op(None, &old, &new)]))
                    .await
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

    #[test]
    fn the_advertised_description_states_the_enforced_limits() {
        let tool = LocalMachine::all_tools().get("edit").cloned().unwrap();
        let description = tool.description.clone().unwrap_or_default();
        for limit in [MAX_EDIT_FILES, MAX_EDIT_OPS] {
            assert!(
                description.contains(&limit.to_string()),
                "the edit description must advertise its {limit} limit: {description}"
            );
        }
    }

    #[tokio::test]
    async fn edit_receipt_is_trimmed_before_commit_and_tunnel_envelope_fits() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let mut dir = root.clone();
        for _ in 0..4 {
            dir = dir.join("x".repeat(200));
            std::fs::create_dir(&dir).unwrap();
        }
        let mut ops = Vec::new();
        let mut files = Vec::new();
        for index in 0..MAX_EDIT_FILES {
            let name = dir.join(format!("file{index:03}.txt"));
            std::fs::write(&name, "before\n").unwrap();
            ops.push(edit_op(Some(name.to_str().unwrap()), "before", "after"));
            files.push(name);
        }
        let mut machine = test_machine(root);
        machine.config.max_output_bytes = 1024;
        let text = machine.edit_impl(EditArgs { edits: ops }).await.unwrap();
        assert!(
            text.len() <= machine.config.max_output_bytes,
            "{}",
            text.len()
        );
        let receipt: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(receipt["edits_applied"], MAX_EDIT_FILES);
        assert_eq!(receipt["changed_files"], MAX_EDIT_FILES);
        assert_eq!(receipt["truncated"], true);
        let visible = receipt["files"].as_array().unwrap().len();
        assert!(visible < MAX_EDIT_FILES && receipt["omitted_files"] == MAX_EDIT_FILES - visible);
        assert!(
            files
                .iter()
                .all(|path| std::fs::read_to_string(path).unwrap() == "after\n")
        );
        // The actual tunnel response includes an additional JSON envelope and
        // string escaping; its centrally derived ceiling must still fit.
        let encoded = serde_json::to_vec(&serde_json::json!({
            "jsonrpc":"2.0", "id":1,
            "result":{"content":[{"type":"text", "text":text}]}
        }))
        .unwrap();
        assert!(encoded.len() <= machine.max_response_body_bytes());
    }

    #[tokio::test]
    async fn edit_rejects_aggregate_source_and_replacement_growth_before_writing() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let file = root.join("large.txt");
        let mut machine = test_machine(root.clone());
        machine.config.max_read_bytes = MAX_MUTATION_PLAN_BYTES;
        machine.config.max_write_bytes = MAX_MUTATION_PLAN_BYTES * 2;
        // One 32 MiB+1 source cannot fit the aggregate with its editable copy.
        std::fs::write(&file, vec![b'x'; MAX_MUTATION_PLAN_BYTES / 2 + 1]).unwrap();
        let error = machine
            .edit_impl(edit_args(Some("large.txt"), vec![edit_op(None, "x", "z")]))
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("aggregate plan budget"), "{error}");
        assert_eq!(
            std::fs::metadata(&file).unwrap().len(),
            (MAX_MUTATION_PLAN_BYTES / 2 + 1) as u64
        );
        // A smaller original file can nevertheless over-expand by replace_all:
        // validate its size *before* allocating the replacement buffer.
        std::fs::write(&file, vec![b'x'; MAX_MUTATION_PLAN_BYTES / 4]).unwrap();
        let mut op = edit_op(Some("large.txt"), "x", "xxxx");
        op.replace_all = true;
        let error = machine
            .edit_impl(EditArgs { edits: vec![op] })
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("aggregate mutation plan"), "{error}");
        assert_eq!(
            std::fs::metadata(&file).unwrap().len(),
            (MAX_MUTATION_PLAN_BYTES / 4) as u64
        );
    }

    #[tokio::test]
    async fn edit_skips_writing_a_file_whose_text_did_not_change() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let path = root.join("a.txt");
        std::fs::write(&path, "same\n").unwrap();
        let before = std::fs::metadata(&path).unwrap().modified().unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        let machine = test_machine(root);

        let receipt: Value = serde_json::from_str(
            &machine
                .edit_impl(edit_args(
                    Some("a.txt"),
                    vec![edit_op(None, "same", "same")],
                ))
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(receipt["edits_applied"], serde_json::json!(1));
        assert_eq!(receipt["changed_files"], serde_json::json!(0));
        assert_eq!(receipt["files"], serde_json::json!([]));
        assert_eq!(
            std::fs::metadata(&path).unwrap().modified().unwrap(),
            before,
            "a no-op edit must not touch the file"
        );
    }
}
