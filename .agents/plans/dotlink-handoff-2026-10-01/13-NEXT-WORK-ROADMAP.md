# Next-work roadmap

Date: 2026-10-01

The current 0.6.0 implementation phase is complete. This roadmap separates **operational next steps** from **optional future product work** so a future agent does not treat optional ideas as unfinished bugs.

## Immediate operational next steps

### 1. Push current local commits when the user wants

At handoff creation local main is ahead of origin.

Verify first:

```bash
git status --short --branch
git log --oneline --decorate -8
```

Then, only if explicitly requested:

```bash
git push origin main
```

Do not push merely because this handoff says the code is ready.

### 2. Publish v0.6.0 when desired

After main is pushed and live GitHub state confirms the 0.6.0 implementation commit:

```bash
git tag -a v0.6.0 -m "abird dotlink v0.6.0"
git push origin v0.6.0
```

The workflow should:

- build `release-all`;
- validate;
- create/update GitHub Release;
- attach individual binary/checksum assets.

Afterward verify:

- GitHub Release exists;
- latest release points to v0.6.0;
- installer latest URLs resolve;
- all platform assets present.

### 3. Refresh live ChatGPT MCP App

After deploying the final binary:

- restart dotlink profile;
- go to ChatGPT plugin/MCP settings;
- refresh tools if permissions/tool surface changed;
- smoke read;
- smoke mutation if profile permits;
- smoke shell if profile permits.

The README FAQ documents "Refresh tools".

### 4. Claude.ai remote OAuth smoke

Useful final external interoperability check:

- stable ngrok domain;
- stable `/mcp`;
- add custom connector in Claude.ai;
- complete owner OAuth;
- call one read tool;
- restart dotlink;
- verify refresh token recovers a new access token.

This is operational interoperability validation, not a known implementation gap.

## Optional near-term improvements

### A. Structural module split

Current behavior is good but some files are large:

- oauth.rs;
- mcp.rs;
- setup.rs.

A future maintenance-only refactor can split them by responsibility.

Do it only:

- one module at a time;
- no behavior changes mixed in;
- tests green after every move;
- public/current docs unchanged unless module map changes.

Suggested structure is in `04-PRODUCT-ARCHITECTURE-SPEC.md`.

### B. Native Windows CI smoke

Current Windows binaries are cross-built and PE-validated.

Optional stronger CI:

- Windows runner;
- run `dotlink.exe --version`;
- PowerShell real installer against local fixture/release;
- maybe local stdio MCP initialize.

Keep build graph Nix-authoritative; native runner is runtime smoke only.

### C. Native macOS runtime smoke

Current macOS ARM64 binary is cross-built and Mach-O validated.

Optional:

- ARM64 macOS CI runner;
- `--version`;
- local stdio/HTTP smoke;
- eventual signing/notarization.

### D. Release signatures/provenance

Potential:

- detached signatures;
- SBOM;
- SLSA/provenance;
- GitHub attestations;
- macOS signing/notarization.

Do not replace reproducible Nix/checksum flow; add on top.

### E. Private dotlink-owned build caches

Current cache sharing can point at the user's normal host cache.

A future option could maintain profile-private caches:

```text
$XDG_CACHE_HOME/abird/dotlink/profiles/<profile>/cargo
$XDG_CACHE_HOME/abird/dotlink/profiles/<profile>/npm
...
```

Benefits:

- persistent cache;
- sandbox cannot corrupt user's normal cache;
- easier trust boundary.

This should be additive, not a replacement unless user asks.

### F. Optional terminal approval for OAuth

Current owner consent uses password form.

A future convenience option could show:

```text
OAuth request from ChatGPT
A approve
D deny
```

in interactive non-stdio mode.

Requirements:

- must not be only auth path;
- headless/browser flow must still work;
- do not weaken owner authentication;
- avoid coupling terminal controls to HTTP lifecycle excessively.

### G. Browser owner session

Current authorization asks owner password per flow.

A short-lived Secure/HttpOnly/SameSite owner session could reduce repeated entry.

Tradeoffs:

- additional CSRF/session lifecycle complexity;
- more state/cookies;
- must not become persistent weak auth.

Not required now.

### H. OAuth management improvements

Possible:

- show approved client redirect/resource metadata more richly;
- revoke access tokens in-process through runtime control/API;
- explicit refresh-grant list;
- expiry display.

Keep management simple unless real need appears.

### I. Nix daemon explicit capability

Only if a real workflow demands it.

Potential:

```text
allow-nix-daemon
deny-nix-daemon
```

Default must remain hidden/denied.

Requires careful threat review because daemon can materially expand authority.

### J. Intel macOS

Current public macOS release is ARM64 only.

If demand appears:

- choose a supported pinned toolchain/SDK strategy;
- do not re-add a flake system known unsupported by current nixpkgs;
- validate runtime on real Intel macOS.

## Longer-term product directions

### Multi-user/hosted dotlink

This is a product boundary change.

If pursued:

- do not keep extending embedded single-owner OAuth into a general IdP;
- use mature identity provider;
- add user/tenant authorization above LocalMachine;
- revisit state layout, logging, audit, revocation and secrets.

### Per-client local policy

Today OAuth client authentication does not map to distinct filesystem profiles.

A future advanced model could bind an approved OAuth client to a named dotlink policy/profile.

Only do this with a clear UX. Avoid creating two conflicting permission systems.

### Signed policy / managed enterprise profiles

Potential enterprise direction:

- centrally distributed policy;
- local verification;
- restricted editable fields;
- audit logs.

Not part of current personal self-hosted design.

## Things not to do without explicit product change

- do not make public HTTP unauthenticated by default;
- do not persist the unsafe public-no-auth flag;
- do not mount whole home for cache convenience;
- do not expose Nix daemon by default;
- do not turn cache grants into MCP file grants;
- do not let transport choose local authority;
- do not add text/binary mode overloads to read/write;
- do not restore `fs_*` naming;
- do not silently enable shell network;
- do not remove Bubblewrap default on Linux;
- do not treat ephemeral URL as authentication;
- do not infer OAuth issuer from Host/forwarded headers;
- do not store owner password/refresh plaintext;
- do not remove cross-process OAuth state locking;
- do not weaken lexical + canonical deny checks;
- do not let future tools bypass the operation gate;
- do not move release-target logic into CI YAML;
- do not add distro-specific Linux release duplicates;
- do not claim 0.6.0 released until GitHub confirms it.

## Suggested next-agent behavior

If the user gives no new feature request:

1. inspect current status;
2. do not invent more work;
3. help with operational push/release/live connector verification;
4. treat the implementation phase as complete.

If the user asks for a feature:

1. identify the owning module from `12-FILE-MAP.md`;
2. identify invariants affected;
3. add regression tests first for security-sensitive changes;
4. run targeted validation;
5. run broader Rust/Nix matrix if release/platform surface changed;
6. update current docs;
7. produce a new handoff only if a substantial phase warrants it.
