# Security model

`abird-tunnel` is a remote-administration bridge by design. If Bash is enabled, a model action approved through the connected ChatGPT surface can execute commands with the permissions of the OS user running the process.

That is substantially more authority than a normal read-only MCP connector.

## Trust boundaries

### No inbound listener

`abird-tunnel` opens outbound HTTPS requests to the OpenAI tunnel control plane. It does not bind a local/public MCP port.

### Filesystem tools are rooted

The `fs_*` tools operate below one workspace root selected for each process. By default this is the directory from which `abird-tunnel` is launched; `--cwd=<dir>` overrides it. They:

- reject absolute paths;
- reject `..` traversal;
- canonicalize existing targets;
- validate existing ancestors before creates;
- do not recursively follow directory symlinks;
- handle final symlinks safely for stat/remove operations;
- enforce bounded read/write sizes; and
- deny the saved tunnel runtime credential path.

### Bash is intentionally not rooted

`shell_exec` executes:

```text
bash --noprofile --norc -lc <command>
```

with the OS identity of `abird-tunnel`. Its working directory must start inside the selected workspace, but the command can use absolute paths, spawn other programs, access the network, and modify anything the account can modify.

This means the selected `--cwd` workspace is **not a security boundary for Bash**.

Known OpenAI/tunnel credential environment variables are removed from shell child processes. This reduces accidental disclosure, but it does not create a security boundary against arbitrary code running as the same OS user.

If you need a real privilege boundary, run `abird-tunnel` under a dedicated OS user, VM, container, or sandbox whose filesystem/network permissions are exactly what you intend to expose.

## Credentials

There are two different credentials in first-run setup:

1. **Runtime key** — should have only Tunnels Read + Use. This is needed every time the tunnel runs.
2. **Admin key** — needs tunnel management permission and is used only if `abird-tunnel` creates the tunnel for you.

The Admin key is never written to disk.

The restricted runtime key is stored separately at:

```text
~/.config/abird-tunnel/runtime.key
```

with mode `0600` on Unix; its parent directory is mode `0700`. The non-secret config is stored in `config.toml` next to it.

The normal filesystem MCP tools explicitly reject the runtime credential path. A fully privileged Bash process running as the same user can still access same-user files. Therefore the runtime credential must remain narrowly scoped and must never be an Admin key.

## Tunnel request filtering

The control plane can supply MCP request headers. `abird-tunnel` forwards only the headers needed by MCP protocol handling:

- `Mcp-Session-Id`
- `Mcp-Protocol-Version`
- `Mcp-Method`
- `Mcp-Name`
- `Mcp-Param-*`
- `Last-Event-ID`

It does not forward OpenAI internal routing/authentication headers to the local MCP handler.

On the response path it returns only protocol-relevant headers documented by the tunnel contract.

## Resource limits

The tunnel queue is bounded to eight concurrently executing commands. Shell stdout/stderr are drained without unbounded accumulation. File reads/writes and command output are bounded. Shell commands have a configurable maximum timeout and are killed on timeout/drop.

These are guardrails, not a sandbox. A shell command can intentionally consume CPU, memory, disk, processes, or network resources available to the OS account.

## Symlinks

Filesystem read/write/create operations verify canonical targets/ancestors remain under the selected workspace. Removal and stat of a final symlink operate on the link itself rather than following it to its target. Recursive listing does not traverse symlinked directories.

## Recommended deployment

For a personal workstation, use a dedicated restricted runtime key and keep ChatGPT tool approvals enabled for destructive/open-world actions.

For stronger separation, run `abird-tunnel` under a dedicated user or container with only the directories and commands you want exposed. Do not run it as root.
