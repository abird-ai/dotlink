# Runtime health check

Repeatable post-deploy validation. Run it against every new build, compare the shape
against the recorded baseline, and treat any new *class* of event as a regression until
proven otherwise.

The baseline is not a target: traffic varies. What must stay stable is the **shape** —
which tools dominate, where the long calls come from, and what the error and cap classes are.

## 0. Confirm the build first

Never analyse a log without knowing which binary produced it.

```bash
systemctl show -p ExecStart --value dotlink | grep -oE 'dotlink-0\.[0-9]+\.[0-9]+'
systemctl show -p MainPID -p ActiveState -p NRestarts -p ExecMainStartTimestamp --value dotlink
systemctl show dotlink -p Environment --value | tr ' ' '\n' | grep DOTLINK_
```

`NRestarts` and `ExecMainStartTimestamp` bound the log window: everything before the last
start belongs to a different process, and **job handles do not survive a process restart**.

## 1. Pull one window per host

Window start = that host's `ExecMainStartTimestamp`.

```bash
# local host
journalctl -u dotlink.service --since "<start>" --no-pager -o cat > /tmp/l5.log
# remote host
ssh -o BatchMode=yes pvl-x2 'journalctl -u dotlink.service --since "<start>" --no-pager -o cat' > /tmp/x2.log
```

Hosts in use: `pvl-l5` (primary; several concurrent sessions) and `pvl-x2` (sporadic).

## 2. Report

`python3 health.py <log> <label>` — stdlib only, no repo imports.

