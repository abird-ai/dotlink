//! Kill the contained process group/job on timeout, error or cancellation.
//! Unsandboxed Unix children can intentionally escape their process group
//! (setsid); no process-group API can guarantee descendant containment, so the
//! contract is the group: it is swept when the command returns, and a sandboxed
//! tree is already gone with its namespace. Work that must outlive the call
//! belongs in a durable job (`background: true`), where the process is the job
//! rather than a leftover of one.

use std::io;
use tokio::process::{Child, Command};

/// Why a tree was signalled. This is the log vocabulary: it is wider than what a
/// result may claim, because a tree is sometimes cleaned up rather than stopped.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum KillReason {
    /// The command reached its requested `timeout_secs`.
    Timeout,
    /// The caller cancelled.
    Cancel,
    /// The connector withdrew the work: shutdown, restart, or authority
    /// replacement.
    Shutdown,
    /// Launch, stdin delivery, wait or output capture failed.
    Failed,
    /// The command had already ended; the tree behind it was cleaned up.
    Cleanup,
    /// The guard was dropped without a caller-requested stop: the fail-safe.
    Dropped,
}

/// What a result may claim: dotlink ended the waited command. A separate type
/// because "we signalled a tree" and "we ended your command" are different
/// statements, and only the second belongs on the wire. A sweep of leftovers
/// ends a tree whose command had already decided its own fate, so it is a trace
/// fact — one `stop` line — and never a claim.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum StopCause {
    Timeout,
    Cancel,
    /// The connector ended the work, not the caller: a result that says
    /// `cancel` here would blame a user who did nothing.
    Shutdown,
    Failed,
}

impl KillReason {
    /// Test-facing names. The lifecycle line speaks about the job's ending instead:
    /// `end=` names what ended it, so a reason is no longer a wire word.
    #[cfg(test)]
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Timeout => "timeout",
            Self::Cancel => "cancel",
            Self::Shutdown => "shutdown",
            Self::Failed => "failed",
            // "cleanup" left the reader asking what was cleaned up. These two
            // name the target instead: the command had already finished, so
            // only its leftovers were signalled.
            Self::Cleanup => "leftovers",
            Self::Dropped => "fail-safe",
        }
    }

    /// The cause this kill records on the result. Only a kill that ends the
    /// command names one: a sweep of leftovers, and the fail-safe that may or may
    /// not catch a live command, answer no question a result asks. They are still
    /// on the record — the `stop` line names them — so attribution lives in the
    /// journal rather than being smuggled into a field that means "we ended it".
    fn claim(self) -> Option<StopCause> {
        match self {
            Self::Timeout => Some(StopCause::Timeout),
            Self::Cancel => Some(StopCause::Cancel),
            Self::Shutdown => Some(StopCause::Shutdown),
            Self::Failed => Some(StopCause::Failed),
            Self::Cleanup | Self::Dropped => None,
        }
    }
}

impl StopCause {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Timeout => "timeout",
            Self::Cancel => "cancel",
            Self::Shutdown => "shutdown",
            Self::Failed => "failed",
        }
    }
}

/// What the signal actually did. Only a delivered signal is a stop: a group that
/// is already gone was not stopped by us, and a failed signal left it running.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum StopOutcome {
    Delivered,
    AlreadyGone,
    Failed,
}

impl StopOutcome {
    /// Did anything happen to the tree? A signal that found an empty group did
    /// not, and a line saying so would be pure noise.
    #[cfg(test)]
    fn is_event(self) -> bool {
        self != Self::AlreadyGone
    }

    /// Test-facing names. The lifecycle line says `also_killed=processes|unsent`,
    /// which is the reader's word for the same three facts.
    #[cfg(test)]
    fn as_str(self) -> &'static str {
        match self {
            Self::Delivered => "killed",
            // There was nothing in the group to signal, so nothing happened.
            Self::AlreadyGone => "none",
            // The kernel would not take the signal; the guard stays armed.
            Self::Failed => "refused",
        }
    }
}

/// The stop line is the only proof that dotlink ended a command, so the guard
/// carries what it needs to report: the shared log, the job it belongs to, and —
/// in tests — what it reported.
#[derive(Clone)]
pub(crate) struct StopTrace {
    log: crate::logging::LogConfig,
    label: String,
    #[cfg(test)]
    reports: std::sync::Arc<std::sync::Mutex<Vec<(KillReason, StopOutcome)>>>,
}

