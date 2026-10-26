use std::{fs, process::Command};

fn run_with_profile(
    config_dir: &std::path::Path,
    profile: &std::path::Path,
) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_dotlink"))
        .arg("--list-tools")
        .env("DOTLINK_CONFIG", profile)
        .env("XDG_CONFIG_HOME", config_dir)
        .env_remove("DOTLINK_API_KEY")
        .env_remove("CONTROL_PLANE_API_KEY")
        .output()
        .unwrap()
}

#[test]
fn persisted_key_generation_is_required_and_never_falls_back() {
    let temp = tempfile::tempdir().unwrap();
    let config_dir = temp.path().join("xdg");
    let private = config_dir.join("abird/dotlink");
    fs::create_dir_all(&private).unwrap();
    let jsonc = temp.path().join("profile.jsonc");
    let revision = "00112233445566778899aabbccddeeff";
    fs::write(
        &jsonc,
        format!(
            r#"{{"version":10,"transports":{{"openai":true,"stdio":true}},"tunnel_id":"tunnel_0123456789abcdef0123456789abcdef","runtime_key_revision":"{revision}"}}"#
        ),
    )
    .unwrap();
    fs::write(private.join("runtime.key"), "old-runtime-key\n").unwrap();
    let selected = private.join(format!("runtime.{revision}.key"));
    fs::write(&selected, "new-runtime-key\n").unwrap();

    let ready = run_with_profile(&config_dir, &jsonc);
    assert!(
        ready.status.success(),
        "{}",
        String::from_utf8_lossy(&ready.stderr)
    );

    fs::remove_file(&selected).unwrap();
    let missing = run_with_profile(&config_dir, &jsonc);
    assert!(!missing.status.success());
    assert!(String::from_utf8_lossy(&missing.stderr).contains("selected runtime key is missing"));

    // A disabled OpenAI transport does not need any runtime key; no unrelated
    // local MCP workflow should fail because a dormant key went missing.
    let dormant = fs::read_to_string(&jsonc)
        .unwrap()
        .replace("\"openai\":true", "\"openai\":false");
    fs::write(&jsonc, dormant).unwrap();
    let local_only = run_with_profile(&config_dir, &jsonc);
    assert!(
        local_only.status.success(),
        "{}",
        String::from_utf8_lossy(&local_only.stderr)
    );
}

#[test]
fn dormant_key_selector_survives_profile_cli_mutations() {
    let temp = tempfile::tempdir().unwrap();
    let config_home = temp.path().join("xdg");
    let private = config_home.join("abird/dotlink");
    fs::create_dir_all(&private).unwrap();
    let config = private.join("config.jsonc");
    let revision = "11223344556677889900aabbccddeeff";
    fs::write(
        &config,
        format!(
            r#"{{"version":10,"transports":{{"openai":false,"stdio":true}},"runtime_key_revision":"{revision}"}}"#
        ),
    )
    .unwrap();

    // A dormant OpenAI key can be missing without blocking unrelated profile
    // management; saving a different rule must never lose its selector.
    for name in ["first", "second"] {
        let path = temp.path().join(name);
        fs::create_dir(&path).unwrap();
        let output = Command::new(env!("CARGO_BIN_EXE_dotlink"))
            .args(["profile", "allow", "default", "read"])
            .arg(&path)
            .env("XDG_CONFIG_HOME", &config_home)
            .env("XDG_STATE_HOME", temp.path().join("state"))
            .env_remove("DOTLINK_CONFIG")
            .env_remove("DOTLINK_API_KEY")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    let text = fs::read_to_string(&config).unwrap();
    let json = text
        .lines()
        .filter(|line| !line.starts_with("//"))
        .collect::<Vec<_>>()
        .join("\n");
    let saved: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert_eq!(saved["runtime_key_revision"], revision);
    assert_eq!(
        saved["permissions"]["allow_read"].as_array().unwrap().len(),
        2
    );
    assert!(run_with_profile(&config_home, &config).status.success());
}

fn profile_cli(config_home: &std::path::Path, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_dotlink"))
        .arg("profile")
        .args(args)
        .env("XDG_CONFIG_HOME", config_home)
        .env_remove("DOTLINK_CONFIG")
        .env_remove("DOTLINK_API_KEY")
        .env_remove("CONTROL_PLANE_API_KEY")
        .output()
        .unwrap()
}

#[test]
fn missing_selected_key_does_not_block_profile_inspection_or_recovery() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("xdg");
    let private = home.join("abird/dotlink");
    fs::create_dir_all(&private).unwrap();
    let path = private.join("config.jsonc");
    let revision = "fedcba9876543210fedcba9876543210";
    fs::write(&path, format!(
        r#"{{"version":10,"transports":{{"openai":true,"stdio":true}},"tunnel_id":"tunnel_0123456789abcdef0123456789abcdef","runtime_key_revision":"{revision}"}}"#
    )).unwrap();

    let show = profile_cli(&home, &["show", "default"]);
    assert!(
        show.status.success(),
        "{}",
        String::from_utf8_lossy(&show.stderr)
    );
    assert!(String::from_utf8_lossy(&show.stdout).contains("missing"));
    let execution = run_with_profile(&home, &path);
    assert!(!execution.status.success());
    assert!(String::from_utf8_lossy(&execution.stderr).contains("selected runtime key is missing"));
}

