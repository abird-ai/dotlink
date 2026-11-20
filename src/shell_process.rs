//! Kill the contained process group/job on stop, and sweep it on command exit.
//! Observe live members *before* cleanup. A delivered signal alone proves nothing
//! about leftovers: even a group containing only zombies accepts SIGKILL.

#[cfg(target_os = "linux")]
mod linux_observation;
#[cfg(target_os = "linux")]
mod namespace_report;
#[cfg(unix)]
mod unix_wait;

use std::{io, process::ExitStatus};
use tokio::process::{Child, Command};

/// What caused dotlink to stop the command. A post-exit sweep has no such cause.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum StopCause {
    Timeout,
    Cancel,
    /// The connector ended the work, not the caller: a result that says
    /// `cancel` here would blame a user who did nothing.
    Shutdown,
    Failed,
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

/// What the signal call returned. Only a delivered signal is a stop, and even that
/// proves little: the kernel accepts a signal for a group of zombies, so it shows
/// neither that work was running nor that it has ended. A group already gone was
/// not stopped by us, and a failed signal — refused, or on Windows unverifiable —
/// leaves termination unproven.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum StopOutcome {
    Delivered,
    AlreadyGone,
    #[cfg(windows)]
    Cleared, // successful job termination, occupancy unknown
    Failed,
}

/// A post-exit observation, not a claim about what the cleanup signal killed.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) enum TreeObservation {
    #[default]
    None,
    Leftovers,
    Unknown,
}

impl TreeObservation {
    pub(crate) fn word(self) -> Option<&'static str> {
        match self {
            Self::None => None,
            Self::Leftovers => Some("leftovers"),
            Self::Unknown => Some("unknown"),
        }
    }
    fn from_live(live: Option<bool>) -> Self {
        match live {
            Some(true) => Self::Leftovers,
            Some(false) => Self::None,
            None => Self::Unknown,
        }
    }
}

/// How a job ended, as the record names it. A type, so "is this the command's own
/// ending" is a match rather than a string prefix.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum End {
    Exit(i32),
    Signal(i32),
    Timeout,
    Cancel,
    Error,
    Abandoned,
}

impl End {
    fn describe(self) -> String {
        match self {
            Self::Exit(code) => format!("exit code={code}"),
            Self::Signal(signal) => format!("signal {}", crate::jobs::signal_token(signal)),
            Self::Timeout => "timeout".to_owned(),
            Self::Cancel => "cancel".to_owned(),
            Self::Error => "error".to_owned(),
            Self::Abandoned => "abandoned".to_owned(),
        }
    }

    /// An ordinary exit is silent unless a tree observation or cleanup alarm adds information.
    pub(crate) fn is_own(self) -> bool {
        matches!(self, Self::Exit(_))
    }
}

#[cfg(test)]
#[derive(Default)]
struct RecordedTrace {
    attempts: Vec<StopOutcome>,
    lines: Vec<String>,
}

/// Job-scoped lifecycle journal, shared with the guard's fail-safe.
#[derive(Clone)]
pub(crate) struct StopTrace {
    log: crate::logging::LogConfig,
    label: String,
    #[cfg(test)]
    recorded: std::sync::Arc<std::sync::Mutex<RecordedTrace>>,
    #[cfg(test)]
    pub(crate) refuse_first: usize,
}

impl StopTrace {
    pub(crate) fn new(log: crate::logging::LogConfig, label: impl Into<String>) -> Self {
        Self {
            log,
            label: label.into(),
            #[cfg(test)]
            recorded: Default::default(),
            #[cfg(test)]
            refuse_first: 0,
        }
    }

    #[cfg(test)]
    pub(crate) fn outcomes(&self) -> Vec<StopOutcome> {
        self.recorded.lock().unwrap().attempts.clone()
    }

    #[cfg(test)]
    pub(crate) fn lines(&self) -> Vec<String> {
        self.recorded.lock().unwrap().lines.clone()
    }

    /// The one line a job gets, written where its ending is known.
    pub(crate) fn ended(
        &self,
        end: &End,
        by: Option<&str>,
        observation: TreeObservation,
        kill_failed: bool,
    ) {
        let line = end_message(&self.label, &end.describe(), by, observation, kill_failed);
        self.log.notice("job", &line);
        #[cfg(test)]
        self.recorded.lock().unwrap().lines.push(line);
    }

    /// The same line for a guard dropped before a completion existed: dotlink
    /// abandoned the job and killed its tree. This is the only writer that runs
    /// without a completion, which is why it cannot be a second line for a job that
    /// already published one — every completion path disarms the guard.
    pub(crate) fn abandoned(&self, kill_failed: bool) {
        self.ended(&End::Abandoned, None, TreeObservation::None, kill_failed);
    }
}

/// Keep observation separate from an independently failed cleanup attempt.
pub(crate) fn end_message(
    label: &str,
    end: &str,
    by: Option<&str>,
    observation: TreeObservation,
    kill_failed: bool,
) -> String {
    let mut line = format!("id={label} end={end}");
    if let Some(by) = by {
        line.push_str(&format!(" by={by}"));
    }
    if let Some(tree) = observation.word() {
        line.push_str(" tree=");
        line.push_str(tree);
    }
    if kill_failed {
        line.push_str(" cleanup=kill-failed");
    }
    line
}

