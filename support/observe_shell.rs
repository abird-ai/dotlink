//! Inside Bubblewrap: wait for the actual shell, take one observation, exit.
//! PID 1 still owns reaping/namespace teardown. No signal is sent here.
#[cfg(target_os = "linux")]
#[path = "../src/shell_process/linux_observation.rs"]
mod linux_observation;
#[cfg(target_os = "linux")]
#[path = "../src/linux_procfs.rs"]
mod linux_procfs;
#[cfg(target_os = "linux")]
#[path = "../src/shell_process/namespace_report.rs"]
mod namespace_report;

#[cfg(target_os = "linux")]
mod linux {
    use super::{linux_observation, namespace_report::ShellStatus};

    use std::{
        env,
        ffi::OsStr,
        io,
        os::{
            fd::{FromRawFd, OwnedFd},
            unix::net::UnixDatagram,
        },
        process::Command,
    };

    fn fd(arg: Option<&OsStr>) -> io::Result<i32> {
        arg.and_then(OsStr::to_str)
            .and_then(|s| s.parse().ok())
            .filter(|&n| n > 2)
            .ok_or(io::ErrorKind::InvalidInput.into())
    }

    pub(super) fn run() -> io::Result<u8> {
        let mut args = env::args_os().skip(1);
        let image = fd(args.next().as_deref())?;
        let channel = fd(args.next().as_deref())?;
        if image == channel {
            return Err(io::ErrorKind::InvalidInput.into());
        }
        // SAFETY: the launcher passes these two distinct, owned descriptors. Close
        // the image and retain only a CLOEXEC duplicate of the private report socket.
        unsafe {
            drop(OwnedFd::from_raw_fd(image));
        }
        // SAFETY: channel is the inherited endpoint of the launcher's socket pair.
        let report = unsafe { UnixDatagram::from_raw_fd(channel) }.try_clone()?;
        // Don't allow the inner shell to reopen our report descriptor through /proc.
        unsafe extern "C" {
            fn prctl(option: i32, ...) -> i32;
        }
        const PR_SET_DUMPABLE: i32 = 4;
        if unsafe { prctl(PR_SET_DUMPABLE, 0usize, 0usize, 0usize, 0usize) } != 0 {
            return Err(io::Error::last_os_error());
        }
        let mut vars = Vec::new();
        loop {
            let flag = args.next().ok_or(io::ErrorKind::InvalidInput)?;
            if flag == "--" {
                break;
            }
            if flag != "--setenv" {
                return Err(io::ErrorKind::InvalidInput.into());
            }
            vars.push((
                args.next().ok_or(io::ErrorKind::InvalidInput)?,
                args.next().ok_or(io::ErrorKind::InvalidInput)?,
            ));
        }
        let shell = args.next().ok_or(io::ErrorKind::InvalidInput)?;
        let status = Command::new(shell)
            .env_clear()
            .envs(vars)
            .args(args)
            .status()?;
        // Publish the command's ending before scanning /proc. A deadline may
        // terminate the launcher at either point; neither send changes its exit code.
        let status = ShellStatus(status);
        if report.send(&<[u8; 2]>::from(status)).is_ok() {
            let tree = match linux_observation::has_live_member(None) {
                Ok(true) => b"L",
                Ok(false) => b"N",
                Err(_) => b"?",
            };
            let _ = report.send(tree);
        }
        Ok(status.launcher_code())
    }
}

fn main() -> std::process::ExitCode {
    use std::process::ExitCode;
    #[cfg(target_os = "linux")]
    let result = linux::run();
    #[cfg(not(target_os = "linux"))]
    let result: std::io::Result<u8> = Err(std::io::Error::other("requires Linux Bubblewrap"));
    match result {
        Ok(code) => ExitCode::from(code),
        Err(error) => {
            eprintln!("dotlink: could not run sandbox shell observer: {error}");
            ExitCode::from(125)
        }
    }
}
