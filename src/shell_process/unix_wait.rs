//! Exit readiness without reaping: the waitable leader reserves its numeric PGID.
use std::io;

pub(super) fn exited(pid: libc::pid_t) -> io::Result<bool> {
    loop {
        // WNOHANG leaves si_pid zero when no terminal status is available.
        let mut info = unsafe { std::mem::zeroed::<libc::siginfo_t>() };
        // SAFETY: our owned child, writable siginfo, no status consumed.
        if unsafe {
            libc::waitid(
                libc::P_PID,
                pid as libc::id_t,
                &mut info,
                libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
            )
        } == 0
        {
            return Ok(unsafe { info.si_pid() } == pid);
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(error);
        }
    }
}

pub(super) async fn wait(pid: libc::pid_t) -> io::Result<()> {
    #[cfg(target_os = "linux")]
    {
        use std::os::fd::{FromRawFd, OwnedFd};
        // A private readiness event: unrelated child exits never wake this waiter.
        let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0u32) };
        if fd >= 0 {
            // SAFETY: pidfd_open returned a fresh owned fd.
            let fd = unsafe { OwnedFd::from_raw_fd(fd as _) };
            let ready = tokio::io::unix::AsyncFd::with_interest(fd, tokio::io::Interest::READABLE)?;
            return wait_pidfd(pid, &ready).await;
        }
        // Older kernels and policies without pidfd_open: same non-reaping contract.
    }
    wait_sigchld(pid).await
}

#[cfg(target_os = "linux")]
async fn wait_pidfd(
    pid: libc::pid_t,
    ready: &tokio::io::unix::AsyncFd<std::os::fd::OwnedFd>,
) -> io::Result<()> {
    loop {
        let mut event = ready.readable().await?;
        // AsyncFd readiness may be spurious. Only waitid authorizes a sweep;
        // otherwise cleanup could SIGKILL the still-running leader itself.
        if exited(pid)? {
            return Ok(());
        }
        event.clear_ready();
    }
}

async fn wait_sigchld(pid: libc::pid_t) -> io::Result<()> {
    use tokio::signal::unix::{SignalKind, signal};
    // Subscribe before checking. SIGCHLD is a broadcast hint, not one event per
    // child: waitid(WNOWAIT) stays true for our PID until its owner reaps it.
    // Coalesced exits need only one wake; an unrelated wake just repeats the check.
    let mut changes = signal(SignalKind::child())?;
    while !exited(pid)? {
        changes.recv().await.ok_or(io::ErrorKind::BrokenPipe)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn a_spurious_pidfd_wake_cannot_make_a_live_child_waitable() {
        use std::{
            os::{fd::OwnedFd, unix::net::UnixDatagram},
            process::Stdio,
        };
        use tokio::io::{Interest, unix::AsyncFd};

        let mut child = crate::shell_process::spawn({
            let mut command = tokio::process::Command::new("sh");
            command
                .args(["-c", "read line; exit 42"])
                .stdin(Stdio::piped());
            command
        })
        .unwrap();
        let pid = child.id().unwrap() as libc::pid_t;
        let (rx, tx) = UnixDatagram::pair().unwrap();
        rx.set_nonblocking(true).unwrap();
        let fd: OwnedFd = rx.try_clone().unwrap().into();
        let ready = AsyncFd::with_interest(fd, Interest::READABLE).unwrap();
        tx.send(b"wake").unwrap();
        drop(ready.readable().await.unwrap());
        rx.recv(&mut [0; 4]).unwrap(); // cached readiness, no kernel readiness
        let wait = wait_pidfd(pid, &ready);
        tokio::pin!(wait);
        assert!(
            futures_util::poll!(&mut wait).is_pending(),
            "a wake is not an exit"
        );
        assert!(!exited(pid).unwrap());
        drop(child.stdin.take());
        wait_sigchld(pid).await.unwrap();
        tx.send(b"exit").unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(5), wait)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(child.try_wait().unwrap().unwrap().code(), Some(42));
    }

    /// Coalesced exits and an unrelated notification must not strand the last waiter.
    #[tokio::test]
    async fn one_burst_wakes_all_eligible_waiters_without_completing_the_rest() {
        use futures_util::{StreamExt, stream::FuturesUnordered};
        let mut children: Vec<_> = (0..8)
            .map(|_| {
                tokio::process::Command::new("sh")
                    .args(["-c", "read line; exit 42"])
                    .stdin(std::process::Stdio::piped())
                    .kill_on_drop(true)
                    .spawn()
                    .unwrap()
            })
            .collect();
        let mut waiters: FuturesUnordered<_> = children
            .iter()
            .map(|child| {
                let pid = child.id().unwrap() as libc::pid_t;
                async move {
                    wait_sigchld(pid).await.unwrap();
                    pid
                }
            })
            .collect();
        assert!(futures_util::poll!(waiters.next()).is_pending());
        let (last, burst) = children.split_last_mut().unwrap();
        for child in burst.iter_mut() {
            drop(child.stdin.take());
        }
        let last_pid = last.id().unwrap() as libc::pid_t;
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            for _ in burst {
                assert_ne!(waiters.next().await.unwrap(), last_pid);
            }
            assert!(futures_util::poll!(waiters.next()).is_pending());
            drop(last.stdin.take());
            assert_eq!(waiters.next().await.unwrap(), last_pid);
        })
        .await
        .expect("a coalesced or unrelated SIGCHLD stranded a waiter");
        for child in &mut children {
            assert_eq!(child.try_wait().unwrap().unwrap().code(), Some(42));
        }
    }
}