/// Establish the containment boundary before user code can run.
pub(crate) fn spawn(command: &mut Command) -> io::Result<Child> {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;
        use tokio::signal::unix::{SignalKind, signal};
        // Tokio keeps the installed handler after the listener drops. Install it
        // *before* spawn: inherited SIG_IGN would otherwise auto-reap the leader.
        drop(signal(SignalKind::child())?);
        command.as_std_mut().process_group(0);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt as _;
        use windows_sys::Win32::System::Threading::{CREATE_SUSPENDED, DETACHED_PROCESS};
        // Noninteractive shell I/O is piped. Never inherit or allocate a console
        // for the interpreter; job accounting needs no image-name exemptions.
        command
            .as_std_mut()
            .creation_flags(CREATE_SUSPENDED | DETACHED_PROCESS);
    }
    command.kill_on_drop(true).spawn()
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

#[cfg(windows)]
const WINDOWS_KILL_CODE: u32 = 0xD071_1A7E; // private termination marker, not a reserved OS code

/// Mutually exclusive evidence: the command's own status or a possible dotlink stop.
#[derive(Clone, Copy, Debug)]
pub(crate) enum ChildEnd {
    Exited(ExitStatus),
    Stopped(Option<StopCause>),
}

/// What killing the waited child proved. The two facts are separate because
/// neither alone is portable evidence: a kill can be accepted for a process that
/// has already exited (Unix zombies), and a kill can be rejected while a status
/// is still readable (Windows).
struct ChildStop {
    /// The child kill was accepted; alone this cannot identify the sender of a terminal SIGKILL.
    delivered: bool,
    /// The status the process was finally reaped with, when one was readable.
    status: Option<std::process::ExitStatus>,
}

impl ChildStop {
    async fn kill(child: &mut Child) -> Self {
        #[cfg(unix)]
        let delivered = child.start_kill().is_ok();
        #[cfg(windows)]
        let delivered = child.raw_handle().is_some_and(|process| {
            // SAFETY: the child owns this handle through the termination call.
            unsafe {
                windows_sys::Win32::System::Threading::TerminateProcess(process, WINDOWS_KILL_CODE)
                    != 0
            }
        });
        Self {
            delivered,
            status: child.wait().await.ok(),
        }
    }

    /// Unix tree and child kills send only SIGKILL. An exit code or any other
    /// terminal signal wins even if the group (possibly zombies) accepted our kill.
    /// Both Windows kill paths use the same private marker. No delivery also rules out our kill.
    fn independent_status(&self, tree_may_have_killed: bool) -> Option<std::process::ExitStatus> {
        let status = self.status?;
        #[cfg(unix)]
        let independent = status.code().is_some()
            || std::os::unix::process::ExitStatusExt::signal(&status)
                .is_some_and(|n| n != libc::SIGKILL);
        #[cfg(windows)]
        let independent = status
            .code()
            .is_some_and(|code| code != WINDOWS_KILL_CODE as i32);
        (independent || !(tree_may_have_killed || self.delivered)).then_some(status)
    }

    fn tree_ending(&self, cause: StopCause, outcome: StopOutcome) -> ChildEnd {
        let delivery = match outcome {
            StopOutcome::Delivered => true,
            StopOutcome::AlreadyGone | StopOutcome::Failed => false,
            // Terminating an empty job also succeeds. Without accounting,
            // require a matching leader status before attributing the stop.
            #[cfg(windows)]
            StopOutcome::Cleared => {
                self.status.and_then(|s| s.code()) == Some(WINDOWS_KILL_CODE as i32)
            }
        };
        self.ending(delivery.then_some(cause))
    }

    fn ending(&self, cause: Option<StopCause>) -> ChildEnd {
        match self.independent_status(cause.is_some()) {
            Some(status) => ChildEnd::Exited(status),
            None => ChildEnd::Stopped(cause),
        }
    }
}

/// Where this command's pre-cleanup evidence comes from. Owned by the guard once spawned.
#[derive(Debug)]
pub(crate) enum TreeObserver {
    Direct,
    #[cfg(target_os = "linux")]
    Namespace(std::os::unix::net::UnixDatagram),
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum GuardState {
    Armed,
    Disarmed,
    /// Both attempts failed. Journal the alarm at completion or on abandonment; don't signal again.
    KillFailed,
}

pub(crate) struct Guard<'a> {
    child: &'a mut Child,
    observer: TreeObserver,
    state: GuardState,
    pub(crate) observation: TreeObservation,
    trace: StopTrace,
    #[cfg(unix)]
    group: libc::pid_t,
    #[cfg(windows)]
    job: std::os::windows::io::OwnedHandle,
}

#[cfg(unix)]
fn attach_target(child: &Child) -> io::Result<libc::pid_t> {
    child
        .id()
        .and_then(|id| libc::pid_t::try_from(id).ok())
        .ok_or_else(|| io::Error::other("spawned shell has no process group"))
}

