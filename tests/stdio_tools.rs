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
fn stdio_job_read_cannot_authorize_ack_across_an_unread_prefix() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap();
    std::fs::write(root.join("data.txt"), "abcdefghijklmnop").unwrap();
    let mut probe = Probe::start(&root);
    let init = probe.request(&initialize_request(1));
    assert_eq!(init["result"]["serverInfo"]["name"], "dotlink");
    let batch = probe.request(&call_request(
        2,
        "batch",
        json!({
            "ops":[["read", {"path":"data.txt","numbered":false}]],
            "background":true
        }),
    ));
    assert_ne!(batch["result"]["isError"], true, "{batch}");
    let job_ref = text_of(&batch)
        .split_whitespace()
        .next()
        .unwrap()
        .to_owned();
    let node_ref = format!("{job_ref}/n:1");
    let terminal = probe.request(&call_request(
        3,
        "jobs_get",
        json!({"ref":job_ref,"wait_ms":1000}),
    ));
    assert!(text_of(&terminal).contains("settled"), "{terminal}");
    let late = probe.request(&call_request(
        4,
        "jobs_read",
        json!({"ref":node_ref,"view":"result","offset":8}),
    ));
    assert_ne!(late["result"]["isError"], true, "{late}");
    let late: Value = serde_json::from_str(text_of(&late)).unwrap();
    let through = late["next_offset"].as_u64().unwrap();
    assert!(through > 8, "{late}");

    let over_ack = probe.request(&call_request(
        5,
        "jobs_ack",
        json!({"ref":node_ref,"view":"result","offset":through}),
    ));
    assert_eq!(
        over_ack["result"]["isError"], true,
        "unread prefix must not be acknowledged: {over_ack}"
    );
    let zero_ack = probe.request(&call_request(
        6,
        "jobs_ack",
        json!({"ref":node_ref,"view":"result","offset":0}),
    ));
    assert_eq!(zero_ack["result"]["isError"], true, "{zero_ack}");

    let first = probe.request(&call_request(
        7,
        "jobs_read",
        json!({"ref":node_ref,"view":"result","offset":0}),
    ));
    assert_ne!(first["result"]["isError"], true, "{first}");
    let first: Value = serde_json::from_str(text_of(&first)).unwrap();
    assert_eq!(first["next_offset"], through);
    let acknowledged = probe.request(&call_request(
        8,
        "jobs_ack",
        json!({"ref":node_ref,"view":"result","offset":through}),
    ));
    assert_ne!(acknowledged["result"]["isError"], true, "{acknowledged}");
    probe.finish();
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
            "batch",
            "bedit",
            "bread",
            "bwrite",
            "cp",
            "edit",
            "find",
            "grep",
            "jobs_ack",
            "jobs_cancel",
            "jobs_get",
            "jobs_list",
            "jobs_read",
            "ls",
            "mv",
            "patch",
            "read",
            "rm",
            "write"
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
    let listing = call(4, "ls", json!({"path": "nested"}));
    let found = call(5, "find", json!({"path": ".", "depth": 2}));
    let read = call(
        6,
        "read",
        json!({"path": "nested/a.txt", "numbered": false}),
    );
    let grepped = call(7, "grep", json!({"pattern": "alpha", "path": "nested"}));
    let edited = call(
        8,
        "edit",
        json!({"path": "nested/a.txt", "edits": [["alpha", "beta"]]}),
    );
    let patched = call(
        9,
        "patch",
        json!({"patch": "*** Begin Patch\n*** Update File: nested/a.txt\n@@\n-beta line\n+gamma line\n*** End Patch"}),
    );
    call(
        10,
        "bwrite",
        json!({"path": "nested/b.bin", "data": "YWJjZA=="}),
    );
    let bedited = call(
        11,
        "bedit",
        json!({"path": "nested/b.bin", "edits": [{"offset": 1, "length": 2, "data": "WFk="}]}),
    );
    let bread = call(
        12,
        "bread",
        json!({"path": "nested/b.bin", "format": "base64"}),
    );
    let copied = call(
        13,
        "cp",
        json!({"from": "nested/a.txt", "to": "nested/copied.txt"}),
    );
    let moved = call(
        14,
        "mv",
        json!({"from": "nested/copied.txt", "to": "nested/moved.txt"}),
    );
    let removed = call(15, "rm", json!({"paths": ["nested/moved.txt"]}));

    let batch = call(
        16,
        "batch",
        json!({"ops": [["read", {"path": "nested/a.txt", "numbered": false}]]}),
    );
    assert!(
        text_of(&batch).contains("gamma line"),
        "{}",
        text_of(&batch)
    );

    let started = call(
        17,
        "batch",
        json!({
            "ops": [["read", {"path": "nested/a.txt", "numbered": false}]],
            "background": true
        }),
    );
    let job_ref = text_of(&started)
        .split_whitespace()
        .next()
        .expect("background batch returns compact Job ref")
        .to_owned();
    assert!(job_ref.starts_with("j:"), "{job_ref}");

    let terminal = loop {
        let status = call(18, "jobs_get", json!({"ref": job_ref, "wait_ms": 100}));
        let status = text_of(&status).to_owned();
        if status.contains(" settled ") || status.contains(" cancelled ") {
            break status;
        }
    };
    assert!(terminal.contains(" outcome=ok"), "{terminal}");

    let node_ref = format!("{job_ref}/n:1");
    let output = call(
        19,
        "jobs_read",
        json!({"ref": node_ref, "view": "result", "encoding": "text"}),
    );
    let output: Value = serde_json::from_str(text_of(&output)).unwrap();
    assert!(
        output["data"]
            .as_str()
            .is_some_and(|data| data.contains("gamma line")),
        "{output}"
    );
    let acknowledged = output["next_offset"].as_u64().unwrap();
    call(
        20,
        "jobs_ack",
        json!({"ref": node_ref, "view": "result", "offset": acknowledged}),
    );

    let listed_jobs = call(21, "jobs_list", json!({"limit": 64}));
    assert!(text_of(&listed_jobs).contains(&job_ref), "{listed_jobs}");
    call(22, "jobs_cancel", json!({"ref": job_ref}));

    let fresh = call(
        23,
        "batch",
        json!({
            "ops": [["read", {"path": "nested/a.txt", "numbered": false}]],
            "background": true
        }),
    );
    let fresh_ref = text_of(&fresh)
        .split_whitespace()
        .next()
        .expect("fresh batch returns compact Job ref");
    assert_ne!(fresh_ref, job_ref);
    probe.finish();

    assert_eq!(
        std::fs::read_to_string(root.join("nested/a.txt")).unwrap(),
        "gamma line\n"
    );
    assert_eq!(std::fs::read(root.join("nested/b.bin")).unwrap(), b"aXYd");
    assert!(!root.join("nested/copied.txt").exists());
    assert!(!root.join("nested/moved.txt").exists());

    assert!(text_of(&listing).contains("a.txt"), "{}", text_of(&listing));
    assert!(
        text_of(&found).contains("nested/a.txt"),
        "{}",
        text_of(&found)
    );
    assert_eq!(text_of(&read), "alpha line\n");
    assert!(text_of(&grepped).contains("nested/a.txt:1:alpha line"));
    for output in [&edited, &patched, &bedited, &copied, &moved, &removed] {
        assert_eq!(text_of(output), "ok", "{output}");
    }
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
        (
            3,
            "read",
            json!({"path": "bytes.dat", "line": 1, "byte": 2}),
        ),
        (
            4,
            "edit",
            json!({
                "path": "bytes.dat",
                "edits": [["needle", "changed"]]
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
                "edits": [["original", "left\0right"]]
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
            json!({"path": ".", "pattern": "needle", "mode": mode}),
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

#[test]
fn stdio_grep_context_and_graph_parity_use_the_shared_contract() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap();
    std::fs::create_dir(root.join("sub")).unwrap();
    std::fs::create_dir(root.join("oversized")).unwrap();
    let wide = (1..=300)
        .map(|line| {
            if line == 151 {
                "hit".to_owned()
            } else {
                format!("line-{line}")
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
        + "\n";
    std::fs::write(root.join("one.txt"), wide).unwrap();
    std::fs::write(root.join("sub/two.txt"), "pre\nhit nested\npost\n").unwrap();
    std::fs::write(root.join("paged.txt"), "hit one\nhit two\nhit three\n").unwrap();
    std::fs::write(
        root.join("oversized/too-big.txt"),
        vec![b'z'; 1024 * 1024 + 1],
    )
    .unwrap();

    let mut probe = Probe::start(&root);
    let initialized = probe.request(&initialize_request(1));
    assert_eq!(initialized["result"]["serverInfo"]["name"], "dotlink");

    let listed = probe.request(&json!({"jsonrpc":"2.0","id":2,"method":"tools/list"}));
    let grep = listed["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .find(|tool| tool["name"] == "grep")
        .expect("grep advertised");
    for key in ["context", "before", "after"] {
        assert_eq!(grep["inputSchema"]["properties"][key]["minimum"], 0);
        assert_eq!(grep["inputSchema"]["properties"][key]["maximum"], 128);
    }
    assert_eq!(grep["inputSchema"]["properties"]["context"]["default"], 0);

    let file_args = json!({
        "pattern":"hit",
        "path":"one.txt",
        "literal":true,
        "context":8,
        "before":128,
        "after":0
    });
    let direct_file = probe.request(&call_request(3, "grep", file_args.clone()));
    assert_ne!(direct_file["result"]["isError"], true, "{direct_file}");
    let direct_file_text = text_of(&direct_file);
    assert!(
        direct_file_text.contains("one.txt-23-line-23"),
        "{direct_file}"
    );
    assert!(
        direct_file_text.contains("one.txt:151:hit"),
        "{direct_file}"
    );
    assert!(
        !direct_file_text.contains("one.txt-152-line-152"),
        "{direct_file}"
    );

    let direct_dir = probe.request(&call_request(
        4,
        "grep",
        json!({
            "pattern":"hit",
            "path":".",
            "literal":true,
            "context":1,
            "depth":8,
            "gitignore":false
        }),
    ));
    assert_ne!(direct_dir["result"]["isError"], true, "{direct_dir}");
    let direct_dir_text = text_of(&direct_dir);
    assert!(direct_dir_text.contains("one.txt:151:hit"), "{direct_dir}");
    assert!(
        direct_dir_text.contains("sub/two.txt-1-pre"),
        "{direct_dir}"
    );
    assert!(
        direct_dir_text.contains("sub/two.txt:2:hit nested"),
        "{direct_dir}"
    );
    assert!(
        direct_dir_text.contains("sub/two.txt-3-post"),
        "{direct_dir}"
    );

    let zero = probe.request(&call_request(
        14,
        "grep",
        json!({"pattern":"hit nested","path":"sub/two.txt","literal":true,"context":0}),
    ));
    assert_ne!(zero["result"]["isError"], true, "{zero}");
    assert_eq!(text_of(&zero), "sub/two.txt:2:hit nested\n");

    for (id, arguments) in [
        (5, json!({"pattern":"hit","path":"one.txt","context":129})),
        (6, json!({"pattern":"hit","path":"one.txt","before":129})),
        (7, json!({"pattern":"hit","path":"one.txt","after":129})),
        (8, json!({"pattern":"hit","path":"one.txt","beforee":1})),
    ] {
        let invalid = probe.request(&call_request(id, "grep", arguments));
        assert_eq!(invalid["result"]["isError"], true, "{invalid}");
    }

    let first = probe.request(&call_request(
        9,
        "grep",
        json!({"pattern":"hit","path":"paged.txt","literal":true,"limit":2}),
    ));
    assert_ne!(first["result"]["isError"], true, "{first}");
    assert!(text_of(&first).contains("paged.txt:1:hit one"), "{first}");
    assert!(text_of(&first).contains("paged.txt:2:hit two"), "{first}");
    assert!(text_of(&first).contains("more offset=2"), "{first}");
    let resumed = probe.request(&call_request(
        10,
        "grep",
        json!({"pattern":"hit","path":"paged.txt","literal":true,"offset":2,"limit":2}),
    ));
    assert_ne!(resumed["result"]["isError"], true, "{resumed}");
    assert!(
        text_of(&resumed).contains("paged.txt:3:hit three"),
        "{resumed}"
    );
    assert!(
        !text_of(&resumed).contains("paged.txt:1:hit one"),
        "{resumed}"
    );

    let incomplete = probe.request(&call_request(
        11,
        "grep",
        json!({
            "pattern":"hit",
            "path":"oversized",
            "literal":true,
            "depth":2,
            "gitignore":false
        }),
    ));
    assert_ne!(incomplete["result"]["isError"], true, "{incomplete}");
    assert_eq!(text_of(&incomplete), "incomplete budget\n");

    let graph = probe.request(&call_request(
        12,
        "batch",
        json!({"ops":[["grep", file_args]]}),
    ));
    assert_ne!(graph["result"]["isError"], true, "{graph}");
    assert!(text_of(&graph).contains("one.txt-23-line-23"), "{graph}");
    assert!(text_of(&graph).contains("one.txt:151:hit"), "{graph}");
    assert!(!text_of(&graph).contains("one.txt-152-line-152"), "{graph}");

    let invalid_graph = probe.request(&call_request(
        13,
        "batch",
        json!({"ops":[["grep",{"pattern":"hit","path":"one.txt","before":129}]]}),
    ));
    assert_eq!(invalid_graph["result"]["isError"], true, "{invalid_graph}");
    probe.finish();
}
