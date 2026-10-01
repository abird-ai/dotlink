# Changelog

## Unreleased

- Bump dotlink to 0.6.0 and strict profile schema v10 for embedded HTTP OAuth and stable ngrok-domain configuration; normal startup rejects incompatible schemas, while `--setup` can replace an older profile from scratch after explicit confirmation.
- Add a single-owner embedded OAuth 2.1 authorization/resource server for Streamable HTTP: Protected Resource Metadata, Authorization Server Metadata, authorization code + mandatory PKCE S256, exact resource/issuer binding, RFC 9207 `iss`, CIMD with SSRF-resistant fetching, DCR fallback, revocation, rotating refresh tokens, and short-lived opaque access tokens.
- Protect public ngrok ingress with OAuth by default, independently of local HTTP OAuth; add stable `--ngrok-domain`, explicit reverse-proxy `--public-url`, local `--oauth/--no-oauth`, and the intentionally unsafe one-run `--allow-public-no-auth` escape hatch.
- Persist only an Argon2id owner-password hash, approved DCR clients, and hashed refresh grants under the private Abird XDG state directory; keep access tokens/authorization codes in memory and hold unauthenticated DCR registrations in bounded memory until owner approval.
- Add `dotlink oauth status/clients/revoke/revoke-all`, transactional hidden owner-credential setup, bounded OAuth state, consent-page client/redirect/resource review, and end-to-end OAuth/MCP regression coverage.

- Add a single-host Nix release graph: x86_64 Linux now cross-builds static Linux x86_64/ARM64, Windows x86_64/ARM64, and macOS ARM64, aggregated by `nix build .#release-all`.
- Keep the release surface OS-level only: generic static Linux x86_64/ARM64 artifacts, Windows x86_64/ARM64, and macOS ARM64; Windows ARM64 uses pinned LLVM-MinGW/UCRT and macOS ARM64 uses pinned Apple SDK 14.4 + clang/ld64.lld.
- Add a minimal GitHub Actions workflow that installs Nix and builds `release-all`; normal runs keep a CI artifact, while `v*` tags create/update a GitHub Release with each binary and checksum sidecar uploaded as an individual asset.
- Extend installers to select Linux ARM64, Windows ARM64, and Apple Silicon macOS artifacts automatically; default to the latest published release, install under the simple `dotlink` command name, and make rerunning the installer an idempotent hash-verified update path.

- Offer an Admin-key-free OpenAI Tunnel setup path: create/manage the tunnel in OpenAI Platform and paste its existing `tunnel_...` ID; keep the one-time Admin-key flow as the optional automated path.
- Fresh correctness/security review: make Runtime-key writes atomic, keep credentials in dotlink's owned XDG directory even with `DOTLINK_CONFIG`, and protect active config/credentials from MCP and shell self-modification.
- Make tool discovery use the canonical runtime policy, including write-only and deny-shadowed capability handling; existing write symlinks now resolve to their canonical target before protected-path checks.
- Make shell timeout one absolute deadline across stdin, process execution, and output collection; explicitly reap failed/timed-out child processes.
- Reject runtime flags mixed with `dotlink profile` commands instead of silently ignoring them, keep mixed stdio/OpenAI runtime status off stdout, and make stdio Ctrl+C handling symmetric on Unix/Windows.

- Rename `-s/--silent` to `-q/--quiet`; default logging remains quiet, and `-q -vv` provides REQ-only diagnostics.
- Keep stdio human/status/diagnostic output on stderr while stdout remains exclusively MCP protocol traffic; stdio is quiet by default unless verbosity is explicitly enabled.
- Make manual TTY stdio robust to inherited broken terminal modes by temporarily enabling `Ctrl+C` interrupt signaling and restoring the exact original terminal state on exit.

- Fix runtime-control terminal output by preserving the terminal's original output flags while raw input is active, preventing stair-stepped/garbled banners and logs.
- Snapshot the exact pre-dotlink terminal state and restore it via RAII on normal exit, `Ctrl+R` restart, transport errors, and unwind/error paths.

- Add live terminal controls for interactive runs: `v` cycles quiet/TOOL/TOOL+REQ verbosity, `Ctrl+R` fully restarts the runtime, and `Ctrl+C` exits.
- Make runtime verbosity shared/mutable so all transports react immediately; HTTP REQ logging can now turn on after startup.
- Keep stdio protocol-safe by disabling runtime key capture whenever stdio transport is active.

