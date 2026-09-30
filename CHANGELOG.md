# Changelog

## Unreleased

- Add automatic OpenAI tunnel recovery: after 10 consecutive transient poll failures, tear down the active runtime and reconstruct profile, policy, MCP state, and transports automatically.
- Bound peer-transport shutdown to 5 seconds before force-aborting remaining tasks so recovery cannot hang indefinitely.
- Add bounded runtime restart backoff (1s up to 30s) and reset it after a successfully connected runtime later becomes unhealthy.
- Keep normal poll retry warnings concise while retaining full transport error details under verbose DEBUG logging.
- Set `https://github.com/abird-ai/abird-link` as the canonical upstream in package metadata, Nix metadata, installers, and documentation; installers still support repository/base-URL overrides.
- Harden installers with explicit HOME handling, checksum-format validation/manual hash comparison, and atomic Unix binary replacement.
- Make the release helper enable `nix-command` and `flakes` explicitly instead of depending on global Nix configuration.

- Refocus the README's front-page story on giving normal ChatGPT web workflows permission-scoped access to local files and tools through OpenAI Secure MCP Tunnel, with MCP server + policy engine + Linux Bubblewrap sandbox in one binary.
- Add a representative startup/tool-activity transcript and document that normal TOOL attempts/completions are logged locally by default without intentionally logging file contents or raw payload bodies.

- Add developer-cache autodiscovery during onboarding for Cargo, npm, pnpm, Yarn, pip, uv, Go, Maven, Gradle, sccache, and ccache.
- Persist typed cache grants with none/read-only/read+write choices and mount approved caches only into the sandboxed shell's private home.
- Keep shared caches outside the MCP filesystem permission surface; filesystem deny rules still remove or downgrade matching cache grants.
- Add tool-specific sandbox cache environment mapping and Bubblewrap RO/RW cache regression tests.

- Refactor Nix builds to Crane with a separate buildDepsOnly dependency artifact layer reused by package, tests, and Clippy.
- Add portable x86_64 Linux/musl and x86_64 Windows GNU cross-build outputs, with separate dependency-cache outputs for CI.
- Add stable release-artifact outputs with SHA-256 sidecars and a scripts/build-release-artifacts.sh helper.
- Add curl-able install.sh and PowerShell install.ps1 installers using stable release asset names.
- Refresh `flake.lock` for Crane v0.24.0 and rust-overlay.
- Keep flake-native macOS support on aarch64-darwin; stop advertising x86_64-darwin now that nixpkgs 26.11 has dropped it, with Intel macOS documented as a Cargo-from-source path.
- Harden Bubblewrap Nix-daemon isolation by masking the daemon endpoint roots (/nix/var/nix/daemon-socket and /run/nix-daemon) instead of assuming a specific socket leaf/type; centralize type-aware path masking for directories and non-directories.

- Rename the package, binary, MCP server identity, Nix outputs, config directory, and environment prefix to abird-link.
- Replace TOML persistence with JSONC: config.jsonc for default and config.<profile>.jsonc for named profiles, with comments/trailing commas and legacy .json read fallback.
- Add -p/--profile and allow -S/--setup -p <name> to create or reconfigure named profiles through the same onboarding flow.
- Expand profile permissions to persist cwd plus allow_read/allow_write/allow_rw and deny_read/deny_write/deny_rw path arrays, with CLI permissions merged on top.
- Keep backward compatibility with v7 boolean allow_rw by mapping true to allow_rw: ["."].
- Add setup-time optional cwd pinning plus persisted shell and sandboxed-network defaults.
- Add symmetric deny-read, deny-write, deny-rw, deny-shell, and deny-network controls; denies override profile defaults and runtime grants.
- Keep legacy --deny=PATH as a read+write deny synonym.
- Make --allow-rw=/ naturally mean unrestricted filesystem RW; remove special all-rw authority flags.
- Replace the intermediate dangerous-suffixed unsandboxed flags with a single paired --allow-all --no-sandbox full-host mode; deny rules are rejected in that mode.
- Store OpenAI runtime keys separately per profile as runtime.key / runtime.<profile>.key.

- Make explicit --stdio and --http runtime flags activate those transports even when persisted setup has them disabled.
- Make bare --allow-rw shorthand for read+write on cwd, matching bare --allow-write.
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
- Make bare --allow-write shorthand for read+write cwd.
- Add Linux Bubblewrap shell sandboxing with network disabled by default.
- Add --allow-network for sandboxed network access.
- Add explicit full-host authority through paired --allow-all --no-sandbox; ordinary --allow-rw=/ remains available for unrestricted filesystem RW inside the Linux sandbox.
- Add Bash on Unix and PowerShell selection on Windows.
- Simplify first-run setup into a compact beginner-friendly checklist.
- Hide verbose tunnel INFO logs by default while keeping them available through RUST_LOG.
- Add a complete Nix flake package, app, development shell, formatter, Bubblewrap runtime, and nix flake check checks.
- Make timestamped tool-attempt/completion logging the normal user-facing activity stream across all transports.
- Add -s/--silent to suppress normal tool activity, repurposing -s from setup; setup now uses -S.
- Redefine -v/--verbose as developer request logging for OpenAI, stdio, and HTTP while retaining normal tool activity.
- Add --color=auto|always|never with TTY-aware automatic ANSI color output.
- Add --list-tools generated from the live policy-aware MCP router.
- Add direct setup links for ChatGPT Workspace IDs and OpenAI Organization IDs.

## 0.2.0

- Added --cwd=<DIR>.
- The filesystem workspace defaults to the process launch directory.
- Added ChatGPT tunnel-registration and Plugin Creator instructions.

## 0.1.0

- Initial native Rust Secure MCP Tunnel client.
- Embedded rmcp server with local filesystem and Bash tools.
