//! Whole-command cleanup without imposing lifetime limits on successful jobs.
//! Kill the process tree on timeout, error or request cancellation; a successful
//! command may deliberately leave an independently running background process.

use std::io;
use tokio::process::{Child, Command};

#[cfg(unix)]
pub(crate) fn configure(command: &mut Command) {
    use std::os::unix::process::CommandExt as _;
    // The shell owns a new group; signals never reach dotlink's own group.
    command.as_std_mut().process_group(0);
}

#[cfg(windows)]
pub(crate) fn configure(_command: &mut Command) {}

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