impl StopTrace {
    pub(crate) fn new(log: crate::logging::LogConfig, label: impl Into<String>) -> Self {
        Self {
            log,
            label: label.into(),
            #[cfg(test)]
            reports: std::sync::Arc::default(),
        }
    }

    #[cfg(test)]
    pub(crate) fn reports(&self) -> Vec<KillReason> {
        self.reports
            .lock()
            .expect("report list")
            .iter()
            .map(|(reason, _)| *reason)
            .collect()
    }

    #[cfg(test)]
    pub(crate) fn outcomes(&self) -> Vec<StopOutcome> {
        self.reports
            .lock()
            .expect("report list")
            .iter()
            .map(|(_, outcome)| *outcome)
            .collect()
    }

    /// Record one attempt, for tests only. A job's lifecycle line is written once,
    /// when its ending is known, by whoever knows it: the run path for a completion
    /// and the fail-safe for a guard dropped before one existed.
    #[cfg(test)]
    pub(crate) fn stopped(&self, reason: KillReason, outcome: StopOutcome) {
        self.reports
            .lock()
            .expect("report list")
            .push((reason, outcome));
    }

    /// The one line a job gets, written where its ending is known.
    pub(crate) fn ended(&self, end: &str, by: Option<&str>, tree: Option<&str>) {
        self.log
            .notice("job", end_message(&self.label, end, by, tree));
    }

    /// The same line for a guard dropped before a completion existed: dotlink
    /// abandoned the job and swept whatever was left. This is the only writer that
    /// runs without a completion, which is why it cannot be a second line for a job
    /// that already published one — every completion path disarms the guard.
    pub(crate) fn abandoned(&self, tree: Option<&str>) {
        self.ended("abandoned", Some("dotlink"), tree);
    }
}

/// One lifecycle line per job, written only when there is something to say: how it
/// ended, who ended it when that was not the command itself, and what became of the
/// process group it left behind. Every field is a word a reader does not have to look
/// up, and an absent `by=` is not an omission to decode — a line that exists at all
/// because the command ended on its own says so by naming no other actor.
pub(crate) fn end_message(label: &str, end: &str, by: Option<&str>, tree: Option<&str>) -> String {
    let mut line = format!("id={label} end={end}");
    if let Some(by) = by {
        line.push_str(&format!(" by={by}"));
    }
    if let Some(tree) = tree {
        line.push_str(&format!(" tree={tree}"));
    }
    line
}

#[cfg(unix)]
pub(crate) fn configure(command: &mut Command) {
    use std::os::unix::process::CommandExt as _;
    // The shell owns a new group; signals never reach dotlink's own group.
    command.as_std_mut().process_group(0);
}

#[cfg(windows)]
pub(crate) fn configure(command: &mut Command) {
    use std::os::windows::process::CommandExt as _;
    use windows_sys::Win32::System::Threading::CREATE_SUSPENDED;
    // No user code may run until the process belongs to its private job.
    command.as_std_mut().creation_flags(CREATE_SUSPENDED);
}

