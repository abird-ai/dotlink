//! External process supervision, declared by the NixOS unit through
//! `DOTLINK_SUPERVISOR=systemd`.

use std::ffi::OsString;

const SUPERVISOR_ENV: &str = "DOTLINK_SUPERVISOR";
const NOTIFY_SOCKET_ENV: &str = "NOTIFY_SOCKET";

/// Captured once at startup, so no child ever inherits supervisor variables.
#[derive(Debug)]
pub struct Supervisor {
    systemd: bool,
    notify_socket: Option<OsString>,
}

impl Supervisor {
    /// Take supervisor variables out of the process environment.
    ///
    /// # Safety
    ///
    /// No other thread may exist: environment mutation is not thread-safe.
    pub unsafe fn take_from_env() -> Self {
        let supervisor = Self::new(
            std::env::var_os(SUPERVISOR_ENV),
            std::env::var_os(NOTIFY_SOCKET_ENV),
        );
        // SAFETY: the caller guarantees a single-threaded process.
        unsafe {
            std::env::remove_var(SUPERVISOR_ENV);
            std::env::remove_var(NOTIFY_SOCKET_ENV);
        }
        supervisor
    }

    fn new(supervisor: Option<OsString>, notify_socket: Option<OsString>) -> Self {
        let systemd = supervisor.is_some_and(|name| name == "systemd");
        Self {
            systemd,
            notify_socket: notify_socket.filter(|_| systemd),
        }
    }

    /// Next step once systemd will not restart the unit by itself.
    pub fn operator_hint(&self) -> Option<&'static str> {
        self.systemd.then_some(
            "systemd leaves this service stopped; fix the configuration or credentials, then restart it.",
        )
    }

    /// Setup is pending; the unit stops cleanly rather than failing.
    pub fn unconfigured_hint(&self) -> Option<&'static str> {
        self.systemd.then_some(
            "systemd stops this unit cleanly, not as a failure; run 'dotlink --setup' as the service user, then restart it.",
        )
    }

    /// After ten minutes of uptime, let systemd's crash backoff start over.
    pub fn schedule_restart_reset(&self) {
        if self.systemd {
            systemd::schedule_restart_reset(self.notify_socket.clone());
        }
    }
}

#[cfg(not(target_os = "linux"))]
mod systemd {
    /// systemd exists only on Linux.
    pub(super) fn schedule_restart_reset(_notify_socket: Option<std::ffi::OsString>) {}
}

#[cfg(target_os = "linux")]
mod systemd {
    use std::{
        ffi::{OsStr, OsString},
        io,
        os::{
            linux::net::SocketAddrExt as _,
            unix::{
                ffi::OsStrExt as _,
                net::{SocketAddr, UnixDatagram},
            },
        },
        time::Duration,
    };

    const RESTART_RESET_DELAY: Duration = Duration::from_secs(10 * 60);
    /// Ignored by systemd before 258.
    const RESTART_RESET: &[u8] = b"RESTART_RESET=1";

    pub(super) fn schedule_restart_reset(notify_socket: Option<OsString>) {
        let Some(notify_socket) = notify_socket else {
            tracing::warn!("systemd supervision without NOTIFY_SOCKET; crash backoff never resets");
            return;
        };
        tokio::spawn(async move {
            tokio::time::sleep(RESTART_RESET_DELAY).await;
            if let Err(error) = notify(&notify_socket, RESTART_RESET) {
                tracing::warn!(%error, "systemd crash-backoff reset failed");
            }
        });
    }

    /// One `sd_notify` datagram.
    fn notify(notify_socket: &OsStr, message: &[u8]) -> io::Result<()> {
        let socket = UnixDatagram::unbound()?;
        socket.set_nonblocking(true)?;
        let address = match notify_socket.as_bytes().strip_prefix(b"@") {
            Some(name) => SocketAddr::from_abstract_name(name)?,
            None => SocketAddr::from_pathname(notify_socket)?,
        };
        if socket.send_to_addr(message, &address)? != message.len() {
            return Err(io::Error::other("partial systemd notification"));
        }
        Ok(())
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn notification_uses_the_sd_notify_datagram_protocol() {
            let temp = tempfile::tempdir().unwrap();
            let path = temp.path().join("notify.sock");
            let receiver = UnixDatagram::bind(&path).unwrap();
            receiver
                .set_read_timeout(Some(Duration::from_secs(1)))
                .unwrap();

            notify(path.as_os_str(), RESTART_RESET).unwrap();
            let mut message = [0_u8; 64];
            let length = receiver.recv(&mut message).unwrap();
            assert_eq!(&message[..length], RESTART_RESET);
        }

        #[test]
        fn notification_reaches_abstract_sockets() {
            let name = format!("dotlink-notify-test-{}", std::process::id());
            let receiver =
                UnixDatagram::bind_addr(&SocketAddr::from_abstract_name(&name).unwrap()).unwrap();
            receiver
                .set_read_timeout(Some(Duration::from_secs(1)))
                .unwrap();

            notify(OsStr::new(&format!("@{name}")), RESTART_RESET).unwrap();
            let mut message = [0_u8; 64];
            let length = receiver.recv(&mut message).unwrap();
            assert_eq!(&message[..length], RESTART_RESET);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_declared_systemd_supervisor_uses_its_socket_or_hint() {
        let socket = || Some(OsString::from("/run/systemd/notify"));

        let systemd = Supervisor::new(Some("systemd".into()), socket());
        assert!(systemd.operator_hint().is_some());
        assert_eq!(systemd.notify_socket, socket());

        for undeclared in [None, Some("other".into())] {
            let leaked = Supervisor::new(undeclared, socket());
            assert!(leaked.operator_hint().is_none());
            assert!(leaked.notify_socket.is_none());
        }
    }
}
