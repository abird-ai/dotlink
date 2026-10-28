use std::{
    future::Future,
    io,
    net::{IpAddr, SocketAddr},
    pin::Pin,
    sync::Arc,
    task::{Context as TaskContext, Poll},
};

use anyhow::{Context, Result, anyhow, bail};
use axum::{
    Router,
    body::Body,
    extract::connect_info::Connected,
    http::{Request, StatusCode, header::HOST, uri::Authority},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    serve::{IncomingStream, Listener},
};
use futures_util::StreamExt as _;
use ngrok::{
    EndpointConn,
    config::Binding,
    prelude::{ConnInfo as _, EndpointInfo, TunnelBuilder, TunnelInfo},
    tunnel::{AcceptError, HttpTunnel},
};
use rmcp::transport::streamable_http_server::{
    StreamableHttpService, session::local::LocalSessionManager,
};
use tokio::{
    io::{AsyncRead, AsyncWrite, ReadBuf},
    net::{TcpListener, TcpStream},
    sync::{Mutex, oneshot},
    task::JoinSet,
    time,
};
use tokio_util::sync::{CancellationToken, WaitForCancellationFutureOwned};
use url::Url;
use uuid::Uuid;

use crate::{
    logging::{self, LogConfig},
    mcp::LocalMachine,
    oauth,
    requester::RequesterId,
};

#[derive(Clone)]
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
    pub ngrok_authtoken: Option<String>,
    pub log: LogConfig,
}

/// Return a startup operation's result, or `None` when shutdown wins. Startup
/// futures are dropped immediately so reconnects cannot strand the supervisor.
async fn startup_or_cancelled<F>(
    cancellation: &CancellationToken,
    operation: F,
) -> Option<F::Output>
where
    F: Future,
{
    tokio::select! {
        biased;
        _ = cancellation.cancelled() => None,
        result = operation => Some(result),
    }
}

pub async fn run(
    machine: LocalMachine,
    config: Config,
    cancellation: CancellationToken,
) -> Result<()> {
    // A cancelled supervisor must also cancel ongoing MCP calls, not merely
    // abort listener tasks. Dropping this guard covers every early exit.
    let _cancel_on_drop = cancellation.clone().drop_guard();
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

    if !config.ngrok {
        print_local_ready(http_addr, &http_path, local_oauth.as_ref());
        return run_tcp_server(
            machine,
            http_listener,
            http_path,
            config.log.clone(),
            local_oauth,
            trusted_http_hosts(http_addr, config.oauth_public_url.as_ref()),
            cancellation,
        )
        .await;
    }

    // The public route is served directly from ngrok's authenticated tunnel
    // connection metadata; no loopback proxy or forwarding header is trusted.
    let ngrok_path = mcp_path(config.ngrok_ephemeral_url);
    let mut session_builder = ngrok::Session::builder();
    if let Some(token) = config.ngrok_authtoken.clone() {
        session_builder.authtoken(token);
    } else {
        session_builder.authtoken_from_env();
    }
    let Some(session) = startup_or_cancelled(&cancellation, session_builder.connect()).await else {
        return Ok(());
    };
    let mut session = session.map_err(ngrok_connect_error)?;

    let mut endpoint = session.http_endpoint();
    endpoint.binding(Binding::Public);
    if let Some(domain) = config.ngrok_domain.as_deref() {
        endpoint.domain(domain);
    }
    let Some(tunnel) = startup_or_cancelled(&cancellation, endpoint.listen()).await else {
        return Ok(());
    };
    let tunnel = tunnel.context("failed to create ngrok HTTP endpoint")?;
    let tunnel_id = tunnel.id().to_owned();
    // Keep the SDK tunnel value alive until the explicit close RPC completes;
    // otherwise its Drop implementation starts an unawaited competing close.
    let tunnel = Arc::new(Mutex::new(tunnel));
    let mut cleanup = None;

    let primary = async {
        let ngrok_public_base = {
            let tunnel = tunnel.lock().await;
            Url::parse(tunnel.url()).context("ngrok returned an invalid public URL")?
        };
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

    let ngrok_hosts = public_http_hosts(&ngrok_public_base)?;
    let (tunnel_terminal_tx, tunnel_terminal_rx) = oneshot::channel();
    let ngrok_listener = NgrokListener::new(tunnel.clone(), tunnel_terminal_tx);

    // The JoinSet *owns* both server tasks. Dropping an interrupted HTTP
    // supervisor aborts them instead of detaching live listeners into a new
    // runtime after a restart. Normal exits request graceful shutdown first.
    let mut servers = JoinSet::new();
    print_local_ready(http_addr, &http_path, local_oauth.as_ref());
    servers.spawn(run_tcp_server(
        machine.clone(),
        http_listener,
        http_path,
        config.log.clone(),
        local_oauth,
        trusted_http_hosts(http_addr, config.oauth_public_url.as_ref()),
        cancellation.child_token(),
    ));
    servers.spawn(run_ngrok_server(
        machine,
        ngrok_listener,
        ngrok_path,
        config.log.clone(),
        ngrok_oauth,
        ngrok_hosts,
        cancellation.child_token(),
    ));

    let result = tokio::select! {
        biased;
        _ = cancellation.cancelled() => Ok(()),
        result = servers.join_next() => match result {
            Some(Ok(result)) => result.context("HTTP MCP server exited"),
            Some(Err(error)) => Err(anyhow!("HTTP MCP server task panicked: {error}")),
            None => Err(anyhow!("both HTTP MCP servers exited unexpectedly")),
        },
        result = tunnel_terminal_rx => match result {
            Ok(terminal) => tunnel_terminal_result(terminal),
            Err(_) => Err(anyhow!("ngrok tunnel listener exited without a terminal status")),
        },
    };

        // Cancel routes before closing ingress. Closing the public listener
        // does not invalidate already-accepted EndpointConn values, so cleanup
        // can overlap their drain and both five-second bounds compose into one.
        cancellation.cancel();
        let (server_cleanup, ngrok_cleanup) = tokio::join!(
            stop_server_tasks(&mut servers, &cancellation),
            close_ngrok_tunnel_and_session(&mut session, &tunnel_id),
        );
        cleanup = Some(merge_server_and_ngrok_cleanup(
            server_cleanup,
            ngrok_cleanup,
        ));
        result
    }
    .await;

    // Setup failures before server ownership begins still close explicitly.
    let cleanup = match cleanup {
        Some(cleanup) => cleanup,
        None => close_ngrok_tunnel_and_session(&mut session, &tunnel_id).await,
    };
    drop(tunnel);
    merge_primary_and_cleanup(primary, cleanup)
}

/// Only ngrok's answer to authentication points at the token, not a network
/// failure on the way. It stays restartable: ngrok also rejects transient
/// conditions such as a lingering prior session.
fn ngrok_connect_error(error: ngrok::session::ConnectError) -> anyhow::Error {
    use ngrok::session::{ConnectError, RpcError};

    let context = match error {
        ConnectError::Auth(RpcError::Response(_)) => {
            "ngrok rejected the session; check NGROK_AUTHTOKEN or NGROK_AUTHTOKEN_FILE"
        }
        _ => "failed to connect to ngrok",
    };
    anyhow::Error::new(error).context(context)
}

pub(super) const HTTP_SHUTDOWN_BUDGET_SECS: u64 = 5;
const HTTP_SHUTDOWN_BUDGET: std::time::Duration =
    std::time::Duration::from_secs(HTTP_SHUTDOWN_BUDGET_SECS);
const NGROK_TUNNEL_CLOSE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(4);
const NGROK_SESSION_CLOSE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(1);
const _: () = assert!(
    NGROK_TUNNEL_CLOSE_TIMEOUT.as_secs() + NGROK_SESSION_CLOSE_TIMEOUT.as_secs()
        <= HTTP_SHUTDOWN_BUDGET_SECS
);

async fn close_ngrok_tunnel_and_session(
    session: &mut ngrok::Session,
    tunnel_id: &str,
) -> Result<()> {
    let tunnel_session = session.clone();
    let tunnel_id = tunnel_id.to_owned();
    run_ngrok_cleanup(
        move || async move {
            tunnel_session
                .close_tunnel(&tunnel_id)
                .await
                .context("failed to close ngrok tunnel")
        },
        || async {
            session
                .close()
                .await
                .context("failed to close ngrok session")
        },
    )
    .await
}

