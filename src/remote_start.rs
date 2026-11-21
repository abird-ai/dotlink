//! Authenticated AW -> dotlink remote-Start bridge primitives.
//!
//! A RemoteStartAuthorization is source-fencing evidence, not a dotlink grant.
//! It is accepted only from the same OAuth-authenticated client named by the
//! target-authenticated issuer. A genuinely new claim additionally requires that
//! client's current explicit AW-bridge trust epoch. Immutable host/profile/client/
//! operation/argument identity is always bound first, then the existing JobManager
//! replay owner is consulted. Lease age, runtime generation and current trust gate only
//! a genuinely new effect claim; an exact retained receipt survives those mutable changes.

use std::{
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Result, anyhow, bail};
use aw_tools::{
    OperationId,
    execution_ref::{ExecutionRef, ExecutionSegment},
    remote_start::{
        MAX_REMOTE_START_LEASE_MILLIS, RemoteRuntimeLease, RemoteSessionAction,
        RemoteStartAuthorization, RemoteStartIssuer, RemoteStartLease,
    },
};
use rmcp::{
    model::{CallToolResult, ContentBlock, RequestMetaObject},
    service::{RequestContext, RoleServer},
};
use serde_json::Value;
use sha2::{Digest, Sha256};

pub(crate) use aw_tools::remote_start::arguments_sha256;

pub(crate) const REMOTE_START_META_KEY: &str = "io.abird.aw/remoteStart";
pub(crate) const REMOTE_START_LEASE_MILLIS: u64 = 30_000;

pub(crate) fn withheld_replay_result() -> CallToolResult {
    CallToolResult::success(vec![ContentBlock::text(
        "prior remote Start is already consumed; protected replay content is unavailable under current authority; do not retry",
    )])
}

