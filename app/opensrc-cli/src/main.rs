mod tui;

use anyhow::{Context, Result};
use clap::{CommandFactory, Parser, Subcommand};
use opensrc_core::{Agent, AgentDefinition, Conversation, ExecutionMode, RunExecutionResult};
use opensrc_providers::{build_adapters, read_provider_file};
use opensrc_runtime::{
    AgentLimits, McpRegistry, McpServer, McpTransport, ModeClassifier, ModelPackRegistry,
    ProviderRouter, RoutingPolicyRegistry, Runtime, SkillRegistry, ToolExecutor,
    load_agent_definition,
};
use opensrc_server::ServerState;
use opensrc_store::Store;
use serde::Deserialize;
use serde_json::json;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tracing_subscriber::EnvFilter;

const DEFAULT_BIND: &str = "127.0.0.1:4545";
const DEFAULT_SERVER: &str = "http://127.0.0.1:4545";

#[derive(Debug, Parser)]
#[command(
    name = "divit",
    version,
    about = "Divit's OpenSource terminal coding agent",
    arg_required_else_help = false
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Subcommand)]
enum Command {
    Serve {
        #[arg(long, default_value = "127.0.0.1:4545")]
        bind: SocketAddr,
        #[arg(long, default_value = ".opensource/state.sqlite3")]
        database: PathBuf,
        #[arg(long)]
        provider_config: Option<PathBuf>,
        #[arg(long, default_value = "skills")]
        skills_dir: PathBuf,
    },
    Run {
        #[arg(required = true, num_args = 1..)]
        request: Vec<String>,
        #[arg(long, default_value = ".")]
        project_root: PathBuf,
        #[arg(long, default_value = "http://127.0.0.1:4545")]
        server: String,
        #[arg(long, default_value = "generalist")]
        agent: String,
        #[arg(long, requires = "model")]
        provider: Option<String>,
        #[arg(long, requires = "provider")]
        model: Option<String>,
        #[arg(long)]
        mode: Option<String>,
    },
    Execute {
        run_id: uuid::Uuid,
        #[arg(long)]
        provider: String,
        #[arg(long)]
        model: String,
        #[arg(long, default_value = "http://127.0.0.1:4545")]
        server: String,
    },
    Tui {
        #[arg(long, default_value = "http://127.0.0.1:4545")]
        server: String,
    },
    Attach {
        url: String,
    },
    Auth {
        #[command(subcommand)]
        command: AuthCommand,
    },
    Providers {
        #[command(subcommand)]
        command: ProviderCommand,
    },
    Models {
        provider: Option<String>,
        #[arg(long, default_value = "http://127.0.0.1:4545")]
        server: String,
    },
    Agent {
        #[command(subcommand)]
        command: AgentCommand,
    },
    Session {
        #[command(subcommand)]
        command: SessionCommand,
    },
    Skill {
        #[command(subcommand)]
        command: SkillCommand,
    },
    Mcp {
        #[command(subcommand)]
        command: McpCommand,
    },
    Stats {
        #[arg(long, default_value = "http://127.0.0.1:4545")]
        server: String,
    },
    Doctor,
    Completions {
        shell: clap_complete::Shell,
    },
    Status {
        #[arg(long, default_value = "http://127.0.0.1:4545")]
        server: String,
    },
    ValidateAgents {
        #[arg(default_value = "agents")]
        directory: PathBuf,
    },
    Classify {
        request: String,
    },
    BenchmarkLocal {
        #[arg(long, default_value = "../benchmarks/scenarios.json")]
        scenarios: PathBuf,
        #[arg(long, default_value_t = 1000)]
        iterations: u32,
        #[arg(long)]
        output: Option<PathBuf>,
    },
}

#[derive(Debug, Subcommand)]
enum AuthCommand {
    Login {
        #[arg(long)]
        provider: String,
        #[arg(long, default_value = "openai_compatible")]
        protocol: String,
        #[arg(long)]
        family: Option<String>,
        #[arg(long)]
        base_url: String,
        #[arg(long)]
        model: String,
        #[arg(long)]
        api_key_env: String,
        #[arg(long, default_value = "http://127.0.0.1:4545")]
        server: String,
        #[arg(long, default_value_t = true)]
        test_connection: bool,
    },
    List {
        #[arg(long, default_value = "http://127.0.0.1:4545")]
        server: String,
    },
    Logout {
        provider: String,
        #[arg(long, default_value = "http://127.0.0.1:4545")]
        server: String,
    },
}

#[derive(Debug, Subcommand)]
enum ProviderCommand {
    List {
        #[arg(long, default_value = "http://127.0.0.1:4545")]
        server: String,
    },
}

#[derive(Debug, Subcommand)]
enum AgentCommand {
    Create {
        name: String,
        #[arg(long, default_value = "Custom project coding agent")]
        description: String,
        #[arg(long, default_value = "owned_paths")]
        workspace_mode: String,
        #[arg(
            long,
            value_delimiter = ',',
            default_value = "fs.*,search.*,patch.apply,shell.run"
        )]
        tools: Vec<String>,
        #[arg(long)]
        force: bool,
        #[arg(long, default_value = ".")]
        project: PathBuf,
    },
    List {
        #[arg(long, default_value = "http://127.0.0.1:4545")]
        server: String,
    },
}

#[derive(Debug, Subcommand)]
enum SessionCommand {
    List {
        #[arg(long)]
        project: Option<PathBuf>,
        #[arg(long, default_value = "http://127.0.0.1:4545")]
        server: String,
    },
    Delete {
        id: uuid::Uuid,
        #[arg(long, default_value = "http://127.0.0.1:4545")]
        server: String,
    },
    Export {
        id: uuid::Uuid,
        #[arg(long)]
        output: Option<PathBuf>,
        #[arg(long, default_value = "http://127.0.0.1:4545")]
        server: String,
    },
    Import {
        file: PathBuf,
        #[arg(long)]
        project: Option<PathBuf>,
        #[arg(long, default_value = "http://127.0.0.1:4545")]
        server: String,
    },
    Compact {
        id: uuid::Uuid,
        #[arg(long, default_value = "http://127.0.0.1:4545")]
        server: String,
    },
}