```python
#!/usr/bin/env python3
"""One host's dotlink runtime health report: python3 health.py <journal-log> <label>."""

import collections
import re
import statistics
import sys

POLL = re.compile(r'TOOL (\w+)\s+(→|←)')
DUR = re.compile(r'←\s+(ok|error)\s+(\d+)ms')


def stamp(line):
    m = re.match(r'^\[(\d\d):(\d\d):(\d\d)\.(\d\d\d)\]', line)
    if not m:
        return None
    return int(m.group(1)) * 3600 + int(m.group(2)) * 60 + int(m.group(3)) + int(m.group(4)) / 1000


def hh(x):
    return f"{int(x // 3600) % 24:02d}:{int(x % 3600 // 60):02d}:{int(x % 60):02d}"


def session(line):
    """Working directory for shell calls, top-level path for file tools."""
    if 'TOOL job_' in line:
        return 'job tools'
    m = re.search(r'\bd="([^"]*)"', line)
    if m:
        return m.group(1)
    for pattern in (r'p="([^"]*)"', r'paths=\["([^"]*)"'):
        m = re.search(pattern, line)
        if m:
            return "/".join(m.group(1).split('/')[:3]) if m.group(1).startswith('./src/') else 'file tools'
    return 'other'


def main():
    path, label = sys.argv[1], sys.argv[2]
    lines = open(path, encoding='utf-8', errors='replace').read().splitlines()

    # monotonic stamps: the journal format carries no date, so detect midnight
    st, prev, day = [], 0, 0
    for line in lines:
        s = stamp(line)
        if s is None:
            continue
        if s + day < prev - 3600:
            day += 86400
        prev = s + day
        st.append((prev, line))
    print(f"\n########## {label} ##########")
    if not st:
        print("no timestamped activity")
        return
    start, end = st[0][0], st[-1][0]
    print(f"window: {hh(start)} → {hh(end)}  ({(end - start) / 3600:.2f} h)")

    calls, open_by_tool = [], collections.defaultdict(list)
    for t, line in st:
        m = POLL.search(line)
        if not m:
            continue
        tool, direction = m.group(1), m.group(2)
        if direction == '→':
            open_by_tool[tool].append((t, line))
        elif open_by_tool[tool]:
            t0, argline = open_by_tool[tool].pop(0)
            d = DUR.search(line)
            if d:
                calls.append((t0, session(argline), tool, int(d.group(2)), d.group(1)))

    per = collections.defaultdict(list)
    for t0, sess, tool, dur, outcome in calls:
        per[sess].append((t0, tool, dur, outcome))
    print("\nper stream:")
    for sess, rows in sorted(per.items(), key=lambda kv: -kv[1][-1][0]):
        durs = sorted(r[2] for r in rows)
        last = rows[-1][0]
        print(f"  {sess:20} calls={len(rows):4} last={hh(last)} idle={int(end - last)}s "
              f"p50={statistics.median(durs):.0f}ms max={durs[-1]}ms")
        print(f"    {dict(collections.Counter(r[1] for r in rows))}")

    errors = [r for r in calls if r[4] == 'error']
    caps = [r for r in calls if r[3] >= 85000]
    print(f"\ncompletions={len(calls)} errors={len(errors)}")
    for t0, sess, tool, dur, _ in errors:
        print(f"  error {hh(t0)} {tool} {dur}ms {sess}")
    print(f"caps>=85s={len(caps)} by tool: {dict(collections.Counter(c[2] for c in caps))}")
    print("  " + ", ".join(f"{hh(c[0])} {c[2]} {c[3] // 1000}s" for c in caps))

    # waits: asked vs actual, and what followed
    waits, pend = [], []
    for i, (t, line) in enumerate(st):
        m = re.search(r'TOOL job_get\s+→.*wait_secs=(\d+)', line)
        if m:
            pend.append((i, t, int(m.group(1))))
            continue
        m = re.search(r'TOOL job_get\s+←\s+\w+\s+(\d+)ms', line)
        if m and pend:
            i0, t0, asked = pend.pop(0)
            nxt = next((st[j][1] for j in range(i + 1, min(i + 6, len(st))) if ' TOOL ' in st[j][1]), '')
            waits.append((asked, int(m.group(1)) // 1000, 'same job' if 'job_get' in nxt else nxt[14:40]))
    if waits:
        early = [w for w in waits if w[1] < w[0] - 2]
        full = [w for w in waits if w[1] >= w[0] - 2]
        print(f"\njob_get waits={len(waits)} early={len(early)} full-term={len(full)}")
        print(f"  silent seconds={sum(w[1] for w in full)}  "
              f"full-term followed by same job={sum(1 for w in full if w[2] == 'same job')}")

    # job lifecycle
    blank = {'start': None, 'get': 0, 'output': 0, 'ack': 0, 'cancel': 0}
    jobs = collections.defaultdict(lambda: dict(blank))
    for t, line in st:
        m = re.search(r'job_id="([0-9a-f]{8})', line)
        if not m:
            continue
        j = jobs[m.group(1)]
        if 'background=true' in line and '→' in line:
            j['start'] = j['start'] or t
        for tool in ('job_get', 'job_output', 'job_ack', 'job_cancel'):
            if f'TOOL {tool}' in line and '→' in line:
                j[tool.replace('job_', '')] += 1
    bg = sum(1 for _, l in st if 'background=true' in l)
    print(f"\nbackground=true={bg} jobs={len(jobs)} "
          f"unacked={sum(1 for v in jobs.values() if v['ack'] == 0 and v['cancel'] == 0)}")
    out_pages = [l for _, l in st if 'TOOL job_output' in l and '→' in l]
    print(f"job_output={len(out_pages)} tail_bytes={sum(1 for l in out_pages if 'tail_bytes=' in l)} "
          f"offset>0={sum(1 for l in out_pages if re.search(r'offset=[1-9]', l))}")
    tails = collections.Counter(int(m.group(1)) for _, l in st
                               if (m := re.search(r'tail_lines=(\d+)', l)) and ' TOOL ' in l)
    print(f"job_get tail_lines={dict(sorted(tails.items()))}")

    print(f"\nprogress notifications logged: {sum(1 for _, l in st if 'notifications/' in l)}")
    print(f"poll failures: {sum(1 for _, l in st if 'tunnel poll failed' in l)}  "
          f"restarts: {sum(1 for _, l in st if 'restarting dotlink' in l or 'Started dotlink' in l)}")
    seen = collections.defaultdict(int)
    for _, line in st:
        if ' REQ ' not in line:
            continue
        m = re.search(r'(→|←)\s+(\S+)', line)
        if m:
            seen[m.group(2)] += 1 if m.group(1) == '→' else -1
    open_now = {k: v for k, v in seen.items() if v > 0}
    print(f"open requests: {open_now or 'none'}")

    # duplicate commands (the retry-loop signature)
    cmds = collections.Counter(re.search(r'c="(.*?)"( |$)', l).group(1)
                               for _, l in st if 'TOOL bash' in l and '→' in l and re.search(r'c="(.*?)"( |$)', l))
    dups = {k: v for k, v in cmds.items() if v > 1}
    print(f"bash commands repeated: {len(dups)} of {len(cmds)} distinct")


if __name__ == '__main__':
    main()
```

