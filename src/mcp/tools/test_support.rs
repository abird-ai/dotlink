//! Test fixtures shared by the tool modules.

#![cfg(test)]

use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

use tokio::sync::RwLock;

use crate::logging::LogConfig;
use crate::mcp::{
    AccessPolicy, AccessSpec, LocalMachine, PreparedLaunchers, RuntimeConfig, TransportContextToken,
};

#[cfg(target_os = "linux")]
pub(crate) fn trusted_bubblewrap_available() -> bool {
    super::shell::prepare_bubblewrap_from_path()
        .ok()
        .flatten()
        .is_some()
}

pub(crate) fn test_machine(root: PathBuf) -> LocalMachine {
    test_machine_with(root, |_| {})
}

/// A machine whose profile limits (output, read, write) can be narrowed, for
/// tests that exercise the ceilings.
pub(crate) fn test_machine_with(
    root: PathBuf,
    configure: impl FnOnce(&mut RuntimeConfig),
) -> LocalMachine {
    test_machine_with_launchers(root, configure, PreparedLaunchers::default())
}

#[cfg(unix)]
pub(crate) fn test_machine_with_shell(root: PathBuf, shell: PathBuf) -> LocalMachine {
    test_machine_with_launchers(
        root,
        |config| {
            config.allow_shell = true;
            config.sandbox_shell = false;
        },
        PreparedLaunchers::for_test(Some(shell), None),
    )
}

fn test_machine_with_launchers(
    root: PathBuf,
    configure: impl FnOnce(&mut RuntimeConfig),
    launchers: PreparedLaunchers,
) -> LocalMachine {
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    let root_for_secure_fs = root.clone();
    let mut config = RuntimeConfig {
        access: AccessPolicy::from_spec(AccessSpec {
            base_dir: root.clone(),
            read_roots: vec![root.clone()],
            write_roots: vec![root.clone()],
            deny_read_roots: Vec::new(),
            deny_write_roots: Vec::new(),
            unrestricted_fs: false,
        })
        .unwrap(),
        cache_mounts: Vec::new(),
        shell_env: Vec::new(),
        #[cfg(target_os = "linux")]
        host_shell: crate::host_services::HostShell::default(),
        log: LogConfig::default(),
        allow_shell: false,
        sandbox_shell: false,
        allow_network: false,
        max_shell_timeout_secs: 60,
        max_output_bytes: 65536,
        max_read_bytes: 65536,
        max_write_bytes: 65536,
        shared_caps: crate::mcp::tools::shared_native::caps(65536, 65536).unwrap(),
        task_result_budget: None,
        surface_policy: crate::limits::RuntimeSurfacePolicy::default(),
        protected_paths: Vec::new(),
    };
    configure(&mut config);
    config.shared_caps =
        crate::mcp::tools::shared_native::caps(config.max_read_bytes, config.max_write_bytes)
            .unwrap();
    let shared_fs =
        crate::mcp::shared_fs::prepare_for_test(&config.access, &config.protected_paths).unwrap();
    LocalMachine {
        shared_fs,
        config,
        launchers,
        jobs: crate::jobs::JobManager::new(std::time::Duration::from_secs(
            crate::limits::DEFAULT_JOB_RETENTION_SECS,
        ))
        .unwrap(),
        operation_gate: Arc::new(RwLock::new(())),
        transport_context_token: TransportContextToken::from("test-transport-context"),
        router: std::sync::OnceLock::new(),
        serialize: false,
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        secure_fs: Arc::new(
            crate::secure_fs::SecureFs::new(
                std::slice::from_ref(&root_for_secure_fs),
                std::slice::from_ref(&root_for_secure_fs),
            )
            .unwrap(),
        ),
    }
    .bind_test_execution_policy()
    .unwrap()
}

pub(crate) fn access(root: &Path) -> AccessPolicy {
    AccessPolicy::from_spec(AccessSpec {
        base_dir: root.to_path_buf(),
        read_roots: vec![root.to_path_buf()],
        write_roots: Vec::new(),
        deny_read_roots: Vec::new(),
        deny_write_roots: Vec::new(),
        unrestricted_fs: false,
    })
    .unwrap()
}
