//! Supervisor-facing exit statuses: usage (2), `CONFIG` (78) only when
//! retrying unchanged cannot succeed, and `NOTCONFIGURED` (6) while setup is
//! still pending; host and network failures stay restartable (1).

use std::{net::TcpListener, path::Path, process::Command};

const EXIT_USAGE: i32 = 2;
const EXIT_CONFIG: i32 = 78;
const EXIT_NOTCONFIGURED: i32 = 6;
const SUPERVISOR_HINT: &str = "systemd leaves this service stopped";
const UNCONFIGURED_HINT: &str = "systemd stops this unit cleanly";

/// A command with no saved profile and no `DOTLINK_CONFIG_FILE`, so setup is pending.
fn unconfigured(home: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_dotlink"));
    command
        .current_dir(home)
        .env("XDG_CONFIG_HOME", home.join("config"))
        .env("XDG_STATE_HOME", home.join("state"))
        .env("XDG_DATA_HOME", home.join("data"))
        .env("XDG_CACHE_HOME", home.join("cache"))
        .env_remove("DOTLINK_CONFIG_FILE")
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
        .env("DOTLINK_CONFIG_FILE", &config)
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
fn explicit_transport_and_serialization_choices_override_invalid_environment() {
    let home = tempfile::tempdir().unwrap();
    let config = r#"{"version":12,"transports":{"openai":false,"stdio":true},"permissions":{"default_allow":true},"serialize":true}"#;
    for (variable, cli_choices) in [
        ("DOTLINK_STDIO", ["--no-stdio", "--no-tunnel"]),
        ("DOTLINK_STDIO", ["--stdio", "--no-tunnel"]),
        ("DOTLINK_SERIALIZE", ["--no-serialize", "--no-tunnel"]),
        ("DOTLINK_SERIALIZE", ["--serialize", "--no-tunnel"]),
    ] {
        // With an explicit CLI decision, the malformed lower-priority setting
        // cannot interfere with the predictable post-configuration policy failure.
        let (status, stderr) = exit(
            dotlink(home.path(), config)
                .args(["--no-default-allow"])
                .args(cli_choices)
                .env(variable, "bogus"),
        );
        assert_eq!(
            status,
            Some(EXIT_CONFIG),
            "{variable} {cli_choices:?}: {stderr}"
        );
        assert!(
            stderr.contains("no effective local capability remains"),
            "{stderr}"
        );
        assert!(!stderr.contains("must be a boolean"), "{stderr}");

        let (status, stderr) = exit(
            dotlink(home.path(), config)
                .arg("--no-default-allow")
                .arg("--no-tunnel")
                .env(variable, "bogus"),
        );
        assert_eq!(status, Some(EXIT_CONFIG), "{stderr}");
        assert!(stderr.contains(variable), "{stderr}");
        assert!(stderr.contains("must be a boolean"), "{stderr}");
    }

    // Explicitly overriding one setting must not hide errors in unrelated
    // active settings.
    let (_, stderr) = exit(
        dotlink(home.path(), config)
            .args(["--no-default-allow", "--no-stdio", "--no-tunnel"])
            .env("DOTLINK_STDIO", "bogus")
            .env("DOTLINK_SERIALIZE", "bogus"),
    );
    assert!(stderr.contains("DOTLINK_SERIALIZE"), "{stderr}");
}

#[test]
fn dormant_openai_does_not_apply_invalid_environment_url() {
    let home = tempfile::tempdir().unwrap();
    let config = r#"{"version":12,"transports":{"openai":false,"stdio":true},"permissions":{"default_allow":true}}"#;
    let (status, stderr) = exit(
        dotlink(home.path(), config)
            .args(["--no-default-allow"])
            .env("DOTLINK_BASE_URL", "not-a-url"),
    );
    assert_eq!(status, Some(EXIT_CONFIG), "{stderr}");
    assert!(
        stderr.contains("no effective local capability remains"),
        "{stderr}"
    );
    assert!(!stderr.contains("control-plane base URL"), "{stderr}");

    let active = r#"{"version":12,"transports":{"openai":true,"stdio":true},"permissions":{"default_allow":true},"tunnel_id":"tunnel_0123456789abcdef0123456789abcdef"}"#;
    // Even with a missing runtime key, a malformed active OpenAI override is
    // caught rather than ignored.
    let (_, stderr) = exit(
        dotlink(home.path(), active)
            .args(["--no-default-allow"])
            .env("DOTLINK_BASE_URL", "not-a-url"),
    );
    assert!(stderr.contains("control-plane base URL"), "{stderr}");
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
fn v12_rejects_misspelled_nested_oauth_authority() {
    let home = tempfile::tempdir().unwrap();
    let config =
        r#"{"version":12,"transports":{"openai":false,"http":true},"oauth":{"enabld":true}}"#;
    let (status, stderr) = exit(dotlink(home.path(), config).arg("--list-tools"));
    assert_ne!(status, Some(0), "{stderr}");
    assert!(stderr.contains("unknown field"), "{stderr}");
    assert!(stderr.contains("enabld"), "{stderr}");
}

#[test]
fn permission_environment_changes_tool_inspection_without_credentials() {
    let home = tempfile::tempdir().unwrap();
    let config =
        r#"{"version":12,"transports":{"openai":false},"permissions":{"default_allow":true}}"#;
    let baseline = dotlink(home.path(), config)
        .arg("--list-tools")
        .env_remove("DOTLINK_ALLOW_SHELL")
        .output()
        .unwrap();
    assert!(baseline.status.success(), "{baseline:?}");
    assert!(!String::from_utf8_lossy(&baseline.stdout).contains("• bash"));

    let env = dotlink(home.path(), config)
        .arg("--list-tools")
        .env("DOTLINK_ALLOW_SHELL", "1")
        .output()
        .unwrap();
    assert!(env.status.success(), "{env:?}");
    assert!(String::from_utf8_lossy(&env.stdout).contains("• bash"));
}

#[test]
fn explicit_color_and_verbosity_override_bad_lower_priority_env() {
    let home = tempfile::tempdir().unwrap();
    let config =
        r#"{"version":12,"transports":{"openai":false},"permissions":{"default_allow":true}}"#;
    for color in ["never", "auto"] {
        let (status, stderr) = exit(
            dotlink(home.path(), config)
                .args(["--list-tools", "--color", color, "-v"])
                .env("DOTLINK_COLOR", "bogus")
                .env("DOTLINK_VERBOSITY", "bogus"),
        );
        assert_eq!(status, Some(0), "color={color} {stderr}");
    }
    let (status, stderr) = exit(
        dotlink(home.path(), config)
            .arg("--list-tools")
            .env("DOTLINK_COLOR", "bogus"),
    );
    assert_eq!(status, Some(EXIT_CONFIG), "{stderr}");
    assert!(stderr.contains("DOTLINK_COLOR"), "{stderr}");
}

#[cfg(unix)]
#[test]
fn non_utf8_profile_selector_is_rejected_unless_cli_overrides_it() {
    use std::{ffi::OsString, os::unix::ffi::OsStringExt as _};
    let home = tempfile::tempdir().unwrap();
    let config = r#"{"version":12,"transports":{"openai":false},"tunnel_id":"tunnel_0123456789abcdef0123456789abcdef"}"#;
    let invalid = OsString::from_vec(vec![0xff, 0xfe]);
    let (status, stderr) = exit(
        dotlink(home.path(), config)
            .arg("--print-id")
            .env("DOTLINK_PROFILE", &invalid),
    );
    assert_eq!(status, Some(EXIT_CONFIG), "{stderr}");
    assert!(stderr.contains("DOTLINK_PROFILE"), "{stderr}");
    let overridden = dotlink(home.path(), config)
        .args(["--print-id", "-p", "work"])
        .env("DOTLINK_PROFILE", invalid)
        .output()
        .unwrap();
    assert!(overridden.status.success(), "{overridden:?}");
}

#[test]
fn environment_selected_named_profile_is_loaded_not_default() {
    let home = tempfile::tempdir().unwrap();
    let root = home.path().join("config/abird/dotlink");
    std::fs::create_dir_all(&root).unwrap();
    for (profile, id) in [
        ("config.jsonc", "tunnel_aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"),
        (
            "config.work.jsonc",
            "tunnel_bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
        ),
    ] {
        std::fs::write(
            root.join(profile),
            format!(r#"{{"version":12,"transports":{{"openai":false}},"tunnel_id":"{id}"}}"#),
        )
        .unwrap();
    }
    let result = unconfigured(home.path())
        .args(["--print-id"])
        .env_remove("DOTLINK_CONFIG")
        .env("DOTLINK_PROFILE", "work")
        .output()
        .unwrap();
    assert!(result.status.success(), "{result:?}");
    assert_eq!(
        String::from_utf8_lossy(&result.stdout).trim(),
        "tunnel_bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
    );
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