#[test]
fn deletion_retries_when_only_orphaned_generated_keys_remain() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("xdg");
    let private = home.join("abird/dotlink");
    fs::create_dir_all(&private).unwrap();
    let orphan = private.join("runtime.00112233445566778899aabbccddeeff.key");
    let unrelated = private.join("runtime.other.11223344556677889900aabbccddeeff.key");
    fs::write(&orphan, "fixture-secret\n").unwrap();
    fs::write(&unrelated, "unrelated-profile-fixture\n").unwrap();

    let deleted = profile_cli(&home, &["delete", "default"]);
    assert!(
        deleted.status.success(),
        "{}",
        String::from_utf8_lossy(&deleted.stderr)
    );
    assert!(!orphan.exists());
    assert!(unrelated.exists());
}

#[test]
fn unrelated_profile_edits_do_not_retire_running_process_credentials() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("xdg");
    let private = home.join("abird/dotlink");
    fs::create_dir_all(&private).unwrap();
    let selected = "00112233445566778899aabbccddeeff";
    let stale = "11223344556677889900aabbccddeeff";
    let path = private.join("config.jsonc");
    fs::write(&path, format!(
        r#"{{"version":10,"transports":{{"openai":false,"stdio":true}},"runtime_key_revision":"{selected}"}}"#
    )).unwrap();
    let active = private.join(format!("runtime.{selected}.key"));
    let outdated = private.join(format!("runtime.{stale}.key"));
    fs::write(&active, "active-key-fixture\n").unwrap();
    fs::write(&outdated, "retired-key-fixture\n").unwrap();
    let extra = temp.path().join("source");
    fs::create_dir(&extra).unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_dotlink"))
        .args(["profile", "allow", "default", "read"])
        .arg(&extra)
        .env("XDG_CONFIG_HOME", &home)
        .env_remove("DOTLINK_CONFIG")
        .env_remove("DOTLINK_API_KEY")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(active.exists());
    assert!(
        outdated.exists(),
        "an unrelated edit must not prune a possibly active prior generation"
    );
}

#[cfg(target_os = "linux")]
#[test]
fn writable_sandbox_parent_prepares_both_private_xdg_directories() {
    use std::os::unix::fs::PermissionsExt;
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let config_home = root.join("cfg");
    let state_home = root.join("state");
    let custom_config = root.join("custom.jsonc");
    fs::write(&custom_config, format!(
        r#"{{"version":10,"transports":{{"openai":false,"stdio":true}},"permissions":{{"allow_rw":["{}"],"allow_shell":true}}}}"#,
        root.display()
    )).unwrap();

    let result = Command::new(env!("CARGO_BIN_EXE_dotlink"))
        .arg("--list-tools")
        .env("DOTLINK_CONFIG", &custom_config)
        .env("XDG_CONFIG_HOME", &config_home)
        .env("XDG_STATE_HOME", &state_home)
        .env("HOME", root)
        .env_remove("DOTLINK_API_KEY")
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    for private in [
        config_home.join("abird/dotlink"),
        state_home.join("abird/dotlink"),
    ] {
        let metadata = fs::metadata(&private).unwrap();
        assert!(metadata.is_dir());
        assert_eq!(metadata.permissions().mode() & 0o777, 0o700);
    }
}
