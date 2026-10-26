//! Cancellation-safe synchronous OS operations inside async request handlers.
//!
//! Unlike spawn_blocking, block_in_place does not detach a mutation after its
//! async guard has been cancelled. Tokio moves other work to another worker;
//! single-threaded test runtimes execute the operation inline.

pub(crate) fn run<T>(operation: impl FnOnce() -> T) -> T {
    if let Ok(handle) = tokio::runtime::Handle::try_current()
        && handle.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread
    {
        return tokio::task::block_in_place(operation);
    }
    operation()
}

/// Run a CPU-bound operation without allowing cancellation of its caller to
/// release a concurrency permit while the blocking work is still executing.
pub(crate) fn spawn_with_permit<T: Send + 'static>(
    permit: tokio::sync::OwnedSemaphorePermit,
    operation: impl FnOnce() -> T + Send + 'static,
) -> tokio::task::JoinHandle<T> {
    tokio::task::spawn_blocking(move || {
        let _permit = permit;
        operation()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(flavor = "multi_thread")]
    async fn cancelling_a_waiter_keeps_the_permit_with_the_running_worker() {
        use std::sync::{Arc, mpsc};
        let semaphore = Arc::new(tokio::sync::Semaphore::new(1));
        let (started, ready) = mpsc::channel();
        let (release, released) = mpsc::channel();
        let permit = semaphore.clone().acquire_owned().await.unwrap();
        let worker = spawn_with_permit(permit, move || {
            started.send(()).unwrap();
            released.recv().unwrap();
        });
        ready
            .recv_timeout(std::time::Duration::from_secs(2))
            .unwrap();
        worker.abort();
        assert_eq!(semaphore.available_permits(), 0);
        assert!(semaphore.clone().try_acquire_owned().is_err());
        release.send(()).unwrap();
        // Abort detaches a running blocking task: it still owns the permit
        // and releases it only after its synchronous work ends.
        for _ in 0..50 {
            if semaphore.available_permits() == 1 {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        panic!("blocking worker did not release its permit after completing");
    }

    #[tokio::test]
    async fn single_thread_runtime_can_execute_blocking_operations() {
        assert_eq!(run(|| 42), 42);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn multithread_runtime_can_execute_blocking_operations() {
        assert_eq!(run(|| 42), 42);
    }
}
