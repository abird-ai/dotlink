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

use std::{
    future::Future,
    sync::atomic::{AtomicU64, Ordering},
    time::Duration,
};

use rmcp::{
    model::{ProgressNotificationParam, ProgressToken},
    service::{Peer, RequestContext, RoleServer},
};
use tokio::time::{Instant, sleep_until};

/// Interval between advisory progress ticks for one `tools/call`. A caller
/// cannot know how much of an arbitrary command or build is done, so a tick is
/// bare: a monotonic counter, no total, no message.
#[cfg(not(test))]
pub(crate) const PROGRESS_HEARTBEAT_INTERVAL: Duration = Duration::from_secs(10);
#[cfg(test)]
pub(crate) const PROGRESS_HEARTBEAT_INTERVAL: Duration = Duration::from_millis(100);

/// Progress is advisory; a stalled transport must never delay the operation it
/// reports on. Delivery is bounded per tick, with no detached task or queue.
const PROGRESS_SEND_BUDGET: Duration = Duration::from_millis(100);

/// Reports are advisory: a stalled peer must never delay the operation. The
/// outcome is returned so a tick can say whether it landed or was cut off.
async fn deliver_heartbeat_bounded<F: Future>(
    deadline: Option<Instant>,
    delivery: F,
) -> Option<F::Output> {
    let bound = Instant::now() + PROGRESS_SEND_BUDGET;
    let send_deadline = deadline.map_or(bound, |deadline| deadline.min(bound));
    tokio::select! {
        biased;
        () = sleep_until(send_deadline) => None,
        output = delivery => Some(output),
    }
}

/// Heartbeat sink for one `tools/call` that carried a progress token. A wait
/// that emits nothing is indistinguishable from a stalled turn, so anything
/// that blocks for a transport window reports while it blocks.
pub(crate) struct ProgressHeartbeat {
    peer: Peer<RoleServer>,
    token: ProgressToken,
    ticks: AtomicU64,
    delivered: AtomicU64,
    log: crate::logging::LogConfig,
    tool: String,
}

impl ProgressHeartbeat {
    /// One sink per call that carried a progress token; the caller's tool name
    /// and log are captured here so ticks and the summary report themselves.
    pub(crate) fn for_request(
        context: &RequestContext<RoleServer>,
        log: crate::logging::LogConfig,
        tool: impl Into<String>,
    ) -> Option<Self> {
        let token = context.meta.get_progress_token()?;
        Some(Self {
            peer: context.peer.clone(),
            token,
            ticks: AtomicU64::new(0),
            delivered: AtomicU64::new(0),
            log,
            tool: tool.into(),
        })
    }

    /// Monotonic progress counter with no total and no message.
    pub(crate) async fn report(&self, deadline: Option<Instant>) {
        let tick = self.ticks.fetch_add(1, Ordering::Relaxed) + 1;
        let param = ProgressNotificationParam::new(self.token.clone(), tick as f64);
        let delivered = deliver_heartbeat_bounded(deadline, async {
            self.peer.notify_progress(param).await.is_ok()
        })
        .await
        .unwrap_or(false);
        if delivered {
            self.delivered.fetch_add(1, Ordering::Relaxed);
        }
        self.log.progress_mark(delivered);
    }
}

/// The call is over: report how many ticks it needed and how many landed. This
/// is the only line a silent wait cannot produce, so a deployment can tell "no
/// progress token" (no line at all) from "ticks delivered" (`hb=n/n`).
impl Drop for ProgressHeartbeat {
    fn drop(&mut self) {
        let attempted = self.ticks.load(Ordering::Relaxed);
        if attempted == 0 {
            return;
        }
        self.log.progress_done(
            &self.tool,
            attempted,
            self.delivered.load(Ordering::Relaxed),
        );
    }
}

pub(crate) async fn heartbeat_tick(heartbeat: Option<&ProgressHeartbeat>) {
    match heartbeat {
        Some(_) => tokio::time::sleep(PROGRESS_HEARTBEAT_INTERVAL).await,
        None => std::future::pending().await,
    }
}

/// Awaits a window-bounded wait while reporting progress. Without a progress
/// token this is exactly `wait`; with one, the caller keeps receiving ticks for
/// as long as the wait blocks.
pub(crate) async fn await_with_heartbeat<F: Future>(
    heartbeat: Option<ProgressHeartbeat>,
    deadline: Option<Instant>,
    wait: F,
) -> F::Output {
    let heartbeat = match heartbeat {
        Some(heartbeat) => heartbeat,
        None => return wait.await,
    };
    tokio::pin!(wait);
    loop {
        tokio::select! {
            biased;
            output = &mut wait => return output,
            () = heartbeat_tick(Some(&heartbeat)) => heartbeat.report(deadline).await,
        }
    }
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

    #[test]
    fn capped_values_keep_host_floor_and_ceiling() {
        assert_eq!(resolve_capped::<usize>(None, 8), 8);
        assert_eq!(resolve_capped(Some(3_usize), 8), 3);
        assert_eq!(resolve_capped(Some(99_usize), 8), 8);
        assert_eq!(resolve_capped(Some(0_usize), 8), 1);
    }
}
