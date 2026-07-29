use base64::Engine;
use diffy::Patch;
use opensrc_core::{
    Agent, CanonicalTool, PolicyDecision, PolicyEngine, PolicyEvaluation, ToolPolicy, ToolRequest,
};
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Instant;
use thiserror::Error;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::process::{Child, ChildStdin, Command};
use uuid::Uuid;
use walkdir::{DirEntry, WalkDir};

const DEFAULT_MAX_OUTPUT_BYTES: usize = 256 * 1024;
const MAX_SEARCH_FILE_BYTES: u64 = 2 * 1024 * 1024;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ToolRisk {
    ReadOnly,
    Write,
    Process,
    Network,
    Destructive,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ToolApprovalRule {
    Never,
    Policy,
    Always,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ToolSandboxRequirement {
    WorkspacePaths,
    RestrictedProcess,
    NetworkRestricted,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ToolCancellation {
    Immediate,
    KillProcessTree,
    NotApplicable,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ToolIdempotency {
    Safe,
    Conditional,
    Unsafe,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
// Tool effects are independent policy facts used before execution.
#[allow(clippy::struct_excessive_bools)]
pub struct ToolDescriptor {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
    pub output_schema: Value,
    pub risk: ToolRisk,
    pub approval_rule: ToolApprovalRule,
    pub sandbox_requirement: ToolSandboxRequirement,
    pub timeout_ms: u64,
    pub cancellation: ToolCancellation,
    pub idempotency: ToolIdempotency,
    pub ui_renderer: String,
    pub supports_parallel: bool,
    pub destructive: bool,
    pub writes_files: bool,
    pub uses_network: bool,
    pub spawns_process: bool,
    pub required_capability: Option<String>,
}

impl From<ToolDescriptor> for CanonicalTool {
    fn from(value: ToolDescriptor) -> Self {
        Self {
            name: value.name,
            description: value.description,
            input_schema: value.input_schema,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FileMutation {
    pub workspace_path: String,
    pub relative_path: String,
    pub preimage_hash: Option<String>,
    pub postimage_hash: Option<String>,
    pub patch: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ToolExecutionResult {
    pub output: Value,
    pub duration_ms: u64,
    pub file_mutations: Vec<FileMutation>,
}

#[derive(Debug, Error)]
pub enum ToolExecutionError {
    #[error("tool `{0}` is not registered")]
    UnknownTool(String),
    #[error("invalid input for tool `{tool}`: {message}")]
    InvalidInput { tool: String, message: String },
    #[error("tool `{tool}` was denied: {reasons}")]
    Denied { tool: String, reasons: String },
    #[error("tool `{tool}` requires approval: {reasons}")]
    ApprovalRequired { tool: String, reasons: String },
    #[error("unsafe workspace path `{0}`")]
    UnsafePath(String),
    #[error("tool I/O failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("search expression is invalid: {0}")]
    Regex(#[from] regex::Error),
    #[error("patch could not be parsed or applied: {0}")]
    Patch(String),
    #[error("process timed out after {0} ms")]
    ProcessTimeout(u64),
    #[error("managed process error: {0}")]
    ManagedProcess(String),
    #[error("network request failed: {0}")]
    Network(#[from] reqwest::Error),
    #[error("product service failed: {0}")]
    Service(String),
}

#[derive(Debug, Clone, Default)]
pub struct ToolRegistry {
    tools: BTreeMap<String, ToolDescriptor>,
}

impl ToolRegistry {
    #[must_use]
    pub fn with_builtins() -> Self {
        let mut registry = Self::default();
        for descriptor in builtin_descriptors() {
            registry.register(descriptor);
        }
        registry
    }

    pub fn register(&mut self, descriptor: ToolDescriptor) -> Option<ToolDescriptor> {
        self.tools.insert(descriptor.name.clone(), descriptor)
    }

    #[must_use]
    pub fn get(&self, name: &str) -> Option<&ToolDescriptor> {
        self.tools.get(name)
    }

    #[must_use]
    pub fn visible_for(&self, policy: &ToolPolicy) -> Vec<ToolDescriptor> {
        self.tools
            .values()
            .filter(|tool| {
                !policy
                    .deny
                    .iter()
                    .any(|pattern| matches_pattern(pattern, &tool.name))
                    && (policy.allow.is_empty()
                        || policy
                            .allow
                            .iter()
                            .any(|pattern| matches_pattern(pattern, &tool.name)))
            })
            .cloned()
            .collect()
    }

    #[must_use]
    pub fn metadata(&self) -> Vec<ToolDescriptor> {
        self.tools.values().cloned().collect()
    }
}

#[derive(Debug, Clone)]
pub struct ToolExecutor {
    registry: ToolRegistry,
    processes: Arc<Mutex<BTreeMap<Uuid, ManagedProcess>>>,
}

#[derive(Debug)]
struct ManagedProcess {
    child: Child,
    stdin: Option<ChildStdin>,
    program: String,
    started: Instant,
    output: Arc<Mutex<ProcessOutput>>,
}

#[derive(Debug, Default)]
struct ProcessOutput {
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    stdout_truncated: bool,
    stderr_truncated: bool,
}

impl Default for ToolExecutor {
    fn default() -> Self {
        Self {
            registry: ToolRegistry::with_builtins(),
            processes: Arc::new(Mutex::new(BTreeMap::new())),
        }
    }
}

impl ToolExecutor {
    #[must_use]
    pub fn new(registry: ToolRegistry) -> Self {
        Self {
            registry,
            processes: Arc::new(Mutex::new(BTreeMap::new())),
        }
    }

    #[must_use]
    pub fn registry(&self) -> &ToolRegistry {
        &self.registry
    }

    pub async fn execute(
        &self,
        agent: &Agent,
        name: &str,
        arguments: Value,
    ) -> Result<ToolExecutionResult, ToolExecutionError> {
        self.execute_with_grant(agent, name, arguments, false).await
    }

    pub fn evaluate(
        &self,
        agent: &Agent,
        name: &str,
        arguments: &Value,
    ) -> Result<PolicyEvaluation, ToolExecutionError> {
        let descriptor = self
            .registry
            .get(name)
            .ok_or_else(|| ToolExecutionError::UnknownTool(name.to_string()))?;
        Ok(PolicyEngine::evaluate(
            agent,
            &tool_request(descriptor, name, arguments),
        ))
    }

    pub async fn execute_approved(
        &self,
        agent: &Agent,
        name: &str,
        arguments: Value,
    ) -> Result<ToolExecutionResult, ToolExecutionError> {
        self.execute_with_grant(agent, name, arguments, true).await
    }

    async fn execute_with_grant(
        &self,
        agent: &Agent,
        name: &str,
        arguments: Value,
        approval_granted: bool,
    ) -> Result<ToolExecutionResult, ToolExecutionError> {
        let evaluation = self.evaluate(agent, name, &arguments)?;
        match evaluation.decision {
            PolicyDecision::Deny => {
                return Err(ToolExecutionError::Denied {
                    tool: name.to_string(),
                    reasons: evaluation.reasons.join("; "),
                });
            }
            PolicyDecision::Ask if !approval_granted => {
                return Err(ToolExecutionError::ApprovalRequired {
                    tool: name.to_string(),
                    reasons: evaluation.reasons.join("; "),
                });
            }
            PolicyDecision::Ask | PolicyDecision::Allow => {}
        }

        let mut approved_agent = agent.clone();
        if approval_granted {
            grant_argument_paths(&mut approved_agent, name, &arguments)?;
        }
        let agent = if approval_granted {
            &approved_agent
        } else {
            agent
        };
        let started = Instant::now();
        let (output, file_mutations) = match name {
            "fs.read" => (read_file(agent, arguments)?, Vec::new()),
            "fs.read_many" => (read_many(agent, arguments)?, Vec::new()),
            "fs.list" => (list_files(agent, arguments)?, Vec::new()),
            "fs.stat" => (stat_path(agent, arguments)?, Vec::new()),
            "fs.glob" => (glob_files(agent, arguments)?, Vec::new()),
            "search.text" => (search_text(agent, arguments)?, Vec::new()),
            "search.symbol" => (search_symbols(agent, arguments)?, Vec::new()),
            "search.fetch" => (fetch_url(arguments).await?, Vec::new()),
            "fs.view_image" => (view_image(agent, arguments)?, Vec::new()),
            "fs.mkdir" => (make_directory(agent, arguments)?, Vec::new()),
            "fs.remove_dir" => (remove_directory(agent, arguments)?, Vec::new()),
            "fs.write" | "docs.write" => write_file(agent, name, arguments)?,
            "fs.edit_exact" => edit_exact(agent, arguments)?,
            "fs.copy" => copy_file(agent, arguments)?,
            "fs.delete" => delete_file(agent, arguments)?,
            "fs.move" => move_file(agent, arguments)?,
            "patch.apply" => apply_patch(agent, arguments)?,
            "shell.run" | "shell.test" => (run_process(agent, name, arguments).await?, Vec::new()),
            "process.start" => (self.start_process(agent, arguments)?, Vec::new()),
            "process.input" => (self.send_process_input(arguments).await?, Vec::new()),
            "process.poll" => (self.poll_process(arguments)?, Vec::new()),
            "process.kill" => (self.kill_process(arguments)?, Vec::new()),
            "git.diff" => (git_diff(agent, arguments).await?, Vec::new()),
            "git.status" | "git.log" | "git.show" | "git.branch" | "git.worktree" => {
                (git_inspect(agent, name, arguments).await?, Vec::new())
            }
            "git.stage" | "git.unstage" | "git.commit" => {
                (git_mutate(agent, name, arguments).await?, Vec::new())
            }
            "git.restore" => git_restore(agent, arguments).await?,
            _ => return Err(ToolExecutionError::UnknownTool(name.to_string())),
        };
        Ok(ToolExecutionResult {
            output,
            duration_ms: elapsed_ms(started),
            file_mutations,
        })
    }

    fn start_process(&self, agent: &Agent, value: Value) -> Result<Value, ToolExecutionError> {
        let args: ProcessArgs = parse_args("process.start", value)?;
        let cwd = resolve_existing(agent, &args.cwd)?;
        let mut command = restricted_command(
            &args.program,
            &args.args,
            &cwd,
            &agent.sandbox_policy.protected_environment,
        );
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let mut child = command.spawn()?;
        let stdin = child.stdin.take();
        let stdout = child.stdout.take().ok_or_else(|| {
            ToolExecutionError::ManagedProcess("stdout pipe was unavailable".to_string())
        })?;
        let stderr = child.stderr.take().ok_or_else(|| {
            ToolExecutionError::ManagedProcess("stderr pipe was unavailable".to_string())
        })?;
        let output = Arc::new(Mutex::new(ProcessOutput::default()));
        tokio::spawn(drain_process_output(stdout, Arc::clone(&output), true));
        tokio::spawn(drain_process_output(stderr, Arc::clone(&output), false));
        let process_id = Uuid::new_v4();
        self.processes
            .lock()
            .map_err(|_| ToolExecutionError::ManagedProcess("process lock poisoned".to_string()))?
            .insert(
                process_id,
                ManagedProcess {
                    child,
                    stdin,
                    program: args.program,
                    started: Instant::now(),
                    output,
                },
            );
        Ok(json!({
            "process_id": process_id,
            "status": "running",
            "cwd": relative_display(agent, &cwd)?
        }))
    }

    async fn send_process_input(&self, value: Value) -> Result<Value, ToolExecutionError> {
        let args: ProcessInputArgs = parse_args("process.input", value)?;
        let id = parse_process_id("process.input", &args.process_id)?;
        let mut process = self
            .processes
            .lock()
            .map_err(|_| ToolExecutionError::ManagedProcess("process lock poisoned".to_string()))?
            .remove(&id)
            .ok_or_else(|| ToolExecutionError::ManagedProcess(format!("unknown process `{id}`")))?;
        let result = async {
            let stdin = process.stdin.as_mut().ok_or_else(|| {
                ToolExecutionError::ManagedProcess(format!("stdin for process `{id}` is closed"))
            })?;
            stdin.write_all(args.input.as_bytes()).await?;
            stdin.flush().await?;
            if args.close_stdin {
                process.stdin.take();
            }
            Ok::<_, ToolExecutionError>(())
        }
        .await;
        self.processes
            .lock()
            .map_err(|_| ToolExecutionError::ManagedProcess("process lock poisoned".to_string()))?
            .insert(id, process);
        result?;
        Ok(json!({
            "process_id": id,
            "bytes_written": args.input.len(),
            "stdin_closed": args.close_stdin
        }))
    }

    fn poll_process(&self, value: Value) -> Result<Value, ToolExecutionError> {
        let args: ProcessIdArgs = parse_args("process.poll", value)?;
        let id = parse_process_id("process.poll", &args.process_id)?;
        let mut processes = self
            .processes
            .lock()
            .map_err(|_| ToolExecutionError::ManagedProcess("process lock poisoned".to_string()))?;
        let process = processes
            .get_mut(&id)
            .ok_or_else(|| ToolExecutionError::ManagedProcess(format!("unknown process `{id}`")))?;
        let status = process.child.try_wait()?;
        let output = process
            .output
            .lock()
            .map_err(|_| ToolExecutionError::ManagedProcess("output lock poisoned".to_string()))?;
        Ok(json!({
            "process_id": id,
            "program": process.program,
            "running": status.is_none(),
            "exit_code": status.and_then(|value| value.code()),
            "elapsed_ms": elapsed_ms(process.started),
            "stdout": String::from_utf8_lossy(&output.stdout),
            "stderr": String::from_utf8_lossy(&output.stderr),
            "stdout_truncated": output.stdout_truncated,
            "stderr_truncated": output.stderr_truncated
        }))
    }

    fn kill_process(&self, value: Value) -> Result<Value, ToolExecutionError> {
        let args: ProcessIdArgs = parse_args("process.kill", value)?;
        let id = parse_process_id("process.kill", &args.process_id)?;
        let mut processes = self
            .processes
            .lock()
            .map_err(|_| ToolExecutionError::ManagedProcess("process lock poisoned".to_string()))?;
        let process = processes
            .get_mut(&id)
            .ok_or_else(|| ToolExecutionError::ManagedProcess(format!("unknown process `{id}`")))?;
        let already_exited = process.child.try_wait()?.is_some();
        if !already_exited {
            process.child.start_kill()?;
        }
        Ok(json!({
            "process_id": id,
            "terminated": !already_exited,
            "already_exited": already_exited
        }))
    }
}

fn tool_request(descriptor: &ToolDescriptor, name: &str, arguments: &Value) -> ToolRequest {
    let target_paths = argument_paths(name, arguments);
    let recursive_directory_removal = name == "fs.remove_dir"
        && arguments
            .get("recursive")
            .and_then(Value::as_bool)
            .unwrap_or(false);
    let command = if name == "git.diff" {
        Some("git".to_string())
    } else if name == "search.fetch" {
        arguments
            .get("url")
            .and_then(Value::as_str)
            .map(str::to_string)
    } else {
        arguments
            .get("program")
            .and_then(Value::as_str)
            .map(str::to_string)
    };
    ToolRequest {
        tool_name: name.to_string(),
        writes_files: descriptor.writes_files,
        uses_network: descriptor.uses_network,
        spawns_process: descriptor.spawns_process,
        target_paths,
        command,
        requires_approval: descriptor.approval_rule == ToolApprovalRule::Always
            || recursive_directory_removal,
    }
}

#[allow(clippy::too_many_lines)]
fn builtin_descriptors() -> Vec<ToolDescriptor> {
    vec![
        descriptor(
            "fs.read",
            "Read one UTF-8 or text-like file from the workspace with a byte limit.",
            json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string"},
                    "max_bytes": {"type": "integer", "minimum": 1, "maximum": 1_048_576}
                },
                "required": ["path"],
                "additionalProperties": false
            }),
            true,
            false,
            false,
        ),
        descriptor(
            "fs.read_many",
            "Read several UTF-8 or text-like workspace files in one deterministic request.",
            json!({
                "type": "object",
                "properties": {
                    "paths": {
                        "type": "array",
                        "items": {"type": "string"},
                        "minItems": 1,
                        "maxItems": 100
                    },
                    "max_bytes_each": {
                        "type": "integer",
                        "minimum": 1,
                        "maximum": 1_048_576
                    }
                },
                "required": ["paths"],
                "additionalProperties": false
            }),
            true,
            false,
            false,
        ),
        descriptor(
            "fs.list",
            "List workspace files beneath a directory with bounded depth and results.",
            json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string"},
                    "depth": {"type": "integer", "minimum": 0, "maximum": 8},
                    "max_results": {"type": "integer", "minimum": 1, "maximum": 2000}
                },
                "additionalProperties": false
            }),
            true,
            false,
            false,
        ),
        descriptor(
            "fs.stat",
            "Inspect one workspace path and return typed filesystem metadata.",
            json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string"}
                },
                "required": ["path"],
                "additionalProperties": false
            }),
            true,
            false,
            false,
        ),
        {
            let mut value = descriptor(
                "fs.mkdir",
                "Create one workspace directory, optionally including missing parent directories.",
                json!({
                    "type": "object",
                    "properties": {
                        "path": {"type": "string"},
                        "parents": {"type": "boolean", "default": true}
                    },
                    "required": ["path"],
                    "additionalProperties": false
                }),
                false,
                true,
                false,
            );
            value.output_schema = json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string"},
                    "kind": {"const": "directory"},
                    "created": {"type": "boolean"},
                    "parents": {"type": "boolean"}
                },
                "required": ["path", "kind", "created", "parents"]
            });
            value.risk = ToolRisk::Write;
            value.idempotency = ToolIdempotency::Safe;
            value.ui_renderer = "filesystem".to_string();
            value.destructive = false;
            value
        },
        {
            let mut value = descriptor(
                "fs.remove_dir",
                "Remove an empty workspace directory, or recursively remove a directory only after explicit approval. Workspace and filesystem roots are protected.",
                json!({
                    "type": "object",
                    "properties": {
                        "path": {"type": "string"},
                        "recursive": {"type": "boolean", "default": false}
                    },
                    "required": ["path"],
                    "additionalProperties": false
                }),
                false,
                true,
                false,
            );
            value.output_schema = json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string"},
                    "deleted": {"const": true},
                    "recursive": {"type": "boolean"},
                    "files_removed": {"type": "integer"},
                    "directories_removed": {"type": "integer"},
                    "bytes_removed": {"type": "integer"}
                },
                "required": [
                    "path",
                    "deleted",
                    "recursive",
                    "files_removed",
                    "directories_removed",
                    "bytes_removed"
                ]
            });
            value.risk = ToolRisk::Destructive;
            value.idempotency = ToolIdempotency::Conditional;
            value.ui_renderer = "filesystem".to_string();
            value.destructive = true;
            value
        },
        descriptor(
            "fs.glob",
            "Find workspace files by a portable *, **, and ? path pattern.",
            json!({
                "type": "object",
                "properties": {
                    "pattern": {"type": "string"},
                    "path": {"type": "string"},
                    "max_results": {"type": "integer", "minimum": 1, "maximum": 2000}
                },
                "required": ["pattern"],
                "additionalProperties": false
            }),
            true,
            false,
            false,
        ),
        descriptor(
            "search.text",
            "Search text files in the workspace using a literal or Rust regular expression.",
            json!({
                "type": "object",
                "properties": {
                    "query": {"type": "string"},
                    "path": {"type": "string"},
                    "regex": {"type": "boolean"},
                    "max_results": {"type": "integer", "minimum": 1, "maximum": 1000}
                },
                "required": ["query"],
                "additionalProperties": false
            }),
            true,
            false,
            false,
        ),
        descriptor(
            "search.symbol",
            "Find likely declarations of a named code symbol in workspace text files.",
            json!({
                "type": "object",
                "properties": {
                    "query": {"type": "string"},
                    "path": {"type": "string"},
                    "max_results": {"type": "integer", "minimum": 1, "maximum": 500}
                },
                "required": ["query"],
                "additionalProperties": false
            }),
            true,
            false,
            false,
        ),
        network_descriptor(
            "search.fetch",
            "Fetch a bounded HTTP or HTTPS text resource after network-policy approval.",
            json!({
                "type": "object",
                "properties": {
                    "url": {"type": "string"},
                    "max_bytes": {"type": "integer", "minimum": 1, "maximum": 1_048_576}
                },
                "required": ["url"],
                "additionalProperties": false
            }),
        ),
        descriptor(
            "fs.view_image",
            "Read a bounded workspace image as a typed data URL with hash and media metadata.",
            json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string"},
                    "max_bytes": {"type": "integer", "minimum": 1, "maximum": 4_194_304}
                },
                "required": ["path"],
                "additionalProperties": false
            }),
            true,
            false,
            false,
        ),
        descriptor(
            "fs.edit_exact",
            "Replace an exact text occurrence with optional match-count and preimage verification.",
            json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string"},
                    "old": {"type": "string"},
                    "new": {"type": "string"},
                    "expected_replacements": {"type": "integer", "minimum": 1},
                    "expected_sha256": {"type": "string"}
                },
                "required": ["path", "old", "new"],
                "additionalProperties": false
            }),
            false,
            true,
            false,
        ),
        descriptor(
            "fs.copy",
            "Copy one workspace file to a new workspace path with destination hash protection.",
            json!({
                "type": "object",
                "properties": {
                    "source": {"type": "string"},
                    "destination": {"type": "string"},
                    "expected_destination_sha256": {"type": "string"}
                },
                "required": ["source", "destination"],
                "additionalProperties": false
            }),
            false,
            true,
            false,
        ),
        descriptor(
            "fs.delete",
            "Delete one workspace file with optional preimage hash verification.",
            json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string"},
                    "expected_sha256": {"type": "string"}
                },
                "required": ["path"],
                "additionalProperties": false
            }),
            false,
            true,
            false,
        ),
        descriptor(
            "fs.move",
            "Move one workspace file to another workspace path with hash protection.",
            json!({
                "type": "object",
                "properties": {
                    "source": {"type": "string"},
                    "destination": {"type": "string"},
                    "expected_source_sha256": {"type": "string"},
                    "expected_destination_sha256": {"type": "string"}
                },
                "required": ["source", "destination"],
                "additionalProperties": false
            }),
            false,
            true,
            false,
        ),
        descriptor(
            "fs.write",
            "Write a complete file after optional preimage hash verification.",
            write_schema(),
            false,
            true,
            false,
        ),
        descriptor(
            "docs.write",
            "Write documentation after optional preimage hash verification.",
            write_schema(),
            false,
            true,
            false,
        ),
        descriptor(
            "patch.apply",
            "Apply one unified patch to a workspace file after optional preimage hash verification.",
            json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string"},
                    "patch": {"type": "string"},
                    "expected_sha256": {"type": "string"}
                },
                "required": ["path", "patch"],
                "additionalProperties": false
            }),
            false,
            true,
            false,
        ),
        process_descriptor(
            "shell.run",
            "Run a process directly without an intermediary shell.",
        ),
        process_descriptor("shell.test", "Run an allowlisted validation process."),
        process_descriptor(
            "process.start",
            "Start a managed long-running process and return a process identifier.",
        ),
        process_control_descriptor(
            "process.input",
            "Send UTF-8 input to a managed process.",
            json!({
                "type": "object",
                "properties": {
                    "process_id": {"type": "string", "format": "uuid"},
                    "input": {"type": "string"},
                    "close_stdin": {"type": "boolean"}
                },
                "required": ["process_id", "input"],
                "additionalProperties": false
            }),
        ),
        process_control_descriptor(
            "process.poll",
            "Read bounded accumulated output and current status from a managed process.",
            json!({
                "type": "object",
                "properties": {"process_id": {"type": "string", "format": "uuid"}},
                "required": ["process_id"],
                "additionalProperties": false
            }),
        ),
        process_control_descriptor(
            "process.kill",
            "Terminate a managed process.",
            json!({
                "type": "object",
                "properties": {"process_id": {"type": "string", "format": "uuid"}},
                "required": ["process_id"],
                "additionalProperties": false
            }),
        ),
        git_descriptor("git.diff", "Read the current Git diff for the workspace."),
        git_descriptor("git.status", "Read machine-parseable Git workspace status."),
        git_descriptor("git.log", "Read a bounded recent Git commit log."),
        git_descriptor(
            "git.show",
            "Show a commit or object without invoking a shell.",
        ),
        git_descriptor("git.branch", "List local and remote Git branches."),
        git_descriptor("git.worktree", "List Git worktrees in porcelain format."),
        git_write_descriptor(
            "git.stage",
            "Stage selected workspace paths in the Git index.",
            json!({
                "type": "object",
                "properties": {
                    "paths": {"type": "array", "items": {"type": "string"}, "minItems": 1}
                },
                "required": ["paths"],
                "additionalProperties": false
            }),
        ),
        git_write_descriptor(
            "git.unstage",
            "Remove selected paths from the Git index without changing working files.",
            json!({
                "type": "object",
                "properties": {
                    "paths": {"type": "array", "items": {"type": "string"}, "minItems": 1}
                },
                "required": ["paths"],
                "additionalProperties": false
            }),
        ),
        git_write_descriptor(
            "git.restore",
            "Restore one working file from the Git index and record a reversible text change.",
            json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string"},
                    "expected_sha256": {"type": "string"}
                },
                "required": ["path"],
                "additionalProperties": false
            }),
        ),
        git_write_descriptor(
            "git.commit",
            "Create a Git commit from the staged index with an explicit message.",
            json!({
                "type": "object",
                "properties": {"message": {"type": "string"}},
                "required": ["message"],
                "additionalProperties": false
            }),
        ),
        product_descriptor(
            "agents.spawn",
            "Start a real child-agent model loop for a bounded task.",
            json!({
                "type": "object",
                "properties": {
                    "task": {"type": "string"},
                    "role": {"type": "string"},
                    "owned_paths": {"type": "array", "items": {"type": "string"}}
                },
                "required": ["task"],
                "additionalProperties": false
            }),
            true,
        ),
        product_descriptor(
            "agents.message",
            "Send a durable message to an agent.",
            json!({
                "type": "object",
                "properties": {
                    "agent_id": {"type": "string", "format": "uuid"},
                    "message": {"type": "string"}
                },
                "required": ["agent_id", "message"],
                "additionalProperties": false
            }),
            false,
        ),
        product_descriptor(
            "agents.status",
            "Read current child-agent states and structured completions.",
            json!({
                "type": "object",
                "properties": {
                    "agent_ids": {"type": "array", "items": {"type": "string", "format": "uuid"}}
                },
                "additionalProperties": false
            }),
            false,
        ),
        product_descriptor(
            "agents.wait",
            "Wait without model polling for child agents to finish or a timeout to expire.",
            json!({
                "type": "object",
                "properties": {
                    "agent_ids": {"type": "array", "items": {"type": "string", "format": "uuid"}},
                    "timeout_ms": {"type": "integer", "minimum": 1, "maximum": 60_000}
                },
                "required": ["agent_ids"],
                "additionalProperties": false
            }),
            false,
        ),
        product_descriptor(
            "agents.interrupt",
            "Interrupt an agent and its active descendants.",
            json!({
                "type": "object",
                "properties": {"agent_id": {"type": "string", "format": "uuid"}},
                "required": ["agent_id"],
                "additionalProperties": false
            }),
            true,
        ),
        product_descriptor(
            "plan.update",
            "Publish the current structured plan or todo state to the event stream.",
            json!({
                "type": "object",
                "properties": {"items": {"type": "array", "items": {"type": "object"}}},
                "required": ["items"],
                "additionalProperties": false
            }),
            false,
        ),
        product_descriptor(
            "skill.activate",
            "Load complete instructions and resources for one registered skill, then continue the \
             original task immediately. This setup action never completes the user's request.",
            json!({
                "type": "object",
                "properties": {"name": {"type": "string"}},
                "required": ["name"],
                "additionalProperties": false
            }),
            false,
        ),
        extension_descriptor(
            "skill.install",
            "Install a validated skill from a local path or Git repository into the current \
             project. The skill becomes available immediately.",
            json!({
                "type": "object",
                "properties": {
                    "source": {"type": "string"},
                    "name": {"type": "string"},
                    "subdirectory": {"type": "string"},
                    "force": {"type": "boolean", "default": false}
                },
                "required": ["source"],
                "additionalProperties": false
            }),
        ),
        extension_descriptor(
            "mcp.connect",
            "Connect and persist an MCP tool server from either a Streamable HTTP URL or a local \
             stdio command. Use token_env or env references; never place secret values in arguments.",
            json!({
                "type": "object",
                "properties": {
                    "name": {"type": "string"},
                    "command": {"type": "string"},
                    "args": {"type": "array", "items": {"type": "string"}},
                    "env": {
                        "type": "object",
                        "additionalProperties": {"type": "string"}
                    },
                    "url": {"type": "string"},
                    "token_env": {"type": "string"},
                    "test": {"type": "boolean", "default": true}
                },
                "required": ["name"],
                "additionalProperties": false
            }),
        ),
        product_descriptor(
            "mcp.list_tools",
            "List the live tool catalog and input schemas exposed by one configured MCP server.",
            json!({
                "type": "object",
                "properties": {
                    "server": {"type": "string"}
                },
                "required": ["server"],
                "additionalProperties": false
            }),
            false,
        ),
        product_descriptor(
            "mcp.invoke",
            "Invoke a discovered tool on a configured MCP server.",
            json!({
                "type": "object",
                "properties": {
                    "server": {"type": "string"},
                    "tool": {"type": "string"},
                    "arguments": {"type": "object"},
                    "timeout_ms": {"type": "integer", "minimum": 1, "maximum": 120_000}
                },
                "required": ["server", "tool", "arguments"],
                "additionalProperties": false
            }),
            true,
        ),
    ]
}