Known limits of the script: it pairs a completion with the oldest unmatched start of the
same tool, so heavy interleaving can mis-attribute one call; the `open requests` line is a
delta count over `REQ` lines, not a per-request join. Both are good enough for shape, and
neither should be quoted as an exact figure.

## 3. Baseline — v0.17.1

Recorded 2026-11-15 02:03 → 07:12 (5.14 h) on `pvl-l5`, PID 3695575, `NRestarts 0`.
Three concurrent sessions: `./src/aw`, `./src/zerogravity`, `./src/dotlink`.
`pvl-x2` on the same build: 14 calls, no jobs, no errors, no caps.

| metric | value |
| --- | --- |
| completions | 907 |
| errors | 8 — all preflight/stale-handle, 0–10 ms |
| caps ≥85 s | 21 — `job_get` 18, `bash` 3 |
| `job_get` waits | 78 — 52 early, 26 full-term, 2 010 s silent, 18 re-waited the same job |
| `background=true` | 52 (56 handles, 50 acks, 3 cancels, 22 with no ack and no cancel) |
| `job_output` | 14 — 1 `tail_bytes`, 0 with `offset>0` |
| `job_get` `tail_lines` | 4×1, 8×2, 12×26, 14×5, **16×43** |
| progress notifications | 0 |
| poll failures / restarts | 0 / 0 |

Per-session shape on this build: `aw` is file-tool heavy (grep, bash, read, edit, bread) with
p50 ≈ 7 ms; `zerogravity` is shell-only and carries the multi-minute builds; `dotlink`
appears for short read/bash bouts on the repo itself.

## 4. Regression signatures

Check these explicitly. Each has a known-good value in the baseline.

| signature | healthy | suspect |
| --- | --- | --- |
| duplicate command within ~10 s of a cap | 0 (baseline: 18 repeated commands, all separated by real work) | any identical re-issue right after a handoff — this was the 0.14.3 disease |
| `job_output` with `offset>0` | 0 (probing goes through `job_get` tails) | pages walking forward from 0 — tail-first has regressed |
| `job_start` calls or `unknown tool` errors | 0 | wrong build deployed (pre-0.17 surface) |
| `job_get` `tail_lines` distribution | mostly 12/16 | everything pinned at 4, or nothing but the default — probe shape changed |
| acks vs handles | acks arrive in sweeps; unacked stays bounded | unacked climbing monotonically across hours |
| poll failures / restarts | 0 / 0 | any — transport instability |
| caps at exactly the hold | expected on long builds | caps *plus* duplicate work, or caps on sub-second tools |
| `notifications/` lines | 0 on the ChatGPT tunnel | non-zero means the client started sending a progress token; heartbeat behaviour changes |
| `end=` lines | an exit with no tree observation or cleanup alarm is silent | a second line for one job; a bare `end=exit code=N`; no line when the shell runner reports a timeout, cancel or internal error |
| `by=external` | a signal in the waited process's status with no `stopped_by` | `by=external` despite `stopped_by`, or inferred from an exit code alone |
| `cleanup=kill-failed` | 0 | any — the tree kill failed or could not be confirmed; check for surviving processes |
| error latency | 0–10 ms | tens of ms in `edit`/`patch` means the planner read files before failing |

`end=abandoned` means an unfinished guard was dropped while armed or with an unresolved kill failure.
During teardown that line may have no caller-visible result to match.

The `id=` of a line is the durable job handle: when the call returned one
(`background: true`, or a handoff receipt under a cap) it is the id `job_get` and
`job_output` use. A synchronous call returns no handle, so match its line by time —
the line's timestamp and the result's `ts` are the same call — and compare `end=`
with the status the caller actually saw.