#[derive(Debug, Subcommand)]
enum SkillCommand {
    Create {
        name: String,
        #[arg(long, default_value = "Custom project workflow")]
        description: String,
        #[arg(long, value_delimiter = ',')]
        triggers: Vec<String>,
        #[arg(long)]
        force: bool,
        #[arg(long, default_value = ".")]
        project: PathBuf,
    },
    List {
        #[arg(long, default_value = "http://127.0.0.1:4545")]
        server: String,
    },
    Validate {
        path: PathBuf,
    },
    Enable {
        name: String,
        #[arg(long, default_value = ".")]
        project: PathBuf,
    },
    Disable {
        name: String,
        #[arg(long, default_value = ".")]
        project: PathBuf,
    },
}

#[derive(Debug, Subcommand)]
enum McpCommand {
    Add {
        name: String,
        #[arg(long, conflicts_with = "url")]
        command: Option<String>,
        #[arg(long, num_args = 0.., trailing_var_arg = true)]
        args: Vec<String>,
        #[arg(long, value_parser = parse_key_value)]
        env: Vec<(String, String)>,
        #[arg(long, conflicts_with = "command")]
        url: Option<String>,
        #[arg(long)]
        token_env: Option<String>,
        #[arg(long, default_value = "http://127.0.0.1:4545")]
        server: String,
    },
    List {
        #[arg(long, default_value = "http://127.0.0.1:4545")]
        server: String,
    },
    Remove {
        name: String,
        #[arg(long, default_value = "http://127.0.0.1:4545")]
        server: String,
    },
    Enable {
        name: String,
        #[arg(long, default_value = "http://127.0.0.1:4545")]
        server: String,
    },
    Disable {
        name: String,
        #[arg(long, default_value = "http://127.0.0.1:4545")]
        server: String,
    },
    Debug {
        name: String,
        #[arg(long, default_value = "http://127.0.0.1:4545")]
        server: String,
    },
}

#[tokio::main]
#[allow(clippy::too_many_lines)]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_target(false)
        .init();
    let cli = Cli::parse();
    match cli.command {
        None => launch().await,
        Some(Command::Serve {
            bind,
            database,
            provider_config,
            skills_dir,
        }) => serve(bind, &database, provider_config.as_deref(), &skills_dir).await,
        Some(Command::Run {
            request,
            project_root,
            server,
            agent,
            provider,
            model,
            mode,
        }) => {
            run_request(
                &server,
                &request.join(" "),
                &project_root,
                &agent,
                provider.as_deref(),
                model.as_deref(),
                mode.as_deref(),
            )
            .await
        }
        Some(Command::Execute {
            run_id,
            provider,
            model,
            server,
        }) => execute_run(&server, run_id, &provider, &model).await,
        Some(Command::Tui { server }) => {
            let project_root =
                std::env::current_dir().context("failed to resolve the current directory")?;
            tui::run(&server, &project_root).await
        }
        Some(Command::Attach { url }) => {
            let project_root =
                std::env::current_dir().context("failed to resolve the current directory")?;
            tui::run(&url, &project_root).await
        }
        Some(Command::Auth { command }) => match command {
            AuthCommand::Login {
                provider,
                protocol,
                family,
                base_url,
                model,
                api_key_env,
                server,
                test_connection,
            } => {
                auth_login(
                    &server,
                    &provider,
                    &protocol,
                    family.as_deref(),
                    &base_url,
                    &model,
                    &api_key_env,
                    test_connection,
                )
                .await
            }
            AuthCommand::List { server } => print_api_resource(&server, "/v1/providers").await,
            AuthCommand::Logout { provider, server } => delete_provider(&server, &provider).await,
        },
        Some(Command::Providers { command }) => match command {
            ProviderCommand::List { server } => print_api_resource(&server, "/v1/providers").await,
        },
        Some(Command::Models { provider, server }) => {
            list_models_command(&server, provider.as_deref()).await
        }
        Some(Command::Agent { command }) => match command {
            AgentCommand::Create {
                name,
                description,
                workspace_mode,
                tools,
                force,
                project,
            } => create_agent_definition(
                &project,
                &name,
                &description,
                &workspace_mode,
                &tools,
                force,
            ),
            AgentCommand::List { server } => list_agent_definitions_command(&server).await,
        },
        Some(Command::Session { command }) => session_command(command).await,
        Some(Command::Skill { command }) => skill_command(command).await,
        Some(Command::Mcp { command }) => mcp_command(command).await,
        Some(Command::Stats { server }) => print_api_resource(&server, "/v1/metrics").await,
        Some(Command::Doctor) => doctor().await,
        Some(Command::Completions { shell }) => {
            let mut command = Cli::command();
            clap_complete::generate(shell, &mut command, "divit", &mut std::io::stdout());
            Ok(())
        }
        Some(Command::Status { server }) => status(&server).await,
        Some(Command::ValidateAgents { directory }) => validate_agents(&directory),
        Some(Command::Classify { request }) => {
            println!(
                "{}",
                serde_json::to_string_pretty(&json!({
                    "decision": ModeClassifier::classify(&request)
                }))?
            );
            Ok(())
        }
        Some(Command::BenchmarkLocal {
            scenarios,
            iterations,
            output,
        }) => benchmark_local(&scenarios, iterations, output.as_deref()),
    }
}

