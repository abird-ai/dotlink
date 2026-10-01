# Validation and known limitations

Date: 2026-10-01

This file records the final validation performed against the implementation baseline:

```text
657ebb3 Harden OAuth and runtime safety after review
8f94c87 Add OAuth-protected remote MCP access
```

Always re-run relevant gates after future code changes.

## Final Git state observed before handoff docs

```text
branch: main
working tree: clean
local main: 2 commits ahead of origin/main
origin/main: b5c9d22
local HEAD: 657ebb3
```

The handoff documentation itself may be committed after this baseline.

## Rust validation

Commands:

```bash
cargo fmt --check
cargo test --locked --all-features
cargo clippy --locked --all-targets --all-features -- -D warnings
git diff --check
```

Final result:

```text
137 tests passed
0 failed
Clippy clean with -D warnings
rustfmt clean
diff check clean
```

Coverage includes:

- permission allow/deny logic;
- lexical + canonical deny behavior;
- symlink escape;
- protected control-plane paths;
- write-only/read-only/dynamic tool routing;
- regular-file target enforcement;
- operation-gate pure-read classification;
- Bubblewrap mount composition;
- cache isolation;
- Nix daemon masking;
- shell timeout/reaping;
- config schema v10;
- setup/profile behavior;
- non-loopback public HTTP validation;
- ngrok domain validation;
- transport precedence;
- OpenAI recovery/restart threshold/backoff;
- stdio output/request logging;
- peer transport drain/abort;
- OAuth metadata/discovery;
- PKCE;
- CIMD validation;
- DCR;
- refresh capability semantics;
- access-only clients;
- refresh rotation/replay rejection;
- revocation;
- owner password change reload;
- no global lockout;
- concurrent OAuth state updates.

## Bubblewrap runtime validation

Command:

```bash
DOTLINK_TEST_BWRAP=1   cargo test --locked --all-features mcp::tests::bubblewrap -- --nocapture
```

Result:

```text
4 / 4 passed
```

The opt-in runtime tests exercised real Bubblewrap behavior available on the validation host.

## Shell-script validation

Commands:

```bash
sh -n install.sh
bash -n scripts/build-release-artifacts.sh
shellcheck install.sh scripts/build-release-artifacts.sh
```

Result:

```text
passed
```

## Unix installer validation

Fixture tests covered:

- first install;
- installed command name `dotlink`;
- downloaded asset version identity;
- SHA-256 verification;
- same-release rerun does not rewrite;
- corrupted/old installed binary replaced;
- explicit requested-version mismatch rejected.

Result:

```text
passed
```

## PowerShell installer validation

PowerShell was provided through Nix for the Linux-hosted installer logic test.

Validated:

- PowerShell parser accepts script;
- first install;
- same-release no-op;
- replacement/update;
- temporary replacement file cleanup;
- requested-version mismatch rejection.

Because Linux PowerShell cannot natively execute the real Windows PE, installer logic used an executable release-shaped fixture that reports `dotlink 0.6.0`. The actual Windows binaries were validated separately by PE inspection/cross build.

Result:

```text
passed
```

## GitHub workflow validation

`.github/workflows/build-binaries.yml` was checked with `actionlint` inside a Nix derivation.

Result:

```text
passed
```

## RustSec dependency audit

Tool:

```text
cargo-audit 0.22.2
```

Final scan:

```text
303 dependencies
0 vulnerabilities
3 warnings
```

Warnings:

```text
generational-arena 0.2.9
  RUSTSEC-2024-0014
  unmaintained
  transitive through ngrok

rustls-pemfile 2.2.0
  RUSTSEC-2025-0134
  unmaintained
  transitive through ngrok

yoke-derive 0.8.3
  yanked
  transitive through URL/IDNA dependency graph
```

At review time `ngrok 0.19.0` was still the current published Rust SDK, so there was no clean upstream version upgrade that removed the first two warnings. They are warnings, not known vulnerabilities.

Do not claim "zero warnings"; claim **zero known RustSec vulnerabilities**.

## Nix evaluation

Commands:

```bash
nixfmt --check flake.nix
nix flake check --all-systems --no-build
```

Result:

```text
all checks passed
```

Evaluated systems:

- x86_64-linux;
- aarch64-linux;
- aarch64-darwin.

Evaluated release graph includes all x86_64-linux cross outputs.

## Native Nix checks

Built:

```text
checks.x86_64-linux.package
checks.x86_64-linux.tests
checks.x86_64-linux.clippy
checks.x86_64-linux.fmt
```

All passed.

## Final aggregate release build

Command:

```bash
nix build .#release-all --no-link --print-out-paths
```

Final observed output:

```text
/nix/store/jf02s78fmrpn7dv5abbwpcsa8jh4yfrw-dotlink-release-all
```

The validation environment used a rooted writable Nix store under `target/nix-local-store`; logical store output was mapped to the rooted physical path for byte inspection.

## Final release bundle contents

```text
dotlink-linux-x86_64
dotlink-linux-x86_64.sha256
dotlink-linux-aarch64
dotlink-linux-aarch64.sha256
dotlink-windows-x86_64.exe
dotlink-windows-x86_64.exe.sha256
dotlink-windows-aarch64.exe
dotlink-windows-aarch64.exe.sha256
dotlink-macos-aarch64
dotlink-macos-aarch64.sha256
VERSION
PLATFORMS.txt
```

