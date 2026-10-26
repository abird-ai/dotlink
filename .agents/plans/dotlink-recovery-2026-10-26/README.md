# DL-120 — Credential and OAuth reliability

Author: ChatGPT GPT-6
Co-authors: none
Owner: @agent

## Scope

Implement the seven findings from the fresh review of commit `81a1c9f`,
preserving dotlink's existing transport and permission architecture.

## Design

- Profile reading distinguishes **execution** (strict selected key) from
  **management** (missing key is repairable). The on-disk JSONC selector
  remains authoritative; never silently switch to an unselected legacy key.
- Private credential and OAuth state generations are selected by one atomic
  profile rename, then pruned only after a durable **credential rotation**.
  Keep selected and immediately preceding references, including across
  unrelated profile edits that might overlap a running process. A failed
  post-commit cleanup warns, never misreports the completed write as a failure.
- Profile deletion acquires its lock before checking every artifact, including
  orphaned versioned keys; retries safely finish an interrupted cleanup.
- OAuth token and authorization-code consumption follows **validate, then
  consume** under the corresponding lock. Malformed requests cannot exhaust a
  valid credential.
- Argon2 work holds the OAuth-specific concurrency permit in its blocking
  worker, not in the cancelable HTTP request.
- Prepare both owned private XDG directories before writable shell mounting.
  Preserve fail-closed policy for missing paths dotlink does not own.
- One safe protocol-name formatter is used by stdio and tunnel logging.
- Local HTTP and stdio tool execution **intentionally has no admission cap**.
  This review does not add one.

## Platform limits

Native Windows/macOS and the opt-in Bubblewrap smoke test depend on
external platform permissions. Nix flake evaluation alone is not a cross-build
or native runtime test. See TODO.md.