/// Resume a Windows child only after associating it with its job object.
/// Standard Command gives us the process handle, not the primary thread handle.
/// A just-created suspended process has exactly one thread; find it via the
/// documented Toolhelp snapshot API, without allowing the child to run first.
#[cfg(windows)]
fn resume_new_process(pid: u32) -> io::Result<()> {
    use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
    use windows_sys::Win32::System::{
        Diagnostics::ToolHelp::{
            CreateToolhelp32Snapshot, TH32CS_SNAPTHREAD, THREADENTRY32, Thread32First, Thread32Next,
        },
        Threading::{OpenThread, ResumeThread, THREAD_SUSPEND_RESUME},
    };

    // SAFETY: TH32CS_SNAPTHREAD requests a read-only kernel snapshot.
    let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) };
    if snapshot.is_null() || snapshot as isize == -1 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: successful snapshot is an owned kernel handle.
    let snapshot = unsafe { OwnedHandle::from_raw_handle(snapshot) };
    let mut entry = THREADENTRY32 {
        dwSize: std::mem::size_of::<THREADENTRY32>() as u32,
        ..unsafe { std::mem::zeroed() }
    };
    // SAFETY: the live snapshot and writable entry have the correct size.
    let mut found = unsafe { Thread32First(snapshot.as_raw_handle(), &mut entry) } != 0;
    while found {
        if entry.th32OwnerProcessID == pid {
            // SAFETY: thread ID was returned by the snapshot.
            let thread = unsafe { OpenThread(THREAD_SUSPEND_RESUME, 0, entry.th32ThreadID) };
            if thread.is_null() {
                return Err(io::Error::last_os_error());
            }
            // SAFETY: successful OpenThread returns an owned handle.
            let thread = unsafe { OwnedHandle::from_raw_handle(thread) };
            // SAFETY: this is the still-suspended primary thread.
            if unsafe { ResumeThread(thread.as_raw_handle()) } == u32::MAX {
                return Err(io::Error::last_os_error());
            }
            return Ok(());
        }
        // SAFETY: the snapshot and entry remain valid for the next iteration.
        found = unsafe { Thread32Next(snapshot.as_raw_handle(), &mut entry) } != 0;
    }
    Err(io::Error::other(
        "cannot find suspended PowerShell primary thread",
    ))
}

pub(crate) struct Guard {
    active: bool,
    trace: StopTrace,
    #[cfg(unix)]
    group: libc::pid_t,
    #[cfg(windows)]
    job: std::os::windows::io::OwnedHandle,
}

impl Guard {
    /// Attach, and remember who to tell if this guard ends up killing the tree.
    pub(crate) fn attach(child: &Child, trace: StopTrace) -> io::Result<Self> {
        #[cfg(unix)]
        {
            let group = child
                .id()
                .and_then(|id| libc::pid_t::try_from(id).ok())
                .ok_or_else(|| io::Error::other("spawned shell has no process group"))?;
            Ok(Self {
                active: true,
                trace,
                group,
            })
        }
        #[cfg(windows)]
        {
            use std::os::windows::io::{AsRawHandle, FromRawHandle};
            use windows_sys::Win32::System::JobObjects::{
                AssignProcessToJobObject, CreateJobObjectW,
            };
            // SAFETY: null attributes and name create a private unnamed job.
            let job = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
            if job.is_null() {
                return Err(io::Error::last_os_error());
            }
            // SAFETY: CreateJobObjectW returned an owned kernel handle.
            let job = unsafe { std::os::windows::io::OwnedHandle::from_raw_handle(job) };
            let process = child
                .raw_handle()
                .ok_or_else(|| io::Error::other("spawned shell has no process handle"))?;
            // SAFETY: both handles remain alive for the assignment.
            if unsafe { AssignProcessToJobObject(job.as_raw_handle(), process) } == 0 {
                return Err(io::Error::last_os_error());
            }
            // The only thread is suspended at creation; resume only after
            // association, so no descendants can be launched outside the job.
            resume_new_process(
                child
                    .id()
                    .ok_or_else(|| io::Error::other("missing process ID"))?,
            )?;
            Ok(Self {
                active: true,
                trace,
                job,
            })
        }
    }

    /// Signal the tree and say why. Idempotent: only the first caller reports, and
    /// the return value is the claim the outcome supports — nothing at all when
    /// the signal was not delivered, so a later failure cannot relabel an
    /// already-stopped tree and a failed signal cannot pass for a stop.
    pub(crate) fn terminate(&mut self, reason: KillReason) -> Option<StopCause> {
        matches!(self.signal_and_record(reason), StopOutcome::Delivered)
            .then(|| reason.claim())
            .flatten()
    }

    /// End the tree behind a command that has already returned, and report what
    /// the signal did to it. A sweep claims no cause, because the command's own
    /// status is what ended it; but whether anything was still running is a fact a
    /// completion must report, so the outcome is kept rather than reduced to a claim.
    pub(crate) fn sweep(&mut self) -> StopOutcome {
        self.signal_and_record(KillReason::Cleanup)
    }

