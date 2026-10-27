//! Supervisor-facing exit statuses: usage (2), `CONFIG` (78) only when
//! retrying unchanged cannot succeed, and `NOTCONFIGURED` (6) while setup is
//! still pending; host and network failures stay restartable (1).

use std::{net::TcpListener, path::Path, process::Command};

const EXIT_USAGE: i32 = 2;
const EXIT_CONFIG: i32 = 78;
const EXIT_NOTCONFIGURED: i32 = 6;
const SUPERVISOR_HINT: &str = "systemd leaves this service stopped";
const UNCONFIGURED_HINT: &str = "systemd stops this unit cleanly";

/// A command with no saved profile and no `DOTLINK_CONFIG`, so setup is pending.
fn unconfigured(home: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_dotlink"));
    command
        .current_dir(home)
        .env("XDG_CONFIG_HOME", home.join("config"))
        .env("XDG_STATE_HOME", home.join("state"))
        .env("XDG_DATA_HOME", home.join("data"))
        .env("XDG_CACHE_HOME", home.join("cache"))
        .env_remove("DOTLINK_CONFIG")
        .env_remove("DOTLINK_SUPERVISOR")
        .env_remove("NOTIFY_SOCKET")
        .env_remove("DOTLINK_API_KEY")
        .env_remove("DOTLINK_API_KEY_FILE")
        .env_remove("DOTLINK_ID")
        .env_remove("CONTROL_PLANE_API_KEY")
        .env_remove("CONTROL_PLANE_TUNNEL_ID");
    command
}

fn dotlink(home: &Path, profile: &str) -> Command {
    let config = home.join("profile.jsonc");
    std::fs::write(&config, profile).unwrap();
    let mut command = Command::new(env!("CARGO_BIN_EXE_dotlink"));
    command
        .current_dir(home)
        .env("DOTLINK_CONFIG", &config)
        .env("XDG_CONFIG_HOME", home.join("config"))
        .env("XDG_STATE_HOME", home.join("state"))
        .env("XDG_DATA_HOME", home.join("data"))
        .env("XDG_CACHE_HOME", home.join("cache"))
        .env_remove("DOTLINK_SUPERVISOR")
        .env_remove("NOTIFY_SOCKET")
        .env_remove("DOTLINK_API_KEY")
        .env_remove("DOTLINK_API_KEY_FILE")
        .env_remove("DOTLINK_ID")
        .env_remove("CONTROL_PLANE_API_KEY")
        .env_remove("CONTROL_PLANE_TUNNEL_ID");
    command
}

fn local_http_profile(bind: &str) -> String {
    format!(
        r#"{{"version":10,"transports":{{"openai":false,"stdio":false,"http":true,"http_bind":"{bind}"}}}}"#
    )
}

/// Run to completion; return the exit status and stderr.
fn exit(command: &mut Command) -> (Option<i32>, String) {
    let output = command.output().unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    (output.status.code(), stderr)
}

#[test]
fn configuration_failures_need_the_operator() {
    let home = tempfile::tempdir().unwrap();
    let (status, stderr) = exit(&mut dotlink(
        home.path(),
        &local_http_profile("not-an-address"),
    ));
    assert_eq!(status, Some(EXIT_CONFIG), "{stderr}");
    assert!(stderr.contains("invalid HTTP bind address"));
    assert!(!stderr.contains(SUPERVISOR_HINT));
}

#[test]
fn supervised_configuration_failures_explain_the_stopped_service() {
    let home = tempfile::tempdir().unwrap();
    let (status, stderr) = exit(
        dotlink(home.path(), &local_http_profile("not-an-address"))
            .env("DOTLINK_SUPERVISOR", "systemd"),
    );
    assert_eq!(status, Some(EXIT_CONFIG), "{stderr}");
    assert!(stderr.contains(SUPERVISOR_HINT));
}

#[test]
fn unavailable_listeners_stay_restartable() {
    let home = tempfile::tempdir().unwrap();
    let occupied = TcpListener::bind("127.0.0.1:0").unwrap();
    let bind = occupied.local_addr().unwrap().to_string();
    let (status, stderr) =
        exit(dotlink(home.path(), &local_http_profile(&bind)).env("DOTLINK_SUPERVISOR", "systemd"));
    assert_eq!(status, Some(1), "{stderr}");
    assert!(stderr.contains("failed to bind HTTP MCP server"));
    assert!(!stderr.contains(SUPERVISOR_HINT));
}

#[test]
fn one_shot_failures_are_ordinary_command_failures() {
    let home = tempfile::tempdir().unwrap();
    let (status, stderr) =
        exit(dotlink(home.path(), &local_http_profile("not-an-address")).arg("--print-id"));
    assert_eq!(status, Some(1), "{stderr}");
    assert!(stderr.contains("OpenAI transport is not configured"));
}

#[test]
fn unconfigured_services_stop_cleanly() {
    let home = tempfile::tempdir().unwrap();
    let (status, stderr) = exit(unconfigured(home.path()).env("DOTLINK_SUPERVISOR", "systemd"));
    assert_eq!(status, Some(EXIT_NOTCONFIGURED), "{stderr}");
    assert!(stderr.contains("not configured yet"), "{stderr}");
    assert!(stderr.contains(UNCONFIGURED_HINT), "{stderr}");
    assert!(!stderr.contains(SUPERVISOR_HINT), "{stderr}");
    assert!(!stderr.contains("Error:"), "{stderr}");
}

#[test]
fn unconfigured_commands_report_without_the_supervisor_hint() {
    let home = tempfile::tempdir().unwrap();
    let (status, stderr) = exit(&mut unconfigured(home.path()));
    assert_eq!(status, Some(EXIT_NOTCONFIGURED), "{stderr}");
    assert!(stderr.contains("not configured yet"), "{stderr}");
    assert!(!stderr.contains(UNCONFIGURED_HINT), "{stderr}");
}

#[test]
fn unknown_flags_are_usage_errors() {
    let home = tempfile::tempdir().unwrap();
    let (status, stderr) =
        exit(dotlink(home.path(), &local_http_profile("127.0.0.1:0")).arg("--no-such-flag"));
    assert_eq!(status, Some(EXIT_USAGE), "{stderr}");
}

#[test]
fn absent_explicit_credential_files_stay_restartable() {
    let home = tempfile::tempdir().unwrap();
    let missing = home.path().join("not-yet-provisioned");
    let (status, stderr) = exit(
        dotlink(home.path(), &local_http_profile("127.0.0.1:0"))
            .args(["--ngrok", "--allow-public-no-auth"])
            .env("NGROK_AUTHTOKEN_FILE", &missing)
            .env("DOTLINK_SUPERVISOR", "systemd"),
    );
    assert_eq!(status, Some(1), "{stderr}");
    assert!(stderr.contains("does not exist"));
    assert!(!stderr.contains(SUPERVISOR_HINT));
}