#[cfg(windows)]
fn attach_target(child: &Child) -> io::Result<std::os::windows::io::OwnedHandle> {
    use std::os::windows::io::{AsRawHandle, FromRawHandle};
    use windows_sys::Win32::System::JobObjects::{AssignProcessToJobObject, CreateJobObjectW};
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
    Ok(job)
}

impl<'a> Guard<'a> {
    /// Either take exclusive reaping authority or stop the uncontained child.
    pub(crate) async fn attach(
        child: &'a mut Child,
        observer: TreeObserver,
        trace: StopTrace,
    ) -> Result<Self, (io::Error, ChildEnd)> {
        let target = match attach_target(child) {
            Ok(target) => target,
            Err(error) => {
                let stop = ChildStop::kill(child).await;
                return Err((
                    error,
                    stop.ending(stop.delivered.then_some(StopCause::Failed)),
                ));
            }
        };
        Ok(Self {
            child,
            observer,
            trace,
            state: GuardState::Armed,
            observation: TreeObservation::None,
            #[cfg(unix)]
            group: target,
            #[cfg(windows)]
            job: target,
        })
    }

    /// The last kill failed or was unverifiable. On Unix even our own child's
    /// group can return `EPERM` if all remaining members changed credentials.
    pub(crate) fn kill_failed(&self) -> bool {
        self.state == GuardState::KillFailed
    }

    /// Stop before reaping. A status inconsistent with our kill remains the command's own.
    pub(crate) async fn stop(&mut self, cause: StopCause) -> ChildEnd {
        #[cfg(target_os = "linux")]
        let before = match &self.observer {
            TreeObserver::Namespace(socket) => NamespaceEvidence::Empty.read(socket),
            TreeObserver::Direct => NamespaceEvidence::Empty,
        };
        let outcome = self.signal();
        let reaped = ChildStop::kill(self.child).await;
        let ending = reaped.tree_ending(cause, outcome);
        #[cfg(target_os = "linux")]
        if let TreeObserver::Namespace(socket) = &self.observer {
            let after = before.read(socket);
            if let ChildEnd::Exited(launcher) = ending {
                let (status, tree) = after.corroborated(launcher);
                self.observation = tree;
                return ChildEnd::Exited(status);
            }
            // If we interrupted the launcher, it cannot corroborate the report.
            // A status published *before* our signal is independent even for SIGKILL.
            // A later non-SIGKILL ending also rules out our kill, but its scan
            // could have overlapped cleanup and cannot establish the original tree.
            use std::os::unix::process::ExitStatusExt;
            if !matches!(after, NamespaceEvidence::Invalid)
                && let Some((status, tree)) = before.facts().or_else(|| {
                    let (status, _) = after.facts()?;
                    (status.signal() != Some(libc::SIGKILL))
                        .then_some((status, TreeObservation::Unknown))
                })
            {
                self.observation = tree;
                return ChildEnd::Exited(status);
            }
        }
        if matches!(ending, ChildEnd::Exited(_)) {
            self.observation = TreeObservation::Unknown;
        }
        ending
    }

    pub(crate) async fn wait(&mut self) -> io::Result<ExitStatus> {
        #[cfg(unix)]
        {
            unix_wait::wait(self.group).await?;
            self.collect_waitable()
        }
        #[cfg(windows)]
        {
            let status = self.child.wait().await?;
            let tree = self.sweep(); // the private job handle survives the leader
            Ok(self.command_exit(status, tree))
        }
    }

