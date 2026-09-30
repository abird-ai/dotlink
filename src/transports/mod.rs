use std::{error::Error, fmt};

use anyhow::{Context, Result, anyhow};
use tokio::{task::JoinSet, time};
use tokio_util::sync::CancellationToken;

use crate::mcp::LocalMachine;

#[derive(Debug)]
pub struct RuntimeRestartRequested {
    reason: String,
    reset_backoff: bool,
}

impl RuntimeRestartRequested {
    pub fn new(reason: String, reset_backoff: bool) -> Self {
        Self {
            reason,
            reset_backoff,
        }
    }

    pub fn reset_backoff(&self) -> bool {
        self.reset_backoff
    }
}

impl fmt::Display for RuntimeRestartRequested {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.reason)
    }
}

impl Error for RuntimeRestartRequested {}

pub fn runtime_restart_request(error: &anyhow::Error) -> Option<&RuntimeRestartRequested> {
    error.downcast_ref::<RuntimeRestartRequested>()
}

pub mod http;
pub mod openai;
pub mod stdio;

#[cfg(not(test))]
const TRANSPORT_SHUTDOWN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);
#[cfg(test)]
const TRANSPORT_SHUTDOWN_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(10);

pub struct ActiveTransports {
    pub openai: Option<openai::Config>,
    pub stdio: bool,
    pub http: Option<http::Config>,
}

impl ActiveTransports {
    pub fn is_empty(&self) -> bool {
        self.openai.is_none() && !self.stdio && self.http.is_none()
    }

    pub fn names(&self) -> Vec<&'static str> {
        let mut names = Vec::new();
        if self.openai.is_some() {
            names.push("openai");
        }
        if self.stdio {
            names.push("stdio");
        }
        if self.http.is_some() {
            names.push("http");
        }
        names
    }

    pub async fn run(self, machine: LocalMachine, cancellation: CancellationToken) -> Result<()> {
        if self.is_empty() {
            return Err(anyhow!("no MCP transports are active"));
        }

        let mut tasks = JoinSet::new();

        if let Some(config) = self.openai {
            let machine = machine.clone();
            let cancellation = cancellation.child_token();
            tasks.spawn(async move {
                openai::run(machine, config, cancellation)
                    .await
                    .context("OpenAI transport failed")
            });
        }

        if self.stdio {
            let machine = machine.clone();
            let cancellation = cancellation.child_token();
            tasks.spawn(async move {
                stdio::run(machine, cancellation)
                    .await
                    .context("stdio transport failed")
            });
        }

        if let Some(config) = self.http {
            let cancellation = cancellation.child_token();
            tasks.spawn(async move {
                http::run(machine, config, cancellation)
                    .await
                    .context("HTTP transport failed")
            });
        }

        while let Some(result) = tasks.join_next().await {
            match result {
                Ok(Ok(())) => {
                    // A cleanly closed transport (for example stdio EOF) does not
                    // stop other active transports.
                }
                Ok(Err(error)) => {
                    stop_peer_transports(&mut tasks, &cancellation).await;
                    return Err(error);
                }
                Err(error) => {
                    stop_peer_transports(&mut tasks, &cancellation).await;
                    return Err(anyhow!("transport task panicked: {error}"));
                }
            }
        }

        Ok(())
    }
}

async fn stop_peer_transports(tasks: &mut JoinSet<Result<()>>, cancellation: &CancellationToken) {
    cancellation.cancel();

    let drained = time::timeout(TRANSPORT_SHUTDOWN_TIMEOUT, async {
        while tasks.join_next().await.is_some() {}
    })
    .await
    .is_ok();

    if !drained {
        tasks.abort_all();
        while tasks.join_next().await.is_some() {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runtime_restart_marker_survives_transport_context() {
        let error = anyhow::Error::new(RuntimeRestartRequested::new("restart".into(), true))
            .context("OpenAI transport failed");
        let request =
            runtime_restart_request(&error).expect("restart marker should survive context");
        assert!(request.reset_backoff());
    }

    #[tokio::test]
    async fn peer_transport_shutdown_cancels_and_drains() {
        let cancellation = CancellationToken::new();
        let child = cancellation.child_token();
        let mut tasks = JoinSet::new();
        tasks.spawn(async move {
            child.cancelled().await;
            Ok(())
        });

        stop_peer_transports(&mut tasks, &cancellation).await;

        assert!(cancellation.is_cancelled());
        assert!(tasks.is_empty());
    }

    #[tokio::test]
    async fn peer_transport_shutdown_force_aborts_unresponsive_task() {
        let cancellation = CancellationToken::new();
        let mut tasks = JoinSet::new();
        tasks.spawn(std::future::pending::<Result<()>>());

        stop_peer_transports(&mut tasks, &cancellation).await;

        assert!(cancellation.is_cancelled());
        assert!(tasks.is_empty());
    }
}
