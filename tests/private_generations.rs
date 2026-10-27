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
        .env_remove("DOTLINK_API_KEY_FILE")
        .env_remove("DOTLINK_ID")
        .env_remove("CONTROL_PLANE_TUNNEL_ID")
        .env_remove("CONTROL_PLANE_API_KEY")
        .output()
        .unwrap()
}

fn run_execution_with_profile(
    config_dir: &std::path::Path,
    profile: &std::path::Path,
) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_dotlink"))
        .arg("--no-stdio")
        .env("DOTLINK_CONFIG", profile)
        .env("XDG_CONFIG_HOME", config_dir)
        .env_remove("DOTLINK_API_KEY")
        .env_remove("DOTLINK_API_KEY_FILE")
        .env_remove("DOTLINK_ID")
        .env_remove("CONTROL_PLANE_TUNNEL_ID")
        .env_remove("CONTROL_PLANE_API_KEY")
        .output()
        .unwrap()
}

fn run_to_post_credential_failure(
    config_dir: &std::path::Path,
    profile: &std::path::Path,
) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_dotlink"))
        .arg("--no-default-allow")
        .env("DOTLINK_CONFIG", profile)
        .env("XDG_CONFIG_HOME", config_dir)
        .env_remove("DOTLINK_API_KEY")
        .env_remove("DOTLINK_API_KEY_FILE")
        .env_remove("DOTLINK_ID")
        .env_remove("CONTROL_PLANE_TUNNEL_ID")
        .env_remove("CONTROL_PLANE_API_KEY")
        .output()
        .unwrap()
}

fn assert_post_credential_policy_failure(output: &std::process::Output) {
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("no effective local capability remains"),
        "{stderr}"
    );
    assert!(
        !stderr.contains("selected runtime key is missing"),
        "persisted key was consulted instead of reaching the post-credential failure: {stderr}"
    );
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
    let missing = run_execution_with_profile(&config_dir, &jsonc);
    assert!(!missing.status.success());
    assert_eq!(missing.status.code(), Some(78));
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
        .env("XDG_STATE_HOME", config_home.join("state"))
        .env_remove("DOTLINK_CONFIG")
        .env_remove("DOTLINK_API_KEY")
        .env_remove("DOTLINK_API_KEY_FILE")
        .env_remove("DOTLINK_ID")
        .env_remove("CONTROL_PLANE_TUNNEL_ID")
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
    let execution = run_execution_with_profile(&home, &path);
    assert!(!execution.status.success());
    assert!(String::from_utf8_lossy(&execution.stderr).contains("selected runtime key is missing"));
}