    pub(crate) fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        #[cfg(unix)]
        {
            unix_wait::exited(self.group)?
                .then(|| self.collect_waitable())
                .transpose()
        }
        #[cfg(windows)]
        {
            Ok(self.child.try_wait()?.map(|status| {
                let tree = self.sweep();
                self.command_exit(status, tree)
            }))
        }
    }

    #[cfg(unix)]
    fn collect_waitable(&mut self) -> io::Result<ExitStatus> {
        // Only this guard may reap. Until the last group signal, the exited
        // leader still owns its PID; even an otherwise empty PGID cannot be reused.
        let tree = self.sweep();
        let status = self
            .child
            .try_wait()?
            .ok_or_else(|| io::Error::other("child exit was not waitable"))?;
        Ok(self.command_exit(status, tree))
    }

    fn sweep(&mut self) -> TreeObservation {
        let tree = match self.observer {
            TreeObserver::Direct => self.observe_direct_tree(),
            #[cfg(target_os = "linux")]
            TreeObserver::Namespace(_) => TreeObservation::Unknown, // evidence travels in the report
        };
        self.signal();
        tree
    }

    fn command_exit(&mut self, launcher: ExitStatus, direct: TreeObservation) -> ExitStatus {
        let (status, tree) = match &self.observer {
            TreeObserver::Direct => (launcher, direct),
            #[cfg(target_os = "linux")]
            TreeObserver::Namespace(socket) => {
                NamespaceEvidence::Empty.read(socket).corroborated(launcher)
            }
        };
        self.observation = tree;
        status
    }

    fn observe_direct_tree(&self) -> TreeObservation {
        #[cfg(target_os = "linux")]
        let check = crate::blocking::run(|| linux_observation::has_live_member(Some(self.group)));
        #[cfg(target_os = "macos")]
        let check = crate::blocking::run(|| mac_group_has_live_member(self.group));
        #[cfg(windows)]
        let check = self.active_job_processes().map(|n| n != 0);
        #[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
        let check: io::Result<bool> = Err(io::ErrorKind::Unsupported.into());
        TreeObservation::from_live(check.ok())
    }

    /// Retry an unconfirmed kill once, immediately — never after draining output
    /// when a Unix process-group number may already have been reused.
    fn signal(&mut self) -> StopOutcome {
        match self.state {
            GuardState::KillFailed => return StopOutcome::Failed,
            GuardState::Disarmed => return StopOutcome::AlreadyGone,
            GuardState::Armed => (),
        }
        let mut outcome = self.attempt_signal();
        if outcome == StopOutcome::Failed {
            outcome = self.attempt_signal();
        }
        self.state = if outcome == StopOutcome::Failed {
            GuardState::KillFailed
        } else {
            GuardState::Disarmed
        };
        outcome
    }

    fn attempt_signal(&self) -> StopOutcome {
        #[cfg(test)]
        {
            let mut record = self.trace.recorded.lock().unwrap();
            let outcome = if record.attempts.len() < self.trace.refuse_first {
                StopOutcome::Failed
            } else {
                self.signal_tree()
            };
            record.attempts.push(outcome);
            outcome
        }
        #[cfg(not(test))]
        self.signal_tree()
    }

    #[cfg(unix)]
    fn signal_tree(&self) -> StopOutcome {
        // SAFETY: negative PID addresses only the shell's own process group.
        if unsafe { libc::kill(-self.group, libc::SIGKILL) } == 0 {
            return StopOutcome::Delivered;
        }
        // An empty group is the state the caller wanted, but it is not a stop we
        // performed: the command may have ended on its own, or left the group.
        match io::Error::last_os_error().raw_os_error() {
            Some(libc::ESRCH) => StopOutcome::AlreadyGone,
            // Darwin's killpg1 excludes zombies and can return EPERM when no
            // signalable member exists. The scan skips the leader, so first
            // prove it exited without reaping it. Live or unknown stays failed.
            #[cfg(target_os = "macos")]
            Some(libc::EPERM)
                if matches!(unix_wait::exited(self.group), Ok(true))
                    && self.observe_direct_tree() == TreeObservation::None =>
            {
                StopOutcome::AlreadyGone
            }
            _ => StopOutcome::Failed,
        }
    }

    #[cfg(windows)]
    fn active_job_processes(&self) -> io::Result<u32> {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::System::JobObjects::{
            JOBOBJECT_BASIC_ACCOUNTING_INFORMATION, JobObjectBasicAccountingInformation,
            QueryInformationJobObject,
        };
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
        if queried == 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(accounting.ActiveProcesses)
        }
    }

    #[cfg(windows)]
    fn signal_tree(&self) -> StopOutcome {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::System::JobObjects::TerminateJobObject;
        let count = self.active_job_processes();
        if matches!(count, Ok(0)) {
            return StopOutcome::AlreadyGone;
        }
        // Always terminate, even when the count is unavailable: failing closed
        // must not leave descendants running. Cleanup and attribution are separate.
        let terminated =
            unsafe { TerminateJobObject(self.job.as_raw_handle(), WINDOWS_KILL_CODE) } != 0;
        match (count.is_ok(), terminated) {
            // Terminating an empty job succeeds too, so without the count a
            // success proves nothing about whether we were the one who stopped it.
            (false, true) => StopOutcome::Cleared,
            (true, true) => StopOutcome::Delivered,
            (_, false) => StopOutcome::Failed,
        }
    }

    pub(crate) fn disarm(&mut self) {
        self.state = GuardState::Disarmed;
    }
}

/// Ordered private datagrams: wait status first, then one post-exit tree observation.
#[cfg(target_os = "linux")]
#[derive(Clone, Copy)]
enum NamespaceEvidence {
    Empty,
    Waited(namespace_report::ShellStatus),
    Complete(namespace_report::ShellStatus, TreeObservation),
    Invalid,
}

#[cfg(target_os = "linux")]
impl NamespaceEvidence {
    fn read(mut self, socket: &std::os::unix::net::UnixDatagram) -> Self {
        while matches!(self, Self::Empty | Self::Waited(_)) {
            // One byte larger than either packet, so truncated datagrams never validate.
            let mut buf = [0; 3];
            let bytes = match socket.recv(&mut buf) {
                Ok(len) => &buf[..len],
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(_) => return Self::Invalid,
            };
            self = match (self, bytes) {
                (Self::Empty, packet) => namespace_report::ShellStatus::try_from(packet)
                    .map_or(Self::Invalid, Self::Waited),
                (Self::Waited(status), b"L") => Self::Complete(status, TreeObservation::Leftovers),
                (Self::Waited(status), b"N") => Self::Complete(status, TreeObservation::None),
                (Self::Waited(status), b"?") => Self::Complete(status, TreeObservation::Unknown),
                _ => Self::Invalid,
            };
        }
        self
    }

