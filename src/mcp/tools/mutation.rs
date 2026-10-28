//! Shared mutation work and reply contracts. Format-specific tools plan edits;
//! this layer bounds retained buffers and ensures replies fit *before* writes.

use super::{LocalMachine, Verified};
use crate::mcp::PendingWrite;
use anyhow::{Result, anyhow, bail};
use rmcp::model::Tool;
use serde::{
    Deserialize, Deserializer,
    de::{Error as _, SeqAccess, Visitor},
};
use serde_json::{Map, Value};
use sha2::{Digest as _, Sha256};
use std::sync::Arc;
use std::{
    fmt,
    io::{Read, Write},
    path::PathBuf,
};

/// One request's simultaneously retained original and edited/decoded bytes.
/// Accounting is for live logical buffers, not the number of tool operations.
#[derive(Default)]
pub(crate) struct PlanBudget {
    source: usize,
    work: usize,
}

impl PlanBudget {
    pub(crate) fn remaining(&self) -> usize {
        super::MAX_MUTATION_PLAN_BYTES.saturating_sub(self.source.saturating_add(self.work))
    }

    /// Each source must fit beside its working copy.
    pub(crate) fn copy_read_limit(&self, per_file: usize) -> Result<usize> {
        let limit = (self.remaining() / 2).min(per_file);
        if limit == 0 {
            bail!(
                "aggregate mutation plan exceeds {} bytes",
                super::MAX_MUTATION_PLAN_BYTES
            );
        }
        Ok(limit)
    }

    pub(crate) fn add(&mut self, source: usize, work: usize) -> Result<()> {
        let new_source = self
            .source
            .checked_add(source)
            .ok_or_else(|| anyhow!("mutation work size overflow"))?;
        let new_work = self
            .work
            .checked_add(work)
            .ok_or_else(|| anyhow!("mutation work size overflow"))?;
        Self::ensure(new_source, new_work)?;
        self.source = new_source;
        self.work = new_work;
        Ok(())
    }

    pub(crate) fn replace_work(&mut self, before: usize, after: usize) -> Result<()> {
        let new_work = self
            .work
            .checked_sub(before)
            .and_then(|used| used.checked_add(after))
            .ok_or_else(|| anyhow!("mutation work size overflow"))?;
        Self::ensure(self.source, new_work)?;
        self.work = new_work;
        Ok(())
    }

    fn ensure(source: usize, work: usize) -> Result<()> {
        if source
            .checked_add(work)
            .is_none_or(|used| used > super::MAX_MUTATION_PLAN_BYTES)
        {
            bail!(
                "aggregate mutation plan exceeds {} bytes",
                super::MAX_MUTATION_PLAN_BYTES
            );
        }
        Ok(())
    }
}

/// Streaming binary replacements never need to retain the original source or
/// a full output buffer. The staged writer below can be reused by any tool
/// whose edits refer to offsets in the *original* file.
pub(crate) struct ByteSplice<'a> {
    pub(crate) offset: usize,
    pub(crate) end: usize,
    pub(crate) data: &'a [u8],
}

const STREAM_CHUNK_BYTES: usize = 64 * 1024;

/// Hash a pinned regular file with bounded IO. The extra byte probe rejects
/// concurrent growth without ever buffering a large binary source.
pub(crate) fn digest_limited(source: &mut impl Read, limit: usize) -> Result<(usize, [u8; 32])> {
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; STREAM_CHUNK_BYTES];
    let mut total = 0_usize;
    loop {
        let requested = limit
            .saturating_sub(total)
            .saturating_add(1)
            .min(buffer.len());
        let size = source.read(&mut buffer[..requested])?;
        if size == 0 {
            break;
        }
        total = total
            .checked_add(size)
            .ok_or_else(|| anyhow!("binary input size overflow"))?;
        if total > limit {
            bail!("file exceeded the {limit} byte binary edit limit");
        }
        hasher.update(&buffer[..size]);
    }
    Ok((total, hasher.finalize().into()))
}