async fn run_ngrok_cleanup<CloseTunnel, TunnelFuture, CloseSession, SessionFuture>(
    close_tunnel: CloseTunnel,
    close_session: CloseSession,
) -> Result<()>
where
    CloseTunnel: FnOnce() -> TunnelFuture,
    TunnelFuture: Future<Output = Result<()>>,
    CloseSession: FnOnce() -> SessionFuture,
    SessionFuture: Future<Output = Result<()>>,
{
    let tunnel = match time::timeout(NGROK_TUNNEL_CLOSE_TIMEOUT, close_tunnel()).await {
        Ok(result) => result,
        Err(_) => Err(anyhow!("timed out closing ngrok tunnel")),
    };
    // Always close the parent session after the explicit tunnel close attempt.
    // After a successful session close, HttpTunnel::drop may issue its SDK
    // follow-up close only against a closed client, not a retained runtime.
    let session = match time::timeout(NGROK_SESSION_CLOSE_TIMEOUT, close_session()).await {
        Ok(result) => result,
        Err(_) => Err(anyhow!("timed out closing ngrok session")),
    };
    merge_ngrok_cleanup_results(tunnel, session)
}

fn merge_ngrok_cleanup_results(tunnel: Result<()>, session: Result<()>) -> Result<()> {
    match (tunnel, session) {
        (Err(tunnel), Err(_session)) => {
            tracing::warn!("ngrok session cleanup also failed after tunnel cleanup failed");
            Err(tunnel)
        }
        (Err(tunnel), Ok(())) => Err(tunnel),
        (Ok(()), session) => session,
    }
}

fn merge_server_and_ngrok_cleanup(server: Result<()>, ngrok: Result<()>) -> Result<()> {
    match (server, ngrok) {
        (Err(server), Err(_ngrok)) => {
            tracing::warn!("ngrok cleanup also failed after HTTP server cleanup failed");
            Err(server)
        }
        (Err(server), Ok(())) => Err(server),
        (Ok(()), ngrok) => ngrok,
    }
}

fn merge_primary_and_cleanup(primary: Result<()>, cleanup: Result<()>) -> Result<()> {
    match (primary, cleanup) {
        (Err(_primary), Err(cleanup)) => {
            tracing::warn!("HTTP cleanup failed after the primary transport failure");
            Err(cleanup)
        }
        (Err(primary), Ok(())) => Err(primary),
        (Ok(()), cleanup) => cleanup,
    }
}

fn print_local_ready(http_addr: SocketAddr, http_path: &str, oauth_server: Option<&oauth::Server>) {
    eprintln!("✓ HTTP MCP: http://{http_addr}{http_path}");
    if let Some(server) = oauth_server {
        eprintln!(
            "✓ OAuth: {} (resource {})",
            server.issuer(),
            server.resource()
        );
    }
}

struct ForceCloseIo<T> {
    io: T,
    force_close: Pin<Box<WaitForCancellationFutureOwned>>,
}

impl<T> ForceCloseIo<T> {
    fn new(io: T, force_close: CancellationToken) -> Self {
        Self {
            io,
            force_close: Box::pin(force_close.cancelled_owned()),
        }
    }

    fn poll_force_close(&mut self, context: &mut TaskContext<'_>) -> io::Result<()> {
        match self.force_close.as_mut().poll(context) {
            Poll::Ready(()) => Err(io::Error::new(
                io::ErrorKind::ConnectionAborted,
                "HTTP connection force-closed during shutdown",
            )),
            Poll::Pending => Ok(()),
        }
    }
}

impl<T: AsyncRead + Unpin> AsyncRead for ForceCloseIo<T> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        context: &mut TaskContext<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        if let Err(error) = self.poll_force_close(context) {
            return Poll::Ready(Err(error));
        }
        Pin::new(&mut self.io).poll_read(context, buffer)
    }
}

impl<T: AsyncWrite + Unpin> AsyncWrite for ForceCloseIo<T> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        context: &mut TaskContext<'_>,
        buffer: &[u8],
    ) -> Poll<io::Result<usize>> {
        if let Err(error) = self.poll_force_close(context) {
            return Poll::Ready(Err(error));
        }
        Pin::new(&mut self.io).poll_write(context, buffer)
    }

    fn poll_flush(mut self: Pin<&mut Self>, context: &mut TaskContext<'_>) -> Poll<io::Result<()>> {
        if let Err(error) = self.poll_force_close(context) {
            return Poll::Ready(Err(error));
        }
        Pin::new(&mut self.io).poll_flush(context)
    }

    fn poll_shutdown(
        mut self: Pin<&mut Self>,
        context: &mut TaskContext<'_>,
    ) -> Poll<io::Result<()>> {
        if let Err(error) = self.poll_force_close(context) {
            return Poll::Ready(Err(error));
        }
        Pin::new(&mut self.io).poll_shutdown(context)
    }
}

struct TcpRequesterListener {
    listener: TcpListener,
    force_close: CancellationToken,
}

impl TcpRequesterListener {
    fn new(listener: TcpListener, force_close: CancellationToken) -> Self {
        Self {
            listener,
            force_close,
        }
    }
}

impl Listener for TcpRequesterListener {
    type Io = ForceCloseIo<TcpStream>;
    type Addr = RequesterId;

    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        let (connection, remote_addr) = <TcpListener as Listener>::accept(&mut self.listener).await;
        let requester = RequesterId::from_socket(remote_addr)
            .expect("an accepted TCP peer always has a specified address and source port");
        (
            ForceCloseIo::new(connection, self.force_close.clone()),
            requester,
        )
    }

    fn local_addr(&self) -> io::Result<Self::Addr> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "a TCP listener has no local requester address",
        ))
    }
}

impl Connected<IncomingStream<'_, TcpRequesterListener>> for RequesterId {
    fn connect_info(stream: IncomingStream<'_, TcpRequesterListener>) -> Self {
        *stream.remote_addr()
    }
}

#[derive(Debug)]
enum TunnelTerminal {
    Eof,
    Error(AcceptError),
}

fn tunnel_terminal_result(terminal: TunnelTerminal) -> Result<()> {
    match terminal {
        TunnelTerminal::Eof => Err(anyhow!("ngrok tunnel closed unexpectedly")),
        TunnelTerminal::Error(error) => {
            Result::<(), AcceptError>::Err(error).context("ngrok tunnel failed")
        }
    }
}

/// Axum's listener contract has no error return, so terminal tunnel state is
/// sent to the supervisor and this listener remains pending until shutdown.
struct NgrokListener {
    tunnel: Arc<Mutex<HttpTunnel>>,
    terminal: Option<oneshot::Sender<TunnelTerminal>>,
    force_close: Option<CancellationToken>,
}

impl NgrokListener {
    fn new(tunnel: Arc<Mutex<HttpTunnel>>, terminal: oneshot::Sender<TunnelTerminal>) -> Self {
        Self {
            tunnel,
            terminal: Some(terminal),
            force_close: None,
        }
    }

    fn with_force_close(mut self, force_close: CancellationToken) -> Self {
        self.force_close = Some(force_close);
        self
    }
}

fn notify_tunnel_terminal(
    sender: &mut Option<oneshot::Sender<TunnelTerminal>>,
    terminal: TunnelTerminal,
) {
    if let Some(sender) = sender.take() {
        let _ = sender.send(terminal);
    }
}

impl Listener for NgrokListener {
    type Io = ForceCloseIo<EndpointConn>;
    type Addr = RequesterId;

    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        let force_close = self
            .force_close
            .as_ref()
            .expect("ngrok listener must be configured with force-close")
            .clone();
        loop {
            let incoming = {
                let mut tunnel = self.tunnel.lock().await;
                tunnel.next().await
            };
            match incoming {
                Some(Ok(connection)) => {
                    // The SDK obtains this address from trusted ngrok
                    // connection metadata, not request forwarding headers.
                    if let Some(requester) = RequesterId::from_socket(connection.remote_addr()) {
                        return (
                            ForceCloseIo::new(connection, force_close.clone()),
                            requester,
                        );
                    }
                    // Never collapse malformed fallback addresses into a
                    // shared identity. Drop their connections before HTTP.
                    tracing::warn!("rejecting ngrok connection without a valid requester identity");
                }
                Some(Err(error)) => {
                    notify_tunnel_terminal(&mut self.terminal, TunnelTerminal::Error(error));
                    return std::future::pending().await;
                }
                None => {
                    notify_tunnel_terminal(&mut self.terminal, TunnelTerminal::Eof);
                    return std::future::pending().await;
                }
            }
        }
    }

    fn local_addr(&self) -> io::Result<Self::Addr> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "an ngrok tunnel has no local requester address",
        ))
    }
}

impl Connected<IncomingStream<'_, NgrokListener>> for RequesterId {
    fn connect_info(stream: IncomingStream<'_, NgrokListener>) -> Self {
        *stream.remote_addr()
    }
}

/// Collapse IPv4-mapped IPv6 before any security classification. Rust treats
/// these as ordinary IPv6 addresses, so calling `is_loopback`/`is_unspecified`
/// first would misclassify `::ffff:127.0.0.1` and `::ffff:0.0.0.0`.
pub(crate) fn normalize_ip(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V6(ip) => ip.to_ipv4_mapped().map_or(IpAddr::V6(ip), IpAddr::V4),
        ip => ip,
    }
}