Approx total output:

```text
~56 MiB
```

## Checksum validation

```bash
sha256sum -c ./*.sha256
```

All five binary sidecars passed.

## Linux artifact validation

### x86_64

- ELF64;
- x86-64;
- `dotlink 0.6.0`;
- no ELF interpreter;
- no dynamic `NEEDED` entries.

### ARM64

- ELF64;
- AArch64;
- no ELF interpreter;
- no dynamic `NEEDED` entries.

Therefore both are static portable Linux release artifacts.

## Windows artifact validation

PE machine IDs:

```text
x86_64  0x8664
ARM64   0xaa64
```

Both passed.

Earlier development also successfully ran the x86_64 Windows artifact under Wine. The final handoff validation relied on the reproducible rebuild + PE checks rather than treating Wine as a mandatory release gate.

## macOS artifact validation

`dotlink-macos-aarch64`:

- Mach-O 64-bit;
- ARM64 CPU type;
- executable file type;
- produced by the pinned Apple SDK/LLVM cross path.

Passed.

## Interactive setup/OAuth state smoke

A fresh temporary XDG environment was used with the actual built binary.

Validated:

- interactive setup completed;
- schema v10 profile created;
- OAuth owner credential entered through hidden prompt;
- config directory mode 0700;
- config file mode 0600;
- OAuth state directory mode 0700;
- OAuth state file mode 0600.

Passed.

## Live HTTP OAuth boundary smoke

Using the actual executable and temporary profile:

- HTTP bound to random loopback port;
- Protected Resource Metadata fetched;
- Authorization Server Metadata fetched;
- resource/issuer values matched live endpoint;
- S256 advertised;
- unauthenticated MCP POST returned 401.

Passed.

## Full OAuth flow validation

The current 137-test suite includes end-to-end in-process flow coverage for:

```text
DCR
→ pending registration
→ owner authorization
→ persistent approval
→ PKCE code exchange
→ access token
→ authenticated resource validation
→ refresh rotation
→ old refresh replay rejected
→ revocation
```

During implementation, a full actual-process DCR/PKCE/authenticated MCP initialize/refresh/revoke smoke was also completed successfully before the final commit.

A later attempt to repeat the entire credential/token exchange in one giant connector shell call was blocked by the external tool harness safety filter; this was not a dotlink failure. The smaller live discovery/401 smoke above passed on the final commit.

## Strict schema smoke

Validated behavior:

- v9 normal startup rejected;
- `--setup` detects strict schema mismatch;
- user can cancel replacement;
- cancellation leaves old config byte-identical.

Passed.

## Final known limitations

### 1. OAuth is single-owner

Current embedded OAuth is intentionally not multi-user.

No:

- account database;
- organizations;
- federation;
- social auth;
- email reset;
- admin roles.

If product becomes multi-user/hosted, use an established IdP rather than expanding embedded auth indefinitely.

### 2. Access-token revocation granularity

Access tokens are memory-only and short-lived.

CLI durable revocation prevents refresh/approved-client reuse, but an already issued access token lives until:

- expiry;
- process restart;
- Ctrl+R.

This is documented behavior.

### 3. OAuth consent has no persistent browser login session

Current consent asks for the owner password per authorization flow.

There is no long-lived owner browser session/cookie.

This is simpler and security-conservative, but less convenient than a session-based UX.

### 4. OAuth token endpoint supports public clients

Current token auth method is:

```text
none
```

PKCE is mandatory.

Private client secret / private_key_jwt token exchange is not implemented.

CIMD negotiation accepts clients whose advertised supported-method set includes `none`.

### 5. Native macOS/Windows shell sandbox

No Bubblewrap-equivalent native shell sandbox.

Secure Windows recommendation:

```text
WSL2 + Linux dotlink
```

Native filesystem tools still enforce Rust policy.

### 6. Intel macOS release

Current published macOS target:

```text
ARM64 only
```

Intel macOS is not a published Nix cross artifact.

### 7. Release signing/notarization

Not currently implemented:

- binary signatures;
- macOS notarization;
- SBOM;
- provenance attestations.

Checksums + reproducible Nix graph exist.

### 8. Same-user external race boundary

The operation gate prevents races caused by dotlink's own concurrent tools.

It does not claim to stop a separate malicious process already running under the same OS account from changing files/symlinks concurrently.

This is outside the stated local threat boundary.

### 9. External provider/service availability

OpenAI Tunnel/ngrok/ChatGPT/Claude behavior is partly external.

Code should handle expected failures cleanly, but provider outages/API changes are not fully controllable.

### 10. ngrok Rust dependency warnings

The RustSec warnings listed above remain transitive upstream warnings.

Monitor future ngrok/URL dependency releases.

### 11. v0.6.0 publication

At handoff creation:

- local implementation is 0.6.0;
- v0.6.0 is not yet a confirmed remote Release/tag.

Do not claim it is released until live GitHub state confirms it.

## Validation rule for future agents

Do not say "fully validated" after only running Cargo tests if the change touches:

- Nix;
- cross-target code;
- installers;
- workflow;
- Bubblewrap;
- OAuth/public ingress.

Use the relevant sections above as the minimum validation matrix.
