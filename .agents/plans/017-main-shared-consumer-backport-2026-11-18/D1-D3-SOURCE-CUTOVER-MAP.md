# D1-D3 source cutover map

Status: pre-repin implementation map; no source cutover yet.

This maps the exact dotlink source sites that move onto published AW P1-P3
contracts after the authority-gated AW publish/repin. It intentionally defines
no temporary compatibility semantics and no path/sibling dependency strategy.

## 1. Blocking condition

Dotlink still consumes AW 792f0b38d104659264b70c31736655df1a46b60f.
The required P1-P3 APIs exist only in the current unpublished AW working tree.

Do not begin source replacement until the signed AW implementation SHA is
published and dotlink is atomically repinned according to AW
PUBLISH-REPIN-PREFLIGHT.md.

## 2. D1 private process ownership to remove

Current src/shell_process.rs owns:

- Unix process_group(0) setup;
- Windows CREATE_SUSPENDED setup and post-Job resume;
- the process-group / Job Object Guard;
- Guard terminate/disarm/Drop.

After repin these move to AW:

- Unix pre-spawn:
  aw_platform::configure_child_group(command.as_std_mut());
- Windows pre-spawn:
  aw_platform::configure_managed_process(command.as_std_mut());
- containment: aw_platform::ManagedProcessGuard;
- signal labels: aw_platform::signal_name.

src/shell_process.rs should disappear or reduce to tiny product cfg glue for
the target-specific ManagedProcessGuard::attach signatures (Unix takes pid;
Windows takes pid + raw process handle). Do not retain a second Guard or stop
implementation.

ManagedProcessGuard::terminate already gives exact platform evidence:

    Ok(true)  -> StopEvidence::Stopped
    Ok(false) -> StopEvidence::NoTarget
    Err(_)    -> StopEvidence::Unknown

NoTarget is positive absence. Unknown remains unresolved and keeps final cleanup
responsibility. Never collapse them.

## 3. Sandbox descendant containment

The old tree_stop_proven(sandboxed, containment_terminated,
direct_child_stopped) mixes:

1. whether the OS containment operation reached/found/failed a target; and
2. whether an unsandboxed Unix descendant may have escaped that containment.

After cutover:

- AW StopEvidence records only the platform containment fact;
- StopRecord::terminal_reason decides whether a semantic stop is claimable;
- dotlink may retain sandboxed/product context for diagnostics;
- product uncertainty about escaped descendants must not rewrite NoTarget into
  Unknown or invent a second stop enum.

## 4. run_shell_job stop race replacement

Primary site:

    src/mcp/tools/shell.rs
    LocalMachine::run_shell_job

Current timeout/cancel flow:

    process_tree.terminate().is_ok()
    -> terminate_child().await
    -> tree_stop_proven(...)
    -> stop_proven bool
    -> JobCompletion timed_out/cancelled
    -> with_unproven_stop()

Replace with factual StopRecord values:

    Timeout               -> StopReason::Timeout
    Cancelled             -> StopReason::Cancelled
    runtime error stop    -> StopReason::RuntimeError
    cleanup / Drop        -> reason=None

Direct child wait/reap remains physical exit evidence, independent of stop
attribution.

Final shared terminal keeps two axes, while the adapter receipt retains the
final factual stop record separately:

    ProcessTerminal {
        exit: observed ProcessExit,
        stop: final_stop_record.and_then(StopRecord::terminal_reason),
        output: ProcessOutputMeta,
    }

    ProcessReceipt::stop_record() -> Option<StopRecord>

Valid examples:

    exit=Unknown, stop=Some(Timeout)
    exit=Signal(9), stop=None

A requested stop never manufactures Signal(n).

## 5. Physical ExitStatus mapping — resolved shared fact

Dotlink currently records only ExitStatus::code(), losing Unix signal numbers.

AW adapter already contains the correct factual projection privately:

    code()               -> ProcessExit::Code(code)
    Unix status.signal() -> ProcessExit::Signal(signal)
    otherwise            -> ProcessExit::Unknown

That helper is private inside aw-tools-adapter PipeProcess::status_facts.

Before publishing AW, prefer one small public platform fact that does not make
aw-platform depend upward on aw-tools, conceptually:

    aw_platform::exit_signal(&std::process::ExitStatus) -> Option<i32>

Then the host/adapter factual projection is simply:

    status.code()                  -> ProcessExit::Code(code)
    aw_platform::exit_signal(...)  -> ProcessExit::Signal(signal)
    otherwise                      -> ProcessExit::Unknown

Requirements:

- no allocation;
- no product wording;
- Unix extracts ExitStatusExt::signal();
- Windows returns None;
- signal names remain presentation via signal_name(number);
- aw-tools stays platform-independent and aw-platform gains no dependency on
  aw-tools merely to return ProcessExit.

The existing AW adapter status_facts should consume the same helper, so dotlink
and the AW adapter cannot drift on Unix signal extraction.

This gap is now resolved in the AW working tree: `aw-platform` exposes
`exit_signal(&ExitStatus)`, and AW's own process adapter consumes it. After
repin, dotlink must use the same helper rather than retaining a local Unix cfg
branch.

## 6. JobCompletion migration

Current semantic-risk fields in src/jobs.rs:

    exit_code
    stop_proven
    output_complete

plus product state/status/error bookkeeping.

stop_proven must disappear.

exit_code alone must stop being authoritative physical process state.

Old constructors/mutators that carry stop semantics must stop doing so:

    JobCompletion::timed_out
    JobCompletion::cancelled
    with_unproven_stop

Retain product-owned fields as needed:

- JobStatus;
- product error/result prose;
- stdout/stderr retention summaries;
- JobResultContext;
- internal_error;
- observed_status for fail-closed diagnostics if still useful;
- spawn_failed;
- output bookkeeping.

Store one authoritative shared ProcessTerminal once the process settles, rather
than reconstructing it later from product status fields.

## 7. Delete shared_process_terminal reconstruction

Current function:

    src/jobs.rs::shared_process_terminal

The old published-AW reconstruction currently maps:

    Completed + no code     -> Signal
    TimedOut + stop_proven  -> DeadlineExceeded
    Cancelled + stop_proven -> Cancelled

Those variants are obsolete.

After repin this function should disappear or become trivial access to the
already-retained ProcessTerminal.

Never derive physical exit from JobStatus.
Never derive semantic stop from JobStatus alone.

settle_shared_completion should submit the retained terminal directly to
SharedJobManager::settle_process.

## 8. Fail-closed settlement preserves facts

Current fail_closed_shared_completion sets product status Unknown and clears
stop_proven.

After migration, shared/journal uncertainty may make product Job status Unknown,
but it must not erase already observed terminal facts.

Example:

    process reaped by SIGKILL
    timeout stop was proven
    later shared checkpoint or spool step fails

Allowed result:

    product status = Unknown
    terminal.exit  = Signal(9)
    terminal.stop  = Some(Timeout)

The same rule applies to later internal/spool failures.

## 9. D2 final stop record and attempt observer

Shared inputs:

    ProcessReceipt::stop_record() -> Option<StopRecord>
    ExecutionLifecycleEvent::ProcessStop
    ExecutionEventSink
    StopTracker

The receipt record is the final factual result for one logical stop. Lifecycle
events are attempt-level evidence and may include graceful, forced and
housekeeping attempts. Dotlink must not render one operator line per event.

Dotlink observer may retain higher-layer context separately:

- Job id;
- explicit vs cleanup/Drop path when useful;
- cancellation provenance needed for cancel vs shutdown.

Do not add those fields to StopRecord.

Only Stopped plus Some(reason) may become stopped_by:

    Timeout      -> timeout
    RuntimeError -> failed
    Cancelled    -> cancel or shutdown from product context

No stopped_by for:

- external signal;
- NoTarget;
- Unknown;
- housekeeping reason=None.

Operator line remains dotlink-owned and is rendered exactly once from the final
receipt record plus product cancel-vs-shutdown context:

    LOG job stop id=<id> reason=<reason> outcome=<delivered|gone|failed>

Presentation mapping:

    Stopped  -> delivered
    NoTarget -> gone
    Unknown  -> failed

Use the existing always-visible LogConfig::lifecycle path.
External process death emits no connector-stop line. v0.17.5 additionally
requires one product-only outside-exit line when and only when:

    terminal.exit = ProcessExit::Signal(n)
    final_stop_record.is_none_or(|record| record.reason.is_none())

Then render:

    LOG job exit id=<id> signal=<name-or-number> from=outside

`from=outside` means only "dotlink did not start a semantic stop sequence"; it
must not identify a culprit. A reasoned stop record suppresses this line even
for NoTarget/Unknown evidence, because direct-child fallback may still have
delivered the physical signal. This conservative predicate avoids falsely
claiming an outside killer. Use `aw_platform::signal_name(n)` for the token;
unknown numbers remain bare decimal values with no whitespace.

## 10. D3 Task projection

Current problem site:

    src/jobs.rs::JobManager::detailed_task

Current internal_error handling returns plain failed-task prose and can lose
earlier terminal facts.

After migration, failed Task data must retain:

    TaskTerminalDetail {
        exit,
        stop,
    }

