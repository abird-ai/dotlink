//! Direct shell lifetime, end to end: dotlink sweeps the process group when the
//! command returns. Observed live leftovers clear `success` but preserve the primary exit status.
//!
//! Driven against the real binary because the defects these pin were invisible to
//! in-process tests: the guard tests call `terminate` themselves, so they cannot see
//! whether the run path ever does, and only the binary chooses the mode.
//!
//! The default run is direct-only. On Linux, `DOTLINK_TEST_BWRAP=1 cargo test --locked --all-features`
//! runs the leftover, finished-child and signal checks in both modes. Missing or unusable
//! Bubblewrap fails that run; the escaped-writer check stays direct-only.

#![cfg(any(unix, windows))]

use std::{
    io::{BufRead, BufReader, Read, Write},
    path::Path,
    process::{Command, Stdio},
};

#[cfg(unix)]
use std::time::{Duration, Instant};

use serde_json::{Value, json};

#[derive(Clone, Copy, Debug, PartialEq)]
enum Mode {
    Direct,
    Sandboxed,
}

#[cfg(unix)]
fn modes() -> Vec<Mode> {
    match std::env::var_os("DOTLINK_TEST_BWRAP") {
        None => vec![Mode::Direct],
        Some(_) if cfg!(target_os = "linux") => vec![Mode::Direct, Mode::Sandboxed],
        Some(_) => panic!("DOTLINK_TEST_BWRAP requires Linux"),
    }
}

