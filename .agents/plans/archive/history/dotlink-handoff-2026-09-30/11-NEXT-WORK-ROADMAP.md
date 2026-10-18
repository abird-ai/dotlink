# Recommended continuation roadmap

Date: 2026-09-30

The implementation/release phase described by the original handoff is complete. This file now lists operational follow-up and future product work rather than unfinished recovery work.

## Immediate operational follow-up

### 1. Refresh the live ChatGPT connector

The code/build is final, but the currently connected ChatGPT MCP process may still be an older running binary.

When convenient:

1. rebuild/install the final binary if the connector launch path does not already point at it;
2. restart `dotlink`;
3. refresh the ChatGPT developer connection/tool schema;
4. confirm `tools/list` matches the selected policy;
5. smoke one read call and, when permitted, one shell call;
6. verify quiet default logging, `-v` TOOL activity, and `-vv` REQ diagnostics.

This is a live-process refresh, not an implementation gap.

### 2. Upstream and releases

Canonical upstream / `origin`:

```text
https://github.com/abird-ai/dotlink
```

Installers default to this repository. Future release work should choose the tag/release convention and publish the validated stable assets there; repository/base-URL overrides remain available for forks and mirrors.

## Completed implementation phases

### Runtime/config/security

Complete:

- JSONC profiles;
- schema-v9 default launch-directory read + persistent allow/deny arrays;
- shell/network defaults;
- profile-specific runtime secrets;
- developer cache discovery/sharing;
- cache deny precedence;
- timestamped TOOL logging;
- quiet/verbose/color semantics;
- NixOS/Bubblewrap hardening;
- Nix daemon isolation;
- simplified `--allow-rw=/` and paired `--allow-all --no-sandbox`.

### Build/release

Complete and validated:

- Crane `buildDepsOnly`;
- native package/tests/Clippy/fmt;
- Linux x86_64 musl deps/package/dist;
- Windows x86_64 GNU deps/package/dist;
- stable asset names + SHA-256 sidecars;
- release helper;
- Unix installer;
- PowerShell installer logic;
- Linux static artifact execution;
- Windows PE inspection + Wine execution.

## Future product candidates

These are not required to call the current phase complete.

### CI

Add CI that uses the existing Crane dependency outputs and cross-build outputs, ideally including:

- Linux native fmt/test/Clippy;
- Linux-musl release artifact;
- Windows-GNU artifact;
- Windows-native installer/executable smoke on a Windows runner;
- checksums/artifact upload.

### Built-in HTTP authentication

Add application-layer authentication for public HTTP/ngrok rather than relying on URL secrecy or external ngrok controls.

### Private abird-owned persistent caches

Possible alternative/addition to sharing host caches:

```text
$XDG_CACHE_HOME/abird/dotlink/profiles/<profile>/cargo
$XDG_CACHE_HOME/abird/dotlink/profiles/<profile>/npm
...
```

This could preserve cache persistence without allowing sandboxed code to mutate the user's normal host caches.

### Explicit Nix daemon capability

Only if a real workflow requires it:

```text
allow-nix-daemon
deny-nix-daemon
```

Default must remain denied.

### More release targets

Potential additions:

- aarch64 Linux release artifact;
- Windows ARM64;
- Apple Silicon macOS release artifact;
- an Intel macOS strategy pinned to a supported nixpkgs branch or Cargo-only release;
- signing/notarization;
- SBOM/provenance.

### Cache CLI controls

Only if demand appears:

```text
--cache=<kind>
--deny-cache=<kind>
```

The current typed profile/onboarding model may already be sufficient.

## Things not to casually change

- do not mount the whole home for convenience;
- do not expose the Nix daemon by default;
- do not let cache grants expand MCP filesystem roots;
- do not describe ephemeral URLs as authentication;
- do not restore `fs_*` tool names;
- do not overload text read/write with binary encodings;
- do not enable TOOL logging by default;
- do not reuse `-q` for setup;
- do not silently enable network because a package manager misses a dependency;
- do not weaken path canonicalization or deny precedence to make a test pass;
- keep the canonical upstream as `https://github.com/abird-ai/dotlink`.