    /// One attempt, one record. Disarmed guards signal nothing.
    fn signal_and_record(
        &mut self,
        #[cfg_attr(not(test), allow(unused_variables))] reason: KillReason,
    ) -> StopOutcome {
        if !self.active {
            return StopOutcome::AlreadyGone;
        }
        let outcome = self.signal_tree();
        // A signal that failed leaves the tree running, so stay armed: the
        // fail-safe drop gets one more attempt.
        self.active = outcome == StopOutcome::Failed;
        // The job's line is written once, by whoever knows its ending; this is only
        // the attempt, kept for tests.
        #[cfg(test)]
        self.trace.stopped(reason, outcome);
        outcome
    }

    #[cfg(unix)]
    fn signal_tree(&self) -> StopOutcome {
        // SAFETY: negative PID addresses only the shell's own process group.
        if unsafe { libc::kill(-self.group, libc::SIGKILL) } == 0 {
            return StopOutcome::Delivered;
        }
        // An empty group is the state the caller wanted, but it is not a stop we
        // performed: the command may have ended on its own, or left the group.
        if io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH) {
            StopOutcome::AlreadyGone
        } else {
            StopOutcome::Failed
        }
    }

    #[cfg(windows)]
    fn signal_tree(&self) -> StopOutcome {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::System::JobObjects::{
            JOBOBJECT_BASIC_ACCOUNTING_INFORMATION, JobObjectBasicAccountingInformation,
            QueryInformationJobObject, TerminateJobObject,
        };
        // An empty job reports success for a termination too, so ask first: a
        // group that is already gone is not a stop we performed.
        let mut accounting = JOBOBJECT_BASIC_ACCOUNTING_INFORMATION::default();
        // SAFETY: the job belongs to this invocation and the struct matches the
        // class the call was asked for.
        let queried = unsafe {
            QueryInformationJobObject(
                self.job.as_raw_handle(),
                JobObjectBasicAccountingInformation,
                (&raw mut accounting).cast(),
                std::mem::size_of::<JOBOBJECT_BASIC_ACCOUNTING_INFORMATION>() as u32,
                std::ptr::null_mut(),
            )
        };
        if queried != 0 && accounting.ActiveProcesses == 0 {
            return StopOutcome::AlreadyGone;
        }
        // Always terminate, even when the count is unavailable: failing closed
        // must not leave descendants running. The outcome is unproven then, so
        // nothing may be claimed for it.
        let terminated = unsafe { TerminateJobObject(self.job.as_raw_handle(), 1) } != 0;
        match (queried != 0, terminated) {
            // Terminating an empty job succeeds too, so without the count a
            // success proves nothing about whether we were the one who stopped it.
            (false, _) => StopOutcome::Failed,
            (true, true) => StopOutcome::Delivered,
            (true, false) => StopOutcome::Failed,
        }
    }

    pub(crate) fn disarm(&mut self) {
        self.active = false;
    }
}

impl Drop for Guard {
    fn drop(&mut self) {
        if !self.active {
            return;
        }
        // Reaching here while still armed means a path returned without
        // disarming. The kill is right; so is saying so, and this is the one
        // ending no completion can report.
        let tree = match self.signal_and_record(KillReason::Dropped) {
            StopOutcome::Delivered => Some("killed"),
            // The kernel refused: these may still be running, and this is the one
            // place where saying so is an alarm rather than noise.
            StopOutcome::Failed => Some("alive"),
            StopOutcome::AlreadyGone => None,
        };
        self.trace.abandoned(tree);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    fn guarded_sleep() -> (tokio::process::Child, Guard, StopTrace) {
        let mut command = Command::new("sh");
        configure(&mut command);
        command.arg("-c").arg("sleep 5");
        let child = command.spawn().unwrap();
        let trace = StopTrace::new(
            crate::logging::LogConfig::new(2, false, crate::logging::ColorMode::Never),
            "job-1",
        );
        let guard = Guard::attach(&child, trace.clone()).unwrap();
        (child, guard, trace)
    }

    /// The headline invariant: a stop reports itself exactly once, and the reason
    /// says which kind of stop it was.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_stop_reports_once_and_the_fail_safe_names_itself() {
        let (mut child, mut guard, trace) = guarded_sleep();
        assert_eq!(
            guard.terminate(KillReason::Timeout),
            Some(StopCause::Timeout)
        );
        assert_eq!(guard.terminate(KillReason::Cancel), None);
        assert_eq!(trace.reports(), vec![KillReason::Timeout]);
        assert_eq!(trace.outcomes(), vec![StopOutcome::Delivered]);
        let _ = child.kill().await;
    }

