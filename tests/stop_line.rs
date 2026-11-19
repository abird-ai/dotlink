//! The stop line is the branch's forensic record: it must appear at every
//! verbosity, because its absence is meant to mean nothing was signalled.
//!
//! Driven against the real binary with a piped stderr, since `LOG` lines are not
//! captured by the in-process tests.

use std::{
    io::{BufRead, BufReader, Write},
    path::Path,
    process::{Command, Stdio},
};

use serde_json::{Value, json};

fn config(allow_shell: bool) -> String {
    json!({
        "version": 10,
        "transports": {"openai": false, "stdio": true, "http": false,
            "http_bind": "127.0.0.1:3000", "http_ephemeral_url": false,
            "ngrok": false, "ngrok_domain": null, "ngrok_ephemeral_url": false},
        "oauth": {"enabled": false, "public_url": null},
        "permissions": {"default_allow": true, "allow_read": [], "allow_write": [],
            "allow_rw": ["."], "deny_read": [], "deny_write": [], "deny_rw": [],
            "allow_shell": allow_shell, "allow_network": false},
        "caches": [], "tunnel_id": null, "organization_id": null,
        "base_url": "https://api.openai.com",
        "max_shell_timeout_secs": 30, "max_output_bytes": 65536,
        "max_read_bytes": 65536, "max_write_bytes": 65536
    })
    .to_string()
}

/// The shell tool this binary advertises, with a command that outlives the call's
/// timeout. Windows has no `bash` tool, so naming one would answer with an
/// unknown-tool error instead of a stop.
fn timed_out_call() -> (&'static str, &'static str) {
    if cfg!(windows) {
        ("powershell", "Start-Sleep 5")
    } else {
        ("bash", "sleep 5")
    }
}

/// Run one timed-out shell call under `flags` and return the stop lines it wrote.
fn stop_lines(workdir: &Path, flags: &[&str]) -> Vec<String> {
    let state = workdir.join("state");
    std::fs::create_dir_all(&state).unwrap();
    let config_path = workdir.join("config.jsonc");
    std::fs::write(&config_path, config(true)).unwrap();

    let mut child = Command::new(env!("CARGO_BIN_EXE_dotlink"))
        .arg("--stdio")
        .args(flags)
        // No sandbox: this test is about logging, and it must not depend on
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

    let mut stdin = child.stdin.take().expect("stdin");
    let mut stdout = BufReader::new(child.stdout.take().expect("stdout"));
    let mut stderr = BufReader::new(child.stderr.take().expect("stderr"));
    let mut send = |value: Value| {
        writeln!(stdin, "{value}").unwrap();
        stdin.flush().unwrap();
    };
    let mut answer = |id: u64| -> Value {
        loop {
            let mut line = String::new();
            assert!(stdout.read_line(&mut line).unwrap() > 0, "closed stdout");
            if line.trim().is_empty() {
                continue;
            }
            let value: Value = serde_json::from_str(&line).expect("jsonrpc response");
            if value.get("id").and_then(Value::as_u64) == Some(id) {
                return value;
            }
        }
    };

    send(json!({"jsonrpc": "2.0", "id": 1, "method": "initialize",
        "params": {"protocolVersion": "2025-06-18", "capabilities": {},
                   "clientInfo": {"name": "stop-line", "version": "0"}}}));
    let _ = answer(1);
    send(json!({"jsonrpc": "2.0", "method": "notifications/initialized", "params": {}}));
    let (shell, command) = timed_out_call();
    send(
        json!({"jsonrpc": "2.0", "id": 2, "method": "tools/call", "params": {
        "name": shell, "arguments": {"command": command, "timeout_secs": 1}}}),
    );
    let response = answer(2);

    drop(stdin);
    let mut lines = String::new();
    let _ = std::io::Read::read_to_string(&mut stderr, &mut lines);
    let _ = child.wait();

    let text = response["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or_default();
    assert!(
        text.contains("\"stopped_by\":\"timeout\""),
        "the result must report the stop: {text}"
    );
    lines
        .lines()
        .filter(|line| line.contains("stop id="))
        .map(str::to_owned)
        .collect()
}

#[test]
fn the_stop_line_survives_every_verbosity() {
    for flags in [vec![], vec!["-v"], vec!["--quiet", "-vv"]] {
        let workdir = tempfile::tempdir().unwrap();
        let lines = stop_lines(workdir.path(), &flags);
        assert_eq!(lines.len(), 1, "one stop line at {flags:?}, got {lines:?}");
        let line = &lines[0];
        assert!(line.contains("reason=timeout"), "{line}");
        assert!(line.contains("outcome=sent"), "{line}");
        assert!(line.contains("stop id="), "{line}");
    }
}