- Make setup transactional: cancel/none exits successfully without writing a profile, so first-run setup restarts cleanly on the next launch.
- Make re-setup profile-aware: existing transport/access/cache/tunnel values become prompt defaults, stored Runtime API keys display only as `[existing key]`, and blank secret input preserves them.
- Add `dotlink profile` management for list/show/create/edit/delete, allow/deny rule add/remove, and persisted boolean enable/disable operations; dependent toggles auto-enable prerequisites and disable safely in cascades.
- Preserve OpenAI tunnel/key data when OpenAI is disabled and remove the saved key only when the profile is deleted.

- Rebrand the project as **abird dotlink** for human-facing branding and `dotlink` for the crate, executable, MCP identity, config/env namespace, Nix outputs, installers, and release assets.
- Move dotlink-owned XDG paths under the shared Abird namespace: `~/.config/abird/dotlink` / `$XDG_CONFIG_HOME/abird/dotlink`, with future private XDG cache/state/data paths under `abird/dotlink` as well.
- Set the canonical upstream to `https://github.com/abird-ai/dotlink` and update installer defaults/release URLs accordingly.
- Refocus the README around ChatGPT dots, Spaces, local project continuity, local data access, and controlled direct access to the user's computer.

- Bump config schema to v9: the launch directory is the internal relative-path base and is read-allowed by default unless `default_allow=false` or `--no-default-allow` is used.
- Make persisted stdio and HTTP transports start automatically, matching persisted OpenAI behavior; `--stdio` / `--http` add one-run transports and `--no-stdio` / `--no-http` suppress profile transports.
- Persist HTTP ephemeral-path, ngrok, and ngrok-ephemeral behavior in profiles; add `--no-ngrok` as a one-run override.
- Redesign setup with concise numbered transport selection, conditional HTTP/ngrok questions, clearer local-access prompts, and ANSI color when enabled.
- Refuse normal startup when no filesystem capability and no shell capability are available.

- Add automatic OpenAI tunnel recovery: after 10 consecutive transient poll failures, tear down the active runtime and reconstruct profile, policy, MCP state, and transports automatically.
- Bound peer-transport shutdown to 5 seconds before force-aborting remaining tasks so recovery cannot hang indefinitely.
- Add bounded runtime restart backoff (1s up to 30s) and reset it after a successfully connected runtime later becomes unhealthy.
- Keep normal poll retry warnings concise while retaining full transport error details under verbose DEBUG logging.
- Harden installers with explicit HOME handling, checksum-format validation/manual hash comparison, and atomic Unix binary replacement.
- Make the release helper enable `nix-command` and `flakes` explicitly instead of depending on global Nix configuration.

- Refocus the README's front-page story on giving normal ChatGPT web workflows permission-scoped access to local files and tools through OpenAI Secure MCP Tunnel, with MCP server + policy engine + Linux Bubblewrap sandbox in one binary.
- Add a representative startup/tool-activity transcript and document opt-in TOOL activity logging without exposing file contents, raw payload bodies, or secrets.

- Add developer-cache autodiscovery during onboarding for Cargo, npm, pnpm, Yarn, pip, uv, Go, Maven, Gradle, sccache, and ccache.
- Persist typed cache grants with none/read-only/read+write choices and mount approved caches only into the sandboxed shell's private home.
- Keep shared caches outside the MCP filesystem permission surface; filesystem deny rules still remove or downgrade matching cache grants.
- Add tool-specific sandbox cache environment mapping and Bubblewrap RO/RW cache regression tests.

- Refactor Nix builds to Crane with a separate buildDepsOnly dependency artifact layer reused by package, tests, and Clippy.
- Add stable release-artifact outputs with SHA-256 sidecars and a scripts/build-release-artifacts.sh helper.
- Add curl-able install.sh and PowerShell install.ps1 installers using stable release asset names.
- Refresh `flake.lock` for Crane v0.24.0 and rust-overlay.
- Harden Bubblewrap Nix-daemon isolation by masking the daemon endpoint roots (/nix/var/nix/daemon-socket and /run/nix-daemon) instead of assuming a specific socket leaf/type; centralize type-aware path masking for directories and non-directories.