Full ProcessTerminal/output metadata remains in Job/process state.

Later failures must preserve examples such as:

    exit=Signal(9), stop=Some(Timeout)
    exit=Code(1), stop=None
    exit=Unknown, stop=Some(Cancelled)

Do not infer stopped_by from Task status.

## 11. One product compatibility mapper

After shared terminal facts are retained, create one small dotlink mapper used
by every outward process-terminal compatibility surface.

Input:

    ProcessExit
    Option<StopReason>
    dotlink cancellation provenance/context

Product output may include:

- numeric signal;
- exact stopped_by;
- existing timeout/cancel/failure prose.

Reuse the same mapper for:

- synchronous shell results;
- jobs_get / retained Job detail where exposed;
- native Task failure data.

It owns presentation only, never process truth.

## 12. Tests to rewrite/remove

Old tests tied to stop_proven or synthetic terminal variants must be rewritten.

Known categories:

- tree_stop_proven truth-table tests;
- unproven_cancel_or_timeout_maps_to_shared_unknown;
- old SharedProcessExit DeadlineExceeded/Cancelled expectations;
- direct completion.stop_proven assertions.

Replace with tests of independent factual axes:

- Stopped/NoTarget/Unknown remain distinct;
- only Stopped yields terminal stop attribution;
- physical signal number survives;
- external signal has no stopped_by;
- later internal failure preserves prior exit and stop;
- incomplete output can make status Unknown without deleting terminal facts.

## 13. Required D1-D3 regressions

D1:

1. external Unix SIGTERM/SIGKILL -> ProcessExit::Signal(n), no stopped_by, no
   connector stop line, exactly one outside-exit line when no reasoned final
   stop record exists;
2. proved timeout -> stop=Some(Timeout), physical exit independently retained;
3. cancellation -> stop=Some(Cancelled) only for Stopped evidence, with
   product cancel/shutdown selection;
4. NoTarget -> no terminal stop attribution, product log outcome gone;
5. Unknown -> no terminal stop attribution, product log outcome failed and
   final cleanup responsibility retained.

D2:

1. exactly one fixed-order stop line for a reasoned connector stop;
2. stop line survives all verbosity modes and suppresses outside-exit;
3. external kill/crash with no reasoned final stop record produces exactly one
   `LOG job exit ... from=outside` at all verbosity modes and no stop line;
4. known signal token uses platform name, unknown signal uses bare number;
5. housekeeping/Drop has no semantic reason;
6. BrokenPipe/unwind remains non-panicking.

D3:

1. reaped signal survives later spool/internal failure;
2. proved timeout survives later spool/internal failure;
3. Task failed error data contains compact terminal facts;
4. sync/jobs_get/Task share one signal/stopped_by mapper;
5. compact handoff remains only a Job handle/no-rerun receipt.

## 14. Post-repin edit order

1. Add aw-platform as part of atomic dependency repin.
2. Consume the now-shared `aw_platform::exit_signal(&ExitStatus)` physical fact.
3. Replace shell_process Guard/pre-spawn setup with AW platform surfaces.
4. Introduce retained ProcessTerminal into JobCompletion/Job state.
5. Convert timeout/cancel/runtime-stop paths to StopRecord.
6. Convert direct child wait to ProcessExit.
7. Delete tree_stop_proven, stop_proven and with_unproven_stop.
8. Delete old shared_process_terminal reconstruction.
9. Pass retained terminal directly into shared Job settlement.
10. Preserve retained terminal through fail-closed settlement.
11. Use `ProcessReceipt::stop_record()` for the one product stop-line decision;
    combine it with `ProcessTerminal.exit` for the mutually exclusive v0.17.5
    outside-exit decision; retain ProcessStop events only for diagnostics/metrics.
12. Add the one product compatibility mapper for signal/stopped_by/stop-vs-exit prose.
13. Project TaskTerminalDetail into failed Task data.
14. Rewrite obsolete tests.
15. Run focused D1-D3 suite, then full dotlink qualification before D4.

## 15. Do-not-cross invariants

The cutover is wrong if any of these become true:

- Job status becomes the source of physical process exit;
- requested timeout/cancel manufactures a signal number;
- NoTarget and Unknown collapse;
- external signal produces stopped_by;
- a reasoned connector stop sequence is mislabeled `from=outside`;
- an unattributed physical signal loses its single outside-exit record;
- later infrastructure failure erases an observed signal/stop;
- cancellation provenance is added back into AW canonical stop enums;
- dotlink retains a second process Guard;
- operator log wording leaks into AW core;
- Task failure uses a different terminal mapper than sync/jobs_get.
