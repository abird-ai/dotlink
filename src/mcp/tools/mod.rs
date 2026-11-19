//! Dotlink-owned tool projections around the shared canonical filesystem core.
//! Filesystem semantics live in `aw-tools`; this module retains only product
//! transport helpers plus shell/job control surfaces.

#[cfg(test)]
pub(crate) mod differential;
pub(crate) mod graph;
pub(crate) mod jobs;
pub(crate) mod shared_native;
pub(crate) mod shell;
#[cfg(test)]
pub(crate) mod test_support;

use rmcp::model::{CallToolResult, ContentBlock};

use super::LocalMachine;

#[cfg(test)]
use std::time::Duration;
use std::{future::Future, sync::Arc};

use aw_mcp_protocol::{
    ProgressHeartbeat, ProgressHeartbeatConfig, ProgressHeartbeatObserver,
    ProgressHeartbeatSummary, await_with_progress_heartbeat,
};
use rmcp::service::{Peer, RequestContext, RoleServer};
use tokio::time::Instant;

/// Dotlink-owned product observer. Shared progress owns scheduling/counting;
/// the product keeps only its existing log surface.
struct ProgressLogObserver {
    log: crate::logging::LogConfig,
    tool: String,
}

impl ProgressHeartbeatObserver for ProgressLogObserver {
    fn completed(&self, summary: ProgressHeartbeatSummary) {
        if summary.attempted == 0 {
            return;
        }
        self.log
            .progress_done(&self.tool, summary.attempted, summary.delivered);
    }
}

/// Request-scoped rmcp delivery context around the shared heartbeat primitive.
/// It owns no timer/counter/scheduler state.
pub(crate) struct RequestProgress {
    heartbeat: ProgressHeartbeat,
    peer: Peer<RoleServer>,
    log: crate::logging::LogConfig,
}

fn progress_heartbeat_config() -> ProgressHeartbeatConfig {
    #[cfg(not(test))]
    {
        ProgressHeartbeatConfig::default()
    }
    #[cfg(test)]
    {
        ProgressHeartbeatConfig::new(
            Duration::from_millis(100),
            aw_mcp_protocol::DEFAULT_PROGRESS_SEND_BUDGET,
        )
        .expect("test heartbeat timing is valid")
    }
}

impl RequestProgress {
    pub(crate) fn for_request(
        context: &RequestContext<RoleServer>,
        log: crate::logging::LogConfig,
        tool: impl Into<String>,
    ) -> Option<Self> {
        let token = context.meta.get_progress_token()?;
        let observer: Arc<dyn ProgressHeartbeatObserver> = Arc::new(ProgressLogObserver {
            log: log.clone(),
            tool: tool.into(),
        });
        let heartbeat = ProgressHeartbeat::from_token(
            Some(token),
            progress_heartbeat_config(),
            Some(observer),
        )?;
        Some(Self {
            heartbeat,
            peer: context.peer.clone(),
            log,
        })
    }
}

/// Thin rmcp transport adapter around the shared heartbeat scheduler. The send
/// closure preserves dotlink's per-tick mark while the shared observer owns the
/// final attempted/delivered summary, including cancellation/drop paths.
pub(crate) async fn await_with_request_progress<F: Future>(
    progress: Option<RequestProgress>,
    deadline: Option<Instant>,
    wait: F,
) -> F::Output {
    let Some(RequestProgress {
        heartbeat,
        peer,
        log,
    }) = progress
    else {
        return wait.await;
    };
    await_with_progress_heartbeat(Some(heartbeat), deadline, wait, move |param| {
        let peer = peer.clone();
        let log = log.clone();
        async move {
            let delivered = peer.notify_progress(param).await.is_ok();
            log.progress_mark(delivered);
            delivered
        }
    })
    .await
}

fn dot() -> String {
    ".".to_owned()
}

fn ok_text(text: impl Into<String>) -> CallToolResult {
    CallToolResult::success(vec![ContentBlock::text(text.into())])
}

fn tool_error(error: impl std::fmt::Display) -> CallToolResult {
    CallToolResult::error(vec![ContentBlock::text(error.to_string())])
}

/// Client-controlled value under a host-selected policy ceiling.
pub(crate) fn resolve_capped<T>(requested: Option<T>, ceiling: T) -> T
where
    T: Ord + From<u8> + Copy,
{
    let ceiling = ceiling.max(T::from(1));
    requested.unwrap_or(ceiling).clamp(T::from(1), ceiling)
}

#[cfg(test)]
mod tests {
    use super::resolve_capped;

    #[tokio::test]
    async fn pending_progress_delivery_never_delays_or_outlives_ready_operation() {
        struct DropSignal(Option<tokio::sync::oneshot::Sender<()>>);

        impl Drop for DropSignal {
            fn drop(&mut self) {
                if let Some(sender) = self.0.take() {
                    let _ = sender.send(());
                }
            }
        }

        let token =
            rmcp::model::ProgressToken(rmcp::model::NumberOrString::String("progress-test".into()));
        let heartbeat = aw_mcp_protocol::ProgressHeartbeat::new(
            token,
            aw_mcp_protocol::ProgressHeartbeatConfig::new(
                std::time::Duration::from_millis(1),
                std::time::Duration::from_secs(1),
            )
            .unwrap(),
            None,
        );
        let (delivery_started, wait_for_delivery) = tokio::sync::oneshot::channel();
        let (delivery_dropped, dropped) = tokio::sync::oneshot::channel();
        let wait = async {
            wait_for_delivery.await.expect("delivery started");
            42_u8
        };
        let mut delivery_started = Some(delivery_started);
        let mut delivery_dropped = Some(delivery_dropped);
        let result = aw_mcp_protocol::await_with_progress_heartbeat(
            Some(heartbeat),
            None,
            wait,
            move |_param| {
                let started = delivery_started.take();
                let dropped = delivery_dropped.take();
                async move {
                    let _drop_signal = DropSignal(dropped);
                    if let Some(started) = started {
                        let _ = started.send(());
                    }
                    std::future::pending::<bool>().await
                }
            },
        )
        .await;
        assert_eq!(result, 42);
        dropped
            .await
            .expect("pending progress delivery must be dropped with the call");
    }

    #[test]
    fn capped_values_keep_host_floor_and_ceiling() {
        assert_eq!(resolve_capped::<usize>(None, 8), 8);
        assert_eq!(resolve_capped(Some(3_usize), 8), 3);
        assert_eq!(resolve_capped(Some(99_usize), 8), 8);
        assert_eq!(resolve_capped(Some(0_usize), 8), 1);
    }
}
