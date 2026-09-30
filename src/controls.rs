use std::{
    io::{self, IsTerminal},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use crossterm::{
    event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers},
    terminal::{disable_raw_mode, enable_raw_mode, is_raw_mode_enabled},
};
use tokio::{sync::mpsc, task::JoinHandle};

use crate::logging::LogConfig;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RuntimeControl {
    Exit,
    Restart,
}

struct TerminalStateGuard {
    was_raw: bool,
    enabled_by_us: bool,
    #[cfg(unix)]
    fd: std::os::fd::RawFd,
    #[cfg(unix)]
    original: libc::termios,
}

impl TerminalStateGuard {
    fn capture() -> io::Result<Self> {
        let was_raw = is_raw_mode_enabled()?;

        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd;

            let stdin = io::stdin();
            let fd = stdin.as_raw_fd();
            let mut original = std::mem::MaybeUninit::<libc::termios>::uninit();

            // SAFETY: fd is the live terminal fd checked by IsTerminal before capture.
            if unsafe { libc::tcgetattr(fd, original.as_mut_ptr()) } != 0 {
                return Err(io::Error::last_os_error());
            }

            // SAFETY: tcgetattr succeeded, so the struct is fully initialized.
            let original = unsafe { original.assume_init() };

            Ok(Self {
                was_raw,
                enabled_by_us: false,
                fd,
                original,
            })
        }

        #[cfg(not(unix))]
        {
            Ok(Self {
                was_raw,
                enabled_by_us: false,
            })
        }
    }

    fn enable_runtime_mode(&mut self) -> io::Result<()> {
        if !self.was_raw {
            enable_raw_mode()?;
            self.enabled_by_us = true;
        }

        #[cfg(unix)]
        self.restore_original_output_flags()?;

        Ok(())
    }

    #[cfg(unix)]
    fn restore_original_output_flags(&self) -> io::Result<()> {
        let mut current = std::mem::MaybeUninit::<libc::termios>::uninit();

        // SAFETY: fd remains a live terminal fd while RuntimeControls owns this guard.
        if unsafe { libc::tcgetattr(self.fd, current.as_mut_ptr()) } != 0 {
            return Err(io::Error::last_os_error());
        }

        // SAFETY: tcgetattr succeeded, so the struct is initialized.
        let mut current = unsafe { current.assume_init() };
        current.c_oflag = self.original.c_oflag;

        // SAFETY: fd is live and current is an initialized termios value.
        if unsafe { libc::tcsetattr(self.fd, libc::TCSANOW, &current) } != 0 {
            return Err(io::Error::last_os_error());
        }

        Ok(())
    }

    fn restore(&mut self) {
        // Keep crossterm's process-global raw-mode bookkeeping consistent so a
        // Ctrl+R restart can enable runtime controls again.
        if self.enabled_by_us {
            let _ = disable_raw_mode();
            self.enabled_by_us = false;
        }

        #[cfg(unix)]
        {
            // Reapply the exact state captured before dotlink touched the TTY.
            // This covers all normal returns, transport errors, ? propagation,
            // unwinding, and explicit runtime restarts.
            // SAFETY: fd is the same terminal fd captured at construction and
            // original is a fully initialized termios snapshot.
            let _ = unsafe { libc::tcsetattr(self.fd, libc::TCSANOW, &self.original) };
        }
    }
}

impl Drop for TerminalStateGuard {
    fn drop(&mut self) {
        self.restore();
    }
}

pub struct RuntimeControls {
    receiver: mpsc::UnboundedReceiver<RuntimeControl>,
    stop: Arc<AtomicBool>,
    task: Option<JoinHandle<()>>,
    _terminal: TerminalStateGuard,
}

impl RuntimeControls {
    pub fn start(log: LogConfig, stdio_active: bool) -> Option<Self> {
        if stdio_active || !io::stdin().is_terminal() {
            return None;
        }

        let mut terminal = TerminalStateGuard::capture().ok()?;
        terminal.enable_runtime_mode().ok()?;

        let (sender, receiver) = mpsc::unbounded_channel();
        let stop = Arc::new(AtomicBool::new(false));
        let task_stop = stop.clone();
        let task = tokio::task::spawn_blocking(move || {
            while !task_stop.load(Ordering::Relaxed) {
                match event::poll(Duration::from_millis(100)) {
                    Ok(true) => match event::read() {
                        Ok(Event::Key(key)) if key.kind == KeyEventKind::Press => {
                            if let Some(control) = handle_key(key, &log) {
                                let _ = sender.send(control);
                            }
                        }
                        Ok(_) => {}
                        Err(_) => break,
                    },
                    Ok(false) => {}
                    Err(_) => break,
                }
            }
        });

        Some(Self {
            receiver,
            stop,
            task: Some(task),
            _terminal: terminal,
        })
    }

    pub async fn recv(&mut self) -> Option<RuntimeControl> {
        self.receiver.recv().await
    }

    pub async fn shutdown(mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(task) = self.task.take() {
            let _ = task.await;
        }
    }
}

impl Drop for RuntimeControls {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

fn handle_key(key: KeyEvent, log: &LogConfig) -> Option<RuntimeControl> {
    if key.modifiers.contains(KeyModifiers::CONTROL) {
        match key.code {
            KeyCode::Char('c' | 'C') => return Some(RuntimeControl::Exit),
            KeyCode::Char('r' | 'R') => {
                log.notice("runtime", "restart requested");
                return Some(RuntimeControl::Restart);
            }
            _ => return None,
        }
    }

    if matches!(key.code, KeyCode::Char('v' | 'V')) {
        let state = log.cycle_verbosity();
        log.notice("verbosity", state);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::logging::ColorMode;

    #[test]
    fn runtime_keys_map_without_consuming_other_input() {
        let log = LogConfig::new(0, false, ColorMode::Never);
        let ctrl_r = KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL);
        let ctrl_c = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        let other = KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE);
        assert_eq!(handle_key(ctrl_r, &log), Some(RuntimeControl::Restart));
        assert_eq!(handle_key(ctrl_c, &log), Some(RuntimeControl::Exit));
        assert_eq!(handle_key(other, &log), None);
    }

    #[test]
    fn v_cycles_runtime_verbosity() {
        let log = LogConfig::new(0, false, ColorMode::Never);
        let v = KeyEvent::new(KeyCode::Char('v'), KeyModifiers::NONE);
        assert_eq!(log.verbosity_label(), "quiet");
        assert_eq!(handle_key(v, &log), None);
        assert_eq!(log.verbosity_label(), "TOOL");
        assert_eq!(handle_key(v, &log), None);
        assert_eq!(log.verbosity_label(), "TOOL + REQ");
        assert_eq!(handle_key(v, &log), None);
        assert_eq!(log.verbosity_label(), "quiet");
    }
}
