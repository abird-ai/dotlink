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
    terminal::{disable_raw_mode, enable_raw_mode},
};
use tokio::{sync::mpsc, task::JoinHandle};

use crate::logging::LogConfig;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RuntimeControl {
    Exit,
    Restart,
}

struct RawModeGuard;

impl Drop for RawModeGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
    }
}

pub struct RuntimeControls {
    receiver: mpsc::UnboundedReceiver<RuntimeControl>,
    stop: Arc<AtomicBool>,
    task: Option<JoinHandle<()>>,
    _raw: RawModeGuard,
}

impl RuntimeControls {
    pub fn start(log: LogConfig, stdio_active: bool) -> Option<Self> {
        if stdio_active || !io::stdin().is_terminal() || enable_raw_mode().is_err() {
            return None;
        }

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
            _raw: RawModeGuard,
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