fn descriptor(
    name: &str,
    description: &str,
    input_schema: Value,
    supports_parallel: bool,
    writes_files: bool,
    spawns_process: bool,
) -> ToolDescriptor {
    let risk = if writes_files {
        ToolRisk::Write
    } else if spawns_process {
        ToolRisk::Process
    } else {
        ToolRisk::ReadOnly
    };
    ToolDescriptor {
        name: name.to_string(),
        description: description.to_string(),
        input_schema,
        output_schema: json!({"type": "object"}),
        risk,
        approval_rule: if writes_files || spawns_process {
            ToolApprovalRule::Policy
        } else {
            ToolApprovalRule::Never
        },
        sandbox_requirement: if spawns_process {
            ToolSandboxRequirement::RestrictedProcess
        } else {
            ToolSandboxRequirement::WorkspacePaths
        },
        timeout_ms: if spawns_process { 600_000 } else { 30_000 },
        cancellation: if spawns_process {
            ToolCancellation::KillProcessTree
        } else {
            ToolCancellation::Immediate
        },
        idempotency: if writes_files {
            ToolIdempotency::Conditional
        } else if spawns_process {
            ToolIdempotency::Unsafe
        } else {
            ToolIdempotency::Safe
        },
        ui_renderer: if writes_files {
            "diff".to_string()
        } else if spawns_process {
            "process".to_string()
        } else {
            "json".to_string()
        },
        supports_parallel,
        destructive: writes_files,
        writes_files,
        uses_network: false,
        spawns_process,
        required_capability: None,
    }
}

