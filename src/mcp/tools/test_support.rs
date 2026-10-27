//! Test fixtures shared by the tool modules.

#![cfg(test)]

use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

use tokio::sync::RwLock;

use crate::logging::LogConfig;
use crate::mcp::tools::dot;
use crate::mcp::tools::grep::{GrepArgs, GrepOutputMode};
use crate::mcp::{AccessPolicy, AccessSpec, LocalMachine, RuntimeConfig};

pub(crate) fn test_machine(root: PathBuf) -> LocalMachine {
    test_machine_with(root, |_| {})
}

/// A machine whose profile limits (output, read, write) can be narrowed, for
/// tests that exercise the ceilings.
pub(crate) fn test_machine_with(
    root: PathBuf,
    configure: impl FnOnce(&mut RuntimeConfig),
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
        shell_program: None,
        bwrap_program: None,
        max_shell_timeout_secs: 60,
        max_output_bytes: 65536,
        max_read_bytes: 65536,
        max_write_bytes: 65536,
        protected_paths: Vec::new(),
    };
    configure(&mut config);
    LocalMachine {
        config,
        operation_gate: Arc::new(RwLock::new(())),
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

pub(crate) fn grep_args(pattern: &str) -> GrepArgs {
    GrepArgs {
        pattern: pattern.to_owned(),
        path: dot(),
        glob: None,
        literal: false,
        ignore_case: false,
        output_mode: GrepOutputMode::Content,
        before: None,
        after: None,
        numbered: None,
        offset: None,
        limit: None,
        gitignore: false,
        skip_hidden: false,
    }
}

pub(crate) fn grep_tree() -> (tempfile::TempDir, LocalMachine) {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap();
    std::fs::create_dir_all(root.join("src/nested")).unwrap();
    std::fs::write(root.join("top.txt"), "alpha\nneedle one\nomega\n").unwrap();
    std::fs::write(root.join("src/lib.rs"), "needle two\nplain\nneedle three\n").unwrap();
    std::fs::write(root.join("src/nested/deep.rs"), "deep needle\n").unwrap();
    std::fs::write(root.join("notes.md"), "needle notes\n").unwrap();
    std::fs::write(root.join("binary.bin"), [0xff_u8, 0xfe, 0x00]).unwrap();
    std::fs::write(root.join(".hidden.txt"), "needle hidden\n").unwrap();
    let machine = test_machine(root);
    (temp, machine)
}
