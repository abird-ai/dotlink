//! Shell mounts: the single host → sandbox remapping primitive.
//!
//! Developer caches are predefined mounts; a profile, `DOTLINK_MOUNTS` and
//! `--mount` declare mounts directly. This module owns the primitive, the
//! sandbox target rule, keyed layer resolution, and the conflict rules, so the
//! configuration layer and the runtime layer share one definition instead of
//! reaching into each other.

use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};

use anyhow::Result;

use crate::recovery;

/// Where a mount came from.
///
/// A declared mount was asked for explicitly: a mistake in the declaration
/// itself is a configuration error, and a source that is not usable yet retries
/// the runtime instead of disappearing. A discovered cache is a best-effort
/// convenience and is skipped with a warning, matching the fact that cache
/// directories appear and disappear with the user's activity.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MountOrigin {
    Declared,
    Discovered,
}

impl MountOrigin {
    pub fn is_declared(self) -> bool {
        matches!(self, Self::Declared)
    }
}

/// One shell-only remapping: host `source` appears at `target`, read-only or
/// read+write, with the environment that points a tool family at it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SandboxMount {
    pub source: PathBuf,
    pub target: PathBuf,
    pub writable: bool,
    pub env: Vec<(String, String)>,
    pub origin: MountOrigin,
}

impl SandboxMount {
    /// `source -> target`, for diagnostics.
    pub fn label(&self) -> String {
        format!("{} -> {}", self.source.display(), self.target.display())
    }
}

/// One named resolution layer. Slice order in [`merge_layers`] is precedence
/// order, so adding a layer is one line at the call site.
pub struct MountLayer {
    pub name: &'static str,
    pub mounts: Vec<SandboxMount>,
}

/// Environment names the runtime itself sets for a sandboxed command. A mount
/// may not repoint them: `HOME`/`TMPDIR`/`PATH` define the sandbox, and the
/// rest are the session facts forwarded from the service environment.
const RESERVED_ENV: [&str; 7] = ["HOME", "PATH", "TMPDIR", "USER", "LANG", "LC_ALL", "TERM"];

/// Mount targets live inside the Linux sandbox home, a POSIX path, whatever
/// host build produced them. Joining with the host separator would yield
/// `/tmp/home\.cargo` on Windows and stop matching the sandbox rule that keeps
/// every mount target below `/tmp/home`.
pub fn sandbox_join(relative: &Path) -> PathBuf {
    let mut value = String::from("/tmp/home");
    for component in relative.components() {
        value.push('/');
        value.push_str(&component.as_os_str().to_string_lossy());
    }
    PathBuf::from(value)
}

/// Parent of a sandbox path, spelled with the same POSIX separator so
/// `CARGO_HOME` and `GRADLE_USER_HOME` stay inside the sandbox home.
pub fn sandbox_parent(sandbox_path: &Path) -> String {
    let value = sandbox_path.to_string_lossy();
    match value.rsplit_once('/') {
        Some((parent, _)) if !parent.is_empty() => parent.to_owned(),
        _ => value.to_string(),
    }
}

/// A configured path must be representable to the OS. An interior NUL byte can
/// never name a file, so it is a declaration mistake rather than a path that
/// might appear later.
pub fn validate_path(path: &Path, what: &str) -> Result<()> {
    if path.as_os_str().as_encoded_bytes().contains(&0) {
        return Err(recovery::operator(anyhow::anyhow!(
            "{what} must not contain a NUL byte"
        )));
    }
    Ok(())
}

/// A sandbox target must be an absolute path strictly below `/tmp/home`.
///
/// Mounting `/tmp/home` itself would replace the sandbox home the runtime
/// builds, and a `..`/`.` component would make the target spelling depend on
/// lexical normalization instead of the declared path. A NUL byte can never
/// name a path at all.
pub fn validate_target(target: &Path) -> Result<()> {
    validate_path(target, "sandbox mount target")?;
    if !target.has_root()
        || !target.starts_with("/tmp/home")
        || target == Path::new("/tmp/home")
        || target
            .components()
            .any(|component| matches!(component, Component::ParentDir | Component::CurDir))
    {
        return Err(recovery::operator(anyhow::anyhow!(
            "sandbox mount target must be an absolute path below /tmp/home: {}",
            target.display()
        )));
    }
    Ok(())
}

