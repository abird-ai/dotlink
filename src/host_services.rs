//! Opt-in host developer integrations exposed only to sandboxed shell tools.
//! Discovery never grants access: the caller selects the requested services.
use std::{
    env, fs,
    os::unix::fs::FileTypeExt,
    path::{Path, PathBuf},
    sync::Arc,
};

use anyhow::{Context, Result};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct HostAccess {
    pub nix_daemon: bool,
    pub nix_config: bool,
    pub ssh_agent: bool,
    pub git_config: bool,
}

impl HostAccess {
    pub fn any(self) -> bool {
        self.nix_daemon || self.nix_config || self.ssh_agent || self.git_config
    }
}

#[derive(Clone, Debug)]
pub struct HostMount {
    pub source: PathBuf,
    pub resolved: PathBuf,
    pub target: PathBuf,
    pub kind: HostMountKind,
    pub pinned: Arc<fs::File>,
}

impl HostMount {
    pub fn new(source: PathBuf, target: PathBuf, kind: HostMountKind) -> Result<Self> {
        let (pinned, resolved) = crate::secure_fs::SecureFs::open_host_mount(&source, kind)?;
        Ok(Self {
            source,
            resolved,
            target,
            kind,
            pinned: Arc::new(pinned),
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HostMountKind {
    RegularFile,
    Socket,
    Directory,
    CharacterDevice,
}

#[derive(Clone, Debug, Default)]
pub struct HostShell {
    pub mounts: Vec<HostMount>,
    pub environment: Vec<(String, String)>,
    pub nix_daemon: bool,
    pub ssh_agent: bool,
    pub kvm: bool,
    pub gpu: Vec<crate::setup::GpuVendor>,
}

#[derive(Clone, Debug, Default)]
pub struct Discovery {
    pub nix_daemon: Option<PathBuf>,
    pub nix_config: Option<PathBuf>,
    pub ssh_agent: Option<PathBuf>,
    pub git_configs: Vec<PathBuf>,
}

fn home_dir() -> Option<PathBuf> {
    env::var_os("HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

fn xdg_config(home: &Path) -> PathBuf {
    env::var_os("XDG_CONFIG_HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .unwrap_or_else(|| home.join(".config"))
}

fn file(path: PathBuf) -> Option<PathBuf> {
    fs::metadata(&path)
        .ok()
        .filter(|meta| meta.is_file())
        .map(|_| path)
}

fn socket(path: PathBuf) -> Option<PathBuf> {
    fs::metadata(&path)
        .ok()
        .filter(|meta| meta.file_type().is_socket())
        .map(|_| path)
}

/// Find an SSH agent without trusting an unverified pathname. An explicitly
/// configured socket is authoritative: a broken override must fail rather
/// than silently grant signing access to another identity.
fn select_ssh_agent(
    explicit: Option<PathBuf>,
    inherited: Option<PathBuf>,
    xdg_runtime_dir: Option<PathBuf>,
    uid: u32,
    run_users_dir: &Path,
) -> Option<PathBuf> {
    let valid = |path: PathBuf| path.is_absolute().then(|| socket(path)).flatten();

    if let Some(explicit) = explicit {
        return valid(explicit);
    }

    inherited
        .and_then(valid)
        .or_else(|| {
            xdg_runtime_dir
                .filter(|path| path.is_absolute())
                .and_then(|path| valid(path.join("gcr/ssh")))
        })
        .or_else(|| valid(run_users_dir.join(uid.to_string()).join("gcr/ssh")))
}

fn discover_ssh_agent() -> Option<PathBuf> {
    // SAFETY: geteuid reads the running process identity, which is the
    // systemd service User= account when launched as a system service.
    let uid = unsafe { libc::geteuid() };
    select_ssh_agent(
        env::var_os("DOTLINK_SSH_AGENT_SOCKET").map(PathBuf::from),
        env::var_os("SSH_AUTH_SOCK").map(PathBuf::from),
        env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from),
        uid,
        Path::new("/run/user"),
    )
}

pub fn discover() -> Discovery {
    let mut found = Discovery {
        nix_daemon: [
            "/nix/var/nix/daemon-socket/socket",
            "/run/nix-daemon/socket",
        ]
        .iter()
        .find_map(|path| socket(PathBuf::from(path))),
        ..Discovery::default()
    };

    if let Some(home) = home_dir() {
        let xdg = xdg_config(&home);
        found.nix_config = file(xdg.join("nix/nix.conf"));
        for config in [home.join(".gitconfig"), xdg.join("git/config")] {
            if let Some(config) = file(config) {
                found.git_configs.push(config);
            }
        }
    }
    found.ssh_agent = discover_ssh_agent();
    found
}

impl HostShell {
    pub fn prepare(access: HostAccess) -> Result<Self> {
        if !access.any() {
            return Ok(Self::default());
        }
        let discovered = discover();
        if access.ssh_agent
            && env::var_os("DOTLINK_SSH_AGENT_SOCKET").is_some()
            && discovered.ssh_agent.is_none()
        {
            anyhow::bail!(
                "DOTLINK_SSH_AGENT_SOCKET is not an accessible Unix-domain socket; fix or remove the explicit sshAgentSocket override"
            );
        }
        Self::prepare_discovered(access, discovered)
    }

    /// Also used by sandbox tests to verify discovery and mount setup without
    /// mutating process-wide HOME/SSH_AUTH_SOCK variables.
    pub(crate) fn prepare_discovered(access: HostAccess, discovered: Discovery) -> Result<Self> {
        let mut shell = Self::default();

        if access.nix_daemon {
            shell.nix_daemon = true;
            let source = discovered
                .nix_daemon
                .context("Nix daemon socket was not found; ensure nix-daemon is active")?;
            shell.mounts.push(HostMount::new(
                source,
                PathBuf::from("/nix/var/nix/daemon-socket/socket"),
                HostMountKind::Socket,
            )?);
            // Nix selects the daemon store without callers setting an
            // environment variable for each command.
            shell
                .environment
                .push(("NIX_REMOTE".into(), "daemon".into()));
        }
        if access.nix_config
            && let Some(source) = discovered.nix_config
        {
            shell.mounts.push(HostMount::new(
                source,
                PathBuf::from("/tmp/home/.config/nix/nix.conf"),
                HostMountKind::RegularFile,
            )?);
            shell
                .environment
                .push(("XDG_CONFIG_HOME".into(), "/tmp/home/.config".into()));
        }
        if access.git_config {
            for source in discovered.git_configs {
                let target = if source.file_name().is_some_and(|name| name == ".gitconfig") {
                    PathBuf::from("/tmp/home/.gitconfig")
                } else {
                    PathBuf::from("/tmp/home/.config/git/config")
                };
                shell
                    .mounts
                    .push(HostMount::new(source, target, HostMountKind::RegularFile)?);
            }
        }
        if access.ssh_agent {
            shell.ssh_agent = true;
            let source = discovered.ssh_agent.context(
                "SSH agent socket was not found. Checked DOTLINK_SSH_AGENT_SOCKET, SSH_AUTH_SOCK, $XDG_RUNTIME_DIR/gcr/ssh, and /run/user/<uid>/gcr/ssh. Start an agent or set services.dotlink.sshAgentSocket to a valid socket path",
            )?;
            shell.mounts.push(HostMount::new(
                source,
                PathBuf::from("/tmp/ssh-agent.sock"),
                HostMountKind::Socket,
            )?);
            shell
                .environment
                .push(("SSH_AUTH_SOCK".into(), "/tmp/ssh-agent.sock".into()));
        }
        Ok(shell)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_access_requires_opt_in() {
        assert!(!HostAccess::default().any());
        assert!(
            HostShell::prepare(HostAccess::default())
                .unwrap()
                .mounts
                .is_empty()
        );
    }

    #[test]
    fn requested_services_are_explicit_and_pinned() {
        use std::os::unix::fs::MetadataExt;
        let temp = tempfile::tempdir().unwrap();
        let nix_config = temp.path().join("nix.conf");
        let git_config = temp.path().join(".gitconfig");
        let xdg_git = temp.path().join("xdg-git");
        fs::write(&nix_config, "store = daemon\n").unwrap();
        fs::write(&git_config, "[user]\n name = Test\n").unwrap();
        fs::write(&xdg_git, "[core]\n editor = true\n").unwrap();
        let agent_path = temp.path().join("ssh-agent.sock");
        let agent = std::os::unix::net::UnixListener::bind(&agent_path).unwrap();
        let daemon_path = temp.path().join("nix-daemon.sock");
        let daemon = std::os::unix::net::UnixListener::bind(&daemon_path).unwrap();
        let discovered = Discovery {
            nix_daemon: Some(daemon_path),
            nix_config: Some(nix_config),
            ssh_agent: Some(agent_path.clone()),
            git_configs: vec![git_config, xdg_git],
        };
        let prepared = HostShell::prepare_discovered(
            HostAccess {
                nix_daemon: true,
                nix_config: true,
                ssh_agent: true,
                git_config: true,
            },
            discovered,
        )
        .unwrap();
        assert_eq!(prepared.mounts.len(), 5);
        assert!(prepared.nix_daemon);
        assert!(
            prepared
                .mounts
                .iter()
                .any(|m| m.target == Path::new("/tmp/home/.gitconfig"))
        );
        assert!(
            prepared
                .mounts
                .iter()
                .any(|m| m.target == Path::new("/tmp/home/.config/git/config"))
        );
        assert!(
            prepared
                .mounts
                .iter()
                .any(|m| m.target == Path::new("/tmp/home/.config/nix/nix.conf"))
        );
        assert!(
            prepared
                .environment
                .iter()
                .any(|(k, v)| k == "NIX_REMOTE" && v == "daemon")
        );
        assert!(
            prepared
                .environment
                .iter()
                .any(|(k, v)| k == "SSH_AUTH_SOCK" && v == "/tmp/ssh-agent.sock")
        );
        let mount = prepared
            .mounts
            .iter()
            .find(|m| m.target == Path::new("/tmp/ssh-agent.sock"))
            .unwrap();
        assert_eq!(
            mount.pinned.metadata().unwrap().ino(),
            fs::metadata(&agent_path).unwrap().ino()
        );
        // Restarting the host agent at the same path must not transparently
        // redirect the already-approved capability to another socket.
        fs::rename(&agent_path, temp.path().join("old-agent.sock")).unwrap();
        let replacement = std::os::unix::net::UnixListener::bind(&agent_path).unwrap();
        assert_ne!(
            mount.pinned.metadata().unwrap().ino(),
            fs::metadata(&agent_path).unwrap().ino()
        );
        drop(replacement);
        drop(agent);
        drop(daemon);
    }

    #[test]
    fn gcr_discovery_respects_precedence_and_the_service_uid() {
        let temp = tempfile::tempdir().unwrap();
        let run_users = temp.path().join("run/user");
        let uid = 12345;
        let gcr = run_users.join(uid.to_string()).join("gcr/ssh");
        fs::create_dir_all(gcr.parent().unwrap()).unwrap();
        let gcr_listener = std::os::unix::net::UnixListener::bind(&gcr).unwrap();

        let runtime_dir = temp.path().join("custom-runtime");
        fs::create_dir_all(runtime_dir.join("gcr")).unwrap();
        let xdg = runtime_dir.join("gcr/ssh");
        let xdg_listener = std::os::unix::net::UnixListener::bind(&xdg).unwrap();

        let inherited = temp.path().join("inherited.sock");
        let inherited_listener = std::os::unix::net::UnixListener::bind(&inherited).unwrap();
        let explicit = temp.path().join("explicit.sock");
        let explicit_listener = std::os::unix::net::UnixListener::bind(&explicit).unwrap();

        // System services generally have neither interactive environment variable.
        assert_eq!(
            select_ssh_agent(None, None, None, uid, &run_users),
            Some(gcr.clone())
        );
        assert_eq!(
            select_ssh_agent(None, None, None, uid + 1, &run_users),
            None
        );
        assert_eq!(
            select_ssh_agent(None, None, Some(runtime_dir.clone()), uid, &run_users),
            Some(xdg.clone()),
        );
        assert_eq!(
            select_ssh_agent(
                None,
                Some(inherited.clone()),
                Some(runtime_dir.clone()),
                uid,
                &run_users
            ),
            Some(inherited.clone()),
        );
        assert_eq!(
            select_ssh_agent(
                Some(explicit.clone()),
                Some(inherited),
                Some(runtime_dir.clone()),
                uid,
                &run_users
            ),
            Some(explicit),
        );

        // A stale inherited value can fall back to GCR, but a broken explicit
        // override must not silently select a different signing identity.
        let missing = temp.path().join("missing.sock");
        assert_eq!(
            select_ssh_agent(
                None,
                Some(missing.clone()),
                Some(runtime_dir),
                uid,
                &run_users
            ),
            Some(xdg),
        );
        assert_eq!(
            select_ssh_agent(Some(missing), None, None, uid, &run_users),
            None,
        );
        assert_eq!(
            select_ssh_agent(None, None, Some(PathBuf::from("relative")), uid, &run_users),
            Some(gcr),
        );
        drop((
            gcr_listener,
            xdg_listener,
            inherited_listener,
            explicit_listener,
        ));
    }

    #[test]
    fn ssh_agent_discovery_rejects_non_sockets_and_missing_agents() {
        let temp = tempfile::tempdir().unwrap();
        let run_users = temp.path().join("run/user");
        let regular = temp.path().join("not-a-socket");
        fs::write(&regular, "not an agent").unwrap();
        assert_eq!(select_ssh_agent(None, None, None, 99, &run_users), None);
        assert_eq!(
            select_ssh_agent(Some(regular.clone()), None, None, 99, &run_users),
            None
        );
        assert_eq!(
            select_ssh_agent(None, Some(regular), None, 99, &run_users),
            None
        );
    }

    #[test]
    fn socket_and_file_are_distinct() {
        let temp = tempfile::tempdir().unwrap();
        let regular = temp.path().join("regular");
        fs::write(&regular, "config").unwrap();
        assert!(file(regular.clone()).is_some());
        assert!(socket(regular).is_none());
        let sock = temp.path().join("socket");
        let listener = std::os::unix::net::UnixListener::bind(&sock).unwrap();
        assert!(socket(sock.clone()).is_some());
        assert!(file(sock).is_none());
        drop(listener);
    }
}