fn process_descriptor(name: &str, description: &str) -> ToolDescriptor {
    let schema = json!({
        "type": "object",
        "properties": {
            "program": {"type": "string"},
            "args": {"type": "array", "items": {"type": "string"}},
            "cwd": {"type": "string"},
            "timeout_ms": {"type": "integer", "minimum": 1, "maximum": 600_000}
        },
        "required": ["program"],
        "additionalProperties": false
    });
    let writes_files = matches!(name, "shell.run" | "process.start" | "process.input");
    descriptor(name, description, schema, false, writes_files, true)
}

fn network_descriptor(name: &str, description: &str, schema: Value) -> ToolDescriptor {
    let mut value = descriptor(name, description, schema, false, false, false);
    value.risk = ToolRisk::Network;
    value.approval_rule = ToolApprovalRule::Policy;
    value.sandbox_requirement = ToolSandboxRequirement::NetworkRestricted;
    value.timeout_ms = 30_000;
    value.uses_network = true;
    value.ui_renderer = "network".to_string();
    value
}

fn product_descriptor(
    name: &str,
    description: &str,
    schema: Value,
    always_approve: bool,
) -> ToolDescriptor {
    let mut value = descriptor(name, description, schema, false, false, false);
    value.risk = if always_approve {
        ToolRisk::Destructive
    } else {
        ToolRisk::ReadOnly
    };
    value.approval_rule = if always_approve {
        ToolApprovalRule::Always
    } else {
        ToolApprovalRule::Never
    };
    value.sandbox_requirement = ToolSandboxRequirement::WorkspacePaths;
    value.ui_renderer = "agent".to_string();
    value.idempotency = ToolIdempotency::Conditional;
    value.destructive = always_approve;
    value
}

fn extension_descriptor(name: &str, description: &str, schema: Value) -> ToolDescriptor {
    let mut value = descriptor(name, description, schema, false, true, true);
    value.risk = ToolRisk::Network;
    value.approval_rule = ToolApprovalRule::Always;
    value.sandbox_requirement = ToolSandboxRequirement::RestrictedProcess;
    value.timeout_ms = 120_000;
    value.cancellation = ToolCancellation::KillProcessTree;
    value.idempotency = ToolIdempotency::Conditional;
    value.ui_renderer = "extension".to_string();
    value.uses_network = true;
    value.destructive = false;
    value
}

fn process_control_descriptor(name: &str, description: &str, schema: Value) -> ToolDescriptor {
    let mut value = descriptor(name, description, schema, false, false, false);
    value.risk = ToolRisk::Process;
    value.sandbox_requirement = ToolSandboxRequirement::RestrictedProcess;
    value.timeout_ms = 30_000;
    value.cancellation = ToolCancellation::Immediate;
    value.idempotency = ToolIdempotency::Conditional;
    value.ui_renderer = "process".to_string();
    value
}

fn git_descriptor(name: &str, description: &str) -> ToolDescriptor {
    let schema = json!({
        "type": "object",
        "properties": {
            "path": {"type": "string"},
            "revision": {"type": "string"},
            "max_count": {"type": "integer", "minimum": 1, "maximum": 200}
        },
        "additionalProperties": false
    });
    descriptor(name, description, schema, true, false, false)
}

fn git_write_descriptor(name: &str, description: &str, schema: Value) -> ToolDescriptor {
    let mut value = descriptor(name, description, schema, false, true, true);
    value.risk = ToolRisk::Destructive;
    value.approval_rule = ToolApprovalRule::Always;
    value.sandbox_requirement = ToolSandboxRequirement::RestrictedProcess;
    value.ui_renderer = "git".to_string();
    value
}

fn write_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "path": {"type": "string"},
            "content": {"type": "string"},
            "expected_sha256": {"type": "string"}
        },
        "required": ["path", "content"],
        "additionalProperties": false
    })
}

#[derive(Debug, Deserialize)]
struct ReadArgs {
    path: String,
    max_bytes: Option<usize>,
}

fn read_file(agent: &Agent, value: Value) -> Result<Value, ToolExecutionError> {
    let args: ReadArgs = parse_args("fs.read", value)?;
    let path = resolve_existing(agent, &args.path)?;
    let maximum = args
        .max_bytes
        .unwrap_or(DEFAULT_MAX_OUTPUT_BYTES)
        .clamp(1, 1024 * 1024);
    let bytes = std::fs::read(&path)?;
    let truncated = bytes.len() > maximum;
    let visible = &bytes[..bytes.len().min(maximum)];
    Ok(json!({
        "path": relative_display(agent, &path)?,
        "content": String::from_utf8_lossy(visible),
        "bytes": bytes.len(),
        "truncated": truncated,
        "sha256": sha256(&bytes)
    }))
}

#[derive(Debug, Deserialize)]
struct ReadManyArgs {
    paths: Vec<String>,
    max_bytes_each: Option<usize>,
}

fn read_many(agent: &Agent, value: Value) -> Result<Value, ToolExecutionError> {
    let args: ReadManyArgs = parse_args("fs.read_many", value)?;
    if args.paths.is_empty() || args.paths.len() > 100 {
        return Err(ToolExecutionError::InvalidInput {
            tool: "fs.read_many".to_string(),
            message: "paths must contain between 1 and 100 entries".to_string(),
        });
    }
    let maximum = args
        .max_bytes_each
        .unwrap_or(DEFAULT_MAX_OUTPUT_BYTES)
        .clamp(1, 1024 * 1024);
    let mut files = Vec::with_capacity(args.paths.len());
    for requested in args.paths {
        match resolve_existing(agent, &requested).and_then(|path| {
            let bytes = std::fs::read(&path)?;
            let visible = bytes.len().min(maximum);
            Ok(json!({
                "path": relative_display(agent, &path)?,
                "content": String::from_utf8_lossy(&bytes[..visible]),
                "bytes": bytes.len(),
                "truncated": bytes.len() > maximum,
                "sha256": sha256(&bytes)
            }))
        }) {
            Ok(file) => files.push(file),
            Err(error) => files.push(json!({
                "path": requested,
                "error": error.to_string()
            })),
        }
    }
    Ok(json!({"files": files}))
}

#[derive(Debug, Deserialize)]
struct ListArgs {
    #[serde(default = "dot")]
    path: String,
    #[serde(default = "default_depth")]
    depth: usize,
    #[serde(default = "default_list_limit")]
    max_results: usize,
}