/// What to do about a nested layout, phrased once for every presentation of
/// the rule.
pub const NESTING_REMEDY: &str =
    "mount one target per tree (remove or override the cache entry, or drop one of the mounts)";

/// The first pair of targets that contain one another, if any.
///
/// Nested targets are order-dependent in a bind namespace: binding a parent
/// after a child silently hides the child, and binding it before needs a
/// mountpoint inside the parent that only exists on the host.
pub fn nesting_conflict<'a>(
    targets: impl IntoIterator<Item = &'a Path>,
) -> Option<(&'a Path, &'a Path)> {
    let targets = targets.into_iter().collect::<Vec<_>>();
    for (index, target) in targets.iter().enumerate() {
        for other in &targets[index + 1..] {
            if target != other && (target.starts_with(other) || other.starts_with(target)) {
                return Some((target, other));
            }
        }
    }
    None
}

/// Reject mount targets that contain one another.
pub fn reject_nested_targets<'a>(targets: impl IntoIterator<Item = &'a Path>) -> Result<()> {
    let Some((first, second)) = nesting_conflict(targets) else {
        return Ok(());
    };
    Err(recovery::operator(anyhow::anyhow!(
        "mount targets must not nest: {} and {}; {NESTING_REMEDY}",
        first.display(),
        second.display()
    )))
}

/// A mount environment rule the configuration breaks.
///
/// The rule names the mounts that disagree; presentation adds provenance when
/// the caller knows which configuration entry owns each mount, so a profile can
/// report `mounts[0]` while the binder reports `source -> target`.
#[derive(Clone, Copy, Debug)]
pub enum EnvConflict<'a> {
    Reserved {
        mount: &'a SandboxMount,
        name: &'a str,
    },
    Mismatch {
        name: &'a str,
        value: &'a str,
        mount: &'a SandboxMount,
        other_value: &'a str,
        other: &'a SandboxMount,
    },
}

impl EnvConflict<'_> {
    /// One line, naming each mount through `owner` when it has a provenance.
    pub fn describe(&self, owner: impl Fn(&SandboxMount) -> Option<String>) -> String {
        let named = |mount: &SandboxMount| {
            owner(mount).map_or_else(
                || mount.label(),
                |owner| format!("{owner} ({})", mount.label()),
            )
        };
        match self {
            Self::Reserved { mount, name } => format!(
                "mount {} must not set {name}: the sandbox owns that variable",
                named(mount)
            ),
            Self::Mismatch {
                name,
                value,
                mount,
                other_value,
                other,
            } => format!(
                "{name} is set to {value} by {} and to {other_value} by {}",
                named(mount),
                named(other)
            ),
        }
    }
}

impl std::fmt::Display for EnvConflict<'_> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.describe(|_| None))
    }
}

/// Reject mount environment that would repoint the sandbox or silently
/// disagree with another mount. Caches legitimately share a variable when they
/// share the same value (both Cargo caches point `CARGO_HOME` at `.cargo`).
pub fn validate_env(mounts: &[SandboxMount]) -> Result<(), EnvConflict<'_>> {
    let mut seen: BTreeMap<&str, (&str, &SandboxMount)> = BTreeMap::new();
    for mount in mounts {
        for (name, value) in &mount.env {
            if RESERVED_ENV.contains(&name.as_str()) {
                return Err(EnvConflict::Reserved { mount, name });
            }
            match seen.get(name.as_str()) {
                Some((existing, other)) if *existing != value.as_str() => {
                    return Err(EnvConflict::Mismatch {
                        name,
                        value,
                        mount,
                        other_value: existing,
                        other,
                    });
                }
                Some(_) => {}
                None => {
                    seen.insert(name, (value, mount));
                }
            }
        }
    }
    Ok(())
}