`end=exit code=0 tree=leftovers` means the main command exited 0 but left live
work inside its cleanup boundary: the result has `success: false`, not a tool error.
`tree=unknown` means the pre-cleanup observation was unavailable; a racing
deadline or cancel can signal the group before it can be inspected. On Linux, check
that `/proc` describes the runner’s PID namespace and is not hiding PIDs (`hidepid`). Neither tree
value means a kill failed. Only `cleanup=kill-failed` means the kill remained
unconfirmed after its immediate retry. Silence does not establish host-wide containment: an unsandboxed
Unix child in another group (including Bash job control or GNU `timeout`) is outside both observation and cleanup. A matching session ID does not put it back in scope.

`by=` identifies an attributed cancel (`caller` or `connector`) or an observed signal
without a recorded dotlink stop (`external`). It cannot identify the sender.
A `signal_generate` tracepoint names the task executing the kernel signal path,
not necessarily a userspace `kill()` caller: `PR_SET_PDEATHSIG` runs in the dying
parent thread's context. Bubblewrap launch must therefore use dotlink's stable
spawn thread, never a retireable Tokio worker. In direct Unix and Bubblewrap runs an
external SIGKILL of the shell gives `signal: "SIGKILL"` and `end=signal SIGKILL by=external`.
A numeric `exit_code: 137` alone proves no signal. If the sandbox report is
missing or inconsistent, dotlink keeps the launcher status and reports `tree: "unknown"`.

## 5. Benign classes — do not chase

- **`edit`/`patch` rejected in 0–1 ms.** Stale anchors in a strict, all-or-nothing preflight.
  A 10 ms `edit` error means the batch read the file before failing; still nothing written.
- **`read`/`grep`/`bread` failing in 0 ms.** Path does not exist, or the argument is outside
  the accepted range. Resolution happens before IO.
- **`job_get` failing in 0 ms.** The handle is unknown: expired (1 h retention) or owned by a
  previous process. Unknown and expired are merged on purpose.
- **`job_get` at the 90 s ceiling.** That is the caller asking for the maximum wait on a job
  that never finished inside it. Not a transport cap.
- **`bash` at the 90 s ceiling with a handle returned.** The sync hold did its job.

## 5a. Reading silence

A session's idle time is a verdict on the *turn*, not on the bridge. The bands
below are an operator's rule of thumb from watching live sessions, not a
measurement; treat them as a prior to check against the log, not a finding:

| idle | reading |
| --- | --- |
| < 5 min | active — the model is between calls |
| 5–10 min | unclear — a turn that finished cleanly (commit, cleanup, handoff) sits quiet this long, and a new turn may still arrive |
| 10–20 min | usually over — the turn has most likely ended |
| 20 min + | presumed inactive until a new user turn arrives |

So a long idle column is not a fault signal by itself. Check the last call's *shape*
first: a session that went quiet right after a 90 s handoff, a commit, or a
cleanup command finished its work; one that went quiet mid-sequence with a job
still running needs the job checked separately.

Corollary: never infer "the model is stuck" from idleness. The bridge reports lost
requests (`open requests`), failed polls and restarts — those are the fault signals.

## 6. Open questions carried across versions

Each needs evidence, not a guess; record the answer here when it is settled.

1. **Progress heartbeats are inert on this tunnel.** The sink is gated on
   `_meta.progressToken`; the client never sends one, so no `notifications/progress` line has
   ever appeared. Whether ChatGPT's UI would react to a token-free notification
   (`notifications/message`) is untested.
2. **Silent waits are the dominant lost time.** 2 010 s inside full-term `job_get` waits in one
   window, with 18 of 26 re-waiting the same job. `job_get`'s wait wakes only on terminal
   state; `job_output`'s offset wait wakes on new bytes. Waking `job_get` on spool growth
   would shorten chatty builds but cannot help silent ones, and would add round trips.
3. **`tail_lines=16` is the cap and 43 of 78 probes ask for it.** Either the ceiling is too
   low or the model is over-asking. Any change moves token cost per probe.
4. **Unacked handles.** Acks arrive in sweeps of ~9, not per job; 22 of 56 handles were still
   unacked at the end of the baseline window. Bounded by retention and the session quota.

## 7. Rules of thumb

- Read the failure section of a gate run, not just the exit status.
- State the window and the build; two windows of different lengths are not comparable.
- Prefer a class over a count: "8 errors, all preflight" is useful, "8 errors" is not.
- If a number moves, look for the mechanism in the code before proposing a fix; two
  explanations usually fit a log until you read the handler.