fn list_files(agent: &Agent, value: Value) -> Result<Value, ToolExecutionError> {
    let args: ListArgs = parse_args("fs.list", value)?;
    let root = resolve_existing(agent, &args.path)?;
    let maximum = args.max_results.clamp(1, 2_000);
    let entries = WalkDir::new(&root)
        .max_depth(args.depth.min(8))
        .follow_links(false)
        .into_iter()
        .filter_entry(visible_entry)
        .filter_map(Result::ok)
        .take(maximum + 1)
        .map(|entry| {
            json!({
                "path": relative_display(agent, entry.path())
                    .unwrap_or_else(|_| entry.path().display().to_string()),
                "kind": if entry.file_type().is_dir() { "directory" } else { "file" },
                "bytes": entry.metadata().ok().filter(std::fs::Metadata::is_file)
                    .map(|metadata| metadata.len())
            })
        })
        .collect::<Vec<_>>();
    let truncated = entries.len() > maximum;
    Ok(json!({
        "entries": entries.into_iter().take(maximum).collect::<Vec<_>>(),
        "truncated": truncated
    }))
}

#[derive(Debug, Deserialize)]
struct StatArgs {
    path: String,
}

fn stat_path(agent: &Agent, value: Value) -> Result<Value, ToolExecutionError> {
    let args: StatArgs = parse_args("fs.stat", value)?;
    let path = resolve_existing(agent, &args.path)?;
    let metadata = std::fs::metadata(&path)?;
    let kind = if metadata.is_dir() {
        "directory"
    } else if metadata.is_file() {
        "file"
    } else {
        "other"
    };
    Ok(json!({
        "path": relative_display(agent, &path)?,
        "kind": kind,
        "bytes": metadata.len(),
        "readonly": metadata.permissions().readonly(),
        "created_unix_ms": system_time_ms(metadata.created()),
        "modified_unix_ms": system_time_ms(metadata.modified()),
        "accessed_unix_ms": system_time_ms(metadata.accessed())
    }))
}

#[derive(Debug, Deserialize)]
struct MakeDirectoryArgs {
    path: String,
    #[serde(default = "true_value")]
    parents: bool,
}

fn make_directory(agent: &Agent, value: Value) -> Result<Value, ToolExecutionError> {
    let args: MakeDirectoryArgs = parse_args("fs.mkdir", value)?;
    let path = resolve_for_write(agent, &args.path)?;
    if path.exists() {
        if !path.is_dir() {
            return Err(ToolExecutionError::InvalidInput {
                tool: "fs.mkdir".to_string(),
                message: "path exists and is not a directory".to_string(),
            });
        }
        return Ok(json!({
            "path": relative_display(agent, &path)?,
            "kind": "directory",
            "created": false,
            "parents": args.parents,
            "directories_created": 0
        }));
    }

    let directories_created = missing_directory_count(&path);
    if args.parents {
        std::fs::create_dir_all(&path)?;
    } else {
        std::fs::create_dir(&path)?;
    }
    let created = path.canonicalize()?;
    Ok(json!({
        "path": relative_display(agent, &created)?,
        "kind": "directory",
        "created": true,
        "parents": args.parents,
        "directories_created": directories_created
    }))
}

#[derive(Debug, Deserialize)]
struct RemoveDirectoryArgs {
    path: String,
    #[serde(default)]
    recursive: bool,
}

#[derive(Debug, Default)]
struct DirectoryRemovalEvidence {
    files: u64,
    directories: u64,
    other_entries: u64,
    bytes: u64,
}

fn remove_directory(agent: &Agent, value: Value) -> Result<Value, ToolExecutionError> {
    let args: RemoveDirectoryArgs = parse_args("fs.remove_dir", value)?;
    let path = resolve_for_write(agent, &args.path)?;
    if !path.is_dir() {
        return Err(ToolExecutionError::InvalidInput {
            tool: "fs.remove_dir".to_string(),
            message: "path must be a directory".to_string(),
        });
    }

    let workspace_root = Path::new(&agent.workspace.root).canonicalize()?;
    if path == workspace_root || path.parent().is_none() {
        return Err(ToolExecutionError::InvalidInput {
            tool: "fs.remove_dir".to_string(),
            message: "refusing to remove a workspace or filesystem root".to_string(),
        });
    }

    if !args.recursive && std::fs::read_dir(&path)?.next().transpose()?.is_some() {
        return Err(ToolExecutionError::InvalidInput {
            tool: "fs.remove_dir".to_string(),
            message:
                "directory is not empty; set recursive=true and approve the destructive operation"
                    .to_string(),
        });
    }

    let display_path = relative_display(agent, &path)?;
    let evidence = directory_removal_evidence(&path)?;
    if args.recursive {
        std::fs::remove_dir_all(&path)?;
    } else {
        std::fs::remove_dir(&path)?;
    }
    Ok(json!({
        "path": display_path,
        "deleted": true,
        "recursive": args.recursive,
        "entries_removed": evidence.files
            .saturating_add(evidence.directories)
            .saturating_add(evidence.other_entries),
        "files_removed": evidence.files,
        "directories_removed": evidence.directories,
        "other_entries_removed": evidence.other_entries,
        "bytes_removed": evidence.bytes
    }))
}

fn directory_removal_evidence(path: &Path) -> Result<DirectoryRemovalEvidence, ToolExecutionError> {
    let mut evidence = DirectoryRemovalEvidence::default();
    for entry in WalkDir::new(path).follow_links(false) {
        let entry = entry.map_err(|error| {
            let message = error.to_string();
            error
                .into_io_error()
                .unwrap_or_else(|| std::io::Error::other(message))
        })?;
        let file_type = entry.file_type();
        if file_type.is_dir() {
            evidence.directories = evidence.directories.saturating_add(1);
        } else if file_type.is_file() {
            evidence.files = evidence.files.saturating_add(1);
            evidence.bytes = evidence
                .bytes
                .saturating_add(std::fs::metadata(entry.path())?.len());
        } else {
            evidence.other_entries = evidence.other_entries.saturating_add(1);
        }
    }
    Ok(evidence)
}

fn missing_directory_count(path: &Path) -> u64 {
    let mut current = Some(path);
    let mut missing = 0_u64;
    while let Some(candidate) = current {
        if candidate.exists() {
            break;
        }
        missing = missing.saturating_add(1);
        current = candidate.parent();
    }
    missing
}

fn system_time_ms(value: std::io::Result<std::time::SystemTime>) -> Option<u64> {
    value
        .ok()
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .and_then(|duration| u64::try_from(duration.as_millis()).ok())
}

#[derive(Debug, Deserialize)]
struct GlobArgs {
    pattern: String,
    #[serde(default = "dot")]
    path: String,
    #[serde(default = "default_list_limit")]
    max_results: usize,
}

fn glob_files(agent: &Agent, value: Value) -> Result<Value, ToolExecutionError> {
    let args: GlobArgs = parse_args("fs.glob", value)?;
    let root = resolve_existing(agent, &args.path)?;
    let expression = glob_expression(&args.pattern)?;
    let maximum = args.max_results.clamp(1, 2_000);
    let mut paths = Vec::new();
    for entry in WalkDir::new(&root)
        .follow_links(false)
        .into_iter()
        .filter_entry(visible_entry)
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_file())
    {
        let relative = relative_display(agent, entry.path())?;
        if expression.is_match(&relative) {
            paths.push(relative);
            if paths.len() > maximum {
                break;
            }
        }
    }
    paths.sort();
    let truncated = paths.len() > maximum;
    paths.truncate(maximum);
    Ok(json!({"paths": paths, "truncated": truncated}))
}

#[derive(Debug, Deserialize)]
struct SearchArgs {
    query: String,
    #[serde(default = "dot")]
    path: String,
    #[serde(default)]
    regex: bool,
    #[serde(default = "default_search_limit")]
    max_results: usize,
}

fn search_text(agent: &Agent, value: Value) -> Result<Value, ToolExecutionError> {
    let args: SearchArgs = parse_args("search.text", value)?;
    let root = resolve_existing(agent, &args.path)?;
    let expression = if args.regex {
        Regex::new(&args.query)?
    } else {
        Regex::new(&regex::escape(&args.query))?
    };
    let maximum = args.max_results.clamp(1, 1_000);
    let mut matches = Vec::new();
    for entry in WalkDir::new(&root)
        .follow_links(false)
        .into_iter()
        .filter_entry(visible_entry)
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_file())
    {
        if matches.len() >= maximum {
            break;
        }
        let Ok(metadata) = entry.metadata() else {
            continue;
        };
        if metadata.len() > MAX_SEARCH_FILE_BYTES {
            continue;
        }
        let Ok(content) = std::fs::read_to_string(entry.path()) else {
            continue;
        };
        for (index, line) in content.lines().enumerate() {
            if expression.is_match(line) {
                matches.push(json!({
                    "path": relative_display(agent, entry.path())?,
                    "line": index + 1,
                    "text": truncate_text(line, 500)
                }));
                if matches.len() >= maximum {
                    break;
                }
            }
        }
    }
    Ok(json!({
        "matches": matches,
        "truncated": matches.len() >= maximum
    }))
}

#[derive(Debug, Deserialize)]
struct SymbolSearchArgs {
    query: String,
    #[serde(default = "dot")]
    path: String,
    #[serde(default = "default_search_limit")]
    max_results: usize,
}

fn search_symbols(agent: &Agent, value: Value) -> Result<Value, ToolExecutionError> {
    let args: SymbolSearchArgs = parse_args("search.symbol", value)?;
    if args.query.trim().is_empty() {
        return Err(ToolExecutionError::InvalidInput {
            tool: "search.symbol".to_string(),
            message: "query cannot be empty".to_string(),
        });
    }
    let root = resolve_existing(agent, &args.path)?;
    let escaped = regex::escape(args.query.trim());
    let declaration = Regex::new(&format!(
        r"(?i)(?:\b(?:fn|struct|enum|trait|class|interface|type|const|static|def|function|module|mod)\s+|\b(?:let|var)\s+){escaped}\b|(?:^|\W){escaped}\s*\("
    ))?;
    let maximum = args.max_results.clamp(1, 500);
    let mut matches = Vec::new();
    for entry in WalkDir::new(&root)
        .follow_links(false)
        .into_iter()
        .filter_entry(visible_entry)
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_file())
    {
        if matches.len() >= maximum {
            break;
        }
        let Ok(metadata) = entry.metadata() else {
            continue;
        };
        if metadata.len() > MAX_SEARCH_FILE_BYTES {
            continue;
        }
        let Ok(content) = std::fs::read_to_string(entry.path()) else {
            continue;
        };
        for (index, line) in content.lines().enumerate() {
            if declaration.is_match(line) {
                matches.push(json!({
                    "path": relative_display(agent, entry.path())?,
                    "line": index + 1,
                    "text": truncate_text(line, 500)
                }));
                if matches.len() >= maximum {
                    break;
                }
            }
        }
    }
    Ok(json!({
        "query": args.query,
        "matches": matches,
        "truncated": matches.len() >= maximum
    }))
}

#[derive(Debug, Deserialize)]
struct FetchArgs {
    url: String,
    max_bytes: Option<usize>,
}

async fn fetch_url(value: Value) -> Result<Value, ToolExecutionError> {
    let args: FetchArgs = parse_args("search.fetch", value)?;
    if !args.url.starts_with("https://") && !args.url.starts_with("http://") {
        return Err(ToolExecutionError::InvalidInput {
            tool: "search.fetch".to_string(),
            message: "url must use http or https".to_string(),
        });
    }
    let maximum = args.max_bytes.unwrap_or(512 * 1024).clamp(1, 1024 * 1024);
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::limited(5))
        .timeout(std::time::Duration::from_secs(30))
        .user_agent(concat!("opensource/", env!("CARGO_PKG_VERSION")))
        .build()?;
    let mut response = client.get(&args.url).send().await?.error_for_status()?;
    let final_url = response.url().to_string();
    let content_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .map(str::to_string);
    let mut bytes = Vec::new();
    let mut truncated = false;
    while let Some(chunk) = response.chunk().await? {
        let remaining = maximum.saturating_sub(bytes.len());
        if chunk.len() > remaining {
            bytes.extend_from_slice(&chunk[..remaining]);
            truncated = true;
            break;
        }
        bytes.extend_from_slice(&chunk);
        if bytes.len() == maximum {
            truncated = true;
            break;
        }
    }
    Ok(json!({
        "requested_url": args.url,
        "final_url": final_url,
        "content_type": content_type,
        "content": String::from_utf8_lossy(&bytes),
        "bytes_returned": bytes.len(),
        "truncated": truncated,
        "sha256": sha256(&bytes)
    }))
}

