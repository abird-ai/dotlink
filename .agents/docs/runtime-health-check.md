# Runtime health check

Repeatable post-deploy validation for current dotlink `main`.

Use it after every deployed build. Compare **shape**, not raw counts: which
tools dominate, where waits occur, whether handoffs lead back to the same Job,
whether output is explicitly read/ACKed, and whether transport failures or
duplicate execution appear.

This document describes current `jobs_*` main. The v0.17.x release line is
behavioral history only.

## 0. Confirm the binary and process window

Never analyze a mixed-process log.

```bash
systemctl show -p ExecStart --value dotlink
systemctl show -p MainPID -p ActiveState -p NRestarts -p ExecMainStartTimestamp --value dotlink
systemctl show dotlink -p Environment --value | tr ' ' '\n' | grep '^DOTLINK_'
```

Start the journal window at that process's `ExecMainStartTimestamp`.

A runtime restart ends that in-memory Job session. Outer hosted replay may keep
its own fail-closed tombstones, but do not attribute a retained Job from one
process to another.

## 1. Pull one window per host

```bash
journalctl -u dotlink.service --since "<start>" --no-pager -o cat > /tmp/dotlink.log
```

For a remote host, copy the same unit window over SSH. Always record host,
binary identity, PID/start timestamp, and `NRestarts` with the report.

## 2. Current finite-Job facts

Current standalone model-facing tools are:

```text
batch
jobs_list
jobs_get
jobs_read
jobs_ack
jobs_send
jobs_cancel
```

Old `job_start/job_get/job_output/job_ack/job_cancel/job_restart` names are not
the current surface.

Current dotlink Job-profile limits:

```text
jobs_get tail default       512 aggregate bytes
jobs_get tail max          1024 aggregate bytes
jobs_get wait_ms max      30000 ms
jobs_read wait_ms max     30000 ms
jobs_read raw page min          1 byte
jobs_read default page   262144 raw bytes, narrowed by the active profile
jobs_list limit max          256
```

`jobs_get` waits only for the selected Job/root/node to settle or for its
observation deadline. `rev` is a status token, not a wake predicate.

`jobs_read` is the ordinary forward pageable retained-output surface.
**Positive `offset` values are healthy and expected.** Do not carry forward
the old release-health rule that treated `job_output offset>0` as suspicious.

Reading never ACKs. Cleanup advances only through explicit `jobs_ack`:

- `offset=<n>` = acknowledge a proven contiguous delivered prefix;
- omitted offset = explicit Finish for the exact terminal Job/node owner.

Compact handoff/control receipts retain the same Job and explicitly say
`no-rerun`.

## 3. Report script

Stdlib only:

```python
#!/usr/bin/env python3
"""dotlink journal shape report: health.py <journal-log> <label>."""

import collections
import re
import statistics
import sys

POLL = re.compile(r"TOOL\s+(\w+)\s+(→|←)")
DUR = re.compile(r"←\s+(ok|error)\s+(\d+)ms")
JOB_REF = re.compile(r'\bref="?j:([^/"\s]+)(?:/n:(\d+))?')
WAIT_MS = re.compile(r"\bwait_ms=(\d+)")
OFFSET = re.compile(r"\boffset=(-?\d+)")
TAIL = re.compile(r"\btail=(\d+)")


def stamp(line):
    m = re.match(r"^\[(\d\d):(\d\d):(\d\d)\.(\d\d\d)\]", line)
    if not m:
        return None
    return (
        int(m.group(1)) * 3600
        + int(m.group(2)) * 60
        + int(m.group(3))
        + int(m.group(4)) / 1000
    )


def hh(value):
    return (
        f"{int(value // 3600) % 24:02d}:"
        f"{int(value % 3600 // 60):02d}:"
        f"{int(value % 60):02d}"
    )


def session(line):
    if re.search(r"TOOL\s+jobs_", line):
        return "job tools"
    m = re.search(r'\bd="([^"]*)"', line)
    if m:
        return m.group(1)
    if re.search(r'\bp="', line) or "paths=[" in line:
        return "file tools"
    return "other"


def main():
    path, label = sys.argv[1], sys.argv[2]
    lines = open(path, encoding="utf-8", errors="replace").read().splitlines()

    # Journal -o cat carries time but not the date. Detect midnight rollover.
    stamped, previous, day = [], 0.0, 0
    for line in lines:
        value = stamp(line)
        if value is None:
            continue
        if value + day < previous - 3600:
            day += 86400
        previous = value + day
        stamped.append((previous, line))

    print(f"\n########## {label} ##########")
    if not stamped:
        print("no timestamped TOOL/REQ activity")
        return

    start, end = stamped[0][0], stamped[-1][0]
    print(f"window: {hh(start)} -> {hh(end)} ({(end - start) / 3600:.2f} h)")

    calls = []
    open_by_tool = collections.defaultdict(list)
    for when, line in stamped:
        match = POLL.search(line)
        if not match:
            continue
        tool, direction = match.groups()
        if direction == "→":
            open_by_tool[tool].append((when, line))
        elif open_by_tool[tool]:
            began, args = open_by_tool[tool].pop(0)
            done = DUR.search(line)
            if done:
                calls.append(
                    (began, session(args), tool, int(done.group(2)), done.group(1), args)
                )

    per = collections.defaultdict(list)
    for row in calls:
        per[row[1]].append(row)

    print("\nper stream:")
    for name, rows in sorted(per.items(), key=lambda item: -item[1][-1][0]):
        durations = sorted(row[3] for row in rows)
        last = rows[-1][0]
        tools = collections.Counter(row[2] for row in rows)
        print(
            f"  {name:20} calls={len(rows):4} last={hh(last)} "
            f"idle={int(end-last)}s p50={statistics.median(durations):.0f}ms "
            f"max={durations[-1]}ms"
        )
        print(f"    {dict(tools)}")

    errors = [row for row in calls if row[4] == "error"]
    print(f"\ncompletions={len(calls)} errors={len(errors)}")
    for began, stream, tool, duration, _, _ in errors:
        print(f"  error {hh(began)} {tool} {duration}ms {stream}")

    # Observation waits. Current limits are milliseconds and 30 s maximum.
    waits = []
    for row in calls:
        if row[2] not in {"jobs_get", "jobs_read"}:
            continue
        asked = WAIT_MS.search(row[5])
        if asked and int(asked.group(1)) > 0:
            waits.append((row[2], int(asked.group(1)), row[3]))
    if waits:
        print("\njob waits:")
        for tool in ("jobs_get", "jobs_read"):
            selected = [row for row in waits if row[0] == tool]
            if not selected:
                continue
            full = sum(1 for _, asked, actual in selected if actual >= max(0, asked - 250))
            early = len(selected) - full
            print(f"  {tool}: calls={len(selected)} early={early} near-deadline={full}")

    # Current Job-tool shape.
    job_calls = collections.Counter(
        row[2] for row in calls if row[2].startswith("jobs_")
    )
    print(f"\njob tools: {dict(sorted(job_calls.items()))}")

    read_starts = [
        line
        for _, line in stamped
        if re.search(r"TOOL\s+jobs_read\s+→", line)
    ]
    positive = 0
    negative = 0
    for line in read_starts:
        match = OFFSET.search(line)
        if not match:
            continue
        value = int(match.group(1))
        positive += value > 0
        negative += value < 0
    print(
        f"jobs_read starts={len(read_starts)} "
        f"offset>0={positive} offset<0={negative}"
    )

    get_starts = [
        line
        for _, line in stamped
        if re.search(r"TOOL\s+jobs_get\s+→", line)
    ]
    tails = collections.Counter(
        int(match.group(1))
        for line in get_starts
        if (match := TAIL.search(line))
    )
    print(f"jobs_get explicit tail={dict(sorted(tails.items()))}")

    refs = collections.Counter()
    for _, line in stamped:
        if " TOOL " not in line:
            continue
        match = JOB_REF.search(line)
        if match:
            refs[match.group(1)] += 1
    print(f"referenced Jobs={len(refs)}")

    progress = [
        line for _, line in stamped
        if re.search(r"TOOL\s+\S+\s+progress \d+/\d+ ticks", line)
    ]
    notifications = sum(
        1 for _, line in stamped
        if "REQ " in line and "notifications/progress" in line
    )
    print(
        f"progress summaries={len(progress)} "
        f"delivered-notification logs={notifications}"
    )

    print(
        "poll failures="
        f"{sum(1 for _, line in stamped if 'tunnel poll failed' in line)} "
        "runtime restarts="
        f"{sum(1 for _, line in stamped if 'restarting dotlink runtime' in line)}"
    )

    # REQ starts minus completions: shape only, not a request-id join.
    seen = collections.defaultdict(int)
    for _, line in stamped:
        if " REQ " not in line:
            continue
        match = re.search(r"(→|←)\s+(\S+)", line)
        if match:
            seen[match.group(2)] += 1 if match.group(1) == "→" else -1
    print(f"open request deltas: {dict((k, v) for k, v in seen.items() if v > 0) or 'none'}")

    # Exact repeated visible shell excerpts are a useful retry-loop signal.
    commands = collections.Counter()
    for _, line in stamped:
        if not re.search(r"TOOL\s+(bash|powershell|pwsh)\s+→", line):
            continue
        match = re.search(r'\bc="(.*?)"(?: |$)', line)
        if match:
            commands[match.group(1)] += 1
    repeated = {command: count for command, count in commands.items() if count > 1}
    print(f"shell excerpts repeated={len(repeated)} of {len(commands)} distinct")


if __name__ == "__main__":
    main()
```

The tool-start/completion pairing is deliberately approximate under heavy
same-tool concurrency. Use it for shape, not forensic attribution.

## 4. Current regression signatures

