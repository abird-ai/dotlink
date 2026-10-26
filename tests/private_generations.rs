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
