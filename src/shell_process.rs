//! Kill the contained process group/job on timeout, error or cancellation.
//! Unsandboxed Unix children can intentionally escape their process group
//! (setsid); no process-group API can guarantee descendant containment.
//! Descendants never outlive the command: the tree is ended when the command
//! returns, and a sandboxed tree is already gone with its namespace. Work that
//! must outlive the call belongs in a durable job (`background: true`), where the
//! process is the job rather than a leftover of one.

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

/// What a result may claim: dotlink stopped the waited command. A separate type
/// because "we signalled a tree" and "we ended your command" are different
/// statements, and only the second belongs on the wire.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum StopCause {
    Timeout,
    Cancel,
    /// The connector ended the work, not the caller: a result that says
    /// `cancel` here would blame a user who did nothing.
    Shutdown,
    Failed,
    /// dotlink ended the tree without ending the command: tidy-up after the
    /// command had already exited, or the guard's fail-safe. It names no cause
    /// because none applies, but it must still record that the signal was ours —
    /// otherwise a kill we performed reads as a stranger's SIGKILL.
    Teardown,
}

impl KillReason {
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

    /// The cause this kill records. Every delivered kill records one, because
    /// "dotlink signalled this tree" and "no cause applies" are different facts:
    /// the first decides attribution, the second only decides wording. Tidy-up
    /// and the fail-safe end a tree rather than the command, so they claim
    /// [`StopCause::Teardown`] instead of a cause.
    fn claim(self) -> Option<StopCause> {
        match self {
            Self::Timeout => Some(StopCause::Timeout),
            Self::Cancel => Some(StopCause::Cancel),
            Self::Shutdown => Some(StopCause::Shutdown),
            Self::Failed => Some(StopCause::Failed),
            Self::Cleanup | Self::Dropped => Some(StopCause::Teardown),
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
            Self::Teardown => "teardown",
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
    fn as_str(self) -> &'static str {
        match self {
            // SIGKILL reached a live process in the group. The verb is the act,
            // not the command's fate: a sweep of leftovers is still a kill.
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
    #[cfg(test)]
    exits: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
}

impl StopTrace {
    pub(crate) fn new(log: crate::logging::LogConfig, label: impl Into<String>) -> Self {
        Self {
            log,
            label: label.into(),
            #[cfg(test)]
            reports: std::sync::Arc::default(),
            #[cfg(test)]
            exits: std::sync::Arc::default(),
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

    #[cfg(test)]
    pub(crate) fn exits(&self) -> Vec<String> {
        self.exits.lock().expect("exit list").clone()
    }

    pub(crate) fn stopped(&self, reason: KillReason, outcome: StopOutcome) {
        // A stop is rare and is the only proof of what the connector did, so it
        // is a lifecycle line: visible whatever verbosity the operator chose,
        // including quiet, because silence here would read as "not us".
        #[cfg(test)]
        self.reports
            .lock()
            .expect("report list")
            .push((reason, outcome));
        self.log
            .notice("job", stop_message(&self.label, reason, outcome));
    }

    /// The other ending: a process that died on a signal dotlink did not deliver.
    /// Paired with [`Self::stopped`] so a job carries exactly one of the two
    /// lines, and a `signal` nobody can account for is still on the record.
    /// `from=outside` states only that the signal was not ours.
    ///
    /// Lifecycle level for the same reason a stop is: its absence has to mean
    /// nothing was signalled, which no verbosity setting may be able to fake.
    pub(crate) fn exited(&self, signal_name: &str) {
        #[cfg(test)]
        self.exits
            .lock()
            .expect("exit list")
            .push(signal_name.to_owned());
        self.log
            .notice("job", exit_message(&self.label, signal_name));
    }
}

/// One line, fixed field order, no prose. The outcome is part of it: a signal that
/// was not delivered must never read as a stop.
fn stop_message(label: &str, reason: KillReason, outcome: StopOutcome) -> String {
    format!(
        "stop id={label} reason={} outcome={}",
        reason.as_str(),
        outcome.as_str()
    )
}

/// The other line, same shape. `from=outside` states the signal was not ours; it
/// does not claim who sent it.
fn exit_message(label: &str, signal_name: &str) -> String {
    format!("exit id={label} signal={signal_name} from=outside")
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
        if !self.active {
            return None;
        }
        let outcome = self.signal_tree();
        // A signal that failed leaves the tree running, so stay armed: the
        // fail-safe drop gets one more attempt.
        self.active = outcome == StopOutcome::Failed;
        self.trace.stopped(reason, outcome);
        matches!(outcome, StopOutcome::Delivered)
            .then(|| reason.claim())
            .flatten()
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
        // Reaching here while still armed means a path returned without
        // disarming; the kill is right, and `reason=dropped` makes it visible.
        let _ = self.terminate(KillReason::Dropped);
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

    /// The other half of the record, and the reason it is separate: an exit names
    /// a signal the guard never sent, so it can never pass for a stop.
    #[cfg(unix)]
    #[tokio::test]
    async fn an_exit_names_the_signal_and_never_claims_a_stop() {
        let trace = StopTrace::new(crate::logging::LogConfig::default(), "test");
        assert!(trace.exits().is_empty());
        trace.exited("SIGKILL");
        assert_eq!(trace.exits(), vec!["SIGKILL".to_owned()]);
        assert!(trace.reports().is_empty(), "an exit is not a stop");
    }

    /// Every delivered kill is attributable, including the two that name no
    /// cause. Otherwise `stopped_by` is absent for a kill dotlink performed, and
    /// `external_signal` reads that absence as a stranger's SIGKILL — which is
    /// how the journal and the result once contradicted each other.
    #[test]
    fn every_delivered_kill_records_that_it_was_ours() {
        for reason in [
            KillReason::Timeout,
            KillReason::Cancel,
            KillReason::Shutdown,
            KillReason::Failed,
            KillReason::Cleanup,
            KillReason::Dropped,
        ] {
            assert!(
                reason.claim().is_some(),
                "{} must record that dotlink signalled",
                reason.as_str()
            );
        }
        // The two that end a tree rather than a command name no cause, but they
        // still say the signal was ours.
        for reason in [KillReason::Cleanup, KillReason::Dropped] {
            assert_eq!(reason.claim(), Some(StopCause::Teardown));
            assert_eq!(reason.claim().map(StopCause::as_str), Some("teardown"));
        }
    }

    /// A group that is already empty is not a stop we performed: claiming one
    /// would read as "dotlink killed your command" when the command had ended.
    /// The contract a caller is told: descendants do not outlive the command.
    /// A shell that backgrounds work and returns leaves that work in this tree,
    /// so ending the tree after the command returns must reach it — and say so.
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
            claimed,
            Some(StopCause::Teardown),
            "ending live leftovers is a signal dotlink delivered"
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

    /// Every delivered kill is dotlink's, so every delivered kill must be able to
    /// say so — including the two that end a tree rather than a command. Reading
    /// "no cause" as "not ours" is how dotlink's own teardown became a report of a
    /// stranger's SIGKILL.
    #[test]
    fn a_teardown_kill_records_itself_instead_of_looking_external() {
        for reason in [KillReason::Cleanup, KillReason::Dropped] {
            assert_eq!(reason.claim(), Some(StopCause::Teardown), "{reason:?}");
        }
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

    /// The fail-safe is the last authority: dropping an armed guard kills the
    /// group and records it. A guard that killed nothing claims nothing.
    #[tokio::test]
    async fn an_armed_guard_that_is_dropped_records_a_signal() {
        let mut command = Command::new("sh");
        configure(&mut command);
        command.arg("-c").arg("sleep 30");
        let mut child = command.spawn().unwrap();
        let trace = StopTrace::new(
            crate::logging::LogConfig::new(0, false, crate::logging::ColorMode::Never),
            "job-dropped",
        );
        let guard = Guard::attach(&child, trace.clone()).unwrap();
        drop(guard);
        assert_eq!(
            trace.reports(),
            vec![KillReason::Dropped],
            "a dropped armed guard must record the kill it performed"
        );
        let _ = child.kill().await;
        let _ = child.wait().await;
    }

    #[test]
    fn stop_line_names_the_job_and_the_reason() {
        assert_eq!(
            stop_message(
                "3f0c9d1e-0d6f-4a0e-9a4e-1a2b3c4d5e6f",
                KillReason::Timeout,
                StopOutcome::Delivered
            ),
            "stop id=3f0c9d1e-0d6f-4a0e-9a4e-1a2b3c4d5e6f reason=timeout outcome=killed"
        );
        for reason in [
            KillReason::Timeout,
            KillReason::Cancel,
            KillReason::Shutdown,
            KillReason::Failed,
            KillReason::Cleanup,
            KillReason::Dropped,
        ] {
            let delivered = stop_message("job", reason, StopOutcome::Delivered);
            assert!(delivered.starts_with("stop "), "{delivered}");
            assert!(delivered.contains(&format!("reason={}", reason.as_str())));
            assert!(delivered.contains("outcome=killed"), "{delivered}");
            // A signal that did not reach a live process must not read as sent.
            for outcome in [StopOutcome::AlreadyGone, StopOutcome::Failed] {
                let line = stop_message("job", reason, outcome);
                assert!(line.contains("outcome="), "{line}");
                assert!(!line.contains("outcome=killed"), "{line}");
            }
        }
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
