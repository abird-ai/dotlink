//! Target glue between Tokio child handles and AW's platform-owned containment.
//!
//! Process-group / Job Object semantics, stop evidence and RAII cleanup live in
//! aw-platform. This module only adapts Tokio Command/Child types.

use std::io;
use tokio::process::{Child, Command};

pub(crate) fn configure(command: &mut Command) {
    #[cfg(windows)]
    aw_platform::configure_managed_process(command.as_std_mut());

    #[cfg(unix)]
    aw_platform::configure_child_group(command.as_std_mut());
}

pub(crate) fn attach(child: &Child) -> io::Result<aw_platform::ManagedProcessGuard> {
    let pid = child
        .id()
        .ok_or_else(|| io::Error::other("spawned shell has no process ID"))?;

    #[cfg(unix)]
    {
        aw_platform::ManagedProcessGuard::attach(pid)
    }

    #[cfg(windows)]
    {
        let process = child
            .raw_handle()
            .ok_or_else(|| io::Error::other("spawned shell has no process handle"))?;
        aw_platform::ManagedProcessGuard::attach(pid, process)
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
        let mut job = attach(&child).unwrap();
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
        let mut tree = attach(&child).unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(120)).await;
        assert!(tree.terminate().unwrap());
        let _ = child.wait().await;
        tokio::time::sleep(std::time::Duration::from_millis(700)).await;
        assert!(!output.exists(), "killed descendant must not run later");
    }
}