fn view_image(agent: &Agent, value: Value) -> Result<Value, ToolExecutionError> {
    let args: ReadArgs = parse_args("fs.view_image", value)?;
    let path = resolve_existing(agent, &args.path)?;
    if !path.is_file() {
        return Err(ToolExecutionError::InvalidInput {
            tool: "fs.view_image".to_string(),
            message: "path must be a file".to_string(),
        });
    }
    let mime_type = match path
        .extension()
        .and_then(|extension| extension.to_str())
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("png") => "image/png",
        Some("jpg" | "jpeg") => "image/jpeg",
        Some("gif") => "image/gif",
        Some("webp") => "image/webp",
        Some("bmp") => "image/bmp",
        _ => {
            return Err(ToolExecutionError::InvalidInput {
                tool: "fs.view_image".to_string(),
                message: "supported formats are PNG, JPEG, GIF, WebP, and BMP".to_string(),
            });
        }
    };
    let maximum = args
        .max_bytes
        .unwrap_or(4 * 1024 * 1024)
        .clamp(1, 4 * 1024 * 1024);
    let bytes = std::fs::read(&path)?;
    if bytes.len() > maximum {
        return Err(ToolExecutionError::InvalidInput {
            tool: "fs.view_image".to_string(),
            message: format!(
                "image is {} bytes; configured limit is {maximum}",
                bytes.len()
            ),
        });
    }
    Ok(json!({
        "path": relative_display(agent, &path)?,
        "mime_type": mime_type,
        "bytes": bytes.len(),
        "sha256": sha256(&bytes),
        "data_url": format!(
            "data:{mime_type};base64,{}",
            base64::engine::general_purpose::STANDARD.encode(&bytes)
        )
    }))
}

#[derive(Debug, Deserialize)]
struct ExactEditArgs {
    path: String,
    old: String,
    new: String,
    #[serde(default = "one")]
    expected_replacements: usize,
    expected_sha256: Option<String>,
}

fn edit_exact(
    agent: &Agent,
    value: Value,
) -> Result<(Value, Vec<FileMutation>), ToolExecutionError> {
    let args: ExactEditArgs = parse_args("fs.edit_exact", value)?;
    if args.old.is_empty() {
        return Err(ToolExecutionError::InvalidInput {
            tool: "fs.edit_exact".to_string(),
            message: "old text must not be empty".to_string(),
        });
    }
    let path = resolve_existing(agent, &args.path)?;
    let original = std::fs::read_to_string(&path)?;
    verify_hash(
        "fs.edit_exact",
        args.expected_sha256.as_deref(),
        Some(original.as_bytes()),
    )?;
    let actual = original.matches(&args.old).count();
    if actual != args.expected_replacements {
        return Err(ToolExecutionError::InvalidInput {
            tool: "fs.edit_exact".to_string(),
            message: format!(
                "expected {} exact replacement(s), found {actual}",
                args.expected_replacements
            ),
        });
    }
    let changed = original.replace(&args.old, &args.new);
    write_file(
        agent,
        "fs.edit_exact",
        json!({
            "path": args.path,
            "content": changed,
            "expected_sha256": sha256(original.as_bytes())
        }),
    )
}

#[derive(Debug, Deserialize)]
struct CopyArgs {
    source: String,
    destination: String,
    expected_destination_sha256: Option<String>,
}

fn copy_file(
    agent: &Agent,
    value: Value,
) -> Result<(Value, Vec<FileMutation>), ToolExecutionError> {
    let args: CopyArgs = parse_args("fs.copy", value)?;
    let source = resolve_existing(agent, &args.source)?;
    if !source.is_file() {
        return Err(ToolExecutionError::InvalidInput {
            tool: "fs.copy".to_string(),
            message: "source must be a file".to_string(),
        });
    }
    let destination = resolve_for_write(agent, &args.destination)?;
    let before = std::fs::read(&destination).ok();
    verify_hash(
        "fs.copy",
        args.expected_destination_sha256.as_deref(),
        before.as_deref(),
    )?;
    let content = std::fs::read(source)?;
    let patch = std::str::from_utf8(&content).ok().and_then(|changed| {
        before.as_deref().map_or_else(
            || Some(diffy::create_patch("", changed).to_string()),
            |original| {
                std::str::from_utf8(original)
                    .ok()
                    .map(|original| diffy::create_patch(original, changed).to_string())
            },
        )
    });
    if let Some(parent) = destination.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&destination, &content)?;
    let relative_path = relative_display(agent, &destination)?;
    let postimage_hash = sha256(&content);
    let mutation = FileMutation {
        workspace_path: mutation_workspace(agent, &destination)?,
        relative_path: relative_path.clone(),
        preimage_hash: before.as_deref().map(sha256),
        postimage_hash: Some(postimage_hash.clone()),
        patch,
    };
    Ok((
        json!({
            "path": relative_path,
            "bytes": content.len(),
            "sha256": postimage_hash
        }),
        vec![mutation],
    ))
}

#[derive(Debug, Deserialize)]
struct DeleteArgs {
    path: String,
    expected_sha256: Option<String>,
}

#[allow(clippy::similar_names)]
fn delete_file(
    agent: &Agent,
    value: Value,
) -> Result<(Value, Vec<FileMutation>), ToolExecutionError> {
    let args: DeleteArgs = parse_args("fs.delete", value)?;
    let path = resolve_existing(agent, &args.path)?;
    if !path.is_file() {
        return Err(ToolExecutionError::InvalidInput {
            tool: "fs.delete".to_string(),
            message: "path must be a file".to_string(),
        });
    }
    let content = std::fs::read(&path)?;
    verify_hash("fs.delete", args.expected_sha256.as_deref(), Some(&content))?;
    let preimage_hash = sha256(&content);
    let patch = std::str::from_utf8(&content)
        .ok()
        .map(|original| diffy::create_patch(original, "").to_string());
    std::fs::remove_file(&path)?;
    let relative_path = relative_display(agent, &path)?;
    Ok((
        json!({"path": relative_path, "deleted": true}),
        vec![FileMutation {
            workspace_path: mutation_workspace(agent, &path)?,
            relative_path,
            preimage_hash: Some(preimage_hash),
            postimage_hash: None,
            patch,
        }],
    ))
}

#[derive(Debug, Deserialize)]
struct MoveArgs {
    source: String,
    destination: String,
    expected_source_sha256: Option<String>,
    expected_destination_sha256: Option<String>,
}

fn move_file(
    agent: &Agent,
    value: Value,
) -> Result<(Value, Vec<FileMutation>), ToolExecutionError> {
    let args: MoveArgs = parse_args("fs.move", value)?;
    let source = resolve_existing(agent, &args.source)?;
    let destination = resolve_for_write(agent, &args.destination)?;
    if source == destination {
        return Err(ToolExecutionError::InvalidInput {
            tool: "fs.move".to_string(),
            message: "source and destination must differ".to_string(),
        });
    }
    if !source.is_file() {
        return Err(ToolExecutionError::InvalidInput {
            tool: "fs.move".to_string(),
            message: "source must be a file".to_string(),
        });
    }
    let content = std::fs::read(&source)?;
    let destination_before = std::fs::read(&destination).ok();
    verify_hash(
        "fs.move",
        args.expected_source_sha256.as_deref(),
        Some(&content),
    )?;
    verify_hash(
        "fs.move",
        args.expected_destination_sha256.as_deref(),
        destination_before.as_deref(),
    )?;
    if let Some(parent) = destination.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&destination, &content)?;
    if let Err(error) = std::fs::remove_file(&source) {
        if let Some(before) = destination_before.as_deref() {
            let _ = std::fs::write(&destination, before);
        } else {
            let _ = std::fs::remove_file(&destination);
        }
        return Err(error.into());
    }
    let source_relative = relative_display(agent, &source)?;
    let destination_relative = relative_display(agent, &destination)?;
    let content_hash = sha256(&content);
    let destination_patch = std::str::from_utf8(&content).ok().and_then(|changed| {
        destination_before.as_deref().map_or_else(
            || Some(diffy::create_patch("", changed).to_string()),
            |original| {
                std::str::from_utf8(original)
                    .ok()
                    .map(|original| diffy::create_patch(original, changed).to_string())
            },
        )
    });
    let source_patch = std::str::from_utf8(&content)
        .ok()
        .map(|original| diffy::create_patch(original, "").to_string());
    Ok((
        json!({
            "source": source_relative,
            "destination": destination_relative,
            "sha256": content_hash
        }),
        vec![
            FileMutation {
                workspace_path: mutation_workspace(agent, &destination)?,
                relative_path: destination_relative,
                preimage_hash: destination_before.as_deref().map(sha256),
                postimage_hash: Some(content_hash.clone()),
                patch: destination_patch,
            },
            FileMutation {
                workspace_path: mutation_workspace(agent, &source)?,
                relative_path: source_relative,
                preimage_hash: Some(content_hash),
                postimage_hash: None,
                patch: source_patch,
            },
        ],
    ))
}

#[derive(Debug, Deserialize)]
struct WriteArgs {
    path: String,
    content: String,
    expected_sha256: Option<String>,
}

#[allow(clippy::similar_names)]
fn write_file(
    agent: &Agent,
    tool: &str,
    value: Value,
) -> Result<(Value, Vec<FileMutation>), ToolExecutionError> {
    let args: WriteArgs = parse_args(tool, value)?;
    let path = resolve_for_write(agent, &args.path)?;
    let before = std::fs::read(&path).ok();
    verify_hash(tool, args.expected_sha256.as_deref(), before.as_deref())?;
    let patch = match before.as_deref() {
        None => Some(diffy::create_patch("", &args.content).to_string()),
        Some(bytes) => std::str::from_utf8(bytes)
            .ok()
            .map(|original| diffy::create_patch(original, &args.content).to_string()),
    };
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&path, args.content.as_bytes())?;
    let postimage_hash = sha256(args.content.as_bytes());
    let relative_path = relative_display(agent, &path)?;
    let mutation = FileMutation {
        workspace_path: mutation_workspace(agent, &path)?,
        relative_path: relative_path.clone(),
        preimage_hash: before.as_deref().map(sha256),
        postimage_hash: Some(postimage_hash.clone()),
        patch,
    };
    Ok((
        json!({"path": relative_path, "sha256": postimage_hash, "bytes": args.content.len()}),
        vec![mutation],
    ))
}

#[derive(Debug, Deserialize)]
struct PatchArgs {
    path: String,
    patch: String,
    expected_sha256: Option<String>,
}

fn apply_patch(
    agent: &Agent,
    value: Value,
) -> Result<(Value, Vec<FileMutation>), ToolExecutionError> {
    let args: PatchArgs = parse_args("patch.apply", value)?;
    let path = resolve_existing(agent, &args.path)?;
    let original = std::fs::read_to_string(&path)?;
    verify_hash(
        "patch.apply",
        args.expected_sha256.as_deref(),
        Some(original.as_bytes()),
    )?;
    let parsed_patch = Patch::from_str(&args.patch)
        .map_err(|error| ToolExecutionError::Patch(error.to_string()))?;
    let changed = diffy::apply(&original, &parsed_patch)
        .map_err(|error| ToolExecutionError::Patch(error.to_string()))?;
    std::fs::write(&path, changed.as_bytes())?;
    let relative_path = relative_display(agent, &path)?;
    let preimage_hash = sha256(original.as_bytes());
    let postimage_hash = sha256(changed.as_bytes());
    let mutation = FileMutation {
        workspace_path: mutation_workspace(agent, &path)?,
        relative_path: relative_path.clone(),
        preimage_hash: Some(preimage_hash.clone()),
        postimage_hash: Some(postimage_hash.clone()),
        patch: Some(args.patch),
    };
    Ok((
        json!({
            "path": relative_path,
            "preimage_sha256": preimage_hash,
            "postimage_sha256": postimage_hash
        }),
        vec![mutation],
    ))
}

#[derive(Debug, Deserialize)]
struct ProcessArgs {
    program: String,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default = "dot")]
    cwd: String,
    #[serde(default = "default_process_timeout")]
    timeout_ms: u64,
}

#[derive(Debug, Deserialize)]
struct ProcessInputArgs {
    process_id: String,
    input: String,
    #[serde(default)]
    close_stdin: bool,
}

#[derive(Debug, Deserialize)]
struct ProcessIdArgs {
    process_id: String,
}

async fn run_process(agent: &Agent, tool: &str, value: Value) -> Result<Value, ToolExecutionError> {
    let args: ProcessArgs = parse_args(tool, value)?;
    let cwd = resolve_existing(agent, &args.cwd)?;
    execute_process(
        &args.program,
        &args.args,
        &cwd,
        args.timeout_ms.clamp(1, 600_000),
        &agent.sandbox_policy.protected_environment,
    )
    .await
}

#[derive(Debug, Deserialize)]
struct GitDiffArgs {
    #[serde(default = "dot")]
    path: String,
}

async fn git_diff(agent: &Agent, value: Value) -> Result<Value, ToolExecutionError> {
    let args: GitDiffArgs = parse_args("git.diff", value)?;
    let path = resolve_existing(agent, &args.path)?;
    execute_process(
        "git",
        &[
            "diff".to_string(),
            "--no-ext-diff".to_string(),
            "--".to_string(),
            path.display().to_string(),
        ],
        Path::new(&agent.workspace.root),
        30_000,
        &agent.sandbox_policy.protected_environment,
    )
    .await
}

