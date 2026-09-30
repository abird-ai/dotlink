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
use tokio::sync::mpsc;

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
    #[cfg(windows)]
    input_handle: windows_sys::Win32::Foundation::HANDLE,
    #[cfg(windows)]
    original_input_mode: u32,
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

        #[cfg(windows)]
        {
            use windows_sys::Win32::{
                Foundation::INVALID_HANDLE_VALUE,
                System::Console::{GetConsoleMode, GetStdHandle, STD_INPUT_HANDLE},
            };

            // SAFETY: GetStdHandle/GetConsoleMode are process console queries.
            let input_handle = unsafe { GetStdHandle(STD_INPUT_HANDLE) };
            if input_handle.is_null() || input_handle == INVALID_HANDLE_VALUE {
                return Err(io::Error::last_os_error());
            }
            let mut original_input_mode = 0_u32;
            // SAFETY: input_handle is valid and mode points to writable storage.
            if unsafe { GetConsoleMode(input_handle, &mut original_input_mode) } == 0 {
                return Err(io::Error::last_os_error());
            }

            Ok(Self {
                was_raw,
                enabled_by_us: false,
                input_handle,
                original_input_mode,
            })
        }

        #[cfg(not(any(unix, windows)))]
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
        let mut current = self.current_termios()?;
        current.c_oflag = self.original.c_oflag;
        self.apply_termios(&current)
    }

    #[cfg(unix)]
    fn ensure_ctrl_c_signal(&self) -> io::Result<()> {
        let mut current = self.current_termios()?;
        current.c_lflag |= libc::ISIG;
        current.c_cc[libc::VINTR] = 3;
        self.apply_termios(&current)
    }

    #[cfg(windows)]
    fn ensure_ctrl_c_signal(&self) -> io::Result<()> {
        use windows_sys::Win32::System::Console::{
            ENABLE_PROCESSED_INPUT, GetConsoleMode, SetConsoleMode,
        };

        let mut mode = 0_u32;
        // SAFETY: input_handle is the live console handle captured at construction.
        if unsafe { GetConsoleMode(self.input_handle, &mut mode) } == 0 {
            return Err(io::Error::last_os_error());
        }
        mode |= ENABLE_PROCESSED_INPUT;
        // SAFETY: input_handle is valid and mode is a console input-mode bitset.
        if unsafe { SetConsoleMode(self.input_handle, mode) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    #[cfg(unix)]
    fn current_termios(&self) -> io::Result<libc::termios> {
        let mut current = std::mem::MaybeUninit::<libc::termios>::uninit();

        // SAFETY: fd remains a live terminal fd while the guard owns this snapshot.
        if unsafe { libc::tcgetattr(self.fd, current.as_mut_ptr()) } != 0 {
            return Err(io::Error::last_os_error());
        }

        // SAFETY: tcgetattr succeeded, so the struct is initialized.
        Ok(unsafe { current.assume_init() })
    }

    #[cfg(unix)]
    fn apply_termios(&self, termios: &libc::termios) -> io::Result<()> {
        // SAFETY: fd is live and termios is fully initialized.
        if unsafe { libc::tcsetattr(self.fd, libc::TCSANOW, termios) } != 0 {
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

        #[cfg(windows)]
        {
            use windows_sys::Win32::System::Console::SetConsoleMode;
            // SAFETY: input_handle is the console handle captured at construction.
            let _ = unsafe { SetConsoleMode(self.input_handle, self.original_input_mode) };
        }
    }
}

impl Drop for TerminalStateGuard {
    fn drop(&mut self) {
        self.restore();
    }
}

pub struct StdioInterruptGuard {
    _terminal: TerminalStateGuard,
}

impl StdioInterruptGuard {
    pub fn start(stdio_active: bool) -> Option<Self> {
        if !stdio_active || !io::stdin().is_terminal() {
            return None;
        }

        let terminal = TerminalStateGuard::capture().ok()?;

        #[cfg(any(unix, windows))]
        terminal.ensure_ctrl_c_signal().ok()?;

        Some(Self {
            _terminal: terminal,
        })
    }
}

pub struct RuntimeControls {
    receiver: mpsc::UnboundedReceiver<RuntimeControl>,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
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
        let thread = std::thread::spawn(move || {
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
            thread: Some(thread),
            _terminal: terminal,
        })
    }

    pub async fn recv(&mut self) -> Option<RuntimeControl> {
        self.receiver.recv().await
    }

    pub fn shutdown(mut self) {
        self.stop_and_join();
    }

    fn stop_and_join(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl Drop for RuntimeControls {
    fn drop(&mut self) {
        // Join the reader before TerminalStateGuard restores the TTY. This
        // guarantees no detached reader can consume shell input after dotlink
        // returns on an error/unwind path.
        self.stop_and_join();
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
