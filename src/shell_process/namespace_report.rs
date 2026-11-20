//! Private datagrams between dotlink and its embedded Linux observer.
use std::{os::unix::process::ExitStatusExt, process::ExitStatus};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ShellStatus(pub ExitStatus);

impl ShellStatus {
    pub fn launcher_code(self) -> u8 {
        self.0
            .code()
            .unwrap_or_else(|| 128 + self.0.signal().expect("terminal wait status")) as u8
    }
}

impl From<ShellStatus> for [u8; 2] {
    fn from(status: ShellStatus) -> Self {
        match status.0.signal() {
            Some(signal) => [b'S', signal as u8],
            None => [b'E', status.launcher_code()],
        }
    }
}

impl TryFrom<&[u8]> for ShellStatus {
    type Error = ();
    fn try_from(bytes: &[u8]) -> Result<Self, ()> {
        Ok(Self(ExitStatus::from_raw(match bytes {
            [b'E', code] => i32::from(*code) << 8,
            [b'S', signal @ 1..=126] => i32::from(*signal),
            _ => return Err(()),
        })))
    }
}
