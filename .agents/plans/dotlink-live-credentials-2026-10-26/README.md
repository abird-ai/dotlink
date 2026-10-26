# DL-130 — Live credential selection and recovery

Author: ChatGPT GPT-6
Co-authors: none
Owner: @agent

## Scope

Resolve three fresh review findings against `713f017` without changing
tool concurrency, permission policy, or the established profile schema.

## Design

- **OpenAI credential source:** persisted JSONC describes whether the transport
  is active and its selected Runtime-key generation. Apply explicit environment
  and file-based overrides first. If OpenAI remains enabled and no override key
  exists, resolve the saved generation strictly. If OpenAI is inactive, do not
  open its saved key. Continue to reject missing active keys without an override.
  An explicit `DOTLINK_API_KEY_FILE` may reuse a saved tunnel ID; complete
  `DOTLINK_ID`/`DOTLINK_API_KEY` pairs retain their existing rules.
- **Live OAuth selection:** selection has three explicit states: absent profile
  (environment-only legacy), saved legacy profile, and saved versioned
  profile. At startup the selected hash must exist and parse. On each owner
  password verification, re-read the committed profile selection and refreshed
  OAuth state. If the selected revision changes during successful hashing,
  refuse that approval, rather than authorizing an already-rotated password.
  A previously saved profile disappearing always fails closed; no implicit
  legacy downgrade. Existing refresh and short-lived access tokens retain
  their separately documented lifecycles.
- **Scope restraint:** reuse existing state-file readers and the cancellation-
  safe Argon2 verifier. No background polling or new shared admission budget.
  Local stdio/HTTP model-directed concurrency remains intentionally open.
- **Tests:** isolated CLI subprocess regression for env/file precedence and
  inactive unreadable Runtime keys; end-to-end HTTP registration/approval
  across owner rotation and profile deletion. No production secrets involved.

## Platform gates

Linux unit/integration suite, Clippy, formatting, Nix evaluation and Markdown
lint are available here. Native macOS/Windows and nested Bubblewrap runtime
checks remain gated by platform access rather than claimed passing.
