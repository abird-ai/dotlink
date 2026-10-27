//! End-to-end stdio probe: the advertised tool surface and one call per tool.
//!
//! Drives the real binary over stdin/stdout so the MCP framing, router and
//! descriptor-rooted filesystem tools are exercised together.

use std::{
    io::Write,
    path::Path,
    process::{Command, Stdio},
};

use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
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

/// A probe that can send concurrently and buffer responses that arrive out of
/// order, so a multi-request test never discards another request's answer.
struct Probe {
    child: std::process::Child,
    stdout: std::io::BufReader<std::process::ChildStdout>,
    pending: std::collections::BTreeMap<u64, Value>,
}

impl Probe {
    fn start(workdir: &Path) -> Self {
        Self::start_with_state(workdir, None)
    }

    fn start_with_state(workdir: &Path, state_namespace: Option<&str>) -> Self {
        let config_dir = workdir.join(".state");
        std::fs::create_dir_all(&config_dir).unwrap();
        let config = config_dir.join("config.jsonc");
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&config)
        {
            Ok(mut file) => file.write_all(config_json().as_bytes()).unwrap(),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => panic!("create test profile {}: {error}", config.display()),
        }

        let mut child = Command::new(env!("CARGO_BIN_EXE_dotlink"))
            .arg("--stdio")
            .current_dir(workdir)
            .env("DOTLINK_CONFIG", &config)
            .env("XDG_CONFIG_HOME", config_dir.join("cfg"))
            .env(
                "XDG_STATE_HOME",
                config_dir.join(state_namespace.unwrap_or("state")),
            )
            .env("XDG_DATA_HOME", config_dir.join("data"))
            .env("XDG_CACHE_HOME", config_dir.join("cache"))
            .env_remove("DOTLINK_API_KEY")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn dotlink --stdio");
        let stdout = std::io::BufReader::new(child.stdout.take().expect("stdout"));
        Self {
            child,
            stdout,
            pending: std::collections::BTreeMap::new(),
        }
    }

    fn send(&mut self, request: &Value) {
        let stdin = self.child.stdin.as_mut().expect("stdin");
        writeln!(stdin, "{request}").unwrap();
        stdin.flush().unwrap();
    }

    fn response(&mut self, id: &Value) -> Value {
        let key = id.as_u64().expect("numeric request id");
        if let Some(response) = self.pending.remove(&key) {
            return response;
        }
        loop {
            let mut line = String::new();
            let read = std::io::BufRead::read_line(&mut self.stdout, &mut line).unwrap();
            assert!(read > 0, "dotlink closed stdout before answering {id}");
            if line.trim().is_empty() {
                continue;
            }
            let response: Value = serde_json::from_str(&line).expect("jsonrpc response");
            match response.get("id").and_then(Value::as_u64) {
                Some(answered) if answered == key => return response,
                Some(answered) => {
                    self.pending.insert(answered, response);
                }
                None => {}
            }
        }
    }

    fn request(&mut self, request: &Value) -> Value {
        let id = request["id"].clone();
        self.send(request);
        self.response(&id)
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
fn stdio_serializes_concurrent_mutations_of_one_file() {
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

    std::fs::write(root.join("data.bin"), vec![b'a'; 64 * 1024]).unwrap();
    let ids = (0..8_u64).map(|index| index + 10).collect::<Vec<_>>();
    for (index, id) in ids.iter().enumerate() {
        let data = BASE64.encode([b'A' + index as u8]);
        probe.send(&call_request(
            *id,
            "bedit",
            json!({
                "path": "data.bin",
                "edits": [{"offset": index, "length": 1, "data": data}]
            }),
        ));
    }
    for id in &ids {
        let response = probe.response(&json!(id));
        assert_ne!(
            response["result"]["isError"],
            json!(true),
            "concurrent bedit {id} failed: {response}"
        );
    }
    probe.finish();

    let bytes = std::fs::read(root.join("data.bin")).unwrap();
    for (index, byte) in bytes.iter().enumerate().take(8) {
        assert_eq!(*byte, b'A' + index as u8);
    }
}

fn initialize_request(id: u64) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": "initialize",
        "params": {
            "protocolVersion": "2025-06-18",
            "capabilities": {},
            "clientInfo": {"name": "probe", "version": "1"}
        }
    })
}

/// Independent dotlink processes share a workspace: their mutation locks must
/// coordinate across processes, or every acknowledged edit but the last is
/// silently overwritten.
#[test]
fn independent_processes_do_not_lose_each_others_mutations() {
    cross_process_mutation_case(false);
    // The lock's identity must not depend on the user's profile/XDG state: a
    // service and an interactive process can use independent configurations.
    cross_process_mutation_case(true);
}

