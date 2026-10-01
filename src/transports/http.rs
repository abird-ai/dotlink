use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

use anyhow::{Context, Result, anyhow, bail};
use axum::{
    Router,
    body::Body,
    http::Request,
    middleware::{self, Next},
    response::Response,
};
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

use crate::{logging::LogConfig, mcp::LocalMachine, oauth};

#[derive(Clone, Debug)]
pub struct Config {
    pub bind: SocketAddr,
    pub ngrok: bool,
    pub ngrok_domain: Option<String>,
    pub http_ephemeral_url: bool,
    pub ngrok_ephemeral_url: bool,
    pub oauth: Option<oauth::Runtime>,
    pub local_oauth: bool,
    pub ngrok_oauth: bool,
    pub oauth_public_url: Option<Url>,
    pub ngrok_no_auth: bool,
    pub log: LogConfig,
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

    let local_oauth = if config.local_oauth {
        let runtime = config
            .oauth
            .clone()
            .ok_or_else(|| anyhow!("OAuth runtime is missing"))?;
        let public_base = match config.oauth_public_url.clone() {
            Some(url) => url,
            None => loopback_public_base(http_addr)?,
        };
        Some(oauth::Server::new(runtime, public_base, &http_path)?)
    } else {
        None
    };

    eprintln!("✓ HTTP MCP: http://{http_addr}{http_path}");
    if let Some(server) = local_oauth.as_ref() {
        eprintln!(
            "✓ OAuth: {} (resource {})",
            server.issuer(),
            server.resource()
        );
    }