    fn facts(self) -> Option<(ExitStatus, TreeObservation)> {
        match self {
            Self::Waited(status) => Some((status.0, TreeObservation::Unknown)),
            Self::Complete(status, tree) => Some((status.0, tree)),
            _ => None,
        }
    }

    fn corroborated(self, launcher: ExitStatus) -> (ExitStatus, TreeObservation) {
        if let Self::Complete(status, tree) = self
            && launcher.code() == Some(i32::from(status.launcher_code()))
        {
            (status.0, tree)
        } else {
            (launcher, TreeObservation::Unknown)
        }
    }
}

#[cfg(target_os = "macos")]
fn mac_group_has_live_member(group: libc::pid_t) -> io::Result<bool> {
    let mut pids: Vec<libc::pid_t> = vec![0; 32];
    loop {
        let bytes = std::mem::size_of_val(&pids[..]);
        // Apple libsyscall/libproc.c: proc_listpgrppids divides proc_listpids' byte result by sizeof(int).
        // SAFETY: writable pid array of exactly `bytes` bytes.
        let used =
            unsafe { libc::proc_listpgrppids(group, pids.as_mut_ptr().cast(), bytes as i32) };
        if used <= 0 {
            // The waitable leader still owns this group. No list is not proof of no leftovers.
            return Err(io::Error::other("cannot list command process group"));
        }
        let listed = &pids[..pids.len().min(used as usize)];
        for &pid in listed {
            // The waitable leader is already terminal.
            if pid <= 0 || pid == group {
                continue;
            }
            let mut info = std::mem::MaybeUninit::<libc::proc_bsdinfo>::uninit();
            let size = size_of::<libc::proc_bsdinfo>() as i32;
            // SAFETY: output storage matches the flavor/size requested.
            let got = unsafe {
                libc::proc_pidinfo(
                    pid,
                    libc::PROC_PIDTBSDINFO,
                    0,
                    info.as_mut_ptr().cast(),
                    size,
                )
            };
            if got != size {
                let err = io::Error::last_os_error();
                if err.raw_os_error() == Some(libc::ESRCH) {
                    continue;
                }
                return Err(err);
            }
            // SAFETY: proc_pidinfo returned the entire structure.
            let info = unsafe { info.assume_init() };
            if info.pbi_pgid == group as u32 && info.pbi_status != libc::SZOMB {
                return Ok(true);
            }
        }
        if listed.len() < pids.len() {
            return Ok(false);
        }
        if pids.len() >= 65536 {
            return Err(io::Error::other("incomplete process group listing"));
        }
        pids.resize(pids.len() * 2, 0);
    }
}