#[test]
fn unusable_selected_keys_fail_execution_but_not_management_or_replacement() {
    for case in ["malformed", "oversized", "unreadable"] {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("xdg");
        let private = home.join("abird/dotlink");
        fs::create_dir_all(&private).unwrap();
        let config = private.join("config.jsonc");
        let revision = "0123456789abcdef0123456789abcdef";
        fs::write(
            &config,
            format!(
                r#"{{"version":10,"transports":{{"openai":true,"stdio":true}},"tunnel_id":"tunnel_0123456789abcdef0123456789abcdef","runtime_key_revision":"{revision}"}}"#
            ),
        )
        .unwrap();
        let selected = private.join(format!("runtime.{revision}.key"));
        match case {
            "malformed" => fs::write(&selected, [0xff]).unwrap(),
            "oversized" => fs::write(&selected, vec![b'x'; 16 * 1024 + 1]).unwrap(),
            "unreadable" => fs::create_dir(&selected).unwrap(),
            _ => unreachable!(),
        }

        let execution = run_execution_with_profile(&home, &config);
        let stderr = String::from_utf8_lossy(&execution.stderr);
        assert!(!execution.status.success(), "case={case}");
        assert!(
            stderr.contains("selected runtime key is unusable"),
            "case={case}: {stderr}"
        );

        let show = profile_cli(&home, &["show", "default"]);
        assert!(
            show.status.success(),
            "case={case}: {}",
            String::from_utf8_lossy(&show.stderr)
        );
        assert!(String::from_utf8_lossy(&show.stdout).contains("replace"));

        let allowed = temp.path().join("allowed");
        fs::create_dir(&allowed).unwrap();
        let mutate = Command::new(env!("CARGO_BIN_EXE_dotlink"))
            .args(["profile", "allow", "default", "read"])
            .arg(&allowed)
            .env("XDG_CONFIG_HOME", &home)
            .env("XDG_STATE_HOME", home.join("state"))
            .env_remove("DOTLINK_CONFIG")
            .env_remove("DOTLINK_API_KEY")
            .env_remove("DOTLINK_API_KEY_FILE")
            .output()
            .unwrap();
        assert!(
            mutate.status.success(),
            "case={case}: {}",
            String::from_utf8_lossy(&mutate.stderr)
        );
        let remove = Command::new(env!("CARGO_BIN_EXE_dotlink"))
            .args(["profile", "remove-allow", "default", "read"])
            .arg(&allowed)
            .env("XDG_CONFIG_HOME", &home)
            .env("XDG_STATE_HOME", home.join("state"))
            .env_remove("DOTLINK_CONFIG")
            .env_remove("DOTLINK_API_KEY")
            .env_remove("DOTLINK_API_KEY_FILE")
            .output()
            .unwrap();
        assert!(
            remove.status.success(),
            "case={case}: {}",
            String::from_utf8_lossy(&remove.stderr)
        );

        // Replacing the selected contents proves management preserved the
        // selector and did not silently disable OpenAI execution.
        if selected.is_dir() {
            fs::remove_dir(&selected).unwrap();
        }
        fs::write(&selected, "replacement-key\n").unwrap();
        let repaired = run_to_post_credential_failure(&home, &config);
        assert_post_credential_policy_failure(&repaired);
    }
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
fn writable_sandbox_parent_cannot_contain_control_plane_paths() {
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
    let owned = [
        config_home.join("abird/dotlink"),
        state_home.join("abird/dotlink"),
    ];
    // Inspection is pure: it must not create private state or mount targets.
    for private in &owned {
        assert!(
            !private.exists(),
            "inspection created {}",
            private.display()
        );
    }
    // A path mask follows a renamed ancestor, so exposing any control-plane
    // path through a broader writable bind must fail before Bubblewrap starts.
    // The conflict is deterministic: a supervisor must not restart it forever.
    let runtime = Command::new(env!("CARGO_BIN_EXE_dotlink"))
        .arg("--stdio")
        .env("DOTLINK_CONFIG", &custom_config)
        .env("XDG_CONFIG_HOME", &config_home)
        .env("XDG_STATE_HOME", &state_home)
        .env("HOME", root)
        .env_remove("DOTLINK_API_KEY")
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&runtime.stderr);
    assert_eq!(runtime.status.code(), Some(78), "{stderr}");
    assert!(stderr.contains("overlaps writable shell mount"), "{stderr}");
    for private in owned {
        assert!(
            !private.exists(),
            "failed startup created {}",
            private.display()
        );
    }
}

#[test]
fn inactive_openai_does_not_open_unreadable_dormant_credentials() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("xdg");
    let private = home.join("abird/dotlink");
    fs::create_dir_all(&private).unwrap();
    let config = private.join("config.jsonc");
    let revision = "99887766554433221100ffeeddccbbaa";
    fs::write(
        &config,
        format!(
            r#"{{"version":10,"transports":{{"openai":false,"stdio":true}},"runtime_key_revision":"{revision}"}}"#
        ),
    )
    .unwrap();
    // A directory is always unreadable *as a credential*, even for root.
    // This exercises the no-I/O invariant without relying on Unix chmod.
    fs::create_dir(private.join(format!("runtime.{revision}.key"))).unwrap();

    // Configuration-only inspection must remain pure.
    let result = run_with_profile(&home, &config);
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );

    // Normal startup also leaves a dormant OpenAI credential untouched. Force
    // a deterministic policy error after credential loading, before any
    // transport can contact the network.
    let dormant_runtime = run_to_post_credential_failure(&home, &config);
    assert_post_credential_policy_failure(&dormant_runtime);

    // Explicit file variables are also dormant until their transport is
    // selected. A directory is a deterministic invalid credential on every OS.
    let invalid_explicit = temp.path().join("invalid-explicit-credential");
    fs::create_dir(&invalid_explicit).unwrap();
    let dormant_explicit = Command::new(env!("CARGO_BIN_EXE_dotlink"))
        .arg("--no-default-allow")
        .env("DOTLINK_CONFIG", &config)
        .env("XDG_CONFIG_HOME", &home)
        .env("DOTLINK_API_KEY_FILE", &invalid_explicit)
        .env("NGROK_AUTHTOKEN_FILE", &invalid_explicit)
        .env_remove("DOTLINK_API_KEY")
        .env_remove("DOTLINK_ID")
        .env_remove("CONTROL_PLANE_TUNNEL_ID")
        .env_remove("CONTROL_PLANE_API_KEY")
        .output()
        .unwrap();
    assert_post_credential_policy_failure(&dormant_explicit);

    let pure_listing = Command::new(env!("CARGO_BIN_EXE_dotlink"))
        .arg("--list-tools")
        .env("DOTLINK_CONFIG", &config)
        .env("XDG_CONFIG_HOME", &home)
        .env("DOTLINK_API_KEY_FILE", &invalid_explicit)
        .env("NGROK_AUTHTOKEN_FILE", &invalid_explicit)
        .env_remove("DOTLINK_API_KEY")
        .env_remove("DOTLINK_ID")
        .output()
        .unwrap();
    assert!(
        pure_listing.status.success(),
        "{}",
        String::from_utf8_lossy(&pure_listing.stderr)
    );

    // An explicit tunnel/key pair activates the dormant OpenAI transport
    // without consulting the unreadable saved credential. The same later
    // policy error proves normal startup accepted the override.
    let activated = Command::new(env!("CARGO_BIN_EXE_dotlink"))
        .arg("--no-default-allow")
        .env("DOTLINK_CONFIG", &config)
        .env("XDG_CONFIG_HOME", &home)
        .env("DOTLINK_ID", "tunnel_0123456789abcdef0123456789abcdef")
        .env("DOTLINK_API_KEY", "synthetic-override-key")
        .env_remove("DOTLINK_API_KEY_FILE")
        .env_remove("CONTROL_PLANE_TUNNEL_ID")
        .env_remove("CONTROL_PLANE_API_KEY")
        .output()
        .unwrap();
    assert_post_credential_policy_failure(&activated);
}