async fn launch() -> Result<()> {
    let project_root =
        std::env::current_dir().context("failed to resolve the current directory")?;
    let state_dir = user_state_directory();
    std::fs::create_dir_all(&state_dir)
        .with_context(|| format!("failed to create {}", state_dir.display()))?;

    let skills_dir = project_root.join(".opensource").join("skills");
    std::fs::create_dir_all(&skills_dir)
        .with_context(|| format!("failed to create {}", skills_dir.display()))?;
    let provider_config = provider_config_for(&project_root, &state_dir);
    let (bind, server) = available_launch_endpoint()?;

    let state = build_server_state(
        &state_dir.join("state.sqlite3"),
        Some(&provider_config),
        &skills_dir,
    )?;
    let server_task = tokio::spawn(opensrc_server::serve(state, bind));

    for _ in 0..40 {
        if server_is_healthy(&server).await {
            let tui_result = tui::run(&server, &project_root).await;
            server_task.abort();
            let _ = server_task.await;
            return tui_result;
        }
        if server_task.is_finished() {
            let server_result = server_task
                .await
                .context("local server task failed before startup")?;
            server_result.context("local server stopped before the TUI could connect")?;
            anyhow::bail!("local server stopped before the TUI could connect");
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    server_task.abort();
    let _ = server_task.await;
    anyhow::bail!("timed out while starting the local server on {bind}");
}

fn available_launch_endpoint() -> Result<(SocketAddr, String)> {
    let preferred: SocketAddr = DEFAULT_BIND.parse().expect("valid built-in bind address");
    let listener = std::net::TcpListener::bind(preferred)
        .or_else(|_| std::net::TcpListener::bind(("127.0.0.1", 0)))
        .context("failed to reserve a loopback port for the local runtime")?;
    let bind = listener
        .local_addr()
        .context("failed to inspect the reserved local runtime port")?;
    drop(listener);
    Ok((bind, format!("http://{bind}")))
}

fn provider_config_for(project_root: &Path, state_dir: &Path) -> PathBuf {
    if let Some(path) = std::env::var_os("OPENSOURCE_PROVIDER_CONFIG") {
        return PathBuf::from(path);
    }
    [
        project_root.join(".opensource").join("providers.json"),
        project_root.join("providers.json"),
    ]
    .into_iter()
    .find(|path| path.is_file())
    .unwrap_or_else(|| state_dir.join("providers.json"))
}

fn user_state_directory() -> PathBuf {
    std::env::var_os("LOCALAPPDATA").map_or_else(
        || {
            std::env::var_os("XDG_STATE_HOME").map_or_else(
                || {
                    std::env::var_os("USERPROFILE")
                        .or_else(|| std::env::var_os("HOME"))
                        .map_or_else(|| PathBuf::from("."), PathBuf::from)
                        .join(".opensource")
                },
                |path| PathBuf::from(path).join("opensource"),
            )
        },
        |path| PathBuf::from(path).join("opensource"),
    )
}

async fn server_is_healthy(server: &str) -> bool {
    let Ok(client) = api_client() else {
        return false;
    };
    client
        .get(format!("{server}/v1/health"))
        .timeout(Duration::from_millis(250))
        .send()
        .await
        .is_ok_and(|response| response.status().is_success())
}

fn api_client() -> Result<reqwest::Client> {
    let mut headers = reqwest::header::HeaderMap::new();
    if let Ok(token) = std::env::var("OPENSOURCE_SERVER_TOKEN") {
        let value = reqwest::header::HeaderValue::from_str(&format!("Bearer {token}"))
            .context("OPENSOURCE_SERVER_TOKEN contains invalid header characters")?;
        headers.insert(reqwest::header::AUTHORIZATION, value);
    }
    reqwest::Client::builder()
        .default_headers(headers)
        .build()
        .context("failed to build HTTP client")
}

async fn serve(
    bind: SocketAddr,
    database: &Path,
    provider_config: Option<&Path>,
    skills_dir: &Path,
) -> Result<()> {
    let state = build_server_state(database, provider_config, skills_dir)?;
    opensrc_server::serve(state, bind).await?;
    Ok(())
}

#[allow(clippy::too_many_lines)]
fn build_server_state(
    database: &Path,
    provider_config: Option<&Path>,
    skills_dir: &Path,
) -> Result<ServerState> {
    let store = Store::open(database)
        .with_context(|| format!("failed to open database {}", database.display()))?;
    let providers = ProviderRouter::default();
    if let Some(path) = provider_config.filter(|path| path.is_file()) {
        let document = read_provider_file(path)
            .with_context(|| format!("failed to load providers from {}", path.display()))?;
        for entry in document.providers {
            let result = build_adapters(opensrc_providers::ProviderFile {
                providers: vec![entry.clone()],
            });
            match result {
                Ok(mut adapters) => {
                    let adapter = adapters.remove(0);
                    if let Some(model) = entry.default_model {
                        providers.register_with_models(adapter, model, entry.models);
                    } else {
                        providers.register(adapter);
                    }
                }
                Err(error) => {
                    if log_unavailable_provider(&error) {
                        continue;
                    }
                    return Err(error).with_context(|| {
                        format!("failed to load providers from {}", path.display())
                    });
                }
            }
        }
    }
    let provider_config_path = provider_config.map_or_else(
        || {
            database
                .parent()
                .unwrap_or_else(|| Path::new("."))
                .join("providers.json")
        },
        Path::to_path_buf,
    );
    let mut skill_roots = vec![skills_dir.to_path_buf()];
    if let Some(opensource_directory) = skills_dir.parent()
        && opensource_directory
            .file_name()
            .is_some_and(|name| name == ".opensource")
        && let Some(project_root) = opensource_directory.parent()
    {
        skill_roots.push(project_root.join(".agents").join("skills"));
        skill_roots.push(project_root.join(".codex").join("skills"));
    }
    if let Some(state_directory) = database.parent() {
        skill_roots.push(state_directory.join("skills"));
    }
    if let Some(profile) = std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME")) {
        let profile = PathBuf::from(profile);
        skill_roots.push(profile.join(".agents").join("skills"));
        skill_roots.push(profile.join(".codex").join("skills"));
    }
    if let Some(codex_home) = std::env::var_os("CODEX_HOME") {
        skill_roots.push(PathBuf::from(codex_home).join("skills"));
    }
    skill_roots.sort();
    skill_roots.dedup();
    let mcp_path = database
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join("mcp.json");
    let model_packs_path = database
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join("model-packs.json");
    let routing_policy_path = database
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join("routing-policy.json");
    let routing_policies =
        RoutingPolicyRegistry::open(&routing_policy_path).with_context(|| {
            format!(
                "failed to load routing policy {}",
                routing_policy_path.display()
            )
        })?;
    let routing_limits = routing_policies.limits();
    Ok(ServerState {
        runtime: Runtime::with_components(
            store,
            AgentLimits {
                max_depth: routing_limits.max_agent_depth,
                max_active_agents_per_run: routing_limits.max_active_agents,
                max_active_writers_per_run: routing_limits.max_active_writers,
                max_deep_reasoning_agents_per_run: routing_limits.max_deep_reasoning_agents,
                ..AgentLimits::default()
            },
            providers,
            ToolExecutor::default(),
            SkillRegistry::discover_many_with_builtins(&skill_roots).with_context(|| {
                format!(
                    "failed to load skills from {}",
                    skill_roots
                        .iter()
                        .map(|path| path.display().to_string())
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            })?,
        )
        .with_mcp_registry(
            McpRegistry::open(&mcp_path)
                .with_context(|| format!("failed to load MCP config {}", mcp_path.display()))?,
        )
        .with_model_pack_registry(ModelPackRegistry::open(&model_packs_path).with_context(
            || {
                format!(
                    "failed to load model packs from {}",
                    model_packs_path.display()
                )
            },
        )?)
        .with_routing_policy_registry(routing_policies),
        provider_config_path: Some(provider_config_path),
    })
}

fn log_unavailable_provider(error: &opensrc_providers::ProviderConfigError) -> bool {
    match error {
        opensrc_providers::ProviderConfigError::MissingCredential { provider, variable } => {
            tracing::warn!(
                %provider,
                %variable,
                "provider is configured but its environment variable is unavailable"
            );
            true
        }
        opensrc_providers::ProviderConfigError::CredentialStore { provider, message } => {
            tracing::warn!(
                %provider,
                %message,
                "provider is configured but its saved credential is unavailable"
            );
            true
        }
        _ => false,
    }
}

async fn run_request(
    server: &str,
    request: &str,
    project_root: &Path,
    agent: &str,
    provider: Option<&str>,
    model: Option<&str>,
    mode: Option<&str>,
) -> Result<()> {
    let project_root = std::fs::canonicalize(project_root).with_context(|| {
        format!(
            "failed to resolve project directory {}",
            project_root.display()
        )
    })?;
    let server_task = ensure_local_server(server, &project_root).await?;
    let mode = mode.map(parse_mode).transpose()?.flatten();
    let client = api_client()?;
    let response = client
        .post(format!("{server}/v1/chat"))
        .json(&json!({
            "project_root": project_root.to_string_lossy(),
            "message": request,
            "provider": provider,
            "model": model,
            "mode": mode,
            "auto": mode.is_none(),
            "agent": agent
        }))
        .send()
        .await?
        .error_for_status()?
        .json::<CliChatResponse>()
        .await?;
    println!("{}", response.result.output);
    if let Some(server_task) = server_task {
        server_task.abort();
        let _ = server_task.await;
    }
    Ok(())
}

#[derive(Debug, Deserialize)]
struct CliChatResponse {
    result: RunExecutionResult,
}

fn parse_mode(value: &str) -> Result<Option<ExecutionMode>> {
    match value {
        "direct" => Ok(Some(ExecutionMode::Direct)),
        "focused" => Ok(Some(ExecutionMode::Focused)),
        "agentic" => Ok(Some(ExecutionMode::Agentic)),
        "auto" => Ok(None),
        other => anyhow::bail!("unknown mode `{other}`; use direct, focused, agentic, or auto"),
    }
}

async fn ensure_local_server(
    server: &str,
    project_root: &Path,
) -> Result<Option<tokio::task::JoinHandle<std::io::Result<()>>>> {
    if server_is_healthy(server).await {
        return Ok(None);
    }
    let bind = local_bind_address(server)?;
    let state_dir = user_state_directory();
    std::fs::create_dir_all(&state_dir)
        .with_context(|| format!("failed to create {}", state_dir.display()))?;
    let skills_dir = project_root.join(".opensource").join("skills");
    std::fs::create_dir_all(&skills_dir)
        .with_context(|| format!("failed to create {}", skills_dir.display()))?;
    let provider_config = provider_config_for(project_root, &state_dir);
    let state = build_server_state(
        &state_dir.join("state.sqlite3"),
        Some(&provider_config),
        &skills_dir,
    )?;
    let task = tokio::spawn(opensrc_server::serve(state, bind));
    for _ in 0..40 {
        if server_is_healthy(server).await {
            return Ok(Some(task));
        }
        if task.is_finished() {
            let result = task
                .await
                .context("local server task failed before startup")?;
            result.context("local server stopped before startup")?;
            anyhow::bail!("local server stopped before startup");
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    task.abort();
    let _ = task.await;
    anyhow::bail!("timed out while starting the local server")
}

fn local_bind_address(server: &str) -> Result<SocketAddr> {
    if server == DEFAULT_SERVER {
        return Ok(DEFAULT_BIND.parse().expect("valid built-in bind address"));
    }
    let url = reqwest::Url::parse(server).context("server URL is invalid")?;
    if url.scheme() != "http"
        || !matches!(url.host_str(), Some("127.0.0.1" | "localhost"))
        || url.path() != "/"
        || url.query().is_some()
        || url.fragment().is_some()
    {
        anyhow::bail!(
            "automatic server startup only supports a plain loopback HTTP URL, got `{server}`"
        );
    }
    let port = url
        .port()
        .ok_or_else(|| anyhow::anyhow!("server URL `{server}` must include a port"))?;
    Ok(SocketAddr::from(([127, 0, 0, 1], port)))
}

async fn prepare_service_command(
    server: &str,
) -> Result<(
    PathBuf,
    Option<tokio::task::JoinHandle<std::io::Result<()>>>,
)> {
    let project_root =
        std::env::current_dir().context("failed to resolve the current directory")?;
    let project_root = std::fs::canonicalize(project_root)
        .context("failed to resolve the current project directory")?;
    let task = ensure_local_server(server, &project_root).await?;
    Ok((project_root, task))
}

async fn print_api_resource(server: &str, path: &str) -> Result<()> {
    let (_, server_task) = prepare_service_command(server).await?;
    let result = async {
        let value: serde_json::Value = api_client()?
            .get(format!("{server}{path}"))
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        println!("{}", serde_json::to_string_pretty(&value)?);
        Ok::<_, anyhow::Error>(())
    }
    .await;
    stop_local_server(server_task).await;
    result
}

async fn list_agent_definitions_command(server: &str) -> Result<()> {
    let (project_root, server_task) = prepare_service_command(server).await?;
    let result = async {
        let value: serde_json::Value = api_client()?
            .get(format!("{server}/v1/agent-definitions"))
            .query(&[("project_root", project_root.to_string_lossy().as_ref())])
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        println!("{}", serde_json::to_string_pretty(&value)?);
        Ok::<_, anyhow::Error>(())
    }
    .await;
    stop_local_server(server_task).await;
    result
}

#[allow(clippy::too_many_arguments)]
async fn auth_login(
    server: &str,
    provider: &str,
    protocol: &str,
    family: Option<&str>,
    base_url: &str,
    model: &str,
    api_key_env: &str,
    test_connection: bool,
) -> Result<()> {
    let (_, server_task) = prepare_service_command(server).await?;
    let result = async {
        let value: serde_json::Value = api_client()?
            .post(format!("{server}/v1/providers/connect"))
            .json(&json!({
                "id": provider,
                "protocol": protocol,
                "family": family,
                "base_url": base_url,
                "api_key_env": api_key_env,
                "default_model": model,
                "test_connection": test_connection
            }))
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        println!("{}", serde_json::to_string_pretty(&value)?);
        Ok::<_, anyhow::Error>(())
    }
    .await;
    stop_local_server(server_task).await;
    result
}

async fn delete_provider(server: &str, provider: &str) -> Result<()> {
    let (_, server_task) = prepare_service_command(server).await?;
    let result = async {
        api_client()?
            .delete(format!("{server}/v1/providers/{provider}"))
            .send()
            .await?
            .error_for_status()?;
        println!("removed provider `{provider}`");
        Ok::<_, anyhow::Error>(())
    }
    .await;
    stop_local_server(server_task).await;
    result
}

async fn list_models_command(server: &str, provider: Option<&str>) -> Result<()> {
    let (_, server_task) = prepare_service_command(server).await?;
    let result = async {
        let client = api_client()?;
        let mut request = client
            .get(format!("{server}/v1/models"))
            .query(&[("refresh", "true")]);
        if let Some(provider) = provider {
            request = request.query(&[("provider", provider)]);
        }
        let value: serde_json::Value = request.send().await?.error_for_status()?.json().await?;
        println!("{}", serde_json::to_string_pretty(&value)?);
        Ok::<_, anyhow::Error>(())
    }
    .await;
    stop_local_server(server_task).await;
    result
}

#[allow(clippy::too_many_lines)]
async fn session_command(command: SessionCommand) -> Result<()> {
    match command {
        SessionCommand::List { project, server } => {
            let (current_project, server_task) = prepare_service_command(&server).await?;
            let project = project.unwrap_or(current_project);
            let project = std::fs::canonicalize(&project)
                .with_context(|| format!("failed to resolve {}", project.display()))?;
            let result = async {
                let conversations: Vec<Conversation> = api_client()?
                    .get(format!("{server}/v1/conversations"))
                    .query(&[("project_root", project.to_string_lossy().as_ref())])
                    .send()
                    .await?
                    .error_for_status()?
                    .json()
                    .await?;
                println!("{}", serde_json::to_string_pretty(&conversations)?);
                Ok::<_, anyhow::Error>(())
            }
            .await;
            stop_local_server(server_task).await;
            result
        }
        SessionCommand::Delete { id, server } => {
            let (_, server_task) = prepare_service_command(&server).await?;
            let result = async {
                api_client()?
                    .post(format!("{server}/v1/conversations/{id}/archive"))
                    .json(&json!({}))
                    .send()
                    .await?
                    .error_for_status()?;
                println!("archived session {id}");
                Ok::<_, anyhow::Error>(())
            }
            .await;
            stop_local_server(server_task).await;
            result
        }
        SessionCommand::Export { id, output, server } => {
            let (_, server_task) = prepare_service_command(&server).await?;
            let result = async {
                let value: serde_json::Value = api_client()?
                    .get(format!("{server}/v1/conversations/{id}/export"))
                    .send()
                    .await?
                    .error_for_status()?
                    .json()
                    .await?;
                if let Some(path) = output {
                    let content = if path.extension().is_some_and(|value| value == "json") {
                        serde_json::to_string_pretty(&value["json"])?
                    } else {
                        value["markdown"].as_str().unwrap_or_default().to_string()
                    };
                    if let Some(parent) = path.parent() {
                        std::fs::create_dir_all(parent)?;
                    }
                    std::fs::write(&path, content)?;
                    println!("exported session to {}", path.display());
                } else {
                    println!("{}", value["markdown"].as_str().unwrap_or_default());
                }
                Ok::<_, anyhow::Error>(())
            }
            .await;
            stop_local_server(server_task).await;
            result
        }
        SessionCommand::Import {
            file,
            project,
            server,
        } => {
            let (current_project, server_task) = prepare_service_command(&server).await?;
            let project = project.unwrap_or(current_project);
            let project = std::fs::canonicalize(&project)
                .with_context(|| format!("failed to resolve {}", project.display()))?;
            let result = async {
                let content = std::fs::read_to_string(&file)
                    .with_context(|| format!("failed to read {}", file.display()))?;
                let value: serde_json::Value = serde_json::from_str(&content)
                    .with_context(|| format!("invalid JSON in {}", file.display()))?;
                let document = value.get("json").cloned().unwrap_or(value);
                let imported: Conversation = api_client()?
                    .post(format!("{server}/v1/conversations/import"))
                    .json(&json!({
                        "project_root": project,
                        "document": document
                    }))
                    .send()
                    .await?
                    .error_for_status()?
                    .json()
                    .await?;
                println!("{}", serde_json::to_string_pretty(&imported)?);
                Ok::<_, anyhow::Error>(())
            }
            .await;
            stop_local_server(server_task).await;
            result
        }
        SessionCommand::Compact { id, server } => {
            let (_, server_task) = prepare_service_command(&server).await?;
            let result = async {
                let message: serde_json::Value = api_client()?
                    .post(format!("{server}/v1/conversations/{id}/compact"))
                    .json(&json!({}))
                    .send()
                    .await?
                    .error_for_status()?
                    .json()
                    .await?;
                println!("{}", serde_json::to_string_pretty(&message)?);
                Ok::<_, anyhow::Error>(())
            }
            .await;
            stop_local_server(server_task).await;
            result
        }
    }
}

async fn skill_command(command: SkillCommand) -> Result<()> {
    match command {
        SkillCommand::Create {
            name,
            description,
            triggers,
            force,
            project,
        } => create_skill(&project, &name, &description, &triggers, force),
        SkillCommand::List { server } => print_api_resource(&server, "/v1/skills").await,
        SkillCommand::Validate { path } => {
            let discovery_root = if path.is_file() {
                let parent = path.parent().unwrap_or_else(|| Path::new("."));
                if path
                    .file_name()
                    .is_some_and(|name| name.eq_ignore_ascii_case("SKILL.md"))
                {
                    parent.parent().unwrap_or(parent).to_path_buf()
                } else {
                    parent.to_path_buf()
                }
            } else if path.join("SKILL.md").is_file() {
                path.parent().unwrap_or(&path).to_path_buf()
            } else {
                path.clone()
            };
            let registry = SkillRegistry::discover(&discovery_root)
                .with_context(|| format!("invalid skill under {}", path.display()))?;
            println!(
                "{}",
                serde_json::to_string_pretty(&json!({
                    "valid": true,
                    "skills": registry.metadata()
                }))?
            );
            Ok(())
        }
        SkillCommand::Enable { name, project } => set_skill_enabled(&project, &name, true),
        SkillCommand::Disable { name, project } => set_skill_enabled(&project, &name, false),
    }
}

fn create_skill(
    project: &Path,
    name: &str,
    description: &str,
    triggers: &[String],
    force: bool,
) -> Result<()> {
    validate_simple_name("skill", name)?;
    let project = std::fs::canonicalize(project)
        .with_context(|| format!("failed to resolve {}", project.display()))?;
    let directory = project.join(".opensource").join("skills").join(name);
    std::fs::create_dir_all(&directory)
        .with_context(|| format!("failed to create {}", directory.display()))?;
    let path = directory.join("SKILL.md");
    if path.exists() && !force {
        anyhow::bail!(
            "{} already exists; pass --force to replace it",
            path.display()
        );
    }
    let triggers = triggers
        .iter()
        .map(serde_json::to_string)
        .collect::<std::result::Result<Vec<_>, _>>()?
        .join(", ");
    let document = format!(
        "---\nname: {name}\ndescription: {}\ntriggers: [{triggers}]\n---\nDescribe the workflow, decision points, required validation, and reusable resources here.\n",
        serde_json::to_string(description)?
    );
    std::fs::write(&path, document)
        .with_context(|| format!("failed to write {}", path.display()))?;
    SkillRegistry::discover(project.join(".opensource").join("skills"))
        .with_context(|| format!("generated skill {} is invalid", path.display()))?;
    println!("created {}", path.display());
    Ok(())
}

fn set_skill_enabled(project: &Path, name: &str, enabled: bool) -> Result<()> {
    validate_simple_name("skill", name)?;
    let project = std::fs::canonicalize(project)
        .with_context(|| format!("failed to resolve {}", project.display()))?;
    let directory = project.join(".opensource").join("skills").join(name);
    let active = directory.join("SKILL.md");
    let inactive = directory.join("SKILL.md.disabled");
    let (source, destination) = if enabled {
        (&inactive, &active)
    } else {
        (&active, &inactive)
    };
    if !source.is_file() {
        anyhow::bail!("{} does not exist", source.display());
    }
    std::fs::rename(source, destination).with_context(|| {
        format!(
            "failed to move {} to {}",
            source.display(),
            destination.display()
        )
    })?;
    println!(
        "{} skill `{name}`",
        if enabled { "enabled" } else { "disabled" }
    );
    Ok(())
}

fn validate_simple_name(entity: &str, name: &str) -> Result<()> {
    if !name.is_empty()
        && name
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '-' | '_'))
    {
        Ok(())
    } else {
        anyhow::bail!("{entity} name may contain only letters, numbers, '-' and '_'")
    }
}

fn parse_key_value(value: &str) -> std::result::Result<(String, String), String> {
    let (key, value) = value
        .split_once('=')
        .ok_or_else(|| "expected TARGET=SOURCE_ENV".to_string())?;
    if key.is_empty() || value.is_empty() {
        return Err("TARGET and SOURCE_ENV must be non-empty".to_string());
    }
    Ok((key.to_string(), value.to_string()))
}

async fn mcp_command(command: McpCommand) -> Result<()> {
    match command {
        McpCommand::Add {
            name,
            command,
            args,
            env,
            url,
            token_env,
            server,
        } => {
            let transport = match (command, url) {
                (Some(command), None) => McpTransport::Stdio {
                    command,
                    args,
                    env: env.into_iter().collect(),
                },
                (None, Some(url)) => McpTransport::Http { url, token_env },
                _ => anyhow::bail!("provide exactly one of --command or --url"),
            };
            let configuration = McpServer {
                name,
                enabled: true,
                transport,
            };
            let (_, server_task) = prepare_service_command(&server).await?;
            let result = async {
                let value: serde_json::Value = api_client()?
                    .post(format!("{server}/v1/mcp"))
                    .json(&configuration)
                    .send()
                    .await?
                    .error_for_status()?
                    .json()
                    .await?;
                println!("{}", serde_json::to_string_pretty(&value)?);
                Ok::<_, anyhow::Error>(())
            }
            .await;
            stop_local_server(server_task).await;
            result
        }
        McpCommand::List { server } => print_api_resource(&server, "/v1/mcp").await,
        McpCommand::Remove { name, server } => {
            let (_, server_task) = prepare_service_command(&server).await?;
            let result = async {
                api_client()?
                    .delete(format!("{server}/v1/mcp/{name}"))
                    .send()
                    .await?
                    .error_for_status()?;
                println!("removed MCP server `{name}`");
                Ok::<_, anyhow::Error>(())
            }
            .await;
            stop_local_server(server_task).await;
            result
        }
        McpCommand::Enable { name, server } => mcp_action(&server, &name, "enable").await,
        McpCommand::Disable { name, server } => mcp_action(&server, &name, "disable").await,
        McpCommand::Debug { name, server } => mcp_action(&server, &name, "debug").await,
    }
}

async fn mcp_action(server: &str, name: &str, action: &str) -> Result<()> {
    let (_, server_task) = prepare_service_command(server).await?;
    let result = async {
        let value: serde_json::Value = api_client()?
            .post(format!("{server}/v1/mcp/{name}/{action}"))
            .json(&json!({}))
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        println!("{}", serde_json::to_string_pretty(&value)?);
        Ok::<_, anyhow::Error>(())
    }
    .await;
    stop_local_server(server_task).await;
    result
}

async fn doctor() -> Result<()> {
    let project = std::fs::canonicalize(
        std::env::current_dir().context("failed to resolve the current directory")?,
    )
    .context("failed to resolve the current project")?;
    let state_directory = user_state_directory();
    let provider_config = provider_config_for(&project, &state_directory);
    let provider_configuration = if provider_config.is_file() {
        match read_provider_file(&provider_config) {
            Ok(document) => json!({
                "status": "valid",
                "path": provider_config,
                "providers": document.providers.len()
            }),
            Err(error) => json!({
                "status": "invalid",
                "path": provider_config,
                "error": error.to_string()
            }),
        }
    } else {
        json!({
            "status": "not_configured",
            "path": provider_config,
            "next": "run `divit` and use the first-run provider setup"
        })
    };
    println!(
        "{}",
        serde_json::to_string_pretty(&json!({
            "ok": true,
            "version": env!("CARGO_PKG_VERSION"),
            "executable": std::env::current_exe().ok(),
            "project": project,
            "state_directory": state_directory,
            "local_server": if server_is_healthy(DEFAULT_SERVER).await {
                "reachable"
            } else {
                "not_running"
            },
            "provider_configuration": provider_configuration,
            "sandbox": {
                "mode": "policy_only",
                "protection": "limited",
                "note": "OS-enforced sandboxing is not available in this build"
            }
        }))?
    );
    Ok(())
}

async fn stop_local_server(server_task: Option<tokio::task::JoinHandle<std::io::Result<()>>>) {
    if let Some(server_task) = server_task {
        server_task.abort();
        let _ = server_task.await;
    }
}

async fn execute_run(server: &str, run_id: uuid::Uuid, provider: &str, model: &str) -> Result<()> {
    let client = api_client()?;
    let result = execute_run_request(&client, server, run_id, provider, model).await?;
    println!("{}", serde_json::to_string_pretty(&result)?);
    Ok(())
}

async fn execute_run_request(
    client: &reqwest::Client,
    server: &str,
    run_id: uuid::Uuid,
    provider: &str,
    model: &str,
) -> Result<RunExecutionResult> {
    Ok(client
        .post(format!("{server}/v1/runs/{run_id}/execute"))
        .json(&json!({"provider": provider, "model": model}))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?)
}

async fn status(server: &str) -> Result<()> {
    let client = api_client()?;
    let health: serde_json::Value = client
        .get(format!("{server}/v1/health"))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    let agents: Vec<Agent> = client
        .get(format!("{server}/v1/agents"))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    println!(
        "{}",
        serde_json::to_string_pretty(&json!({
            "health": health,
            "agents": agents
        }))?
    );
    Ok(())
}

fn validate_agents(directory: &Path) -> Result<()> {
    let mut files: Vec<_> = std::fs::read_dir(directory)
        .with_context(|| format!("failed to read {}", directory.display()))?
        .filter_map(std::result::Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|extension| extension == "md"))
        .collect();
    files.sort();
    let mut definitions: Vec<AgentDefinition> = Vec::new();
    for file in files {
        definitions.push(
            load_agent_definition(&file)
                .with_context(|| format!("invalid definition {}", file.display()))?,
        );
    }
    println!("validated {} agent definitions", definitions.len());
    Ok(())
}

fn create_agent_definition(
    project: &Path,
    name: &str,
    description: &str,
    workspace_mode: &str,
    tools: &[String],
    force: bool,
) -> Result<()> {
    if name.is_empty()
        || !name
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '-' | '_'))
    {
        anyhow::bail!("agent name may contain only letters, numbers, '-' and '_'");
    }
    if !matches!(
        workspace_mode,
        "shared_readonly"
            | "shared_write"
            | "owned_paths"
            | "git_worktree"
            | "temporary_copy"
            | "container_isolated"
    ) {
        anyhow::bail!(
            "workspace mode must be shared_readonly, shared_write, owned_paths, git_worktree, temporary_copy, or container_isolated"
        );
    }
    let project = std::fs::canonicalize(project)
        .with_context(|| format!("failed to resolve {}", project.display()))?;
    let directory = project.join(".opensource").join("agents");
    std::fs::create_dir_all(&directory)
        .with_context(|| format!("failed to create {}", directory.display()))?;
    let path = directory.join(format!("{name}.md"));
    if path.exists() && !force {
        anyhow::bail!(
            "{} already exists; pass --force to replace it",
            path.display()
        );
    }
    let quoted_description = serde_json::to_string(description)?;
    let quoted_tools = tools
        .iter()
        .map(serde_json::to_string)
        .collect::<std::result::Result<Vec<_>, _>>()?
        .join(", ");
    let document = format!(
        "---\nname: {name}\ndescription: {quoted_description}\ntools:\n  allow: [{quoted_tools}]\n  deny: []\n  may_spawn_children: false\nworkspace_mode: {workspace_mode}\ncompletion_schema: task_completion\n---\nDescribe the role's concrete responsibilities, constraints, and completion criteria here.\n"
    );
    std::fs::write(&path, document)
        .with_context(|| format!("failed to write {}", path.display()))?;
    load_agent_definition(&path)
        .with_context(|| format!("generated definition {} is invalid", path.display()))?;
    println!("created {}", path.display());
    Ok(())
}

