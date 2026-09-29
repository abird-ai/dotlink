use std::{
    future::IntoFuture,
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
};

use anyhow::{Context, Result, anyhow};
use axum::Router;
use ngrok::{
    config::{Binding, ForwarderBuilder},
    prelude::EndpointInfo,
};
use rmcp::transport::streamable_http_server::{
    StreamableHttpServerConfig, StreamableHttpService, session::local::LocalSessionManager,
};
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;
use url::Url;

use crate::mcp::LocalMachine;

#[derive(Clone, Copy, Debug)]
pub struct Config {
    pub bind: SocketAddr,
    pub ngrok: bool,
}

pub async fn run(
    machine: LocalMachine,
    config: Config,
    cancellation: CancellationToken,
) -> Result<()> {
    let server_config = StreamableHttpServerConfig::default()
        .with_legacy_session_mode(false)
        .with_json_response(true)
        .with_sse_keep_alive(None)
        .with_sse_retry(None)
        .with_cancellation_token(cancellation.child_token());

    let service: StreamableHttpService<LocalMachine, LocalSessionManager> =
        StreamableHttpService::new(
            move || Ok(machine.clone()),
            LocalSessionManager::default().into(),
            server_config,
        );

    let router = Router::new().nest_service("/mcp", service);
    let listener = TcpListener::bind(config.bind)
        .await
        .with_context(|| format!("failed to bind HTTP MCP server to {}", config.bind))?;
    let local_addr = listener
        .local_addr()
        .context("failed to read HTTP listen address")?;

    eprintln!("✓ HTTP MCP: http://{local_addr}/mcp");

    let shutdown = cancellation.child_token();
    let server = axum::serve(listener, router)
        .with_graceful_shutdown(async move {
            shutdown.cancelled_owned().await;
        })
        .into_future();
    tokio::pin!(server);

    if !config.ngrok {
        server.await.context("HTTP MCP server failed")?;
        return Ok(());
    }

    let upstream = ngrok_upstream(local_addr)?;
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

    let public_mcp = format!("{}/mcp", forwarder.url().trim_end_matches('/'));
    eprintln!("✓ ngrok MCP: {public_mcp}");
    eprintln!("  Connect any Streamable HTTP MCP client directly to that URL.");
    eprintln!(
        "  Warning: the public endpoint exposes the MCP permissions granted to this process."
    );

    tokio::select! {
        result = &mut server => {
            result.context("HTTP MCP server failed")?;
        }
        result = forwarder.join() => {
            let result = result.context("ngrok forwarding task panicked")?;
            result.map_err(|error| anyhow!("ngrok forwarding failed: {error}"))?;
        }
        _ = cancellation.cancelled() => {}
    }

    Ok(())
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