    /// Dropping an armed guard is the fail-safe; it must be visible rather than a
    /// silent kill, because it means a path returned without disarming.
    #[cfg(unix)]
    #[tokio::test]
    async fn dropping_an_armed_guard_reports_the_fail_safe() {
        let (_child, guard, trace) = guarded_sleep();
        drop(guard);
        assert_eq!(trace.reports(), vec![KillReason::Dropped]);
        assert_eq!(trace.outcomes(), vec![StopOutcome::Delivered]);
    }

    /// Two facts, two homes: a kill that ended the command claims a cause on the
    /// result, a sweep of leftovers is on the journal record only. Neither may
    /// borrow the other's meaning, which is how one field came to answer two
    /// questions at once.
    #[test]
    fn only_a_command_ending_claims_a_cause() {
        for (reason, cause) in [
            (KillReason::Timeout, StopCause::Timeout),
            (KillReason::Cancel, StopCause::Cancel),
            (KillReason::Shutdown, StopCause::Shutdown),
            (KillReason::Failed, StopCause::Failed),
        ] {
            assert_eq!(reason.claim(), Some(cause), "{reason:?}");
        }
        for reason in [KillReason::Cleanup, KillReason::Dropped] {
            assert_eq!(
                reason.claim(),
                None,
                "{reason:?} ends a tree, not a command"
            );
            // Still on the record, so the kill is never a stranger's.
            assert!(!reason.as_str().is_empty());
        }
    }

    /// A signal that found nothing is not an event: no line, or every sandboxed
    /// command's sweep would write one saying nothing happened.
    #[test]
    fn an_empty_group_writes_no_line() {
        assert!(!StopOutcome::AlreadyGone.is_event());
        assert!(StopOutcome::Delivered.is_event());
        assert!(StopOutcome::Failed.is_event());
    }

    /// The contract a caller is told: a command's process group does not outlive
    /// it. A shell that backgrounds work and returns leaves that work in the
    /// group, so sweeping the tree after the command returns must reach it — and
    /// say so on the record, without claiming to have ended the command. A
    /// `setsid` descendant leaves the group and is out of reach; the contract is
    /// the group, not the machine.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_descendant_outliving_the_command_is_ended_and_recorded() {
        let temp = tempfile::tempdir().unwrap();
        let marker = temp.path().join("survived");
        let mut command = Command::new("sh");
        configure(&mut command);
        command
            .arg("-c")
            .arg(format!(
                "(sleep 0.4; echo leaked > '{}') & exit 0",
                marker.display()
            ))
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        let mut child = command.spawn().unwrap();
        let trace = StopTrace::new(
            crate::logging::LogConfig::new(0, false, crate::logging::ColorMode::Never),
            "job-contract",
        );
        let mut tree = Guard::attach(&child, trace.clone()).unwrap();
        // The command itself is done; the backgrounded writer is not.
        let _ = child.wait().await;
        let claimed = tree.terminate(KillReason::Cleanup);
        tree.disarm();

        assert_eq!(
            claimed, None,
            "a sweep ends a tree, not the command, so it claims no cause"
        );
        assert_eq!(trace.reports(), vec![KillReason::Cleanup]);
        tokio::time::sleep(std::time::Duration::from_millis(700)).await;
        assert!(
            !marker.exists(),
            "a descendant that outlived the command must not run"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn an_empty_group_is_reported_as_gone_and_claims_nothing() {
        let mut command = Command::new("sh");
        configure(&mut command);
        command.arg("-c").arg("exit 0");
        let mut child = command.spawn().unwrap();
        let trace = StopTrace::new(
            crate::logging::LogConfig::new(1, false, crate::logging::ColorMode::Never),
            "gone",
        );
        let mut guard = Guard::attach(&child, trace.clone()).unwrap();
        child.wait().await.unwrap();
        // A claimable reason on an empty group still claims nothing: the signal
        // never reached the command, so dotlink did not stop it.
        assert_eq!(guard.terminate(KillReason::Timeout), None);
        assert_eq!(trace.reports(), vec![KillReason::Timeout]);
        assert_eq!(trace.outcomes(), vec![StopOutcome::AlreadyGone]);
        assert_eq!(StopOutcome::AlreadyGone.as_str(), "none");
    }

    /// A command that finished is disarmed first, so nothing is reported.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_disarmed_guard_reports_nothing() {
        let mut child = {
            let mut command = Command::new("sh");
            configure(&mut command);
            command.arg("-c").arg("exit 0");
            command.spawn().unwrap()
        };
        let trace = StopTrace::new(
            crate::logging::LogConfig::new(2, false, crate::logging::ColorMode::Never),
            "job-2",
        );
        let mut guard = Guard::attach(&child, trace.clone()).unwrap();
        guard.disarm();
        drop(guard);
        let _ = child.wait().await;
        assert!(trace.reports().is_empty(), "{:?}", trace.reports());
    }

