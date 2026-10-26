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

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn single_thread_runtime_can_execute_blocking_operations() {
        assert_eq!(run(|| 42), 42);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn multithread_runtime_can_execute_blocking_operations() {
        assert_eq!(run(|| 42), 42);
    }
}
