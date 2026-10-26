//! Whole-command cleanup without imposing lifetime limits on successful jobs.
//! Kill the contained process group/job on timeout, error or cancellation.
//! Unsandboxed Unix children can intentionally escape their process group
//! (setsid); no process-group API can guarantee descendant containment.
//! A successful command may deliberately leave background processes running.

use std::io;
use tokio::process::{Child, Command};

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
    #[cfg(unix)]
    group: libc::pid_t,
    #[cfg(windows)]
    job: std::os::windows::io::OwnedHandle,
}

impl Guard {
    pub(crate) fn attach(child: &Child) -> io::Result<Self> {
        #[cfg(unix)]
        {
            let group = child
                .id()
                .and_then(|id| libc::pid_t::try_from(id).ok())
                .ok_or_else(|| io::Error::other("spawned shell has no process group"))?;
            Ok(Self {
                active: true,
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
            Ok(Self { active: true, job })
        }
    }

    pub(crate) fn terminate(&mut self) {
        if !self.active {
            return;
        }
        self.active = false;
        #[cfg(unix)]
        {
            // SAFETY: negative PID addresses only the shell's own process group.
            unsafe { libc::kill(-self.group, libc::SIGKILL) };
        }
        #[cfg(windows)]
        {
            use std::os::windows::io::AsRawHandle;
            use windows_sys::Win32::System::JobObjects::TerminateJobObject;
            // SAFETY: the job belongs to this invocation.
            unsafe { TerminateJobObject(self.job.as_raw_handle(), 1) };
        }
    }

    pub(crate) fn disarm(&mut self) {
        self.active = false;
    }
}

impl Drop for Guard {
    fn drop(&mut self) {
        self.terminate();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(windows)]
    #[tokio::test]
    async fn suspended_windows_command_resumes_only_after_job_assignment() {
        use std::process::Stdio;
        let mut command = Command::new("cmd.exe");
        configure(&mut command);
        command.arg("/C").arg("exit 0").stdout(Stdio::null());
        let mut child = command.spawn().unwrap();
        let mut job = Guard::attach(&child).unwrap();
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
        let mut tree = Guard::attach(&child).unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(120)).await;
        tree.terminate();
        let _ = child.wait().await;
        tokio::time::sleep(std::time::Duration::from_millis(700)).await;
        assert!(!output.exists(), "killed descendant must not run later");
    }
}
