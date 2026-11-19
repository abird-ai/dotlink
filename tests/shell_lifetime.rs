//! The lifetime contract, end to end: a command's process group does not
//! outlive it, and the journal records what dotlink did to the tree without
//! claiming to have ended a command that ended on its own.
//!
//! Driven against the real binary because the defects these pin were invisible
//! to in-process tests: the in-process guard tests call `terminate` themselves,
//! so they cannot see whether the run path ever does.

use std::{
    io::{BufRead, BufReader, Write},
    path::Path,
    process::{Command, Stdio},
    time::{Duration, Instant},
};

use serde_json::{Value, json};

fn config() -> String {
    json!({
        "version": 12,
        "transports": {"openai": false, "stdio": true, "http": false,
            "http_bind": "127.0.0.1:3000", "http_ephemeral_url": false,
            "ngrok": false, "ngrok_domain": null, "ngrok_ephemeral_url": false},
        "oauth": {"enabled": false, "public_url": null},
        "permissions": {"default_allow": true, "allow_read": [], "allow_write": [],
            "allow_rw": ["."], "deny_read": [], "deny_write": [], "deny_rw": [],
            "allow_shell": true, "allow_network": false},
        "caches": [], "tunnel_id": null, "organization_id": null,
        "base_url": "https://api.openai.com",
        "max_shell_timeout_secs": 30, "max_output_bytes": 65536,
        "max_read_bytes": 65536, "max_write_bytes": 65536,
        "job_retention_secs": 600, "job_spool_bytes": 16777216
    })
    .to_string()
}

/// One shell call, returning the payload, the journal lines and how long the call
/// took. `--unrestricted` because the contract is about the process group, not
/// about bubblewrap being installed.
fn one_call(workdir: &Path, command: &str) -> (Value, String, Duration) {
    std::fs::create_dir_all(workdir.join("state")).unwrap();
    let config_path = workdir.join("config.jsonc");
    std::fs::write(&config_path, config()).unwrap();

    let mut child = Command::new(env!("CARGO_BIN_EXE_dotlink"))
        .arg("--stdio")
        .arg("--unrestricted")
        .current_dir(workdir)
        .env("DOTLINK_CONFIG_FILE", &config_path)
        .env("XDG_CONFIG_HOME", workdir.join("cfg"))
        .env("XDG_STATE_HOME", workdir.join("state"))
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
                   "clientInfo": {"name": "shell-lifetime", "version": "0"}}}));
    let _ = answer(1);
    send(json!({"jsonrpc": "2.0", "method": "notifications/initialized", "params": {}}));

    let started = Instant::now();
    send(
        json!({"jsonrpc": "2.0", "id": 2, "method": "tools/call", "params": {
        "name": "bash", "arguments": {"command": command}}}),
    );
    let response = answer(2);
    let elapsed = started.elapsed();

    drop(stdin);
    let mut journal = String::new();
    let _ = std::io::Read::read_to_string(&mut stderr, &mut journal);
    let _ = child.wait();

    let text = response["result"]["content"][0]["text"]
        .as_str()
        .expect("shell payload");
    (
        serde_json::from_str(text).expect("payload json"),
        journal,
        elapsed,
    )
}

#[cfg(unix)]
#[test]
fn a_finished_command_does_not_wait_for_its_leftovers() {
    let workdir = tempfile::tempdir().unwrap();
    let (payload, journal, elapsed) =
        one_call(workdir.path(), "(sleep 1; touch marker.txt) & printf ok");

    // The command returned at once, so the call returns at once: the leftover
    // writer cannot hold the reply open, and the deadline (30 s) is not a
    // collection budget.
    assert!(
        elapsed < Duration::from_secs(3),
        "a finished command waited {elapsed:?} for its leftovers"
    );
    // The command's own status is preserved, and the fact that it left work
    // running is the verdict: the invocation did not finish.
    assert_eq!(payload["exit_code"], 0, "{payload}");
    assert_eq!(payload["stdout"], "ok", "{payload}");
    assert_eq!(payload["success"], false, "{payload}");
    let error = payload["error"].as_str().unwrap_or_default();
    assert!(error.contains("still running"), "{payload}");
    assert!(!error.contains("collecting command output"), "{payload}");
    assert!(
        payload.get("stopped_by").is_none(),
        "a sweep claims nothing: {payload}"
    );

    // It was swept with the group, and the record says so without pretending the
    // command was stopped.
    std::thread::sleep(Duration::from_millis(1500));
    assert!(
        !workdir.path().join("marker.txt").exists(),
        "the leftover writer outlived the command's group"
    );
    let lines: Vec<&str> = journal
        .lines()
        .filter(|line| line.contains("LOG") && line.contains("end="))
        .collect();
    assert_eq!(lines.len(), 1, "one line per job: {journal}");
    let line = lines[0];
    assert!(line.contains("end=exit code=0"), "{line}");
    assert!(line.contains("by=command"), "{line}");
    assert!(line.contains("tree=killed"), "{line}");
}

#[cfg(unix)]
#[test]
fn a_command_killed_by_someone_else_keeps_its_own_ending() {
    let workdir = tempfile::tempdir().unwrap();
    let (payload, journal, _) = one_call(workdir.path(), "(sleep 3) & kill -9 $$");

    // The command died on a signal dotlink did not deliver, and no cause claims
    // it: the reply names that ending, separately from the sweep's verdict.
    assert_eq!(payload["success"], false, "{payload}");
    assert_eq!(payload["signal"], 9, "{payload}");
    let error = payload["error"].as_str().unwrap_or_default();
    assert!(error.contains("still running"), "{payload}");
    assert!(payload.get("stopped_by").is_none(), "{payload}");
    let next = payload["next"].as_str().unwrap_or_default();
    assert!(next.contains("outside dotlink"), "{next}");

    // One line carries both facts, so they cannot read as a contradiction: the
    // command was killed from outside, and dotlink also killed what it left running.
    let lines: Vec<&str> = journal
        .lines()
        .filter(|line| line.contains("LOG") && line.contains("end="))
        .collect();
    assert_eq!(lines.len(), 1, "one line per job: {journal}");
    let line = lines[0];
    assert!(line.contains("end=signal signal=SIGKILL"), "{line}");
    assert!(line.contains("by=external"), "{line}");
    assert!(line.contains("tree=killed"), "{line}");
}
