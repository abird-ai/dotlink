# Decision log

This file records decisions that should not be casually reversed without understanding their rationale.

## Product name

**Decision:** `dotlink` is canonical.

Old name `abird-tunnel` is historical only.

Rationale: project supports multiple transports and is no longer tunnel-specific.

## Text tool vocabulary

**Decision:** Pi-like names, no `fs_*` prefixes.

```text
read
write
edit
ls
bash / powershell
```

Rationale: simple model-facing vocabulary reduces tool-selection ambiguity.

## Binary tools

**Decision:** binary remains separate.

```text
read_binary
write_binary
patch_binary
```

Rationale: do not overload text APIs with binary modes/encodings.

## Binary read formats

**Decision:**

```text
mcp
base64
hex
```

`mcp` means best typed MCP content representation, not “native binary.”

## Default authority

**Decision:** the directory where dotlink is launched is the internal relative-path base and is read-only by default. `--no-default-allow` removes the implicit read grant.

Schema v9 is a clean break; older profile schemas are not accepted.

Rationale: keep the safe ergonomic default while collapsing filesystem authority into one allow/deny vocabulary.

## Dynamic tool routing

**Decision:** unavailable tools are omitted from MCP tool list rather than always exposed and rejected later.

Rationale: safer and easier for the model to reason about.

## Linux shell

**Decision:** Bubblewrap sandbox by default.

Rationale: arbitrary shell cannot be constrained reliably by Rust path checks alone.

## Shell network

**Decision:** disabled by default.

`--allow-network` enables normal sandbox network.

Rationale: package/build tooling should not silently imply outbound network authority.

## NixOS support

**Decision:** expose Nix runtime/profile graph RO, not whole home.

Rationale: package tools run while home secrets remain hidden.

## Nix daemon

**Decision:** hide daemon endpoint roots when network is denied.

```text
/nix/var/nix/daemon-socket
/run/nix-daemon
```

Do not target a presumed socket leaf/type.

Rationale: security boundary is daemon endpoint namespace; filesystem layout can change.

## Developer caches

**Decision:** typed shell-only cache grants.

Rationale: solve package-manager usability without expanding MCP filesystem authority.

RW sharing is explicit opt-in because sandboxed code can mutate shared host cache.

## Cache discovery

**Decision:** discover known cache locations only and avoid probes that create caches.

Use env vars, known existing paths and safe read-only queries.

Do not scan entire home.

Do not mount parent config/credential dirs.

## Profiles

**Decision:** reusable named profiles.

```text
config.jsonc
config.work.jsonc
```

Rationale: different projects/trust contexts should be easy to switch.

## JSONC

**Decision:** canonical config format is JSONC, not JSONL.

Support comments and trailing commas in strict schema-v9 JSONC profiles.

Rationale: hand-editable structured config.

## Runtime transport flags

**Decision:** every transport enabled in the selected profile starts automatically. Explicit `--stdio` / `--http` add local transports for one run; `--no-stdio` / `--no-http` suppress profile local transports; `--no-ngrok` suppresses profile ngrok while preserving HTTP.

HTTP bind/ephemeral/ngrok settings are persisted profile defaults and may be overridden for one run.

Rationale: profiles should be complete runnable configurations, while CLI remains a clean one-run override layer.

## OpenAI onboarding

**Decision:** skip all OpenAI prompts/credentials if OpenAI transport is not selected.

Rationale: local-only users should not need OpenAI configuration.

## HTTP/ngrok

**Decision:** ngrok is an HTTP enhancer, not a fourth MCP transport.

## Ephemeral URLs

**Decision:** HTTP and ngrok ephemeral policies are independent.

Global shorthand exists; per-transport override wins.

Rationale: stable local + ephemeral public and inverse both have real uses.

## Ephemeral path security

**Decision:** explicitly document it as obscurity/capability-like URL, not authentication.

## Full host authority

**Decision:** paired escape hatch:

```text
--allow-all --no-sandbox
```

Rationale: loud deliberate acknowledgement; avoid proliferating intermediate `*-dangerous` flags.

## Logging

**Decision:** activity logging is quiet by default; `-v` enables TOOL activity and `-vv` adds REQ diagnostics.

`-q/--quiet` hides TOOL activity.

`-v` enables TOOL logging; `-vv` adds developer REQ logging.

`-S/--setup` is setup.

`--color=auto|always|never` controls ANSI.

Rationale: tool activity is normal useful feedback; verbose should mean protocol/server diagnostics.

## Logging privacy

**Decision:** developer logs show metadata/methods, not raw payload bodies.

Rationale: debugging must not become accidental secret/file-content logging.

## Multi-transport policy

**Decision:** all transports share the same LocalMachine policy domain.

Rationale: transport should not implicitly change local authority.

## stdio stdout

**Decision:** stdout protocol-only.

All human/log output goes stderr.

## Build system

**Decision:** Crane dependency separation.

Rationale: source-only changes should reuse Cargo dependency artifacts and CI caches.

## Linux release target

**Decision:** musl/static portable x86_64 Linux artifact rather than distro-specific dynamically linked Debian artifact.

Rationale: satisfies Debian use while increasing portability.

## Installer repository owner

**Decision:** the canonical upstream is `https://github.com/abird-ai/dotlink`.

Installers default to `abird-ai/dotlink`, while `DOTLINK_REPO` and a custom release base remain supported for forks, mirrors and alternate hosting.
