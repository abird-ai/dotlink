# Context and project history

> Historical note: this file intentionally records earlier designs, including an earlier configurable project-base model. Schema v9 supersedes that model: the launch directory is now the internal base, `default_allow` controls its implicit read grant, and older schemas are unsupported.

## Original problem

The project started as a clean Rust binary that would let ChatGPT connect to a user's local machine through OpenAI Secure MCP Tunnel, exposing local filesystem access and optionally Bash.

Initial UX goal:

```text
run one binary
→ local MCP server starts
→ secure tunnel connects
→ print Tunnel ID
→ paste/select it in ChatGPT
→ done
```

The first name was `abird-tunnel`.

## Why it became `dotlink`

The product evolved beyond a single OpenAI tunnel:

- OpenAI Secure MCP Tunnel;
- stdio MCP;
- local Streamable HTTP;
- public HTTPS through ngrok;
- ChatGPT;
- Claude.ai remote connector;
- Claude Desktop/Code/local MCP clients.

The generic link/bridge concept was broader than “tunnel,” so product/package/binary/config/docs were renamed to `dotlink`.

## Major design evolution

### Phase 1 — native Rust tunnel

- native Rust process;
- OpenAI tunnel setup;
- local filesystem and Bash;
- one-command onboarding.

### Phase 2 — workspace boundary

- a removed project-base flag;
- filesystem tool path checks;
- prevent escaping selected workspace;
- beginner-friendly setup.

### Phase 3 — observability

- concise tunnel/tool logs;
- `--list-tools`;
- Nix flake;
- validation.

### Phase 4 — Pi-inspired tools

Old `fs_*` surface was replaced by:

```text
read
write
edit
ls
bash/powershell
```

Binary tools added separately:

```text
read_binary
write_binary
patch_binary
```

### Phase 5 — least privilege

- default read-only;
- additive read/write/rw grants;
- deny precedence;
- shell explicit;
- network explicit.

### Phase 6 — Linux Bubblewrap

- sandbox shell;
- private home/tmp;
- NixOS runtime support;
- network namespace;
- Nix daemon isolation;
- PATH canonicalization;
- profile symlink handling.

### Phase 7 — modular transports

- OpenAI;
- stdio;
- HTTP;
- concurrent lifecycle;
- ngrok;
- independent ephemeral local/public paths.

### Phase 8 — profiles and config

- rename to `dotlink`;
- `-p/--profile`;
- JSONC config;
- per-profile runtime key;
- earlier persistent base/path rules;
- persistent shell/network defaults;
- symmetric deny model.

### Phase 9 — developer caches

Motivation came from real validation: Cargo executable was available through Nix profile/store, but host `~/.cargo` was hidden by the private sandbox home and network was intentionally disabled.

Initially validation used a Nix vendor closure.

Product decision:

- discover existing developer caches;
- ask user explicitly;
- cache sharing is shell-only;
- mount cache into private sandbox home;
- RO/RW choice;
- never implicitly expose adjacent credentials/config.

### Phase 10 — logging redesign

Original `-v` was user-facing concise tool logging.

Decision changed:

- TOOL activity is available on demand with `-v`;
- `-s` should suppress it;
- `-v` should mean developer request/protocol logging;
- setup short flag moves to `-S`;
- colors should auto-follow interactive stderr.

### Phase 11 — build/release

- Crane dependency separation;
- Linux musl cross artifact;
- Windows GNU cross artifact;
- release dist outputs;
- curl/sh installer;
- PowerShell installer;
- CI-ready output structure.

## Important empirical issues that shaped design

### NixOS Bash path

Bubblewrap originally tried to execute:

```text
/etc/profiles/per-user/.../bin/bash
```

but that profile path was not visible in sandbox.

Fix direction:

- canonicalize executable;
- mount Nix runtime/profile graph RO;
- sanitize/canonicalize PATH.

### Nix daemon endpoint

A prior implementation assumed:

```text
/nix/var/nix/daemon-socket/socket
```

was a Unix socket.

On one NixOS state it was reported as a directory, causing Bubblewrap target creation failure.

Final design direction:

- mask endpoint roots, not presumed leaf:

```text
/nix/var/nix/daemon-socket
/run/nix-daemon
```

- generic type-aware `mask_path` abstraction.

### Cargo cache missing

With:

```text
HOME=/tmp/home
network off
```

`cargo test` could find Cargo but not host cached crates.

This validated the need for explicit cache sharing or a vendored/Nix dependency source.

### Cache discovery can mutate

`yarn cache dir` in a synthetic onboarding test created a cache directory.

Decision:

- discovery must avoid probes that initialize/create cache state;
- prefer env vars, known existing paths and safe read-only queries.

## Historical commit boundary

Current last committed baseline:

```text
5b8bbdd Rename to dotlink and add profiles and deny policy
```

Everything after that in the working tree reflects the later phases above.
