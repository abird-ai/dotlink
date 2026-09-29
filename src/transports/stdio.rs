use anyhow::{Context, Result};
use rmcp::ServiceExt;
use tokio_util::sync::CancellationToken;

use crate::mcp::LocalMachine;

pub async fn run(machine: LocalMachine, cancellation: CancellationToken) -> Result<()> {
    let service = machine
        .serve_with_ct(rmcp::transport::stdio(), cancellation)
        .await
        .context("failed to initialize stdio MCP server")?;

    service
        .waiting()
        .await
        .context("stdio MCP server task failed")?;

    Ok(())
}
