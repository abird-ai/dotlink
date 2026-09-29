use anyhow::{Context, Result, anyhow};
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

use crate::mcp::LocalMachine;

pub mod http;
pub mod openai;
pub mod stdio;

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
                    cancellation.cancel();
                    while tasks.join_next().await.is_some() {}
                    return Err(error);
                }
                Err(error) => {
                    cancellation.cancel();
                    while tasks.join_next().await.is_some() {}
                    return Err(anyhow!("transport task panicked: {error}"));
                }
            }
        }

        Ok(())
    }
}
