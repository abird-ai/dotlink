//! What happens after the runtime fails.
//!
//! Classification is opt-in at a failure's source and transparent to its
//! message. Unclassified errors default to [`Recovery::Process`], so a missing
//! or lost tag fails safe: a supervisor retries with backoff instead of leaving
//! the service down. Reserve [`Recovery::Operator`] for failures that retrying
//! unchanged provably cannot fix. [`Recovery::Unconfigured`] is not a failure:
//! it reports that first-run setup has not run yet, so a supervisor stops
//! instead of failing.
//!
//! A tag wraps the error: the tagged head stays visible in messages but not to
//! `chain()` downcasts, and a captured backtrace starts at the tag. Tag a
//! context-carrying error, not a bare foreign error that callers inspect.

use std::{error::Error as StdError, fmt, io, process::ExitCode};

/// sysexits `EX_CONFIG`; systemd names it `CONFIG`.
pub const EXIT_OPERATOR: u8 = 78;

/// systemd's `NOTCONFIGURED`: first-run setup is still pending.
pub const EXIT_NOTCONFIGURED: u8 = 6;

/// Next step after a runtime failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Recovery {
    /// Rebuild the runtime in-process; `reset_backoff` after a healthy run.
    Runtime { reset_backoff: bool },
    /// Exit generically; a supervisor restarts the process with backoff.
    Process,
    /// Exit `CONFIG`: configuration, credentials or the binary must change.
    Operator,
    /// Exit `NOTCONFIGURED`: no profile exists yet, so setup is pending. Nothing
    /// is broken; a supervisor treats it as a clean stop, not a failure.
    Unconfigured,
}

impl Recovery {
    /// Outermost classification on `error`, else [`Recovery::Process`].
    pub fn of(error: &anyhow::Error) -> Self {
        error
            .downcast_ref::<Classified>()
            .map_or(Self::Process, |classified| classified.recovery)
    }

    /// Classify `error` without changing its message or source chain.
    pub fn tag(self, error: impl Into<anyhow::Error>) -> anyhow::Error {
        anyhow::Error::new(Classified {
            recovery: self,
            error: error.into(),
        })
    }

    pub fn restarts_runtime(self) -> bool {
        matches!(self, Self::Runtime { .. })
    }

    /// Process status once no in-process restart remains.
    pub fn exit_code(self) -> ExitCode {
        match self {
            Self::Operator => ExitCode::from(EXIT_OPERATOR),
            Self::Unconfigured => ExitCode::from(EXIT_NOTCONFIGURED),
            Self::Runtime { .. } | Self::Process => ExitCode::FAILURE,
        }
    }
}

/// `Recovery::Operator.tag(error)`, shaped for `map_err`.
pub fn operator(error: impl Into<anyhow::Error>) -> anyhow::Error {
    Recovery::Operator.tag(error)
}

/// `Recovery::Unconfigured.tag(error)`, shaped for `map_err`.
pub fn unconfigured(error: impl Into<anyhow::Error>) -> anyhow::Error {
    Recovery::Unconfigured.tag(error)
}

/// A failure to resolve configuration from local state needs the operator,
/// unless its source classified it or I/O caused it: unreadable state may be
/// transient (late mounts, swapped secrets). Invalid content is not I/O.
pub fn config_failure(error: anyhow::Error) -> anyhow::Error {
    let caused_by_io = error
        .chain()
        .filter_map(|cause| cause.downcast_ref::<io::Error>())
        .any(|cause| cause.kind() != io::ErrorKind::InvalidData);
    if caused_by_io || error.downcast_ref::<Classified>().is_some() {
        error
    } else {
        operator(error)
    }
}

/// Transparent carrier: displays and chains exactly like the wrapped error.
struct Classified {
    recovery: Recovery,
    error: anyhow::Error,
}

impl fmt::Debug for Classified {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&self.error, f)
    }
}

impl fmt::Display for Classified {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Head only; anyhow renders the chain through `source`.
        write!(f, "{}", self.error)
    }
}

impl StdError for Classified {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        self.error.source()
    }
}

#[cfg(test)]
mod tests {
    use anyhow::{Context, anyhow};

    use super::*;

    fn layered() -> anyhow::Error {
        Err::<(), _>(std::io::Error::other("root"))
            .context("middle")
            .unwrap_err()
    }

    #[test]
    fn unclassified_failures_restart_the_process() {
        assert_eq!(Recovery::of(&anyhow!("unknown")), Recovery::Process);
        assert_eq!(Recovery::of(&layered().context("outer")), Recovery::Process);
    }

    #[test]
    fn classification_survives_context_and_the_outermost_tag_wins() {
        let error = operator(layered()).context("transport failed");
        assert_eq!(Recovery::of(&error), Recovery::Operator);

        let retagged = Recovery::Process.tag(error);
        assert_eq!(Recovery::of(&retagged), Recovery::Process);
    }

    #[test]
    fn classification_is_invisible_in_every_rendering() {
        // Backtraces differ by construction site when RUST_BACKTRACE is set.
        let report = |error: &anyhow::Error| {
            let report = format!("{error:?}");
            report
                .split("\n\nStack backtrace:")
                .next()
                .unwrap_or_default()
                .to_owned()
        };
        let plain = layered().context("outer");
        let tagged = operator(layered()).context("outer");
        assert_eq!(tagged.to_string(), plain.to_string());
        assert_eq!(format!("{tagged:#}"), format!("{plain:#}"));
        assert_eq!(report(&tagged), report(&plain));
        assert_eq!(
            tagged.chain().map(ToString::to_string).collect::<Vec<_>>(),
            ["outer", "middle", "root"]
        );
    }

    #[test]
    fn configuration_failures_need_the_operator_unless_io_caused_them() {
        let invalid = config_failure(anyhow!("invalid HTTP bind address"));
        assert_eq!(Recovery::of(&invalid), Recovery::Operator);

        let malformed = config_failure(
            Err::<(), _>(io::Error::from(io::ErrorKind::InvalidData))
                .context("cannot read profile")
                .unwrap_err(),
        );
        assert_eq!(Recovery::of(&malformed), Recovery::Operator);

        let unreadable = config_failure(layered());
        assert_eq!(Recovery::of(&unreadable), Recovery::Process);

        let classified = config_failure(Recovery::Process.tag(anyhow!("secret changed")));
        assert_eq!(Recovery::of(&classified), Recovery::Process);

        let pending = config_failure(unconfigured(anyhow!("no profile yet")));
        assert_eq!(Recovery::of(&pending), Recovery::Unconfigured);
    }

    #[test]
    fn only_operator_failures_exit_config() {
        assert_eq!(Recovery::Operator.exit_code(), ExitCode::from(78));
        assert_eq!(
            Recovery::Unconfigured.exit_code(),
            ExitCode::from(EXIT_NOTCONFIGURED)
        );
        assert_eq!(Recovery::Process.exit_code(), ExitCode::FAILURE);
        assert_eq!(
            Recovery::Runtime {
                reset_backoff: true
            }
            .exit_code(),
            ExitCode::FAILURE
        );
        assert!(
            Recovery::Runtime {
                reset_backoff: false
            }
            .restarts_runtime()
        );
        assert!(!Recovery::Process.restarts_runtime());
        assert!(!Recovery::Unconfigured.restarts_runtime());
    }
}
