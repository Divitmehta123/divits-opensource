use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use thiserror::Error;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};

const MAX_MCP_MESSAGE_BYTES: usize = 4 * 1024 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "transport", rename_all = "snake_case")]
pub enum McpTransport {
    Stdio {
        command: String,
        #[serde(default)]
        args: Vec<String>,
        #[serde(default)]
        env: BTreeMap<String, String>,
    },
    Http {
        url: String,
        token_env: Option<String>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct McpServer {
    pub name: String,
    pub enabled: bool,
    #[serde(flatten)]
    pub transport: McpTransport,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct McpDocument {
    #[serde(default)]
    servers: Vec<McpServer>,
}

#[derive(Debug, Error)]
pub enum McpError {
    #[error("MCP server name `{0}` is invalid")]
    InvalidName(String),
    #[error("MCP server `{0}` was not found")]
    UnknownServer(String),
    #[error("MCP server `{0}` is disabled")]
    Disabled(String),
    #[error("MCP configuration I/O failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("MCP configuration is invalid: {0}")]
    Serialization(#[from] serde_json::Error),
    #[error("MCP HTTP request failed: {0}")]
    Http(#[from] reqwest::Error),
    #[error("MCP protocol error: {0}")]
    Protocol(String),
    #[error("MCP request timed out after {0} ms")]
    Timeout(u64),
    #[error("MCP registry lock was poisoned")]
    Poisoned,
}

#[derive(Debug, Clone, Default)]
pub struct McpRegistry {
    path: Option<PathBuf>,
    document: Arc<Mutex<McpDocument>>,
}

impl McpRegistry {
    pub fn open(path: impl Into<PathBuf>) -> Result<Self, McpError> {
        let path = path.into();
        let document = if path.is_file() {
            serde_json::from_str(&std::fs::read_to_string(&path)?)?
        } else {
            McpDocument::default()
        };
        Ok(Self {
            path: Some(path),
            document: Arc::new(Mutex::new(document)),
        })
    }

    pub fn list(&self) -> Result<Vec<McpServer>, McpError> {
        let mut servers = self
            .document
            .lock()
            .map_err(|_| McpError::Poisoned)?
            .servers
            .clone();
        servers.sort_by(|left, right| left.name.cmp(&right.name));
        Ok(servers)
    }

    pub fn upsert(&self, server: McpServer) -> Result<McpServer, McpError> {
        validate_name(&server.name)?;
        {
            let mut document = self.document.lock().map_err(|_| McpError::Poisoned)?;
            if let Some(existing) = document
                .servers
                .iter_mut()
                .find(|existing| existing.name == server.name)
            {
                *existing = server.clone();
            } else {
                document.servers.push(server.clone());
            }
        }
        self.save()?;
        Ok(server)
    }

    pub fn remove(&self, name: &str) -> Result<(), McpError> {
        let removed = {
            let mut document = self.document.lock().map_err(|_| McpError::Poisoned)?;
            let before = document.servers.len();
            document.servers.retain(|server| server.name != name);
            document.servers.len() != before
        };
        if !removed {
            return Err(McpError::UnknownServer(name.to_string()));
        }
        self.save()
    }

    pub fn set_enabled(&self, name: &str, enabled: bool) -> Result<McpServer, McpError> {
        let server = {
            let mut document = self.document.lock().map_err(|_| McpError::Poisoned)?;
            let server = document
                .servers
                .iter_mut()
                .find(|server| server.name == name)
                .ok_or_else(|| McpError::UnknownServer(name.to_string()))?;
            server.enabled = enabled;
            server.clone()
        };
        self.save()?;
        Ok(server)
    }

    pub async fn list_tools(&self, name: &str) -> Result<Value, McpError> {
        self.call_rpc(name, "tools/list", json!({}), 30_000).await
    }

    pub async fn invoke(
        &self,
        name: &str,
        tool: &str,
        arguments: Value,
        timeout_ms: u64,
    ) -> Result<Value, McpError> {
        self.call_rpc(
            name,
            "tools/call",
            json!({"name": tool, "arguments": arguments}),
            timeout_ms.clamp(1, 120_000),
        )
        .await
    }

    async fn call_rpc(
        &self,
        name: &str,
        method: &str,
        params: Value,
        timeout_ms: u64,
    ) -> Result<Value, McpError> {
        let server = self
            .list()?
            .into_iter()
            .find(|server| server.name == name)
            .ok_or_else(|| McpError::UnknownServer(name.to_string()))?;
        if !server.enabled {
            return Err(McpError::Disabled(name.to_string()));
        }
        let call = async {
            match server.transport {
                McpTransport::Stdio { command, args, env } => {
                    call_stdio(&command, &args, &env, method, params).await
                }
                McpTransport::Http { url, token_env } => {
                    call_http(&url, token_env.as_deref(), method, params).await
                }
            }
        };
        tokio::time::timeout(std::time::Duration::from_millis(timeout_ms), call)
            .await
            .map_err(|_| McpError::Timeout(timeout_ms))?
    }

    fn save(&self) -> Result<(), McpError> {
        let Some(path) = &self.path else {
            return Ok(());
        };
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let data = {
            let document = self.document.lock().map_err(|_| McpError::Poisoned)?;
            serde_json::to_vec_pretty(&*document)?
        };
        let temporary = path.with_extension("json.tmp");
        std::fs::write(&temporary, data)?;
        if path.exists() {
            std::fs::remove_file(path)?;
        }
        std::fs::rename(temporary, path)?;
        Ok(())
    }
}

async fn call_stdio(
    command: &str,
    args: &[String],
    env: &BTreeMap<String, String>,
    method: &str,
    params: Value,
) -> Result<Value, McpError> {
    if command.trim().is_empty() {
        return Err(McpError::Protocol("stdio command is empty".to_string()));
    }
    let mut process = restricted_stdio_command(command, args, env).spawn()?;
    let mut stdin = process
        .stdin
        .take()
        .ok_or_else(|| McpError::Protocol("MCP stdin was unavailable".to_string()))?;
    let stdout = process
        .stdout
        .take()
        .ok_or_else(|| McpError::Protocol("MCP stdout was unavailable".to_string()))?;
    let mut stdout = BufReader::new(stdout);
    send_rpc(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": {"name": "opensource", "version": env!("CARGO_PKG_VERSION")}
            }
        }),
    )
    .await?;
    let initialized = read_response(&mut stdout, 1).await?;
    if initialized.get("error").is_some() {
        terminate(&mut process).await;
        return Err(McpError::Protocol(initialized["error"].to_string()));
    }
    send_rpc(
        &mut stdin,
        &json!({"jsonrpc": "2.0", "method": "notifications/initialized", "params": {}}),
    )
    .await?;
    send_rpc(
        &mut stdin,
        &json!({"jsonrpc": "2.0", "id": 2, "method": method, "params": params}),
    )
    .await?;
    let response = read_response(&mut stdout, 2).await?;
    terminate(&mut process).await;
    rpc_result(&response)
}

fn restricted_stdio_command(
    program: &str,
    args: &[String],
    environment_references: &BTreeMap<String, String>,
) -> Command {
    let mut command = Command::new(program);
    command
        .args(args)
        .env_clear()
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    for name in [
        "PATH",
        "Path",
        "PATHEXT",
        "SYSTEMROOT",
        "SystemRoot",
        "WINDIR",
        "TEMP",
        "TMP",
    ] {
        if let Some(value) = std::env::var_os(name) {
            command.env(name, value);
        }
    }
    for (target, source) in environment_references {
        if let Some(value) = std::env::var_os(source) {
            command.env(target, value);
        }
    }
    command
}

async fn send_rpc(stdin: &mut ChildStdin, value: &Value) -> Result<(), McpError> {
    let mut line = serde_json::to_vec(value)?;
    line.push(b'\n');
    stdin.write_all(&line).await?;
    stdin.flush().await?;
    Ok(())
}

async fn read_response(stdout: &mut BufReader<ChildStdout>, id: i64) -> Result<Value, McpError> {
    loop {
        let mut line = String::new();
        if stdout.read_line(&mut line).await? == 0 {
            return Err(McpError::Protocol(
                "MCP server closed stdout before responding".to_string(),
            ));
        }
        if line.len() > MAX_MCP_MESSAGE_BYTES {
            return Err(McpError::Protocol(
                "MCP response exceeded 4 MiB".to_string(),
            ));
        }
        let value: Value = serde_json::from_str(line.trim())?;
        if value.get("id").and_then(Value::as_i64) == Some(id) {
            return Ok(value);
        }
    }
}

async fn terminate(process: &mut Child) {
    let _ = process.start_kill();
    let _ = process.wait().await;
}

async fn call_http(
    url: &str,
    token_env: Option<&str>,
    method: &str,
    params: Value,
) -> Result<Value, McpError> {
    if !url.starts_with("https://") && !url.starts_with("http://") {
        return Err(McpError::Protocol(
            "remote MCP URL must use http or https".to_string(),
        ));
    }
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()?;
    let mut initialize = client
        .post(url)
        .header("Accept", "application/json, text/event-stream")
        .json(&json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": {"name": "opensource", "version": env!("CARGO_PKG_VERSION")}
            }
        }));
    if let Some(token) = token_env.and_then(|name| std::env::var(name).ok()) {
        initialize = initialize.bearer_auth(token);
    }
    let initialize = initialize.send().await?.error_for_status()?;
    let session = initialize
        .headers()
        .get("Mcp-Session-Id")
        .and_then(|value| value.to_str().ok())
        .map(str::to_string);
    let initialized = parse_http_response(initialize).await?;
    if initialized.get("error").is_some() {
        return Err(McpError::Protocol(initialized["error"].to_string()));
    }
    let mut notification = client
        .post(url)
        .header("Accept", "application/json, text/event-stream")
        .json(&json!({
            "jsonrpc": "2.0",
            "method": "notifications/initialized",
            "params": {}
        }));
    if let Some(session) = session.as_deref() {
        notification = notification.header("Mcp-Session-Id", session);
    }
    if let Some(token) = token_env.and_then(|name| std::env::var(name).ok()) {
        notification = notification.bearer_auth(token);
    }
    notification.send().await?.error_for_status()?;
    let mut request = client
        .post(url)
        .header("Accept", "application/json, text/event-stream")
        .json(&json!({"jsonrpc": "2.0", "id": 2, "method": method, "params": params}));
    if let Some(session) = session {
        request = request.header("Mcp-Session-Id", session);
    }
    if let Some(token) = token_env.and_then(|name| std::env::var(name).ok()) {
        request = request.bearer_auth(token);
    }
    let response = parse_http_response(request.send().await?.error_for_status()?).await?;
    rpc_result(&response)
}