#[derive(Debug, Deserialize)]
struct BenchmarkScenario {
    id: String,
    description: String,
    prompt: String,
    modes: Vec<ExecutionMode>,
}

fn benchmark_local(path: &Path, iterations: u32, output: Option<&Path>) -> Result<()> {
    let content = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read {}", path.display()))?;
    let scenarios: Vec<BenchmarkScenario> = serde_json::from_str(&content)
        .with_context(|| format!("invalid scenario file {}", path.display()))?;
    let iterations = iterations.clamp(1, 1_000_000);
    let mut results = Vec::new();
    for scenario in scenarios {
        let mut samples = Vec::with_capacity(iterations as usize);
        let mut decision = None;
        for _ in 0..iterations {
            let started = std::time::Instant::now();
            decision = Some(ModeClassifier::classify(&scenario.prompt));
            samples.push(u64::try_from(started.elapsed().as_nanos()).unwrap_or(u64::MAX));
        }
        samples.sort_unstable();
        let chosen = decision.expect("at least one benchmark iteration");
        let p50 = percentile(&samples, 50);
        let p95 = percentile(&samples, 95);
        results.push(json!({
            "id": scenario.id,
            "description": scenario.description,
            "decision": chosen,
            "accepted": scenario.modes.contains(&chosen.mode),
            "iterations": iterations,
            "latency_ns": {"p50": p50, "p95": p95, "max": samples.last()}
        }));
    }
    let report = serde_json::to_string_pretty(&json!({
        "benchmark": "local_mode_classifier",
        "results": results
    }))?;
    if let Some(output) = output {
        if let Some(parent) = output.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }
        std::fs::write(output, &report)
            .with_context(|| format!("failed to write {}", output.display()))?;
    }
    println!("{report}");
    Ok(())
}

