//! A job's lifecycle line says how it ended and who ended it, in one line and two
//! words an operator does not have to look up: `end_src=dotlink|caller|connector` is
//! ours, `end_src=outside` is not, and `end_src=command` means the command ended
//! itself. Exactly one line appears per job, so its absence is still evidence.
//!
//! Driven against the real binary with a piped stderr, since `LOG` lines are not
//! captured by the in-process tests. Signals are a Unix concept here; on Windows
//! `exit_code` already carries everything the platform reports.

#![cfg(unix)]

use std::{
    io::{BufRead, BufReader, Write},
    path::Path,
    process::{Child, Command, Stdio},
};

use serde_json::{Value, json};

fn config() -> String {
    json!({
        "version": 10,
        "transports": {"openai": false, "stdio": true, "http": false,
            "http_bind": "127.0.0.1:3000", "http_ephemeral_url": false,
            "ngrok": false, "ngrok_domain": null, "ngrok_ephemeral_url": false},
        "oauth": {"enabled": false, "public_url": null},
        "permissions": {"default_allow": true, "allow_read": [], "allow_write": [],
            "allow_rw": ["."], "deny_read": [], "deny_write": [], "deny_rw": [],
            "allow_shell": true, "allow_network": false},
        "caches": [], "tunnel_id": null, "organization_id": null,
        "base_url": "https://api.openai.com",
        "max_shell_timeout_secs": 60, "max_output_bytes": 65536,
        "max_read_bytes": 65536, "max_write_bytes": 65536
    })
    .to_string()
}

struct Session {
    child: Child,
    stdin: std::process::ChildStdin,
    stdout: BufReader<std::process::ChildStdout>,
    stderr: BufReader<std::process::ChildStderr>,
}

impl Session {
    fn start(workdir: &Path, flags: &[&str]) -> Self {
        let state = workdir.join("state");
        std::fs::create_dir_all(&state).unwrap();
        let config_path = workdir.join("config.jsonc");
        std::fs::write(&config_path, config()).unwrap();
        let mut child = Command::new(env!("CARGO_BIN_EXE_dotlink"))
            .arg("--stdio")
            .args(flags)
            // No sandbox: this test is about the record, and it must not depend on
            // bubblewrap being installed in the test environment.
            .arg("--unrestricted")
            .current_dir(workdir)
            .env("DOTLINK_CONFIG_FILE", &config_path)
            .env("XDG_CONFIG_HOME", workdir.join("cfg"))
            .env("XDG_STATE_HOME", &state)
            .env("XDG_DATA_HOME", workdir.join("data"))
            .env("XDG_CACHE_HOME", workdir.join("cache"))
            .env_remove("DOTLINK_VERBOSITY")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn dotlink --stdio");
        let stdin = child.stdin.take().expect("stdin");
        let stdout = BufReader::new(child.stdout.take().expect("stdout"));
        let stderr = BufReader::new(child.stderr.take().expect("stderr"));
        let mut session = Self {
            child,
            stdin,
            stdout,
            stderr,
        };
        session.send(json!({"jsonrpc": "2.0", "id": 1, "method": "initialize",
            "params": {"protocolVersion": "2025-06-18", "capabilities": {},
                       "clientInfo": {"name": "exit-line", "version": "0"}}}));
        let _ = session.answer(1);
        session
            .send(json!({"jsonrpc": "2.0", "method": "notifications/initialized", "params": {}}));
        session
    }

    fn send(&mut self, value: Value) {
        writeln!(self.stdin, "{value}").unwrap();
        self.stdin.flush().unwrap();
    }

    fn answer(&mut self, id: u64) -> Value {
        loop {
            let mut line = String::new();
            assert!(
                self.stdout.read_line(&mut line).unwrap() > 0,
                "closed stdout"
            );
            if line.trim().is_empty() {
                continue;
            }
            let value: Value = serde_json::from_str(&line).expect("jsonrpc response");
            if value.get("id").and_then(Value::as_u64) == Some(id) {
                return value;
            }
        }
    }

    fn shell_call(&mut self, command: &str, timeout_secs: u64) {
        self.send(
            json!({"jsonrpc": "2.0", "id": 2, "method": "tools/call", "params": {
            "name": "bash", "arguments": {"command": command, "timeout_secs": timeout_secs}}}),
        );
    }

    /// Every lifecycle line this session wrote, in order.
    fn finish(mut self) -> Vec<String> {
        drop(self.stdin);
        let mut lines = String::new();
        let _ = std::io::Read::read_to_string(&mut self.stderr, &mut lines);
        let _ = self.child.wait();
        lines
            .lines()
            .filter(|line| line.contains("LOG") && line.contains("job") && line.contains("end="))
            .map(str::to_owned)
            .collect()
    }
}

fn result_text(response: &Value) -> &str {
    response["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or_default()
}

/// Wait for `pid` to publish itself, then SIGKILL it from outside dotlink.
fn kill_from_outside(pid_file: &Path) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    let pid = loop {
        if let Some(pid) = std::fs::read_to_string(pid_file)
            .ok()
            .and_then(|text| text.trim().parse::<i32>().ok())
        {
            break pid;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the command never published its pid"
        );
        std::thread::sleep(std::time::Duration::from_millis(20));
    };
    // SAFETY: the pid came from the command we just started, and SIGKILL cannot be
    // delivered to the wrong process while that command still holds it.
    assert_eq!(unsafe { libc::kill(pid, libc::SIGKILL) }, 0, "kill {pid}");
}

#[test]
fn an_externally_signalled_job_records_its_exit_at_every_verbosity() {
    for flags in [vec![], vec!["-v"], vec!["--quiet", "-vv"]] {
        let workdir = tempfile::tempdir().unwrap();
        let pid_file = workdir.path().join("pid");
        let mut session = Session::start(workdir.path(), &flags);
        session.shell_call(
            &format!("echo $$ > {}; exec sleep 30", pid_file.display()),
            60,
        );
        kill_from_outside(&pid_file);
        let response = session.answer(2);
        let lines = session.finish();

        let text = result_text(&response);
        assert!(text.contains("\"signal\":9"), "{text}");
        assert!(!text.contains("stopped_by"), "no stop to report: {text}");
        assert_eq!(lines.len(), 1, "one line per job at {flags:?}: {lines:?}");
        let line = &lines[0];
        assert!(line.contains("end=signal signal=SIGKILL"), "{line}");
        assert!(line.contains("end_src=outside"), "{line}");
    }
}

#[test]
fn a_stop_dotlink_performed_is_not_an_exit() {
    let workdir = tempfile::tempdir().unwrap();
    let mut session = Session::start(workdir.path(), &[]);
    session.shell_call("sleep 30", 1);
    let response = session.answer(2);
    let lines = session.finish();

    assert!(
        result_text(&response).contains("\"stopped_by\":\"timeout\""),
        "{response}"
    );
    assert_eq!(lines.len(), 1, "one line per job, got {lines:?}");
    assert!(
        lines[0].contains("end=timeout end_src=dotlink"),
        "{}",
        lines[0]
    );
    assert!(
        !lines[0].contains("end_src=outside"),
        "our own stop is not an outside ending: {}",
        lines[0]
    );
}
