# Changelog

All notable changes to dotlink are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project follows [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- `grep` searches UTF-8 text files with a regex or literal pattern, `glob` filters, optional `ignore_case`, and three output modes: `content` (matching lines with context and `--` group separators), `files`, and `count`. Paging uses `offset`/`limit` (200 hits by default), and `gitignore`/`skip_hidden` are opt-in. It walks with the same descriptor-rooted enumerator as `ls`, so search stays inside filesystem read authority and never reaches shell-only grants.
- `edit` accepts an ordered `edits[]` array of `{path?, old_text, new_text, replace_all?}` across up to 16 files with an inherited top-level `path`. Entries apply sequentially per file, each `old_text` must be unique unless `replace_all` is set, and every file is re-hashed before writing: any concurrent change or failed entry writes nothing. A failed `old_text` that starts with `digits + tab` reports that `read`'s line-number prefix must be stripped.
- `patch` applies the Codex `*** Begin Patch` grammar: mandatory header and terminator, `*** Update File` hunks located by content (exact, then `trim_end`-tolerant, ambiguity failing), `*** Add File` as create-only, and explicit rejection of `*** Delete File` / `*** Move to`. Planning, snapshot verification and writing are atomic; `best_effort` writes the files that planned cleanly and reports the rest.
- `bedit` edits many byte ranges at once. Offsets are relative to the original file, ranges apply ascending with delta tracking, overlaps and out-of-bounds ranges are rejected, and an optional `sha256` precondition gates the write.
- `ls` takes `pattern` (one `globset` grammar everywhere: `*`, `?`, `[...]`, `{a,b}`, `**`), `depth`, `kind` (`any`/`file`/`dir`), `sort` (`path`/`mtime`), and `limit`, returning `{entries, truncated}`. `depth` defaults to 4 when the pattern contains `/` or `**`, else 1.
- `read` prefixes each line with its 1-based number and a tab by default (`numbered: false` opts out) and defaults to a 4096-line window that ends with one `… truncated at line N of M` marker.
- Shell calls emit `notifications/progress` heartbeats while running when the client supplies a progress token: a monotonic counter with no total and no message, so long commands stay visibly alive on streamed transports.

### Fixed

- `edit` reports a signed `line_delta`; a file that shrinks no longer returns an underflowed count.
- `read` clamps `limit` to at least one line, so `limit: 0` cannot produce a `truncated at line 0` marker.
- `bedit` bounds the total decoded payload size, not just each range, so a multi-range call cannot pin more than one patchable file in memory.
- `grep`/`ls` skip directories they cannot enumerate instead of failing the whole search, and `grep` refuses to read a `.gitignore` that policy denies.
- `patch` with `best_effort` now skips and reports a file whose snapshot changed and continues past per-file write failures instead of aborting the call.
- `*** Add File` uses an exclusive create, so a target that appears between planning and writing is refused rather than truncated.
- `patch` and `edit` no longer rewrite a file whose planned text is byte-identical.
- Shell timeouts return a real MCP tool error (`isError`) carrying `timed_out: true`, the failure reason, and any partial stdout/stderr salvaged within a 250 ms grace, instead of a message-only error that discarded the output collected before the deadline.
- Undeliverable tunnel responses are logged at warn instead of debug when the client's `response_timeout` window has already expired.

### Changed

- Binary tools are `bread`, `bwrite`, and `bedit` (`read_binary`, `write_binary`, `patch_binary` are gone, with no alias routes), and `edit` is array-only. Clients must refresh their tool list after upgrading.
- Tool descriptions describe only tool behavior: access boilerplate was dropped because unavailable tools are already hidden, and the `bash`/`powershell` arguments advertise the configured ceilings as concrete numbers instead of naming config keys.
- `max_shell_timeout_secs` and `max_output_bytes` are now pure policy ceilings, defaulting to 3600 s and 1 MiB per stream. The `shell` timeout is caller-controlled via `timeout_secs`, clamped to the ceiling, and defaults to the ceiling when omitted instead of a hidden 30 s.
- The OpenAI tunnel threads the client-supplied `response_timeout` into shell calls: an omitted `timeout_secs` defaults to that window minus a 5 s delivery reserve, and a larger explicit value is shortened, so a long command returns a real tool result instead of being dropped at the client window boundary.
- The OpenAI tunnel keeps its timing and concurrency parameters in one validated `TunnelTuning` value: the HTTP request timeout is derived from the poll hold plus a 10 s margin, and `poll_limit` is held between `max_in_flight` and `3 * max_in_flight`. Protocol and defaults for the poll hold are unchanged.
- Tunnel concurrency is 16 command tasks with 32 commands per poll (was 8 and 25), so a full batch leaves at most 16 commands queueing for a permit.
- `regex`, `globset`, and `ignore` are direct dependencies; `ignore` is used only for `GitignoreBuilder`/`Gitignore`, never `ignore::Walk`.

## [0.9.1] - 2026-10-16

### Fixed

- Host-mount deny/protected policy is now validated before resolving `bash` or Bubblewrap, so sandbox policy violations fail closed even when the sandbox tooling is absent from `PATH` (restores `nix flake check` in minimal build environments).

### Changed

- Attribute the MIT license to Prasanna Loganathar and dotlink contributors, and document the project's human-led, AI-assisted development posture.

## [0.9.0] - 2026-10-14

### Added

- Linux Bubblewrap hardware capabilities: `--allow-kvm` and repeatable `--allow-gpu=amd|nvidia`, including additive profile settings, guided hardware discovery, and NixOS `hostAccess.kvm` / `hostAccess.gpu` options with service-scoped `kvm`/`render`/`video` group access.
- Vendor-verified AMD/NVIDIA DRM discovery, pinned character-device `--dev-bind` mounts, and read-only GPU driver/runtime metadata mounts; hardware remains shell-only and disabled unless explicitly granted.

## [0.8.1] - 2026-10-13

### Fixed

- Explicit `--allow-cache-<kind>` flags and NixOS `cacheAccess` entries whose cache directory does not exist are now skipped with a warning instead of aborting startup, so declarative services no longer fail before a cache has been created on the host.

## [0.8.0] - 2026-10-13

### Added

- Markdown lint and Alejandra checks in `nix flake check`, both tools in the dev shell, and CI validation before release builds.
- Optional `-s/--serialize` runtime flag and NixOS service setting to run MCP calls one at a time; Linux runs tools concurrently by default.
- Explicit `--allow-nix-daemon`, `--allow-nix-config`, `--allow-ssh-agent`, and `--allow-git-config` Linux shell grants, each discoverable during onboarding; their mounts remain pinned, shell-only, and off by default.
- `--allow-cache-<kind>[=ro|rw]` for all 13 existing developer-cache families, using the same detection as setup and explicit read-only/read-write choices. NixOS gained matching `hostAccess`, `sshAgentSocket`, and `cacheAccess` options.
- GCR SSH-agent autodetection for interactive and NixOS system services: valid `SSH_AUTH_SOCK`, `$XDG_RUNTIME_DIR/gcr/ssh`, and `/run/user/<effective-uid>/gcr/ssh`. An optional `DOTLINK_SSH_AGENT_SOCKET` (the NixOS `sshAgentSocket` setting) takes strict precedence; no explicit socket configuration is necessary when GCR is running.

### Changed

- Moved the agent guide to `AGENTS.md` and updated its references.
- `nix fmt` now uses Alejandra for Nix formatting.
- Split Nix build, release, check, and development definitions into `nix/`; package versions now come from `Cargo.toml`.

### Fixed

- Linux directory listings now apply deny/protected-path checks and format relative paths against the original authorized directory instead of internal `/proc/self/fd/` names, restoring recursive traversal and avoiding filtered-entry disclosure.
- File reads, text edits, binary MCP reads, and binary patches now bound the actual bytes read even when another process grows the file concurrently. Listings tolerate entries removed during enumeration.
- Read+write tools now pin the narrower of overlapping read and write grants, preventing replacement paths from bypassing the narrower authority; matching grants share the same opened directory identity.

### Security

- Linux native file tools now open authorized paths through pinned directory handles and kernel-enforced `openat2` resolution, removing reliance on the global tool gate for symlink/path-race protection. Legacy conservative dispatch remains on non-Linux platforms.
- Linux Bubblewrap grant/cache mounts now use pinned directory FDs inherited only by the sandbox child, preventing mount-source symlink swaps across concurrent requests.
- SSH-agent opt-in uses a pinned socket and minimal read-only Unix account lookup files, allowing real Git SSH commit signing without sharing any private SSH keys.
- Nix daemon access now remains masked without explicit opt-in even when shell network access or broad filesystem grants are enabled; pinned host mounts are authorized against the exact resolved path used to open the FD.

## [0.7.0] - 2026-10-11

### Added

- A native `nixosModules.default` / `nixosModules.dotlink` flake module with `services.dotlink.enable`, independent named instances, optional declarative settings, user-profile fallback, configurable service user/working directory/package, systemd supervision and journald logging.
- Optional systemd `LoadCredential` integration for OpenAI Runtime and ngrok tokens. Explicit file-based credentials override saved keys without requiring a new profile or embedding secrets in the Nix store.
- SIGTERM-aware graceful shutdown for unattended systemd services and NixOS module evaluation checks.

### Changed

- Standardized the repository agent guide filename and updated all handoff references.

### Security

- Explicit credential-file overrides are included among MCP-protected control-plane paths, and credential-file environment paths are stripped from spawned shells.

## [0.6.0] - 2026-10-01

### Added

- Embedded single-owner OAuth 2.1 authorization/resource server for Streamable HTTP, including Protected Resource Metadata, Authorization Server Metadata, authorization code + mandatory PKCE S256, exact resource binding, RFC 9207 `iss`, CIMD, DCR fallback, revocation, rotating refresh tokens, and short-lived opaque access tokens.
- Stable `--ngrok-domain`, explicit reverse-proxy `--public-url`, local `--oauth/--no-oauth`, and the one-run `--allow-public-no-auth` escape hatch.
- `dotlink oauth status/clients/revoke/revoke-all` for inspecting and revoking persisted remote OAuth clients and refresh grants.
- OAuth owner credentials stored as Argon2id hashes, approved DCR clients and hashed refresh grants stored under the private Abird XDG state namespace, while authorization codes/access tokens remain memory-only.
- Cross-process locking around OAuth state read/modify/write so setup, management commands, and a live server cannot lose one another's changes.
- Task-oriented, colorized CLI help with sections for setup/profiles, transports, remote HTTP/OAuth, filesystem access, shell/network, and diagnostics; profile/OAuth subcommand help now documents argument meanings and examples.
- Automatic forward migration for known-compatible profile schemas. Schema v9 now migrates to v10 in memory because the v10 fields are additive/defaulted; migrated profiles are written as v10 on their next edit/save.

### Changed

- Public ngrok ingress is OAuth-protected by default, independently of local HTTP OAuth.
- Direct non-loopback HTTP is also safe-by-default: it requires OAuth plus an explicit HTTPS public origin, unless `--allow-public-no-auth` is supplied for that run.
- Refresh tokens are issued only when the approved client advertises the `refresh_token` grant; `offline_access` requires that capability.
- Installers now resolve the latest GitHub Release by default and act as idempotent updaters when re-run.
- The version command and GitHub Release title now use the concise `dotlink v0.6.0` form; installer and CI version checks accept the new CLI format while still recognizing older installed binaries.
- README positioning now emphasizes invoking `@dotlink` directly from ChatGPT chats/Spaces/dots and connecting several machines without installing the ChatGPT desktop app or provisioning SSH.
- Profile and OAuth CLI documentation has been updated to match the live command surface, including the persisted `oauth` setting.
- Setup now describes launch-directory access accurately: the default `.` permission follows whichever directory dotlink is started from, while the current directory is shown only as context.
- Compatible config migrations are explicit and versioned rather than globally rejecting every older schema; newer schemas and older schemas without a defined migration path are still rejected rather than guessed.

### Fixed

- Filesystem deny checks now apply to both lexical and canonical paths so a denied namespace cannot be reintroduced through a later symlink alias.
- Filesystem mutators and shell execution are serialized against reads through the LocalMachine operation gate, closing dotlink-originated path-topology TOCTOU races.
- Existing write/edit targets must be regular files; special files such as sockets/FIFOs/devices are rejected.
- Runtime-key writes are atomic, credentials stay in dotlink's owned XDG directory even with `DOTLINK_CONFIG`, and active config/credential paths are protected from MCP/sandbox self-modification.
- Shell timeout uses one absolute deadline across stdin, process execution, and output collection, and timed-out/failed children are explicitly reaped.
- Runtime flags mixed with profile subcommands are rejected rather than silently ignored.
- Mixed stdio/OpenAI human status remains off stdout, and manual stdio Ctrl+C handling is symmetric across Unix/Windows.
- Installers validate the downloaded binary's reported version before replacement and use same-directory replacement paths for updates.

### Security

- Argon2 verification runs in bounded blocking workers rather than Tokio workers, with bounded concurrency and failed-attempt delay but no remotely triggerable global owner lockout.
- CIMD validation uses HTTPS-only metadata URLs, redirect refusal, public-IP/DNS checks, pinned vetted addresses, bounded bodies, and exact client-ID matching.
- Unauthenticated DCR registrations remain bounded and memory-only until owner approval.
- Public OAuth state is bounded, durable refresh-token material is stored only as hashes, and owner password plaintext is never persisted.
- Public ingress never derives OAuth issuer/resource identity from Host/Forwarded headers.
- Public no-auth remains an explicit one-run-only escape hatch and is not persisted.

## [0.5.0] - 2026-09-30

### Added

- Modular OpenAI Tunnel, stdio, Streamable HTTP, and optional ngrok MCP transports around one shared LocalMachine policy.
- Pi-style MCP tool surface: `ls`, `read`, `write`, `edit`, platform shell, plus separate binary read/write/patch tools.
- Named JSONC profiles with persisted transport, filesystem allow/deny, shell/network, and developer-cache configuration.
- Linux Bubblewrap shell sandbox with network disabled by default, NixOS runtime/profile support, Nix-daemon masking, and explicit developer-cache mounts.
- Interactive setup, profile management commands, live `Ctrl+R` restart and `v` verbosity cycling, quiet-by-default TOOL/REQ logging, and policy-aware `--list-tools`.
- Automatic full-runtime OpenAI Tunnel recovery after repeated transient poll failures.
- Crane-based Nix builds, static Linux x86_64/ARM64 releases, Windows x86_64/ARM64 cross builds, macOS ARM64 cross build, release aggregation, and curl/PowerShell installers.
- GitHub tag-to-release automation with individual binaries and SHA-256 sidecars.

### Changed

- Rebranded the project to **abird dotlink** with `dotlink` as the crate, executable, MCP identity, config namespace, and release prefix.
- Moved dotlink-owned XDG state under `abird/dotlink`.
- Made profile-enabled transports start automatically; runtime `--stdio/--http` add transports and `--no-*` flags suppress persisted defaults.
- Made the launch directory the relative-path base and default read-only grant; all additional authority is expressed through additive allow/deny rules.
- Made `--allow-rw=/` the natural unrestricted filesystem grant while keeping Linux shell sandboxing independent.
- Consolidated unsandboxed full-host authority into the paired `--allow-all --no-sandbox` flags.
- Made developer caches shell-only capabilities that never expand MCP filesystem access.
- Made stdio stdout protocol-only and routed human/log output to stderr.

### Fixed

- Restored exact terminal state on clean exit, restart, transport errors, and unwind paths.
- Fixed NixOS shell executable/PATH resolution and runtime mount behavior without exposing the whole home directory.
- Hardened Nix-daemon masking against varying filesystem shapes.
- Hardened Runtime API key storage and protected control-plane paths.
- Made setup transactional and re-setup preserve existing secrets by default.

## [0.2.0] - 2026-09-29

### Changed

- Scoped the filesystem workspace to the process launch directory by default.
- Added ChatGPT tunnel-registration instructions.

## [0.1.0] - 2026-09-29

### Added

- Initial native Rust Secure MCP Tunnel client.
- Embedded rmcp server with local filesystem and Bash tools.

[Unreleased]: https://github.com/abird-ai/dotlink/compare/v0.9.1...HEAD
[0.9.1]: https://github.com/abird-ai/dotlink/compare/v0.9.0...v0.9.1
[0.9.0]: https://github.com/abird-ai/dotlink/compare/v0.8.1...v0.9.0
[0.8.1]: https://github.com/abird-ai/dotlink/compare/v0.8.0...v0.8.1
[0.8.0]: https://github.com/abird-ai/dotlink/compare/v0.7.0...v0.8.0
[0.7.0]: https://github.com/abird-ai/dotlink/compare/v0.6.0...v0.7.0
[0.6.0]: https://github.com/abird-ai/dotlink/compare/v0.5.0...v0.6.0
[0.5.0]: https://github.com/abird-ai/dotlink/compare/v0.2.0...v0.5.0
[0.2.0]: https://github.com/abird-ai/dotlink/releases/tag/v0.2.0
[0.1.0]: https://github.com/abird-ai/dotlink/commit/d10e69e