    if !config.ngrok {
        return run_server(
            machine,
            http_listener,
            http_path,
            config.log.clone(),
            local_oauth,
            cancellation,
        )
        .await;
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
    if let Some(domain) = config.ngrok_domain.as_deref() {
        endpoint.domain(domain);
    }
    let mut forwarder = endpoint
        .listen_and_forward(upstream)
        .await
        .context("failed to create ngrok HTTP endpoint")?;

    let ngrok_public_base =
        Url::parse(forwarder.url()).context("ngrok returned an invalid public URL")?;
    if let Some(expected_domain) = config.ngrok_domain.as_deref() {
        let actual_domain = ngrok_public_base
            .host_str()
            .ok_or_else(|| anyhow!("ngrok public URL has no hostname"))?;
        if !actual_domain.eq_ignore_ascii_case(expected_domain) {
            bail!(
                "ngrok returned unexpected domain {actual_domain:?}; expected {expected_domain:?}"
            );
        }
    }

    let public_mcp = format!(
        "{}{}",
        ngrok_public_base.as_str().trim_end_matches('/'),
        ngrok_path
    );
    let ngrok_oauth = if config.ngrok_oauth {
        let runtime = config
            .oauth
            .clone()
            .ok_or_else(|| anyhow!("OAuth runtime is missing"))?;
        Some(oauth::Server::new(
            runtime,
            ngrok_public_base.clone(),
            &ngrok_path,
        )?)
    } else {
        None
    };

    eprintln!("✓ ngrok MCP: {public_mcp}");
    if let Some(server) = ngrok_oauth.as_ref() {
        eprintln!(
            "✓ OAuth: {} (resource {})",
            server.issuer(),
            server.resource()
        );
        if config.ngrok_domain.is_none() {
            eprintln!(
                "  Note: ngrok hostname is automatic; if it changes, reconnect the OAuth client."
            );
        }
        if config.ngrok_ephemeral_url {
            eprintln!(
                "  Note: ngrok MCP path is ephemeral; reconnect the OAuth client after each restart."
            );
        }
    } else if config.ngrok_no_auth {
        eprintln!("  WARNING: public ngrok MCP is intentionally unauthenticated for this run.");
    }
    eprintln!("  Connect any Streamable HTTP MCP client directly to that URL.");

    let http_ct = cancellation.child_token();
    let ngrok_ct = cancellation.child_token();
    let mut http_task = spawn_server(
        machine.clone(),
        http_listener,
        http_path,
        config.log.clone(),
        local_oauth,
        http_ct,
    );
    let mut ngrok_task = spawn_server(
        machine,
        ngrok_listener,
        ngrok_path,
        config.log.clone(),
        ngrok_oauth,
        ngrok_ct,
    );

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
    log: LogConfig,
    oauth_server: Option<oauth::Server>,
    cancellation: CancellationToken,
) -> Result<()> {
    let router = mcp_router(
        machine,
        &path,
        log,
        oauth_server,
        cancellation.child_token(),
    );
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
    log: LogConfig,
    oauth_server: Option<oauth::Server>,
    cancellation: CancellationToken,
) -> JoinHandle<Result<()>> {
    tokio::spawn(async move {
        run_server(machine, listener, path, log, oauth_server, cancellation).await
    })
}

fn join_server(name: &str, result: Result<Result<()>, tokio::task::JoinError>) -> Result<()> {
    match result {
        Ok(result) => result.with_context(|| format!("{name} failed")),
        Err(error) => Err(anyhow!("{name} task panicked: {error}")),
    }
}

fn mcp_router(
    machine: LocalMachine,
    path: &str,
    log: LogConfig,
    oauth_server: Option<oauth::Server>,
    cancellation: CancellationToken,
) -> Router {
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

    let mut mcp = Router::new().nest_service(path, service);
    if let Some(server) = oauth_server.clone() {
        mcp = mcp.layer(middleware::from_fn(
            move |request: Request<Body>, next: Next| {
                let server = server.clone();
                async move { server.require_bearer(request, next).await }
            },
        ));
    }

    let mut router = mcp;
    if let Some(server) = oauth_server {
        router = server.router().merge(router);
    }

    router.layer(middleware::from_fn(
        move |request: Request<Body>, next: Next| {
            let log = log.clone();
            async move { log_http_request(log, request, next).await }
        },
    ))
}

async fn log_http_request(log: LogConfig, request: Request<Body>, next: Next) -> Response {
    let method = request.method().clone();
    let uri = safe_log_path(request.uri().path());
    let label = format!("{method} {uri}");
    let started = log.request("http", &label);
    let response = next.run(request).await;
    log.request_done(
        "http",
        &label,
        started,
        response.status().as_u16().to_string(),
    );
    response
}

fn safe_log_path(path: &str) -> &str {
    if path.starts_with("/mcp/") {
        "/mcp/<ephemeral>"
    } else {
        path
    }
}

fn mcp_path(ephemeral: bool) -> String {
    if !ephemeral {
        return "/mcp".to_owned();
    }

    // Two UUIDv4 values provide ~244 bits of randomness while staying URL-safe.
    let token = format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple());
    format!("/mcp/{token}")
}

fn loopback_public_base(bind: SocketAddr) -> Result<Url> {
    let ip = bind.ip();
    if !ip.is_loopback() {
        bail!(
            "OAuth on non-loopback HTTP requires --public-url=https://... so issuer/resource are never inferred from request headers"
        );
    }

    let host = match ip {
        IpAddr::V4(ip) => ip.to_string(),
        IpAddr::V6(ip) => format!("[{ip}]"),
    };
    Url::parse(&format!("http://{host}:{}", bind.port()))
        .context("failed to build loopback OAuth public URL")
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
    fn http_log_path_redacts_ephemeral_mcp_tokens() {
        assert_eq!(safe_log_path("/mcp"), "/mcp");
        assert_eq!(safe_log_path("/mcp/0123456789abcdef"), "/mcp/<ephemeral>");
        assert_eq!(safe_log_path("/health"), "/health");
    }

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
    fn oauth_loopback_origin_refuses_non_loopback_bind() {
        assert_eq!(
            loopback_public_base("127.0.0.1:4321".parse().unwrap())
                .unwrap()
                .as_str(),
            "http://127.0.0.1:4321/"
        );
        assert!(loopback_public_base("0.0.0.0:4321".parse().unwrap()).is_err());
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
