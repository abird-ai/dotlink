# Known limitations

Date: 2026-10-01

Validation receipts are not stored here. Re-run the gates in
`.agents/AGENTS.md` → "Validation before commit" and record results in
`TODO.md` in this directory. This file records only the accepted boundaries of
the current design.

## Known limitations

### 1. OAuth is single-owner

Current embedded OAuth is intentionally not multi-user.

No:

- account database;
- organizations;
- federation;
- social auth;
- email reset;
- admin roles.

If the product becomes multi-user or hosted, use an established identity
provider rather than expanding embedded auth indefinitely.

### 2. Access-token revocation granularity

Access tokens are memory-only and short-lived.

CLI durable revocation prevents refresh and approved-client reuse, but an
already-issued access token lives until:

- expiry;
- process restart;
- Ctrl+R.

This is documented behavior.

### 3. OAuth consent has no persistent browser login session

Current consent asks for the owner password per authorization flow.

There is no long-lived owner browser session or cookie.

This is simpler and security-conservative, but less convenient than a
session-based UX.

### 4. OAuth token endpoint supports public clients

The current token auth method is:

```text
none
```

PKCE is mandatory.

Private client secret or `private_key_jwt` token exchange is not implemented.

CIMD negotiation accepts clients whose advertised supported-method set includes
`none`.

### 5. Native macOS and Windows shell sandbox

There is no Bubblewrap-equivalent native shell sandbox.

Secure Windows recommendation:

```text
WSL2 + Linux dotlink
```

Native filesystem tools still enforce the Rust policy.

### 6. Intel macOS release

The current published macOS target is:

```text
ARM64 only
```

Intel macOS is not a published Nix cross artifact.

### 7. Release signing and notarization

Not currently implemented:

- binary signatures;
- macOS notarization;
- SBOM;
- provenance attestations.

Checksums and the reproducible Nix graph exist.

### 8. Same-user external race boundary

The operation gate prevents races caused by dotlink's own concurrent tools.

It does not claim to stop a separate malicious process already running under the
same OS account from changing files or symlinks concurrently.

This is outside the stated local threat boundary.

### 9. External provider and service availability

OpenAI Tunnel, ngrok, ChatGPT, and Claude behavior is partly external.

Code should handle expected failures cleanly, but provider outages and API
changes are not fully controllable.

### 10. ngrok Rust dependency warnings

RustSec transitive maintenance warnings remain for crates pulled in through
`ngrok`. Monitor future ngrok and URL dependency releases. These are
unmaintained-crate warnings, not known vulnerabilities.

### 11. Release publication

The repository version and the live GitHub Release can diverge until a release
is explicitly published and verified. Do not claim a version is released until
the live GitHub state confirms it.

## Validation rule for future agents

Do not say "fully validated" after only running Cargo tests if the change
touches:

- Nix;
- cross-target code;
- installers;
- workflow;
- Bubblewrap;
- OAuth or public ingress.

Run the relevant gates from `.agents/AGENTS.md` and record the result in the
plan checklist.