fn percentile(sorted: &[u64], percentile: usize) -> u64 {
    let rank = sorted
        .len()
        .saturating_mul(percentile)
        .div_ceil(100)
        .saturating_sub(1);
    sorted[rank.min(sorted.len().saturating_sub(1))]
}

#[cfg(test)]
mod cli_tests {
    use super::{AuthCommand, Cli, Command, SessionCommand};
    use clap::Parser;

    #[test]
    fn no_arguments_selects_the_integrated_launcher() {
        let cli = Cli::try_parse_from(["divit"]).expect("parse no-argument command");
        assert!(cli.command.is_none());
    }

    #[test]
    fn parses_scriptable_provider_and_session_commands() {
        let cli = Cli::try_parse_from([
            "divit",
            "auth",
            "logout",
            "fixture",
            "--server",
            "http://localhost:9999",
        ])
        .expect("auth command");
        assert!(matches!(
            cli.command,
            Some(Command::Auth {
                command: AuthCommand::Logout { provider, .. }
            }) if provider == "fixture"
        ));

        let id = uuid::Uuid::new_v4();
        let cli = Cli::try_parse_from(["divit", "session", "compact", &id.to_string()])
            .expect("session command");
        assert!(matches!(
            cli.command,
            Some(Command::Session {
                command: SessionCommand::Compact { id: parsed, .. }
            }) if parsed == id
        ));
    }
}
