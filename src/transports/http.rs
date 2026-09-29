use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

use anyhow::{Context, Result, anyhow};
use axum::Router;
use ngrok::{
    config::{Binding, ForwarderBuilder},
    prelude::EndpointInfo,
};
use rmcp::transport::streamable_http_server::{
    StreamableHttpServerConfig, StreamableHttpService, session::local::LocalSessionManager,
};
use tokio::{net::TcpListener, task::JoinHandle};
use tokio_util::sync::CancellationToken;
use url::Url;
use uuid::Uuid;

use crate::mcp::LocalMachine;

#[derive(Clone, Copy, Debug)]
pub struct Config {
    pub bind: SocketAddr,
    pub ngrok: bool,
    pub http_ephemeral_url: bool,
    pub ngrok_ephemeral_url: bool,
}

pub async fn run(
    machine: LocalMachine,
    config: Config,
    cancellation: CancellationToken,
) -> Result<()> {
    let http_path = mcp_path(config.http_ephemeral_url);
    let http_listener = TcpListener::bind(config.bind)
        .await
        .with_context(|| format!("failed to bind HTTP MCP server to {}", config.bind))?;
    let http_addr = http_listener
        .local_addr()
        .context("failed to read HTTP listen address")?;

    eprintln!("✓ HTTP MCP: http://{http_addr}{http_path}");

    if !config.ngrok {
        return run_server(machine, http_listener, http_path, cancellation).await;
    }

    // ngrok gets a separate loopback-only backend so its public route can be
    // independently ephemeral (or stable) from the local HTTP route.
    let ngrok_path = mcp_path(config.ngrok_ephemeral_url);
    let ngrok_listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
        .await
        .context("failed to bind ngrok MCP backend")?;
    let ngrok_addr = ngrok_listener
        .local_addr()
        .context("failed to read ngrok backend address")?;

    let upstream = ngrok_upstream(ngrok_addr)?;
    let session = ngrok::Session::builder()
        .authtoken_from_env()
        .connect()
        .await
        .context("failed to connect to ngrok; set NGROK_AUTHTOKEN")?;

    let mut endpoint = session.http_endpoint();
    endpoint.binding(Binding::Public);
    let mut forwarder = endpoint
        .listen_and_forward(upstream)
        .await
        .context("failed to create ngrok HTTP endpoint")?;

    let public_mcp = format!("{}{}", forwarder.url().trim_end_matches('/'), ngrok_path);
    eprintln!("✓ ngrok MCP: {public_mcp}");
    eprintln!("  Connect any Streamable HTTP MCP client directly to that URL.");
    eprintln!(
        "  Warning: the public endpoint exposes the MCP permissions granted to this process."
    );

    let http_ct = cancellation.child_token();
    let ngrok_ct = cancellation.child_token();
    let mut http_task = spawn_server(machine.clone(), http_listener, http_path, http_ct);
    let mut ngrok_task = spawn_server(machine, ngrok_listener, ngrok_path, ngrok_ct);

    let result = tokio::select! {
        result = &mut http_task => join_server("HTTP MCP server", result),
        result = &mut ngrok_task => join_server("ngrok MCP backend", result),
        result = forwarder.join() => {
            let result = result.context("ngrok forwarding task panicked")?;
            result.map_err(|error| anyhow!("ngrok forwarding failed: {error}"))
        }
        _ = cancellation.cancelled() => Ok(()),
    };

    cancellation.cancel();

    if !http_task.is_finished() {
        let _ = http_task.await;
    }
    if !ngrok_task.is_finished() {
        let _ = ngrok_task.await;
    }

    result
}

async fn run_server(
    machine: LocalMachine,
    listener: TcpListener,
    path: String,
    cancellation: CancellationToken,
) -> Result<()> {
    let router = mcp_router(machine, &path, cancellation.child_token());
    axum::serve(listener, router)
        .with_graceful_shutdown(async move {
            cancellation.cancelled_owned().await;
        })
        .await
        .context("HTTP MCP server failed")
}

fn spawn_server(
    machine: LocalMachine,
    listener: TcpListener,
    path: String,
    cancellation: CancellationToken,
) -> JoinHandle<Result<()>> {
    tokio::spawn(async move { run_server(machine, listener, path, cancellation).await })
}

fn join_server(name: &str, result: Result<Result<()>, tokio::task::JoinError>) -> Result<()> {
    match result {
        Ok(result) => result.with_context(|| format!("{name} failed")),
        Err(error) => Err(anyhow!("{name} task panicked: {error}")),
    }
}

fn mcp_router(machine: LocalMachine, path: &str, cancellation: CancellationToken) -> Router {
    let server_config = StreamableHttpServerConfig::default()
        .with_legacy_session_mode(false)
        .with_json_response(true)
        .with_sse_keep_alive(None)
        .with_sse_retry(None)
        .with_cancellation_token(cancellation);

    let service: StreamableHttpService<LocalMachine, LocalSessionManager> =
        StreamableHttpService::new(
            move || Ok(machine.clone()),
            LocalSessionManager::default().into(),
            server_config,
        );

    Router::new().nest_service(path, service)
}

fn mcp_path(ephemeral: bool) -> String {
    if !ephemeral {
        return "/mcp".to_owned();
    }

    // Two UUIDv4 values provide ~244 bits of randomness while staying URL-safe.
    let token = format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple());
    format!("/mcp/{token}")
}

fn ngrok_upstream(bind: SocketAddr) -> Result<Url> {
    let ip = match bind.ip() {
        IpAddr::V4(ip) if ip.is_unspecified() => IpAddr::V4(Ipv4Addr::LOCALHOST),
        IpAddr::V6(ip) if ip.is_unspecified() => IpAddr::V6(Ipv6Addr::LOCALHOST),
        ip => ip,
    };

    let host = match ip {
        IpAddr::V4(ip) => ip.to_string(),
        IpAddr::V6(ip) => format!("[{ip}]"),
    };

    Url::parse(&format!("http://{host}:{}", bind.port()))
        .context("failed to build ngrok upstream URL")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normal_mcp_path_is_stable() {
        assert_eq!(mcp_path(false), "/mcp");
    }

    #[test]
    fn ephemeral_mcp_path_is_long_url_safe_and_fresh() {
        let first = mcp_path(true);
        let second = mcp_path(true);
        let token = first.strip_prefix("/mcp/").expect("ephemeral prefix");
        assert_eq!(token.len(), 64);
        assert!(token.bytes().all(|byte| byte.is_ascii_hexdigit()));
        assert_ne!(first, second);
    }

    #[test]
    fn unspecified_http_bind_forwards_ngrok_to_loopback() {
        let upstream = ngrok_upstream("0.0.0.0:3000".parse().unwrap()).unwrap();
        assert_eq!(upstream.as_str(), "http://127.0.0.1:3000/");
    }

    #[test]
    fn loopback_http_bind_is_preserved() {
        let upstream = ngrok_upstream("127.0.0.1:4321".parse().unwrap()).unwrap();
        assert_eq!(upstream.as_str(), "http://127.0.0.1:4321/");
    }
}