| signature | healthy | investigate |
| --- | --- | --- |
| old `job_*` model calls | zero | wrong/stale client or binary |
| duplicate shell execution immediately after a `no-rerun` handoff | zero | exactly-once/handoff regression |
| `jobs_read offset>0` | normal | only suspicious if bytes repeat/skip or ACK outruns delivered prefix |
| `jobs_ack` | explicit and idempotent | implicit cleanup after read, or ACK beyond proven delivery |
| `jobs_get wait_ms` | ≤ 30000 | larger value accepted, or unrelated revision wakes it |
| `jobs_read wait_ms` | ≤ 30000 | larger value accepted, or observation itself ACKs |
| handoff | same `j:<id>`, `no-rerun` | new execution/identity for receipt fitting |
| progress | only with request progress token; pending sends never delay result readiness | token-free traffic, fake total/message, or progress delivery delaying cleanup/result |
| tunnel poll failures / runtime restarts | normally zero | any unexplained occurrence |
| fast preflight error | usually low milliseconds | substantial IO/effect before a supposedly pre-effect rejection |

Do not use the old release heuristic “forward output paging is suspicious”.
`jobs_read` is intentionally pageable.

## 5. Receipt timestamp semantics

Current main already timestamps selected Toolset receipts.

The authoritative stored value is:

```text
_meta["io.abird/receipt"]["ts"]
```

in UTC Unix seconds.

For model-visible selected results, the same timestamp is also exposed as:

```text
ts=<unix-seconds>
```

unless the typed receipt already carries that exact line.

A replay reuses the stored timestamp; it does **not** mint a new observation
time. Therefore receipt time identifies the original result observation, not
the time of every redelivery.

When comparing logs with a receipt, remember that TOOL log timestamps are local
wall-clock display while receipt `ts` is UTC Unix seconds.

## 6. Stop evidence on current main

D1-D3 is complete on current main. The runtime now consumes these shared AW facts:

```text
ProcessExit::{Code(i32), Signal(i32), Unknown}
StopReason::{Timeout, Cancelled, RuntimeError}
StopEvidence::{Stopped, NoTarget, Unknown}
StopRecord { reason: Option<StopReason>, evidence }
StopTracker                  # Unknown -> retry reason preservation
ProcessReceipt::stop_record  # final factual logical stop record
```

Only `evidence=Stopped + reason=Some(...)` may produce `stopped_by`.

Dotlink owns the compatibility wording:

```text
Timeout      -> stopped_by="timeout"
Cancelled    -> stopped_by="cancel" or "shutdown" from product context
RuntimeError -> stopped_by="failed"
```

No `stopped_by` for:

- external signal;
- `NoTarget`;
- `Unknown`;
- housekeeping (`reason=None`).

A physical Unix signal is retained as `signal: <n>` independently of stop
semantics.

After the v0.17.5-compatible cutover, operator lifecycle records are mutually
exclusive at the logical-job level:

```text
reasoned final stop record
  -> LOG job stop id=<id> reason=<reason> outcome=<delivered|gone|failed>

ProcessExit::Signal(n) + no reasoned final stop record
  -> LOG job exit id=<id> signal=<name-or-number> from=outside
```

`from=outside` means only that dotlink did not enter a semantic stop sequence;
it never identifies the sender. A crash and another process's SIGKILL both land
there. A reasoned final stop record suppresses `from=outside` even when its
evidence is NoTarget/Unknown, because direct-child fallback may have delivered
the physical signal. Attempt-level `ExecutionLifecycleEvent::ProcessStop`
records are diagnostics/metrics and are **not** one operator line each.

These lifecycle lines are current-main health invariants. Real-binary coverage
proves their mutual exclusion and visibility at quiet/default/verbose.

## 7. Reading idle time

Idleness is not a transport diagnosis.

A useful operator prior:

| idle | interpretation |
| --- | --- |
| < 5 min | commonly active/between calls |
| 5–10 min | ambiguous |
| 10–20 min | turn often complete |
| 20 min+ | presume inactive until a new turn appears |

Always inspect the final call shape first. A completed handoff, commit, cleanup,
or explicit Job observation may naturally be followed by silence.

Do not infer “model stuck” from idle time. Use transport evidence: open-request
deltas, poll failures, runtime restarts, or a Job that is still objectively
running.

## 8. Thirty-minute implementation-turn guidance

The project workflow asks implementation agents to work for roughly **30 minutes
per user turn**, stop at the next coherent checkpoint, record durable state, and
pause.

That is **human/agent workflow guidance only**. In contrast,
`timeout_secs` on a shell tool is a hard execution deadline: reaching it stops
the command tree. `background:true` only changes how long the caller waits; it
does not remove that deadline.

The 30-minute workflow rule is not:

- a shell execution timeout;
- a transport response window;
- a `jobs_get/jobs_read` wait limit;
- a Job retention TTL;
- a cancellation deadline.

Do not diagnose a runtime fault merely because a model turn approaches or
crosses that guidance window.

## 9. Rules of thumb

- Record build, process start, host, and journal window.
- Prefer classes over counts: “all errors are preflight” is more useful than a
  bare error total.
- Check whether a result was handed off before treating a long shell call as a
  transport timeout.
- A compact receipt saying `no-rerun` is recovery guidance, not permission to
  start another execution.
- Read output with `jobs_read`; ACK only through explicit `jobs_ack`.
- Never infer stop cause from status text when canonical stop evidence is
  unavailable.
