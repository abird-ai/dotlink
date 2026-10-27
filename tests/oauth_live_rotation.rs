//! Exercise owner credential rotation through the actual HTTP authorization
//! service, not an in-memory handler carrying a fixed password generation.

use std::{
    fs,
    io::Read,
    net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener},
    path::Path,
    process::{Child, Command, Stdio},
    time::Duration,
};

use argon2::{Argon2, PasswordHasher, password_hash::SaltString};
use reqwest::{Client, StatusCode, redirect::Policy};
use serde_json::{Value, json};

struct Server(Child);

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn owner_hash(password: &str) -> String {
    let salt = SaltString::encode_b64(&[7_u8; 16]).unwrap();
    Argon2::default()
        .hash_password(password.as_bytes(), &salt)
        .unwrap()
        .to_string()
}

fn replace_json(path: &Path, data: &Value) {
    let replacement = path.with_extension("staged");
    fs::write(&replacement, serde_json::to_vec_pretty(data).unwrap()).unwrap();
    fs::rename(&replacement, path).unwrap();
}

async fn approval(
    client: &Client,
    base: &str,
    client_id: &str,
    password: &str,
) -> (StatusCode, String, Option<String>) {
    let response = client
        .get(format!("{base}/oauth/authorize"))
        .query(&[
            ("response_type", "code"),
            ("client_id", client_id),
            ("redirect_uri", "https://client.example/callback"),
            ("resource", &format!("{base}/mcp")),
            ("scope", "mcp:access offline_access"),
            (
                "code_challenge",
                "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM",
            ),
            ("code_challenge_method", "S256"),
        ])
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let html = response.text().await.unwrap();
    let id = html
        .split("name=\"request_id\" value=\"")
        .nth(1)
        .and_then(|value| value.split('"').next())
        .expect("OAuth form must provide a request ID");
    let response = client
        .post(format!("{base}/oauth/authorize"))
        .form(&[
            ("request_id", id),
            ("password", password),
            ("decision", "allow"),
        ])
        .send()
        .await
        .unwrap();
    let status = response.status();
    let location = response
        .headers()
        .get(reqwest::header::LOCATION)
        .and_then(|header| header.to_str().ok())
        .map(str::to_owned);
    let body = response.text().await.unwrap();
    (status, body, location)
}

