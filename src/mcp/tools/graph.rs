//! Model-facing canonical Graph adapter. Shared aw-tools owns Graph semantics,
//! deterministic scheduling, status and retained output; dotlink owns policy,
//! process custody, transport exposure and replay.

use std::{panic::AssertUnwindSafe, path::Path, process::Stdio, sync::Arc, time::Duration};

use anyhow::{Result, anyhow, bail};
#[cfg(windows)]
use aw_tools::filesystem::access::Action;
use aw_tools::filesystem::{
    backend::MetadataBackend as _,
    batch::{Graph, NodeTool, Status},
    footprint::{Access as FootprintAccess, Extent, Footprint, Scope},
    job::{JournalError, Lifecycle, NodeState},
    job_manager::{
        JobManager as SharedJobManager, NodePlan, NodePolicy, OutputStream, SemanticBudget,
        StartRequest, plan_graph,
    },
    status::Primary,
};
use futures_util::{FutureExt as _, StreamExt as _, stream::FuturesUnordered};
use rmcp::schemars;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::{sync::oneshot, time::Instant};

use super::{
    LocalMachine, dot, resolve_capped, shared_native,
    shell::{ShellArgs, ShellJobRun},
};
use crate::jobs::{JobCompletion, JobHandle, JobStart, SharedShellWriters};

const DEFAULT_GRAPH_WIDTH: usize = 8;
const SHELL_RESULT_BYTES: usize = 4 * 1024;

#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct GraphArgs {
    pub(crate) ops: Vec<Value>,
    /// Return an exposed Job handoff after admission instead of waiting for the
    /// Graph result in this tools/call.
    #[serde(default)]
    pub(crate) background: bool,
    /// Optional display metadata only; never part of Graph node semantics.
    #[serde(default)]
    pub(crate) title: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct GraphShellArgs {
    command: String,
    #[serde(default = "dot")]
    dir: String,
    #[serde(default)]
    stdin: Option<String>,
    #[serde(default)]
    timeout_secs: Option<u64>,
    #[serde(default)]
    max_output_bytes: Option<usize>,
    #[serde(default)]
    writes: Option<Vec<String>>,
    #[serde(default)]
    interactive: bool,
}

impl GraphShellArgs {
    fn shell_args(&self) -> ShellArgs {
        ShellArgs {
            command: self.command.clone(),
            dir: self.dir.clone(),
            stdin: self.stdin.clone(),
            timeout_secs: self.timeout_secs,
            max_output_bytes: self.max_output_bytes,
            writes: self.writes.clone(),
        }
    }
}

#[derive(Clone)]
struct PreparedShellNode {
    args: ShellArgs,
    interactive: bool,
    footprint: Footprint,
    timeout_secs: u64,
    output_limit: usize,
    guard_generation: Option<[u8; 32]>,
    runtime_binding: [u8; 32],
}

#[derive(Clone)]
enum PreparedNode {
    Native(Box<shared_native::PreparedNativeGraphNode>),
    Shell(Box<PreparedShellNode>),
}

impl PreparedNode {
    fn policy(&self, authority: [u8; 32]) -> NodePolicy {
        match self {
            Self::Native(node) => NodePolicy {
                budget: SemanticBudget {
                    work_bytes: aw_tools::mutation::MAX_MUTATION_PLAN_BYTES as u64,
                    result_bytes: node.result_bytes(),
                },
                footprint: node.footprint.clone(),
                registry_generation: 1,
                authority_binding: authority,
                runtime_binding: authority,
                task_result_eligible: false,
            },
            Self::Shell(node) => NodePolicy {
                budget: SemanticBudget {
                    work_bytes: (node.output_limit as u64).saturating_mul(2).max(1),
                    result_bytes: SHELL_RESULT_BYTES,
                },
                footprint: node.footprint.clone(),
                registry_generation: 1,
                authority_binding: authority,
                runtime_binding: node.runtime_binding,
                task_result_eligible: false,
            },
        }
    }
}

pub(crate) struct PreparedGraph {
    graph: Graph,
    plans: Vec<NodePlan>,
    nodes: Arc<Vec<PreparedNode>>,
    width: usize,
    shell_guard_generation: Option<[u8; 32]>,
}

#[derive(Clone)]
struct GraphRuntime {
    shell: super::shell::ShellRuntime,
    operation_gate: Arc<tokio::sync::RwLock<()>>,
    serialize: bool,
    nodes: Arc<Vec<PreparedNode>>,
}

fn resolve_node(machine: &LocalMachine, name: &str) -> Option<NodeTool> {
    if machine.has_tool_for_policy(name)
        && let Some(tool) = shared_native::graph_core_tool(name)
    {
        return Some(NodeTool::Native(tool));
    }
    machine
        .launchers
        .shell_tool
        .filter(|tool| tool.name() == name && machine.has_tool_for_policy(name))
}

fn shell_footprint(
    machine: &LocalMachine,
    args: &GraphShellArgs,
    working_dir: &Path,
) -> Result<Footprint> {
    use aw_tools::filesystem::write_hint::{WriteHint, normalize};

    let writes = normalize(working_dir, args.writes.as_deref())
        .map_err(|_| anyhow!("invalid shell writes scheduling hint"))?;
    let WriteHint::Known(normalized) = writes else {
        return Ok(Footprint::unknown(false, true));
    };
    if normalized.is_empty() {
        return Ok(Footprint::none());
    }

    // The hint is scheduling metadata, never authority. Reuse the model-path
    // spelling only when it can be mapped into the same backend scheduling
    // namespace as native nodes. A valid absolute/out-of-authority/otherwise
    // unrepresentable hint stays conservative instead of rejecting the shell.
    let Some(raw) = args.writes.as_ref() else {
        unreachable!("known write hint came from an absent scope list")
    };
    let mut scopes = Vec::with_capacity(raw.len());
    for write in raw {
        let Ok(model_path) = aw_tools::filesystem::path::join(&args.dir, write) else {
            return Ok(Footprint::unknown(false, true));
        };
        let Ok(path) = machine.shared_fs.scheduling_path(&model_path) else {
            return Ok(Footprint::unknown(false, true));
        };
        scopes.push(Scope {
            path,
            access: FootprintAccess::Write,
            extent: Extent::Subtree,
        });
    }
    Ok(Footprint::new(scopes, false, false).unwrap_or_else(|_| Footprint::unknown(false, true)))
}

fn validate_shell_arguments(machine: &LocalMachine, arguments: &Value) -> bool {
    let Ok(args) = serde_json::from_value::<GraphShellArgs>(arguments.clone()) else {
        return false;
    };
    if args
        .stdin
        .as_ref()
        .is_some_and(|stdin| stdin.len() > machine.config.max_write_bytes)
    {
        return false;
    }
    // Full dir/executable authority is prepared asynchronously below. Here the
    // Graph compiler only validates the model-visible shape and hint syntax.
    if aw_tools::filesystem::path::validate(&args.dir).is_err() {
        return false;
    }
    let cwd = if args.dir == "." {
        machine.config.access.base_dir.clone()
    } else {
        machine.config.access.base_dir.join(&args.dir)
    };
    aw_tools::filesystem::write_hint::normalize(&cwd, args.writes.as_deref()).is_ok()
}