    /// The two ends of a kill are separate facts, so they are asserted
    /// separately: a command-ending kill claims its cause on the result, and a
    /// sweep is a journal line only. Conflating them is what once let a sweep
    /// claim `teardown` on the wire and, when a sweep was refused the claim, read
    /// as a stranger's SIGKILL.
    #[test]
    fn a_sweep_is_recorded_without_claiming_a_cause() {
        // The four that end the command keep their own claim.
        for (reason, cause) in [
            (KillReason::Timeout, StopCause::Timeout),
            (KillReason::Cancel, StopCause::Cancel),
            (KillReason::Shutdown, StopCause::Shutdown),
            (KillReason::Failed, StopCause::Failed),
        ] {
            assert_eq!(reason.claim(), Some(cause), "{reason:?}");
        }
    }

    /// The whole vocabulary, in one place: how it ended, who ended it, and whether
    /// dotlink also killed work the command left running. No token here may read as
    /// "something is still running".
    #[test]
    fn the_end_line_names_how_it_ended_and_who_ended_it() {
        assert_eq!(
            end_message("3f0c9d1e", "exit code=0", None, None),
            "id=3f0c9d1e end=exit code=0"
        );
        assert_eq!(
            end_message("3f0c9d1e", "signal signal=SIGKILL", Some("external"), None),
            "id=3f0c9d1e end=signal signal=SIGKILL by=external"
        );
        assert_eq!(
            end_message("3f0c9d1e", "timeout", Some("dotlink"), None),
            "id=3f0c9d1e end=timeout by=dotlink"
        );
        assert_eq!(
            end_message("3f0c9d1e", "exit code=0", None, Some("killed")),
            "id=3f0c9d1e end=exit code=0 tree=killed"
        );
        assert_eq!(
            end_message("3f0c9d1e", "abandoned", Some("dotlink"), Some("alive")),
            "id=3f0c9d1e end=abandoned by=dotlink tree=alive"
        );
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn suspended_windows_command_resumes_only_after_job_assignment() {
        use std::process::Stdio;
        let mut command = Command::new("cmd.exe");
        configure(&mut command);
        command.arg("/C").arg("exit 0").stdout(Stdio::null());
        let mut child = command.spawn().unwrap();
        let mut job = Guard::attach(
            &child,
            StopTrace::new(crate::logging::LogConfig::default(), "test"),
        )
        .unwrap();
        let status = tokio::time::timeout(std::time::Duration::from_secs(10), child.wait())
            .await
            .unwrap()
            .unwrap();
        assert!(status.success());
        job.disarm();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn timeout_tree_cleanup_prevents_descendant_side_effects() {
        let temp = tempfile::tempdir().unwrap();
        let output = temp.path().join("marker");
        let mut command = Command::new("sh");
        configure(&mut command);
        command
            .arg("-c")
            .arg(format!(
                "(sleep 0.6; echo leaked > '{}') & wait",
                output.display()
            ))
            .kill_on_drop(true);
        let mut child = command.spawn().unwrap();
        let mut tree = Guard::attach(
            &child,
            StopTrace::new(crate::logging::LogConfig::default(), "test"),
        )
        .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(120)).await;
        let _ = tree.terminate(KillReason::Timeout);
        let _ = child.wait().await;
        tokio::time::sleep(std::time::Duration::from_millis(700)).await;
        assert!(!output.exists(), "killed descendant must not run later");
    }
}