#[derive(Debug, Deserialize)]
struct GitInspectArgs {
    #[serde(default = "dot")]
    path: String,
    revision: Option<String>,
    #[serde(default = "default_git_log_count")]
    max_count: usize,
}

async fn git_inspect(agent: &Agent, tool: &str, value: Value) -> Result<Value, ToolExecutionError> {
    let args: GitInspectArgs = parse_args(tool, value)?;
    let cwd = resolve_existing(agent, &args.path)?;
    let command_args = match tool {
        "git.status" => vec![
            "status".to_string(),
            "--short".to_string(),
            "--branch".to_string(),
        ],
        "git.log" => vec![
            "log".to_string(),
            format!("--max-count={}", args.max_count.clamp(1, 200)),
            "--date=iso-strict".to_string(),
            "--format=%H%x09%ad%x09%an%x09%s".to_string(),
        ],
        "git.show" => {
            let revision = args.revision.unwrap_or_else(|| "HEAD".to_string());
            if revision.starts_with('-') {
                return Err(ToolExecutionError::InvalidInput {
                    tool: tool.to_string(),
                    message: "revision must not begin with '-'".to_string(),
                });
            }
            vec![
                "show".to_string(),
                "--no-ext-diff".to_string(),
                "--stat".to_string(),
                "--oneline".to_string(),
                revision,
            ]
        }
        "git.branch" => vec![
            "branch".to_string(),
            "--all".to_string(),
            "--no-color".to_string(),
        ],
        "git.worktree" => vec![
            "worktree".to_string(),
            "list".to_string(),
            "--porcelain".to_string(),
        ],
        _ => return Err(ToolExecutionError::UnknownTool(tool.to_string())),
    };
    execute_process(
        "git",
        &command_args,
        &cwd,
        30_000,
        &agent.sandbox_policy.protected_environment,
    )
    .await
}

#[derive(Debug, Deserialize)]
struct GitPathsArgs {
    paths: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct GitCommitArgs {
    message: String,
}

async fn git_mutate(agent: &Agent, tool: &str, value: Value) -> Result<Value, ToolExecutionError> {
    let cwd = Path::new(&agent.workspace.root);
    let command_args = match tool {
        "git.stage" | "git.unstage" => {
            let args: GitPathsArgs = parse_args(tool, value)?;
            if args.paths.is_empty() || args.paths.len() > 200 {
                return Err(ToolExecutionError::InvalidInput {
                    tool: tool.to_string(),
                    message: "paths must contain between 1 and 200 entries".to_string(),
                });
            }
            let paths = args
                .paths
                .iter()
                .map(|path| {
                    resolve_for_write(agent, path).and_then(|path| relative_display(agent, &path))
                })
                .collect::<Result<Vec<_>, _>>()?;
            let mut command = if tool == "git.stage" {
                vec!["add".to_string()]
            } else {
                vec!["restore".to_string(), "--staged".to_string()]
            };
            command.push("--".to_string());
            command.extend(paths);
            command
        }
        "git.commit" => {
            let args: GitCommitArgs = parse_args(tool, value)?;
            let message = args.message.trim();
            if message.is_empty() || message.len() > 10_000 {
                return Err(ToolExecutionError::InvalidInput {
                    tool: tool.to_string(),
                    message: "message must contain between 1 and 10,000 characters".to_string(),
                });
            }
            vec!["commit".to_string(), "-m".to_string(), message.to_string()]
        }
        _ => return Err(ToolExecutionError::UnknownTool(tool.to_string())),
    };
    execute_process(
        "git",
        &command_args,
        cwd,
        120_000,
        &agent.sandbox_policy.protected_environment,
    )
    .await
}

#[derive(Debug, Deserialize)]
struct GitRestoreArgs {
    path: String,
    expected_sha256: Option<String>,
}

#[allow(clippy::similar_names)]
async fn git_restore(
    agent: &Agent,
    value: Value,
) -> Result<(Value, Vec<FileMutation>), ToolExecutionError> {
    let args: GitRestoreArgs = parse_args("git.restore", value)?;
    let path = resolve_existing(agent, &args.path)?;
    if !path.is_file() {
        return Err(ToolExecutionError::InvalidInput {
            tool: "git.restore".to_string(),
            message: "path must be a file".to_string(),
        });
    }
    let before = std::fs::read(&path)?;
    verify_hash(
        "git.restore",
        args.expected_sha256.as_deref(),
        Some(&before),
    )?;
    let relative = relative_display(agent, &path)?;
    let result = execute_process(
        "git",
        &[
            "restore".to_string(),
            "--worktree".to_string(),
            "--".to_string(),
            relative.clone(),
        ],
        Path::new(&agent.workspace.root),
        30_000,
        &agent.sandbox_policy.protected_environment,
    )
    .await?;
    if !result["success"].as_bool().unwrap_or(false) {
        return Ok((result, Vec::new()));
    }
    let after = std::fs::read(&path)?;
    let patch = std::str::from_utf8(&before).ok().and_then(|before| {
        std::str::from_utf8(&after)
            .ok()
            .map(|after| diffy::create_patch(before, after).to_string())
    });
    let mutation = FileMutation {
        workspace_path: mutation_workspace(agent, &path)?,
        relative_path: relative.clone(),
        preimage_hash: Some(sha256(&before)),
        postimage_hash: Some(sha256(&after)),
        patch,
    };
    Ok((
        json!({
            "path": relative,
            "git": result,
            "preimage_sha256": mutation.preimage_hash.as_deref(),
            "postimage_sha256": mutation.postimage_hash.as_deref()
        }),
        vec![mutation],
    ))
}

async fn execute_process(
    program: &str,
    args: &[String],
    cwd: &Path,
    timeout_ms: u64,
    protected_environment: &[String],
) -> Result<Value, ToolExecutionError> {
    let mut command = restricted_command(program, args, cwd, protected_environment);
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let output = tokio::time::timeout(
        std::time::Duration::from_millis(timeout_ms),
        command.output(),
    )
    .await
    .map_err(|_| ToolExecutionError::ProcessTimeout(timeout_ms))??;
    let stdout = truncate_bytes(&output.stdout, DEFAULT_MAX_OUTPUT_BYTES);
    let stderr = truncate_bytes(&output.stderr, DEFAULT_MAX_OUTPUT_BYTES);
    Ok(json!({
        "exit_code": output.status.code(),
        "success": output.status.success(),
        "stdout": stdout.0,
        "stdout_truncated": stdout.1,
        "stderr": stderr.0,
        "stderr_truncated": stderr.1
    }))
}

fn restricted_command(
    program: &str,
    args: &[String],
    cwd: &Path,
    protected_environment: &[String],
) -> Command {
    let mut command = Command::new(program);
    command.args(args).current_dir(cwd).env_clear();
    for name in safe_environment_names() {
        if !protected_environment
            .iter()
            .any(|protected| protected.eq_ignore_ascii_case(name))
            && let Some(value) = std::env::var_os(name)
        {
            command.env(name, value);
        }
    }
    command
}

async fn drain_process_output<R>(mut reader: R, output: Arc<Mutex<ProcessOutput>>, stdout: bool)
where
    R: AsyncRead + Unpin,
{
    let mut buffer = [0_u8; 8192];
    loop {
        let Ok(read) = reader.read(&mut buffer).await else {
            return;
        };
        if read == 0 {
            return;
        }
        let Ok(mut output) = output.lock() else {
            return;
        };
        if stdout {
            let available = DEFAULT_MAX_OUTPUT_BYTES.saturating_sub(output.stdout.len());
            let accepted = read.min(available);
            output.stdout.extend_from_slice(&buffer[..accepted]);
            if accepted < read {
                output.stdout_truncated = true;
            }
        } else {
            let available = DEFAULT_MAX_OUTPUT_BYTES.saturating_sub(output.stderr.len());
            let accepted = read.min(available);
            output.stderr.extend_from_slice(&buffer[..accepted]);
            if accepted < read {
                output.stderr_truncated = true;
            }
        }
    }
}

fn parse_args<T: for<'de> Deserialize<'de>>(
    tool: &str,
    value: Value,
) -> Result<T, ToolExecutionError> {
    serde_json::from_value(value).map_err(|error| ToolExecutionError::InvalidInput {
        tool: tool.to_string(),
        message: error.to_string(),
    })
}

fn argument_paths(tool: &str, arguments: &Value) -> Vec<String> {
    if matches!(tool, "git.stage" | "git.unstage") {
        return arguments
            .get("paths")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .map(str::to_string)
            .collect();
    }
    if matches!(tool, "fs.copy" | "fs.move") {
        return ["source", "destination"]
            .into_iter()
            .filter_map(|field| arguments.get(field).and_then(Value::as_str))
            .map(str::to_string)
            .collect();
    }
    let field = if matches!(tool, "shell.run" | "shell.test" | "process.start") {
        "cwd"
    } else {
        "path"
    };
    arguments
        .get(field)
        .and_then(Value::as_str)
        .map_or_else(|| vec![".".to_string()], |path| vec![path.to_string()])
}

fn grant_argument_paths(
    agent: &mut Agent,
    tool: &str,
    arguments: &Value,
) -> Result<(), ToolExecutionError> {
    for value in argument_paths(tool, arguments) {
        let path = Path::new(&value);
        if !path.is_absolute() {
            continue;
        }
        let granted = if path.exists() {
            path.canonicalize()?
        } else {
            let parent = path
                .parent()
                .ok_or_else(|| ToolExecutionError::UnsafePath(value.clone()))?;
            nearest_existing_parent(parent)?.canonicalize()?
        }
        .to_string_lossy()
        .into_owned();
        if !agent.sandbox_policy.read_paths.contains(&granted) {
            agent.sandbox_policy.read_paths.push(granted.clone());
        }
        if !agent.sandbox_policy.write_paths.contains(&granted) {
            agent.sandbox_policy.write_paths.push(granted);
        }
    }
    Ok(())
}

fn glob_expression(pattern: &str) -> Result<Regex, ToolExecutionError> {
    if pattern.is_empty() {
        return Err(ToolExecutionError::InvalidInput {
            tool: "fs.glob".to_string(),
            message: "pattern must not be empty".to_string(),
        });
    }
    let normalized = pattern
        .replace('\\', "/")
        .trim_start_matches("./")
        .to_string();
    let mut expression = String::from("^");
    let chars = normalized.chars().collect::<Vec<_>>();
    let mut index = 0;
    while index < chars.len() {
        match chars[index] {
            '*' if chars.get(index + 1) == Some(&'*') => {
                expression.push_str(".*");
                index += 2;
            }
            '*' => {
                expression.push_str("[^/]*");
                index += 1;
            }
            '?' => {
                expression.push_str("[^/]");
                index += 1;
            }
            character => {
                expression.push_str(&regex::escape(&character.to_string()));
                index += 1;
            }
        }
    }
    expression.push('$');
    Regex::new(&expression).map_err(Into::into)
}

fn parse_process_id(tool: &str, value: &str) -> Result<Uuid, ToolExecutionError> {
    Uuid::parse_str(value).map_err(|error| ToolExecutionError::InvalidInput {
        tool: tool.to_string(),
        message: format!("invalid process identifier: {error}"),
    })
}

fn allowed_roots(agent: &Agent, write: bool) -> Result<Vec<PathBuf>, ToolExecutionError> {
    let mut roots = vec![Path::new(&agent.workspace.root).canonicalize()?];
    let configured = if write {
        &agent.sandbox_policy.write_paths
    } else {
        &agent.sandbox_policy.read_paths
    };
    for value in configured {
        if value == "." {
            continue;
        }
        let path = Path::new(value);
        if path.is_absolute() {
            let root = path.canonicalize()?;
            if !roots.contains(&root) {
                roots.push(root);
            }
        }
    }
    Ok(roots)
}

fn resolve_existing(agent: &Agent, value: &str) -> Result<PathBuf, ToolExecutionError> {
    let roots = allowed_roots(agent, false)?;
    let input = Path::new(value);
    let candidate = if input.is_absolute() {
        input.to_path_buf()
    } else {
        join_checked(&roots[0], value)?
    };
    let resolved = candidate.canonicalize()?;
    if !roots.iter().any(|root| resolved.starts_with(root)) {
        return Err(ToolExecutionError::UnsafePath(value.to_string()));
    }
    Ok(resolved)
}

fn resolve_for_write(agent: &Agent, value: &str) -> Result<PathBuf, ToolExecutionError> {
    let roots = allowed_roots(agent, true)?;
    let input = Path::new(value);
    let candidate = if input.is_absolute() {
        input.to_path_buf()
    } else {
        join_checked(&roots[0], value)?
    };
    if candidate.exists() {
        let resolved = candidate.canonicalize()?;
        return roots
            .iter()
            .any(|root| resolved.starts_with(root))
            .then_some(resolved)
            .ok_or_else(|| ToolExecutionError::UnsafePath(value.to_string()));
    }
    let parent = candidate
        .parent()
        .ok_or_else(|| ToolExecutionError::UnsafePath(value.to_string()))?;
    let existing_parent = nearest_existing_parent(parent)?;
    let resolved_parent = existing_parent.canonicalize()?;
    if !roots.iter().any(|root| resolved_parent.starts_with(root)) {
        return Err(ToolExecutionError::UnsafePath(value.to_string()));
    }
    let tail = candidate
        .strip_prefix(&existing_parent)
        .map_err(|_| ToolExecutionError::UnsafePath(value.to_string()))?;
    Ok(resolved_parent.join(tail))
}

fn join_checked(root: &Path, value: &str) -> Result<PathBuf, ToolExecutionError> {
    let input = Path::new(value);
    if input.is_absolute()
        || input
            .components()
            .any(|component| matches!(component, Component::ParentDir | Component::Prefix(_)))
    {
        return Err(ToolExecutionError::UnsafePath(value.to_string()));
    }
    Ok(root.join(input))
}

fn nearest_existing_parent(path: &Path) -> Result<PathBuf, ToolExecutionError> {
    let mut current = path;
    loop {
        if current.exists() {
            return Ok(current.to_path_buf());
        }
        current = current
            .parent()
            .ok_or_else(|| ToolExecutionError::UnsafePath(path.display().to_string()))?;
    }
}

fn containing_root(agent: &Agent, path: &Path, write: bool) -> Result<PathBuf, ToolExecutionError> {
    allowed_roots(agent, write)?
        .into_iter()
        .filter(|root| path.starts_with(root))
        .max_by_key(|root| root.components().count())
        .ok_or_else(|| ToolExecutionError::UnsafePath(path.display().to_string()))
}

fn mutation_workspace(agent: &Agent, path: &Path) -> Result<String, ToolExecutionError> {
    let root = containing_root(agent, path, true)?;
    let workspace = if root.is_file() {
        root.parent()
            .ok_or_else(|| ToolExecutionError::UnsafePath(path.display().to_string()))?
            .to_path_buf()
    } else {
        root
    };
    Ok(workspace.to_string_lossy().into_owned())
}

fn relative_display(agent: &Agent, path: &Path) -> Result<String, ToolExecutionError> {
    let root =
        containing_root(agent, path, false).or_else(|_| containing_root(agent, path, true))?;
    let relative = path
        .strip_prefix(&root)
        .map_err(|_| ToolExecutionError::UnsafePath(path.display().to_string()))?;
    let display = if relative.as_os_str().is_empty() {
        ".".to_string()
    } else {
        relative.to_string_lossy().replace('\\', "/")
    };
    if root == Path::new(&agent.workspace.root).canonicalize()? {
        Ok(display)
    } else {
        Ok(path.to_string_lossy().into_owned())
    }
}

fn verify_hash(
    tool: &str,
    expected: Option<&str>,
    actual: Option<&[u8]>,
) -> Result<(), ToolExecutionError> {
    if let Some(expected) = expected {
        let actual = actual.map(sha256).unwrap_or_default();
        if !expected.eq_ignore_ascii_case(&actual) {
            return Err(ToolExecutionError::InvalidInput {
                tool: tool.to_string(),
                message: format!("preimage hash mismatch: expected {expected}, actual {actual}"),
            });
        }
    }
    Ok(())
}

fn sha256(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn truncate_text(value: &str, maximum: usize) -> String {
    value.chars().take(maximum).collect()
}

fn truncate_bytes(value: &[u8], maximum: usize) -> (String, bool) {
    let truncated = value.len() > maximum;
    (
        String::from_utf8_lossy(&value[..value.len().min(maximum)]).to_string(),
        truncated,
    )
}

fn elapsed_ms(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)
}

fn visible_entry(entry: &DirEntry) -> bool {
    let name = entry.file_name().to_string_lossy();
    !entry.file_type().is_dir()
        || !matches!(name.as_ref(), ".git" | "target" | "node_modules" | ".cache")
}

fn dot() -> String {
    ".".to_string()
}

fn default_depth() -> usize {
    2
}

fn default_list_limit() -> usize {
    500
}

fn default_search_limit() -> usize {
    200
}

fn default_process_timeout() -> u64 {
    120_000
}

fn default_git_log_count() -> usize {
    20
}

fn one() -> usize {
    1
}

fn true_value() -> bool {
    true
}

fn safe_environment_names() -> &'static [&'static str] {
    if cfg!(windows) {
        &["PATH", "PATHEXT", "SystemRoot", "WINDIR", "TEMP", "TMP"]
    } else {
        &["PATH", "LANG", "LC_ALL", "TMPDIR"]
    }
}