fn config() -> String {
    json!({
        "version": 13,
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

struct Call {
    payload: Value,
    is_error: bool,
    /// The job's lifecycle lines, if any.
    lines: Vec<String>,
    #[cfg(unix)]
    elapsed: Duration,
}

/// Use `--unrestricted` for direct execution; otherwise let dotlink launch Bubblewrap.
#[cfg(unix)]
fn one_call(workdir: &Path, command: &str, mode: Mode) -> Call {
    one_call_with(workdir, command, mode, |_| ())
}

fn one_call_with(
    workdir: &Path,
    command: &str,
    mode: Mode,
    setup: impl FnOnce(&mut Command),
) -> Call {
    std::fs::create_dir_all(workdir.join("state")).unwrap();
    let config_path = workdir.join("config.jsonc");
    std::fs::write(&config_path, config()).unwrap();

    let mut binary = Command::new(env!("CARGO_BIN_EXE_dotlink"));
    binary.arg("--stdio");
    if mode == Mode::Direct {
        binary.arg("--unrestricted");
    }
    setup(&mut binary);
    let mut child = binary
        .current_dir(workdir)
        .env("DOTLINK_CONFIG_FILE", &config_path)
        .env("XDG_CONFIG_HOME", workdir.join("cfg"))
        .env("XDG_STATE_HOME", workdir.join("state"))
        .env("XDG_DATA_HOME", workdir.join("data"))
        .env("XDG_CACHE_HOME", workdir.join("cache"))
        .env_remove("DOTLINK_VERBOSITY")
        .env_remove("BASH_ENV")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn dotlink --stdio");

    let mut stdin = child.stdin.take().expect("stdin");
    let mut stdout = BufReader::new(child.stdout.take().expect("stdout"));
    let mut stderr = child.stderr.take().expect("stderr");
    let mut send = |value: Value| {
        writeln!(stdin, "{value}").unwrap();
        stdin.flush().unwrap();
    };
    let mut answer = |id: u64| -> Option<Value> {
        loop {
            let mut line = String::new();
            if stdout.read_line(&mut line).unwrap() == 0 {
                return None;
            }
            if line.trim().is_empty() {
                continue;
            }
            let value: Value = serde_json::from_str(&line).expect("jsonrpc response");
            if value.get("id").and_then(Value::as_u64) == Some(id) {
                return Some(value);
            }
        }
    };

    send(json!({"jsonrpc": "2.0", "id": 1, "method": "initialize",
        "params": {"protocolVersion": "2025-06-18", "capabilities": {},
                   "clientInfo": {"name": "shell-lifetime", "version": "0"}}}));
    let shell = if cfg!(windows) { "powershell" } else { "bash" };
    let initialized = answer(1);
    #[cfg(unix)]
    let started = Instant::now();
    let response = initialized.and_then(|_| {
        send(json!({"jsonrpc": "2.0", "method": "notifications/initialized", "params": {}}));
        send(
            json!({"jsonrpc": "2.0", "id": 2, "method": "tools/call", "params": {
            "name": shell, "arguments": {"command": command}}}),
        );
        answer(2)
    });
    #[cfg(unix)]
    let elapsed = started.elapsed();

    drop(stdin);
    let mut journal = String::new();
    let _ = stderr.read_to_string(&mut journal);
    let _ = child.wait();
    let Some(response) = response else {
        panic!("dotlink ({mode:?}) closed stdout without answering: {journal}");
    };

    let result = &response["result"];
    let text = result["content"][0]["text"]
        .as_str()
        .unwrap_or_else(|| panic!("shell payload: {response}"));
    let payload: Value = serde_json::from_str(text).expect("payload json");
    // A sandbox request must not pass on a direct result.
    assert_eq!(
        payload["sandboxed"],
        mode == Mode::Sandboxed,
        "{mode:?}: {payload}"
    );
    Call {
        payload,
        is_error: result["isError"].as_bool().unwrap_or(false),
        lines: journal
            .lines()
            .filter(|line| line.contains("LOG"))
            .filter_map(|line| line.split_once(" end=").map(|(_, end)| end.to_owned()))
            .collect(),
        #[cfg(unix)]
        elapsed,
    }
}

#[cfg(unix)]
#[test]
fn an_inherited_ignored_sigchld_cannot_auto_reap_the_group_leader() {
    use std::os::unix::process::CommandExt;
    let dir = tempfile::tempdir().unwrap();
    let call = one_call_with(dir.path(), "exit 42", Mode::Direct, |command| {
        // Only the disposable dotlink process inherits this disposition.
        unsafe {
            command.pre_exec(|| {
                if libc::signal(libc::SIGCHLD, libc::SIG_IGN) == libc::SIG_ERR {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
    });
    assert!(!call.is_error);
    assert_eq!(call.payload["exit_code"], 42);
    assert!(call.payload.get("tree").is_none());
    assert!(call.lines.is_empty());
}

/// Compare the entire model verdict (including future fields) and journal endings.
#[cfg(unix)]
fn semantics(call: &Call) -> Value {
    let mut payload = call.payload.as_object().unwrap().clone();
    for key in ["sandboxed", "network", "dir", "ts"] {
        payload.remove(key);
    }
    json!({"payload": payload, "is_error": call.is_error, "end": call.lines})
}

#[cfg(unix)]
fn runs(command: &str) -> Vec<(Mode, tempfile::TempDir, Call)> {
    let mut runs = Vec::new();
    for mode in modes() {
        let dir = tempfile::tempdir().unwrap();
        let call = one_call(dir.path(), command, mode);
        if let Some((_, _, direct)) = runs.first() {
            assert_eq!(
                semantics(direct),
                semantics(&call),
                "{mode:?} disagrees on {command:?}"
            );
        }
        runs.push((mode, dir, call));
    }
    runs
}

/// The marker makes the cleanup check independent of the model verdict.
#[cfg(unix)]
const LEFTOVER: &str =
    "(touch started; sleep 1; touch marker) & until [ -e started ]; do sleep 0.01; done";

#[cfg(unix)]
#[test]
fn leftovers_clear_success_without_overwriting_the_exit_or_failing_the_tool() {
    let mut swept = Vec::new();
    for (bare, code) in [("printf ok", 0), ("printf ok; false", 1)] {
        for (_, _, call) in runs(bare) {
            assert!(!call.is_error);
            assert_eq!(call.payload["success"], code == 0);
            assert_eq!(call.payload["exit_code"], code);
            assert!(call.payload.get("tree").is_none());
            assert!(call.lines.is_empty());
        }
        for (mode, dir, call) in runs(&format!("{LEFTOVER}; {bare}")) {
            let payload = &call.payload;
            assert!(
                call.elapsed < Duration::from_secs(5),
                "{mode:?}: {:?}",
                call.elapsed
            );
            assert!(!call.is_error, "{payload}");
            assert_eq!(payload["tree"], "leftovers");
            assert_eq!(payload["exit_code"], code);
            assert_eq!(payload["success"], false);
            assert_eq!(payload["stdout"], "ok");
            for absent in ["error", "stopped_by", "signal", "next"] {
                assert!(payload.get(absent).is_none(), "{mode:?}: {payload}");
            }
            assert_eq!(call.lines, [format!("exit code={code} tree=leftovers")]);
            assert!(dir.path().join("started").exists());
            swept.push(dir);
        }
    }
    std::thread::sleep(Duration::from_millis(1800));
    for dir in swept {
        assert!(
            !dir.path().join("marker").exists(),
            "leftover outlived the command"
        );
    }
}

/// Wider than libproc's first 32-PID buffer; a byte/count mix-up reports unknown.
#[cfg(unix)]
#[test]
fn a_wide_direct_process_group_reports_live_leftovers() {
    let dir = tempfile::tempdir().unwrap();
    let call = one_call(
        dir.path(),
        "for ((i=0; i<40; i++)); do sleep 15 & done; exit 23",
        Mode::Direct,
    );
    assert!(!call.is_error, "{}", call.payload);
    assert_eq!(call.payload["tree"], "leftovers", "{}", call.payload);
    assert_eq!(call.payload["exit_code"], 23);
    assert_eq!(call.lines, ["exit code=23 tree=leftovers"]);
}

#[cfg(unix)]
#[test]
fn a_finished_child_does_not_turn_success_into_failure() {
    for command in ["true & exec sleep 0.05", "sleep 0.05 & wait"] {
        for (_, _, call) in runs(command) {
            assert!(!call.is_error);
            assert_eq!(call.payload["success"], true);
            assert_eq!(call.payload["exit_code"], 0);
            assert!(call.payload.get("tree").is_none());
            assert!(call.lines.is_empty());
        }
    }
}

#[cfg(unix)]
#[test]
fn a_signal_is_not_an_explicit_exit_137() {
    let mut swept = Vec::new();
    for (command, signalled) in [("kill -9 $$", true), ("exit 137", false)] {
        for leftovers in [false, true] {
            let command = if leftovers {
                format!("{LEFTOVER}; {command}")
            } else {
                command.to_owned()
            };
            for (_, dir, call) in runs(&command) {
                let payload = &call.payload;
                assert!(!call.is_error, "{payload}");
                assert_eq!(payload["success"], false);
                assert!(payload.get("stopped_by").is_none());
                assert!(payload.get("error").is_none());
                let end = if signalled {
                    assert!(payload.get("exit_code").is_none());
                    assert_eq!(payload["signal"], "SIGKILL");
                    assert!(
                        payload["next"]
                            .as_str()
                            .unwrap()
                            .contains("outside dotlink")
                    );
                    "signal SIGKILL by=external"
                } else {
                    assert_eq!(payload["exit_code"], 137);
                    assert!(payload.get("signal").is_none());
                    assert!(payload.get("next").is_none());
                    "exit code=137"
                };
                if leftovers {
                    assert_eq!(payload["tree"], "leftovers");
                    assert_eq!(call.lines, [format!("{end} tree=leftovers")]);
                    assert!(dir.path().join("started").exists());
                    swept.push(dir);
                } else {
                    assert!(payload.get("tree").is_none());
                    if signalled {
                        assert_eq!(call.lines, [end]);
                    } else {
                        assert!(call.lines.is_empty());
                    }
                }
            }
        }
    }
    std::thread::sleep(Duration::from_millis(1800));
    for dir in swept {
        assert!(
            !dir.path().join("marker").exists(),
            "leftover outlived the command"
        );
    }
}

#[cfg(unix)]
#[test]
fn bash_uses_non_login_script_semantics() {
    let command = "if shopt -q login_shell; then exit 99; fi; printf ok; exit 7";
    for (_, _, call) in runs(command) {
        assert!(!call.is_error);
        assert_eq!(call.payload["exit_code"], 7);
        assert_eq!(call.payload["stdout"], "ok");
        assert!(call.lines.is_empty());
    }
}

#[cfg(target_os = "linux")]
#[test]
fn stacked_proc_under_root_rw_preserves_the_tree_verdict() {
    if std::env::var_os("DOTLINK_TEST_BWRAP").is_none() {
        return;
    }
    for (command, leftovers) in [
        ("printf ok".to_owned(), false),
        (format!("{LEFTOVER}; printf ok"), true),
    ] {
        // Even with a root grant, /tmp is private inside the namespace.
        let dir = tempfile::tempdir_in("/var/tmp").unwrap();
        let call = one_call_with(dir.path(), &command, Mode::Sandboxed, |binary| {
            binary.arg("--allow-rw=/");
        });
        assert!(!call.is_error);
        assert_eq!(call.payload["exit_code"], 0, "{}", call.payload);
        assert_eq!(call.payload["stdout"], "ok");
        assert_eq!(call.payload["success"], !leftovers);
        assert_eq!(
            call.payload["tree"].as_str(),
            leftovers.then_some("leftovers")
        );
        if leftovers {
            assert_eq!(call.lines, ["exit code=0 tree=leftovers"]);
        } else {
            assert!(call.lines.is_empty(), "{:?}", call.lines);
        }
    }
}

#[cfg(windows)]
#[test]
#[ignore = "unverified on Windows CI: the detached interpreter's script has no observable effect there"]
fn detached_powershell_counts_real_job_members_not_a_console_host() {
    // A quiet, bounded child: its lifetime does not depend on an inherited pipe.
    let child = r#"$p = New-Object System.Diagnostics.ProcessStartInfo
$p.FileName = (Get-Process -Id $PID).Path
$p.Arguments = '-NoLogo -NoProfile -NonInteractive -Command "Start-Sleep -Seconds 15"'
$p.UseShellExecute = $false
$p.CreateNoWindow = $true
$null = [System.Diagnostics.Process]::Start($p);"#;
    // The product detaches the interpreter itself; detaching dotlink as well only
    // changes how the harness reaches it. `[Console]::Out` needs a console the
    // detached child does not have, so prove liveness with a file: this test is
    // about job accounting, not console capture.
    for prefix in ["", child] {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("ran");
        let call = one_call_with(
            dir.path(),
            &format!(
                "{prefix} Set-Content -LiteralPath '{marker}' -Value ran; exit 0",
                marker = marker.display()
            ),
            Mode::Direct,
            |_| (),
        );
        assert!(!call.is_error);
        assert_eq!(call.payload["exit_code"], 0);
        assert!(
            marker.exists(),
            "the command must have run: {}",
            call.payload
        );
        assert_eq!(call.payload["success"], prefix.is_empty());
        if prefix.is_empty() {
            assert!(call.payload.get("tree").is_none());
            assert!(call.lines.is_empty());
        } else {
            assert_eq!(call.payload["tree"], "leftovers");
            assert_eq!(call.lines, ["exit code=0 tree=leftovers"]);
        }
    }
}

/// A `setsid` descendant leaves the group, so the direct sweep cannot reach it and it
/// holds the inherited pipe. That is out of contract — the description says so — and
/// the answer must be the collected prefix marked drain-incomplete, not an internal failure
/// claiming the execution deadline elapsed. The sandbox has no such escape: the
/// descendant ends with the PID namespace.
#[cfg(target_os = "linux")]
#[test]
fn direct_escaped_writer_costs_the_drain_window_not_the_deadline() {
    if std::process::Command::new("setsid")
        .arg("true")
        .output()
        .is_err()
    {
        return;
    }
    let workdir = tempfile::tempdir().unwrap();
    let call = one_call(
        workdir.path(),
        "setsid sh -c 'sleep 3; echo late' & sleep 0.3; echo ok",
        Mode::Direct,
    );

    // The command finished in a moment; the escaped writer must not hold the reply
    // until `timeout_secs` (30 in this profile).
    assert!(
        call.elapsed < Duration::from_secs(10),
        "the drain waited {:?} instead of its own window",
        call.elapsed
    );
    let payload = &call.payload;
    assert!(!call.is_error, "{payload}");
    assert_eq!(payload["exit_code"], 0, "{payload}");
    assert_eq!(payload["success"], true, "{payload}");
    assert!(
        payload["stdout"]
            .as_str()
            .unwrap_or_default()
            .contains("ok"),
        "the command's output must survive an escaped writer: {payload}"
    );
    assert!(payload.get("error").is_none(), "{payload}");
    // A cut stream says so rather than pretending it was complete.
    assert_eq!(payload["stdout_drain_incomplete"], true, "{payload}");
    assert!(payload.get("stdout_truncated").is_none(), "{payload}");
    assert!(call.lines.is_empty(), "{:?}", call.lines);
}