fn runtime_binding(
    authority: [u8; 32],
    tool: NodeTool,
    guard_generation: Option<[u8; 32]>,
) -> [u8; 32] {
    let mut bytes = b"dotlink-graph-runtime-v1\0".to_vec();
    bytes.extend_from_slice(&authority);
    bytes.extend_from_slice(tool.name().as_bytes());
    bytes.extend_from_slice(&guard_generation.unwrap_or([0; 32]));
    let mut binding = crate::mcp::sha256(&bytes);
    if binding == [0; 32] {
        binding[31] = 1;
    }
    binding
}

async fn prepare_shell_node(
    machine: &LocalMachine,
    tool: NodeTool,
    arguments: &Value,
    authority: [u8; 32],
) -> Result<PreparedShellNode> {
    let args: GraphShellArgs = serde_json::from_value(arguments.clone())?;
    let shell_args = args.shell_args();
    let runtime = machine.shell_runtime();
    let powershell = matches!(tool, NodeTool::Pwsh | NodeTool::Powershell);
    let (_command, working_dir, guard_generation) = runtime
        .prepare_shell_command(&shell_args, powershell)
        .await?;
    let footprint = shell_footprint(machine, &args, &working_dir)?;
    let timeout_secs = resolve_capped(
        shell_args.timeout_secs,
        machine.config.max_shell_timeout_secs,
    );
    let output_limit = runtime.output_limit(shell_args.max_output_bytes);
    Ok(PreparedShellNode {
        args: shell_args,
        interactive: args.interactive,
        footprint,
        timeout_secs,
        output_limit,
        guard_generation,
        runtime_binding: runtime_binding(authority, tool, guard_generation),
    })
}

async fn prepare_compiled(machine: &LocalMachine, graph: Graph) -> Result<PreparedGraph> {
    let authority = machine.jobs.policy_fingerprint()?;
    if authority == [0; 32] {
        bail!("Graph execution authority is invalid");
    }
    let mut nodes = Vec::with_capacity(graph.steps().len());
    let mut shell_guard_generation = None;
    for step in graph.steps() {
        if resolve_node(machine, step.tool.name()) != Some(step.tool) {
            bail!("Graph tool {} is no longer granted", step.tool.name());
        }
        let valid = if step.tool.native().is_some() {
            shared_native::validate_graph_arguments(machine, step.tool.name(), &step.arguments)
        } else {
            validate_shell_arguments(machine, &step.arguments)
        };
        if !valid {
            bail!(
                "Graph node {} is no longer valid under current policy",
                step.tool.name()
            );
        }
        let prepared = if step.tool.native().is_some() {
            PreparedNode::Native(Box::new(shared_native::prepare_graph_node(
                machine,
                step.tool.name(),
                &step.arguments,
            )?))
        } else {
            let shell = prepare_shell_node(machine, step.tool, &step.arguments, authority).await?;
            if let Some(generation) = shell.guard_generation {
                if shell_guard_generation.is_some_and(|existing| existing != generation) {
                    bail!("shell guard generation changed during Graph preflight");
                }
                shell_guard_generation = Some(generation);
            }
            PreparedNode::Shell(Box::new(shell))
        };
        nodes.push(prepared);
    }
    let plans = plan_graph(&graph, |coordinate, _| {
        Ok::<_, ()>(nodes[coordinate - 1].policy(authority))
    })
    .map_err(|error| anyhow!("cannot capture Graph node policy: {error:?}"))?;
    let width = if machine.serialize {
        1
    } else {
        DEFAULT_GRAPH_WIDTH.min(nodes.len().max(1))
    };
    Ok(PreparedGraph {
        graph,
        plans,
        nodes: Arc::new(nodes),
        width,
        shell_guard_generation,
    })
}

pub(crate) async fn prepare(machine: &LocalMachine, ops: Vec<Value>) -> Result<PreparedGraph> {
    let input = json!({"ops": ops});
    let graph = Graph::compile_with(
        &input,
        |name| resolve_node(machine, name),
        |node, arguments| {
            if node.native().is_some() {
                shared_native::validate_graph_arguments(machine, node.name(), arguments)
            } else {
                validate_shell_arguments(machine, arguments)
            }
        },
    )
    .map_err(|error| anyhow!("invalid canonical Graph: {error:?}"))?;
    prepare_compiled(machine, graph).await
}

#[cfg(test)]
pub(crate) async fn prepare_existing(
    machine: &LocalMachine,
    graph: Graph,
) -> Result<PreparedGraph> {
    prepare_compiled(machine, graph).await
}

fn next_sequence(manager: &SharedJobManager, id: &str) -> Result<u64> {
    crate::jobs::next_shared_sequence(manager, id)
        .map_err(|error| anyhow!("cannot advance Graph sequence: {error}"))
}

fn settle_unlaunched(job: &JobHandle, coordinate: usize, claimed_sequence: u64) -> Result<()> {
    job.with_shared_transition(|manager| {
        manager
            .settle_unlaunched(
                &job.id,
                coordinate,
                claimed_sequence,
                next_sequence(manager, &job.id)?,
                |_, sequence| {
                    if sequence == claimed_sequence {
                        Ok(())
                    } else {
                        Err(aw_tools::filesystem::job::Error::StaleGeneration)
                    }
                },
                |_, _| Ok::<_, JournalError>(()),
            )
            .map_err(|error| anyhow!("cannot settle unlaunched Graph node: {error:?}"))
    })
}

fn settle_running(
    job: &JobHandle,
    coordinate: usize,
    running_sequence: u64,
    status: Status,
) -> Result<()> {
    job.with_shared_transition(|manager| {
        manager
            .settle(
                &job.id,
                coordinate,
                NodeState::Running {
                    sequence: running_sequence,
                },
                status,
                next_sequence(manager, &job.id)?,
                |_, _| Ok::<_, JournalError>(()),
            )
            .map_err(|error| anyhow!("cannot settle running Graph node: {error:?}"))
    })
}

fn finish_if_present(
    manager: &SharedJobManager,
    id: &str,
    coordinate: usize,
    stream: OutputStream,
) {
    let _ = manager.finish_output(id, coordinate, stream);
}

fn settle_prelaunch_failure(
    manager: &SharedJobManager,
    job: &JobHandle,
    coordinate: usize,
    claimed_sequence: u64,
    message: &str,
) -> Result<()> {
    let id = &job.id;
    let result_ready = match manager.output_writer(id, coordinate, OutputStream::Result) {
        Ok(_) => true,
        Err(aw_tools::filesystem::job_manager::Error::OutputMissing) => {
            manager.job_output_owner(id).ok().is_some_and(|owner| {
                manager
                    .attach_output_reserved(
                        id,
                        coordinate,
                        OutputStream::Result,
                        [owner],
                        message.len().max(1) as u64,
                    )
                    .is_ok()
            })
        }
        Err(_) => false,
    };
    if result_ready {
        let _ = manager.append_output(id, coordinate, OutputStream::Result, message.as_bytes());
        let _ = manager.finish_output(id, coordinate, OutputStream::Result);
    } else {
        // If an output object exists but became unavailable concurrently, still
        // make a best effort to seal it so terminal cleanup is never pinned by
        // this failure path.
        finish_if_present(manager, id, coordinate, OutputStream::Result);
    }
    // No Start checkpoint has been committed, so this node is positively
    // known to be unlaunched. Keep the diagnostic output, but preserve the
    // canonical state-machine proof as Skip rather than inventing Fail/Unknown.
    settle_unlaunched(job, coordinate, claimed_sequence)
}