fn matches_pattern(pattern: &str, name: &str) -> bool {
    pattern == "*"
        || pattern == name
        || pattern
            .strip_suffix(".*")
            .is_some_and(|prefix| name == prefix || name.starts_with(&format!("{prefix}.")))
}

#[cfg(test)]
mod tests {
    use super::{
        ToolApprovalRule, ToolCancellation, ToolDescriptor, ToolExecutor, ToolIdempotency,
        ToolRegistry, ToolRisk, ToolSandboxRequirement,
    };
    use base64::Engine;
    use chrono::Utc;
    use opensrc_core::{
        Agent, AgentStatus, Budgets, ContextPolicy, ReasoningConfig, RetryPolicy, SandboxPolicy,
        ToolPolicy, Workspace, WorkspaceMode,
    };
    use serde_json::json;
    use uuid::Uuid;

    fn test_agent(root: &str, policy: ToolPolicy) -> Agent {
        Agent {
            id: Uuid::new_v4(),
            run_id: Uuid::new_v4(),
            canonical_path: "/root".to_string(),
            parent_id: None,
            child_ids: Vec::new(),
            role: "test".to_string(),
            task: "test".to_string(),
            status: AgentStatus::Running,
            provider: "mock".to_string(),
            model: "mock".to_string(),
            reasoning: ReasoningConfig::default(),
            system_instructions: String::new(),
            context_policy: ContextPolicy::default(),
            tool_policy: policy,
            workspace: Workspace {
                mode: WorkspaceMode::OwnedPaths,
                root: root.to_string(),
                owned_paths: vec![".".to_string()],
            },
            sandbox_policy: SandboxPolicy::default(),
            budgets: Budgets::default(),
            retry_policy: RetryPolicy::default(),
            fallback_chain: Vec::new(),
            completion_schema: "task_completion".to_string(),
            created_at: Utc::now(),
            updated_at: Utc::now(),
        }
    }

    #[test]
    fn exposes_only_allowed_tools() {
        let mut registry = ToolRegistry::default();
        for name in ["fs.read", "fs.write", "shell.run"] {
            registry.register(ToolDescriptor {
                name: name.to_string(),
                description: name.to_string(),
                input_schema: json!({}),
                output_schema: json!({}),
                risk: ToolRisk::ReadOnly,
                approval_rule: ToolApprovalRule::Never,
                sandbox_requirement: ToolSandboxRequirement::WorkspacePaths,
                timeout_ms: 30_000,
                cancellation: ToolCancellation::Immediate,
                idempotency: ToolIdempotency::Safe,
                ui_renderer: "json".to_string(),
                supports_parallel: true,
                destructive: false,
                writes_files: false,
                uses_network: false,
                spawns_process: false,
                required_capability: None,
            });
        }
        let policy = ToolPolicy {
            allow: vec!["fs.*".to_string()],
            deny: vec!["fs.write".to_string()],
            may_spawn_children: false,
        };
        assert_eq!(registry.visible_for(&policy).len(), 1);
        assert_eq!(registry.visible_for(&policy)[0].name, "fs.read");
    }

    #[test]
    fn extension_installers_are_explicit_approval_tools() {
        let registry = ToolRegistry::with_builtins();
        for name in ["skill.install", "mcp.connect"] {
            let descriptor = registry.get(name).expect("extension descriptor");
            assert_eq!(descriptor.approval_rule, ToolApprovalRule::Always);
            assert!(descriptor.uses_network);
            assert!(descriptor.writes_files);
            assert!(descriptor.spawns_process);
        }
    }