fn cross_process_mutation_case(independent_xdg: bool) {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap();
    // Write the profile once here, so the servers never race on the config file.
    let config_dir = root.join(".state");
    std::fs::create_dir_all(&config_dir).unwrap();
    std::fs::write(config_dir.join("config.jsonc"), config_json()).unwrap();
    std::fs::write(root.join("data.bin"), b"0123456789").unwrap();

    let workers = (0..8)
        .map(|index| {
            let root = root.clone();
            std::thread::spawn(move || {
                let state = format!("state-{index}");
                let mut probe =
                    Probe::start_with_state(&root, independent_xdg.then_some(state.as_str()));
                let initialized = probe.request(&initialize_request(1));
                assert_eq!(
                    initialized["result"]["serverInfo"]["name"],
                    json!("dotlink")
                );
                let response = probe.request(&call_request(
                    2,
                    "bedit",
                    json!({
                        "path": "data.bin",
                        // Insert one byte at the start of the file.
                        "edits": [{"offset": 0, "length": 0, "data": "WA=="}],
                    }),
                ));
                probe.finish();
                assert_ne!(
                    response["result"]["isError"],
                    json!(true),
                    "process {index} was refused: {response}"
                );
            })
        })
        .collect::<Vec<_>>();
    for worker in workers {
        worker.join().unwrap();
    }

    let bytes = std::fs::read(root.join("data.bin")).unwrap();
    let inserts = bytes.iter().filter(|byte| **byte == b'X').count();
    assert_eq!(
        inserts,
        8,
        "every acknowledged insert must survive: {:?}",
        String::from_utf8_lossy(&bytes)
    );
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
    assert_eq!(patch_receipt["mode"], json!("strict"));
    let bedit_receipt: Value = serde_json::from_str(text_of(&bedited)).unwrap();
    assert_eq!(bedit_receipt["bytes_written"], json!(2));
    let bread: Value = serde_json::from_str(text_of(&bread)).unwrap();
    // "abcd" with bytes 1..3 replaced by "XY".
    assert_eq!(bread["data"], json!("YVhZZA=="));
}

#[test]
fn text_tools_share_binary_detection_without_restricting_binary_tools() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap();
    std::fs::write(root.join("bytes.dat"), b"header\0needle\0trailer\n").unwrap();
    std::fs::write(root.join("plain.txt"), b"needle original\n").unwrap();

    let mut probe = Probe::start(&root);
    let initialized = probe.request(&initialize_request(1));
    assert_eq!(initialized["result"]["serverInfo"]["name"], "dotlink");

    for (id, name, arguments) in [
        (2, "read", json!({"path": "bytes.dat"})),
        (3, "read", json!({"path": "bytes.dat", "offset": 2})),
        (
            4,
            "edit",
            json!({
                "path": "bytes.dat",
                "edits": [{"old_text": "needle", "new_text": "changed"}]
            }),
        ),
        (
            5,
            "write",
            json!({"path": "new.dat", "content": "left\0right"}),
        ),
        (
            6,
            "edit",
            json!({
                "path": "plain.txt",
                "edits": [{"old_text": "original", "new_text": "left\0right"}]
            }),
        ),
        (
            7,
            "patch",
            json!({
                "patch": "*** Begin Patch\n*** Add File: new-patch.dat\n+left\0right\n*** End Patch\n"
            }),
        ),
    ] {
        let result = probe.request(&call_request(id, name, arguments));
        assert_eq!(result["result"]["isError"], true, "{name}: {result}");
        assert!(
            text_of(&result).contains("binary"),
            "{name} did not identify binary content: {result}"
        );
    }
    assert!(!root.join("new.dat").exists());
    assert!(!root.join("new-patch.dat").exists());
    assert_eq!(
        std::fs::read(root.join("plain.txt")).unwrap(),
        b"needle original\n"
    );

    for (id, mode) in [(8, "content"), (9, "files"), (10, "count")] {
        let result = probe.request(&call_request(
            id,
            "grep",
            json!({"path": ".", "pattern": "needle", "output_mode": mode}),
        ));
        assert_ne!(result["result"]["isError"], true, "{result}");
        assert!(
            text_of(&result).contains("plain.txt"),
            "text match missing: {result}"
        );
        assert!(
            !text_of(&result).contains("bytes.dat"),
            "binary file leaked into grep {mode}: {result}"
        );
    }
    let binary = probe.request(&call_request(
        11,
        "bread",
        json!({"path": "bytes.dat", "format": "base64"}),
    ));
    assert_ne!(binary["result"]["isError"], true, "{binary}");
    let bytes: Value = serde_json::from_str(text_of(&binary)).unwrap();
    assert_eq!(bytes["data"], BASE64.encode(b"header\0needle\0trailer\n"));

    probe.finish();
}