- Use JSONC profiles: config.jsonc for default and config.<profile>.jsonc for named profiles, with comments and trailing commas.
- Add -p/--profile and allow -S/--setup -p <name> to create or reconfigure named profiles through the same onboarding flow.
- Persist allow_read/allow_write/allow_rw and deny_read/deny_write/deny_rw path arrays, with CLI permissions merged on top.
- Add symmetric deny-read, deny-write, deny-rw, deny-shell, and deny-network controls; denies override profile defaults and runtime grants.
- Keep legacy --deny=PATH as a read+write deny synonym.
- Make --allow-rw=/ naturally mean unrestricted filesystem RW; remove special all-rw authority flags.
- Replace the intermediate dangerous-suffixed unsandboxed flags with a single paired --allow-all --no-sandbox full-host mode; deny rules are rejected in that mode.
- Store OpenAI runtime keys separately per profile as runtime.key / runtime.<profile>.key.

- Make explicit --stdio and --http runtime flags activate those transports even when persisted setup has them disabled.
- Make bare --allow-rw shorthand for read+write on the launch directory, matching bare --allow-write.
- Canonicalize shell executables and sandbox PATH entries so Bubblewrap shell execution works correctly on NixOS profile symlinks.
- Mount the standard Nix store/profile graph read-only inside Bubblewrap while keeping the user's home and Nix daemon socket out of the default sandbox.

- Add --ephemeral-url for fresh high-entropy HTTP/ngrok MCP paths.
- Add independent --http-ephemeral-url and --ngrok-ephemeral-url overrides, including explicit =false.
- Use a separate loopback-only ngrok backend so local and public MCP routes can differ without cross-exposure.
- Add modular persisted transport support for OpenAI Secure MCP Tunnel, stdio MCP, and Streamable HTTP MCP.
- Add --stdio for subprocess MCP clients such as Claude Desktop and Claude Code.
- Add --http and --http-bind for local Streamable HTTP MCP at /mcp.
- Add --ngrok using the ngrok Rust SDK to publish the HTTP MCP endpoint directly as a public HTTPS /mcp URL.
- Skip all OpenAI credential/tunnel setup when OpenAI transport is disabled.
- Move transport implementations into src/transports/openai.rs, stdio.rs, and http.rs around one shared LocalMachine policy.
- Let clean termination of one transport leave concurrently active transports running.
- Reserve stdout exclusively for MCP protocol traffic when stdio is active.
- Replace the old fs_* API with a Pi-like tool surface: read, write, edit, ls, and platform shell.
- Add read_binary, write_binary, and patch_binary with MCP/base64/hex support.
- Make the default tool surface read-only.
- Add additive --allow-read, --allow-write, --allow-rw, and precedence --deny directory policies.
- Make bare --allow-write shorthand for read+write on the launch directory.
- Add Linux Bubblewrap shell sandboxing with network disabled by default.
- Add --allow-network for sandboxed network access.
- Add explicit full-host authority through paired --allow-all --no-sandbox; ordinary --allow-rw=/ remains available for unrestricted filesystem RW inside the Linux sandbox.
- Add Bash on Unix and PowerShell selection on Windows.
- Simplify first-run setup into a compact beginner-friendly checklist.
- Hide verbose tunnel INFO logs by default while keeping them available through RUST_LOG.
- Add a complete Nix flake package, app, development shell, formatter, Bubblewrap runtime, and nix flake check checks.
- Add timestamped TOOL activity logging across all transports.
- Add -q/--quiet to suppress TOOL activity; setup remains available as -S.
- Make activity logging quiet by default: -v shows TOOL activity and -vv adds developer REQ diagnostics.
- Add --color=auto|always|never with TTY-aware automatic ANSI color output.
- Add --list-tools generated from the live policy-aware MCP router.
- Add direct setup links for ChatGPT Workspace IDs and OpenAI Organization IDs.

## 0.2.0

- The filesystem workspace defaults to the process launch directory.
- Added ChatGPT tunnel-registration instructions.

## 0.1.0

- Initial native Rust Secure MCP Tunnel client.
- Embedded rmcp server with local filesystem and Bash tools.