/// The exact HTTP authorities accepted by a listener. Host validation is a
/// transport concern, not an OAuth identity source: never infer this list from
/// forwarded or other request headers.
#[derive(Clone, Debug)]
struct HostAllowlist {
    authorities: Vec<Authority>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct NormalizedAuthority {
    host: String,
    port: Option<u16>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum HostValidation {
    Allowed,
    Malformed,
    Forbidden,
}

#[derive(Clone, Copy, Debug)]
struct InvalidRequestAuthority;

impl HostAllowlist {
    fn new(bind: SocketAddr, public_origin: Option<&Url>) -> Self {
        let mut authorities = Vec::new();
        let bind_ip = normalize_ip(bind.ip());
        push_authority(&mut authorities, "localhost", bind.port(), 80);
        push_authority(&mut authorities, "127.0.0.1", bind.port(), 80);
        push_authority(&mut authorities, "::1", bind.port(), 80);
        push_authority(&mut authorities, &bind_ip.to_string(), bind.port(), 80);

        if let Some(origin) = public_origin
            && let Some((host, port, default_port)) = origin_authority(origin)
        {
            push_authority(&mut authorities, host, port, default_port);
        }

        Self::from_authorities(authorities)
    }

    fn public_origin(origin: &Url) -> Result<Self> {
        let (host, port, default_port) = origin_authority(origin).ok_or_else(|| {
            anyhow!("public HTTP origin must have an http(s) hostname and a known or explicit port")
        })?;
        let mut authorities = Vec::new();
        push_authority(&mut authorities, host, port, default_port);
        Ok(Self::from_authorities(authorities))
    }

    fn from_authorities(mut authorities: Vec<Authority>) -> Self {
        authorities.sort_by(|left, right| left.as_str().cmp(right.as_str()));
        authorities.dedup_by(|left, right| left.as_str().eq_ignore_ascii_case(right.as_str()));
        Self { authorities }
    }

    fn validate(&self, request: &Request<Body>) -> HostValidation {
        let requested = match request_authority(request) {
            Ok(requested) => requested,
            Err(InvalidRequestAuthority) => return HostValidation::Malformed,
        };
        if self.authorities.iter().any(|allowed| {
            normalized_authority(allowed, None).is_some_and(|allowed| allowed == requested)
        }) {
            HostValidation::Allowed
        } else {
            HostValidation::Forbidden
        }
    }
}

fn origin_authority(origin: &Url) -> Option<(&str, u16, u16)> {
    let host = origin.host_str()?;
    let default_port = match origin.scheme() {
        "http" => 80,
        "https" => 443,
        _ => return None,
    };
    Some((host, origin.port_or_known_default()?, default_port))
}

/// Add both valid spellings for a scheme's default port. Non-default ports
/// remain exact, including listener ports and explicit public-origin ports.
fn push_authority(authorities: &mut Vec<Authority>, host: &str, port: u16, default_port: u16) {
    let host = host
        .strip_prefix('[')
        .and_then(|host| host.strip_suffix(']'))
        .unwrap_or(host);
    let host = host
        .parse::<IpAddr>()
        .map(normalize_ip)
        .map(|ip| ip.to_string())
        .unwrap_or_else(|_| host.to_owned());
    let host = if host.contains(':') {
        format!("[{host}]")
    } else {
        host
    };
    if port == default_port {
        authorities.push(host.parse().expect("URL/socket host is a valid authority"));
    }
    authorities.push(
        format!("{host}:{port}")
            .parse()
            .expect("URL/socket host and port are a valid authority"),
    );
}

fn normalized_authority(
    authority: &Authority,
    default_port: Option<u16>,
) -> Option<NormalizedAuthority> {
    let raw = authority.as_str();
    if raw.contains('@') {
        return None;
    }
    let has_port_delimiter = if raw.starts_with('[') {
        raw.find(']')
            .and_then(|end| raw.get(end + 1..))
            .is_some_and(|suffix| suffix.starts_with(':'))
    } else {
        raw.contains(':')
    };
    // `http::uri::Authority` accepts syntactically present non-u16 ports while
    // `port()` reports them as absent. Never reinterpret those as a default.
    if has_port_delimiter && authority.port().is_none() {
        return None;
    }
    let raw_host = authority
        .host()
        .strip_prefix('[')
        .and_then(|host| host.strip_suffix(']'))
        .unwrap_or_else(|| authority.host());
    if raw_host.is_empty() {
        return None;
    }
    let host = raw_host
        .parse::<IpAddr>()
        .map(normalize_ip)
        .map(|ip| ip.to_string())
        .unwrap_or_else(|_| raw_host.to_ascii_lowercase());
    let port = match authority.port() {
        Some(port) => Some(port.as_u16()),
        None => default_port,
    };
    Some(NormalizedAuthority { host, port })
}

fn request_authority(
    request: &Request<Body>,
) -> std::result::Result<NormalizedAuthority, InvalidRequestAuthority> {
    let mut hosts = request.headers().get_all(HOST).iter();
    let host = match hosts.next() {
        Some(host) => {
            // Multiple Host fields are invalid and must not be resolved
            // differently by separate protocol/application layers.
            if hosts.next().is_some() {
                return Err(InvalidRequestAuthority);
            }
            Some(
                host.to_str()
                    .map_err(|_| InvalidRequestAuthority)?
                    .parse::<Authority>()
                    .map_err(|_| InvalidRequestAuthority)?,
            )
        }
        None => None,
    };

    // HTTP/2 carries :authority in the URI. HTTP/1 absolute-form requests can
    // also contain a URI authority and a Host field. Parse both and reject the
    // request unless their normalized host/effective-port identities agree.
    let uri = request.uri().authority().cloned();
    let host_normalized = match host.as_ref() {
        Some(authority) => {
            Some(normalized_authority(authority, None).ok_or(InvalidRequestAuthority)?)
        }
        None => None,
    };
    let uri_normalized = match uri.as_ref() {
        Some(authority) => {
            Some(normalized_authority(authority, None).ok_or(InvalidRequestAuthority)?)
        }
        None => None,
    };

    if let (Some(host), Some(uri)) = (&host, &uri) {
        // Preserve an omitted port for allowlist matching: a reverse proxy may
        // carry a public HTTPS authority over a plaintext HTTP/2 backend. Use
        // the URI scheme's default only to compare explicit/default spellings.
        let same = host_normalized == uri_normalized
            || match request.uri().scheme_str() {
                Some("http") => {
                    normalized_authority(host, Some(80)) == normalized_authority(uri, Some(80))
                }
                Some("https") => {
                    normalized_authority(host, Some(443)) == normalized_authority(uri, Some(443))
                }
                _ => false,
            };
        if !same {
            return Err(InvalidRequestAuthority);
        }
    }
    uri_normalized
        .or(host_normalized)
        .ok_or(InvalidRequestAuthority)
}

fn trusted_http_hosts(bind: SocketAddr, public_origin: Option<&Url>) -> HostAllowlist {
    HostAllowlist::new(bind, public_origin)
}

fn public_http_hosts(public_origin: &Url) -> Result<HostAllowlist> {
    HostAllowlist::public_origin(public_origin)
}

async fn run_tcp_server(
    machine: LocalMachine,
    listener: TcpListener,
    path: String,
    log: LogConfig,
    oauth_server: Option<oauth::Server>,
    allowed_hosts: HostAllowlist,
    cancellation: CancellationToken,
) -> Result<()> {
    // Axum detaches accepted connection tasks. If this serve future is aborted
    // after the graceful drain budget, dropping the guard force-closes every
    // accepted IO so no task can survive the transport runtime.
    let force_close = CancellationToken::new();
    let _force_close_on_drop = force_close.clone().drop_guard();
    let listener = TcpRequesterListener::new(listener, force_close);
    let router = mcp_router(
        machine,
        &path,
        log,
        oauth_server,
        allowed_hosts,
        cancellation.child_token(),
    );
    axum::serve(
        listener,
        router.into_make_service_with_connect_info::<RequesterId>(),
    )
    .with_graceful_shutdown(async move {
        cancellation.cancelled_owned().await;
    })
    .await
    .context("HTTP MCP server failed")
}

async fn run_ngrok_server(
    machine: LocalMachine,
    listener: NgrokListener,
    path: String,
    log: LogConfig,
    oauth_server: Option<oauth::Server>,
    allowed_hosts: HostAllowlist,
    cancellation: CancellationToken,
) -> Result<()> {
    let force_close = CancellationToken::new();
    let _force_close_on_drop = force_close.clone().drop_guard();
    let listener = listener.with_force_close(force_close);
    let router = mcp_router(
        machine,
        &path,
        log,
        oauth_server,
        allowed_hosts,
        cancellation.child_token(),
    );
    axum::serve(
        listener,
        router.into_make_service_with_connect_info::<RequesterId>(),
    )
    .with_graceful_shutdown(async move {
        cancellation.cancelled_owned().await;
    })
    .await
    .context("ngrok MCP server failed")
}

/// Drain gracefully for bounded time, then abort outstanding servers. A drain
/// timeout is a cleanup failure: accepted Axum tasks may be inside synchronous
/// work that force-closing their IO cannot preempt, so a replacement runtime
/// must not start. A cancelled caller drops the JoinSet and aborts its tasks.
async fn stop_server_tasks(
    servers: &mut JoinSet<Result<()>>,
    cancellation: &CancellationToken,
) -> Result<()> {
    stop_server_tasks_with_budget(servers, cancellation, HTTP_SHUTDOWN_BUDGET).await
}

async fn stop_server_tasks_with_budget(
    servers: &mut JoinSet<Result<()>>,
    cancellation: &CancellationToken,
    budget: std::time::Duration,
) -> Result<()> {
    cancellation.cancel();
    let drained = time::timeout(budget, async {
        let mut failure = None;
        while let Some(result) = servers.join_next().await {
            let error = match result {
                Ok(Ok(())) => continue,
                Ok(Err(error)) => error,
                Err(error) => anyhow!("HTTP server task panicked during shutdown: {error}"),
            };
            failure.get_or_insert(error);
        }
        failure
    })
    .await;

    match drained {
        Ok(Some(error)) => Err(error),
        Ok(None) => Ok(()),
        Err(_) => {
            servers.abort_all();
            while servers.join_next().await.is_some() {}
            Err(anyhow!("HTTP servers exceeded the shutdown deadline"))
        }
    }
}

fn mcp_router(
    machine: LocalMachine,
    path: &str,
    log: LogConfig,
    oauth_server: Option<oauth::Server>,
    allowed_hosts: HostAllowlist,
    cancellation: CancellationToken,
) -> Router {
    // One canonical validator must own Host semantics for MCP and OAuth.
    // Disable rmcp's textual check: it cannot normalize mapped/noncanonical IPs
    // or compare Host with HTTP/2/absolute-form URI authority consistently.
    let request_cancellation = cancellation.clone();
    let request_body_limit = machine.max_request_body_bytes();
    let policy_machine = Arc::new(machine.clone());
    let server_config = super::streamable_config(&machine, cancellation).disable_allowed_hosts();

    let service: StreamableHttpService<LocalMachine, LocalSessionManager> =
        StreamableHttpService::new(
            move || Ok(machine.clone()),
            LocalSessionManager::default().into(),
            server_config,
        );

    let mut mcp =
        Router::new()
            .nest_service(path, service)
            .layer(middleware::from_fn(
                move |request: Request<Body>, next: Next| {
                    let machine = policy_machine.clone();
                    async move {
                        guard_standard_tool_call(machine, request_body_limit, request, next).await
                    }
                },
            ));
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

    finish_http_router(router, log, allowed_hosts, request_cancellation)
}

/// Apply transport-wide middleware only after MCP and OAuth have been merged.
fn finish_http_router(
    router: Router,
    log: LogConfig,
    allowed_hosts: HostAllowlist,
    cancellation: CancellationToken,
) -> Router {
    router
        .layer(middleware::from_fn(
            move |request: Request<Body>, next: Next| {
                let log = log.clone();
                async move { log_http_request(log, request, next).await }
            },
        ))
        .layer(middleware::from_fn(
            move |request: Request<Body>, next: Next| {
                let allowed_hosts = allowed_hosts.clone();
                async move { validate_http_host(allowed_hosts, request, next).await }
            },
        ))
        // This must remain the outermost route layer: dropping `next` cancels
        // body extraction and every merged MCP/OAuth handler on shutdown.
        .layer(middleware::from_fn(
            move |request: Request<Body>, next: Next| {
                let cancellation = cancellation.clone();
                async move { cancel_http_request(cancellation, request, next).await }
            },
        ))
}

const INVALID_REQUEST_BODY_ERROR_BODY: &[u8] = br#"{"jsonrpc":"2.0","id":null,"error":{"code":-32600,"message":"Invalid or oversized request body"}}"#;

async fn guard_standard_tool_call(
    machine: Arc<LocalMachine>,
    request_body_limit: usize,
    request: Request<Body>,
    next: Next,
) -> Response {
    if request.method() != axum::http::Method::POST
        || !super::uses_standard_mcp_headers(request.headers())
        // Preserve rmcp's validation order: it rejects these before reading or
        // caching a body, so the preflight must not turn them into a 413.
        || !request
            .headers()
            .get(axum::http::header::ACCEPT)
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| {
                value.contains("application/json") && value.contains("text/event-stream")
            })
        || !request
            .headers()
            .get(axum::http::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| value.starts_with("application/json"))
    {
        return next.run(request).await;
    }

    let (mut parts, body) = request.into_parts();
    let bytes = match axum::body::to_bytes(body, request_body_limit).await {
        Ok(bytes) => bytes,
        Err(_) => {
            return fixed_jsonrpc_response(
                StatusCode::PAYLOAD_TOO_LARGE,
                INVALID_REQUEST_BODY_ERROR_BODY,
            );
        }
    };
    let body = if super::known_standard_tool_body(&bytes, &machine) {
        // Do not materialize a duplicate serde_json::Value for a known tool.
        // rmcp owns the actual validation and parse of this unchanged body.
        Body::from(bytes)
    } else if let Ok(mut message) = serde_json::from_slice(&bytes) {
        if super::canonicalize_unknown_standard_tool(&mut parts.headers, &mut message, &machine) {
            Body::from(
                serde_json::to_vec(&message)
                    .expect("a parsed JSON-RPC request can be serialized again"),
            )
        } else {
            Body::from(bytes)
        }
    } else {
        Body::from(bytes)
    };
    next.run(Request::from_parts(parts, body)).await
}

fn fixed_jsonrpc_response(status: StatusCode, body: &'static [u8]) -> Response {
    Response::builder()
        .status(status)
        .header(axum::http::header::CONTENT_TYPE, "application/json")
        .body(Body::from(body))
        .expect("fixed JSON-RPC HTTP response is valid")
}

async fn cancel_http_request(
    cancellation: CancellationToken,
    request: Request<Body>,
    next: Next,
) -> Response {
    tokio::select! {
        biased;
        _ = cancellation.cancelled_owned() => (
            StatusCode::SERVICE_UNAVAILABLE,
            "Service unavailable: server is shutting down",
        ).into_response(),
        response = next.run(request) => response,
    }
}

async fn validate_http_host(
    allowed_hosts: HostAllowlist,
    request: Request<Body>,
    next: Next,
) -> Response {
    match allowed_hosts.validate(&request) {
        HostValidation::Allowed => next.run(request).await,
        HostValidation::Malformed => (
            StatusCode::BAD_REQUEST,
            "Bad Request: Host or URI authority is invalid",
        )
            .into_response(),
        HostValidation::Forbidden => (
            StatusCode::FORBIDDEN,
            "Forbidden: Host header is not allowed",
        )
            .into_response(),
    }
}

const HTTP_LOG_METHOD_CHARS: usize = 32;
const HTTP_LOG_PATH_CHARS: usize = 160;

async fn log_http_request(log: LogConfig, request: Request<Body>, next: Next) -> Response {
    let label = http_log_label(request.method().as_str(), request.uri().path());
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

fn http_log_label(method: &str, path: &str) -> String {
    let method = bounded_log_component(method, HTTP_LOG_METHOD_CHARS);
    let path = bounded_log_component(safe_log_path(path), HTTP_LOG_PATH_CHARS);
    format!("{method} {path}")
}

fn bounded_log_component(value: &str, max_chars: usize) -> String {
    logging::truncate(value, max_chars)
        .chars()
        .map(|character| {
            if character.is_control() || matches!(character, '\u{2028}' | '\u{2029}') {
                '�'
            } else {
                character
            }
        })
        .collect()
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
    let ip = normalize_ip(bind.ip());
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

#[cfg(test)]
mod tests {
    use super::*;
    use axum::extract::{ConnectInfo, connect_info::MockConnectInfo};

    #[tokio::test]
    async fn startup_operations_race_cancellation() {
        let active = CancellationToken::new();
        assert_eq!(startup_or_cancelled(&active, async { 42 }).await, Some(42));

        let already_cancelled = CancellationToken::new();
        already_cancelled.cancel();
        assert_eq!(
            startup_or_cancelled(&already_cancelled, async { 42 }).await,
            None
        );

        let cancellation = CancellationToken::new();
        let waiter = {
            let cancellation = cancellation.clone();
            tokio::spawn(async move {
                startup_or_cancelled(&cancellation, std::future::pending::<()>()).await
            })
        };
        tokio::task::yield_now().await;
        cancellation.cancel();
        let result = time::timeout(std::time::Duration::from_secs(1), waiter)
            .await
            .expect("startup operation ignored cancellation")
            .unwrap();
        assert_eq!(result, None);
    }

    #[tokio::test]
    async fn standard_unknown_tool_guard_canonicalizes_one_bounded_downstream_key() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        use tower::ServiceExt as _;

        const BODY_LIMIT: usize = 2 * 1024;
        const UNKNOWN_REQUESTS: usize = 512;
        const VERSION: &str = "2026-07-28";

        let temp = tempfile::tempdir().unwrap();
        let machine = crate::mcp::test_machine(temp.path().canonicalize().unwrap());
        let downstream_calls = Arc::new(AtomicUsize::new(0));
        let handler_calls = downstream_calls.clone();
        let guard_machine = Arc::new(machine.clone());
        let router = Router::new()
            .route(
                "/mcp",
                axum::routing::post(move |request: Request<Body>| {
                    let handler_calls = handler_calls.clone();
                    async move {
                        handler_calls.fetch_add(1, Ordering::Relaxed);
                        let seen_name = request
                            .headers()
                            .get("Mcp-Name")
                            .and_then(|value| value.to_str().ok())
                            .unwrap_or("")
                            .to_owned();
                        let body = axum::body::to_bytes(request.into_body(), BODY_LIMIT + 1)
                            .await
                            .unwrap();
                        Response::builder()
                            .status(StatusCode::OK)
                            .header("X-Seen-Mcp-Name", seen_name)
                            .body(Body::from(body))
                            .unwrap()
                    }
                }),
            )
            .layer(middleware::from_fn(move |request, next| {
                let machine = guard_machine.clone();
                async move { guard_standard_tool_call(machine, BODY_LIMIT, request, next).await }
            }));

        let make_request = |version: &'static str, name: &str, body: Vec<u8>| {
            Request::builder()
                .method("POST")
                .uri("/mcp")
                .header("MCP-Protocol-Version", version)
                .header("Mcp-Method", "tools/call")
                .header("Mcp-Name", name)
                .header("Content-Type", "application/json")
                .header("Accept", "application/json, text/event-stream")
                .body(Body::from(body))
                .unwrap()
        };

        let mut last_unknown = (String::new(), Vec::new());
        for index in 0..UNKNOWN_REQUESTS {
            let name = format!(
                "unknown-{index:04}-{}",
                "x".repeat(BODY_LIMIT.saturating_sub(280))
            );
            let body = serde_json::to_vec(&serde_json::json!({
                "jsonrpc": "2.0",
                "id": index,
                "method": "tools/call",
                "params": {
                    "name": name,
                    "arguments": {},
                    "_meta": {
                        "io.modelcontextprotocol/protocolVersion": VERSION,
                        "io.modelcontextprotocol/clientInfo": {"name": "test", "version": "1.0"},
                        "io.modelcontextprotocol/clientCapabilities": {}
                    }
                }
            }))
            .unwrap();
            assert!(body.len() <= BODY_LIMIT);
            assert!(body.len() > BODY_LIMIT - 384);
            last_unknown = (name.clone(), body.clone());

            let response = router
                .clone()
                .oneshot(make_request(VERSION, &name, body))
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            assert_eq!(
                response.headers()["X-Seen-Mcp-Name"],
                super::super::UNKNOWN_TOOL_SENTINEL
            );
            let response_body = axum::body::to_bytes(response.into_body(), BODY_LIMIT)
                .await
                .unwrap();
            let canonical: serde_json::Value = serde_json::from_slice(&response_body).unwrap();
            assert_eq!(canonical["id"], index);
            assert_eq!(
                canonical["params"]["name"],
                super::super::UNKNOWN_TOOL_SENTINEL
            );
            assert!(
                !response_body
                    .windows(name.len())
                    .any(|window| window == name.as_bytes())
            );
        }
        assert_eq!(downstream_calls.load(Ordering::Relaxed), UNKNOWN_REQUESTS);

        // A known body at the exact ceiling is rebuilt byte-for-byte unchanged.
        let mut exact_body = serde_json::to_vec(&serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": {
                "name": "read",
                "arguments": {"path": "file"},
                "_meta": {
                    "io.modelcontextprotocol/protocolVersion": VERSION,
                    "io.modelcontextprotocol/clientInfo": {"name": "test", "version": "1.0"},
                    "io.modelcontextprotocol/clientCapabilities": {}
                }
            }
        }))
        .unwrap();
        exact_body.resize(BODY_LIMIT, b' ');
        let response = router
            .clone()
            .oneshot(make_request(VERSION, "read", exact_body.clone()))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()["X-Seen-Mcp-Name"], "read");
        assert_eq!(
            axum::body::to_bytes(response.into_body(), BODY_LIMIT)
                .await
                .unwrap(),
            exact_body
        );

        // Older protocol traffic is untouched, while one byte over the
        // standard-header ceiling never reaches the downstream rmcp service.
        let response = router
            .clone()
            .oneshot(make_request(
                "2025-11-25",
                &last_unknown.0,
                last_unknown.1.clone(),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()["X-Seen-Mcp-Name"], last_unknown.0);
        assert_eq!(
            axum::body::to_bytes(response.into_body(), BODY_LIMIT)
                .await
                .unwrap(),
            last_unknown.1
        );
        let mut oversized = exact_body;
        oversized.push(b' ');
        let response = router
            .oneshot(make_request(VERSION, "read", oversized))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
        assert_eq!(
            downstream_calls.load(Ordering::Relaxed),
            UNKNOWN_REQUESTS + 2
        );

        // The production MCP router lets rmcp generate the normal correlated
        // unknown-tool response after canonicalization.
        let production = mcp_router(
            machine,
            "/mcp",
            LogConfig::default(),
            None,
            trusted_http_hosts("127.0.0.1:3000".parse().unwrap(), None),
            CancellationToken::new(),
        )
        .layer(MockConnectInfo(RequesterId::Local));
        let body = serde_json::to_vec(&serde_json::json!({
            "jsonrpc": "2.0",
            "id": 73,
            "method": "tools/call",
            "params": {
                "name": "production-unknown",
                "arguments": {},
                "_meta": {
                    "io.modelcontextprotocol/protocolVersion": VERSION,
                    "io.modelcontextprotocol/clientInfo": {"name": "test", "version": "1.0"},
                    "io.modelcontextprotocol/clientCapabilities": {}
                }
            }
        }))
        .unwrap();
        let response = production
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/mcp")
                    .header(HOST, "127.0.0.1:3000")
                    .header("MCP-Protocol-Version", VERSION)
                    .header("Mcp-Method", "tools/call")
                    .header("Mcp-Name", "production-unknown")
                    .header("Content-Type", "application/json")
                    .header("Accept", "application/json, text/event-stream")
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = response.status();
        let response_body = axum::body::to_bytes(response.into_body(), 1024)
            .await
            .unwrap();
        let error: serde_json::Value = serde_json::from_slice(&response_body).unwrap();
        assert_eq!(status, StatusCode::BAD_REQUEST, "{error}");
        assert_eq!(error["id"], 73);
        assert_eq!(error["error"]["code"], -32602);
        assert!(
            !response_body
                .windows("production-unknown".len())
                .any(|window| window == b"production-unknown")
        );
    }

    #[tokio::test]
    async fn canonicalized_unknown_tools_retain_rmcp_validation_errors() {
        use tower::ServiceExt as _;

        const VERSION: &str = "2026-07-28";
        const TOOL: &str = "validation-unknown";

        fn request(
            id: i64,
            protocol_header: Option<&str>,
            meta_version: Option<&str>,
            include_method: bool,
            names: &[&str],
            content_type: &str,
            accept: &str,
        ) -> Request<Body> {
            let mut params = serde_json::json!({"name": TOOL, "arguments": {}});
            if let Some(version) = meta_version {
                params["_meta"] = serde_json::json!({
                    "io.modelcontextprotocol/protocolVersion": version,
                    "io.modelcontextprotocol/clientInfo": {"name": "test", "version": "1.0"},
                    "io.modelcontextprotocol/clientCapabilities": {}
                });
            }
            let body = serde_json::to_vec(&serde_json::json!({
                "jsonrpc": "2.0",
                "id": id,
                "method": "tools/call",
                "params": params
            }))
            .unwrap();
            let mut builder = Request::builder()
                .method("POST")
                .uri("/mcp")
                .header(HOST, "127.0.0.1:3000")
                .header("Content-Type", content_type)
                .header(
                    "Accept",
                    if accept == "application/json" {
                        "application/json, text/event-stream"
                    } else {
                        accept
                    },
                );
            if let Some(version) = protocol_header {
                builder = builder.header("MCP-Protocol-Version", version);
            }
            if include_method {
                builder = builder.header("Mcp-Method", "tools/call");
            }
            let mut request = builder.body(Body::from(body)).unwrap();
            for name in names {
                request
                    .headers_mut()
                    .append("Mcp-Name", axum::http::HeaderValue::from_str(name).unwrap());
            }
            request
        }

        let temp = tempfile::tempdir().unwrap();
        let machine = crate::mcp::test_machine(temp.path().canonicalize().unwrap());
        let router = mcp_router(
            machine,
            "/mcp",
            LogConfig::default(),
            None,
            trusted_http_hosts("127.0.0.1:3000".parse().unwrap(), None),
            CancellationToken::new(),
        )
        .layer(MockConnectInfo(RequesterId::Local));

        let cases = [
            (
                "missing Mcp-Method",
                101,
                request(
                    101,
                    Some(VERSION),
                    Some(VERSION),
                    false,
                    &[TOOL],
                    "application/json",
                    "application/json",
                ),
                -32020,
            ),
            (
                "missing Mcp-Name",
                102,
                request(
                    102,
                    Some(VERSION),
                    Some(VERSION),
                    true,
                    &[],
                    "application/json",
                    "application/json",
                ),
                -32020,
            ),
            (
                "mismatched Mcp-Name",
                103,
                request(
                    103,
                    Some(VERSION),
                    Some(VERSION),
                    true,
                    &["other"],
                    "application/json",
                    "application/json",
                ),
                -32020,
            ),
            (
                "mismatch already equal to body sentinel",
                104,
                request(
                    104,
                    Some(VERSION),
                    Some(VERSION),
                    true,
                    &[super::super::UNKNOWN_TOOL_SENTINEL],
                    "application/json",
                    "application/json",
                ),
                -32020,
            ),
            (
                "invalid base64 Mcp-Name",
                105,
                request(
                    105,
                    Some(VERSION),
                    Some(VERSION),
                    true,
                    &["=?base64?%%%?="],
                    "application/json",
                    "application/json",
                ),
                -32020,
            ),
            (
                "mismatched protocol metadata",
                106,
                request(
                    106,
                    Some(VERSION),
                    Some("2025-11-25"),
                    true,
                    &[TOOL],
                    "application/json",
                    "application/json",
                ),
                -32020,
            ),
            (
                "missing protocol header",
                107,
                request(
                    107,
                    None,
                    Some(VERSION),
                    true,
                    &[TOOL],
                    "application/json",
                    "application/json",
                ),
                -32020,
            ),
            (
                "missing protocol metadata",
                111,
                request(
                    111,
                    Some(VERSION),
                    None,
                    true,
                    &[TOOL],
                    "application/json",
                    "application/json",
                ),
                -32602,
            ),
        ];
        for (label, expected_id, request, expected_code) in cases {
            let response = router.clone().oneshot(request).await.unwrap();
            assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{label}");
            let body = axum::body::to_bytes(response.into_body(), 2048)
                .await
                .unwrap();
            let error: serde_json::Value = serde_json::from_slice(&body).unwrap();
            assert_eq!(error["id"], expected_id, "{label}: {error}");
            assert_eq!(error["error"]["code"], expected_code, "{label}: {error}");
        }

        for (label, request, expected_status) in [
            (
                "content type",
                request(
                    108,
                    Some(VERSION),
                    Some(VERSION),
                    true,
                    &[TOOL],
                    "text/plain",
                    "application/json",
                ),
                StatusCode::UNSUPPORTED_MEDIA_TYPE,
            ),
            (
                "accept",
                request(
                    109,
                    Some(VERSION),
                    Some(VERSION),
                    true,
                    &[TOOL],
                    "application/json",
                    "text/plain",
                ),
                StatusCode::NOT_ACCEPTABLE,
            ),
        ] {
            let response = router.clone().oneshot(request).await.unwrap();
            assert_eq!(response.status(), expected_status, "{label}");
        }

        let duplicate = request(
            110,
            Some(VERSION),
            Some(VERSION),
            true,
            &[TOOL, TOOL],
            "application/json",
            "application/json",
        );
        // RequestBuilder may coalesce in future versions; ensure the duplicate
        // singleton reaches rmcp as two field values.
        assert_eq!(duplicate.headers().get_all("Mcp-Name").iter().count(), 2);
        let response = router.oneshot(duplicate).await.unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let body = axum::body::to_bytes(response.into_body(), 2048)
            .await
            .unwrap();
        let error: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(error["id"], 110);
        assert_eq!(error["error"]["code"], -32020);
    }

    #[tokio::test]
    async fn force_close_wakes_pending_io_and_drops_connection() {
        use tokio::io::AsyncReadExt as _;

        struct PendingIo(Option<oneshot::Sender<()>>);

        impl Drop for PendingIo {
            fn drop(&mut self) {
                if let Some(sender) = self.0.take() {
                    let _ = sender.send(());
                }
            }
        }

        impl AsyncRead for PendingIo {
            fn poll_read(
                self: Pin<&mut Self>,
                _context: &mut TaskContext<'_>,
                _buffer: &mut ReadBuf<'_>,
            ) -> Poll<io::Result<()>> {
                Poll::Pending
            }
        }

        let force_close = CancellationToken::new();
        let force_close_owner = force_close.clone().drop_guard();
        let (dropped_tx, dropped_rx) = oneshot::channel();
        let task = tokio::spawn(async move {
            let mut io = ForceCloseIo::new(PendingIo(Some(dropped_tx)), force_close);
            let mut byte = [0_u8; 1];
            io.read(&mut byte).await.unwrap_err()
        });
        tokio::task::yield_now().await;
        assert!(!task.is_finished());

        drop(force_close_owner);
        let error = time::timeout(std::time::Duration::from_secs(1), task)
            .await
            .expect("force-closed IO did not wake")
            .unwrap();
        assert_eq!(error.kind(), io::ErrorKind::ConnectionAborted);
        time::timeout(std::time::Duration::from_secs(1), dropped_rx)
            .await
            .expect("force-closed IO outlived its owner")
            .unwrap();
    }

    #[tokio::test]
    async fn force_close_rejects_write_flush_and_shutdown() {
        use tokio::io::AsyncWriteExt as _;

        let force_close = CancellationToken::new();
        force_close.cancel();
        let (connection, _peer) = tokio::io::duplex(8);
        let mut io = ForceCloseIo::new(connection, force_close);

        for error in [
            io.write_all(b"x").await.unwrap_err(),
            io.flush().await.unwrap_err(),
            io.shutdown().await.unwrap_err(),
        ] {
            assert_eq!(error.kind(), io::ErrorKind::ConnectionAborted);
        }
    }

    #[test]
    fn ngrok_cleanup_failure_overrides_primary_to_prevent_restart() {
        let cleanup = merge_primary_and_cleanup(
            Err(anyhow!("primary transport failure")),
            Err(anyhow!("cleanup failure")),
        )
        .unwrap_err();
        assert_eq!(cleanup.to_string(), "cleanup failure");

        let cleanup =
            merge_primary_and_cleanup(Ok(()), Err(anyhow!("cleanup failure"))).unwrap_err();
        assert_eq!(cleanup.to_string(), "cleanup failure");
        assert!(merge_primary_and_cleanup(Ok(()), Ok(())).is_ok());
    }

    #[test]
    fn server_cleanup_failure_precedes_ngrok_cleanup_failure() {
        let error = merge_server_and_ngrok_cleanup(
            Err(anyhow!("server cleanup failure")),
            Err(anyhow!("ngrok cleanup failure")),
        )
        .unwrap_err();
        assert_eq!(error.to_string(), "server cleanup failure");
    }

    #[tokio::test]
    async fn ngrok_cleanup_sequences_tunnel_then_session_and_preserves_results() {
        assert_eq!(NGROK_TUNNEL_CLOSE_TIMEOUT.as_secs(), 4);
        assert_eq!(NGROK_SESSION_CLOSE_TIMEOUT.as_secs(), 1);
        assert_eq!(
            NGROK_TUNNEL_CLOSE_TIMEOUT.as_secs() + NGROK_SESSION_CLOSE_TIMEOUT.as_secs(),
            HTTP_SHUTDOWN_BUDGET_SECS
        );

        let events = Arc::new(std::sync::Mutex::new(Vec::new()));
        let tunnel_events = events.clone();
        let session_events = events.clone();
        let error = run_ngrok_cleanup(
            move || async move {
                tunnel_events.lock().unwrap().push("tunnel-start");
                tokio::task::yield_now().await;
                tunnel_events.lock().unwrap().push("tunnel-end");
                Err(anyhow!("tunnel cleanup failure"))
            },
            move || async move {
                session_events.lock().unwrap().push("session");
                Err(anyhow!("session cleanup failure"))
            },
        )
        .await
        .unwrap_err();
        assert_eq!(error.to_string(), "tunnel cleanup failure");
        assert_eq!(
            *events.lock().unwrap(),
            ["tunnel-start", "tunnel-end", "session"]
        );

        let error = run_ngrok_cleanup(
            || async { Ok(()) },
            || async { Err(anyhow!("session cleanup failure")) },
        )
        .await
        .unwrap_err();
        assert_eq!(error.to_string(), "session cleanup failure");
    }

    #[tokio::test]
    async fn shutdown_cancels_and_drains_child_server_tasks() {
        let cancellation = CancellationToken::new();
        let child = cancellation.child_token();
        let mut servers = JoinSet::new();
        servers.spawn(async move {
            child.cancelled().await;
            Ok(())
        });
        stop_server_tasks(&mut servers, &cancellation)
            .await
            .unwrap();
        assert!(cancellation.is_cancelled());
        assert!(servers.is_empty());
    }

    #[test]
    fn only_an_ngrok_authentication_answer_blames_the_token() {
        use ngrok::session::{ConnectError, RpcError};

        let token_hint = "check NGROK_AUTHTOKEN";
        let rejected = ngrok_connect_error(ConnectError::Auth(RpcError::Response(
            "authentication failed\nERR_NGROK_107".into(),
        )));
        assert!(rejected.to_string().contains(token_hint));

        for unreachable in [
            ConnectError::Auth(RpcError::Send(io::Error::other("connection reset"))),
            ConnectError::Tcp(io::Error::other("network unreachable")),
        ] {
            let error = ngrok_connect_error(unreachable);
            assert_eq!(error.to_string(), "failed to connect to ngrok");
        }
        assert_eq!(
            crate::recovery::Recovery::of(&rejected),
            crate::recovery::Recovery::Process
        );
    }

    #[tokio::test]
    async fn forced_server_abort_is_a_restart_blocking_cleanup_failure() {
        let cancellation = CancellationToken::new();
        let mut servers = JoinSet::new();
        servers.spawn(std::future::pending::<Result<()>>());

        let error = stop_server_tasks_with_budget(
            &mut servers,
            &cancellation,
            std::time::Duration::from_millis(1),
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains("shutdown deadline"));
        assert!(servers.is_empty());
    }

    #[tokio::test]
    async fn aborted_supervisor_cancels_child_calls_and_does_not_detach_servers() {
        let cancellation = CancellationToken::new();
        let (started, ready) = tokio::sync::oneshot::channel();
        let (alive, dropped) = tokio::sync::oneshot::channel::<()>();
        let parent = cancellation.clone();
        let supervisor = tokio::spawn(async move {
            let _cancel_on_drop = parent.clone().drop_guard();
            let mut servers = JoinSet::new();
            servers.spawn(async move {
                let _lifetime = alive;
                std::future::pending::<Result<()>>().await
            });
            // Ensure the child started before checking cancellation.
            tokio::task::yield_now().await;
            started.send(()).unwrap();
            std::future::pending::<()>().await;
        });
        ready.await.unwrap();
        supervisor.abort();
        assert!(supervisor.await.is_err());
        assert!(cancellation.is_cancelled());
        tokio::time::timeout(std::time::Duration::from_secs(2), dropped)
            .await
            .expect("child task was not aborted")
            .expect_err("child task should drop without sending");
    }

    #[tokio::test]
    async fn outer_cancellation_drops_in_flight_route_future() {
        use tower::ServiceExt as _;

        struct DropSignal(Option<oneshot::Sender<()>>);

        impl Drop for DropSignal {
            fn drop(&mut self) {
                if let Some(sender) = self.0.take() {
                    let _ = sender.send(());
                }
            }
        }

        let (started_tx, started_rx) = oneshot::channel();
        let (dropped_tx, dropped_rx) = oneshot::channel();
        let signals = std::sync::Arc::new(std::sync::Mutex::new(Some((started_tx, dropped_tx))));
        let route_signals = signals.clone();
        let routes = Router::new().route(
            "/pending",
            axum::routing::get(move || {
                let (started, dropped) = route_signals
                    .lock()
                    .unwrap()
                    .take()
                    .expect("pending handler called more than once");
                async move {
                    let _drop_signal = DropSignal(Some(dropped));
                    let _ = started.send(());
                    std::future::pending::<StatusCode>().await
                }
            }),
        );
        let cancellation = CancellationToken::new();
        let router = finish_http_router(
            routes,
            LogConfig::default(),
            trusted_http_hosts("127.0.0.1:3000".parse().unwrap(), None),
            cancellation.clone(),
        );
        let request_task = tokio::spawn(
            router.oneshot(
                Request::builder()
                    .uri("/pending")
                    .header(HOST, "127.0.0.1:3000")
                    .body(Body::empty())
                    .unwrap(),
            ),
        );

        started_rx.await.unwrap();
        cancellation.cancel();
        let response = time::timeout(std::time::Duration::from_secs(1), request_task)
            .await
            .expect("cancelled route did not return")
            .unwrap()
            .unwrap();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        time::timeout(std::time::Duration::from_secs(1), dropped_rx)
            .await
            .expect("in-flight route future was not dropped")
            .unwrap();
    }

    #[tokio::test]
    async fn direct_requester_comes_from_peer_not_forwarding_headers() {
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        let address = listener.local_addr().unwrap();
        let listener = TcpRequesterListener::new(listener, CancellationToken::new());
        let router = Router::new().route(
            "/",
            axum::routing::get(
                |ConnectInfo(requester): ConnectInfo<RequesterId>| async move {
                    if requester == RequesterId::Local {
                        StatusCode::NO_CONTENT
                    } else {
                        StatusCode::INTERNAL_SERVER_ERROR
                    }
                },
            ),
        );
        let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
        let server = tokio::spawn(async move {
            axum::serve(
                listener,
                router.into_make_service_with_connect_info::<RequesterId>(),
            )
            .with_graceful_shutdown(async move {
                let _ = shutdown_rx.await;
            })
            .await
            .unwrap();
        });

        let response = reqwest::Client::builder()
            .no_proxy()
            .build()
            .unwrap()
            .get(format!("http://{address}/"))
            .header("Forwarded", "for=198.51.100.7:4321")
            .header("X-Forwarded-For", "198.51.100.8")
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NO_CONTENT);

        shutdown_tx.send(()).unwrap();
        server.await.unwrap();
    }

    #[tokio::test]
    async fn tunnel_terminal_notification_is_one_shot() {
        let (terminal_tx, terminal_rx) = oneshot::channel();
        let mut terminal_tx = Some(terminal_tx);
        notify_tunnel_terminal(&mut terminal_tx, TunnelTerminal::Eof);
        assert!(terminal_tx.is_none());
        assert!(matches!(terminal_rx.await, Ok(TunnelTerminal::Eof)));
    }

    #[tokio::test]
    async fn http_host_validation_accepts_only_configured_public_origin() {
        use axum::http::StatusCode;
        use tower::ServiceExt as _;
        let temp = tempfile::tempdir().unwrap();
        let machine = crate::mcp::test_machine(temp.path().canonicalize().unwrap());
        let public = Url::parse("https://my-dotlink.example").unwrap();
        let router = mcp_router(
            machine,
            "/mcp",
            LogConfig::default(),
            None,
            trusted_http_hosts("127.0.0.1:3000".parse().unwrap(), Some(&public)),
            CancellationToken::new(),
        )
        .layer(MockConnectInfo(RequesterId::Local));
        for (host, expected) in [
            ("my-dotlink.example", StatusCode::METHOD_NOT_ALLOWED),
            ("other.example", StatusCode::FORBIDDEN),
        ] {
            let request = Request::builder()
                .method("GET")
                .uri("/mcp")
                .header("Host", host)
                .body(Body::empty())
                .unwrap();
            let result = router.clone().oneshot(request).await.unwrap();
            assert_eq!(result.status(), expected, "host={host}");
        }
    }

    #[tokio::test]
    async fn canonical_outer_host_validation_is_the_only_mcp_host_gate() {
        use tower::ServiceExt as _;

        let temp = tempfile::tempdir().unwrap();
        let machine = crate::mcp::test_machine(temp.path().canonicalize().unwrap());
        let router = mcp_router(
            machine,
            "/mcp",
            LogConfig::default(),
            None,
            trusted_http_hosts("[::ffff:127.0.0.1]:3000".parse().unwrap(), None),
            CancellationToken::new(),
        )
        .layer(MockConnectInfo(RequesterId::Local));
        for host in ["127.0.0.1:3000", "[::ffff:127.0.0.1]:3000"] {
            let response = router
                .clone()
                .oneshot(
                    Request::builder()
                        .method("GET")
                        .uri("/mcp")
                        .header(HOST, host)
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::METHOD_NOT_ALLOWED, "{host}");
        }
    }

    #[tokio::test]
    async fn host_validation_wraps_merged_mcp_and_oauth_routes() {
        use tower::ServiceExt as _;

        let public = Url::parse("https://my-dotlink.example").unwrap();
        let requester_aware = |ConnectInfo(_): ConnectInfo<RequesterId>| async { StatusCode::OK };
        let routes = Router::new()
            .route("/mcp", axum::routing::get(requester_aware))
            .route(
                "/.well-known/oauth-protected-resource",
                axum::routing::get(requester_aware),
            );
        let router = finish_http_router(
            routes,
            LogConfig::default(),
            trusted_http_hosts("127.0.0.1:3000".parse().unwrap(), Some(&public)),
            CancellationToken::new(),
        )
        .layer(MockConnectInfo(RequesterId::Local));

        for path in ["/mcp", "/.well-known/oauth-protected-resource"] {
            for (host, expected) in [
                ("my-dotlink.example", StatusCode::OK),
                ("arbitrary.example", StatusCode::FORBIDDEN),
            ] {
                let request = Request::builder()
                    .uri(path)
                    .header(HOST, host)
                    // Forwarded values are intentionally irrelevant.
                    .header("X-Forwarded-Host", "my-dotlink.example")
                    .body(Body::empty())
                    .unwrap();
                let result = router.clone().oneshot(request).await.unwrap();
                assert_eq!(result.status(), expected, "path={path}, host={host}");
            }

            // Absolute-form HTTP/1 and HTTP/2 :authority are represented by
            // the URI authority. Host and URI must agree even when each value
            // is independently allowlisted; default ports and case normalize.
            let suffix = if path == "/mcp" {
                "/mcp"
            } else {
                "/.well-known/oauth-protected-resource"
            };
            for scheme in ["https", "http"] {
                let authority_only = Request::builder()
                    .uri(format!("{scheme}://my-dotlink.example{suffix}"))
                    .body(Body::empty())
                    .unwrap();
                assert_eq!(
                    router
                        .clone()
                        .oneshot(authority_only)
                        .await
                        .unwrap()
                        .status(),
                    StatusCode::OK,
                    "URI authority was not validated for path={path}, scheme={scheme}"
                );
            }

            for (uri, host, expected) in [
                (
                    format!("https://MY-DOTLINK.EXAMPLE{suffix}"),
                    "my-dotlink.example:443",
                    StatusCode::OK,
                ),
                (
                    format!("https://my-dotlink.example{suffix}"),
                    "localhost:3000",
                    StatusCode::BAD_REQUEST,
                ),
                (
                    format!("https://my-dotlink.example:444{suffix}"),
                    "my-dotlink.example:443",
                    StatusCode::BAD_REQUEST,
                ),
            ] {
                let request = Request::builder()
                    .uri(uri.as_str())
                    .header(HOST, host)
                    .body(Body::empty())
                    .unwrap();
                let result = router.clone().oneshot(request).await.unwrap();
                assert_eq!(
                    result.status(),
                    expected,
                    "path={path}, uri={uri}, host={host}"
                );
            }

            let missing = Request::builder().uri(path).body(Body::empty()).unwrap();
            assert_eq!(
                router.clone().oneshot(missing).await.unwrap().status(),
                StatusCode::BAD_REQUEST,
                "missing authority was accepted for path={path}"
            );

            let malformed = Request::builder()
                .uri(path)
                .header(HOST, "my-dotlink.example:garbage")
                .body(Body::empty())
                .unwrap();
            assert_eq!(
                router.clone().oneshot(malformed).await.unwrap().status(),
                StatusCode::BAD_REQUEST,
                "malformed authority was not rejected for path={path}"
            );

            let mut duplicate = Request::builder()
                .uri(path)
                .header(HOST, "my-dotlink.example")
                .body(Body::empty())
                .unwrap();
            duplicate.headers_mut().append(
                HOST,
                axum::http::HeaderValue::from_static("my-dotlink.example"),
            );
            assert_eq!(
                router.clone().oneshot(duplicate).await.unwrap().status(),
                StatusCode::BAD_REQUEST,
                "duplicate Host was not rejected for path={path}"
            );
        }
    }

    #[test]
    fn trusted_http_hosts_preserve_ip_and_port_semantics() {
        fn is_allowed(hosts: &HostAllowlist, host: &str) -> bool {
            let request = Request::builder()
                .uri("/")
                .header(HOST, host)
                .body(Body::empty())
                .unwrap();
            hosts.validate(&request) == HostValidation::Allowed
        }

        let public = Url::parse("https://my-dotlink.example:8443").unwrap();
        let hosts = trusted_http_hosts("192.168.1.50:3000".parse().unwrap(), Some(&public));
        assert!(is_allowed(&hosts, "192.168.1.50:3000"));
        assert!(!is_allowed(&hosts, "192.168.1.50"));
        assert!(is_allowed(&hosts, "my-dotlink.example:8443"));
        assert!(!is_allowed(&hosts, "my-dotlink.example"));
        assert!(!is_allowed(&hosts, "my-dotlink.example:443"));
        assert!(!is_allowed(&hosts, "arbitrary.example:8443"));
        for malformed in [
            "my-dotlink.example:",
            "my-dotlink.example:garbage",
            "my-dotlink.example:99999",
        ] {
            assert!(!is_allowed(&hosts, malformed), "{malformed}");
        }

        let ipv6 = trusted_http_hosts("[::1]:3000".parse().unwrap(), None);
        assert!(is_allowed(&ipv6, "[::1]:3000"));
        assert!(!is_allowed(&ipv6, "[::1]:3001"));

        let mapped = trusted_http_hosts("[::ffff:127.0.0.1]:3000".parse().unwrap(), None);
        assert!(is_allowed(&mapped, "127.0.0.1:3000"));
        assert!(is_allowed(&mapped, "[::ffff:127.0.0.1]:3000"));

        let default_https = Url::parse("https://public.example").unwrap();
        let hosts = trusted_http_hosts("127.0.0.1:3000".parse().unwrap(), Some(&default_https));
        assert!(is_allowed(&hosts, "public.example"));
        assert!(is_allowed(&hosts, "public.example:443"));
        assert!(!is_allowed(&hosts, "public.example:444"));
        for malformed in [
            "public.example:",
            "public.example:garbage",
            "public.example:99999",
        ] {
            assert!(!is_allowed(&hosts, malformed), "{malformed}");
        }
    }

    #[test]
    fn ngrok_hosts_are_public_authority_only() {
        fn is_allowed(hosts: &HostAllowlist, host: &str) -> bool {
            let request = Request::builder()
                .uri("/")
                .header(HOST, host)
                .body(Body::empty())
                .unwrap();
            hosts.validate(&request) == HostValidation::Allowed
        }

        let hosts = public_http_hosts(&Url::parse("https://public.ngrok.app").unwrap()).unwrap();
        assert!(is_allowed(&hosts, "public.ngrok.app"));
        assert!(is_allowed(&hosts, "public.ngrok.app:443"));
        for host in [
            "localhost",
            "localhost:3000",
            "127.0.0.1:3000",
            "public.ngrok.app:80",
            "other.ngrok.app",
        ] {
            assert!(!is_allowed(&hosts, host), "host={host}");
        }

        let nondefault =
            public_http_hosts(&Url::parse("https://public.ngrok.app:8443").unwrap()).unwrap();
        assert!(is_allowed(&nondefault, "public.ngrok.app:8443"));
        assert!(!is_allowed(&nondefault, "public.ngrok.app"));
        assert!(!is_allowed(&nondefault, "public.ngrok.app:443"));
    }

    #[test]
    fn http_log_labels_are_redacted_sanitized_and_bounded() {
        assert_eq!(http_log_label("GET", "/mcp"), "GET /mcp");
        assert_eq!(
            http_log_label("POST", "/mcp/0123456789abcdef"),
            "POST /mcp/<ephemeral>"
        );
        assert_eq!(http_log_label("GET", "/health"), "GET /health");

        let long_path = format!("/{}", "x".repeat(16 * 1024));
        let label = http_log_label("GET", &long_path);
        let (_, logged_path) = label.split_once(' ').unwrap();
        assert_eq!(logged_path.chars().count(), HTTP_LOG_PATH_CHARS + 1);
        assert!(logged_path.ends_with('…'));
        assert!(!label.contains(&"x".repeat(HTTP_LOG_PATH_CHARS + 1)));

        let long_method = "M".repeat(1024);
        let label = http_log_label(&long_method, "/health");
        let (logged_method, _) = label.split_once(' ').unwrap();
        assert_eq!(logged_method.chars().count(), HTTP_LOG_METHOD_CHARS + 1);
        assert!(logged_method.ends_with('…'));

        let sanitized = http_log_label("BAD\nMETHOD", "/bad\rpath\u{2028}tail");
        assert!(!sanitized.chars().any(char::is_control));
        assert!(!sanitized.contains('\u{2028}'));
        assert!(!sanitized.contains('\u{2029}'));
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
        assert_eq!(
            loopback_public_base("[::ffff:127.0.0.1]:4321".parse().unwrap())
                .unwrap()
                .as_str(),
            "http://127.0.0.1:4321/"
        );
        assert!(loopback_public_base("0.0.0.0:4321".parse().unwrap()).is_err());
        assert!(loopback_public_base("[::ffff:0.0.0.0]:4321".parse().unwrap()).is_err());
    }
}
