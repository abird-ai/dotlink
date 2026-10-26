# DL-140 — Post-review correctness and work limits

Author: ChatGPT GPT-6
Co-authors: none
Owner: @agent

## Scope

Resolve the four October 26 review findings without redesigning dotlink's
permission model, transport interfaces or local execution parallelism.

## Decisions

- **Configuration-only inspection:** `--print-id` reads a migrated JSONC
  profile or default-profile tunnel ID override. It never loads a Runtime
  key, starts transports, or invokes interactive setup. A known ID remains
  visible even while the active Runtime credential needs repair.
- **Text classification:** one UTF-8 + binary-control predicate belongs to
  `src/mcp/tools/mod.rs`. `read`, `write`, `edit`, `patch`, and `grep` use
  it consistently. Binary files belong to `bread`, `bwrite`, `bedit`.
  Paged reads stream skipped lines, rejecting encountered binary controls
  without buffering giant skipped text.
- **OAuth issuance:** a one-time code remains locked through validation and
  token issuance, and is consumed only on success. Token generation and the
  in-memory access-token lock are acquired before persistent refresh-grant
  mutation so there are no fallible post-commit steps. Storage failures
  return retryable HTTP 503 rather than an invalid-grant error; a concurrent
  second code redemption still fails as one-time.
- **Bounded search:** `grep` retains the existing 100,000-entry discovery
  budget, adds a separate 32 MiB/4096-candidate per-request content work
  budget, and reports `scan_incomplete`/`incomplete_reason` in JSON or an
  explicit content marker. This is not a global tool admission budget.
- **Intentionally open local concurrency:** HTTP and stdio retain
  model-directed parallelism, as requested.

## Validation boundaries

Linux unit/integration tests, formatting, Clippy, flake evaluation and Markdown
lint are available. Native macOS/Windows runtime and nested Bubblewrap
smoke tests remain gated by their platform permissions.