/// Resolve every layer over the builtin mounts.
///
/// Mounts are keyed by sandbox target and the last declaration wins, so a layer
/// list reads exactly like the precedence it implements. A first-seen target
/// keeps its position, so the assembled list stays deterministic.
///
/// This is precedence only. The result is not valid by construction: whoever
/// binds it must apply [`validate_assembled`] to the set that survives its own
/// skips, because that is the only set the binder can be wrong about.
pub fn merge_layers(builtin: Vec<SandboxMount>, layers: &[MountLayer]) -> Vec<SandboxMount> {
    let mut merged = builtin;
    for layer in layers {
        for mount in &layer.mounts {
            match merged
                .iter_mut()
                .find(|existing| existing.target == mount.target)
            {
                Some(existing) => {
                    tracing::debug!(
                        "mount {} replaced by {} layer",
                        existing.label(),
                        layer.name
                    );
                    *existing = mount.clone();
                }
                None => merged.push(mount.clone()),
            }
        }
    }
    merged
}

/// Apply the layout and environment rules to the mounts that will be bound.
///
/// The binder calls this on its post-skip set, and configuration load calls it
/// (through `setup::DeclaredMounts`) on what a saved profile declares, so a
/// mistake is a configuration error wherever it is seen. Host-integration
/// conflicts are the separate [`reject_host_conflicts`] rule, which needs host
/// state. Layout is checked first because environment wiring is derived from it.
pub fn validate_assembled(mounts: &[SandboxMount]) -> Result<()> {
    reject_nested_targets(mounts.iter().map(|mount| mount.target.as_path()))?;
    validate_env(mounts).map_err(|conflict| recovery::operator(anyhow::anyhow!("{conflict}")))
}

/// Reject a mount that would collide with a fixed host integration.
///
/// Host integrations bind their own targets (for example the Git config file at
/// `/tmp/home/.gitconfig`) after the mount layer, so a colliding mount would
/// either be shadowed or break the bind outright. Environment collides only
/// when the values differ: setting the same value twice is harmless.
pub fn reject_host_conflicts(
    mounts: &[SandboxMount],
    host_targets: &[PathBuf],
    host_env: &[(&str, &str)],
) -> Result<()> {
    for mount in mounts {
        for host in host_targets {
            if &mount.target == host
                || mount.target.starts_with(host)
                || host.starts_with(&mount.target)
            {
                return Err(recovery::operator(anyhow::anyhow!(
                    "mount {} conflicts with the host integration target {}; disable that integration or move the mount",
                    mount.label(),
                    host.display()
                )));
            }
        }
        for (name, value) in host_env {
            if let Some((_, mount_value)) =
                mount.env.iter().find(|(mount_name, _)| mount_name == name)
                && mount_value != value
            {
                return Err(recovery::operator(anyhow::anyhow!(
                    "mount {} sets {name}={mount_value} but the host integration sets {name}={value}",
                    mount.label()
                )));
            }
        }
    }
    Ok(())
}