fn start_native(job: &JobHandle, coordinate: usize, claimed_sequence: u64) -> Result<u64> {
    job.with_shared_transition(|manager| {
        let running_sequence = next_sequence(manager, &job.id)?;
        manager
            .start(
                &job.id,
                StartRequest {
                    coordinate,
                    claimed_sequence,
                    source_live: true,
                    launch_authorized: true,
                    sequence: running_sequence,
                },
                |_, _| Ok::<_, JournalError>(()),
            )
            .map_err(|error| anyhow!("cannot start Graph node: {error:?}"))?;
        Ok(running_sequence)
    })
}

async fn execute_native(
    manager: Arc<SharedJobManager>,
    id: String,
    coordinate: usize,
    claimed_sequence: u64,
    node: shared_native::PreparedNativeGraphNode,
    job: JobHandle,
) -> Result<()> {
    let owner = manager
        .job_output_owner(&id)
        .map_err(|error| anyhow!("cannot derive Graph output owner: {error:?}"))?;
    if let Err(error) = manager.attach_output_reserved(
        &id,
        coordinate,
        OutputStream::Result,
        [owner],
        node.result_bytes() as u64,
    ) {
        return settle_prelaunch_failure(
            &manager,
            &job,
            coordinate,
            claimed_sequence,
            &format!("fail result capacity unavailable: {error:?}"),
        );
    }
    if job.cancel.is_cancelled() {
        finish_if_present(&manager, &id, coordinate, OutputStream::Result);
        return settle_unlaunched(&job, coordinate, claimed_sequence);
    }
    if !node.footprint_is_current() {
        return settle_prelaunch_failure(
            &manager,
            &job,
            coordinate,
            claimed_sequence,
            "fail filesystem topology changed after Graph preflight; restart the Graph",
        );
    }
    let running_sequence = start_native(&job, coordinate, claimed_sequence)?;
    let cancellation = job.cancel.token();
    let outcome = crate::blocking::run(|| node.execute_graph(&cancellation));
    if outcome.text.len() > node.result_bytes() {
        let _ = manager.mark_output_failed(&id, coordinate, OutputStream::Result, 0);
        finish_if_present(&manager, &id, coordinate, OutputStream::Result);
        return settle_running(
            &job,
            coordinate,
            running_sequence,
            Status {
                primary: Primary::Unknown,
                more: false,
                incomplete: false,
            },
        );
    }
    let retained = manager
        .append_output(
            &id,
            coordinate,
            OutputStream::Result,
            outcome.text.as_bytes(),
        )
        .map_err(|error| anyhow!("cannot retain native Graph result: {error:?}"))?;
    manager
        .finish_output(&id, coordinate, OutputStream::Result)
        .map_err(|error| anyhow!("cannot seal native Graph result: {error:?}"))?;
    if retained != outcome.text.len() {
        return settle_running(
            &job,
            coordinate,
            running_sequence,
            Status {
                primary: Primary::Unknown,
                more: false,
                incomplete: false,
            },
        );
    }
    settle_running(&job, coordinate, running_sequence, outcome.status)
}

