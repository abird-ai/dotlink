# DL-110 — Reliability review remediation

Author: ChatGPT GPT-6
Co-authors: none
Owner: @agent

## Scope

Remediation of the October 26 adversarial source review at base `55f8ef5`:
filesystem scan completeness, diagnostic payload isolation, absent control-plane
paths, shell lifecycle, profile/credential commit semantics, text paging,
Windows process-job association and regression validation.

## Design and decisions

- **Local admission intentionally unrestricted.** HTTP/stdio model-directed
  parallelism is an explicit product choice, **not an unresolved audit issue**.
  Only the OpenAI tunnel has transport-specific in-flight throttling; tool byte
  ceilings and timeouts remain.
- **Discovery:** a shared 100,000-entry per-request visit limit bounds work.
  Globally sorted output is guaranteed only on a complete walk. JSON returns
  `scan_incomplete`; content searches return an incomplete-scan marker. Narrow
  the path/glob for complete answers. Avoid falsely promising deterministic
  continuation across mutated directory trees.
- **Private shell paths:** mask absent protected paths within Bubblewrap as well
  as existing directories/files; handle read and write grants. Unsandboxed
  execution intentionally has a weaker isolation boundary.
- **Process lifetime:** request-owned task handles abort on cancellation.
  Windows children begin suspended, join a Job Object, then resume. Unix
  process-group termination is best-effort for unsandboxed `setsid` children;
  Bubblewrap's PID namespace is the default stronger boundary.
- **Secrets:** immutable per-generation Runtime keys and OAuth owner hashes are
  staged before selecting them via one atomic JSONC profile rename. Old v10
  profiles keep working without selectors. A missing selected live Runtime key
  fails closed; a disabled OpenAI transport can operate without its dormant key.
  Config mutations lock before reading; interactive edits refuse stale profiles.
  Profile deletion clears generated Runtime keys.
- **Scope restraint:** no globally shared tool-admission semaphore, no new
  external service (only a bounded lock in dotlink's private XDG directory), no history rewrite, no release.

## Verification

The native test suite and a subprocess regression for selected runtime keys
exercise the changed boundaries. Native Windows/macOS execution and Bubblewrap
smoke tests depend on external platform permissions; the release workflow
already gates native platform tests. See TODO.md for executed checks and
remaining validation.