impl Drop for Guard<'_> {
    fn drop(&mut self) {
        match self.state {
            GuardState::Armed => {
                self.signal();
                self.trace.abandoned(self.kill_failed());
            }
            GuardState::KillFailed => self.trace.abandoned(true),
            GuardState::Disarmed => (),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(target_os = "linux")]
    #[test]
    fn sandbox_report_requires_a_terminal_packet_and_matching_launcher() {
        use std::os::unix::{net::UnixDatagram, process::ExitStatusExt};

        let code137 = ExitStatus::from_raw(137 << 8);
        let killed = ExitStatus::from_raw(libc::SIGKILL);
        for (status, tree, launcher) in [
            (None, None, code137),
            (Some(&b"S"[..]), None, code137),
            (Some(b"S\x09!"), None, code137),
            (Some(b"S\0"), None, code137),
            (Some(b"S\x7f"), None, code137),
            (Some(b"S\x09"), None, code137),
            (Some(b"S\x09"), Some(&b"X"[..]), code137),
            (Some(b"S\x09"), Some(b"L!"), code137),
            (Some(b"E\0"), Some(b"L"), code137),
            (Some(b"S\x09"), Some(b"L"), killed),
            (Some(b"S\x09"), Some(b"?"), code137),
        ] {
            let (rx, tx) = UnixDatagram::pair().unwrap();
            rx.set_nonblocking(true).unwrap();
            for packet in [status, tree].into_iter().flatten() {
                tx.send(packet).unwrap();
            }
            let expected = if tree == Some(b"?") { killed } else { launcher };
            assert_eq!(
                NamespaceEvidence::Empty.read(&rx).corroborated(launcher),
                (expected, TreeObservation::Unknown)
            );
        }
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn stop_during_observer_tail_preserves_only_published_evidence() {
        use std::os::unix::{net::UnixDatagram, process::ExitStatusExt};
        // A live launcher, but the private observer has already waited on the
        // command. Exercise either side of the scan without a timing-dependent race.
        for cause in [StopCause::Timeout, StopCause::Cancel] {
            for tree in [None, Some(&b"L"[..]), Some(b"N")] {
                let (recv, send) = UnixDatagram::pair().unwrap();
                recv.set_nonblocking(true).unwrap();
                let (mut child, trace) = guarded_sleep();
                let mut guard = Guard::attach(&mut child, TreeObserver::Namespace(recv), trace)
                    .await
                    .unwrap();
                let packet = if cause == StopCause::Timeout {
                    b"E\x00"
                } else {
                    b"S\x09"
                };
                send.send(packet).unwrap();
                if let Some(packet) = tree {
                    send.send(packet).unwrap();
                }
                assert!(guard.try_wait().unwrap().is_none());
                let ChildEnd::Exited(status) = guard.stop(cause).await else {
                    panic!("stopping only the launcher cannot own the shell exit");
                };
                if cause == StopCause::Timeout {
                    assert_eq!(status.code(), Some(0));
                } else {
                    assert_eq!(status.signal(), Some(libc::SIGKILL));
                }
                assert_eq!(
                    guard.observation,
                    match tree {
                        Some(b"L") => TreeObservation::Leftovers,
                        Some(b"N") => TreeObservation::None,
                        _ => TreeObservation::Unknown,
                    }
                );
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_racing_stop_cannot_claim_a_different_terminal_signal_or_an_exit_code() {
        use std::{os::unix::process::ExitStatusExt, process::ExitStatus};
        for raw in [3 << 8, libc::SIGTERM, libc::SIGSEGV, libc::SIGKILL] {
            let status = ExitStatus::from_raw(raw);
            let stop = ChildStop {
                delivered: true,
                status: Some(status),
            };
            let compatible = raw == libc::SIGKILL;
            let no_delivery = ChildStop {
                delivered: false,
                status: Some(status),
            };
            assert_eq!(no_delivery.independent_status(false), Some(status));
            assert_eq!(
                stop.independent_status(false),
                (!compatible).then_some(status)
            );
            assert_eq!(
                matches!(
                    stop.ending(Some(StopCause::Failed)),
                    ChildEnd::Stopped(Some(StopCause::Failed))
                ),
                compatible
            );
        }
    }

    #[cfg(windows)]
    #[test]
    fn only_the_windows_termination_code_is_ambiguous() {
        use std::os::windows::process::ExitStatusExt;
        for code in [0, 1, 42, WINDOWS_KILL_CODE, 0xc0000005] {
            let status = ExitStatus::from_raw(code);
            let no_delivery = ChildStop {
                delivered: false,
                status: Some(status),
            };
            assert_eq!(
                matches!(
                    no_delivery.tree_ending(StopCause::Timeout, StopOutcome::Cleared),
                    ChildEnd::Stopped(Some(StopCause::Timeout))
                ),
                code == WINDOWS_KILL_CODE
            );
            assert_eq!(no_delivery.independent_status(false), Some(status));
            assert_eq!(
                no_delivery.independent_status(true),
                (code != WINDOWS_KILL_CODE).then_some(status)
            );
            let stop = ChildStop {
                delivered: true,
                status: Some(status),
            };
            assert_eq!(
                stop.independent_status(false),
                (code != WINDOWS_KILL_CODE).then_some(status)
            );
            assert_eq!(
                matches!(
                    stop.ending(Some(StopCause::Failed)),
                    ChildEnd::Stopped(Some(StopCause::Failed))
                ),
                code == WINDOWS_KILL_CODE
            );
        }
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn a_child_only_windows_stop_uses_the_same_private_marker() {
        let mut command = Command::new("ping.exe");
        command
            .args(["-n", "30", "127.0.0.1"])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true);
        let mut child = command.spawn().unwrap();
        let stop = ChildStop::kill(&mut child).await;
        assert!(stop.delivered);
        assert_eq!(stop.status.unwrap().code(), Some(WINDOWS_KILL_CODE as i32));
        assert!(stop.independent_status(false).is_none());
    }

    #[cfg(unix)]
    fn guarded_sleep() -> (Child, StopTrace) {
        let mut command = Command::new("sh");
        command.arg("-c").arg("exec sleep 5");
        let child = spawn(&mut command).unwrap();
        let trace = StopTrace::new(
            crate::logging::LogConfig::new(2, false, crate::logging::ColorMode::Never),
            "job-1",
        );
        (child, trace)
    }

    /// The headline invariant: a stop reports itself exactly once, and the reason
    /// says which kind of stop it was.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_stop_reports_once_and_the_fail_safe_names_itself() {
        let (mut child, trace) = guarded_sleep();
        let mut guard = Guard::attach(&mut child, TreeObserver::Direct, trace.clone())
            .await
            .unwrap();
        let end = guard.stop(StopCause::Timeout).await;
        assert!(matches!(end, ChildEnd::Stopped(Some(StopCause::Timeout))));
        assert_eq!(guard.signal(), StopOutcome::AlreadyGone);
        assert_eq!(trace.outcomes(), vec![StopOutcome::Delivered]);
        drop(guard);
        let _ = child.kill().await;
    }

    /// Dropping an armed guard is the fail-safe; it must be visible rather than a
    /// silent kill, because it means a path returned without disarming.
    #[cfg(unix)]
    #[tokio::test]
    async fn dropping_an_armed_guard_reports_the_fail_safe() {
        let (mut child, trace) = guarded_sleep();
        let guard = Guard::attach(&mut child, TreeObserver::Direct, trace.clone())
            .await
            .unwrap();
        drop(guard);
        assert_eq!(trace.lines(), ["id=job-1 end=abandoned"]);
        assert_eq!(trace.outcomes(), vec![StopOutcome::Delivered]);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn transient_refusal_retries_immediately_but_never_signals_again_on_drop() {
        let mut command = Command::new("sh");
        command.args(["-c", "sleep 15 & exit 42"]);
        let mut child = spawn(&mut command).unwrap();
        let pid = child.id().unwrap() as libc::pid_t;
        let trace = StopTrace::new(crate::logging::LogConfig::default(), "racing-stop");
        let mut guard = Guard::attach(&mut child, TreeObserver::Direct, trace.clone())
            .await
            .unwrap();
        guard.trace.refuse_first = 1;
        unix_wait::wait(pid).await.unwrap();
        let ChildEnd::Exited(status) = guard.stop(StopCause::Timeout).await else {
            panic!("a sweep of the remaining group cannot own the leader's exit");
        };
        assert_eq!(status.code(), Some(42));
        assert_eq!(guard.observation, TreeObservation::Unknown);
        assert!(!guard.kill_failed());
        drop(guard);
        assert_eq!(
            trace.outcomes(),
            [StopOutcome::Failed, StopOutcome::Delivered]
        );
        assert!(trace.lines().is_empty());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn abandonment_after_two_refusals_reports_the_alarm_without_a_third_kill() {
        let (mut child, trace) = guarded_sleep();
        let mut guard = Guard::attach(&mut child, TreeObserver::Direct, trace.clone())
            .await
            .unwrap();
        guard.trace.refuse_first = 2;
        assert_eq!(guard.signal(), StopOutcome::Failed);
        drop(guard);
        assert_eq!(trace.outcomes(), [StopOutcome::Failed; 2]);
        assert_eq!(
            trace.lines(),
            ["id=job-1 end=abandoned cleanup=kill-failed"]
        );
        child.kill().await.unwrap();
    }

    /// Why a sweep is never evidence: a group whose only member has exited but not
    /// been collected still takes the signal. The command left nothing running, yet
    /// the kernel reports delivery — so nothing a caller or operator reads may depend
    /// on it, and only a refusal survives as a fact.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn a_zombie_group_takes_the_signal_and_records_nothing() {
        let mut command = Command::new("sh");
        command.arg("-c").arg("exit 0");
        let mut child = spawn(&mut command).unwrap();
        let pid = child.id().unwrap() as libc::pid_t;
        let trace = StopTrace::new(crate::logging::LogConfig::default(), "zombie");
        let mut guard = Guard::attach(&mut child, TreeObserver::Direct, trace.clone())
            .await
            .unwrap();
        unix_wait::wait(pid).await.unwrap();
        assert!(
            unix_wait::exited(pid).unwrap(),
            "exit notification must not reap"
        );
        assert!(guard.try_wait().unwrap().unwrap().success());
        assert_eq!(
            unix_wait::exited(pid).unwrap_err().raw_os_error(),
            Some(libc::ECHILD)
        );
        assert_eq!(guard.observation, TreeObservation::None);
        assert_eq!(trace.outcomes(), vec![StopOutcome::Delivered]);
        assert!(!guard.kill_failed(), "delivery to zombies is not a refusal");
        guard.disarm();
    }

    /// The contract a caller is told: a command's process group does not outlive
    /// it. A shell that backgrounds work and returns leaves that work in the
    /// group, so sweeping the tree after the command returns must reach it, without
    /// claiming to have ended the command. A `setsid` descendant leaves the group and
    /// is out of reach; the contract is the group, not the machine.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_descendant_outliving_the_command_is_ended() {
        let temp = tempfile::tempdir().unwrap();
        let marker = temp.path().join("survived");
        let mut command = Command::new("sh");
        command
            .arg("-c")
            .arg(format!(
                "(sleep 0.4; echo leaked > '{}') & exit 0",
                marker.display()
            ))
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        let mut child = spawn(&mut command).unwrap();
        let trace = StopTrace::new(
            crate::logging::LogConfig::new(0, false, crate::logging::ColorMode::Never),
            "job-contract",
        );
        let mut tree = Guard::attach(&mut child, TreeObserver::Direct, trace.clone())
            .await
            .unwrap();
        // The command itself is done; the backgrounded writer is not.
        assert!(tree.wait().await.unwrap().success());
        assert_eq!(tree.observation, TreeObservation::Leftovers);
        assert!(!tree.kill_failed());
        tree.disarm();

        assert_eq!(trace.outcomes(), [StopOutcome::Delivered]);
        tokio::time::sleep(std::time::Duration::from_millis(700)).await;
        assert!(
            !marker.exists(),
            "a descendant that outlived the command must not run"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_later_stop_never_signals_the_reaped_group() {
        let mut command = Command::new("sh");
        command.arg("-c").arg("exit 0");
        let mut child = spawn(&mut command).unwrap();
        let trace = StopTrace::new(
            crate::logging::LogConfig::new(1, false, crate::logging::ColorMode::Never),
            "gone",
        );
        let mut guard = Guard::attach(&mut child, TreeObserver::Direct, trace.clone())
            .await
            .unwrap();
        assert!(guard.wait().await.unwrap().success());
        assert!(
            !guard.kill_failed(),
            "an exited leader alone needs no cleanup"
        );
        let swept = trace.outcomes();
        assert_eq!(guard.signal(), StopOutcome::AlreadyGone);
        assert_eq!(trace.outcomes(), swept, "no group signal after reaping");
    }

    // A stop can reach the zombie before the regular sweep. Neither Darwin's
    // refusal nor Linux's accepted signal is evidence the timeout ended it.
    #[cfg(unix)]
    #[tokio::test]
    async fn stopping_a_zombie_only_group_preserves_its_exit() {
        let mut command = Command::new("sh");
        command.args(["-c", "exit 23"]);
        let mut child = spawn(&mut command).unwrap();
        let trace = StopTrace::new(crate::logging::LogConfig::default(), "gone");
        let mut guard = Guard::attach(&mut child, TreeObserver::Direct, trace)
            .await
            .unwrap();
        unix_wait::wait(guard.group).await.unwrap();
        let ChildEnd::Exited(status) = guard.stop(StopCause::Timeout).await else {
            panic!("a zombie cannot have been stopped by the timeout");
        };
        assert_eq!(status.code(), Some(23));
        assert!(!guard.kill_failed());
    }

    /// A command that finished is disarmed first, so nothing is reported.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_disarmed_guard_reports_nothing() {
        let mut child = {
            let mut command = Command::new("sh");
            command.arg("-c").arg("exit 0");
            spawn(&mut command).unwrap()
        };
        let trace = StopTrace::new(
            crate::logging::LogConfig::new(2, false, crate::logging::ColorMode::Never),
            "job-2",
        );
        let mut guard = Guard::attach(&mut child, TreeObserver::Direct, trace.clone())
            .await
            .unwrap();
        guard.disarm();
        drop(guard);
        let _ = child.wait().await;
        assert!(trace.outcomes().is_empty(), "{:?}", trace.outcomes());
    }

    /// The whole vocabulary, in one place, as the line is actually written: how it
    /// ended, who ended it when that was a choice, and the one tree fact worth a line.
    #[cfg(unix)]
    #[test]
    fn the_end_line_names_how_it_ended_and_who_ended_it() {
        let line = |end: End, by, kill_failed| {
            end_message(
                "3f0c9d1e",
                &end.describe(),
                by,
                TreeObservation::None,
                kill_failed,
            )
        };
        assert_eq!(
            line(End::Exit(0), None, false),
            "id=3f0c9d1e end=exit code=0"
        );
        assert_eq!(
            line(End::Signal(libc::SIGKILL), Some("external"), false),
            "id=3f0c9d1e end=signal SIGKILL by=external"
        );
        assert_eq!(
            end_message("id", "exit code=0", None, TreeObservation::Leftovers, true),
            "id=id end=exit code=0 tree=leftovers cleanup=kill-failed"
        );
        assert_eq!(line(End::Timeout, None, false), "id=3f0c9d1e end=timeout");
        assert_eq!(
            line(End::Cancel, Some("caller"), false),
            "id=3f0c9d1e end=cancel by=caller"
        );
        assert_eq!(
            line(End::Exit(0), None, true),
            "id=3f0c9d1e end=exit code=0 cleanup=kill-failed"
        );
        assert_eq!(
            line(End::Abandoned, None, true),
            "id=3f0c9d1e end=abandoned cleanup=kill-failed"
        );
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn suspended_windows_command_resumes_only_after_job_assignment() {
        use std::process::Stdio;
        let mut command = Command::new("cmd.exe");
        command.arg("/C").arg("exit 0").stdout(Stdio::null());
        let mut child = spawn(&mut command).unwrap();
        let mut job = Guard::attach(
            &mut child,
            TreeObserver::Direct,
            StopTrace::new(crate::logging::LogConfig::default(), "test"),
        )
        .await
        .unwrap();
        let status = tokio::time::timeout(std::time::Duration::from_secs(10), job.wait())
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
        command
            .arg("-c")
            .arg(format!(
                "(sleep 0.6; echo leaked > '{}') & wait",
                output.display()
            ))
            .kill_on_drop(true);
        let mut child = spawn(&mut command).unwrap();
        let mut tree = Guard::attach(
            &mut child,
            TreeObserver::Direct,
            StopTrace::new(crate::logging::LogConfig::default(), "test"),
        )
        .await
        .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(120)).await;
        let _ = tree.stop(StopCause::Timeout).await;
        tokio::time::sleep(std::time::Duration::from_millis(700)).await;
        assert!(!output.exists(), "killed descendant must not run later");
    }
}