async fn execute_shell(
    runtime: GraphRuntime,
    manager: Arc<SharedJobManager>,
    id: String,
    coordinate: usize,
    claimed_sequence: u64,
    node: PreparedShellNode,
    job: JobHandle,
) -> Result<()> {
    let owner = manager
        .job_output_owner(&id)
        .map_err(|error| anyhow!("cannot derive shell Graph output owner: {error:?}"))?;
    let (stdout, stderr) = match manager.attach_shell_outputs(
        &id,
        coordinate,
        owner,
        SHELL_RESULT_BYTES as u64,
        node.output_limit as u64,
    ) {
        Ok(writers) => writers,
        Err(error) => {
            return settle_prelaunch_failure(
                &manager,
                &job,
                coordinate,
                claimed_sequence,
                &format!("fail shell output capacity unavailable: {error:?}"),
            );
        }
    };
    let (mut command, working_dir, guard_generation) = match runtime
        .shell
        .prepare_shell_command(&node.args, cfg!(windows))
        .await
    {
        Ok(prepared) => prepared,
        Err(error) => {
            finish_if_present(&manager, &id, coordinate, OutputStream::Stdout);
            finish_if_present(&manager, &id, coordinate, OutputStream::Stderr);
            return settle_prelaunch_failure(
                &manager,
                &job,
                coordinate,
                claimed_sequence,
                &format!("fail shell preparation changed: {error}"),
            );
        }
    };
    if guard_generation != node.guard_generation {
        finish_if_present(&manager, &id, coordinate, OutputStream::Stdout);
        finish_if_present(&manager, &id, coordinate, OutputStream::Stderr);
        return settle_prelaunch_failure(
            &manager,
            &job,
            coordinate,
            claimed_sequence,
            "fail shell authority changed after Graph preflight",
        );
    }
    crate::shell_process::configure(&mut command);
    crate::credentials::hide_from_child(&mut command)
        .stdin(if node.args.stdin.is_some() || node.interactive {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let Some(deadline) = Instant::now().checked_add(Duration::from_secs(node.timeout_secs)) else {
        finish_if_present(&manager, &id, coordinate, OutputStream::Stdout);
        finish_if_present(&manager, &id, coordinate, OutputStream::Stderr);
        return settle_prelaunch_failure(
            &manager,
            &job,
            coordinate,
            claimed_sequence,
            "fail shell timeout is unschedulable",
        );
    };
    let context = runtime
        .shell
        .job_result_context(&working_dir, node.timeout_secs);
    let completion = LocalMachine::run_shell_job(ShellJobRun {
        command,
        log: runtime.shell.log_config(),
        stdin_text: node.args.stdin,
        interactive: node.interactive,
        deadline,
        job: job.clone(),
        lifecycle_id: format!("{}/n:{coordinate}", job.id),
        coordinate,
        shared_writers: SharedShellWriters { stdout, stderr },
        result_context: context,
    })
    .await;
    let terminal =
        (!completion.spawn_failed).then(|| crate::jobs::shared_process_terminal(&completion));
    let (compact, status) = match terminal {
        Some(terminal) => (terminal.compact_result(), terminal.status()),
        None => (
            "fail launch=spawn_failed".to_owned(),
            Status {
                primary: Primary::Fail,
                more: false,
                incomplete: false,
            },
        ),
    };
    if manager
        .append_output(&id, coordinate, OutputStream::Result, compact.as_bytes())
        .is_err()
    {
        let _ = manager.mark_output_failed(&id, coordinate, OutputStream::Result, 0);
    }
    finish_if_present(&manager, &id, coordinate, OutputStream::Result);
    finish_if_present(&manager, &id, coordinate, OutputStream::Stdout);
    finish_if_present(&manager, &id, coordinate, OutputStream::Stderr);

    job.with_shared_transition(|shared| match shared.node_state(&id, coordinate) {
        Ok(NodeState::Running { sequence }) => match terminal {
            Some(terminal) => shared
                .settle_process(
                    &id,
                    coordinate,
                    NodeState::Running { sequence },
                    terminal,
                    next_sequence(shared, &id)?,
                    |_, _| Ok::<_, JournalError>(()),
                )
                .map(|_| ())
                .map_err(|error| anyhow!("cannot settle ShellNode: {error:?}")),
            None => shared
                .settle(
                    &id,
                    coordinate,
                    NodeState::Running { sequence },
                    status,
                    next_sequence(shared, &id)?,
                    |_, _| Ok::<_, JournalError>(()),
                )
                .map_err(|error| anyhow!("cannot settle running ShellNode: {error:?}")),
        },
        Ok(NodeState::Claimed { sequence }) => shared
            .settle(
                &id,
                coordinate,
                NodeState::Claimed { sequence },
                Status {
                    primary: Primary::Unknown,
                    more: false,
                    incomplete: false,
                },
                next_sequence(shared, &id)?,
                |_, _| Ok::<_, JournalError>(()),
            )
            .map_err(|error| anyhow!("cannot settle claimed ShellNode: {error:?}")),
        Ok(NodeState::Terminal { .. }) => Ok(()),
        Ok(NodeState::Pending) => bail!("ShellNode returned to pending after launch attempt"),
        Err(error) => Err(anyhow!(
            "ShellNode disappeared during settlement: {error:?}"
        )),
    })
}

async fn execute_claimed(
    runtime: GraphRuntime,
    manager: Arc<SharedJobManager>,
    id: String,
    coordinate: usize,
    claimed_sequence: u64,
    job: JobHandle,
) -> Result<()> {
    let node = runtime.nodes[coordinate - 1].clone();
    #[cfg(windows)]
    if !runtime.serialize {
        match &node {
            PreparedNode::Native(native) if native.tool().action() == Action::Read => {
                let _guard = runtime.operation_gate.read().await;
                return match node {
                    PreparedNode::Native(native) => {
                        execute_native(manager, id, coordinate, claimed_sequence, *native, job)
                            .await
                    }
                    PreparedNode::Shell(_) => unreachable!(),
                };
            }
            _ => {
                let _guard = runtime.operation_gate.write().await;
                return match node {
                    PreparedNode::Native(native) => {
                        execute_native(manager, id, coordinate, claimed_sequence, *native, job)
                            .await
                    }
                    PreparedNode::Shell(shell) => {
                        execute_shell(
                            runtime,
                            manager,
                            id,
                            coordinate,
                            claimed_sequence,
                            *shell,
                            job,
                        )
                        .await
                    }
                };
            }
        }
    }
    #[cfg(not(windows))]
    let _ = &runtime.operation_gate;
    match node {
        PreparedNode::Native(native) => {
            execute_native(manager, id, coordinate, claimed_sequence, *native, job).await
        }
        PreparedNode::Shell(shell) => {
            execute_shell(
                runtime,
                manager,
                id,
                coordinate,
                claimed_sequence,
                *shell,
                job,
            )
            .await
        }
    }
}

fn graph_output_streams_for_abort() -> [OutputStream; 3] {
    [
        OutputStream::Result,
        OutputStream::Stdout,
        OutputStream::Stderr,
    ]
}

fn settle_abort_inner(manager: &SharedJobManager, id: &str) -> Result<()> {
    if manager
        .runtime_snapshot(id)
        .map_err(|error| anyhow!("Graph abort status unavailable: {error:?}"))?
        .lifecycle
        == Lifecycle::Terminal
    {
        return Ok(());
    }
    let _ = manager.revoke(id, next_sequence(manager, id)?, |_, _| {
        Ok::<_, JournalError>(())
    });
    loop {
        let mut progressed = false;
        for coordinate in manager
            .skippable(id)
            .map_err(|error| anyhow!("Graph abort skip projection failed: {error:?}"))?
        {
            manager
                .skip(id, coordinate, next_sequence(manager, id)?, |_, _| {
                    Ok::<_, JournalError>(())
                })
                .map_err(|error| anyhow!("Graph abort skip failed: {error:?}"))?;
            progressed = true;
        }
        for (coordinate, state) in manager
            .pending_reattach(id)
            .map_err(|error| anyhow!("Graph abort in-flight projection failed: {error:?}"))?
        {
            match state {
                NodeState::Claimed { sequence } => {
                    for stream in graph_output_streams_for_abort() {
                        finish_if_present(manager, id, coordinate, stream);
                    }
                    // Claimed is still pre-effect: no Start checkpoint means the
                    // node is provably unlaunched even when the runner itself
                    // aborts. Preserve that proof as skip, not unknown.
                    manager
                        .settle_unlaunched(
                            id,
                            coordinate,
                            sequence,
                            next_sequence(manager, id)?,
                            |_, observed| {
                                if observed == sequence {
                                    Ok(())
                                } else {
                                    Err(aw_tools::filesystem::job::Error::StaleGeneration)
                                }
                            },
                            |_, _| Ok::<_, JournalError>(()),
                        )
                        .map_err(|error| {
                            anyhow!("Graph abort no-launch settle failed: {error:?}")
                        })?;
                    progressed = true;
                }
                NodeState::Running { sequence } => {
                    for stream in graph_output_streams_for_abort() {
                        finish_if_present(manager, id, coordinate, stream);
                    }
                    manager
                        .settle(
                            id,
                            coordinate,
                            NodeState::Running { sequence },
                            Status {
                                primary: Primary::Unknown,
                                more: false,
                                incomplete: false,
                            },
                            next_sequence(manager, id)?,
                            |_, _| Ok::<_, JournalError>(()),
                        )
                        .map_err(|error| anyhow!("Graph abort running settle failed: {error:?}"))?;
                    progressed = true;
                }
                NodeState::Pending | NodeState::Terminal { .. } => {}
            }
        }
        if manager
            .runtime_snapshot(id)
            .map_err(|error| anyhow!("Graph abort status unavailable: {error:?}"))?
            .lifecycle
            == Lifecycle::Terminal
        {
            return Ok(());
        }
        if !progressed {
            bail!("Graph abort could not settle authoritative shared state");
        }
    }
}

fn settle_abort(job: &JobHandle) -> Result<()> {
    job.with_shared_transition(|manager| settle_abort_inner(manager, &job.id))
}

async fn run_inner(
    runtime: GraphRuntime,
    job: JobHandle,
) -> Result<aw_tools::filesystem::job_manager::Snapshot> {
    let manager = job.shared_manager()?;
    loop {
        let snapshot = manager
            .runtime_snapshot(&job.id)
            .map_err(|error| anyhow!("Graph status unavailable: {error:?}"))?;
        if snapshot.lifecycle == Lifecycle::Terminal {
            return Ok(snapshot);
        }
        if job.cancel.is_cancelled() && snapshot.lifecycle == Lifecycle::Running {
            job.with_shared_transition(|shared| {
                let _ = shared.cancel(&job.id, next_sequence(shared, &job.id)?, |_, _| {
                    Ok::<_, JournalError>(())
                });
                Ok(())
            })?;
        }
        for coordinate in manager
            .skippable(&job.id)
            .map_err(|error| anyhow!("Graph skip projection failed: {error:?}"))?
        {
            job.with_shared_transition(|shared| {
                shared
                    .skip(
                        &job.id,
                        coordinate,
                        next_sequence(shared, &job.id)?,
                        |_, _| Ok::<_, JournalError>(()),
                    )
                    .map_err(|error| anyhow!("Graph conditional skip failed: {error:?}"))
            })?;
        }
        let snapshot = manager
            .runtime_snapshot(&job.id)
            .map_err(|error| anyhow!("Graph status unavailable: {error:?}"))?;
        if snapshot.lifecycle == Lifecycle::Terminal {
            return Ok(snapshot);
        }
        let runnable = manager
            .runnable(&job.id)
            .map_err(|error| anyhow!("Graph runnable projection failed: {error:?}"))?;
        if runnable.is_empty() {
            tokio::task::yield_now().await;
            continue;
        }
        let mut wave = FuturesUnordered::new();
        for coordinate in runnable {
            let claimed_sequence = job.with_shared_transition(|shared| {
                let sequence = next_sequence(shared, &job.id)?;
                match shared.claim(&job.id, coordinate, true, sequence, |_, _| {
                    Ok::<_, JournalError>(())
                }) {
                    Ok(_) => Ok(Some(sequence)),
                    Err(aw_tools::filesystem::job_manager::Error::Busy) => Ok(None),
                    Err(error) => Err(anyhow!("Graph node claim failed: {error:?}")),
                }
            })?;
            let Some(claimed_sequence) = claimed_sequence else {
                break;
            };
            let node_runtime = runtime.clone();
            let node_manager = manager.clone();
            let node_id = job.id.clone();
            let node_job = job.clone();
            wave.push(async move {
                execute_claimed(
                    node_runtime,
                    node_manager,
                    node_id,
                    coordinate,
                    claimed_sequence,
                    node_job,
                )
                .await
            });
        }
        if wave.is_empty() {
            tokio::task::yield_now().await;
            continue;
        }
        let mut fatal = None;
        while let Some(outcome) = wave.next().await {
            if let Err(error) = outcome
                && fatal.is_none()
            {
                // Internal runner failure is not an ordinary Graph `fail`
                // result. Stop admitting work and cooperatively cancel the
                // already-running wave so process containment/output drains
                // quiesce before authoritative unknown settlement.
                fatal = Some(error);
                job.cancel.cancel_as(crate::jobs::CancelOrigin::Connector);
                let snapshot = manager
                    .runtime_snapshot(&job.id)
                    .map_err(|error| anyhow!("Graph status unavailable during abort: {error:?}"))?;
                if snapshot.lifecycle == Lifecycle::Running {
                    job.with_shared_transition(|shared| {
                        let _ = shared.cancel(&job.id, next_sequence(shared, &job.id)?, |_, _| {
                            Ok::<_, JournalError>(())
                        });
                        Ok(())
                    })?;
                }
            }
        }
        if let Some(error) = fatal {
            settle_abort(&job)?;
            return Err(error);
        }
    }
}

async fn run(
    runtime: GraphRuntime,
    job: JobHandle,
) -> Result<aw_tools::filesystem::job_manager::Snapshot> {
    if runtime.serialize {
        let gate = runtime.operation_gate.clone();
        let _guard = gate.write().await;
        run_inner(runtime, job).await
    } else {
        run_inner(runtime, job).await
    }
}

pub(crate) async fn start(
    machine: &LocalMachine,
    prepared: PreparedGraph,
    source_id: Option<String>,
    request_key: Option<String>,
    expose_before_run: bool,
) -> Result<JobStart> {
    let runtime = GraphRuntime {
        shell: machine.shell_runtime(),
        operation_gate: machine.operation_gate.clone(),
        serialize: machine.serialize,
        nodes: prepared.nodes.clone(),
    };
    let graph = prepared.graph;
    let plans = prepared.plans;
    let width = prepared.width;
    let guard_generation = prepared.shell_guard_generation;
    let (admitted_tx, admitted_rx) = oneshot::channel::<std::result::Result<(), String>>();
    let (go_tx, go_rx) = oneshot::channel::<()>();
    let started = machine.jobs.spawn(
        machine.config.max_shell_timeout_secs,
        machine.config.max_output_bytes,
        request_key,
        move |job| async move {
            let admitted = match source_id.as_deref() {
                Some(source) => {
                    job.restart_prepared_graph(source, graph, plans, width, guard_generation)
                }
                None => job.admit_prepared_graph(graph, plans, width, guard_generation, true),
            };
            match admitted {
                Ok(()) => {
                    let _ = admitted_tx.send(Ok(()));
                }
                Err(error) => {
                    let message = error.to_string();
                    let _ = admitted_tx.send(Err(message.clone()));
                    return JobCompletion::failed(message);
                }
            }
            if go_rx.await.is_err() {
                let _ = job.with_shared_transition(|manager| {
                    let sequence = next_sequence(manager, &job.id)?;
                    let _ =
                        manager.cancel(&job.id, sequence, |_, _| Ok::<_, JournalError>(()));
                    Ok(())
                });
                return JobCompletion::cancelled(Default::default(), Default::default());
            }
            match AssertUnwindSafe(run(runtime, job.clone()))
                .catch_unwind()
                .await
            {
                Ok(Ok(snapshot)) => JobCompletion::from_shared_graph(&snapshot),
                Ok(Err(error)) => {
                    let settlement = settle_abort(&job);
                    let message = match settlement {
                        Ok(()) => format!("Graph runner failed and shared state was settled: {error}"),
                        Err(settle_error) => format!(
                            "Graph runner failed: {error}; shared settlement also failed: {settle_error}"
                        ),
                    };
                    JobCompletion::internal_failed(message, Default::default(), Default::default())
                }
                Err(_) => {
                    let settlement = settle_abort(&job);
                    let message = match settlement {
                        Ok(()) => "Graph runner panicked; shared state settled unknown".to_owned(),
                        Err(error) => {
                            format!("Graph runner panicked and shared settlement failed: {error}")
                        }
                    };
                    JobCompletion::internal_failed(message, Default::default(), Default::default())
                }
            }
        },
    )?;
    if !started.is_new {
        return Ok(started);
    }
    match admitted_rx.await {
        Ok(Ok(())) => {}
        Ok(Err(error)) => bail!(error),
        Err(_) => bail!("Graph runner ended before shared admission"),
    }
    if expose_before_run {
        machine
            .jobs
            .expose_compatibility(&started.id)
            .map_err(|error| anyhow!(error.to_string()))?;
    }
    let _ = go_tx.send(());
    Ok(started)
}

pub(crate) fn input_schema(node_tools: &[NodeTool]) -> Value {
    let mut schema = aw_tools::filesystem::batch::parameters_for(node_tools.iter().copied());
    if let Some(properties) = schema.get_mut("properties").and_then(Value::as_object_mut) {
        properties.insert(
            "background".into(),
            serde_json::json!({
                "type":"boolean",
                "description":"Expose the admitted Graph Job immediately instead of waiting in this call."
            }),
        );
        properties.insert(
            "title".into(),
            serde_json::json!({
                "type":["string","null"],
                "maxLength":160,
                "description":"Optional display title; does not affect execution semantics."
            }),
        );
    }
    schema
}

pub(crate) fn allowed_node_tools(
    capabilities: crate::mcp::Capabilities,
    shell_tool: Option<NodeTool>,
) -> Vec<NodeTool> {
    let mut tools = Vec::new();
    for tool in aw_tools::filesystem::schema::Tool::GRAPH_NATIVE {
        let allowed = crate::mcp::TOOL_SPECS
            .iter()
            .find(|spec| spec.name == tool.name())
            .is_some_and(|spec| capabilities.allows(spec.capability));
        if allowed {
            tools.push(NodeTool::Native(tool));
        }
    }
    if capabilities.shell
        && let Some(shell) = shell_tool
    {
        tools.push(shell);
    }
    tools
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        jobs::{JobHandoff, JobWait},
        mcp::tools::test_support,
    };
    use std::{fs, path::PathBuf};

    #[test]
    fn preattached_shell_failure_seals_result_and_allows_terminal_cleanup() {
        use aw_tools::filesystem::{
            footprint::Footprint,
            job_manager::{Limits, NodePolicy, RuntimeAdmission},
        };
        let graph = Graph::compile_with(
            &json!({"ops":[["bash", {"command":"true"}]]}),
            |name| (name == "bash").then_some(NodeTool::Bash),
            |node, arguments| node == NodeTool::Bash && arguments.is_object(),
        )
        .unwrap();
        let plans = plan_graph(&graph, |_, _| {
            Ok::<_, ()>(NodePolicy {
                budget: SemanticBudget {
                    work_bytes: 1024,
                    result_bytes: SHELL_RESULT_BYTES,
                },
                footprint: Footprint::unknown(true, true),
                registry_generation: 1,
                authority_binding: [1; 32],
                runtime_binding: [2; 32],
                task_result_eligible: false,
            })
        })
        .unwrap();
        let manager = SharedJobManager::new(Limits::default()).unwrap();
        manager
            .admit_runtime(
                graph,
                plans,
                RuntimeAdmission {
                    id: "shell-prelaunch-fail",
                    runtime_session: "session",
                    epoch: 1,
                    admission_sequence: 1,
                    width: 1,
                    exposed: true,
                },
            )
            .unwrap();
        let claimed = next_sequence(&manager, "shell-prelaunch-fail").unwrap();
        manager
            .claim("shell-prelaunch-fail", 1, true, claimed, |_, _| {
                Ok::<_, JournalError>(())
            })
            .unwrap();
        let owner = manager.job_output_owner("shell-prelaunch-fail").unwrap();
        let (stdout, stderr) = manager
            .attach_shell_outputs(
                "shell-prelaunch-fail",
                1,
                owner,
                SHELL_RESULT_BYTES as u64,
                64,
            )
            .unwrap();
        stdout.finish().unwrap();
        stderr.finish().unwrap();
        let message = "fail shell authority changed after Graph preflight";
        manager
            .append_output(
                "shell-prelaunch-fail",
                1,
                OutputStream::Result,
                message.as_bytes(),
            )
            .unwrap();
        manager
            .finish_output("shell-prelaunch-fail", 1, OutputStream::Result)
            .unwrap();
        manager
            .settle_unlaunched(
                "shell-prelaunch-fail",
                1,
                claimed,
                next_sequence(&manager, "shell-prelaunch-fail").unwrap(),
                |_, observed| {
                    if observed == claimed {
                        Ok(())
                    } else {
                        Err(aw_tools::filesystem::job::Error::StaleGeneration)
                    }
                },
                |_, _| Ok::<_, JournalError>(()),
            )
            .unwrap();
        let (result, finished) = manager
            .output_summary("shell-prelaunch-fail", 1, OutputStream::Result)
            .unwrap();
        assert!(finished);
        assert!(result.retained_bytes > 0);
        assert!(matches!(
            manager.node_state("shell-prelaunch-fail", 1).unwrap(),
            NodeState::Terminal {
                status: Status {
                    primary: Primary::Skip,
                    ..
                },
                ..
            }
        ));
        for stream in [
            OutputStream::Result,
            OutputStream::Stdout,
            OutputStream::Stderr,
        ] {
            let (summary, _) = manager
                .output_summary("shell-prelaunch-fail", 1, stream)
                .unwrap();
            manager
                .acknowledge_output(
                    "shell-prelaunch-fail",
                    1,
                    stream,
                    owner,
                    summary.retained_bytes,
                )
                .unwrap();
        }
        manager.expire_terminal("shell-prelaunch-fail").unwrap();
        assert!(matches!(
            manager.runtime_snapshot("shell-prelaunch-fail"),
            Err(aw_tools::filesystem::job_manager::Error::NotFound)
        ));
    }

    #[test]
    fn claimed_but_unlaunched_node_settles_skip_not_fail() {
        use aw_tools::filesystem::{
            footprint::Footprint,
            job_manager::{Limits, NodePolicy, RuntimeAdmission},
        };
        let graph = Graph::compile(&json!({"ops":[["read", {"path":"a"}]]}), |_, _| true).unwrap();
        let plans = plan_graph(&graph, |_, _| {
            Ok::<_, ()>(NodePolicy {
                budget: SemanticBudget {
                    work_bytes: 1024,
                    result_bytes: 1024,
                },
                footprint: Footprint::none(),
                registry_generation: 1,
                authority_binding: [1; 32],
                runtime_binding: [2; 32],
                task_result_eligible: false,
            })
        })
        .unwrap();
        let manager = SharedJobManager::new(Limits::default()).unwrap();
        manager
            .admit_runtime(
                graph,
                plans,
                RuntimeAdmission {
                    id: "cancel-before-effect",
                    runtime_session: "session",
                    epoch: 1,
                    admission_sequence: 1,
                    width: 1,
                    exposed: true,
                },
            )
            .unwrap();
        let claimed = next_sequence(&manager, "cancel-before-effect").unwrap();
        manager
            .claim("cancel-before-effect", 1, true, claimed, |_, _| {
                Ok::<_, JournalError>(())
            })
            .unwrap();
        manager
            .settle_unlaunched(
                "cancel-before-effect",
                1,
                claimed,
                next_sequence(&manager, "cancel-before-effect").unwrap(),
                |_, observed| {
                    if observed == claimed {
                        Ok(())
                    } else {
                        Err(aw_tools::filesystem::job::Error::StaleGeneration)
                    }
                },
                |_, _| Ok::<_, JournalError>(()),
            )
            .unwrap();
        assert!(matches!(
            manager.node_state("cancel-before-effect", 1).unwrap(),
            NodeState::Terminal {
                status: Status {
                    primary: Primary::Skip,
                    ..
                },
                ..
            }
        ));
    }

    #[test]
    fn shell_write_hints_accept_absolute_scopes_and_preserve_empty_vs_unknown() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let machine = test_support::test_machine(root.clone());
        let absolute = root.join("absolute.txt");
        let absolute = absolute.to_str().unwrap().replace('\\', "/");
        let absolute_args = GraphShellArgs {
            command: "true".into(),
            dir: ".".into(),
            stdin: None,
            timeout_secs: None,
            max_output_bytes: None,
            writes: Some(vec![absolute]),
            interactive: false,
        };
        let absolute_footprint = shell_footprint(&machine, &absolute_args, &root).unwrap();
        assert!(!absolute_footprint.unknown_write());
        assert_eq!(absolute_footprint.scopes().len(), 1);

        let empty_args = GraphShellArgs {
            writes: Some(Vec::new()),
            ..absolute_args.clone()
        };
        let empty = shell_footprint(&machine, &empty_args, &root).unwrap();
        assert!(!empty.unknown_write());
        assert!(empty.scopes().is_empty());

        let unknown_args = GraphShellArgs {
            writes: None,
            ..absolute_args
        };
        let unknown = shell_footprint(&machine, &unknown_args, &root).unwrap();
        assert!(unknown.unknown_write());
    }

    #[test]
    fn shell_write_hint_validation_rejects_expansion_but_not_unmappable_advisory_scope() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let machine = test_support::test_machine(root.clone());
        let invalid = GraphShellArgs {
            command: "true".into(),
            dir: ".".into(),
            stdin: None,
            timeout_secs: None,
            max_output_bytes: None,
            writes: Some(vec!["src/*.rs".into()]),
            interactive: false,
        };
        assert!(shell_footprint(&machine, &invalid, &root).is_err());

        let child = root.join("child");
        fs::create_dir(&child).unwrap();
        let advisory = GraphShellArgs {
            writes: Some(vec!["../outside-advisory".into()]),
            ..invalid
        };
        let footprint = shell_footprint(&machine, &advisory, &child).unwrap();
        assert!(footprint.unknown_write());
        assert!(footprint.scopes().is_empty());
    }

    #[tokio::test]
    async fn native_graph_executes_dependencies_and_skips_false_branch() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        fs::write(root.join("a.txt"), "old").unwrap();
        fs::write(root.join("skipped.txt"), "skip").unwrap();
        let machine = test_support::test_machine(root.clone());
        let prepared = prepare(
            &machine,
            vec![
                json!(["write", {"path":"a.txt","content":"alpha"}, "*"]),
                json!(["read", {"path":"a.txt","numbered":false}, "1.ok"]),
                json!(["read", {"path":"skipped.txt","numbered":false}, "1.fail"]),
            ],
        )
        .await
        .unwrap();
        let started = start(&machine, prepared, None, None, true).await.unwrap();
        assert!(started.is_new);
        assert!(matches!(
            machine.jobs.replay_handoff(&started.id).unwrap(),
            Some(JobHandoff::Compatibility(_))
        ));
        assert!(matches!(
            machine.jobs.wait_until(&started.id, None).await.unwrap(),
            JobWait::Terminal
        ));
        assert_eq!(fs::read_to_string(root.join("a.txt")).unwrap(), "alpha");
        let rendered = machine
            .jobs
            .render_graph_result(&started.id, 64 * 1024)
            .unwrap()
            .unwrap();
        assert!(rendered.contains("2\nalpha"), "{rendered}");
        assert!(rendered.contains("3 skip condition"), "{rendered}");
        let snapshot = machine.jobs.shared_snapshot(&started.id).unwrap();
        assert_eq!(snapshot.finished, 3);
        assert_eq!(snapshot.summary, Some((Primary::Ok, false, false)));
    }

    #[tokio::test]
    async fn standalone_graph_root_result_is_synthesized_and_node_ack_is_delivery_fenced() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        fs::write(root.join("a.txt"), "old").unwrap();
        let machine = test_support::test_machine(root.clone());
        let prepared = prepare(
            &machine,
            vec![
                json!(["write", {"path":"a.txt","content":"alpha"}, "*"]),
                json!(["read", {"path":"a.txt","numbered":false}, "1.ok"]),
            ],
        )
        .await
        .unwrap();
        let started = start(&machine, prepared, None, None, true).await.unwrap();
        assert!(matches!(
            machine.jobs.wait_until(&started.id, None).await.unwrap(),
            JobWait::Terminal
        ));

        let aggregate: serde_json::Value = serde_json::from_str(
            &machine
                .jobs
                .standalone_graph_result_page(&started.id, 0, 64 * 1024, 64 * 1024, false)
                .unwrap(),
        )
        .unwrap();
        assert!(aggregate["node"].is_null(), "{aggregate}");
        assert_eq!(aggregate["stream"], "result");
        assert!(
            aggregate["data"].as_str().unwrap().contains("2\nalpha"),
            "{aggregate}"
        );
        let root_ack = machine
            .jobs
            .acknowledge_node_output(
                &started.id,
                None,
                Some(crate::jobs::JobOutputStream::Result),
                1,
            )
            .unwrap_err();
        assert!(
            root_ack.to_string().contains("synthesized aggregate"),
            "{root_ack}"
        );

        let unread_ack = machine
            .jobs
            .acknowledge_node_output(
                &started.id,
                Some(2),
                Some(crate::jobs::JobOutputStream::Result),
                1,
            )
            .unwrap_err();
        assert!(
            unread_ack.to_string().contains("exceeds bytes delivered"),
            "{unread_ack}"
        );

        let node_page: serde_json::Value = serde_json::from_str(
            &machine
                .jobs
                .output_node_page(
                    &started.id,
                    crate::jobs::OutputPageRequest {
                        node: Some(2),
                        stream: Some(crate::jobs::JobOutputStream::Result),
                        offset: 0,
                        requested_bytes: 1,
                        response_ceiling: 64 * 1024,
                        base64: false,
                    },
                )
                .unwrap(),
        )
        .unwrap();
        assert_eq!(node_page["next_offset"], 1);
        assert_eq!(node_page["data"], "a");

        let skip_ack = machine
            .jobs
            .acknowledge_node_output(
                &started.id,
                Some(2),
                Some(crate::jobs::JobOutputStream::Result),
                2,
            )
            .unwrap_err();
        assert!(
            skip_ack.to_string().contains("exceeds bytes delivered"),
            "{skip_ack}"
        );
        machine
            .jobs
            .acknowledge_node_output(
                &started.id,
                Some(2),
                Some(crate::jobs::JobOutputStream::Result),
                1,
            )
            .unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn native_graph_rejects_final_symlink_retarget_before_effect() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let first = root.join("first.txt");
        let second = root.join("second.txt");
        let alias = root.join("alias.txt");
        fs::write(&first, "first").unwrap();
        fs::write(&second, "second").unwrap();
        symlink("first.txt", &alias).unwrap();

        let machine = test_support::test_machine(root.clone());
        let prepared = prepare(
            &machine,
            vec![json!(["write", {"path":"alias.txt","content":"changed"}])],
        )
        .await
        .unwrap();

        fs::remove_file(&alias).unwrap();
        symlink("second.txt", &alias).unwrap();

        let started = start(&machine, prepared, None, None, true).await.unwrap();
        machine.jobs.wait_until(&started.id, None).await.unwrap();
        assert_eq!(fs::read_to_string(&first).unwrap(), "first");
        assert_eq!(fs::read_to_string(&second).unwrap(), "second");
        let snapshot = machine.jobs.shared_snapshot(&started.id).unwrap();
        assert_eq!(snapshot.summary, Some((Primary::Skip, false, false)));
    }

    #[tokio::test]
    async fn serialize_captures_graph_width_one_without_changing_graph_shape() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let mut machine = test_support::test_machine(root);
        machine.serialize = true;
        let prepared = prepare(
            &machine,
            vec![
                json!(["write", {"path":"a","content":"a"}, "*"]),
                json!(["write", {"path":"b","content":"b"}, "*"]),
                json!(["write", {"path":"c","content":"c"}, "*"]),
            ],
        )
        .await
        .unwrap();
        assert_eq!(prepared.graph.steps().len(), 3);
        assert_eq!(prepared.width, 1);
    }

    #[tokio::test]
    async fn restart_creates_distinct_execution_from_immutable_source_graph() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let machine = test_support::test_machine(root.clone());
        let prepared = prepare(
            &machine,
            vec![json!(["write", {"path":"restart.txt","content":"one"}])],
        )
        .await
        .unwrap();
        let first = start(&machine, prepared, None, None, true).await.unwrap();
        machine.jobs.wait_until(&first.id, None).await.unwrap();
        let graph = machine.jobs.shared_graph(&first.id).unwrap();
        let prepared = prepare_existing(&machine, graph).await.unwrap();
        let second = start(&machine, prepared, Some(first.id.clone()), None, true)
            .await
            .unwrap();
        assert_ne!(first.id, second.id);
        machine.jobs.wait_until(&second.id, None).await.unwrap();
        assert_eq!(fs::read_to_string(root.join("restart.txt")).unwrap(), "one");
        assert_eq!(
            machine.jobs.shared_snapshot(&second.id).unwrap().summary,
            Some((Primary::Ok, false, false))
        );
    }

    #[cfg(unix)]
    fn bash() -> PathBuf {
        std::env::var_os("AW_BASH")
            .map(PathBuf::from)
            .or_else(|| {
                ["/bin/bash", "/usr/bin/bash"]
                    .iter()
                    .map(PathBuf::from)
                    .find(|path| path.is_file())
            })
            .expect("Graph shell tests require bash")
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn shell_write_hint_orders_lower_shell_before_native_read() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        fs::write(root.join("x.txt"), "old").unwrap();
        let machine = test_support::test_machine_with_shell(root.clone(), bash());
        let prepared = prepare(
            &machine,
            vec![
                json!(["bash", {
                    "command":"sleep 0.03; printf shell > x.txt",
                    "writes":["x.txt"]
                }, "*"]),
                json!(["read", {"path":"x.txt","numbered":false}, "*"]),
            ],
        )
        .await
        .unwrap();
        let started = start(&machine, prepared, None, None, true).await.unwrap();
        machine.jobs.wait_until(&started.id, None).await.unwrap();
        let rendered = machine
            .jobs
            .render_graph_result(&started.id, 64 * 1024)
            .unwrap()
            .unwrap();
        assert!(rendered.contains("2\nshell"), "{rendered}");
        assert_eq!(fs::read_to_string(root.join("x.txt")).unwrap(), "shell");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn graph_cancel_stops_shell_before_late_side_effect() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let machine = test_support::test_machine_with_shell(root.clone(), bash());
        let prepared = prepare(
            &machine,
            vec![json!(["bash", {
                "command":"sleep 0.5; printf late > late.txt",
                "writes":["late.txt"]
            }])],
        )
        .await
        .unwrap();
        let started = start(&machine, prepared, None, None, true).await.unwrap();
        machine.jobs.cancel(&started.id).unwrap();
        machine.jobs.wait_until(&started.id, None).await.unwrap();
        tokio::time::sleep(Duration::from_millis(650)).await;
        assert!(!root.join("late.txt").exists());
        let snapshot = machine.jobs.shared_snapshot(&started.id).unwrap();
        assert_eq!(snapshot.lifecycle, Lifecycle::Terminal);
    }

    #[tokio::test]
    async fn synchronous_multinode_result_releases_every_shared_node_stream() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        fs::write(root.join("a.txt"), "alpha").unwrap();
        fs::write(root.join("b.txt"), "beta").unwrap();
        let machine = test_support::test_machine(root);
        let prepared = prepare(
            &machine,
            vec![
                json!(["read", {"path":"a.txt","numbered":false}, "*"]),
                json!(["read", {"path":"b.txt","numbered":false}, "*"]),
            ],
        )
        .await
        .unwrap();
        let started = start(&machine, prepared, None, None, false).await.unwrap();
        machine.jobs.wait_until(&started.id, None).await.unwrap();
        let before = machine.jobs.shared_spool_usage().unwrap().0;
        assert!(
            before >= 9,
            "expected retained native Graph results, got {before}"
        );
        assert!(
            machine
                .jobs
                .render_graph_result(&started.id, 64 * 1024)
                .unwrap()
                .is_some()
        );
        assert_eq!(
            machine.jobs.mark_synchronous_result(&started.id).unwrap(),
            crate::jobs::JobDisposition::Synchronous
        );
        assert_eq!(machine.jobs.shared_spool_usage().unwrap().0, 0);
    }

    #[test]
    fn fatal_runner_abort_settles_authoritative_shared_graph_terminal_unknown() {
        use aw_tools::filesystem::{
            footprint::Footprint,
            job_manager::{Limits, NodePolicy, RuntimeAdmission},
        };
        let graph = Graph::compile(
            &json!({"ops":[
                ["read", {"path":"a"}, "*"],
                ["read", {"path":"b"}, "*"]
            ]}),
            |_, _| true,
        )
        .unwrap();
        let plans = plan_graph(&graph, |_, _| {
            Ok::<_, ()>(NodePolicy {
                budget: SemanticBudget {
                    work_bytes: 1024,
                    result_bytes: 1024,
                },
                footprint: Footprint::none(),
                registry_generation: 1,
                authority_binding: [1; 32],
                runtime_binding: [2; 32],
                task_result_eligible: false,
            })
        })
        .unwrap();
        let manager = SharedJobManager::new(Limits::default()).unwrap();
        manager
            .admit_runtime(
                graph,
                plans,
                RuntimeAdmission {
                    id: "abort",
                    runtime_session: "session",
                    epoch: 1,
                    admission_sequence: 1,
                    width: 2,
                    exposed: true,
                },
            )
            .unwrap();
        let claimed = next_sequence(&manager, "abort").unwrap();
        manager
            .claim("abort", 1, true, claimed, |_, _| Ok::<_, JournalError>(()))
            .unwrap();
        let running = next_sequence(&manager, "abort").unwrap();
        manager
            .start(
                "abort",
                StartRequest {
                    coordinate: 1,
                    claimed_sequence: claimed,
                    source_live: true,
                    launch_authorized: true,
                    sequence: running,
                },
                |_, _| Ok::<_, JournalError>(()),
            )
            .unwrap();
        let claimed_two = next_sequence(&manager, "abort").unwrap();
        manager
            .claim("abort", 2, true, claimed_two, |_, _| {
                Ok::<_, JournalError>(())
            })
            .unwrap();
        settle_abort_inner(&manager, "abort").unwrap();
        let snapshot = manager.runtime_snapshot("abort").unwrap();
        assert_eq!(snapshot.lifecycle, Lifecycle::Terminal);
        assert!(matches!(
            manager.node_state("abort", 1).unwrap(),
            NodeState::Terminal {
                status: Status {
                    primary: Primary::Unknown,
                    ..
                },
                ..
            }
        ));
        assert!(matches!(
            manager.node_state("abort", 2).unwrap(),
            NodeState::Terminal {
                status: Status {
                    primary: Primary::Skip,
                    ..
                },
                ..
            }
        ));
    }
}