    #[tokio::test]
    async fn reads_inside_workspace_and_rejects_parent_escape() {
        let root = std::env::temp_dir().join(format!("opensrc-tool-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&root).expect("temp workspace");
        std::fs::write(root.join("sample.txt"), "hello").expect("fixture");
        let agent = test_agent(
            &root.to_string_lossy(),
            ToolPolicy {
                allow: vec!["fs.read".to_string()],
                ..ToolPolicy::default()
            },
        );
        let executor = ToolExecutor::default();
        let result = executor
            .execute(&agent, "fs.read", json!({"path": "sample.txt"}))
            .await
            .expect("read");
        assert_eq!(result.output["content"], "hello");
        assert!(
            executor
                .execute(&agent, "fs.read", json!({"path": "../outside.txt"}))
                .await
                .is_err()
        );
        std::fs::remove_dir_all(root).expect("cleanup");
    }

    #[tokio::test]
    async fn creates_stats_and_recursively_removes_directories_with_evidence() {
        let root = std::env::temp_dir().join(format!("opensrc-directories-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&root).expect("temp workspace");
        let agent = test_agent(
            &root.to_string_lossy(),
            ToolPolicy {
                allow: vec!["fs.*".to_string()],
                ..ToolPolicy::default()
            },
        );
        let executor = ToolExecutor::default();
        assert!(matches!(
            executor
                .execute(&agent, "fs.mkdir", json!({"path": "generated/nested"}),)
                .await,
            Err(super::ToolExecutionError::ApprovalRequired { .. })
        ));
        let created = executor
            .execute_approved(&agent, "fs.mkdir", json!({"path": "generated/nested"}))
            .await
            .expect("create nested directories");
        assert_eq!(created.output["created"], true);
        assert_eq!(created.output["directories_created"], 2);

        std::fs::write(root.join("generated/nested/data.bin"), [1_u8, 2, 3, 4])
            .expect("file fixture");
        let file_stat = executor
            .execute_approved(
                &agent,
                "fs.stat",
                json!({"path": "generated/nested/data.bin"}),
            )
            .await
            .expect("file stat");
        assert_eq!(file_stat.output["kind"], "file");
        assert_eq!(file_stat.output["bytes"], 4);

        let request = json!({"path": "generated", "recursive": true});
        let evaluation = executor
            .evaluate(&agent, "fs.remove_dir", &request)
            .expect("recursive removal evaluation");
        assert_eq!(evaluation.decision, opensrc_core::PolicyDecision::Ask);
        assert!(
            evaluation
                .reasons
                .iter()
                .any(|reason| reason.contains("always requires explicit approval"))
        );
        let removed = executor
            .execute_approved(&agent, "fs.remove_dir", request)
            .await
            .expect("recursive directory removal");
        assert_eq!(removed.output["deleted"], true);
        assert_eq!(removed.output["recursive"], true);
        assert_eq!(removed.output["files_removed"], 1);
        assert_eq!(removed.output["directories_removed"], 2);
        assert_eq!(removed.output["bytes_removed"], 4);
        assert!(!root.join("generated").exists());
        std::fs::remove_dir_all(root).expect("cleanup");
    }

    #[tokio::test]
    async fn directory_removal_rejects_nonempty_and_workspace_roots() {
        let root = std::env::temp_dir().join(format!("opensrc-safe-remove-{}", Uuid::new_v4()));
        std::fs::create_dir_all(root.join("kept")).expect("temp workspace");
        std::fs::write(root.join("kept/file.txt"), "keep").expect("fixture");
        let agent = test_agent(
            &root.to_string_lossy(),
            ToolPolicy {
                allow: vec!["fs.*".to_string()],
                ..ToolPolicy::default()
            },
        );
        let executor = ToolExecutor::default();
        let nonempty = executor
            .execute_approved(
                &agent,
                "fs.remove_dir",
                json!({"path": "kept", "recursive": false}),
            )
            .await;
        assert!(matches!(
            nonempty,
            Err(super::ToolExecutionError::InvalidInput { ref message, .. })
                if message.contains("not empty")
        ));
        let workspace_root = executor
            .execute_approved(
                &agent,
                "fs.remove_dir",
                json!({"path": ".", "recursive": true}),
            )
            .await;
        assert!(matches!(
            workspace_root,
            Err(super::ToolExecutionError::InvalidInput { ref message, .. })
                if message.contains("workspace or filesystem root")
        ));
        assert!(root.join("kept/file.txt").is_file());
        std::fs::remove_dir_all(root).expect("cleanup");
    }

    #[tokio::test]
    async fn external_directory_tools_require_and_honor_explicit_approval() {
        let root = std::env::temp_dir().join(format!("opensrc-dir-primary-{}", Uuid::new_v4()));
        let external =
            std::env::temp_dir().join(format!("opensrc-dir-external-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&root).expect("primary workspace");
        std::fs::create_dir_all(&external).expect("external parent");
        let target = external.join("approved");
        let mut agent = test_agent(
            &root.to_string_lossy(),
            ToolPolicy {
                allow: vec!["fs.*".to_string()],
                ..ToolPolicy::default()
            },
        );
        let primary = std::fs::canonicalize(&root)
            .expect("canonical primary")
            .to_string_lossy()
            .into_owned();
        agent.sandbox_policy.read_paths.push(primary.clone());
        agent.sandbox_policy.write_paths.push(primary);
        let executor = ToolExecutor::default();
        assert!(matches!(
            executor
                .execute(
                    &agent,
                    "fs.mkdir",
                    json!({"path": target.to_string_lossy()}),
                )
                .await,
            Err(super::ToolExecutionError::ApprovalRequired { .. })
        ));
        executor
            .execute_approved(
                &agent,
                "fs.mkdir",
                json!({"path": target.to_string_lossy()}),
            )
            .await
            .expect("approved external mkdir");
        let stat = executor
            .execute_approved(&agent, "fs.stat", json!({"path": target.to_string_lossy()}))
            .await
            .expect("approved external stat");
        assert_eq!(stat.output["kind"], "directory");
        executor
            .execute_approved(
                &agent,
                "fs.remove_dir",
                json!({"path": target.to_string_lossy(), "recursive": false}),
            )
            .await
            .expect("approved external removal");
        assert!(!target.exists());
        std::fs::remove_dir_all(root).expect("cleanup primary");
        std::fs::remove_dir_all(external).expect("cleanup external");
    }

    #[tokio::test]
    async fn external_paths_require_approval_and_work_after_it_is_granted() {
        let root = std::env::temp_dir().join(format!("opensrc-primary-{}", Uuid::new_v4()));
        let external = std::env::temp_dir().join(format!("opensrc-external-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&root).expect("primary workspace");
        std::fs::create_dir_all(&external).expect("external workspace");
        let external_file = external.join("media.txt");
        std::fs::write(&external_file, "external").expect("fixture");
        let mut agent = test_agent(
            &root.to_string_lossy(),
            ToolPolicy {
                allow: vec!["fs.*".to_string()],
                ..ToolPolicy::default()
            },
        );
        let primary = std::fs::canonicalize(&root)
            .expect("canonical primary")
            .to_string_lossy()
            .into_owned();
        agent.sandbox_policy.read_paths.push(primary.clone());
        agent.sandbox_policy.write_paths.push(primary);
        let executor = ToolExecutor::default();
        assert!(matches!(
            executor
                .execute(
                    &agent,
                    "fs.read",
                    json!({"path": external_file.to_string_lossy()}),
                )
                .await,
            Err(super::ToolExecutionError::ApprovalRequired { .. })
        ));
        let read = executor
            .execute_approved(
                &agent,
                "fs.read",
                json!({"path": external_file.to_string_lossy()}),
            )
            .await
            .expect("granted read");
        assert_eq!(read.output["content"], "external");
        let created = external.join("created.txt");
        let write = executor
            .execute_approved(
                &agent,
                "fs.write",
                json!({"path": created.to_string_lossy(), "content": "created"}),
            )
            .await
            .expect("granted write");
        assert_eq!(
            std::path::PathBuf::from(&write.file_mutations[0].workspace_path),
            std::fs::canonicalize(&external).expect("canonical external")
        );
        assert_eq!(
            std::fs::read_to_string(created).expect("created"),
            "created"
        );
        std::fs::remove_dir_all(root).expect("cleanup primary");
        std::fs::remove_dir_all(external).expect("cleanup external");
    }

    #[tokio::test]
    async fn edits_an_explicitly_attached_file_without_granting_its_directory() {
        let root = std::env::temp_dir().join(format!("opensrc-primary-{}", Uuid::new_v4()));
        let external = std::env::temp_dir().join(format!("opensrc-attached-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&root).expect("primary workspace");
        std::fs::create_dir_all(&external).expect("attachment directory");
        let attached = external.join("attached.txt");
        let sibling = external.join("sibling.txt");
        std::fs::write(&attached, "before").expect("attached fixture");
        std::fs::write(&sibling, "private").expect("sibling fixture");
        let mut agent = test_agent(
            &root.to_string_lossy(),
            ToolPolicy {
                allow: vec!["fs.*".to_string()],
                ..ToolPolicy::default()
            },
        );
        let attached_root = std::fs::canonicalize(&attached)
            .expect("canonical attachment")
            .to_string_lossy()
            .into_owned();
        agent.sandbox_policy.read_paths.push(attached_root.clone());
        agent.sandbox_policy.write_paths.push(attached_root.clone());
        let executor = ToolExecutor::default();
        let changed = executor
            .execute_approved(
                &agent,
                "fs.edit_exact",
                json!({
                    "path": attached_root,
                    "old": "before",
                    "new": "after"
                }),
            )
            .await
            .expect("edit attachment");
        assert_eq!(
            std::fs::read_to_string(&attached).expect("changed attachment"),
            "after"
        );
        assert_eq!(
            std::path::PathBuf::from(&changed.file_mutations[0].workspace_path),
            std::fs::canonicalize(&external).expect("canonical parent")
        );
        assert!(
            executor
                .execute(
                    &agent,
                    "fs.read",
                    json!({"path": sibling.to_string_lossy()}),
                )
                .await
                .is_err()
        );
        std::fs::remove_dir_all(root).expect("cleanup primary");
        std::fs::remove_dir_all(external).expect("cleanup external");
    }

    #[tokio::test]
    async fn applies_hash_guarded_unified_patch() {
        let root = std::env::temp_dir().join(format!("opensrc-patch-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&root).expect("temp workspace");
        std::fs::write(root.join("sample.txt"), "hello\n").expect("fixture");
        let agent = test_agent(
            &root.to_string_lossy(),
            ToolPolicy {
                allow: vec!["patch.apply".to_string()],
                ..ToolPolicy::default()
            },
        );
        let executor = ToolExecutor::default();
        assert!(matches!(
            executor
                .execute(
                    &agent,
                    "patch.apply",
                    json!({
                        "path": "sample.txt",
                        "expected_sha256":
                            "5891b5b522d5df086d0ff0b110fbd9d21bb4fc7163af34d08286a2e846f6be03",
                        "patch": "--- original\n+++ modified\n@@ -1 +1 @@\n-hello\n+world\n"
                    }),
                )
                .await,
            Err(super::ToolExecutionError::ApprovalRequired { .. })
        ));
        let result = executor
            .execute_approved(
                &agent,
                "patch.apply",
                json!({
                    "path": "sample.txt",
                    "expected_sha256":
                        "5891b5b522d5df086d0ff0b110fbd9d21bb4fc7163af34d08286a2e846f6be03",
                    "patch": "--- original\n+++ modified\n@@ -1 +1 @@\n-hello\n+world\n"
                }),
            )
            .await
            .expect("patch");
        assert!(!result.file_mutations.is_empty());
        assert_eq!(
            std::fs::read_to_string(root.join("sample.txt")).expect("changed file"),
            "world\n"
        );
        std::fs::remove_dir_all(root).expect("cleanup");
    }

    #[tokio::test]
    async fn batches_reads_globs_and_applies_exact_edits() {
        let root = std::env::temp_dir().join(format!("opensrc-batch-{}", Uuid::new_v4()));
        std::fs::create_dir_all(root.join("src")).expect("temp workspace");
        std::fs::write(root.join("src").join("alpha.rs"), "fn alpha() {}\n").expect("fixture");
        std::fs::write(root.join("src").join("beta.rs"), "fn beta() {}\n").expect("fixture");
        let agent = test_agent(
            &root.to_string_lossy(),
            ToolPolicy {
                allow: vec!["fs.*".to_string(), "search.*".to_string()],
                ..ToolPolicy::default()
            },
        );
        let executor = ToolExecutor::default();
        let reads = executor
            .execute_approved(
                &agent,
                "fs.read_many",
                json!({"paths": ["src/alpha.rs", "src/beta.rs"]}),
            )
            .await
            .expect("batch read");
        assert_eq!(reads.output["files"].as_array().map(Vec::len), Some(2));
        let glob = executor
            .execute(&agent, "fs.glob", json!({"pattern": "src/*.rs"}))
            .await
            .expect("glob");
        assert_eq!(glob.output["paths"].as_array().map(Vec::len), Some(2));
        let changed = executor
            .execute_approved(
                &agent,
                "fs.edit_exact",
                json!({
                    "path": "src/alpha.rs",
                    "old": "alpha",
                    "new": "renamed",
                    "expected_replacements": 1
                }),
            )
            .await
            .expect("exact edit");
        assert!(!changed.file_mutations.is_empty());
        assert_eq!(
            std::fs::read_to_string(root.join("src").join("alpha.rs")).expect("changed file"),
            "fn renamed() {}\n"
        );
        let symbols = executor
            .execute(&agent, "search.symbol", json!({"query": "renamed"}))
            .await
            .expect("symbol search");
        assert_eq!(symbols.output["matches"].as_array().map(Vec::len), Some(1));
        std::fs::write(
            root.join("pixel.png"),
            base64::engine::general_purpose::STANDARD
                .decode("iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+A8AAQUBAScY42YAAAAASUVORK5CYII=")
                .expect("png"),
        )
        .expect("image fixture");
        let image = executor
            .execute(&agent, "fs.view_image", json!({"path": "pixel.png"}))
            .await
            .expect("view image");
        assert_eq!(image.output["mime_type"], "image/png");
        assert!(
            image.output["data_url"]
                .as_str()
                .is_some_and(|value| value.starts_with("data:image/png;base64,"))
        );
        std::fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn network_fetch_requires_domain_aware_approval() {
        let root = std::env::temp_dir().join(format!("opensrc-network-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&root).expect("temp workspace");
        let mut agent = test_agent(
            &root.to_string_lossy(),
            ToolPolicy {
                allow: vec!["search.*".to_string()],
                ..ToolPolicy::default()
            },
        );
        let executor = ToolExecutor::default();
        let request = json!({"url": "https://docs.example.com/guide"});
        assert_eq!(
            executor
                .evaluate(&agent, "search.fetch", &request)
                .expect("evaluation")
                .decision,
            opensrc_core::PolicyDecision::Ask
        );
        agent.sandbox_policy.network_allow = vec!["*.example.com".to_string()];
        assert_eq!(
            executor
                .evaluate(&agent, "search.fetch", &request)
                .expect("evaluation")
                .decision,
            opensrc_core::PolicyDecision::Allow
        );
        std::fs::remove_dir_all(root).expect("cleanup");
    }

    #[tokio::test]
    async fn manages_a_long_running_process_with_input_and_polling() {
        let root = std::env::temp_dir().join(format!("opensrc-process-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&root).expect("temp workspace");
        let mut agent = test_agent(
            &root.to_string_lossy(),
            ToolPolicy {
                allow: vec!["process.*".to_string()],
                ..ToolPolicy::default()
            },
        );
        agent.sandbox_policy.command_allow = vec!["*".to_string()];
        let executor = ToolExecutor::default();
        let (program, args) = if cfg!(windows) {
            (
                "cmd.exe",
                vec![
                    "/Q",
                    "/D",
                    "/V:ON",
                    "/C",
                    "set /p value=& echo received:!value!",
                ],
            )
        } else {
            ("sh", vec!["-c", "read value; echo received:$value"])
        };
        let started = executor
            .execute_approved(
                &agent,
                "process.start",
                json!({"program": program, "args": args}),
            )
            .await
            .expect("process start");
        let id = started.output["process_id"]
            .as_str()
            .expect("process identifier");
        executor
            .execute_approved(
                &agent,
                "process.input",
                json!({"process_id": id, "input": "hello\n", "close_stdin": true}),
            )
            .await
            .expect("process input");
        let mut final_poll = None;
        for _ in 0..40 {
            let poll = executor
                .execute(&agent, "process.poll", json!({"process_id": id}))
                .await
                .expect("process poll");
            if poll.output["running"] == false {
                final_poll = Some(poll);
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let output = final_poll.expect("process should finish").output;
        assert!(
            output["stdout"]
                .as_str()
                .is_some_and(|value| value.contains("received:hello")),
            "unexpected process output: {output}"
        );
        std::fs::remove_dir_all(root).expect("cleanup");
    }
}