/// A host-absolute scratch path for tests: the mount grammar requires an
/// absolute source, and "absolute" is host-shaped.
#[cfg(test)]
pub(crate) fn test_host_path(name: &str) -> PathBuf {
    if cfg!(windows) {
        PathBuf::from(format!(r"C:\{name}"))
    } else {
        PathBuf::from(format!("/{name}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mount(source: &str, target: &str, writable: bool) -> SandboxMount {
        SandboxMount {
            source: PathBuf::from(source),
            target: PathBuf::from(target),
            writable,
            env: Vec::new(),
            origin: MountOrigin::Declared,
        }
    }

    #[test]
    fn layers_override_by_target_and_keep_first_seen_position() {
        let builtin = vec![SandboxMount {
            env: vec![("HOME".to_owned(), String::new())],
            ..mount("/srv/cache", "/tmp/home/a", false)
        }];
        let layers = [
            MountLayer {
                name: "profile",
                mounts: vec![mount("/srv/a", "/tmp/home/a", false)],
            },
            MountLayer {
                name: "--mount",
                mounts: vec![mount("/srv/c", "/tmp/home/c", true)],
            },
        ];
        let merged = merge_layers(builtin, &layers);
        assert_eq!(merged.len(), 2);
        assert_eq!(merged[0].source, PathBuf::from("/srv/a"));
        assert_eq!(merged[1].target, PathBuf::from("/tmp/home/c"));

        let layers = [MountLayer {
            name: "profile",
            mounts: vec![SandboxMount {
                env: vec![("HOME".to_owned(), "/host".to_owned())],
                ..mount("/srv/a", "/tmp/home/a", false)
            }],
        }];
        let reserved = merge_layers(Vec::new(), &layers);
        let error = validate_assembled(&reserved).unwrap_err();
        assert!(
            error.to_string().contains("sandbox owns that variable"),
            "{error}"
        );
        assert_eq!(
            crate::recovery::Recovery::of(&error),
            crate::recovery::Recovery::Operator
        );
    }

    #[test]
    fn nesting_conflict_names_the_offending_pair() {
        assert_eq!(
            nesting_conflict([
                Path::new("/tmp/home/.cache"),
                Path::new("/tmp/home/other"),
                Path::new("/tmp/home/.cache/abird"),
            ]),
            Some((
                Path::new("/tmp/home/.cache"),
                Path::new("/tmp/home/.cache/abird")
            ))
        );
        assert_eq!(
            nesting_conflict([Path::new("/tmp/home/a"), Path::new("/tmp/home/a")]),
            None,
            "one target declared twice is an override, not nesting"
        );
        assert_eq!(
            nesting_conflict([
                Path::new("/tmp/home/.cargo/registry"),
                Path::new("/tmp/home/.cargo/git"),
            ]),
            None
        );
    }

    #[test]
    fn shared_cache_env_is_allowed_but_disagreement_is_not() {
        let shared = [
            SandboxMount {
                env: vec![("CARGO_HOME".to_owned(), "/tmp/home/.cargo".to_owned())],
                ..mount("/srv/registry", "/tmp/home/.cargo/registry", true)
            },
            SandboxMount {
                env: vec![("CARGO_HOME".to_owned(), "/tmp/home/.cargo".to_owned())],
                ..mount("/srv/git", "/tmp/home/.cargo/git", true)
            },
        ];
        assert!(validate_env(&shared).is_ok());

        let conflicting = [
            shared[0].clone(),
            SandboxMount {
                env: vec![("CARGO_HOME".to_owned(), "/tmp/home/other".to_owned())],
                ..shared[1].clone()
            },
        ];
        let conflict = validate_env(&conflicting).unwrap_err();
        let error = conflict.to_string();
        assert!(error.contains("CARGO_HOME is set to"), "{error}");
        let owned = conflict.describe(|mount| {
            (mount.target == Path::new("/tmp/home/.cargo/registry")).then(|| "mounts[1]".to_owned())
        });
        assert!(owned.starts_with("CARGO_HOME is set to"), "{owned}");
        assert!(owned.contains("mounts[1] ("), "{owned}");
    }

    #[test]
    fn host_conflicts_cover_targets_and_environment() {
        let mounts = vec![mount("/srv/git", "/tmp/home/.gitconfig", true)];
        let error = reject_host_conflicts(&mounts, &[PathBuf::from("/tmp/home/.gitconfig")], &[])
            .unwrap_err()
            .to_string();
        assert!(error.contains("host integration target"), "{error}");

        let nested = vec![mount("/srv/x", "/tmp/home/.config", false)];
        assert!(
            reject_host_conflicts(
                &nested,
                &[PathBuf::from("/tmp/home/.config/nix/nix.conf")],
                &[]
            )
            .is_err()
        );

        let env = vec![SandboxMount {
            env: vec![("XDG_CONFIG_HOME".to_owned(), "/tmp/custom".to_owned())],
            ..mount("/srv/x", "/tmp/home/x", false)
        }];
        let error = reject_host_conflicts(&env, &[], &[("XDG_CONFIG_HOME", "/tmp/home/.config")])
            .unwrap_err()
            .to_string();
        assert!(error.contains("host integration sets"), "{error}");
        assert!(
            reject_host_conflicts(&env, &[], &[("XDG_CONFIG_HOME", "/tmp/custom")]).is_ok(),
            "the same value twice is harmless"
        );
    }
}