#[test]
fn explicit_env_credentials_supersede_missing_persisted_runtime_keys() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("xdg");
    let private = home.join("abird/dotlink");
    fs::create_dir_all(&private).unwrap();
    let config = private.join("config.jsonc");
    let id = "tunnel_0123456789abcdef0123456789abcdef";
    let revision = "99887766554433221100ffeeddccbbaa";
    fs::write(
        &config,
        format!(
            r#"{{"version":10,"transports":{{"openai":true,"stdio":true}},"tunnel_id":"{id}","runtime_key_revision":"{revision}"}}"#
        ),
    )
    .unwrap();

    let missing = run_execution_with_profile(&home, &config);
    assert!(!missing.status.success());
    assert!(String::from_utf8_lossy(&missing.stderr).contains("selected runtime key is missing"));

    // Force normal startup to fail deterministically after credentials have
    // been resolved, but before the OpenAI transport can use the network.
    let overridden = Command::new(env!("CARGO_BIN_EXE_dotlink"))
        .arg("--no-default-allow")
        .env("DOTLINK_CONFIG", &config)
        .env("XDG_CONFIG_HOME", &home)
        .env("DOTLINK_ID", id)
        .env("DOTLINK_API_KEY", "synthetic-override-key")
        .env_remove("DOTLINK_API_KEY_FILE")
        .env_remove("CONTROL_PLANE_TUNNEL_ID")
        .env_remove("CONTROL_PLANE_API_KEY")
        .output()
        .unwrap();
    assert_post_credential_policy_failure(&overridden);

    let override_file = temp.path().join("credential");
    fs::write(&override_file, "synthetic-file-key\n").unwrap();
    let overridden_file = Command::new(env!("CARGO_BIN_EXE_dotlink"))
        .arg("--no-default-allow")
        .env("DOTLINK_CONFIG", &config)
        .env("XDG_CONFIG_HOME", &home)
        .env("DOTLINK_API_KEY_FILE", &override_file)
        .env("DOTLINK_API_KEY", "lower-priority-plaintext-key")
        .env_remove("DOTLINK_ID")
        .env_remove("CONTROL_PLANE_TUNNEL_ID")
        .env_remove("CONTROL_PLANE_API_KEY")
        .output()
        .unwrap();
    assert_post_credential_policy_failure(&overridden_file);
}

#[test]
fn print_id_inspects_config_without_requiring_saved_runtime_credentials() {
    let temp = tempfile::tempdir().unwrap();
    let config_home = temp.path().join("xdg");
    let private = config_home.join("abird/dotlink");
    fs::create_dir_all(&private).unwrap();
    let config = private.join("config.jsonc");
    let id = "tunnel_0123456789abcdef0123456789abcdef";
    fs::write(
        &config,
        format!(
            r#"{{"version":10,"transports":{{"openai":true,"stdio":true}},"tunnel_id":"{id}","runtime_key_revision":"{revision}"}}"#,
            revision = "a".repeat(32)
        ),
    )
    .unwrap();
    let inspect = Command::new(env!("CARGO_BIN_EXE_dotlink"))
        .arg("--print-id")
        .env("XDG_CONFIG_HOME", &config_home)
        .env_remove("DOTLINK_CONFIG")
        .env_remove("DOTLINK_API_KEY")
        .env_remove("DOTLINK_API_KEY_FILE")
        .env_remove("DOTLINK_ID")
        .env_remove("CONTROL_PLANE_API_KEY")
        .env_remove("CONTROL_PLANE_TUNNEL_ID")
        .output()
        .unwrap();
    assert!(
        inspect.status.success(),
        "{}",
        String::from_utf8_lossy(&inspect.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&inspect.stdout).trim(), id);
    // A malformed/missing runtime key still fails actual tool execution.
    let runtime = run_execution_with_profile(&config_home, &config);
    assert!(!runtime.status.success());
    assert!(String::from_utf8_lossy(&runtime.stderr).contains("selected runtime key is missing"));

    let env_id = "tunnel_fedcba9876543210fedcba9876543210";
    let overridden = Command::new(env!("CARGO_BIN_EXE_dotlink"))
        .arg("--print-id")
        .env("XDG_CONFIG_HOME", &config_home)
        .env("DOTLINK_ID", env_id)
        .env_remove("DOTLINK_CONFIG")
        .env_remove("DOTLINK_API_KEY")
        .output()
        .unwrap();
    assert!(overridden.status.success());
    assert_eq!(String::from_utf8_lossy(&overridden.stdout).trim(), env_id);
}