/// Stream only the requested changed ranges to the staged file, hashing every
/// source byte even across deleted spans. A changed or grown source aborts
/// staging; the backend rechecks the original digest again at commit.
/// Returns whether the resulting bytes differ from the original snapshot.
pub(crate) fn stream_splice(
    source: &mut impl Read,
    output: &mut impl Write,
    expected: (usize, [u8; 32]),
    splices: &[ByteSplice<'_>],
) -> Result<bool> {
    fn source_chunk(
        source: &mut impl Read,
        output: &mut impl Write,
        remaining: usize,
        original: &mut Sha256,
        replacement: &mut Sha256,
        copy: bool,
    ) -> Result<()> {
        let mut buffer = [0_u8; STREAM_CHUNK_BYTES];
        let mut remaining = remaining;
        while remaining > 0 {
            let requested = remaining.min(buffer.len());
            let size = source.read(&mut buffer[..requested])?;
            if size == 0 {
                bail!("binary source changed during staged edit");
            }
            original.update(&buffer[..size]);
            if copy {
                output.write_all(&buffer[..size])?;
                replacement.update(&buffer[..size]);
            }
            remaining -= size;
        }
        Ok(())
    }

    let mut old_hash = Sha256::new();
    let mut new_hash = Sha256::new();
    let (source_len, digest) = expected;
    let (mut cursor, mut written) = (0_usize, 0_usize);
    for splice in splices {
        if splice.offset < cursor || splice.end < splice.offset || splice.end > source_len {
            bail!("invalid binary edit range after ordering");
        }
        let unchanged = splice.offset - cursor;
        source_chunk(
            source,
            output,
            unchanged,
            &mut old_hash,
            &mut new_hash,
            true,
        )?;
        written = written
            .checked_add(unchanged)
            .ok_or_else(|| anyhow!("binary output size overflow"))?;
        source_chunk(
            source,
            output,
            splice.end - splice.offset,
            &mut old_hash,
            &mut new_hash,
            false,
        )?;
        output.write_all(splice.data)?;
        new_hash.update(splice.data);
        written = written
            .checked_add(splice.data.len())
            .ok_or_else(|| anyhow!("binary output size overflow"))?;
        cursor = splice.end;
    }
    source_chunk(
        source,
        output,
        source_len - cursor,
        &mut old_hash,
        &mut new_hash,
        true,
    )?;
    written = written
        .checked_add(source_len - cursor)
        .ok_or_else(|| anyhow!("binary output size overflow"))?;
    let mut extra = [0];
    if source.read(&mut extra)? != 0 || <[u8; 32]>::from(old_hash.finalize()) != digest {
        bail!("binary source changed during staged edit");
    }
    Ok(written != source_len || <[u8; 32]>::from(new_hash.finalize()) != digest)
}

/// Shared verified replacement/create, with only the tool-specific receipt
/// record provided by its caller. Both `edit` and `patch` run their staged
/// multi-file commits through the same all-stage-before-any-commit engine.
pub(crate) enum MutationTarget {
    Update(Verified),
    Create(PathBuf),
}

pub(crate) struct PlannedMutation {
    pub(crate) target: MutationTarget,
    pub(crate) display: String,
    pub(crate) bytes: Vec<u8>,
    pub(crate) record: Value,
}

#[derive(Default)]
pub(crate) struct MutationOutcome {
    pub(crate) records: Vec<Value>,
    pub(crate) failures: Vec<(String, String)>,
}

pub(crate) async fn stage_and_commit(
    machine: &LocalMachine,
    verb: &str,
    best_effort: bool,
    files: Vec<PlannedMutation>,
) -> Result<MutationOutcome> {
    let mut staged: Vec<(String, Value, PendingWrite)> = Vec::with_capacity(files.len());
    let mut outcome = MutationOutcome::default();
    for PlannedMutation {
        target,
        display,
        bytes,
        record,
    } in files
    {
        let pending = match target {
            MutationTarget::Update(verified) => verified.stage(machine, &bytes).await,
            MutationTarget::Create(path) => {
                machine.stage_create_write(&path, &bytes).await.map(Some)
            }
        };
        match pending {
            Ok(Some(pending)) => staged.push((display, record, pending)),
            Ok(None) => {} // Byte-identical update: no write, no receipt entry.
            Err(error) if best_effort => outcome.failures.push((display, error.to_string())),
            Err(error) => {
                // Dropping every staged tempfile before returning leaves all
                // host files untouched; the first commit has not run.
                bail!("{display}: {verb} staging failed; nothing was written: {error}");
            }
        }
    }
    for (display, record, pending) in staged {
        match machine.commit_write(pending).await {
            Ok(()) => outcome.records.push(record),
            Err(error) if best_effort => outcome.failures.push((display, error.to_string())),
            Err(error) => bail!(
                "{display}: commit failed after staging; earlier files in this {verb} may already have been replaced: {error}"
            ),
        }
    }
    Ok(outcome)
}

/// A bounded successful multi-file response. Construct before the first stage
/// or commit; `finish` is deliberately infallible after any number of writes.
/// Preserve the complete receipt if it fits; on overflow, prioritize failure
/// diagnostics and show the largest prefix of successful file details next.
pub(crate) struct MutationReceipt {
    ceiling: usize,
    metadata: Map<String, Value>,
}

impl MutationReceipt {
    pub(crate) fn new(ceiling: usize, metadata: Map<String, Value>) -> Result<Self> {
        let receipt = Self { ceiling, metadata };
        // Verify the minimal truncated response *before* changing any files.
        if receipt.render(&[], &[], 1, 1).len() > ceiling {
            bail!(
                "mutation response metadata exceeds the {ceiling}-byte output limit; nothing was written"
            );
        }
        Ok(receipt)
    }

    fn render(
        &self,
        files: &[Value],
        failures: &[Value],
        omitted_files: usize,
        omitted_failures: usize,
    ) -> String {
        let mut result = self.metadata.clone();
        result.insert("files".into(), Value::Array(files.to_vec()));
        if !failures.is_empty() || omitted_failures > 0 {
            result.insert("failures".into(), Value::Array(failures.to_vec()));
        }
        if omitted_files > 0 || omitted_failures > 0 {
            result.insert("truncated".into(), Value::Bool(true));
            if omitted_files > 0 {
                result.insert("omitted_files".into(), Value::from(omitted_files));
            }
            if omitted_failures > 0 {
                result.insert("omitted_failures".into(), Value::from(omitted_failures));
            }
        }
        Value::Object(result).to_string()
    }

    pub(crate) fn finish(&self, files: &[Value], failures: &[Value]) -> String {
        let full = self.render(files, failures, 0, 0);
        if full.len() <= self.ceiling {
            return full;
        }

        // Bounded diagnostic previews should not disappear behind long paths.
        // Binary search is safe because prefix JSON length grows monotonically.
        let keep_failures = super::largest_fitting_prefix(failures.len(), |n| {
            self.render(&[], &failures[..n], files.len(), failures.len() - n)
                .len()
                <= self.ceiling
        });
        let keep_files = super::largest_fitting_prefix(files.len(), |n| {
            self.render(
                &files[..n],
                &failures[..keep_failures],
                files.len() - n,
                failures.len() - keep_failures,
            )
            .len()
                <= self.ceiling
        });
        let result = self.render(
            &files[..keep_files],
            &failures[..keep_failures],
            files.len() - keep_files,
            failures.len() - keep_failures,
        );
        debug_assert!(result.len() <= self.ceiling);
        result
    }
}

/// Single-file receipts contain no unbounded arrays. Omit long path labels
/// rather than failing after the mutation or misrepresenting its outcome.
pub(crate) fn compact_receipt(mut value: Value, ceiling: usize) -> Result<String> {
    let full = value.to_string();
    if full.len() <= ceiling {
        return Ok(full);
    }
    let Some(object) = value.as_object_mut() else {
        bail!("mutation response exceeds configured output limit; nothing was written");
    };
    if object.remove("path").is_some() {
        object.insert("path_omitted".into(), Value::Bool(true));
    }
    let compact = value.to_string();
    if compact.len() > ceiling {
        bail!("mutation response exceeds configured output limit; nothing was written");
    }
    Ok(compact)
}

pub(crate) fn compact_text_receipt(
    full: String,
    compact: String,
    ceiling: usize,
) -> Result<String> {
    if full.len() <= ceiling {
        return Ok(full);
    }
    if compact.len() <= ceiling {
        return Ok(compact);
    }
    bail!("mutation response exceeds configured output limit; nothing was written")
}

/// Keep advertised array bounds in sync with the wire decoder from one
/// constant. Both text `edit` and binary `bedit` use this schema adjustment.
pub(crate) fn set_array_max_items(tool: &mut Tool, field: &str, max_items: usize) {
    if let Some(array) = Arc::make_mut(&mut tool.input_schema)
        .get_mut("properties")
        .and_then(Value::as_object_mut)
        .and_then(|properties| properties.get_mut(field))
        .and_then(Value::as_object_mut)
    {
        array.insert("maxItems".into(), Value::from(max_items));
    }
}

/// Bound a wire array while deserializing, not after materializing untrusted
/// elements. Used by both text and binary multi-operation editors.
pub(crate) fn bounded_array<'de, D, T, const MAX: usize>(
    de: D,
    tool: &'static str,
    unit: &'static str,
) -> std::result::Result<Vec<T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    struct Bounded<T, const MAX: usize>(std::marker::PhantomData<T>, &'static str, &'static str);
    impl<'de, T: Deserialize<'de>, const MAX: usize> Visitor<'de> for Bounded<T, MAX> {
        type Value = Vec<T>;
        fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
            write!(f, "at most {MAX} {}", self.2)
        }
        fn visit_seq<A: SeqAccess<'de>>(
            self,
            mut seq: A,
        ) -> std::result::Result<Self::Value, A::Error> {
            if seq.size_hint().is_some_and(|n| n > MAX) {
                return Err(A::Error::custom(format!(
                    "{}: at most {MAX} {} per call",
                    self.1, self.2
                )));
            }
            let mut values = Vec::new();
            while let Some(value) = seq.next_element::<T>()? {
                if values.len() == MAX {
                    return Err(A::Error::custom(format!(
                        "{}: at most {MAX} operations per call",
                        self.1
                    )));
                }
                values.push(value);
            }
            Ok(values)
        }
    }
    de.deserialize_seq(Bounded::<T, MAX>(std::marker::PhantomData, tool, unit))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn shared_receipt_retains_failure_totals_without_exceeding_ceiling() {
        let mut meta = Map::new();
        meta.insert("mode".into(), json!("best_effort"));
        let receipt = MutationReceipt::new(1024, meta).unwrap();
        let files = (0..32)
            .map(|i| json!({"path":format!("a/{i}/{}", "x".repeat(300))}))
            .collect::<Vec<_>>();
        let failures = (0..14)
            .map(|i| json!({"path":format!("bad/{i}"),"error":"x".repeat(2048)}))
            .collect::<Vec<_>>();
        let result: Value = serde_json::from_str(&receipt.finish(&files, &failures)).unwrap();
        let reported = result["files"].as_array().unwrap().len();
        let errors = result["failures"].as_array().unwrap().len();
        assert_eq!(
            result["omitted_files"].as_u64().unwrap() as usize + reported,
            files.len()
        );
        assert_eq!(
            result["omitted_failures"].as_u64().unwrap() as usize + errors,
            failures.len()
        );
        assert!(result["truncated"].as_bool().unwrap());
        assert!(result.to_string().len() <= 1024);
    }

    #[test]
    fn budget_prevents_growth_and_stays_unchanged_after_rejection() {
        let mut budget = PlanBudget::default();
        budget.add(1024, 1024).unwrap();
        assert!(
            budget
                .replace_work(1024, super::super::MAX_MUTATION_PLAN_BYTES)
                .is_err()
        );
        assert_eq!(
            budget.remaining(),
            super::super::MAX_MUTATION_PLAN_BYTES - 2048
        );
    }
}
