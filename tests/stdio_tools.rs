//! End-to-end stdio probe: the advertised tool surface and one call per tool.
//!
//! Drives the real binary over stdin/stdout so the MCP framing, router and
//! descriptor-rooted filesystem tools are exercised together.

use std::{
    io::Write,
    path::Path,
    process::{Command, Stdio},
};

use serde_json::{Value, json};

fn config_json() -> String {
    json!({
        "version": 10,
        "transports": {
            "openai": false,
            "stdio": true,
            "http": false,
            "http_bind": "127.0.0.1:3000",
            "http_ephemeral_url": false,
            "ngrok": false,
            "ngrok_domain": null,
            "ngrok_ephemeral_url": false
        },
        "oauth": {"enabled": false, "public_url": null},
        "permissions": {
            "default_allow": true,
            "allow_read": [],
            "allow_write": [],
            "allow_rw": ["."],
            "deny_read": [],
            "deny_write": [],
            "deny_rw": [],
            "allow_shell": false,
            "allow_network": false
        },
        "caches": [],
        "tunnel_id": null,
        "organization_id": null,
        "base_url": "https://api.openai.com",
        "max_shell_timeout_secs": 30,
        "max_output_bytes": 65536,
        "max_read_bytes": 65536,
        "max_write_bytes": 65536
    })
    .to_string()
}

/// One request/response pair at a time, so ordering is deterministic even if
/// the server dispatches concurrently.
struct Probe {
    child: std::process::Child,
    stdout: std::io::BufReader<std::process::ChildStdout>,
}

impl Probe {
    fn start(workdir: &Path) -> Self {
        let config_dir = workdir.join(".state");
        std::fs::create_dir_all(&config_dir).unwrap();
        let config = config_dir.join("config.jsonc");
        std::fs::write(&config, config_json()).unwrap();

        let mut child = Command::new(env!("CARGO_BIN_EXE_dotlink"))
            .arg("--stdio")
            .current_dir(workdir)
            .env("DOTLINK_CONFIG", &config)
            .env("XDG_CONFIG_HOME", config_dir.join("cfg"))
            .env("XDG_STATE_HOME", config_dir.join("state"))
            .env("XDG_DATA_HOME", config_dir.join("data"))
            .env("XDG_CACHE_HOME", config_dir.join("cache"))
            .env_remove("DOTLINK_API_KEY")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn dotlink --stdio");
        let stdout = std::io::BufReader::new(child.stdout.take().expect("stdout"));
        Self { child, stdout }
    }

    fn request(&mut self, request: &Value) -> Value {
        let id = request["id"].clone();
        {
            let stdin = self.child.stdin.as_mut().expect("stdin");
            writeln!(stdin, "{request}").unwrap();
            stdin.flush().unwrap();
        }
        loop {
            let mut line = String::new();
            let read = std::io::BufRead::read_line(&mut self.stdout, &mut line).unwrap();
            assert!(read > 0, "dotlink closed stdout before answering {id}");
            if line.trim().is_empty() {
                continue;
            }
            let response: Value = serde_json::from_str(&line).expect("jsonrpc response");
            if response.get("id") == Some(&id) {
                return response;
            }
        }
    }

    fn finish(mut self) {
        drop(self.child.stdin.take());
        let status = self.child.wait().expect("wait for dotlink");
        assert!(status.success(), "dotlink exited: {status:?}");
    }
}

fn call_request(id: u64, name: &str, arguments: Value) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": "tools/call",
        "params": {"name": name, "arguments": arguments}
    })
}

fn text_of(response: &Value) -> &str {
    response["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or_else(|| panic!("no text content: {response}"))
}

#[test]
fn stdio_advertises_the_tool_surface_and_serves_every_tool() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap();
    let mut probe = Probe::start(&root);

    let initialized = probe.request(&json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "protocolVersion": "2025-06-18",
            "capabilities": {},
            "clientInfo": {"name": "probe", "version": "1"}
        }
    }));
    assert_eq!(
        initialized["result"]["serverInfo"]["name"],
        json!("dotlink")
    );

    let listed = probe.request(&json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"}));
    let tools = listed["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|tool| tool["name"].as_str().unwrap().to_owned())
        .collect::<Vec<_>>();
    assert_eq!(
        tools,
        [
            "bedit", "bread", "bwrite", "edit", "grep", "ls", "patch", "read", "write"
        ]
    );

    let mut call = |id: u64, name: &str, arguments: Value| {
        let response = probe.request(&call_request(id, name, arguments));
        assert_ne!(
            response["result"]["isError"],
            json!(true),
            "id {id} failed: {response}"
        );
        response
    };

    call(
        3,
        "write",
        json!({"path": "nested/a.txt", "content": "alpha line\n"}),
    );
    let listing = call(4, "ls", json!({"path": ".", "depth": 2}));
    let read = call(5, "read", json!({"path": "nested/a.txt"}));
    let grepped = call(6, "grep", json!({"pattern": "alpha", "path": "nested"}));
    let edited = call(
        7,
        "edit",
        json!({"path": "nested/a.txt", "edits": [{"old_text": "alpha", "new_text": "beta"}]}),
    );
    let patched = call(
        8,
        "patch",
        json!({"patch": "*** Begin Patch\n*** Update File: nested/a.txt\n@@\n-beta line\n+gamma line\n*** End Patch"}),
    );
    call(
        9,
        "bwrite",
        json!({"path": "nested/b.bin", "data": "YWJjZA=="}),
    );
    let bedited = call(
        10,
        "bedit",
        json!({"path": "nested/b.bin", "edits": [{"offset": 1, "length": 2, "data": "WFk="}]}),
    );
    let bread = call(
        11,
        "bread",
        json!({"path": "nested/b.bin", "format": "base64"}),
    );
    probe.finish();

    assert_eq!(
        std::fs::read_to_string(root.join("nested/a.txt")).unwrap(),
        "gamma line\n"
    );
    assert_eq!(std::fs::read(root.join("nested/b.bin")).unwrap(), b"aXYd");

    let listing: Value = serde_json::from_str(text_of(&listing)).unwrap();
    let paths = listing["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|entry| entry["path"].as_str().unwrap().to_owned())
        .collect::<Vec<_>>();
    assert!(paths.contains(&"nested/a.txt".to_owned()), "{paths:?}");
    assert_eq!(text_of(&read), "1\talpha line");
    assert!(text_of(&grepped).contains("nested/a.txt:1: alpha line"));
    let edit_receipt: Value = serde_json::from_str(text_of(&edited)).unwrap();
    assert_eq!(edit_receipt["changed_files"], json!(1));
    let patch_receipt: Value = serde_json::from_str(text_of(&patched)).unwrap();
    assert_eq!(patch_receipt["mode"], json!("atomic"));
    let bedit_receipt: Value = serde_json::from_str(text_of(&bedited)).unwrap();
    assert_eq!(bedit_receipt["bytes_written"], json!(2));
    let bread: Value = serde_json::from_str(text_of(&bread)).unwrap();
    // "abcd" with bytes 1..3 replaced by "XY".
    assert_eq!(bread["data"], json!("YVhZZA=="));
}