async fn parse_http_response(response: reqwest::Response) -> Result<Value, McpError> {
    let is_sse = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.contains("text/event-stream"));
    let body = response.text().await?;
    if body.len() > MAX_MCP_MESSAGE_BYTES {
        return Err(McpError::Protocol(
            "MCP response exceeded 4 MiB".to_string(),
        ));
    }
    if is_sse {
        let data = body
            .lines()
            .filter_map(|line| line.strip_prefix("data:"))
            .map(str::trim)
            .find(|line| !line.is_empty())
            .ok_or_else(|| McpError::Protocol("MCP SSE response had no data event".to_string()))?;
        Ok(serde_json::from_str(data)?)
    } else {
        Ok(serde_json::from_str(&body)?)
    }
}

fn rpc_result(response: &Value) -> Result<Value, McpError> {
    if let Some(error) = response.get("error") {
        Err(McpError::Protocol(error.to_string()))
    } else {
        response
            .get("result")
            .cloned()
            .ok_or_else(|| McpError::Protocol("MCP response had no result".to_string()))
    }
}

fn validate_name(name: &str) -> Result<(), McpError> {
    if !name.is_empty()
        && name
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '-' | '_'))
    {
        Ok(())
    } else {
        Err(McpError::InvalidName(name.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::{McpRegistry, McpServer, McpTransport};
    use std::collections::BTreeMap;
    use uuid::Uuid;

    #[test]
    fn persists_server_lifecycle_without_secret_values() {
        let root = std::env::temp_dir().join(format!("opensrc-mcp-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&root).expect("root");
        let path = root.join("mcp.json");
        let registry = McpRegistry::open(&path).expect("registry");
        registry
            .upsert(McpServer {
                name: "local".to_string(),
                enabled: true,
                transport: McpTransport::Stdio {
                    command: "server".to_string(),
                    args: vec!["--stdio".to_string()],
                    env: BTreeMap::from([("TOKEN".to_string(), "EXTERNAL_TOKEN_ENV".to_string())]),
                },
            })
            .expect("upsert");
        let reopened = McpRegistry::open(&path).expect("reopen");
        assert_eq!(reopened.list().expect("list").len(), 1);
        let persisted = std::fs::read_to_string(&path).expect("persisted");
        assert!(persisted.contains("EXTERNAL_TOKEN_ENV"));
        assert!(!persisted.contains("secret-value"));
        reopened.set_enabled("local", false).expect("disable");
        assert!(!reopened.list().expect("list")[0].enabled);
        reopened.remove("local").expect("remove");
        assert!(reopened.list().expect("list").is_empty());
        std::fs::remove_dir_all(root).expect("cleanup");
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn discovers_tools_from_a_real_stdio_json_rpc_process() {
        let root = std::env::temp_dir().join(format!("opensrc-mcp-stdio-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&root).expect("root");
        let script = root.join("server.ps1");
        std::fs::write(
            &script,
            r"$ErrorActionPreference = 'Stop'
while (($line = [Console]::In.ReadLine()) -ne $null) {
  $request = $line | ConvertFrom-Json
  if ($request.id -eq 1) {
    @{jsonrpc='2.0'; id=1; result=@{protocolVersion='2025-06-18'; capabilities=@{}; serverInfo=@{name='fixture'; version='1'}}} | ConvertTo-Json -Depth 8 -Compress
  } elseif ($request.id -eq 2) {
    @{jsonrpc='2.0'; id=2; result=@{tools=@(@{name='echo'; description='Echo'; inputSchema=@{type='object'}})}} | ConvertTo-Json -Depth 8 -Compress
  }
}
",
        )
        .expect("script");
        let registry = McpRegistry::open(root.join("mcp.json")).expect("registry");
        registry
            .upsert(McpServer {
                name: "fixture".to_string(),
                enabled: true,
                transport: McpTransport::Stdio {
                    command: "powershell.exe".to_string(),
                    args: vec![
                        "-NoProfile".to_string(),
                        "-NonInteractive".to_string(),
                        "-File".to_string(),
                        script.to_string_lossy().into_owned(),
                    ],
                    env: BTreeMap::new(),
                },
            })
            .expect("server");
        let discovery = registry.list_tools("fixture").await.expect("discovery");
        assert_eq!(discovery["tools"][0]["name"], "echo");
        std::fs::remove_dir_all(root).expect("cleanup");
    }
}
