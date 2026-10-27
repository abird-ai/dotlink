//! Tool commands never inherit supervisor variables or dotlink credentials.
#![cfg(unix)]

use std::{
    io::{BufRead, BufReader, Write},
    process::{Child, Command, Stdio},
    sync::mpsc,
    time::{Duration, Instant},
};

use serde_json::{Value, json};

/// Bound for each response; a hung server fails the test instead of the run.
const RESPONSE_TIMEOUT: Duration = Duration::from_secs(30);

/// A stdio MCP server, killed on every exit path including failed asserts.
struct Server {
    child: Child,
    lines: mpsc::Receiver<String>,
}

impl Server {
    fn spawn(command: &mut Command) -> Self {
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let stdout = BufReader::new(child.stdout.take().unwrap());
        let (sender, lines) = mpsc::channel();
        // Ends at EOF, once the server exits.
        std::thread::spawn(move || {
            for line in stdout.lines().map_while(Result::ok) {
                if sender.send(line).is_err() {
                    break;
                }
            }
        });
        Self { child, lines }
    }

    fn send(&mut self, message: &Value) {
        let stdin = self.child.stdin.as_mut().unwrap();
        writeln!(stdin, "{message}").unwrap();
        stdin.flush().unwrap();
    }

    /// The response to request `id`, skipping notifications.
    fn response(&self, id: u64) -> Value {
        let deadline = Instant::now() + RESPONSE_TIMEOUT;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            let line = self
                .lines
                .recv_timeout(remaining)
                .unwrap_or_else(|error| panic!("no response to request {id}: {error}"));
            let message: Value = serde_json::from_str(&line).unwrap();
            if message["id"] == id {
                return message;
            }
        }
    }

    /// Initialize, then confirm, as the MCP lifecycle requires.
    fn handshake(&mut self) {
        self.send(
            &json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {
            "protocolVersion": "2025-06-18", "capabilities": {},
            "clientInfo": {"name": "test", "version": "1"}}}),
        );
        let initialized = self.response(1);
        assert!(initialized["result"].is_object(), "{initialized}");
        self.send(&json!({"jsonrpc": "2.0", "method": "notifications/initialized"}));
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn shell_commands_inherit_neither_supervisor_variables_nor_credentials() {
    let home = tempfile::tempdir().unwrap();
    let config = home.path().join("profile.jsonc");
    std::fs::write(
        &config,
        r#"{"version":10,"transports":{"openai":false,"stdio":true}}"#,
    )
    .unwrap();

    let mut server = Server::spawn(
        Command::new(env!("CARGO_BIN_EXE_dotlink"))
            .args(["--stdio", "--allow-all", "--no-sandbox"])
            .current_dir(home.path())
            .env("DOTLINK_CONFIG", &config)
            .env("XDG_CONFIG_HOME", home.path().join("config"))
            .env("XDG_STATE_HOME", home.path().join("state"))
            .env("DOTLINK_SUPERVISOR", "systemd")
            .env("NOTIFY_SOCKET", "/run/systemd/notify")
            .env("OPENAI_API_KEY", "secret")
            .env("NGROK_AUTHTOKEN", "secret")
            .env_remove("DOTLINK_API_KEY")
            .env_remove("DOTLINK_ID"),
    );
    server.handshake();
    server.send(
        &json!({"jsonrpc": "2.0", "id": 2, "method": "tools/call", "params": {
        "name": "bash", "arguments": {"command": "env"}}}),
    );
    let response = server.response(2);

    let result: Value =
        serde_json::from_str(response["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(result["exit_code"], 0, "{result}");
    let environment = result["stdout"].as_str().unwrap();
    assert!(environment.contains("PATH="), "{environment}");
    for name in [
        "DOTLINK_SUPERVISOR",
        "NOTIFY_SOCKET",
        "OPENAI_API_KEY",
        "NGROK_AUTHTOKEN",
    ] {
        assert!(
            !environment
                .lines()
                .any(|line| line.starts_with(&format!("{name}="))),
            "{name} leaked into a tool command"
        );
    }
}