pub(crate) fn compacted_replay_result() -> CallToolResult {
    CallToolResult::success(vec![ContentBlock::text(
        "prior remote Start is already consumed; original replay detail has expired; do not retry",
    )])
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct AuthenticatedMcpClient {
    client_id: Arc<str>,
    remote_start_trust_epoch: Option<u64>,
}

impl AuthenticatedMcpClient {
    pub(crate) fn new(client_id: impl Into<Arc<str>>) -> Result<Self> {
        let client_id = client_id.into();
        if client_id.is_empty() || client_id.len() > 512 || client_id.chars().any(char::is_control)
        {
            bail!("invalid authenticated MCP client identity");
        }
        Ok(Self {
            client_id,
            remote_start_trust_epoch: None,
        })
    }

    pub(crate) fn with_remote_start_trust(
        client_id: impl Into<Arc<str>>,
        trust_epoch: u64,
    ) -> Result<Self> {
        if trust_epoch == 0 {
            bail!("invalid remote Start trust epoch");
        }
        let mut client = Self::new(client_id)?;
        client.remote_start_trust_epoch = Some(trust_epoch);
        Ok(client)
    }

    pub(crate) fn id(&self) -> &str {
        self.client_id.as_ref()
    }

    pub(crate) fn remote_start_trust_epoch(&self) -> Option<u64> {
        self.remote_start_trust_epoch
    }

    fn issuer(&self) -> Result<RemoteStartIssuer> {
        let trust_epoch = self
            .remote_start_trust_epoch
            .ok_or_else(|| anyhow!("authenticated MCP client is not trusted as an AW bridge"))?;
        let issuer = RemoteStartIssuer {
            client_id: self.id().to_owned(),
            trust_epoch,
        };
        issuer
            .validate()
            .map_err(|_| anyhow!("invalid remote Start issuer identity"))?;
        Ok(issuer)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct BridgeTarget {
    host: Arc<str>,
    profile: Arc<str>,
}

impl BridgeTarget {
    pub(crate) fn new(host: impl Into<Arc<str>>, profile: impl Into<Arc<str>>) -> Result<Self> {
        let host = host.into();
        let profile = profile.into();
        for (label, value) in [("host", host.as_ref()), ("profile", profile.as_ref())] {
            if value.is_empty() || value.len() > 128 || value.chars().any(char::is_control) {
                bail!("invalid remote bridge {label}");
            }
        }
        Ok(Self { host, profile })
    }

    pub(crate) fn host(&self) -> &str {
        self.host.as_ref()
    }

    pub(crate) fn profile(&self) -> &str {
        self.profile.as_ref()
    }

    pub(crate) fn lease(
        &self,
        generation: [u8; 32],
        client: &AuthenticatedMcpClient,
    ) -> Result<RemoteStartLease> {
        if generation == [0; 32] {
            bail!("remote bridge runtime generation is unavailable");
        }
        let issued_at_unix_ms = now_unix_ms()?;
        let expires_at_unix_ms = issued_at_unix_ms
            .checked_add(REMOTE_START_LEASE_MILLIS.min(MAX_REMOTE_START_LEASE_MILLIS))
            .ok_or_else(|| anyhow!("remote Start lease timestamp overflow"))?;
        let target = RemoteRuntimeLease {
            target_host: self.host().to_owned(),
            target_profile: self.profile().to_owned(),
            generation,
            issued_at_unix_ms,
            expires_at_unix_ms,
        };
        target
            .validate()
            .map_err(|_| anyhow!("invalid remote Start runtime lease"))?;
        let lease = RemoteStartLease {
            target,
            issuer: client.issuer()?,
        };
        lease
            .validate()
            .map_err(|_| anyhow!("invalid remote Start lease"))?;
        Ok(lease)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum StartClass {
    Submit,
    Control,
}

impl StartClass {
    fn action(self) -> RemoteSessionAction {
        match self {
            Self::Submit => RemoteSessionAction::Submit,
            Self::Control => RemoteSessionAction::Control,
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct ValidatedRemoteStart {
    authorization: RemoteStartAuthorization,
    semantic_hash: [u8; 32],
    new_claim_denial: Option<NewClaimDenial>,
    issuer_current: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum NewClaimDenial {
    LeaseNotCurrent,
    RuntimeGenerationChanged,
    IssuerNotTrusted,
}

impl NewClaimDenial {
    const fn message(self) -> &'static str {
        match self {
            Self::LeaseNotCurrent => "remote Start lease is not current for a new effect",
            Self::RuntimeGenerationChanged => {
                "remote Start targets an old runtime generation; a new effect is forbidden"
            }
            Self::IssuerNotTrusted => {
                "remote Start issuer is not currently trusted for new effects"
            }
        }
    }
}

impl ValidatedRemoteStart {
    pub(crate) fn semantic_hash(&self) -> [u8; 32] {
        self.semantic_hash
    }

    pub(crate) fn claim_key(&self) -> [u8; 32] {
        let mut hash = Sha256::new();
        hash.update(b"dotlink-aw-remote-start-claim-v1\0");
        // One AW authorization ID owns exactly one remote effect-attempt slot.
        // Authenticated client identity belongs in the semantic fingerprint,
        // not the lookup key: the same proof from another OAuth client must
        // collide and fail closed rather than allocate a second effect slot.
        hash.update(self.authorization.authorization_id);
        hash.finalize().into()
    }

    pub(crate) fn replay_key(&self) -> String {
        format!("aw-start:{:x}", Sha256::digest(self.claim_key()))
    }

    #[cfg(test)]
    pub(crate) fn new_claim_authorized(&self) -> bool {
        self.new_claim_denial.is_none()
    }

    pub(crate) fn new_claim_denial(&self) -> Option<&'static str> {
        self.new_claim_denial.map(NewClaimDenial::message)
    }

    /// Payload disclosure requires the authenticated issuer to remain trusted.
    /// Lease expiry/runtime-generation drift blocks only new effects, not replay.
    pub(crate) fn replay_source_current(&self) -> bool {
        self.issuer_current
    }
}

pub(crate) fn now_unix_ms() -> Result<u64> {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| anyhow!("system clock is before the Unix epoch"))?
        .as_millis();
    u64::try_from(millis).map_err(|_| anyhow!("system clock exceeds remote Start timestamp range"))
}

fn authenticated_client(context: &RequestContext<RoleServer>) -> Result<AuthenticatedMcpClient> {
    let parts = context
        .extensions
        .get::<http::request::Parts>()
        .ok_or_else(|| anyhow!("remote Start requires authenticated HTTP transport"))?;
    parts
        .extensions
        .get::<AuthenticatedMcpClient>()
        .cloned()
        .ok_or_else(|| anyhow!("remote Start requires authenticated MCP client identity"))
}

pub(crate) fn has_metadata(meta: &RequestMetaObject) -> bool {
    meta.get(REMOTE_START_META_KEY).is_some()
}

fn authorization_from_meta(meta: &RequestMetaObject) -> Result<Option<RemoteStartAuthorization>> {
    let Some(value) = meta.get(REMOTE_START_META_KEY) else {
        return Ok(None);
    };
    let authorization = serde_json::from_value::<RemoteStartAuthorization>(value.clone())
        .map_err(|_| anyhow!("invalid remote Start metadata"))?;
    authorization
        .validate()
        .map_err(|_| anyhow!("invalid remote Start authorization"))?;
    Ok(Some(authorization))
}

fn semantic_hash(
    authorization: &RemoteStartAuthorization,
    client_id: &str,
    operation: OperationId,
    arguments: &Value,
) -> [u8; 32] {
    let mut hash = Sha256::new();
    hash.update(b"dotlink-aw-remote-start-semantic-v1\0");
    hash.update((client_id.len() as u64).to_be_bytes());
    hash.update(client_id.as_bytes());
    hash.update(authorization.authorization_id);
    hash.update(operation.semantic_name().as_bytes());
    hash.update(arguments_sha256(arguments));
    hash.finalize().into()
}

fn finite_suffix(reference: &str) -> Result<(String, Option<usize>)> {
    let parsed = ExecutionRef::parse(reference).map_err(|_| anyhow!("invalid remote Start ref"))?;
    let resolved = parsed
        .resolve(None, None)
        .map_err(|_| anyhow!("remote Start ref must already be canonical"))?;
    let suffix = resolved.suffix();
    let job = suffix
        .iter()
        .find_map(|segment| match segment {
            ExecutionSegment::Job(job) => Some(job.as_str().to_owned()),
            _ => None,
        })
        .ok_or_else(|| anyhow!("remote control Start must bind an exact finite Job"))?;
    let node = suffix.iter().find_map(|segment| match segment {
        ExecutionSegment::Node(node) => Some(node.get() as usize),
        _ => None,
    });
    Ok((job, node))
}

pub(crate) struct ValidationContext<'a> {
    pub(crate) client_id: &'a str,
    pub(crate) remote_start_trust_epoch: Option<u64>,
    pub(crate) target: &'a BridgeTarget,
    pub(crate) generation: [u8; 32],
    pub(crate) now_unix_ms: u64,
    pub(crate) operation: OperationId,
    pub(crate) class: StartClass,
    pub(crate) arguments: &'a Value,
    pub(crate) control_ref: Option<&'a str>,
}

pub(crate) fn validate_authorization(
    authorization: RemoteStartAuthorization,
    request: ValidationContext<'_>,
) -> Result<ValidatedRemoteStart> {
    let client = match request.remote_start_trust_epoch {
        Some(epoch) => AuthenticatedMcpClient::with_remote_start_trust(request.client_id, epoch)?,
        None => AuthenticatedMcpClient::new(request.client_id)?,
    };
    authorization
        .validate()
        .map_err(|_| anyhow!("invalid remote Start authorization"))?;
    if authorization.intent.target.target_host != request.target.host()
        || authorization.intent.target.target_profile != request.target.profile()
    {
        bail!("remote Start targets another bridge");
    }

    if authorization.intent.operation != request.operation.semantic_name()
        || authorization.intent.session_action != request.class.action()
        || authorization.intent.arguments_sha256 != arguments_sha256(request.arguments)
    {
        bail!("remote Start differs from the exact semantic request");
    }

    let issuer =
        authorization.provenance.issuer.as_ref().ok_or_else(|| {
            anyhow!("remote Start is missing target-authenticated issuer identity")
        })?;
    if issuer.client_id != client.id() {
        bail!("remote Start issuer differs from authenticated MCP client");
    }
    let target = &authorization.intent.target;
    let issuer_current = client.remote_start_trust_epoch() == Some(issuer.trust_epoch);
    let new_claim_denial = if target.generation != request.generation {
        Some(NewClaimDenial::RuntimeGenerationChanged)
    } else if request.now_unix_ms < target.issued_at_unix_ms
        || request.now_unix_ms >= target.expires_at_unix_ms
    {
        Some(NewClaimDenial::LeaseNotCurrent)
    } else if !issuer_current {
        Some(NewClaimDenial::IssuerNotTrusted)
    } else {
        None
    };

    if let Some(reference) = request.control_ref {
        let (requested_job, requested_node) = finite_suffix(reference)?;
        let (bound_job, bound_node) = finite_suffix(&authorization.intent.canonical_ref)?;
        if bound_job != requested_job || bound_node != requested_node {
            bail!("remote Start finite target differs from requested Job ref");
        }
    }

    let semantic_hash = semantic_hash(
        &authorization,
        client.id(),
        request.operation,
        request.arguments,
    );
    Ok(ValidatedRemoteStart {
        authorization,
        semantic_hash,
        new_claim_denial,
        issuer_current,
    })
}

pub(crate) fn validate_request(
    context: &RequestContext<RoleServer>,
    target: &BridgeTarget,
    generation: [u8; 32],
    operation: OperationId,
    class: StartClass,
    arguments: &Value,
    control_ref: Option<&str>,
) -> Result<Option<ValidatedRemoteStart>> {
    let Some(authorization) = authorization_from_meta(&context.meta)? else {
        return Ok(None);
    };
    let client = authenticated_client(context)?;
    validate_authorization(
        authorization,
        ValidationContext {
            client_id: client.id(),
            remote_start_trust_epoch: client.remote_start_trust_epoch(),
            target,
            generation,
            now_unix_ms: now_unix_ms()?,
            operation,
            class,
            arguments,
            control_ref,
        },
    )
    .map(Some)
}

#[cfg(test)]
mod tests {
    use super::*;
    use aw_tools::remote_start::{
        RemoteStartIntent, RemoteStartIssuer, RemoteStartProvenance, RemoteStartSource,
    };

    fn auth(arguments: &Value, generation: [u8; 32]) -> RemoteStartAuthorization {
        RemoteStartAuthorization::new(
            RemoteStartIntent {
                version: aw_tools::remote_start::REMOTE_START_VERSION,
                original_invocation_id: "invocation".into(),
                turn_id: 7,
                call_id: "call".into(),
                effect_attempt_id: "attempt".into(),
                operation: OperationId::Bash.semantic_name().into(),
                canonical_ref: "/d:d/i:i/p:p/a:0/t:7".into(),
                arguments_sha256: arguments_sha256(arguments),
                original_request_sha256: [4; 32],
                session_action: RemoteSessionAction::Submit,
                target: RemoteRuntimeLease {
                    target_host: "l5".into(),
                    target_profile: "default".into(),
                    generation,
                    issued_at_unix_ms: 100,
                    expires_at_unix_ms: 200,
                },
            },
            RemoteStartProvenance {
                actor: "actor".into(),
                source: RemoteStartSource {
                    source_kind: "host".into(),
                    source_id: "shell".into(),
                    source_epoch: 1,
                    site: "l5".into(),
                    resource_scope: "root".into(),
                    grant_sha256: [5; 32],
                },
                issuer: Some(RemoteStartIssuer {
                    client_id: "client-a".into(),
                    trust_epoch: 7,
                }),
                policy_revision_sha256: [6; 32],
                policy_admission_id: "policy".into(),
                session_id: "session".into(),
                branch_id: "branch".into(),
                writer_epoch: 1,
                membership_epoch: 1,
                audience_epoch: 1,
            },
        )
        .unwrap()
    }

    #[test]
    fn argument_digest_is_object_order_independent() {
        let left = serde_json::json!({"b":[2,1],"a":{"z":true,"x":"v"}});
        let right = serde_json::json!({"a":{"x":"v","z":true},"b":[2,1]});
        assert_eq!(arguments_sha256(&left), arguments_sha256(&right));
    }

    #[test]
    fn replay_slot_is_authorization_owned_and_client_identity_is_semantic_bound() {
        let arguments = serde_json::json!({"command":"true"});
        let authorization = auth(&arguments, [9; 32]);
        let now = now_unix_ms().unwrap();
        let mut intent = authorization.intent.clone();
        intent.target.issued_at_unix_ms = now;
        intent.target.expires_at_unix_ms = now + 1_000;
        let authorization =
            RemoteStartAuthorization::new(intent, authorization.provenance.clone()).unwrap();
        let first = ValidatedRemoteStart {
            semantic_hash: semantic_hash(&authorization, "client-a", OperationId::Bash, &arguments),
            authorization: authorization.clone(),
            new_claim_denial: None,
            issuer_current: true,
        };
        let second = ValidatedRemoteStart {
            semantic_hash: semantic_hash(&authorization, "client-b", OperationId::Bash, &arguments),
            authorization,
            new_claim_denial: None,
            issuer_current: true,
        };
        assert_eq!(first.claim_key(), second.claim_key());
        assert_eq!(first.replay_key(), second.replay_key());
        assert_ne!(first.semantic_hash(), second.semantic_hash());

        let manager = crate::jobs::JobManager::with_spool_limit(
            std::time::Duration::from_secs(60),
            crate::limits::DEFAULT_JOB_SPOOL_BYTES,
        )
        .unwrap();
        let guard = match manager.claim_remote_start(&first).unwrap() {
            crate::jobs::RemoteStartDecision::New(guard) => guard,
            _ => panic!("first authenticated client must own the Start claim"),
        };
        assert!(manager.claim_remote_start(&second).is_err());
        guard.release_before_effect();
    }

    #[test]
    fn validation_binds_target_generation_operation_arguments_and_expiry() {
        let arguments = serde_json::json!({"command":"true","interactive":false});
        let generation = [9; 32];
        let target = BridgeTarget::new("l5", "default").unwrap();
        let authorization = auth(&arguments, generation);
        let validated = validate_authorization(
            authorization.clone(),
            ValidationContext {
                client_id: "client-a",
                remote_start_trust_epoch: Some(7),
                target: &target,
                generation,
                now_unix_ms: 150,
                operation: OperationId::Bash,
                class: StartClass::Submit,
                arguments: &arguments,
                control_ref: None,
            },
        )
        .unwrap();
        assert_ne!(validated.claim_key(), [0; 32]);
        let old_generation = validate_authorization(
            authorization.clone(),
            ValidationContext {
                client_id: "client-a",
                remote_start_trust_epoch: Some(7),
                target: &target,
                generation: [8; 32],
                now_unix_ms: 150,
                operation: OperationId::Bash,
                class: StartClass::Submit,
                arguments: &arguments,
                control_ref: None,
            },
        )
        .unwrap();
        assert!(!old_generation.new_claim_authorized());
        assert!(
            old_generation
                .new_claim_denial()
                .is_some_and(|reason| reason.contains("old runtime generation"))
        );
        let expired = validate_authorization(
            authorization.clone(),
            ValidationContext {
                client_id: "client-a",
                remote_start_trust_epoch: Some(7),
                target: &target,
                generation,
                now_unix_ms: 201,
                operation: OperationId::Bash,
                class: StartClass::Submit,
                arguments: &arguments,
                control_ref: None,
            },
        )
        .unwrap();
        assert!(!expired.new_claim_authorized());
        assert!(
            expired
                .new_claim_denial()
                .is_some_and(|reason| reason.contains("lease is not current"))
        );
        assert!(
            validate_authorization(
                authorization.clone(),
                ValidationContext {
                    client_id: "client-a",
                    remote_start_trust_epoch: Some(7),
                    target: &target,
                    generation,
                    now_unix_ms: 150,
                    operation: OperationId::Batch,
                    class: StartClass::Submit,
                    arguments: &arguments,
                    control_ref: None,
                },
            )
            .is_err()
        );
        let changed = serde_json::json!({"command":"false","interactive":false});
        assert!(
            validate_authorization(
                authorization,
                ValidationContext {
                    client_id: "client-a",
                    remote_start_trust_epoch: Some(7),
                    target: &target,
                    generation,
                    now_unix_ms: 150,
                    operation: OperationId::Bash,
                    class: StartClass::Submit,
                    arguments: &changed,
                    control_ref: None,
                },
            )
            .is_err()
        );
    }

    #[test]
    fn job_manager_remote_start_claim_is_single_use_and_semantic_bound() {
        let arguments = serde_json::json!({"command":"true"});
        let generation = [9; 32];
        let now = now_unix_ms().unwrap();
        let mut authorization = auth(&arguments, generation);
        authorization.intent.target.issued_at_unix_ms = now;
        authorization.intent.target.expires_at_unix_ms = now + 1_000;
        authorization = RemoteStartAuthorization::new(
            authorization.intent.clone(),
            authorization.provenance.clone(),
        )
        .unwrap();
        let target = BridgeTarget::new("l5", "default").unwrap();
        let validated = validate_authorization(
            authorization,
            ValidationContext {
                client_id: "client-a",
                remote_start_trust_epoch: Some(7),
                target: &target,
                generation,
                now_unix_ms: now,
                operation: OperationId::Bash,
                class: StartClass::Submit,
                arguments: &arguments,
                control_ref: None,
            },
        )
        .unwrap();
        let manager = crate::jobs::JobManager::with_spool_limit(
            std::time::Duration::from_secs(60),
            crate::limits::DEFAULT_JOB_SPOOL_BYTES,
        )
        .unwrap();
        let guard = match manager.claim_remote_start(&validated).unwrap() {
            crate::jobs::RemoteStartDecision::New(guard) => guard,
            _ => panic!("first claim must be new"),
        };
        assert!(matches!(
            manager.claim_remote_start(&validated).unwrap(),
            crate::jobs::RemoteStartDecision::Unknown
        ));
        guard
            .mark_ready(crate::jobs::RemoteStartReceipt::Mcp(
                rmcp::model::CallToolResult::success(vec![rmcp::model::ContentBlock::text(
                    "j:abc process running rev=1",
                )]),
            ))
            .unwrap();
        assert!(matches!(
            manager.claim_remote_start(&validated).unwrap(),
            crate::jobs::RemoteStartDecision::Existing(crate::jobs::RemoteStartReceipt::Mcp(
                ref result
            )) if result.content[0].as_text().is_some_and(|text| text.text == "j:abc process running rev=1")
        ));

        manager.set_limits(
            std::time::Duration::ZERO,
            crate::limits::DEFAULT_JOB_SPOOL_BYTES,
        );
        assert!(matches!(
            manager.claim_remote_start(&validated).unwrap(),
            crate::jobs::RemoteStartDecision::Existing(crate::jobs::RemoteStartReceipt::Compacted)
        ));

        let mut changed = validated.clone();
        changed.semantic_hash[0] ^= 1;
        assert!(
            manager.claim_remote_start(&changed).is_err(),
            "compaction must retain semantic collision proof rather than reopen the effect slot"
        );
    }

    #[test]
    fn revoked_issuer_blocks_fresh_claim_but_exact_replay_survives() {
        let arguments = serde_json::json!({"command":"true"});
        let generation = [9; 32];
        let now = now_unix_ms().unwrap();
        let mut authorization = auth(&arguments, generation);
        authorization.intent.target.issued_at_unix_ms = now;
        authorization.intent.target.expires_at_unix_ms = now + 5_000;
        authorization = RemoteStartAuthorization::new(
            authorization.intent.clone(),
            authorization.provenance.clone(),
        )
        .unwrap();
        let target = BridgeTarget::new("l5", "default").unwrap();
        let trusted = validate_authorization(
            authorization.clone(),
            ValidationContext {
                client_id: "client-a",
                remote_start_trust_epoch: Some(7),
                target: &target,
                generation,
                now_unix_ms: now,
                operation: OperationId::Bash,
                class: StartClass::Submit,
                arguments: &arguments,
                control_ref: None,
            },
        )
        .unwrap();
        assert!(trusted.new_claim_authorized());

        let manager = crate::jobs::JobManager::with_spool_limit(
            std::time::Duration::from_secs(60),
            crate::limits::DEFAULT_JOB_SPOOL_BYTES,
        )
        .unwrap();
        let guard = match manager.claim_remote_start(&trusted).unwrap() {
            crate::jobs::RemoteStartDecision::New(guard) => guard,
            _ => panic!("trusted issuer must create the first claim"),
        };
        let receipt = rmcp::model::CallToolResult::success(vec![rmcp::model::ContentBlock::text(
            "j:abc process settled rev=2 outcome=ok",
        )]);
        guard
            .mark_ready(crate::jobs::RemoteStartReceipt::Mcp(receipt.clone()))
            .unwrap();

        let revoked_replay = validate_authorization(
            authorization.clone(),
            ValidationContext {
                client_id: "client-a",
                remote_start_trust_epoch: None,
                target: &target,
                generation,
                now_unix_ms: now,
                operation: OperationId::Bash,
                class: StartClass::Submit,
                arguments: &arguments,
                control_ref: None,
            },
        )
        .unwrap();
        assert!(!revoked_replay.new_claim_authorized());
        assert!(
            !revoked_replay.replay_source_current(),
            "revoked issuer keeps replay identity factual but loses protected payload disclosure"
        );
        assert!(matches!(
            manager.claim_remote_start(&revoked_replay).unwrap(),
            crate::jobs::RemoteStartDecision::Existing(crate::jobs::RemoteStartReceipt::Mcp(
                ref replay
            )) if *replay == receipt
        ));
        let expired_and_revoked = validate_authorization(
            authorization,
            ValidationContext {
                client_id: "client-a",
                remote_start_trust_epoch: None,
                target: &target,
                generation,
                now_unix_ms: revoked_replay
                    .authorization
                    .intent
                    .target
                    .expires_at_unix_ms,
                operation: OperationId::Bash,
                class: StartClass::Submit,
                arguments: &arguments,
                control_ref: None,
            },
        )
        .unwrap();
        assert!(
            !expired_and_revoked.replay_source_current(),
            "lease expiry must not hide simultaneous issuer revocation from disclosure"
        );

        let mut fresh_intent = revoked_replay.authorization.intent.clone();
        fresh_intent.effect_attempt_id = "fresh-after-revoke".into();
        fresh_intent.call_id = "fresh-after-revoke".into();
        let fresh_authorization = RemoteStartAuthorization::new(
            fresh_intent,
            revoked_replay.authorization.provenance.clone(),
        )
        .unwrap();
        let fresh = validate_authorization(
            fresh_authorization,
            ValidationContext {
                client_id: "client-a",
                remote_start_trust_epoch: None,
                target: &target,
                generation,
                now_unix_ms: now,
                operation: OperationId::Bash,
                class: StartClass::Submit,
                arguments: &arguments,
                control_ref: None,
            },
        )
        .unwrap();
        assert!(!fresh.new_claim_authorized());
        let error = match manager.claim_remote_start(&fresh) {
            Ok(_) => panic!("revoked issuer unexpectedly created a fresh Start claim"),
            Err(error) => error.to_string(),
        };
        assert!(error.contains("issuer is not currently trusted"), "{error}");
    }

    #[test]
    fn expired_lease_blocks_fresh_claim_but_retained_exact_replay_survives() {
        let arguments = serde_json::json!({"command":"true"});
        let generation = [9; 32];
        let now = now_unix_ms().unwrap();
        let mut authorization = auth(&arguments, generation);
        authorization.intent.target.issued_at_unix_ms = now;
        authorization.intent.target.expires_at_unix_ms = now + 1_000;
        authorization = RemoteStartAuthorization::new(
            authorization.intent.clone(),
            authorization.provenance.clone(),
        )
        .unwrap();
        let target = BridgeTarget::new("l5", "default").unwrap();
        let fresh = validate_authorization(
            authorization.clone(),
            ValidationContext {
                client_id: "client-a",
                remote_start_trust_epoch: Some(7),
                target: &target,
                generation,
                now_unix_ms: now,
                operation: OperationId::Bash,
                class: StartClass::Submit,
                arguments: &arguments,
                control_ref: None,
            },
        )
        .unwrap();
        let manager = crate::jobs::JobManager::with_spool_limit(
            std::time::Duration::from_secs(60),
            crate::limits::DEFAULT_JOB_SPOOL_BYTES,
        )
        .unwrap();
        let guard = match manager.claim_remote_start(&fresh).unwrap() {
            crate::jobs::RemoteStartDecision::New(guard) => guard,
            _ => panic!("fresh lease must create the initial claim"),
        };
        let receipt = rmcp::model::CallToolResult::success(vec![rmcp::model::ContentBlock::text(
            "j:abc process settled rev=2 outcome=ok",
        )]);
        guard
            .mark_ready(crate::jobs::RemoteStartReceipt::Mcp(receipt.clone()))
            .unwrap();

        let expired = validate_authorization(
            authorization.clone(),
            ValidationContext {
                client_id: "client-a",
                remote_start_trust_epoch: None,
                target: &target,
                generation,
                now_unix_ms: now + 1_000,
                operation: OperationId::Bash,
                class: StartClass::Submit,
                arguments: &arguments,
                control_ref: None,
            },
        )
        .unwrap();
        assert!(!expired.new_claim_authorized());
        assert!(matches!(
            manager.claim_remote_start(&expired).unwrap(),
            crate::jobs::RemoteStartDecision::Existing(crate::jobs::RemoteStartReceipt::Mcp(
                ref replay
            )) if *replay == receipt
        ));

        let mut fresh_intent = authorization.intent.clone();
        fresh_intent.call_id = "expired-fresh".into();
        fresh_intent.effect_attempt_id = "expired-fresh".into();
        let expired_fresh = validate_authorization(
            RemoteStartAuthorization::new(fresh_intent, authorization.provenance.clone()).unwrap(),
            ValidationContext {
                client_id: "client-a",
                remote_start_trust_epoch: Some(7),
                target: &target,
                generation,
                now_unix_ms: now + 1_000,
                operation: OperationId::Bash,
                class: StartClass::Submit,
                arguments: &arguments,
                control_ref: None,
            },
        )
        .unwrap();
        let error = match manager.claim_remote_start(&expired_fresh) {
            Ok(_) => panic!("expired lease unexpectedly created a fresh Start claim"),
            Err(error) => error.to_string(),
        };
        assert!(error.contains("lease is not current"), "{error}");
    }

    #[test]
    fn old_generation_authorization_is_rejected_after_runtime_restart() {
        let arguments = serde_json::json!({"command":"true"});
        let first = crate::jobs::JobManager::with_spool_limit(
            std::time::Duration::from_secs(60),
            crate::limits::DEFAULT_JOB_SPOOL_BYTES,
        )
        .unwrap();
        let second = crate::jobs::JobManager::with_spool_limit(
            std::time::Duration::from_secs(60),
            crate::limits::DEFAULT_JOB_SPOOL_BYTES,
        )
        .unwrap();
        let first_generation = first.runtime_generation_fingerprint();
        let second_generation = second.runtime_generation_fingerprint();
        assert_ne!(first_generation, second_generation);

        let now = now_unix_ms().unwrap();
        let mut authorization = auth(&arguments, first_generation);
        authorization.intent.target.issued_at_unix_ms = now;
        authorization.intent.target.expires_at_unix_ms = now + 1_000;
        authorization = RemoteStartAuthorization::new(
            authorization.intent.clone(),
            authorization.provenance.clone(),
        )
        .unwrap();
        let target = BridgeTarget::new("l5", "default").unwrap();
        let stale = validate_authorization(
            authorization,
            ValidationContext {
                client_id: "client-a",
                remote_start_trust_epoch: Some(7),
                target: &target,
                generation: second_generation,
                now_unix_ms: now,
                operation: OperationId::Bash,
                class: StartClass::Submit,
                arguments: &arguments,
                control_ref: None,
            },
        )
        .unwrap();
        assert!(!stale.new_claim_authorized());
        let error = match second.claim_remote_start(&stale) {
            Ok(_) => panic!("old runtime proof unexpectedly created a new Start claim"),
            Err(error) => error.to_string(),
        };
        assert!(error.contains("old runtime generation"), "{error}");
    }

    #[test]
    fn finite_control_target_extracts_only_exact_job_and_node() {
        assert_eq!(
            finite_suffix("/d:d/i:i/p:p/a:0/j:abc/n:3").unwrap(),
            ("abc".into(), Some(3))
        );
        assert_eq!(
            finite_suffix("/d:d/i:i/p:p/a:0/j:abc").unwrap(),
            ("abc".into(), None)
        );
        assert!(finite_suffix("/d:d/i:i/p:p/a:0/t:7").is_err());
    }
}