#[cfg(target_os = "linux")]
#[test]
fn tool_listing_needs_neither_runtime_keys_nor_host_agent() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let config = root.join("profile.jsonc");
    fs::write(&config, format!(
        r#"{{"version":10,"transports":{{"openai":true,"stdio":true}},"tunnel_id":"tunnel_0123456789abcdef0123456789abcdef","runtime_key_revision":"{}","permissions":{{"allow_rw":["{}"],"allow_shell":true,"allow_ssh_agent":true}}}}"#,
        "b".repeat(32), root.display()
    )).unwrap();
    let listed = Command::new(env!("CARGO_BIN_EXE_dotlink"))
        .arg("--list-tools")
        .env("DOTLINK_CONFIG", &config)
        .env("XDG_CONFIG_HOME", root.join("config"))
        .env("DOTLINK_SSH_AGENT_SOCKET", root.join("no-such-agent"))
        .env_remove("DOTLINK_API_KEY")
        .env_remove("DOTLINK_API_KEY_FILE")
        .output()
        .unwrap();
    assert!(
        listed.status.success(),
        "{}",
        String::from_utf8_lossy(&listed.stderr)
    );
    let output = String::from_utf8_lossy(&listed.stdout);
    assert!(output.contains("bash"), "{output}");
    assert!(output.contains("edit"), "{output}");
    // Execution without an override still enforces the selected key first.
    let missing_key = Command::new(env!("CARGO_BIN_EXE_dotlink"))
        .arg("--no-stdio")
        .env("DOTLINK_CONFIG", &config)
        .env("XDG_CONFIG_HOME", root.join("config"))
        .env("DOTLINK_SSH_AGENT_SOCKET", root.join("no-such-agent"))
        .env_remove("DOTLINK_API_KEY")
        .env_remove("DOTLINK_API_KEY_FILE")
        .env_remove("DOTLINK_ID")
        .env_remove("CONTROL_PLANE_TUNNEL_ID")
        .env_remove("CONTROL_PLANE_API_KEY")
        .output()
        .unwrap();
    let missing_key_stderr = String::from_utf8_lossy(&missing_key.stderr);
    assert!(!missing_key.status.success());
    assert!(
        missing_key_stderr.contains("selected runtime key is missing"),
        "{missing_key_stderr}"
    );

    // Supplying a complete credential override gets past key resolution and
    // proves runtime startup independently enforces the requested host agent.
    let missing_agent = Command::new(env!("CARGO_BIN_EXE_dotlink"))
        .arg("--no-stdio")
        .env("DOTLINK_CONFIG", &config)
        .env("XDG_CONFIG_HOME", root.join("config"))
        .env("DOTLINK_SSH_AGENT_SOCKET", root.join("no-such-agent"))
        .env("DOTLINK_ID", "tunnel_0123456789abcdef0123456789abcdef")
        .env("DOTLINK_API_KEY", "synthetic-override-key")
        .env_remove("DOTLINK_API_KEY_FILE")
        .env_remove("CONTROL_PLANE_TUNNEL_ID")
        .env_remove("CONTROL_PLANE_API_KEY")
        .output()
        .unwrap();
    let missing_agent_stderr = String::from_utf8_lossy(&missing_agent.stderr);
    assert!(!missing_agent.status.success());
    assert!(
        missing_agent_stderr
            .contains("DOTLINK_SSH_AGENT_SOCKET is not an accessible Unix-domain socket"),
        "{missing_agent_stderr}"
    );
    assert!(
        !missing_agent_stderr.contains("selected runtime key is missing"),
        "credential override did not reach host-agent enforcement: {missing_agent_stderr}"
    );
}