#[tokio::test]
async fn rotation_is_observed_by_live_http_without_accepting_old_password() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let revision_one = "11111111111111111111111111111111";
    let revision_two = "22222222222222222222222222222222";
    let listener = TcpListener::bind(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    let base = format!("http://127.0.0.1:{port}");
    let config = root.join("profile.jsonc");
    let state = root.join("state/abird/dotlink/oauth.json");
    fs::create_dir_all(state.parent().unwrap()).unwrap();

    let mut profile = json!({
        "version": 10,
        "transports": {
            "openai": false, "stdio": false, "http": true,
            "http_bind": format!("127.0.0.1:{port}")
        },
        "oauth": { "enabled": true },
        "owner_password_revision": revision_one
    });
    replace_json(&config, &profile);
    let mut saved = json!({
        "version": 1,
        "owner_password_hash": null,
        "owner_password_generations": {},
        "clients": {},
        "refresh_tokens": {}
    });
    saved["owner_password_generations"][revision_one] =
        json!(owner_hash("first-valid-owner-password"));
    replace_json(&state, &saved);

    let process = Command::new(env!("CARGO_BIN_EXE_dotlink"))
        .current_dir(root)
        .env("DOTLINK_CONFIG", &config)
        .env("XDG_CONFIG_HOME", root.join("config"))
        .env("XDG_STATE_HOME", root.join("state"))
        .env("HOME", root)
        .env_remove("DOTLINK_API_KEY")
        .env_remove("DOTLINK_API_KEY_FILE")
        .env_remove("DOTLINK_ID")
        .env_remove("CONTROL_PLANE_TUNNEL_ID")
        .env_remove("CONTROL_PLANE_API_KEY")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut server = Server(process);
    let client = Client::builder()
        .no_proxy()
        .redirect(Policy::none())
        .timeout(Duration::from_secs(3))
        .build()
        .unwrap();

    let mut started = false;
    for _ in 0..80 {
        if let Some(status) = server.0.try_wait().unwrap() {
            let mut stderr = String::new();
            server
                .0
                .stderr
                .take()
                .unwrap()
                .read_to_string(&mut stderr)
                .unwrap();
            panic!("dotlink exited prematurely ({status}): {stderr}");
        }
        if let Ok(response) = client
            .get(format!("{base}/.well-known/oauth-authorization-server"))
            .send()
            .await
            && response.status() == StatusCode::OK
        {
            started = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(40)).await;
    }
    assert!(started, "OAuth server did not start");

    let registration: Value = client
        .post(format!("{base}/oauth/register"))
        .json(&json!({
            "client_name":"Integration fixture",
            "redirect_uris":["https://client.example/callback"],
            "grant_types":["authorization_code","refresh_token"],
            "response_types":["code"],
            "token_endpoint_auth_method":"none"
        }))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    let client_id = registration["client_id"].as_str().unwrap();

    let (old_before, _, _) =
        approval(&client, &base, client_id, "first-valid-owner-password").await;
    assert_eq!(old_before, StatusCode::SEE_OTHER);

    // Stage the new hash first, then commit the profile selector. Preserve
    // approved clients and grants, as setup does.
    saved = serde_json::from_slice(&fs::read(&state).unwrap()).unwrap();
    saved["owner_password_generations"][revision_two] =
        json!(owner_hash("second-valid-owner-password"));
    replace_json(&state, &saved);
    profile["owner_password_revision"] = json!(revision_two);
    replace_json(&config, &profile);

    let (new_after, _, new_location) =
        approval(&client, &base, client_id, "second-valid-owner-password").await;
    assert_eq!(
        new_after,
        StatusCode::SEE_OTHER,
        "rotated owner password was rejected"
    );
    let (old_after, body, _) =
        approval(&client, &base, client_id, "first-valid-owner-password").await;
    assert_eq!(
        old_after,
        StatusCode::OK,
        "previous owner password was accepted"
    );
    assert!(body.contains("Invalid owner password"), "{body}");

    // A temporary persistence failure must not consume a valid authorization
    // code. Once state can be written again, exactly one concurrent redemption
    // succeeds; the second request must not issue another pair of tokens.
    let callback = url::Url::parse(new_location.as_deref().unwrap()).unwrap();
    let code = callback
        .query_pairs()
        .find(|(key, _)| key == "code")
        .map(|(_, value)| value.into_owned())
        .unwrap();
    let resource = format!("{base}/mcp");
    let form = [
        ("grant_type", "authorization_code"),
        ("code", code.as_str()),
        ("client_id", client_id),
        ("redirect_uri", "https://client.example/callback"),
        ("resource", resource.as_str()),
        (
            "code_verifier",
            "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk",
        ),
    ];
    let lock = std::path::PathBuf::from(format!("{}.lock", state.display()));
    fs::remove_file(&lock).unwrap();
    fs::create_dir(&lock).unwrap();
    let failed = client
        .post(format!("{base}/oauth/token"))
        .form(&form)
        .send()
        .await
        .unwrap();
    assert_eq!(failed.status(), StatusCode::SERVICE_UNAVAILABLE);
    let body: Value = failed.json().await.unwrap();
    assert_eq!(body["error"], "server_error");
    fs::remove_dir(&lock).unwrap();

    let first = client
        .post(format!("{base}/oauth/token"))
        .form(&form)
        .send();
    let second = client
        .post(format!("{base}/oauth/token"))
        .form(&form)
        .send();
    let (first, second) = tokio::join!(first, second);
    let statuses = [first.unwrap().status(), second.unwrap().status()];
    assert_eq!(
        statuses
            .iter()
            .filter(|status| **status == StatusCode::OK)
            .count(),
        1,
        "one exchange must succeed after the state lock becomes available"
    );
    assert_eq!(
        statuses
            .iter()
            .filter(|status| {
                matches!(
                    **status,
                    StatusCode::BAD_REQUEST | StatusCode::SERVICE_UNAVAILABLE
                )
            })
            .count(),
        1,
        "the duplicate must be rejected or told to retry while redemption is busy"
    );
    let replay = client
        .post(format!("{base}/oauth/token"))
        .form(&form)
        .send()
        .await
        .unwrap();
    assert_eq!(
        replay.status(),
        StatusCode::BAD_REQUEST,
        "a successfully redeemed code must be one-time"
    );

    // Deleting a previously persisted profile must never fall back to a
    // legacy hash, even if that hash remains in the OAuth state.
    saved["owner_password_hash"] = json!(owner_hash("first-valid-owner-password"));
    replace_json(&state, &saved);
    fs::remove_file(&config).unwrap();
    let (deleted, _, _) = approval(&client, &base, client_id, "first-valid-owner-password").await;
    assert_ne!(
        deleted,
        StatusCode::SEE_OTHER,
        "deleted profile re-enabled an old password"
    );
}
