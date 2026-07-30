#![allow(
    clippy::large_enum_variant,
    clippy::struct_excessive_bools,
    clippy::too_many_lines,
    clippy::unused_async
)]

use anyhow::{Context, Result};
use base64::Engine;
use crossterm::event::{
    self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
    Event as TerminalEvent, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseEventKind,
};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use futures::StreamExt;
use opensrc_core::{
    Agent, AgentDefinition, Approval, ApprovalDecision, CommandId, Conversation, Event,
    ExecutionMode, FileChange, FileChangeState, Message, MessageContent, MessageRole, ModelEvent,
    PerformanceSnapshot, PermissionRule, RoutingBenchmarkAggregate, RoutingBenchmarkMetrics, Run,
    Task, builtin_commands, resolve_command,
};
use opensrc_runtime::{
    CustomCommand, McpServer, ModelPackDescriptor, RolePolicyDescriptor, SkillMetadata,
    ToolDescriptor, expand_custom_command, is_continuation_request,
};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{
    Block, Borders, Cell, Clear, List, ListItem, ListState, Paragraph, Row, Table, Wrap,
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::VecDeque;
use std::io::{self, Stdout, Write};
use std::path::Path;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;

const POLL_INTERVAL: Duration = Duration::from_millis(50);
const DOUBLE_INTERRUPT: Duration = Duration::from_secs(2);
const CUBE_LOADER_STEP: Duration = Duration::from_millis(90);
const CUBE_LOADER_COUNT: usize = 7;
const PRODUCT_NAME: &str = "Divit's OpenSource";
const PANEL_BG: Color = Color::Rgb(27, 27, 27);
const SUBTLE_BG: Color = Color::Rgb(18, 18, 18);
const PRIMARY_ACCENT: Color = Color::Rgb(112, 156, 255);
const MENU_ACCENT: Color = Color::Rgb(202, 202, 202);
const ERROR_ACCENT: Color = Color::Rgb(255, 94, 141);
const MODAL_BG: Color = Color::Rgb(24, 24, 24);
const MODAL_INSET_BG: Color = Color::Rgb(31, 31, 31);
const MODAL_BORDER: Color = Color::Rgb(105, 105, 105);
const MODAL_MUTED: Color = Color::Rgb(150, 150, 150);
const MODAL_SHADOW: Color = Color::Rgb(7, 7, 7);
const MODAL_SELECTION: Color = Color::Rgb(218, 218, 218);
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum View {
    Chat,
    Changes,
    Terminal,
    Agents,
    Tasks,
    Sessions,
    Context,
    Skills,
    Tools,
    Mcp,
    Plugins,
    Metrics,
    Logs,
    Settings,
}

impl View {
    const ALL: [Self; 14] = [
        Self::Chat,
        Self::Changes,
        Self::Terminal,
        Self::Agents,
        Self::Tasks,
        Self::Sessions,
        Self::Context,
        Self::Skills,
        Self::Tools,
        Self::Mcp,
        Self::Plugins,
        Self::Metrics,
        Self::Logs,
        Self::Settings,
    ];

    const fn label(self) -> &'static str {
        match self {
            Self::Chat => "Chat",
            Self::Changes => "Changes",
            Self::Terminal => "Terminal",
            Self::Agents => "Agents",
            Self::Tasks => "Tasks",
            Self::Sessions => "Sessions",
            Self::Context => "Context",
            Self::Skills => "Skills",
            Self::Tools => "Tools",
            Self::Mcp => "MCP",
            Self::Plugins => "Extensions",
            Self::Metrics => "Metrics",
            Self::Logs => "Logs",
            Self::Settings => "Settings",
        }
    }
}

#[derive(Debug)]
enum Overlay {
    Help,
    Setup(SetupState),
    Approval(Approval),
    ApprovalEditor {
        approval: Approval,
        editor: PromptEditor,
    },
    Picker(PickerState),
    DeleteConversation(Conversation),
    Error(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PickerKind {
    Command,
    Provider,
    Model,
    ModelPack,
    Agent,
    Session,
}

#[derive(Debug, Clone)]
struct PickerOption {
    value: String,
    label: String,
    auxiliary: Option<String>,
}

#[derive(Debug)]
struct PickerState {
    kind: PickerKind,
    title: &'static str,
    options: Vec<PickerOption>,
    selected: usize,
    query: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AttachmentKind {
    Image,
    Video,
    Audio,
    File,
}

impl AttachmentKind {
    const fn label(self) -> &'static str {
        match self {
            Self::Image => "Image",
            Self::Video => "Video",
            Self::Audio => "Audio",
            Self::File => "File",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PendingAttachment {
    path: String,
    kind: AttachmentKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CredentialMode {
    ApiKey,
    Environment,
}

#[derive(Debug, Clone)]
struct ProviderTemplate {
    name: &'static str,
    id: &'static str,
    protocol: &'static str,
    family: Option<&'static str>,
    base_url: &'static str,
    model: &'static str,
    key_env: &'static str,
}

const PROVIDER_TEMPLATES: &[ProviderTemplate] = &[
    ProviderTemplate {
        name: "OpenAI",
        id: "openai",
        protocol: "openai_compatible",
        family: Some("openai"),
        base_url: "https://api.openai.com/v1",
        model: "gpt-5.6-sol",
        key_env: "OPENAI_API_KEY",
    },
    ProviderTemplate {
        name: "OpenRouter",
        id: "openrouter",
        protocol: "openai_compatible",
        family: Some("openrouter"),
        base_url: "https://openrouter.ai/api/v1",
        model: "google/gemini-3.6-flash",
        key_env: "OPENROUTER_API_KEY",
    },
    ProviderTemplate {
        name: "AICredits",
        id: "aicredits",
        protocol: "openai_compatible",
        family: Some("aicredits"),
        base_url: "https://api.aicredits.in/v1",
        model: "google/gemini-2.5-flash",
        key_env: "AICREDITS_API_KEY",
    },
    ProviderTemplate {
        name: "Krutrim Cloud (India)",
        id: "krutrim",
        protocol: "openai_compatible",
        family: Some("custom"),
        base_url: "https://cloud.olakrutrim.com/v1",
        model: "Meta-Llama-3-8B-Instruct",
        key_env: "KRUTRIM_API_KEY",
    },
    ProviderTemplate {
        name: "Groq",
        id: "groq",
        protocol: "openai_compatible",
        family: Some("custom"),
        base_url: "https://api.groq.com/openai/v1",
        model: "openai/gpt-oss-120b",
        key_env: "GROQ_API_KEY",
    },
    ProviderTemplate {
        name: "Together AI",
        id: "together",
        protocol: "openai_compatible",
        family: Some("custom"),
        base_url: "https://api.together.xyz/v1",
        model: "deepseek-ai/DeepSeek-V3.1",
        key_env: "TOGETHER_API_KEY",
    },
    ProviderTemplate {
        name: "Fireworks AI",
        id: "fireworks",
        protocol: "openai_compatible",
        family: Some("custom"),
        base_url: "https://api.fireworks.ai/inference/v1",
        model: "accounts/fireworks/models/deepseek-v3p1",
        key_env: "FIREWORKS_API_KEY",
    },
    ProviderTemplate {
        name: "Mistral AI",
        id: "mistral",
        protocol: "openai_compatible",
        family: Some("custom"),
        base_url: "https://api.mistral.ai/v1",
        model: "mistral-small-latest",
        key_env: "MISTRAL_API_KEY",
    },
    ProviderTemplate {
        name: "xAI",
        id: "xai",
        protocol: "openai_compatible",
        family: Some("custom"),
        base_url: "https://api.x.ai/v1",
        model: "grok-4.5",
        key_env: "XAI_API_KEY",
    },
    ProviderTemplate {
        name: "DeepInfra",
        id: "deepinfra",
        protocol: "openai_compatible",
        family: Some("custom"),
        base_url: "https://api.deepinfra.com/v1/openai",
        model: "deepseek-ai/DeepSeek-V3",
        key_env: "DEEPINFRA_TOKEN",
    },
    ProviderTemplate {
        name: "NVIDIA NIM",
        id: "nvidia",
        protocol: "openai_compatible",
        family: Some("custom"),
        base_url: "https://integrate.api.nvidia.com/v1",
        model: "meta/llama-3.3-70b-instruct",
        key_env: "NVIDIA_API_KEY",
    },
    ProviderTemplate {
        name: "Cerebras",
        id: "cerebras",
        protocol: "openai_compatible",
        family: Some("custom"),
        base_url: "https://api.cerebras.ai/v1",
        model: "llama-3.3-70b",
        key_env: "CEREBRAS_API_KEY",
    },
    ProviderTemplate {
        name: "SambaNova",
        id: "sambanova",
        protocol: "openai_compatible",
        family: Some("custom"),
        base_url: "https://api.sambanova.ai/v1",
        model: "Meta-Llama-3.3-70B-Instruct",
        key_env: "SAMBANOVA_API_KEY",
    },
    ProviderTemplate {
        name: "Cohere",
        id: "cohere",
        protocol: "openai_compatible",
        family: Some("custom"),
        base_url: "https://api.cohere.ai/compatibility/v1",
        model: "command-a-03-2025",
        key_env: "COHERE_API_KEY",
    },
    ProviderTemplate {
        name: "SiliconFlow",
        id: "siliconflow",
        protocol: "openai_compatible",
        family: Some("custom"),
        base_url: "https://api.siliconflow.com/v1",
        model: "deepseek-ai/DeepSeek-V3",
        key_env: "SILICONFLOW_API_KEY",
    },
    ProviderTemplate {
        name: "Hugging Face Inference",
        id: "huggingface",
        protocol: "openai_compatible",
        family: Some("custom"),
        base_url: "https://router.huggingface.co/v1",
        model: "deepseek-ai/DeepSeek-V3",
        key_env: "HF_TOKEN",
    },
    ProviderTemplate {
        name: "Perplexity",
        id: "perplexity",
        protocol: "openai_compatible",
        family: Some("custom"),
        base_url: "https://api.perplexity.ai",
        model: "sonar",
        key_env: "PERPLEXITY_API_KEY",
    },
    ProviderTemplate {
        name: "Gemini",
        id: "gemini",
        protocol: "gemini",
        family: None,
        base_url: "https://generativelanguage.googleapis.com/v1beta",
        model: "gemini-3.6-flash",
        key_env: "GEMINI_API_KEY",
    },
    ProviderTemplate {
        name: "DeepSeek",
        id: "deepseek",
        protocol: "openai_compatible",
        family: Some("deepseek"),
        base_url: "https://api.deepseek.com",
        model: "deepseek-v4-pro",
        key_env: "DEEPSEEK_API_KEY",
    },
    ProviderTemplate {
        name: "Kimi / Moonshot",
        id: "kimi",
        protocol: "openai_compatible",
        family: Some("kimi"),
        base_url: "",
        model: "",
        key_env: "MOONSHOT_API_KEY",
    },
    ProviderTemplate {
        name: "GLM / Z.AI",
        id: "glm",
        protocol: "openai_compatible",
        family: Some("glm"),
        base_url: "",
        model: "",
        key_env: "ZAI_API_KEY",
    },
    ProviderTemplate {
        name: "Qwen",
        id: "qwen",
        protocol: "openai_compatible",
        family: Some("qwen"),
        base_url: "https://dashscope-us.aliyuncs.com/compatible-mode/v1",
        model: "qwen3.7-plus",
        key_env: "DASHSCOPE_API_KEY",
    },
    ProviderTemplate {
        name: "Ollama (local)",
        id: "ollama",
        protocol: "openai_compatible",
        family: Some("custom"),
        base_url: "http://127.0.0.1:11434/v1",
        model: "",
        key_env: "OLLAMA_API_KEY",
    },
    ProviderTemplate {
        name: "LM Studio (local)",
        id: "lm-studio",
        protocol: "openai_compatible",
        family: Some("custom"),
        base_url: "http://127.0.0.1:1234/v1",
        model: "",
        key_env: "LM_STUDIO_API_KEY",
    },
    ProviderTemplate {
        name: "Local vLLM",
        id: "vllm",
        protocol: "openai_compatible",
        family: Some("custom"),
        base_url: "http://127.0.0.1:8000/v1",
        model: "",
        key_env: "VLLM_API_KEY",
    },
    ProviderTemplate {
        name: "Custom OpenAI-compatible",
        id: "custom",
        protocol: "openai_compatible",
        family: Some("custom"),
        base_url: "",
        model: "",
        key_env: "CUSTOM_API_KEY",
    },
];

struct SetupState {
    template: usize,
    credential_mode: CredentialMode,
    credential: String,
    model: String,
    base_url: String,
    field: usize,
    submitting: bool,
}

impl std::fmt::Debug for SetupState {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SetupState")
            .field("template", &self.template)
            .field("credential_mode", &self.credential_mode)
            .field(
                "credential",
                &(!self.credential.is_empty()).then_some("[REDACTED]"),
            )
            .field("model", &self.model)
            .field("base_url", &self.base_url)
            .field("field", &self.field)
            .field("submitting", &self.submitting)
            .finish()
    }
}

impl Default for SetupState {
    fn default() -> Self {
        let template_index = PROVIDER_TEMPLATES
            .iter()
            .position(|template| template.id == "openrouter")
            .unwrap_or_default();
        let template = &PROVIDER_TEMPLATES[template_index];
        Self {
            template: template_index,
            credential_mode: CredentialMode::ApiKey,
            credential: String::new(),
            model: template.model.to_string(),
            base_url: template.base_url.to_string(),
            field: 0,
            submitting: false,
        }
    }
}

impl SetupState {
    fn select_template(&mut self, index: usize) {
        self.template = index % PROVIDER_TEMPLATES.len();
        let template = &PROVIDER_TEMPLATES[self.template];
        self.model = template.model.to_string();
        self.base_url = template.base_url.to_string();
        if self.credential_mode == CredentialMode::Environment {
            self.credential = template.key_env.to_string();
        } else {
            self.credential.clear();
        }
    }

    fn active_value_mut(&mut self) -> &mut String {
        match self.field {
            0 => &mut self.credential,
            1 => &mut self.model,
            _ => &mut self.base_url,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
struct ProviderDescriptor {
    id: String,
    default_model: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ProviderPage {
    providers: Vec<ProviderDescriptor>,
}

#[derive(Debug, Clone, Deserialize)]
struct ModelDescriptor {
    provider: String,
    id: String,
    #[serde(default)]
    capabilities: ModelCapabilities,
}

#[derive(Debug, Clone, Deserialize)]
struct ModelCapabilities {
    #[serde(default = "default_true")]
    chat: bool,
    #[serde(default)]
    tools: bool,
    #[serde(default)]
    multimodal: bool,
}

impl Default for ModelCapabilities {
    fn default() -> Self {
        Self {
            chat: true,
            tools: false,
            multimodal: false,
        }
    }
}

const fn default_true() -> bool {
    true
}

#[derive(Debug, Deserialize)]
struct ModelPage {
    models: Vec<ModelDescriptor>,
}

#[derive(Debug, Deserialize)]
struct ModelPackPage {
    packs: Vec<ModelPackDescriptor>,
}

#[derive(Debug, Deserialize)]
struct RoutingPolicyPage {
    roles: Vec<RolePolicyDescriptor>,
}

#[derive(Debug)]
struct Snapshot {
    providers: Vec<ProviderDescriptor>,
    models: Vec<ModelDescriptor>,
    model_packs: Vec<ModelPackDescriptor>,
    role_policies: Vec<RolePolicyDescriptor>,
    conversations: Vec<Conversation>,
    conversation: Option<Conversation>,
    messages: Vec<Message>,
    agents: Vec<Agent>,
    tasks: Vec<Task>,
    skills: Vec<SkillMetadata>,
    custom_commands: Vec<CustomCommand>,
    mcp_servers: Vec<McpServer>,
    tools: Vec<ToolDescriptor>,
    metrics: PerformanceSnapshot,
    routing_benchmarks: Vec<RoutingBenchmarkAggregate>,
    event_cursor: i64,
    pending_approvals: Vec<Approval>,
    permissions: Vec<PermissionRule>,
    changes: Vec<FileChange>,
    agent_definitions: Vec<AgentDefinition>,
    workspace_roots: Vec<String>,
}

#[derive(Debug)]
enum ClientEvent {
    Snapshot(Snapshot),
    Domain(Event),
    ChatFinished,
    ProviderConnected {
        provider: String,
        model: String,
        persisted: bool,
    },
    CatalogRefreshed {
        models: Vec<ModelDescriptor>,
        model_packs: Vec<ModelPackDescriptor>,
        picker: PickerKind,
    },
    SelectionUpdated(Conversation),
    ApprovalDecided(Approval),
    ChangeUpdated(FileChange),
    RunCancelled(Run),
    ChatFailed(String),
    OperationFailed(String),
    Notice(String),
    Failed(String),
}

#[derive(Debug, Clone)]
struct RuntimeTraceEntry {
    key: String,
    category: &'static str,
    name: String,
    target: Option<String>,
    status: String,
    started_at: Instant,
    elapsed: Option<Duration>,
}

#[derive(Debug, Default)]
struct PromptEditor {
    text: String,
    cursor: usize,
    selection_anchor: Option<usize>,
    undo: Vec<(String, usize)>,
    redo: Vec<(String, usize)>,
    history: Vec<String>,
    history_index: Option<usize>,
}

impl PromptEditor {
    fn checkpoint(&mut self) {
        self.undo.push((self.text.clone(), self.cursor));
        if self.undo.len() > 200 {
            self.undo.remove(0);
        }
        self.redo.clear();
    }

    fn insert_char(&mut self, value: char) {
        self.checkpoint();
        self.delete_selection();
        self.text.insert(self.cursor, value);
        self.cursor += value.len_utf8();
    }

    fn insert_str(&mut self, value: &str) {
        self.checkpoint();
        self.delete_selection();
        self.text.insert_str(self.cursor, value);
        self.cursor += value.len();
    }

    fn backspace(&mut self) {
        if self.selected_range().is_some() {
            self.checkpoint();
            self.delete_selection();
            return;
        }
        if self.cursor == 0 {
            return;
        }
        self.checkpoint();
        let previous = self.text[..self.cursor]
            .char_indices()
            .next_back()
            .map_or(0, |(index, _)| index);
        self.text.drain(previous..self.cursor);
        self.cursor = previous;
    }

    fn delete(&mut self) {
        if self.selected_range().is_some() {
            self.checkpoint();
            self.delete_selection();
            return;
        }
        if self.cursor == self.text.len() {
            return;
        }
        self.checkpoint();
        let next = self.text[self.cursor..]
            .char_indices()
            .nth(1)
            .map_or(self.text.len(), |(index, _)| self.cursor + index);
        self.text.drain(self.cursor..next);
    }

    fn left(&mut self) {
        self.cursor = self.text[..self.cursor]
            .char_indices()
            .next_back()
            .map_or(0, |(index, _)| index);
    }

    fn right(&mut self) {
        if self.cursor < self.text.len() {
            self.cursor = self.text[self.cursor..]
                .char_indices()
                .nth(1)
                .map_or(self.text.len(), |(index, _)| self.cursor + index);
        }
    }

    fn move_vertical(&mut self, direction: i32) {
        let before = &self.text[..self.cursor];
        let line_start = before.rfind('\n').map_or(0, |index| index + 1);
        let column = before[line_start..].chars().count();
        if direction < 0 {
            if line_start == 0 {
                return;
            }
            let previous_end = line_start - 1;
            let previous_start = self.text[..previous_end]
                .rfind('\n')
                .map_or(0, |index| index + 1);
            self.cursor =
                byte_at_column(&self.text[previous_start..previous_end], column) + previous_start;
        } else {
            let Some(current_end_offset) = self.text[self.cursor..].find('\n') else {
                return;
            };
            let next_start = self.cursor + current_end_offset + 1;
            let next_end = self.text[next_start..]
                .find('\n')
                .map_or(self.text.len(), |index| next_start + index);
            self.cursor = byte_at_column(&self.text[next_start..next_end], column) + next_start;
        }
    }

    fn undo(&mut self) {
        if let Some(previous) = self.undo.pop() {
            self.redo.push((self.text.clone(), self.cursor));
            (self.text, self.cursor) = previous;
            self.selection_anchor = None;
        }
    }

    fn redo(&mut self) {
        if let Some(next) = self.redo.pop() {
            self.undo.push((self.text.clone(), self.cursor));
            (self.text, self.cursor) = next;
            self.selection_anchor = None;
        }
    }

    fn take(&mut self) -> String {
        let value = std::mem::take(&mut self.text);
        self.cursor = 0;
        self.selection_anchor = None;
        self.undo.clear();
        self.redo.clear();
        self.history_index = None;
        if !value.trim().is_empty() {
            self.history.push(value.clone());
        }
        value
    }

    fn history(&mut self, older: bool) {
        if self.history.is_empty() {
            return;
        }
        let index = match (self.history_index, older) {
            (None, true) => self.history.len() - 1,
            (Some(index), true) => index.saturating_sub(1),
            (Some(index), false) if index + 1 < self.history.len() => index + 1,
            _ => return,
        };
        self.history_index = Some(index);
        self.text.clone_from(&self.history[index]);
        self.cursor = self.text.len();
        self.selection_anchor = None;
    }

    fn prepare_selection(&mut self, selecting: bool) {
        if selecting {
            self.selection_anchor.get_or_insert(self.cursor);
        } else {
            self.selection_anchor = None;
        }
    }

    fn selected_range(&self) -> Option<std::ops::Range<usize>> {
        let anchor = self.selection_anchor?;
        match anchor.cmp(&self.cursor) {
            std::cmp::Ordering::Equal => None,
            std::cmp::Ordering::Less => Some(anchor..self.cursor),
            std::cmp::Ordering::Greater => Some(self.cursor..anchor),
        }
    }

    fn selected_text(&self) -> Option<&str> {
        self.selected_range().map(|range| &self.text[range])
    }

    fn delete_selection(&mut self) {
        if let Some(range) = self.selected_range() {
            self.cursor = range.start;
            self.text.drain(range);
        }
        self.selection_anchor = None;
    }
}

fn byte_at_column(value: &str, column: usize) -> usize {
    value
        .char_indices()
        .nth(column)
        .map_or(value.len(), |(index, _)| index)
}

fn copy_to_terminal_clipboard(value: &str) -> Result<()> {
    let encoded = base64::engine::general_purpose::STANDARD.encode(value.as_bytes());
    let mut stdout = io::stdout();
    write!(stdout, "\u{1b}]52;c;{encoded}\u{7}")?;
    stdout.flush()?;
    Ok(())
}

fn latest_copyable_response(messages: &[Message], streaming_text: &str) -> Option<String> {
    if !streaming_text.trim().is_empty() {
        return Some(streaming_text.to_string());
    }
    messages
        .iter()
        .rev()
        .filter(|message| message.role == MessageRole::Assistant)
        .find_map(|message| {
            let text = message
                .content
                .iter()
                .filter_map(|content| match content {
                    MessageContent::Text { text } => Some(text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n");
            (!text.trim().is_empty()).then_some(text)
        })
}

fn normalize_paste(project_root: &str, value: &str) -> String {
    let trimmed = value.trim();
    let unquoted = trimmed
        .strip_prefix('"')
        .and_then(|value| value.strip_suffix('"'))
        .unwrap_or(trimmed);
    let pasted_path = Path::new(unquoted);
    let Ok(path) = std::fs::canonicalize(pasted_path) else {
        return value.to_string();
    };
    let Ok(root) = std::fs::canonicalize(project_root) else {
        return value.to_string();
    };
    let display = path
        .strip_prefix(root)
        .map_or_else(
            |_| path.to_string_lossy().into_owned(),
            |relative| relative.to_string_lossy().into_owned(),
        )
        .replace('\\', "/");
    if display.contains(char::is_whitespace) {
        format!("@\"{display}\"")
    } else {
        format!("@{display}")
    }
}

fn attachment_kind(path: &Path) -> AttachmentKind {
    match path
        .extension()
        .and_then(|value| value.to_str())
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp" | "svg") => AttachmentKind::Image,
        Some("mp4" | "mov" | "mkv" | "webm" | "avi") => AttachmentKind::Video,
        Some("mp3" | "wav" | "m4a" | "aac" | "ogg" | "flac") => AttachmentKind::Audio,
        _ => AttachmentKind::File,
    }
}

fn dropped_files(value: &str) -> Vec<PendingAttachment> {
    let trimmed = value.trim();
    let mut candidates = Vec::new();
    for quote in ['"', '\''] {
        let mut remainder = trimmed;
        while let Some(start) = remainder.find(quote) {
            remainder = &remainder[start + 1..];
            let Some(end) = remainder.find(quote) else {
                break;
            };
            candidates.push(remainder[..end].to_string());
            remainder = &remainder[end + 1..];
        }
    }
    if candidates.is_empty() {
        candidates.extend(trimmed.lines().map(str::to_string));
    }
    let mut attachments = Vec::new();
    for candidate in candidates {
        let candidate = candidate
            .trim()
            .trim_start_matches('&')
            .trim()
            .trim_matches(['"', '\'']);
        let candidate = candidate
            .strip_prefix("file:///")
            .or_else(|| candidate.strip_prefix("file://"))
            .unwrap_or(candidate)
            .replace("%20", " ");
        let resolved = std::fs::canonicalize(&candidate).ok().into_iter().chain(
            candidate
                .split_whitespace()
                .filter_map(|part| std::fs::canonicalize(part).ok()),
        );
        for path in resolved.filter(|path| path.is_file()) {
            let path = path.to_string_lossy().into_owned();
            if !attachments
                .iter()
                .any(|attachment: &PendingAttachment| attachment.path == path)
            {
                attachments.push(PendingAttachment {
                    kind: attachment_kind(Path::new(&path)),
                    path,
                });
            }
        }
    }
    attachments
}

fn attach_files(app: &mut App, attachments: Vec<PendingAttachment>) -> usize {
    let before = app.attachments.len();
    for attachment in attachments {
        if !app
            .attachments
            .iter()
            .any(|current| current.path == attachment.path)
        {
            app.attachments.push(attachment);
        }
    }
    app.attachments.len().saturating_sub(before)
}

fn capture_editor_drop(app: &mut App) -> bool {
    let attachments = dropped_files(&app.editor.text);
    if attachments.is_empty() {
        return false;
    }
    let added = attach_files(app, attachments);
    app.editor = PromptEditor::default();
    app.suggestion_index = 0;
    app.activity.push_back(format!("attached {added} file(s)"));
    true
}

fn edit_prompt_externally(editor: &mut PromptEditor) -> Result<()> {
    let path = std::env::temp_dir().join(format!("opensource-prompt-{}.md", uuid::Uuid::new_v4()));
    std::fs::write(&path, &editor.text)
        .with_context(|| format!("failed to write {}", path.display()))?;
    let configured = std::env::var("VISUAL")
        .or_else(|_| std::env::var("EDITOR"))
        .unwrap_or_else(|_| {
            if cfg!(windows) {
                "notepad.exe".to_string()
            } else {
                "vi".to_string()
            }
        });
    let command = shell_words::split(&configured)
        .with_context(|| format!("invalid editor command `{configured}`"))?;
    let (program, arguments) = command
        .split_first()
        .ok_or_else(|| anyhow::anyhow!("editor command is empty"))?;
    let status = std::process::Command::new(program)
        .args(arguments)
        .arg(&path)
        .status()
        .with_context(|| format!("failed to launch `{program}`"))?;
    if !status.success() {
        anyhow::bail!("editor exited with status {status}");
    }
    let changed = std::fs::read_to_string(&path)
        .with_context(|| format!("failed to read {}", path.display()))?;
    let _ = std::fs::remove_file(&path);
    editor.checkpoint();
    editor.text = changed;
    editor.cursor = editor.text.len();
    editor.selection_anchor = None;
    Ok(())
}

#[derive(Debug)]
struct App {
    project_root: String,
    connected: bool,
    view: View,
    overlay: Option<Overlay>,
    editor: PromptEditor,
    attachments: Vec<PendingAttachment>,
    suggestion_index: usize,
    providers: Vec<ProviderDescriptor>,
    models: Vec<ModelDescriptor>,
    model_packs: Vec<ModelPackDescriptor>,
    role_policies: Vec<RolePolicyDescriptor>,
    provider: Option<String>,
    model: Option<String>,
    model_pack: Option<String>,
    reasoning_level: Option<String>,
    selected_agent: String,
    automatic_agent: bool,
    mode: Option<ExecutionMode>,
    conversation: Option<Conversation>,
    conversations: Vec<Conversation>,
    messages: Vec<Message>,
    streaming_text: String,
    pending_prompt: Option<String>,
    loader_started: Option<Instant>,
    chat_scroll_offset: u16,
    tool_details_expanded: bool,
    activity: VecDeque<String>,
    runtime_trace: VecDeque<RuntimeTraceEntry>,
    agents: Vec<Agent>,
    agent_definitions: Vec<AgentDefinition>,
    tasks: Vec<Task>,
    skills: Vec<SkillMetadata>,
    active_skills: Vec<String>,
    last_active_skills: Vec<String>,
    custom_commands: Vec<CustomCommand>,
    prompt_allowed_tools: Vec<String>,
    mcp_servers: Vec<McpServer>,
    tools: Vec<ToolDescriptor>,
    permissions: Vec<PermissionRule>,
    changes: Vec<FileChange>,
    workspace_roots: Vec<String>,
    metrics: PerformanceSnapshot,
    routing_benchmarks: Vec<RoutingBenchmarkAggregate>,
    live_input_tokens: u64,
    live_output_tokens: u64,
    live_cached_tokens: u64,
    busy: bool,
    active_run: Option<uuid::Uuid>,
    cancelling: bool,
    last_error: Option<String>,
    last_ctrl_c: Option<Instant>,
    should_quit: bool,
}

impl App {
    fn new(project_root: &Path) -> Self {
        Self {
            project_root: project_root.to_string_lossy().into_owned(),
            connected: false,
            view: View::Chat,
            overlay: None,
            editor: PromptEditor::default(),
            attachments: Vec::new(),
            suggestion_index: 0,
            providers: Vec::new(),
            models: Vec::new(),
            model_packs: Vec::new(),
            role_policies: Vec::new(),
            provider: None,
            model: None,
            model_pack: None,
            reasoning_level: None,
            selected_agent: "auto".to_string(),
            automatic_agent: true,
            mode: None,
            conversation: None,
            conversations: Vec::new(),
            messages: Vec::new(),
            streaming_text: String::new(),
            pending_prompt: None,
            loader_started: None,
            chat_scroll_offset: 0,
            tool_details_expanded: false,
            activity: VecDeque::new(),
            runtime_trace: VecDeque::new(),
            agents: Vec::new(),
            agent_definitions: Vec::new(),
            tasks: Vec::new(),
            skills: Vec::new(),
            active_skills: Vec::new(),
            last_active_skills: Vec::new(),
            custom_commands: Vec::new(),
            prompt_allowed_tools: Vec::new(),
            mcp_servers: Vec::new(),
            tools: Vec::new(),
            permissions: Vec::new(),
            changes: Vec::new(),
            workspace_roots: Vec::new(),
            metrics: PerformanceSnapshot::default(),
            routing_benchmarks: Vec::new(),
            live_input_tokens: 0,
            live_output_tokens: 0,
            live_cached_tokens: 0,
            busy: false,
            active_run: None,
            cancelling: false,
            last_error: None,
            last_ctrl_c: None,
            should_quit: false,
        }
    }

    fn apply_snapshot(&mut self, snapshot: Snapshot) {
        self.connected = true;
        self.providers = snapshot.providers;
        self.models = snapshot.models;
        self.model_packs = snapshot.model_packs;
        self.role_policies = snapshot.role_policies;
        self.conversations = snapshot.conversations;
        self.conversation = snapshot.conversation;
        self.messages = snapshot.messages;
        if self.pending_prompt.as_ref().is_some_and(|prompt| {
            self.messages
                .iter()
                .rev()
                .filter(|message| message.role == MessageRole::User)
                .any(|message| message_contains_text(message, prompt))
        }) {
            self.pending_prompt = None;
        }
        self.agents = snapshot.agents;
        self.agent_definitions = snapshot.agent_definitions;
        self.tasks = snapshot.tasks;
        self.skills = snapshot.skills;
        self.custom_commands = snapshot.custom_commands;
        self.mcp_servers = snapshot.mcp_servers;
        self.tools = snapshot.tools;
        self.permissions = snapshot.permissions;
        self.changes = snapshot.changes;
        self.workspace_roots = snapshot.workspace_roots;
        self.metrics = snapshot.metrics;
        self.routing_benchmarks = snapshot.routing_benchmarks;
        if self.overlay.is_none()
            && let Some(approval) = snapshot.pending_approvals.first()
        {
            self.overlay = Some(Overlay::Approval(approval.clone()));
        }
        if let Some(conversation) = &self.conversation {
            self.provider.clone_from(&conversation.provider);
            self.model.clone_from(&conversation.model);
            self.model_pack.clone_from(&conversation.model_pack);
            self.reasoning_level
                .clone_from(&conversation.reasoning_level);
            if !self.automatic_agent
                && let Some(agent) = &conversation.agent
            {
                self.selected_agent.clone_from(agent);
            }
            self.mode = conversation.preferred_mode;
        }
        if self.provider.is_none()
            && let Some(provider) = self.providers.first()
        {
            self.provider = Some(provider.id.clone());
            self.model.clone_from(&provider.default_model);
        }
        if self.providers.is_empty() {
            self.overlay = Some(Overlay::Setup(SetupState::default()));
        }
    }

    fn set_trace(
        &mut self,
        key: impl Into<String>,
        category: &'static str,
        name: impl Into<String>,
        target: Option<String>,
        status: impl Into<String>,
        finished: bool,
    ) {
        let key = key.into();
        let status = status.into();
        if let Some(entry) = self.runtime_trace.iter_mut().find(|entry| entry.key == key) {
            entry.category = category;
            entry.name = name.into();
            if target.is_some() {
                entry.target = target;
            }
            entry.status = status;
            if finished && entry.elapsed.is_none() {
                entry.elapsed = Some(entry.started_at.elapsed());
            }
        } else {
            self.runtime_trace.push_back(RuntimeTraceEntry {
                key,
                category,
                name: name.into(),
                target,
                status,
                started_at: Instant::now(),
                elapsed: finished.then_some(Duration::ZERO),
            });
        }
        while self.runtime_trace.len() > 24 {
            self.runtime_trace.pop_front();
        }
    }

    fn finish_trace(
        &mut self,
        key: &str,
        fallback_category: &'static str,
        fallback_name: &str,
        status: &str,
    ) {
        if let Some(entry) = self.runtime_trace.iter_mut().find(|entry| entry.key == key) {
            entry.status = status.to_string();
            entry.elapsed = Some(entry.started_at.elapsed());
        } else {
            self.set_trace(key, fallback_category, fallback_name, None, status, true);
        }
    }

    fn model_pack_descriptor(&self, id: &str) -> Option<&ModelPackDescriptor> {
        self.model_packs
            .iter()
            .find(|descriptor| descriptor.pack.id == id)
    }

    fn apply_domain_event(&mut self, event: &Event) {
        if let Some(conversation) = &self.conversation
            && event.conversation_id != conversation.id
        {
            return;
        }
        match event.kind.as_str() {
            "model.event" => {
                if !self.busy {
                    return;
                }
                if let Some(value) = event.payload.get("event")
                    && let Ok(model_event) = serde_json::from_value::<ModelEvent>(value.clone())
                {
                    match model_event {
                        ModelEvent::TextDelta { text } => self.streaming_text.push_str(&text),
                        ModelEvent::ToolCall {
                            id,
                            name,
                            arguments,
                        } => {
                            self.activity.push_back(format!("tool call: {name}"));
                            self.set_trace(
                                format!("tool:{id}"),
                                "tool",
                                name,
                                tool_trace_target(&arguments),
                                "running",
                                false,
                            );
                        }
                        ModelEvent::Usage {
                            input_tokens,
                            output_tokens,
                            cached_tokens,
                        } => {
                            self.live_input_tokens = input_tokens;
                            self.live_output_tokens = output_tokens;
                            self.live_cached_tokens = cached_tokens;
                            self.activity.push_back(format!(
                                "usage: {input_tokens} in / {output_tokens} out / {cached_tokens} cached"
                            ));
                        }
                        ModelEvent::Completed { .. } => {
                            self.activity.push_back("model turn completed".to_string());
                            if self.runtime_trace.iter().any(|entry| {
                                entry.key == "agent:synthesis" && entry.elapsed.is_none()
                            }) {
                                self.finish_trace(
                                    "agent:synthesis",
                                    "agent",
                                    "generalist",
                                    "complete",
                                );
                            }
                        }
                    }
                }
            }
            "tool.started" => {
                let name = event.payload["name"].as_str().unwrap_or("tool");
                let call_id = event.payload["call_id"].as_str().unwrap_or(name);
                self.set_trace(
                    format!("tool:{call_id}"),
                    "tool",
                    name,
                    event.payload["target"].as_str().map(str::to_string),
                    "running",
                    false,
                );
            }
            "tool.completed" => {
                let name = event.payload["name"].as_str().unwrap_or("tool");
                self.activity.push_back(format!("completed: {name}"));
                let call_id = event.payload["call_id"].as_str().unwrap_or(name);
                let key = format!("tool:{call_id}");
                let status = event.payload["status"].as_str().unwrap_or("done");
                self.finish_trace(&key, "tool", name, status);
                if let Some(elapsed_ms) = event.payload["elapsed_ms"].as_u64()
                    && let Some(entry) =
                        self.runtime_trace.iter_mut().find(|entry| entry.key == key)
                {
                    entry.elapsed = Some(Duration::from_millis(elapsed_ms));
                }
            }
            "skill.activated" => {
                let name = event.payload["name"].as_str().unwrap_or("skill");
                self.set_trace(format!("skill:{name}"), "skill", name, None, "active", true);
            }
            "routing.policy_selected" => {
                let role = event.payload["role"].as_str().unwrap_or("model");
                let key = routing_trace_key(event, Some(role));
                let name = self
                    .runtime_trace
                    .iter()
                    .find(|entry| entry.key == key)
                    .map_or_else(|| role.to_string(), |entry| entry.name.clone());
                let reason = event.payload["reason"]
                    .as_str()
                    .unwrap_or("policy selected")
                    .replace('_', " ");
                self.set_trace(
                    key,
                    "route",
                    name,
                    provider_model_target(&event.payload, "provider", "model"),
                    reason,
                    false,
                );
            }
            "routing.model_pinned" => {
                let role = event.payload["role"].as_str().unwrap_or("model");
                let key = routing_trace_key(event, Some(role));
                let name = self
                    .runtime_trace
                    .iter()
                    .find(|entry| entry.key == key)
                    .map_or_else(|| role.to_string(), |entry| entry.name.clone());
                self.set_trace(
                    key,
                    "route",
                    name,
                    provider_model_target(&event.payload, "provider", "model"),
                    "pinned",
                    false,
                );
            }
            "agent.route_changed" => {
                let key = routing_trace_key(event, None);
                let name = self
                    .runtime_trace
                    .iter()
                    .find(|entry| entry.key == key)
                    .map_or_else(|| "model".to_string(), |entry| entry.name.clone());
                self.set_trace(
                    key,
                    "route",
                    name,
                    model_transition_target(&event.payload),
                    "route updated",
                    false,
                );
            }
            "routing.model_transition" => {
                let key = routing_trace_key(event, None);
                let name = self
                    .runtime_trace
                    .iter()
                    .find(|entry| entry.key == key)
                    .map_or_else(|| "model".to_string(), |entry| entry.name.clone());
                let reason = event.payload["reason"]
                    .as_str()
                    .unwrap_or("fallback")
                    .replace('_', " ");
                let pinned = event.payload["pinned_for_remaining_agent_cycles"]
                    .as_bool()
                    .unwrap_or(false);
                self.set_trace(
                    key,
                    "route",
                    name,
                    model_transition_target(&event.payload),
                    if pinned {
                        format!("{reason} active (pinned)")
                    } else {
                        format!("{reason} active")
                    },
                    false,
                );
            }
            "model_pack.selected" => {
                let id = event.payload["id"].as_str().unwrap_or("model pack");
                self.set_trace(
                    format!("pack:{id}"),
                    "pack",
                    id,
                    self.model_pack_descriptor(id)
                        .map(|pack| format!("{} models", pack.pack.members.len())),
                    "selected",
                    true,
                );
            }
            "model_pack.assignment_selected" => {
                let role = event.payload["role"].as_str().unwrap_or("agent");
                let stage = event.payload["stage"].as_str().unwrap_or("assigned");
                let provider = event.payload["provider"].as_str().unwrap_or_default();
                let model = event.payload["model"].as_str().unwrap_or_default();
                let target = (!provider.is_empty() || !model.is_empty())
                    .then(|| format!("{provider}/{model}").trim_matches('/').to_string());
                self.set_trace(
                    format!(
                        "assignment:{}:{stage}:{role}",
                        event
                            .agent_id
                            .map_or_else(|| "root".to_string(), |id| id.to_string())
                    ),
                    "agent",
                    role,
                    target,
                    stage.replace('_', " "),
                    true,
                );
            }
            "agent.plan_started" => {
                let provider = event.payload["provider"].as_str().unwrap_or_default();
                let model = event.payload["model"].as_str().unwrap_or_default();
                self.set_trace(
                    "agent:plan",
                    "agent",
                    "architect",
                    Some(format!("{provider}/{model}").trim_matches('/').to_string()),
                    "planning",
                    false,
                );
            }
            "agent.plan_created" => {
                let count = event.payload["task_count"].as_u64().unwrap_or_default();
                self.set_trace(
                    "agent:plan",
                    "agent",
                    "architect",
                    Some(format!("{count} task{}", if count == 1 { "" } else { "s" })),
                    "planned",
                    true,
                );
            }
            "agent.plan_fallback" => {
                self.set_trace(
                    "agent:plan",
                    "agent",
                    "architect",
                    Some("safe fallback".to_string()),
                    "recovered",
                    true,
                );
            }
            "agent.synthesis_started" => {
                let provider = event.payload["provider"].as_str().unwrap_or_default();
                let model = event.payload["model"].as_str().unwrap_or_default();
                self.set_trace(
                    "agent:synthesis",
                    "agent",
                    "generalist",
                    Some(format!("{provider}/{model}").trim_matches('/').to_string()),
                    "synthesizing",
                    false,
                );
            }
            "task.contract_issued" => {
                let role = event.payload["role"].as_str().unwrap_or("agent");
                let provider = event.payload["provider"].as_str().unwrap_or_default();
                let model = event.payload["model"].as_str().unwrap_or_default();
                self.set_trace(
                    format!(
                        "agent:{}",
                        event
                            .agent_id
                            .map_or_else(|| role.to_string(), |id| id.to_string())
                    ),
                    "agent",
                    role,
                    Some(format!("{provider}/{model}").trim_matches('/').to_string()),
                    "working",
                    false,
                );
            }
            "agent.created" => {
                if let Ok(agent) = serde_json::from_value::<Agent>(event.payload.clone()) {
                    self.set_trace(
                        format!("agent:{}", agent.id),
                        "agent",
                        agent.role,
                        Some(format!("{}/{}", agent.provider, agent.model)),
                        format!("{:?}", agent.status).to_ascii_lowercase(),
                        false,
                    );
                }
            }
            "agent.status_changed" => {
                if let Some(agent_id) = event.agent_id {
                    let key = format!("agent:{agent_id}");
                    let status = event.payload["to"]
                        .as_str()
                        .unwrap_or("updated")
                        .replace('_', " ");
                    let finished = matches!(
                        status.as_str(),
                        "completed" | "failed" | "interrupted" | "cancelled" | "unloaded"
                    );
                    if let Some(entry) =
                        self.runtime_trace.iter_mut().find(|entry| entry.key == key)
                    {
                        entry.status.clone_from(&status);
                        if finished {
                            entry.elapsed = Some(entry.started_at.elapsed());
                        }
                    } else {
                        self.set_trace(key, "agent", "agent", None, status, finished);
                    }
                }
            }
            "agent.completed_contract" => {
                if let Some(agent_id) = event.agent_id {
                    self.finish_trace(&format!("agent:{agent_id}"), "agent", "agent", "verified");
                }
            }
            "agent.message_delivered" => {
                let target = event.agent_id.map(|id| id.to_string());
                self.set_trace(
                    format!("coordination:{}", event.id),
                    "agent",
                    "coordination",
                    target,
                    "delivered",
                    true,
                );
            }
            "approval.created" => {
                if let Ok(approval) = serde_json::from_value::<Approval>(event.payload.clone()) {
                    self.overlay = Some(Overlay::Approval(approval));
                }
            }
            "approval.decided" => {
                if matches!(
                    &self.overlay,
                    Some(Overlay::Approval(approval))
                        if event.payload["id"].as_str() == Some(&approval.id.to_string())
                ) {
                    self.overlay = None;
                }
            }
            "change.recorded" | "change.state_changed" => {
                if let Ok(change) = serde_json::from_value::<FileChange>(event.payload.clone()) {
                    self.changes.retain(|item| item.id != change.id);
                    self.changes.insert(0, change);
                }
            }
            "task.status_changed" => {
                self.activity.push_back(event.kind.clone());
            }
            "provider.retry_scheduled" => {
                let attempt = event.payload["next_attempt"].as_u64().unwrap_or(0);
                let wait = event.payload["backoff_ms"].as_u64().unwrap_or(0);
                self.activity.push_back(format!(
                    "provider temporarily unavailable; retry {attempt} in {wait} ms"
                ));
            }
            "provider.fallback_selected" => {
                let key = routing_trace_key(event, None);
                let name = self
                    .runtime_trace
                    .iter()
                    .find(|entry| entry.key == key)
                    .map_or_else(|| "model".to_string(), |entry| entry.name.clone());
                self.set_trace(
                    key,
                    "route",
                    name,
                    fallback_transition_target(&event.payload),
                    "switching after failure",
                    false,
                );
            }
            "mode.selected" => {
                let mode = event.payload["mode"].as_str().unwrap_or("unknown");
                let reasons = event.payload["reasons"]
                    .as_array()
                    .map(|values| {
                        values
                            .iter()
                            .filter_map(Value::as_str)
                            .collect::<Vec<_>>()
                            .join("; ")
                    })
                    .unwrap_or_default();
                self.activity
                    .push_back(format!("mode selected: {mode} — {reasons}"));
            }
            "run.created" => {
                if let Ok(run) = serde_json::from_value::<Run>(event.payload.clone()) {
                    self.active_run = Some(run.id);
                }
            }
            _ => {}
        }
        while self.activity.len() > 100 {
            self.activity.pop_front();
        }
    }

    fn cycle_view(&mut self, direction: i32) {
        let index = View::ALL
            .iter()
            .position(|view| *view == self.view)
            .unwrap_or_default();
        let length = View::ALL.len();
        let next = if direction < 0 {
            index.checked_sub(1).unwrap_or(length - 1)
        } else {
            (index + 1) % length
        };
        self.view = View::ALL[next];
    }
}

fn routing_trace_key(event: &Event, role: Option<&str>) -> String {
    event.agent_id.map_or_else(
        || format!("routing:{}", role.unwrap_or("fallback")),
        |agent_id| format!("agent:{agent_id}"),
    )
}

fn provider_model_target(payload: &Value, provider_key: &str, model_key: &str) -> Option<String> {
    let provider = payload[provider_key].as_str().unwrap_or_default();
    let model = payload[model_key].as_str().unwrap_or_default();
    (!provider.is_empty() || !model.is_empty())
        .then(|| format!("{provider}/{model}").trim_matches('/').to_string())
}

fn model_transition_target(payload: &Value) -> Option<String> {
    let from = provider_model_target(payload, "from_provider", "from_model");
    let to = provider_model_target(payload, "to_provider", "to_model");
    match (from, to) {
        (Some(from), Some(to)) if from != to => Some(format!("{from} -> {to}")),
        (_, Some(to)) => Some(to),
        (Some(from), None) => Some(from),
        (None, None) => None,
    }
}

fn fallback_transition_target(payload: &Value) -> Option<String> {
    let from = provider_model_target(payload, "failed_provider", "failed_model");
    let to = provider_model_target(payload, "next_provider", "next_model");
    match (from, to) {
        (Some(from), Some(to)) => Some(format!("{from} -> {to}")),
        (_, Some(to)) => Some(to),
        (Some(from), None) => Some(from),
        (None, None) => None,
    }
}

fn tool_trace_target(arguments: &Value) -> Option<String> {
    const KEYS: [&str; 10] = [
        "path", "root", "command", "query", "pattern", "url", "name", "file", "from", "to",
    ];
    const MAX: usize = 56;
    let value = KEYS.iter().find_map(|key| {
        arguments
            .get(*key)
            .and_then(Value::as_str)
            .map(str::to_string)
    })?;
    let mut value = value.replace(['\r', '\n', '\t'], " ");
    if value.chars().count() > MAX {
        value = format!("{}…", value.chars().take(MAX - 1).collect::<String>());
    }
    Some(value)
}

pub async fn run(server: &str, project_root: &Path) -> Result<()> {
    let mut terminal = TerminalSession::enter()?;
    let client = http_client()?;
    let (tx, mut rx) = mpsc::unbounded_channel();
    let mut app = App::new(project_root);
    let mut event_stream_started = false;
    spawn_snapshot(
        &client,
        server,
        &app.project_root,
        app.conversation.as_ref().map(|value| value.id),
        &tx,
    );

    while !app.should_quit {
        while let Ok(message) = rx.try_recv() {
            let chat_finished = matches!(message, ClientEvent::ChatFinished);
            let pipeline_changed = matches!(
                &message,
                ClientEvent::Domain(event)
                    if matches!(
                        event.kind.as_str(),
                        "agent.created"
                            | "agent.status_changed"
                            | "task.created"
                            | "task.status_changed"
                    )
            );
            let stream_cursor = match &message {
                ClientEvent::Snapshot(snapshot) if !event_stream_started => {
                    Some(snapshot.event_cursor)
                }
                _ => None,
            };
            handle_client_event(&mut app, message);
            if let Some(cursor) = stream_cursor {
                spawn_event_stream(client.clone(), server.to_string(), cursor, tx.clone());
                event_stream_started = true;
            }
            if chat_finished {
                spawn_snapshot(
                    &client,
                    server,
                    &app.project_root,
                    app.conversation.as_ref().map(|value| value.id),
                    &tx,
                );
            } else if pipeline_changed {
                spawn_snapshot(
                    &client,
                    server,
                    &app.project_root,
                    app.conversation.as_ref().map(|value| value.id),
                    &tx,
                );
            }
        }
        terminal.terminal.draw(|frame| render(frame, &app))?;
        if event::poll(POLL_INTERVAL)? {
            match event::read()? {
                TerminalEvent::Key(key) if key.kind == KeyEventKind::Press => {
                    if app.overlay.is_none()
                        && key.modifiers.contains(KeyModifiers::CONTROL)
                        && matches!(key.code, KeyCode::Char('e' | 'E'))
                    {
                        terminal.suspend()?;
                        if let Err(error) = edit_prompt_externally(&mut app.editor) {
                            app.overlay =
                                Some(Overlay::Error(format!("external editor failed: {error}")));
                        }
                        terminal.resume()?;
                    } else {
                        handle_key(&mut app, key, &client, server, &tx);
                    }
                }
                TerminalEvent::Paste(value) => {
                    if app.overlay.is_none() {
                        let attachments = dropped_files(&value);
                        if attachments.is_empty() {
                            let value = normalize_paste(&app.project_root, &value);
                            if value.len() > 32 * 1024 {
                                app.activity.push_back(format!(
                                    "inserted a large paste ({} bytes)",
                                    value.len()
                                ));
                            }
                            app.editor.insert_str(&value);
                            capture_editor_drop(&mut app);
                        } else {
                            let added = attach_files(&mut app, attachments);
                            app.activity.push_back(format!("attached {added} file(s)"));
                        }
                    } else if let Some(Overlay::Setup(setup)) = app.overlay.as_mut() {
                        setup.active_value_mut().push_str(&value);
                    }
                }
                TerminalEvent::Mouse(mouse) if app.overlay.is_none() => match mouse.kind {
                    MouseEventKind::ScrollUp => scroll_chat(&mut app, 4),
                    MouseEventKind::ScrollDown => scroll_chat(&mut app, -4),
                    _ => {}
                },
                TerminalEvent::Resize(_, _)
                | TerminalEvent::FocusGained
                | TerminalEvent::FocusLost
                | TerminalEvent::Mouse(_)
                | TerminalEvent::Key(_) => {}
            }
        }
    }
    Ok(())
}

fn http_client() -> Result<reqwest::Client> {
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

fn handle_client_event(app: &mut App, event: ClientEvent) {
    match event {
        ClientEvent::Snapshot(snapshot) => app.apply_snapshot(snapshot),
        ClientEvent::Domain(event) => app.apply_domain_event(&event),
        ClientEvent::ChatFinished => {
            app.busy = false;
            app.active_run = None;
            app.cancelling = false;
            app.loader_started = None;
            app.streaming_text.clear();
            app.attachments.clear();
            app.activity.push_back("response completed".to_string());
        }
        ClientEvent::ProviderConnected {
            provider,
            model,
            persisted,
        } => {
            if !model.trim().is_empty() {
                app.models
                    .retain(|candidate| !(candidate.provider == provider && candidate.id == model));
                app.models.push(ModelDescriptor {
                    provider: provider.clone(),
                    id: model.clone(),
                    capabilities: ModelCapabilities::default(),
                });
            }
            app.provider = Some(provider);
            app.model = (!model.trim().is_empty()).then_some(model);
            app.busy = false;
            app.loader_started = None;
            app.overlay = None;
            app.activity.push_back(if persisted {
                "provider connected and saved".to_string()
            } else {
                "provider connected for this process; use an environment variable to persist"
                    .to_string()
            });
        }
        ClientEvent::CatalogRefreshed {
            models,
            model_packs,
            picker,
        } => {
            app.models = models;
            app.model_packs = model_packs;
            open_picker(app, picker);
        }
        ClientEvent::SelectionUpdated(conversation) => {
            if let Some(current) = app
                .conversations
                .iter_mut()
                .find(|current| current.id == conversation.id)
            {
                current.clone_from(&conversation);
            }
            app.conversation = Some(conversation);
        }
        ClientEvent::ApprovalDecided(approval) => {
            app.overlay = None;
            app.activity.push_back(format!(
                "approval {}: {:?}",
                approval.tool_name, approval.status
            ));
        }
        ClientEvent::ChangeUpdated(change) => {
            app.changes.retain(|item| item.id != change.id);
            app.changes.insert(0, change);
        }
        ClientEvent::RunCancelled(run) => {
            app.busy = false;
            app.cancelling = true;
            app.active_run = None;
            app.loader_started = None;
            app.activity.push_back(format!("run {} cancelled", run.id));
        }
        ClientEvent::ChatFailed(error) => {
            app.busy = false;
            app.active_run = None;
            app.loader_started = None;
            if app.cancelling {
                app.cancelling = false;
                app.streaming_text.clear();
            } else {
                let error = friendly_chat_error(error);
                app.last_error = Some(error);
            }
        }
        ClientEvent::OperationFailed(error) => {
            app.last_error = Some(error.clone());
            app.overlay = Some(Overlay::Error(error));
        }
        ClientEvent::Notice(message) => app.activity.push_back(message),
        ClientEvent::Failed(error) => {
            app.busy = false;
            app.loader_started = None;
            app.connected = false;
            app.last_error = Some(error.clone());
            app.overlay = Some(Overlay::Error(error));
        }
    }
}

fn friendly_chat_error(error: String) -> String {
    if error.contains("502") || error.contains("503") || error.contains("504") {
        "The selected model returned a temporary gateway error after automatic retries and configured fallback models. This is not a directory-access problem, and no permission setup is required. If every provider was temporarily unavailable, wait a few seconds and send the request again."
            .to_string()
    } else {
        error
    }
}

async fn chat_response_result(response: reqwest::Response) -> Result<(), String> {
    let status = response.status();
    if status.is_success() {
        return Ok(());
    }
    let payload = response.json::<Value>().await.ok();
    Err(chat_error_message(status, payload.as_ref()))
}

fn chat_error_message(status: reqwest::StatusCode, payload: Option<&Value>) -> String {
    payload
        .and_then(|value| value.pointer("/error/message"))
        .and_then(Value::as_str)
        .filter(|message| !message.trim().is_empty())
        .map_or_else(
            || format!("request failed with HTTP {status}"),
            |message| format!("{message} (HTTP {status})"),
        )
}

fn approval_decision_for_key(key: KeyEvent) -> Option<ApprovalDecision> {
    match key.code {
        KeyCode::Char('y' | 'Y') | KeyCode::Enter => Some(ApprovalDecision::AllowOnce),
        KeyCode::Char('r' | 'R') => Some(ApprovalDecision::AllowRun),
        KeyCode::Char('p' | 'P') => Some(ApprovalDecision::AllowProject),
        KeyCode::Char('A') => Some(ApprovalDecision::AlwaysAllowAll),
        KeyCode::Char('a') if key.modifiers.contains(KeyModifiers::SHIFT) => {
            Some(ApprovalDecision::AlwaysAllowAll)
        }
        KeyCode::Char('a') => Some(ApprovalDecision::AlwaysAllowPattern),
        KeyCode::Char('n' | 'N') | KeyCode::Esc => Some(ApprovalDecision::DenyOnce),
        KeyCode::Char('d' | 'D') => Some(ApprovalDecision::AlwaysDenyPattern),
        _ => None,
    }
}

fn handle_key(
    app: &mut App,
    key: KeyEvent,
    client: &reqwest::Client,
    server: &str,
    tx: &mpsc::UnboundedSender<ClientEvent>,
) {
    if matches!(app.overlay, Some(Overlay::Picker(_))) {
        handle_picker_key(app, key, client, server, tx);
        return;
    }
    if let Some(overlay) = app.overlay.as_mut() {
        match overlay {
            Overlay::Help | Overlay::Error(_) => {
                if matches!(key.code, KeyCode::Esc | KeyCode::Enter)
                    || key.code == KeyCode::Char('q')
                {
                    app.overlay = None;
                }
            }
            Overlay::DeleteConversation(conversation) => match key.code {
                KeyCode::Char('y' | 'Y') | KeyCode::Enter => {
                    let id = conversation.id;
                    app.overlay = None;
                    submit_conversation_delete(client, server, &app.project_root, id, tx);
                }
                KeyCode::Char('n' | 'N') | KeyCode::Esc => app.overlay = None,
                _ => {}
            },
            Overlay::Setup(setup) => {
                handle_setup_key(setup, key, client, server, tx);
            }
            Overlay::Approval(approval) => {
                if matches!(key.code, KeyCode::Char('e' | 'E')) {
                    let text = serde_json::to_string_pretty(&approval.arguments)
                        .unwrap_or_else(|_| approval.arguments.to_string());
                    let editor = PromptEditor {
                        cursor: text.len(),
                        text,
                        ..PromptEditor::default()
                    };
                    app.overlay = Some(Overlay::ApprovalEditor {
                        approval: approval.clone(),
                        editor,
                    });
                    return;
                }
                let decision = approval_decision_for_key(key);
                if let Some(decision) = decision {
                    submit_approval(client, server, approval.id, decision, None, tx);
                }
            }
            Overlay::ApprovalEditor { approval, editor } => {
                if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Enter {
                    match serde_json::from_str::<Value>(&editor.text) {
                        Ok(arguments) => submit_approval(
                            client,
                            server,
                            approval.id,
                            ApprovalDecision::AllowOnce,
                            Some(arguments),
                            tx,
                        ),
                        Err(error) => {
                            app.last_error = Some(format!("invalid edited arguments: {error}"));
                        }
                    }
                } else {
                    match key.code {
                        KeyCode::Esc => {
                            app.overlay = Some(Overlay::Approval(approval.clone()));
                        }
                        KeyCode::Char(value) => editor.insert_char(value),
                        KeyCode::Enter => editor.insert_char('\n'),
                        KeyCode::Backspace => editor.backspace(),
                        KeyCode::Delete => editor.delete(),
                        KeyCode::Left => editor.left(),
                        KeyCode::Right => editor.right(),
                        KeyCode::Up => editor.move_vertical(-1),
                        KeyCode::Down => editor.move_vertical(1),
                        _ => {}
                    }
                }
            }
            Overlay::Picker(_) => {}
        }
        return;
    }

    if key.modifiers.contains(KeyModifiers::CONTROL)
        && key.modifiers.contains(KeyModifiers::SHIFT)
        && matches!(key.code, KeyCode::Char('c' | 'C'))
    {
        let copyable = app
            .editor
            .selected_text()
            .map(str::to_string)
            .or_else(|| latest_copyable_response(&app.messages, &app.streaming_text));
        if let Some(copyable) = copyable {
            if let Err(error) = copy_to_terminal_clipboard(&copyable) {
                app.last_error = Some(format!("copy failed: {error}"));
            } else {
                app.activity
                    .push_back(format!("copied {} bytes", copyable.len()));
            }
        } else {
            app.last_error =
                Some("there is no prompt selection or assistant response to copy".to_string());
        }
        return;
    }

    if key.modifiers.contains(KeyModifiers::CONTROL) {
        match key.code {
            KeyCode::Enter => submit_prompt(app, client, server, tx),
            KeyCode::Char('c') => {
                let now = Instant::now();
                if app.busy {
                    if let Some(run_id) = app.active_run {
                        app.cancelling = true;
                        submit_cancellation(client, server, run_id, tx);
                        app.activity.push_back("cancelling current run".to_string());
                    } else {
                        app.last_error =
                            Some("the run is starting; press Ctrl+C again to quit".into());
                    }
                }
                if app
                    .last_ctrl_c
                    .is_some_and(|last| now.duration_since(last) <= DOUBLE_INTERRUPT)
                    || !app.busy
                {
                    app.should_quit = true;
                } else {
                    app.last_ctrl_c = Some(now);
                }
            }
            KeyCode::Char('n') => {
                app.conversation = None;
                app.messages.clear();
                app.streaming_text.clear();
                app.chat_scroll_offset = 0;
                app.activity.clear();
                app.runtime_trace.clear();
                app.attachments.clear();
                app.selected_agent = "auto".to_string();
                app.automatic_agent = true;
                app.view = View::Chat;
            }
            KeyCode::Char('p' | 'h') => app.overlay = Some(Overlay::Help),
            KeyCode::Char('k') => open_picker(app, PickerKind::Command),
            KeyCode::Char('m') => {
                refresh_models(client, server, app.provider.as_deref(), tx);
            }
            KeyCode::Char('a') => open_picker(app, PickerKind::Agent),
            KeyCode::Char('s') => open_picker(app, PickerKind::Session),
            KeyCode::Char('d') => app.view = View::Changes,
            KeyCode::Char('t') => app.view = View::Terminal,
            KeyCode::Char('l') => app.view = View::Logs,
            KeyCode::Char('z') => app.editor.undo(),
            KeyCode::Char('y') => app.editor.redo(),
            KeyCode::Char('o') => {
                app.tool_details_expanded = !app.tool_details_expanded;
                app.activity.push_back(if app.tool_details_expanded {
                    "tool details expanded".to_string()
                } else {
                    "tool details collapsed".to_string()
                });
            }
            KeyCode::Up => app.editor.history(true),
            KeyCode::Down => app.editor.history(false),
            KeyCode::Home => app.chat_scroll_offset = u16::MAX,
            KeyCode::End => app.chat_scroll_offset = 0,
            KeyCode::Left => app.cycle_view(-1),
            KeyCode::Right => app.cycle_view(1),
            _ => {}
        }
        return;
    }

    let selecting = key.modifiers.contains(KeyModifiers::SHIFT);
    let suggestions = command_suggestions(app);
    if !suggestions.is_empty() {
        match key.code {
            KeyCode::Up => {
                app.suggestion_index = app
                    .suggestion_index
                    .checked_sub(1)
                    .unwrap_or(suggestions.len() - 1);
                return;
            }
            KeyCode::Down => {
                app.suggestion_index = (app.suggestion_index + 1) % suggestions.len();
                return;
            }
            _ => {}
        }
    }
    match key.code {
        KeyCode::Char(value) => {
            app.editor.insert_char(value);
            app.suggestion_index = 0;
            capture_editor_drop(app);
        }
        KeyCode::Enter => app.editor.insert_char('\n'),
        KeyCode::Backspace if app.editor.text.is_empty() && !app.attachments.is_empty() => {
            if let Some(removed) = app.attachments.pop() {
                app.activity
                    .push_back(format!("removed {}", removed.kind.label()));
            }
        }
        KeyCode::Backspace => {
            app.editor.backspace();
            app.suggestion_index = 0;
        }
        KeyCode::Delete => {
            app.editor.delete();
            app.suggestion_index = 0;
        }
        KeyCode::Left => {
            app.editor.prepare_selection(selecting);
            app.editor.left();
        }
        KeyCode::Right => {
            app.editor.prepare_selection(selecting);
            app.editor.right();
        }
        KeyCode::Up if app.editor.text.is_empty() => scroll_chat(app, 3),
        KeyCode::Down if app.editor.text.is_empty() => scroll_chat(app, -3),
        KeyCode::Up => {
            app.editor.prepare_selection(selecting);
            app.editor.move_vertical(-1);
        }
        KeyCode::Down => {
            app.editor.prepare_selection(selecting);
            app.editor.move_vertical(1);
        }
        KeyCode::Tab => complete_editor(app),
        KeyCode::PageUp => {
            scroll_chat(app, 10);
        }
        KeyCode::PageDown => {
            scroll_chat(app, -10);
        }
        KeyCode::Home => {
            app.editor.prepare_selection(selecting);
            app.editor.cursor = app.editor.text[..app.editor.cursor]
                .rfind('\n')
                .map_or(0, |index| index + 1);
        }
        KeyCode::End => {
            app.editor.prepare_selection(selecting);
            app.editor.cursor = app.editor.text[app.editor.cursor..]
                .find('\n')
                .map_or(app.editor.text.len(), |index| app.editor.cursor + index);
        }
        KeyCode::F(1) => app.overlay = Some(Overlay::Help),
        KeyCode::Esc => {
            app.editor.selection_anchor = None;
            app.view = View::Chat;
        }
        _ => {}
    }
}

fn scroll_chat(app: &mut App, lines: i32) {
    if lines >= 0 {
        app.chat_scroll_offset = app
            .chat_scroll_offset
            .saturating_add(u16::try_from(lines).unwrap_or(u16::MAX));
    } else {
        app.chat_scroll_offset = app
            .chat_scroll_offset
            .saturating_sub(u16::try_from(lines.unsigned_abs()).unwrap_or(u16::MAX));
    }
}

fn complete_editor(app: &mut App) {
    let suggestions = command_suggestions(app);
    if !suggestions.is_empty() {
        let selected = app.suggestion_index.min(suggestions.len() - 1);
        let completion = &suggestions[selected].value;
        app.editor.checkpoint();
        app.editor
            .text
            .replace_range(0..app.editor.cursor, completion);
        app.editor.cursor = completion.len();
        if resolve_command(completion).is_some()
            || app
                .custom_commands
                .iter()
                .any(|command| command.name == *completion)
        {
            app.editor.insert_char(' ');
        }
        app.suggestion_index = 0;
        return;
    }
    let start = app.editor.text[..app.editor.cursor]
        .rfind(char::is_whitespace)
        .map_or(0, |index| index + 1);
    let token = &app.editor.text[start..app.editor.cursor];
    let completion = if let Some(query) = token.strip_prefix("@agent:") {
        let query = query.to_ascii_lowercase();
        app.agent_definitions
            .iter()
            .find(|agent| agent.name.to_ascii_lowercase().contains(&query))
            .map(|agent| format!("@agent:{}", agent.name))
    } else if let Some(query) = token.strip_prefix('@') {
        let query = query.to_ascii_lowercase();
        walkdir::WalkDir::new(&app.project_root)
            .follow_links(false)
            .max_depth(8)
            .into_iter()
            .filter_map(std::result::Result::ok)
            .filter(|entry| entry.file_type().is_file())
            .filter_map(|entry| {
                entry
                    .path()
                    .strip_prefix(Path::new(&app.project_root))
                    .ok()
                    .map(|path| path.to_string_lossy().replace('\\', "/"))
            })
            .find(|path| path.to_ascii_lowercase().contains(&query))
            .map(|path| format!("@{path}"))
    } else {
        None
    };
    if let Some(completion) = completion {
        app.editor.checkpoint();
        app.editor
            .text
            .replace_range(start..app.editor.cursor, &completion);
        app.editor.cursor = start + completion.len();
    }
}

fn command_suggestions(app: &App) -> Vec<PickerOption> {
    let query = &app.editor.text[..app.editor.cursor];
    if !query.starts_with('/') {
        return Vec::new();
    }
    if let Some(split) = query.find(char::is_whitespace) {
        let command_name = &query[..split];
        let Some(command) = resolve_command(command_name) else {
            return Vec::new();
        };
        let argument = query[split..].trim_start().to_ascii_lowercase();
        let mut suggestions = command_value_suggestions(app, command.id, command_name)
            .into_iter()
            .filter(|option| {
                option
                    .value
                    .strip_prefix(command_name)
                    .unwrap_or(&option.value)
                    .trim_start()
                    .trim_matches('"')
                    .to_ascii_lowercase()
                    .starts_with(&argument)
            })
            .collect::<Vec<_>>();
        suggestions.sort_by(|left, right| left.value.cmp(&right.value));
        return suggestions;
    }
    let query = query.to_ascii_lowercase();
    let mut suggestions = builtin_commands()
        .into_iter()
        .flat_map(|command| {
            std::iter::once(command.name)
                .chain(command.aliases.iter().copied())
                .map(move |name| PickerOption {
                    value: name.to_string(),
                    label: command.usage.to_string(),
                    auxiliary: Some(command.summary.to_string()),
                })
        })
        .chain(app.custom_commands.iter().map(|command| PickerOption {
            value: command.name.clone(),
            label: command.name.clone(),
            auxiliary: Some(command.description.clone()),
        }))
        .filter(|option| option.value.to_ascii_lowercase().starts_with(&query))
        .collect::<Vec<_>>();
    suggestions.sort_by(|left, right| left.value.cmp(&right.value));
    suggestions.dedup_by(|left, right| left.value == right.value);
    suggestions
}

fn command_value_suggestions(app: &App, id: CommandId, command: &str) -> Vec<PickerOption> {
    let values = match id {
        CommandId::Reasoning => reasoning_levels(app.model.as_deref())
            .into_iter()
            .map(|(value, detail)| (value.to_string(), detail.to_string()))
            .collect(),
        CommandId::Mode => [
            ("auto", "Automatically choose direct, focused, or agentic"),
            ("direct", "Answer without local tools"),
            ("focused", "Use local tools for a bounded task"),
            ("agentic", "Coordinate a broad multi-step task"),
        ]
        .into_iter()
        .map(|(value, detail)| (value.to_string(), detail.to_string()))
        .collect(),
        CommandId::ModelPack => std::iter::once((
            "off".to_string(),
            "Use the selected single model".to_string(),
        ))
        .chain(app.model_packs.iter().map(|descriptor| {
            (
                descriptor.pack.id.clone(),
                if descriptor.available {
                    format!(
                        "{} · {} models · {:?}",
                        descriptor.pack.name,
                        descriptor.pack.members.len(),
                        descriptor.pack.strategy
                    )
                } else {
                    format!(
                        "{} · unavailable: connect {}",
                        descriptor.pack.name,
                        descriptor.missing_providers.join(", ")
                    )
                },
            )
        }))
        .collect(),
        CommandId::Agent => std::iter::once((
            "auto".to_string(),
            "Automatically select the right agent pipeline".to_string(),
        ))
        .chain(
            app.agent_definitions
                .iter()
                .map(|agent| (agent.name.clone(), agent.description.clone())),
        )
        .collect(),
        CommandId::Skill => app
            .skills
            .iter()
            .map(|skill| (skill.name.clone(), skill.description.clone()))
            .collect(),
        CommandId::Disconnect => app
            .providers
            .iter()
            .map(|provider| {
                (
                    provider.id.clone(),
                    provider
                        .default_model
                        .clone()
                        .unwrap_or_else(|| "configured provider".to_string()),
                )
            })
            .collect(),
        CommandId::RemoveDirectory => app
            .workspace_roots
            .iter()
            .map(|path| {
                (
                    path.clone(),
                    "Revoke persistent directory access".to_string(),
                )
            })
            .collect(),
        _ => Vec::new(),
    };
    values
        .into_iter()
        .map(|(value, detail)| {
            let escaped = if value.chars().any(char::is_whitespace) {
                format!("\"{value}\"")
            } else {
                value.clone()
            };
            PickerOption {
                value: format!("{command} {escaped}"),
                label: value,
                auxiliary: Some(detail),
            }
        })
        .collect()
}

fn reasoning_levels(model: Option<&str>) -> Vec<(&'static str, &'static str)> {
    let model = model.unwrap_or_default().to_ascii_lowercase();
    if model.contains("gpt-5.6-sol") {
        return vec![
            ("low", "Fast reasoning for the selected model"),
            ("medium", "Balanced reasoning for the selected model"),
            ("high", "Deeper reasoning for the selected model"),
            ("xhigh", "Very deep reasoning for the selected model"),
            ("max", "Maximum reasoning for the selected model"),
            ("ultra", "Highest reasoning setting for the selected model"),
        ];
    }
    if model.contains("gpt-5-pro") {
        return vec![("high", "The reasoning level supported by this model")];
    }
    if model.contains("gpt-") {
        return vec![
            (
                "none",
                "Disable reasoning when the selected model supports it",
            ),
            ("minimal", "Minimal reasoning latency"),
            ("low", "Fast reasoning"),
            ("medium", "Balanced reasoning"),
            ("high", "Deep reasoning"),
            ("xhigh", "Very deep reasoning when supported"),
        ];
    }
    if model.contains("gemini") {
        let minimal = model.contains("3.6")
            || model.contains("3.5")
            || model.contains("3-flash")
            || model.contains("flash-lite-image");
        let medium = !model.contains("3-pro-preview") && !model.contains("flash-lite-image");
        let mut levels = Vec::new();
        if minimal {
            levels.push(("minimal", "Minimal thinking for this Gemini model"));
        }
        levels.push(("low", "Fast thinking for this Gemini model"));
        if medium {
            levels.push(("medium", "Balanced thinking for this Gemini model"));
        }
        levels.push(("high", "Deep thinking for this Gemini model"));
        return levels;
    }
    vec![
        ("low", "Fast reasoning"),
        ("medium", "Balanced reasoning"),
        ("high", "Deep reasoning"),
    ]
}

#[derive(Debug, Clone, Copy)]
struct ModelTaskRequirements {
    vision: bool,
    tools: bool,
}

fn model_task_requirements(
    prompt: &str,
    attachments: &[PendingAttachment],
    history: &[Message],
    mode: Option<ExecutionMode>,
) -> ModelTaskRequirements {
    let prompt = prompt.to_ascii_lowercase();
    let explicit_tool_mode = matches!(mode, Some(ExecutionMode::Focused | ExecutionMode::Agentic));
    let tool_intent = [
        "build ",
        "build it",
        "code it",
        "code this",
        "code that",
        "create ",
        "write ",
        "edit ",
        "modify ",
        "fix ",
        "implement ",
        "run ",
        "test ",
        "folder",
        "directory",
        "codebase",
        "repository",
        "file",
    ]
    .iter()
    .any(|marker| prompt.contains(marker));
    let inherited_image = is_continuation_request(&prompt)
        && history
            .iter()
            .rev()
            .find(|message| message.role == MessageRole::User)
            .is_some_and(|message| {
                message.content.iter().any(|content| {
                    matches!(
                        content,
                        MessageContent::FileReference {
                            mime_type: Some(mime_type),
                            ..
                        } if mime_type.starts_with("image/")
                    )
                })
            });
    ModelTaskRequirements {
        vision: attachments
            .iter()
            .any(|attachment| attachment.kind == AttachmentKind::Image)
            || inherited_image,
        tools: explicit_tool_mode
            || (mode != Some(ExecutionMode::Direct)
                && (tool_intent
                    || attachments
                        .iter()
                        .any(|attachment| attachment.kind != AttachmentKind::Image))),
    }
}

fn model_matches_task(model: &ModelDescriptor, requirements: ModelTaskRequirements) -> bool {
    model.capabilities.chat
        && (!requirements.vision || model.capabilities.multimodal)
        && (!requirements.tools || model.capabilities.tools)
}

fn model_capability_badges(model: &ModelDescriptor) -> String {
    let mut badges = Vec::new();
    if model.capabilities.multimodal {
        badges.push("vision");
    }
    if model.capabilities.tools {
        badges.push("tools");
    }
    if badges.is_empty() {
        badges.push("chat");
    }
    badges.join(" · ")
}

fn open_picker(app: &mut App, kind: PickerKind) {
    let model_requirements =
        model_task_requirements(&app.editor.text, &app.attachments, &app.messages, app.mode);
    let (title, options): (&'static str, Vec<PickerOption>) = match kind {
        PickerKind::Command => (
            " Command palette ",
            builtin_commands()
                .into_iter()
                .map(|command| PickerOption {
                    value: command.name.to_string(),
                    label: command.usage.to_string(),
                    auxiliary: Some(command.summary.to_string()),
                })
                .chain(app.custom_commands.iter().map(|command| PickerOption {
                    value: command.name.clone(),
                    label: command.name.clone(),
                    auxiliary: Some(command.description.clone()),
                }))
                .collect(),
        ),
        PickerKind::Provider => (
            " Select provider ",
            app.providers
                .iter()
                .map(|provider| PickerOption {
                    value: provider.id.clone(),
                    label: provider.id.clone(),
                    auxiliary: provider.default_model.clone(),
                })
                .collect(),
        ),
        PickerKind::Model => (
            " Select model or pack ",
            app.model_packs
                .iter()
                .filter(|descriptor| descriptor.available)
                .map(|descriptor| PickerOption {
                    value: format!("@pack:{}", descriptor.pack.id),
                    label: format!(
                        "[pack] {} · {} models",
                        descriptor.pack.name,
                        descriptor.pack.members.len()
                    ),
                    auxiliary: Some(descriptor.pack.description.clone()),
                })
                .chain(
                    app.models
                        .iter()
                        .filter(|model| model_matches_task(model, model_requirements))
                        .map(|model| PickerOption {
                            value: model.id.clone(),
                            label: format!(
                                "{}/{}  [{}]",
                                model.provider,
                                model.id,
                                model_capability_badges(model)
                            ),
                            auxiliary: Some(model.provider.clone()),
                        }),
                )
                .collect(),
        ),
        PickerKind::ModelPack => (
            " Select model pack ",
            app.model_packs
                .iter()
                .map(|descriptor| PickerOption {
                    value: descriptor.pack.id.clone(),
                    label: format!(
                        "{} · {} models",
                        descriptor.pack.name,
                        descriptor.pack.members.len()
                    ),
                    auxiliary: Some(if descriptor.available {
                        descriptor.pack.description.clone()
                    } else {
                        format!(
                            "Unavailable · connect {}",
                            descriptor.missing_providers.join(", ")
                        )
                    }),
                })
                .collect(),
        ),
        PickerKind::Agent => (
            " Agent routing ",
            std::iter::once(PickerOption {
                value: "auto".to_string(),
                label: "auto".to_string(),
                auxiliary: Some(
                    "Automatically choose and coordinate the right pipeline".to_string(),
                ),
            })
            .chain(app.agent_definitions.iter().map(|agent| PickerOption {
                value: agent.name.clone(),
                label: agent.name.clone(),
                auxiliary: Some(agent.description.clone()),
            }))
            .collect(),
        ),
        PickerKind::Session => (
            " Restore session ",
            app.conversations
                .iter()
                .map(|conversation| PickerOption {
                    value: conversation.id.to_string(),
                    label: conversation
                        .title
                        .clone()
                        .unwrap_or_else(|| conversation.id.to_string()),
                    auxiliary: Some(conversation.updated_at.format("%Y-%m-%d %H:%M").to_string()),
                })
                .collect(),
        ),
    };
    if options.is_empty() {
        app.overlay = Some(Overlay::Error(format!(
            "no {} choices are available",
            title.trim().to_lowercase()
        )));
    } else {
        app.overlay = Some(Overlay::Picker(PickerState {
            kind,
            title,
            options,
            selected: 0,
            query: String::new(),
        }));
    }
}

fn handle_picker_key(
    app: &mut App,
    key: KeyEvent,
    client: &reqwest::Client,
    server: &str,
    tx: &mpsc::UnboundedSender<ClientEvent>,
) {
    let Some(Overlay::Picker(picker)) = app.overlay.as_mut() else {
        return;
    };
    let matching_options = || {
        let query = picker.query.to_ascii_lowercase();
        picker
            .options
            .iter()
            .filter(|option| {
                query.is_empty()
                    || option.label.to_ascii_lowercase().contains(&query)
                    || option
                        .auxiliary
                        .as_deref()
                        .is_some_and(|value| value.to_ascii_lowercase().contains(&query))
            })
            .count()
    };
    match key.code {
        KeyCode::Delete if picker.kind == PickerKind::Session => {
            let query = picker.query.to_ascii_lowercase();
            let selected = picker
                .options
                .iter()
                .filter(|option| {
                    query.is_empty()
                        || option.label.to_ascii_lowercase().contains(&query)
                        || option
                            .auxiliary
                            .as_deref()
                            .is_some_and(|value| value.to_ascii_lowercase().contains(&query))
                })
                .nth(picker.selected)
                .cloned();
            if let Some(option) = selected
                && let Ok(id) = uuid::Uuid::parse_str(&option.value)
                && let Some(conversation) = app
                    .conversations
                    .iter()
                    .find(|conversation| conversation.id == id)
                    .cloned()
            {
                app.overlay = Some(Overlay::DeleteConversation(conversation));
            }
            return;
        }
        KeyCode::Up => {
            picker.selected = picker.selected.saturating_sub(1);
            return;
        }
        KeyCode::Down => {
            picker.selected = (picker.selected + 1).min(matching_options().saturating_sub(1));
            return;
        }
        KeyCode::Char(value) => {
            picker.query.push(value);
            picker.selected = 0;
            return;
        }
        KeyCode::Backspace => {
            picker.query.pop();
            picker.selected = 0;
            return;
        }
        KeyCode::Esc => {
            app.overlay = None;
            return;
        }
        KeyCode::Enter => {}
        _ => return,
    }
    let kind = picker.kind;
    let query = picker.query.to_ascii_lowercase();
    let Some(option) = picker
        .options
        .iter()
        .filter(|option| {
            query.is_empty()
                || option.label.to_ascii_lowercase().contains(&query)
                || option
                    .auxiliary
                    .as_deref()
                    .is_some_and(|value| value.to_ascii_lowercase().contains(&query))
        })
        .nth(picker.selected)
        .cloned()
    else {
        return;
    };
    app.overlay = None;
    match kind {
        PickerKind::Command => {
            app.editor.checkpoint();
            app.editor.text = option.value;
            if !app.editor.text.ends_with(' ') {
                app.editor.text.push(' ');
            }
            app.editor.cursor = app.editor.text.len();
        }
        PickerKind::Provider => {
            app.provider = Some(option.value);
            app.model = option.auxiliary;
            submit_conversation_selection(app, client, server, tx);
        }
        PickerKind::Model => {
            if let Some(id) = option.value.strip_prefix("@pack:") {
                app.model_pack = Some(id.to_string());
            } else {
                app.model_pack = None;
                app.model = Some(option.value);
                if let Some(provider) = option.auxiliary {
                    app.provider = Some(provider);
                }
            }
            submit_conversation_selection(app, client, server, tx);
        }
        PickerKind::ModelPack => {
            if app
                .model_pack_descriptor(&option.value)
                .is_some_and(|descriptor| descriptor.available)
            {
                app.model_pack = Some(option.value);
                submit_conversation_selection(app, client, server, tx);
            } else {
                app.overlay = Some(Overlay::Error(
                    "that pack needs another connected provider".to_string(),
                ));
            }
        }
        PickerKind::Agent => {
            app.automatic_agent = option.value == "auto";
            app.selected_agent = option.value;
            submit_conversation_selection(app, client, server, tx);
        }
        PickerKind::Session => {
            if let Ok(id) = uuid::Uuid::parse_str(&option.value) {
                spawn_snapshot(client, server, &app.project_root, Some(id), tx);
            }
        }
    }
}

fn handle_setup_key(
    setup: &mut SetupState,
    key: KeyEvent,
    client: &reqwest::Client,
    server: &str,
    tx: &mpsc::UnboundedSender<ClientEvent>,
) {
    if setup.submitting {
        return;
    }
    if key.modifiers.contains(KeyModifiers::CONTROL) {
        match key.code {
            KeyCode::Enter => {
                let template = PROVIDER_TEMPLATES[setup.template].clone();
                let is_local = is_loopback_compatible_url(&setup.base_url);
                if (!is_local && setup.credential.trim().is_empty())
                    || setup.base_url.trim().is_empty()
                {
                    let _ = tx.send(ClientEvent::Failed(
                        "credential and base URL are required; model can be selected after discovery"
                            .to_string(),
                    ));
                    return;
                }
                setup.submitting = true;
                let payload = json!({
                    "id": template.id,
                    "protocol": template.protocol,
                    "family": template.family,
                    "base_url": setup.base_url,
                    "api_key": (setup.credential_mode == CredentialMode::ApiKey && !setup.credential.trim().is_empty())
                        .then_some(setup.credential.clone()),
                    "api_key_env": (setup.credential_mode == CredentialMode::Environment)
                        .then_some(setup.credential.clone()),
                    "default_model": setup.model,
                    "test_connection": !is_local && !setup.model.trim().is_empty()
                });
                let client = client.clone();
                let server = server.to_string();
                let tx = tx.clone();
                tokio::spawn(async move {
                    let result = async {
                        let value: Value = client
                            .post(format!("{server}/v1/providers/connect"))
                            .json(&payload)
                            .send()
                            .await?
                            .error_for_status()?
                            .json()
                            .await?;
                        Ok::<_, reqwest::Error>(value)
                    }
                    .await;
                    match result {
                        Ok(value) => {
                            let provider =
                                value["provider"].as_str().unwrap_or_default().to_string();
                            let _ = tx.send(ClientEvent::ProviderConnected {
                                provider: provider.clone(),
                                model: value["model"].as_str().unwrap_or_default().to_string(),
                                persisted: value["persisted"].as_bool().unwrap_or(false),
                            });
                            let catalog = async {
                                let models = client
                                    .get(format!("{server}/v1/models"))
                                    .query(&[("refresh", "true"), ("provider", provider.as_str())])
                                    .send()
                                    .await?
                                    .error_for_status()?
                                    .json::<ModelPage>()
                                    .await?
                                    .models;
                                let model_packs = client
                                    .get(format!("{server}/v1/model-packs"))
                                    .send()
                                    .await?
                                    .error_for_status()?
                                    .json::<ModelPackPage>()
                                    .await?
                                    .packs;
                                Ok::<_, reqwest::Error>((models, model_packs))
                            }
                            .await;
                            let _ = tx.send(match catalog {
                                Ok((models, model_packs)) => ClientEvent::CatalogRefreshed {
                                    models,
                                    model_packs,
                                    picker: PickerKind::Model,
                                },
                                Err(error) => ClientEvent::OperationFailed(format!(
                                    "provider connected, but model catalog refresh failed: {error}"
                                )),
                            });
                        }
                        Err(error) => {
                            let _ = tx.send(ClientEvent::Failed(error.to_string()));
                        }
                    }
                });
            }
            KeyCode::Char('e' | 'E') => {
                setup.credential_mode = match setup.credential_mode {
                    CredentialMode::ApiKey => CredentialMode::Environment,
                    CredentialMode::Environment => CredentialMode::ApiKey,
                };
                setup.credential = if setup.credential_mode == CredentialMode::Environment {
                    PROVIDER_TEMPLATES[setup.template].key_env.to_string()
                } else {
                    String::new()
                };
            }
            _ => {}
        }
        return;
    }
    match key.code {
        KeyCode::F(2) => {
            setup.select_template((setup.template + 1) % PROVIDER_TEMPLATES.len());
        }
        KeyCode::Right if setup.field == 0 && setup.credential.is_empty() => {
            setup.select_template((setup.template + 1) % PROVIDER_TEMPLATES.len());
        }
        KeyCode::Left if setup.field == 0 && setup.credential.is_empty() => {
            setup.select_template(
                setup
                    .template
                    .checked_sub(1)
                    .unwrap_or(PROVIDER_TEMPLATES.len() - 1),
            );
        }
        KeyCode::Tab | KeyCode::Enter => setup.field = (setup.field + 1) % 3,
        KeyCode::BackTab => setup.field = setup.field.checked_sub(1).unwrap_or(2),
        KeyCode::Backspace => {
            setup.active_value_mut().pop();
        }
        KeyCode::Char(value) => setup.active_value_mut().push(value),
        _ => {}
    }
}

fn is_loopback_compatible_url(base_url: &str) -> bool {
    let base_url = base_url.to_ascii_lowercase();
    ["http://127.0.0.1:", "http://localhost:", "http://[::1]:"]
        .iter()
        .any(|prefix| base_url.starts_with(prefix))
}

fn submit_prompt(
    app: &mut App,
    client: &reqwest::Client,
    server: &str,
    tx: &mpsc::UnboundedSender<ClientEvent>,
) {
    if app.busy {
        app.last_error = Some("a response is already running".to_string());
        return;
    }
    let mut value = app.editor.take();
    if value.trim().is_empty() && app.attachments.is_empty() {
        return;
    }
    if value.trim().is_empty() {
        value = "Analyze the attached files.".to_string();
    }
    if value.trim_start().starts_with('/') {
        let command_name = value
            .split_whitespace()
            .next()
            .unwrap_or_default()
            .to_string();
        if resolve_command(&command_name).is_some() {
            if handle_slash_command(app, value.trim(), client, server, tx) {
                return;
            }
        } else if let Some(command) = app
            .custom_commands
            .iter()
            .find(|command| command.name == command_name)
            .cloned()
        {
            match expand_custom_command(&command, value.trim()) {
                Ok(expanded) => {
                    value = expanded;
                    app.prompt_allowed_tools = command.allowed_tools;
                    if let Some(agent) = command.preferred_agent {
                        app.selected_agent = agent;
                    }
                    if let Some(mode) = command.preferred_mode {
                        app.mode = Some(mode);
                    }
                    if let Some(model) = command.preferred_model {
                        app.model_pack = None;
                        if let Some((provider, model)) = model.split_once('/') {
                            app.provider = Some(provider.to_string());
                            app.model = Some(model.to_string());
                        } else {
                            app.model = Some(model);
                        }
                    }
                    app.activity
                        .push_back(format!("expanded custom command `{command_name}`"));
                }
                Err(error) => {
                    app.overlay = Some(Overlay::Error(error.to_string()));
                    return;
                }
            }
        } else {
            app.overlay = Some(Overlay::Error(format!(
                "`{command_name}` is not a registered product or custom command"
            )));
            return;
        }
    }
    let (Some(provider), Some(model)) = (app.provider.clone(), app.model.clone()) else {
        app.overlay = Some(Overlay::Setup(SetupState::default()));
        app.editor.insert_str(&value);
        return;
    };
    let requirements = model_task_requirements(&value, &app.attachments, &app.messages, app.mode);
    if app.model_pack.is_none()
        && let Some(selected) = app
            .models
            .iter()
            .find(|candidate| candidate.provider == provider && candidate.id == model)
        && !model_matches_task(selected, requirements)
    {
        let reason = if requirements.vision && !selected.capabilities.multimodal {
            "The selected model cannot read images. Choose one of the vision-capable models shown."
        } else if requirements.tools && !selected.capabilities.tools {
            "The selected model cannot use the local tools required for this task. Choose one of the tool-capable models shown."
        } else {
            "The selected model cannot perform this task. Choose one of the compatible models shown."
        };
        app.editor.insert_str(&value);
        app.activity.push_back(reason.to_string());
        open_picker(app, PickerKind::Model);
        return;
    }
    app.busy = true;
    app.connected = true;
    app.last_error = None;
    app.streaming_text.clear();
    app.pending_prompt = Some(value.clone());
    app.loader_started = Some(Instant::now());
    app.chat_scroll_offset = 0;
    app.live_input_tokens = 0;
    app.live_output_tokens = 0;
    app.live_cached_tokens = 0;
    app.runtime_trace.clear();
    app.activity.push_back("submitting prompt".to_string());
    let normalized_prompt = value.to_ascii_lowercase();
    for skill in &app.skills {
        if skill.triggers.iter().any(|trigger| {
            let trigger = trigger.trim().to_ascii_lowercase();
            !trigger.is_empty() && normalized_prompt.contains(&trigger)
        }) && !app.active_skills.contains(&skill.name)
        {
            app.active_skills.push(skill.name.clone());
        }
    }
    app.active_skills.sort();
    app.active_skills.dedup();
    app.last_active_skills.clone_from(&app.active_skills);
    let active_skills = std::mem::take(&mut app.active_skills);
    let allowed_tools = std::mem::take(&mut app.prompt_allowed_tools);
    let attachments = app
        .attachments
        .iter()
        .map(|attachment| attachment.path.clone())
        .collect::<Vec<_>>();
    let payload = json!({
        "conversation_id": app.conversation.as_ref().map(|value| value.id),
        "project_root": app.project_root,
        "message": value,
        "provider": provider,
        "model": model,
        "model_pack": app.model_pack,
        "reasoning_level": app.reasoning_level,
        "mode": app.mode,
        "auto": app.mode.is_none(),
        "agent": (!app.automatic_agent).then_some(app.selected_agent.clone()),
        "skills": active_skills,
        "allowed_tools": allowed_tools,
        "attachments": attachments
    });
    let client = client.clone();
    let server = server.to_string();
    let tx = tx.clone();
    tokio::spawn(async move {
        let result = match client
            .post(format!("{server}/v1/chat"))
            .json(&payload)
            .send()
            .await
        {
            Ok(response) => chat_response_result(response).await,
            Err(error) => Err(error.to_string()),
        };
        match result {
            Ok(()) => {
                let _ = tx.send(ClientEvent::ChatFinished);
            }
            Err(error) => {
                let _ = tx.send(ClientEvent::ChatFailed(error));
            }
        }
    });
}

fn handle_slash_command(
    app: &mut App,
    value: &str,
    client: &reqwest::Client,
    server: &str,
    tx: &mpsc::UnboundedSender<ClientEvent>,
) -> bool {
    let mut parts = value.split_whitespace();
    let command = parts.next().unwrap_or_default();
    let Some(descriptor) = resolve_command(command) else {
        app.overlay = Some(Overlay::Error(format!(
            "`{command}` is not a registered product command"
        )));
        return true;
    };
    match descriptor.id {
        CommandId::Help => app.overlay = Some(Overlay::Help),
        CommandId::New => {
            app.conversation = None;
            app.messages.clear();
            app.activity.clear();
            app.streaming_text.clear();
            app.pending_prompt = None;
            app.loader_started = None;
            app.runtime_trace.clear();
        }
        CommandId::Rename => {
            let title = parts.collect::<Vec<_>>().join(" ");
            if title.is_empty() {
                app.overlay = Some(Overlay::Error(format!("usage: {}", descriptor.usage)));
            } else if let Some(conversation) = &app.conversation {
                submit_session_action(
                    client,
                    server,
                    &app.project_root,
                    conversation.id,
                    SessionAction::Rename(title),
                    tx,
                );
            }
        }
        CommandId::Delete => {
            if let Some(conversation) = &app.conversation {
                app.overlay = Some(Overlay::DeleteConversation(conversation.clone()));
            }
        }
        CommandId::Fork => {
            if let Some(conversation) = &app.conversation {
                submit_session_action(
                    client,
                    server,
                    &app.project_root,
                    conversation.id,
                    SessionAction::Fork,
                    tx,
                );
            }
        }
        CommandId::Export => {
            if let Some(conversation) = &app.conversation {
                export_session(client, server, &app.project_root, conversation.id, tx);
            }
        }
        CommandId::Import => {
            let path = parts.collect::<Vec<_>>().join(" ");
            if path.is_empty() {
                app.overlay = Some(Overlay::Error(format!("usage: {}", descriptor.usage)));
            } else {
                submit_session_import(client, server, &app.project_root, &path, tx);
            }
        }
        CommandId::Compact => {
            if let Some(conversation) = &app.conversation {
                submit_session_compaction(client, server, &app.project_root, conversation.id, tx);
            } else {
                app.overlay = Some(Overlay::Error(
                    "there is no active conversation to compact".to_string(),
                ));
            }
        }
        CommandId::Quit => app.should_quit = true,
        CommandId::Sessions | CommandId::Resume => open_picker(app, PickerKind::Session),
        CommandId::Agents => app.view = View::Agents,
        CommandId::Agent => {
            if let Some(agent) = parts.next() {
                if agent == "auto" {
                    app.selected_agent = "auto".to_string();
                    app.automatic_agent = true;
                } else if app
                    .agent_definitions
                    .iter()
                    .any(|definition| definition.name == agent)
                {
                    app.selected_agent = agent.to_string();
                    app.automatic_agent = false;
                } else {
                    app.overlay = Some(Overlay::Error(format!("unknown agent `{agent}`")));
                }
                if app.overlay.is_none() {
                    submit_conversation_selection(app, client, server, tx);
                }
            } else {
                open_picker(app, PickerKind::Agent);
            }
        }
        CommandId::Tasks => app.view = View::Tasks,
        CommandId::Skills => app.view = View::Skills,
        CommandId::Skill => {
            let Some(name) = parts.next() else {
                app.overlay = Some(Overlay::Error(format!("usage: {}", descriptor.usage)));
                return true;
            };
            if app.skills.iter().any(|skill| skill.name == name) {
                if !app.active_skills.iter().any(|active| active == name) {
                    app.active_skills.push(name.to_string());
                }
                app.activity
                    .push_back(format!("skill `{name}` will activate on the next prompt"));
            } else {
                app.overlay = Some(Overlay::Error(format!("unknown skill `{name}`")));
            }
        }
        CommandId::Tools => app.view = View::Tools,
        CommandId::Mcp => app.view = View::Mcp,
        CommandId::Permissions | CommandId::Settings => app.view = View::Settings,
        CommandId::Stats => app.view = View::Metrics,
        CommandId::Logs => app.view = View::Logs,
        CommandId::Connect => app.overlay = Some(Overlay::Setup(SetupState::default())),
        CommandId::Disconnect => {
            let provider = parts
                .next()
                .map(str::to_string)
                .or_else(|| app.provider.clone());
            if let Some(provider) = provider {
                submit_provider_disconnect(client, server, &app.project_root, &provider, tx);
            } else {
                app.overlay = Some(Overlay::Error(format!("usage: {}", descriptor.usage)));
            }
        }
        CommandId::Providers => open_picker(app, PickerKind::Provider),
        CommandId::Models => refresh_models(client, server, app.provider.as_deref(), tx),
        CommandId::ModelPacks => refresh_model_packs(client, server, tx),
        CommandId::ModelPack => {
            let Some(requested) = parts.next() else {
                refresh_model_packs(client, server, tx);
                return true;
            };
            if requested.eq_ignore_ascii_case("off") {
                app.model_pack = None;
                app.activity
                    .push_back("model pack disabled; using the selected model".to_string());
                submit_conversation_selection(app, client, server, tx);
            } else if let Some(descriptor) = app
                .model_packs
                .iter()
                .find(|descriptor| descriptor.pack.id.eq_ignore_ascii_case(requested))
            {
                if descriptor.available {
                    app.model_pack = Some(descriptor.pack.id.clone());
                    app.activity
                        .push_back(format!("model pack selected: {}", descriptor.pack.name));
                    submit_conversation_selection(app, client, server, tx);
                } else {
                    app.overlay = Some(Overlay::Error(format!(
                        "model pack `{}` needs connected provider(s): {}",
                        descriptor.pack.id,
                        descriptor.missing_providers.join(", ")
                    )));
                }
            } else {
                app.overlay = Some(Overlay::Error(format!(
                    "unknown model pack `{requested}`; use /packs to list available packs"
                )));
            }
        }
        CommandId::Reasoning => {
            app.reasoning_level = parts
                .next()
                .map(str::trim)
                .filter(|level| !level.is_empty())
                .map(str::to_ascii_lowercase);
            submit_conversation_selection(app, client, server, tx);
        }
        CommandId::Mode => {
            app.mode = match parts.next() {
                None | Some("auto") => None,
                Some("direct") => Some(ExecutionMode::Direct),
                Some("focused") => Some(ExecutionMode::Focused),
                Some("agentic") => Some(ExecutionMode::Agentic),
                Some(other) => {
                    app.overlay = Some(Overlay::Error(format!("unknown mode `{other}`")));
                    return true;
                }
            };
            submit_conversation_selection(app, client, server, tx);
        }
        CommandId::Diff => app.view = View::Changes,
        CommandId::Checkpoint => {
            let run_id = app
                .active_run
                .or_else(|| app.messages.iter().rev().find_map(|message| message.run_id));
            if let Some(run_id) = run_id {
                let label = parts.collect::<Vec<_>>().join(" ");
                submit_checkpoint(
                    client,
                    server,
                    run_id,
                    (!label.is_empty()).then_some(label),
                    tx,
                );
            } else {
                app.overlay = Some(Overlay::Error(
                    "run at least one coding turn before creating a checkpoint".to_string(),
                ));
            }
        }
        CommandId::Undo => {
            if let Some(change) = app
                .changes
                .iter()
                .find(|change| change.state == FileChangeState::Applied)
            {
                submit_change_action(client, server, change.id, "undo", tx);
            } else {
                app.overlay = Some(Overlay::Error(
                    "no applied change can be undone".to_string(),
                ));
            }
        }
        CommandId::Redo => {
            if let Some(change) = app
                .changes
                .iter()
                .find(|change| change.state == FileChangeState::Undone)
            {
                submit_change_action(client, server, change.id, "redo", tx);
            } else {
                app.overlay = Some(Overlay::Error("no undone change can be redone".to_string()));
            }
        }
        CommandId::Terminal => app.view = View::Terminal,
        CommandId::Directories => {
            app.view = View::Context;
            app.activity.push_back(if app.workspace_roots.is_empty() {
                "only the project directory is currently available".to_string()
            } else {
                format!(
                    "{} additional director{} available",
                    app.workspace_roots.len(),
                    if app.workspace_roots.len() == 1 {
                        "y"
                    } else {
                        "ies"
                    }
                )
            });
        }
        CommandId::AddDirectory | CommandId::RemoveDirectory => {
            let path = value
                .strip_prefix(command)
                .unwrap_or_default()
                .trim()
                .trim_matches('"');
            if path.is_empty() {
                app.overlay = Some(Overlay::Error(format!("usage: {}", descriptor.usage)));
            } else {
                submit_workspace_root(
                    client,
                    server,
                    &app.project_root,
                    path,
                    descriptor.id == CommandId::AddDirectory,
                    tx,
                );
            }
        }
    }
    true
}

fn spawn_snapshot(
    client: &reqwest::Client,
    server: &str,
    project_root: &str,
    preferred_conversation: Option<uuid::Uuid>,
    tx: &mpsc::UnboundedSender<ClientEvent>,
) {
    let client = client.clone();
    let server = server.to_string();
    let project_root = project_root.to_string();
    let tx = tx.clone();
    tokio::spawn(async move {
        let result = load_snapshot(&client, &server, &project_root, preferred_conversation).await;
        let _ = tx.send(match result {
            Ok(snapshot) => ClientEvent::Snapshot(snapshot),
            Err(error) => ClientEvent::Failed(error.to_string()),
        });
    });
}

fn submit_provider_disconnect(
    client: &reqwest::Client,
    server: &str,
    project_root: &str,
    provider: &str,
    tx: &mpsc::UnboundedSender<ClientEvent>,
) {
    let client = client.clone();
    let server = server.to_string();
    let project_root = project_root.to_string();
    let provider = provider.to_string();
    let tx = tx.clone();
    tokio::spawn(async move {
        let result = async {
            client
                .delete(format!("{server}/v1/providers/{provider}"))
                .send()
                .await?
                .error_for_status()?;
            load_snapshot(&client, &server, &project_root, None).await
        }
        .await;
        let _ = tx.send(match result {
            Ok(snapshot) => ClientEvent::Snapshot(snapshot),
            Err(error) => ClientEvent::Failed(format!("provider disconnect failed: {error}")),
        });
    });
}

fn refresh_models(
    client: &reqwest::Client,
    server: &str,
    provider: Option<&str>,
    tx: &mpsc::UnboundedSender<ClientEvent>,
) {
    refresh_catalog(client, server, provider, PickerKind::Model, tx);
}

fn refresh_model_packs(
    client: &reqwest::Client,
    server: &str,
    tx: &mpsc::UnboundedSender<ClientEvent>,
) {
    refresh_catalog(client, server, None, PickerKind::ModelPack, tx);
}

fn refresh_catalog(
    client: &reqwest::Client,
    server: &str,
    provider: Option<&str>,
    picker: PickerKind,
    tx: &mpsc::UnboundedSender<ClientEvent>,
) {
    let client = client.clone();
    let server = server.to_string();
    let provider = provider.map(str::to_string);
    let tx = tx.clone();
    tokio::spawn(async move {
        let result = async {
            let mut request = client
                .get(format!("{server}/v1/models"))
                .query(&[("refresh", "true")]);
            if let Some(provider) = provider.as_deref() {
                request = request.query(&[("provider", provider)]);
            }
            let models = request
                .send()
                .await?
                .error_for_status()?
                .json::<ModelPage>()
                .await?
                .models;
            let model_packs = client
                .get(format!("{server}/v1/model-packs"))
                .send()
                .await?
                .error_for_status()?
                .json::<ModelPackPage>()
                .await?
                .packs;
            Ok::<_, reqwest::Error>((models, model_packs))
        }
        .await;
        let _ = tx.send(match result {
            Ok((models, model_packs)) => ClientEvent::CatalogRefreshed {
                models,
                model_packs,
                picker,
            },
            Err(error) => {
                ClientEvent::OperationFailed(format!("model catalog refresh failed: {error}"))
            }
        });
    });
}

fn submit_conversation_selection(
    app: &App,
    client: &reqwest::Client,
    server: &str,
    tx: &mpsc::UnboundedSender<ClientEvent>,
) {
    let Some(conversation_id) = app
        .conversation
        .as_ref()
        .map(|conversation| conversation.id)
    else {
        return;
    };
    let payload = json!({
        "provider": app.provider,
        "model": app.model,
        "model_pack": app.model_pack,
        "reasoning_level": app.reasoning_level,
        "mode": app.mode,
        "agent": (!app.automatic_agent).then_some(app.selected_agent.clone())
    });
    let client = client.clone();
    let server = server.to_string();
    let tx = tx.clone();
    tokio::spawn(async move {
        let result = async {
            client
                .post(format!(
                    "{server}/v1/conversations/{conversation_id}/selection"
                ))
                .json(&payload)
                .send()
                .await?
                .error_for_status()?
                .json::<Conversation>()
                .await
        }
        .await;
        let _ = tx.send(match result {
            Ok(conversation) => ClientEvent::SelectionUpdated(conversation),
            Err(error) => ClientEvent::OperationFailed(format!("selection update failed: {error}")),
        });
    });
}

async fn load_snapshot(
    client: &reqwest::Client,
    server: &str,
    project_root: &str,
    preferred_conversation: Option<uuid::Uuid>,
) -> Result<Snapshot> {
    let event_cursor = client
        .get(format!("{server}/v1/events/cursor"))
        .send()
        .await?
        .error_for_status()?
        .json::<Value>()
        .await?
        .get("after")
        .and_then(Value::as_i64)
        .unwrap_or_default();
    let providers = client
        .get(format!("{server}/v1/providers"))
        .send()
        .await?
        .error_for_status()?
        .json::<ProviderPage>()
        .await?
        .providers;
    let models = client
        .get(format!("{server}/v1/models"))
        .query(&[("refresh", "true")])
        .send()
        .await?
        .error_for_status()?
        .json::<ModelPage>()
        .await?
        .models;
    let model_packs = client
        .get(format!("{server}/v1/model-packs"))
        .send()
        .await?
        .error_for_status()?
        .json::<ModelPackPage>()
        .await?
        .packs;
    let role_policies = client
        .get(format!("{server}/v1/routing-policies"))
        .send()
        .await?
        .error_for_status()?
        .json::<RoutingPolicyPage>()
        .await?
        .roles;
    let conversations = client
        .get(format!("{server}/v1/conversations"))
        .query(&[("project_root", project_root)])
        .send()
        .await?
        .error_for_status()?
        .json::<Vec<Conversation>>()
        .await?;
    let conversation = preferred_conversation
        .and_then(|id| conversations.iter().find(|value| value.id == id).cloned())
        .or_else(|| conversations.first().cloned());
    let messages = if let Some(conversation) = &conversation {
        client
            .get(format!(
                "{server}/v1/conversations/{}/messages",
                conversation.id
            ))
            .send()
            .await?
            .error_for_status()?
            .json::<Vec<Message>>()
            .await?
    } else {
        Vec::new()
    };
    let agents = client
        .get(format!("{server}/v1/agents"))
        .send()
        .await?
        .error_for_status()?
        .json::<Vec<Agent>>()
        .await?;
    let agent_definitions = client
        .get(format!("{server}/v1/agent-definitions"))
        .query(&[("project_root", project_root)])
        .send()
        .await?
        .error_for_status()?
        .json::<Vec<AgentDefinition>>()
        .await?;
    let tasks = client
        .get(format!("{server}/v1/tasks"))
        .send()
        .await?
        .error_for_status()?
        .json::<Vec<Task>>()
        .await?;
    let skills = client
        .get(format!("{server}/v1/skills"))
        .send()
        .await?
        .error_for_status()?
        .json::<Value>()
        .await?
        .get("skills")
        .cloned()
        .map(serde_json::from_value)
        .transpose()?
        .unwrap_or_default();
    let custom_commands = client
        .get(format!("{server}/v1/commands/custom"))
        .query(&[("project_root", project_root)])
        .send()
        .await?
        .error_for_status()?
        .json::<Value>()
        .await?
        .get("commands")
        .cloned()
        .map(serde_json::from_value)
        .transpose()?
        .unwrap_or_default();
    let mcp_servers = client
        .get(format!("{server}/v1/mcp"))
        .send()
        .await?
        .error_for_status()?
        .json::<Value>()
        .await?
        .get("servers")
        .cloned()
        .map(serde_json::from_value)
        .transpose()?
        .unwrap_or_default();
    let tools = client
        .get(format!("{server}/v1/tools"))
        .send()
        .await?
        .error_for_status()?
        .json::<Value>()
        .await?
        .get("tools")
        .cloned()
        .map(serde_json::from_value)
        .transpose()?
        .unwrap_or_default();
    let metrics = client
        .get(format!("{server}/v1/metrics"))
        .send()
        .await?
        .error_for_status()?
        .json::<PerformanceSnapshot>()
        .await?;
    let routing_benchmarks = client
        .get(format!("{server}/v1/routing-benchmarks/aggregate"))
        .send()
        .await?
        .error_for_status()?
        .json::<Vec<RoutingBenchmarkAggregate>>()
        .await?;
    let changes = client
        .get(format!("{server}/v1/changes"))
        .send()
        .await?
        .error_for_status()?
        .json::<Vec<FileChange>>()
        .await?;
    let pending_approvals = client
        .get(format!("{server}/v1/approvals"))
        .query(&[("pending", true)])
        .send()
        .await?
        .error_for_status()?
        .json::<Vec<Approval>>()
        .await?;
    let permissions = client
        .get(format!("{server}/v1/permissions"))
        .send()
        .await?
        .error_for_status()?
        .json::<Vec<PermissionRule>>()
        .await?;
    let workspace_roots = client
        .get(format!("{server}/v1/workspace/roots"))
        .query(&[("project_root", project_root)])
        .send()
        .await?
        .error_for_status()?
        .json::<Value>()
        .await?
        .get("roots")
        .cloned()
        .map(serde_json::from_value)
        .transpose()?
        .unwrap_or_default();
    Ok(Snapshot {
        providers,
        models,
        model_packs,
        role_policies,
        conversations,
        conversation,
        messages,
        agents,
        tasks,
        skills,
        custom_commands,
        mcp_servers,
        tools,
        metrics,
        routing_benchmarks,
        event_cursor,
        pending_approvals,
        permissions,
        changes,
        agent_definitions,
        workspace_roots,
    })
}

fn submit_workspace_root(
    client: &reqwest::Client,
    server: &str,
    project_root: &str,
    path: &str,
    add: bool,
    tx: &mpsc::UnboundedSender<ClientEvent>,
) {
    let client = client.clone();
    let server = server.to_string();
    let project_root = project_root.to_string();
    let path = path.to_string();
    let tx = tx.clone();
    tokio::spawn(async move {
        let result = if add {
            client
                .post(format!("{server}/v1/workspace/roots"))
                .json(&json!({"project_root": project_root, "path": path}))
                .send()
                .await
        } else {
            client
                .delete(format!("{server}/v1/workspace/roots"))
                .query(&[
                    ("project_root", project_root.as_str()),
                    ("path", path.as_str()),
                ])
                .send()
                .await
        };
        let event = match result.and_then(reqwest::Response::error_for_status) {
            Ok(_) => match load_snapshot(&client, &server, &project_root, None).await {
                Ok(snapshot) => {
                    let _ = tx.send(ClientEvent::Notice(if add {
                        format!("directory access granted: {path}")
                    } else {
                        format!("directory access revoked: {path}")
                    }));
                    ClientEvent::Snapshot(snapshot)
                }
                Err(error) => ClientEvent::OperationFailed(error.to_string()),
            },
            Err(error) => {
                ClientEvent::OperationFailed(format!("directory access update failed: {error}"))
            }
        };
        let _ = tx.send(event);
    });
}

fn submit_approval(
    client: &reqwest::Client,
    server: &str,
    approval_id: uuid::Uuid,
    decision: ApprovalDecision,
    edited_arguments: Option<Value>,
    tx: &mpsc::UnboundedSender<ClientEvent>,
) {
    let client = client.clone();
    let server = server.to_string();
    let tx = tx.clone();
    tokio::spawn(async move {
        let result = async {
            client
                .post(format!("{server}/v1/approvals/{approval_id}/decision"))
                .json(&json!({
                    "decision": decision,
                    "edited_arguments": edited_arguments
                }))
                .send()
                .await?
                .error_for_status()?
                .json::<Approval>()
                .await
        }
        .await;
        let _ = tx.send(match result {
            Ok(approval) => ClientEvent::ApprovalDecided(approval),
            Err(error) => ClientEvent::Failed(format!("approval decision: {error}")),
        });
    });
}

fn submit_change_action(
    client: &reqwest::Client,
    server: &str,
    change_id: uuid::Uuid,
    action: &'static str,
    tx: &mpsc::UnboundedSender<ClientEvent>,
) {
    let client = client.clone();
    let server = server.to_string();
    let tx = tx.clone();
    tokio::spawn(async move {
        let result = async {
            client
                .post(format!("{server}/v1/changes/{change_id}/{action}"))
                .send()
                .await?
                .error_for_status()?
                .json::<FileChange>()
                .await
        }
        .await;
        let _ = tx.send(match result {
            Ok(change) => ClientEvent::ChangeUpdated(change),
            Err(error) => ClientEvent::Failed(format!("{action} failed: {error}")),
        });
    });
}

fn submit_checkpoint(
    client: &reqwest::Client,
    server: &str,
    run_id: uuid::Uuid,
    label: Option<String>,
    tx: &mpsc::UnboundedSender<ClientEvent>,
) {
    let client = client.clone();
    let server = server.to_string();
    let tx = tx.clone();
    tokio::spawn(async move {
        let result = async {
            client
                .post(format!("{server}/v1/checkpoints"))
                .json(&json!({"run_id": run_id, "label": label}))
                .send()
                .await?
                .error_for_status()?
                .json::<Value>()
                .await
        }
        .await;
        let _ = tx.send(match result {
            Ok(checkpoint) => ClientEvent::Notice(format!(
                "checkpoint `{}` created",
                checkpoint["label"].as_str().unwrap_or("Manual checkpoint")
            )),
            Err(error) => {
                ClientEvent::OperationFailed(format!("checkpoint creation failed: {error}"))
            }
        });
    });
}

fn submit_cancellation(
    client: &reqwest::Client,
    server: &str,
    run_id: uuid::Uuid,
    tx: &mpsc::UnboundedSender<ClientEvent>,
) {
    let client = client.clone();
    let server = server.to_string();
    let tx = tx.clone();
    tokio::spawn(async move {
        let result = async {
            client
                .post(format!("{server}/v1/runs/{run_id}/cancel"))
                .send()
                .await?
                .error_for_status()?
                .json::<Run>()
                .await
        }
        .await;
        let _ = tx.send(match result {
            Ok(run) => ClientEvent::RunCancelled(run),
            Err(error) => ClientEvent::Failed(format!("cancel failed: {error}")),
        });
    });
}

enum SessionAction {
    Rename(String),
    Fork,
}

fn submit_conversation_delete(
    client: &reqwest::Client,
    server: &str,
    project_root: &str,
    conversation_id: uuid::Uuid,
    tx: &mpsc::UnboundedSender<ClientEvent>,
) {
    let client = client.clone();
    let server = server.to_string();
    let project_root = project_root.to_string();
    let tx = tx.clone();
    tokio::spawn(async move {
        let result = async {
            client
                .delete(format!("{server}/v1/conversations/{conversation_id}"))
                .send()
                .await?
                .error_for_status()?;
            load_snapshot(&client, &server, &project_root, None).await
        }
        .await;
        let _ = tx.send(match result {
            Ok(snapshot) => ClientEvent::Snapshot(snapshot),
            Err(error) => ClientEvent::Failed(format!("conversation deletion failed: {error}")),
        });
    });
}

fn submit_session_action(
    client: &reqwest::Client,
    server: &str,
    project_root: &str,
    conversation_id: uuid::Uuid,
    action: SessionAction,
    tx: &mpsc::UnboundedSender<ClientEvent>,
) {
    let client = client.clone();
    let server = server.to_string();
    let project_root = project_root.to_string();
    let tx = tx.clone();
    tokio::spawn(async move {
        let result = async {
            let (url, body, preferred) = match action {
                SessionAction::Rename(title) => (
                    format!("{server}/v1/conversations/{conversation_id}/rename"),
                    json!({"title": title}),
                    Some(conversation_id),
                ),
                SessionAction::Fork => (
                    format!("{server}/v1/conversations/{conversation_id}/fork"),
                    json!({}),
                    None,
                ),
            };
            let conversation = client
                .post(url)
                .json(&body)
                .send()
                .await?
                .error_for_status()?
                .json::<Conversation>()
                .await?;
            let preferred = preferred.or(Some(conversation.id));
            load_snapshot(&client, &server, &project_root, preferred).await
        }
        .await;
        let _ = tx.send(match result {
            Ok(snapshot) => ClientEvent::Snapshot(snapshot),
            Err(error) => ClientEvent::Failed(format!("session action failed: {error}")),
        });
    });
}

fn export_session(
    client: &reqwest::Client,
    server: &str,
    project_root: &str,
    conversation_id: uuid::Uuid,
    tx: &mpsc::UnboundedSender<ClientEvent>,
) {
    let client = client.clone();
    let server = server.to_string();
    let project_root = project_root.to_string();
    let tx = tx.clone();
    tokio::spawn(async move {
        let result = async {
            let response = client
                .get(format!(
                    "{server}/v1/conversations/{conversation_id}/export"
                ))
                .send()
                .await?
                .error_for_status()?
                .json::<Value>()
                .await?;
            let directory = Path::new(&project_root).join(".opensource").join("exports");
            tokio::fs::create_dir_all(&directory).await?;
            let path = directory.join(format!("{conversation_id}.md"));
            tokio::fs::write(&path, response["markdown"].as_str().unwrap_or_default()).await?;
            Ok::<_, anyhow::Error>(path)
        }
        .await;
        let _ = tx.send(match result {
            Ok(path) => ClientEvent::Notice(format!("exported session to {}", path.display())),
            Err(error) => ClientEvent::Failed(format!("session export failed: {error}")),
        });
    });
}

fn submit_session_import(
    client: &reqwest::Client,
    server: &str,
    project_root: &str,
    path: &str,
    tx: &mpsc::UnboundedSender<ClientEvent>,
) {
    let client = client.clone();
    let server = server.to_string();
    let project_root = project_root.to_string();
    let path = Path::new(path).to_path_buf();
    let tx = tx.clone();
    tokio::spawn(async move {
        let result = async {
            let content = tokio::fs::read_to_string(&path).await?;
            let value: Value = serde_json::from_str(&content)?;
            let document = value.get("json").cloned().unwrap_or(value);
            let conversation: Conversation = client
                .post(format!("{server}/v1/conversations/import"))
                .json(&json!({
                    "project_root": project_root.clone(),
                    "document": document
                }))
                .send()
                .await?
                .error_for_status()?
                .json()
                .await?;
            load_snapshot(&client, &server, &project_root, Some(conversation.id)).await
        }
        .await;
        let _ = tx.send(match result {
            Ok(snapshot) => ClientEvent::Snapshot(snapshot),
            Err(error) => ClientEvent::Failed(format!("session import failed: {error}")),
        });
    });
}

fn submit_session_compaction(
    client: &reqwest::Client,
    server: &str,
    project_root: &str,
    conversation_id: uuid::Uuid,
    tx: &mpsc::UnboundedSender<ClientEvent>,
) {
    let client = client.clone();
    let server = server.to_string();
    let project_root = project_root.to_string();
    let tx = tx.clone();
    tokio::spawn(async move {
        let result = async {
            client
                .post(format!(
                    "{server}/v1/conversations/{conversation_id}/compact"
                ))
                .json(&json!({}))
                .send()
                .await?
                .error_for_status()?;
            load_snapshot(&client, &server, &project_root, Some(conversation_id)).await
        }
        .await;
        let _ = tx.send(match result {
            Ok(snapshot) => ClientEvent::Snapshot(snapshot),
            Err(error) => ClientEvent::Failed(format!("conversation compaction failed: {error}")),
        });
    });
}

fn spawn_event_stream(
    client: reqwest::Client,
    server: String,
    initial_cursor: i64,
    tx: mpsc::UnboundedSender<ClientEvent>,
) {
    tokio::spawn(async move {
        let mut after = initial_cursor;
        loop {
            let result = consume_event_stream(&client, &server, after, &tx).await;
            match result {
                Ok(cursor) => after = cursor,
                Err(error) => {
                    let _ = tx.send(ClientEvent::Notice(format!(
                        "event stream disconnected; reconnecting: {error}"
                    )));
                    tokio::time::sleep(Duration::from_secs(1)).await;
                }
            }
        }
    });
}

async fn consume_event_stream(
    client: &reqwest::Client,
    server: &str,
    after: i64,
    tx: &mpsc::UnboundedSender<ClientEvent>,
) -> Result<i64> {
    let response = client
        .get(format!("{server}/v1/events/stream"))
        .query(&[("after", after)])
        .send()
        .await?
        .error_for_status()?;
    let mut stream = response.bytes_stream();
    let mut buffer = String::new();
    let mut cursor = after;
    while let Some(chunk) = stream.next().await {
        buffer.push_str(&String::from_utf8_lossy(&chunk?));
        buffer = buffer.replace("\r\n", "\n");
        while let Some(index) = buffer.find("\n\n") {
            let frame = buffer[..index].to_string();
            buffer.drain(..index + 2);
            let data = frame
                .lines()
                .filter_map(|line| line.strip_prefix("data:").map(str::trim_start))
                .collect::<Vec<_>>()
                .join("\n");
            if data.is_empty() {
                continue;
            }
            let event: Event = serde_json::from_str(&data)
                .with_context(|| format!("invalid event frame: {data}"))?;
            cursor = event.id;
            let _ = tx.send(ClientEvent::Domain(event));
        }
    }
    Ok(cursor)
}

fn render(frame: &mut ratatui::Frame<'_>, app: &App) {
    let area = frame.area();
    if is_welcome_state(app) {
        render_welcome(frame, area, app);
        if app.overlay.is_none() {
            let editor = welcome_editor_area(area);
            render_command_suggestions(frame, editor, app);
        }
        if let Some(overlay) = &app.overlay {
            render_overlay(frame, area, overlay);
        }
        return;
    }
    let compact = area.height < 21;
    let sections = Layout::default()
        .direction(Direction::Vertical)
        .constraints(if compact {
            [
                Constraint::Length(2),
                Constraint::Min(4),
                Constraint::Length(5),
                Constraint::Length(1),
            ]
        } else {
            [
                Constraint::Length(2),
                Constraint::Min(8),
                Constraint::Length(6),
                Constraint::Length(1),
            ]
        })
        .split(area);
    render_header(frame, sections[0], app);
    let content = sections[1];
    let editor = sections[2];
    render_view(frame, content, app);
    render_editor(frame, editor, app);
    render_footer(frame, sections[3], app);
    if app.overlay.is_none() {
        render_command_suggestions(frame, editor, app);
    }
    if let Some(overlay) = &app.overlay {
        render_overlay(frame, area, overlay);
    }
}

fn is_welcome_state(app: &App) -> bool {
    app.view == View::Chat
        && app.messages.is_empty()
        && app.pending_prompt.is_none()
        && app.streaming_text.is_empty()
        && app.last_error.is_none()
        && !app.busy
}

fn welcome_editor_area(area: Rect) -> Rect {
    let width = area.width.saturating_sub(4).min(72);
    let height = 6.min(area.height.saturating_sub(2));
    Rect::new(
        area.x.saturating_add(area.width.saturating_sub(width) / 2),
        area.y
            .saturating_add(area.height / 2)
            .saturating_sub(if area.height < 18 { 1 } else { 2 }),
        width,
        height,
    )
}

fn render_welcome(frame: &mut ratatui::Frame<'_>, area: Rect, app: &App) {
    let editor = welcome_editor_area(area);
    let logo_y = editor
        .y
        .saturating_sub(if area.height < 18 { 2 } else { 4 });
    let logo_area = Rect::new(area.x, logo_y, area.width, 3);
    let logo = Text::from(vec![
        Line::from(vec![
            Span::styled("◆  ", Style::default().fg(PRIMARY_ACCENT)),
            Span::styled(
                PRODUCT_NAME,
                Style::default()
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD),
            ),
        ]),
        Line::styled(
            "A quiet terminal agent for real work",
            Style::default().fg(Color::DarkGray),
        ),
    ]);
    frame.render_widget(Paragraph::new(logo).alignment(Alignment::Center), logo_area);

    render_editor_with_mode(frame, editor, app, true);

    let details_y = editor.bottom();
    if details_y < area.bottom() {
        let details = Rect::new(editor.x, details_y, editor.width, 1);
        frame.render_widget(Paragraph::new(shortcut_line(editor.width)), details);
    }
    if area.height >= 24 && details_y.saturating_add(4) < area.bottom() {
        let tip = Rect::new(editor.x, details_y.saturating_add(4), editor.width, 1);
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(
                    "●  Tip  ",
                    Style::default()
                        .fg(MENU_ACCENT)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    "Drop files, folders, images, audio, or video directly into the prompt",
                    Style::default().fg(Color::DarkGray),
                ),
            ])),
            tip,
        );
    }
    if area.height > 1 {
        let footer = Rect::new(area.x, area.bottom().saturating_sub(1), area.width, 1);
        let columns = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(70), Constraint::Percentage(30)])
            .split(footer);
        frame.render_widget(
            Paragraph::new(app.project_root.as_str()).style(Style::default().fg(Color::DarkGray)),
            columns[0],
        );
        frame.render_widget(
            Paragraph::new(env!("CARGO_PKG_VERSION"))
                .style(Style::default().fg(Color::DarkGray))
                .alignment(Alignment::Right),
            columns[1],
        );
    }
}

fn render_command_suggestions(frame: &mut ratatui::Frame<'_>, editor_area: Rect, app: &App) {
    let suggestions = command_suggestions(app);
    if suggestions.is_empty() {
        return;
    }
    let visible = suggestions.len().min(10);
    let height = u16::try_from(visible).unwrap_or(10);
    let width = editor_area.width;
    let area = Rect::new(
        editor_area.x,
        editor_area.y.saturating_sub(height),
        width,
        height,
    );
    let selected = app.suggestion_index.min(suggestions.len() - 1);
    let offset = selected.saturating_sub(visible.saturating_sub(1));
    let items = suggestions
        .iter()
        .skip(offset)
        .take(visible)
        .map(|option| {
            let detail = option.auxiliary.as_deref().unwrap_or_default();
            ListItem::new(Line::from(vec![
                Span::styled(
                    format!(" {:<18}", option.value),
                    Style::default()
                        .fg(Color::White)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(detail.to_string(), Style::default().fg(Color::Gray)),
            ]))
        })
        .collect::<Vec<_>>();
    let mut state = ListState::default().with_selected(Some(selected - offset));
    frame.render_widget(Clear, area);
    frame.render_stateful_widget(
        List::new(items)
            .style(Style::default().bg(PANEL_BG))
            .highlight_style(
                Style::default()
                    .fg(Color::Black)
                    .bg(MENU_ACCENT)
                    .add_modifier(Modifier::BOLD),
            ),
        area,
        &mut state,
    );
}

fn render_header(frame: &mut ratatui::Frame<'_>, area: Rect, app: &App) {
    let provider = app.provider.as_deref().unwrap_or("not connected");
    let model = selected_model_label(app);
    let session = app
        .conversation
        .as_ref()
        .and_then(|value| value.title.as_deref())
        .unwrap_or("new session");
    let (status_mark, status_text) = if app.busy {
        ("●", "working")
    } else if app.connected {
        ("●", "ready")
    } else {
        ("○", "offline")
    };
    frame.render_widget(
        Block::default()
            .borders(Borders::BOTTOM)
            .border_style(Style::default().fg(Color::DarkGray)),
        area,
    );
    let inner = Rect::new(
        area.x.saturating_add(1),
        area.y,
        area.width.saturating_sub(2),
        area.height.saturating_sub(1),
    );
    let columns = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(58), Constraint::Percentage(42)])
        .split(inner);
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(
                "◆",
                Style::default()
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!("  {PRODUCT_NAME}"),
                Style::default()
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!("  /  {}", app.view.label().to_ascii_lowercase()),
                Style::default().fg(Color::DarkGray),
            ),
            Span::styled(format!("  {session}"), Style::default().fg(Color::Gray)),
        ])),
        columns[0],
    );
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(
                format!("{status_mark} {status_text}"),
                Style::default().fg(if app.connected {
                    Color::White
                } else {
                    Color::DarkGray
                }),
            ),
            Span::styled(
                if app.model_pack.is_some() {
                    format!("   pack/{model}")
                } else {
                    format!("   {provider}/{model}")
                },
                Style::default().fg(Color::DarkGray),
            ),
        ]))
        .alignment(Alignment::Right),
        columns[1],
    );
}

fn render_view(frame: &mut ratatui::Frame<'_>, area: Rect, app: &App) {
    match app.view {
        View::Chat => render_chat(frame, area, app),
        View::Changes => render_changes(frame, area, app),
        View::Terminal => render_terminal(frame, area, app),
        View::Agents => render_agents(frame, area, app),
        View::Tasks => render_tasks(frame, area, app),
        View::Sessions => render_sessions(frame, area, app),
        View::Context => render_context(frame, area, app),
        View::Skills => render_skills(frame, area, app),
        View::Tools => render_tools(frame, area, app),
        View::Mcp => render_mcp_connections(frame, area, app),
        View::Metrics => render_metrics(frame, area, app),
        View::Logs => render_named_list(
            frame,
            area,
            " Live activity ",
            app.activity.iter().cloned().collect(),
        ),
        View::Settings => render_settings(frame, area, app),
        View::Plugins => render_extensions(frame, area, app),
    }
}

fn render_mcp_connections(frame: &mut ratatui::Frame<'_>, area: Rect, app: &App) {
    let mut lines = vec![
        Line::styled(
            "Connect services and tool servers by prompting the agent.",
            Style::default().fg(Color::Gray),
        ),
        Line::styled(
            "Example: Connect GitHub using the GITHUB_PAT environment variable.",
            Style::default().fg(Color::DarkGray),
        ),
        Line::raw(""),
    ];
    if app.mcp_servers.is_empty() {
        lines.push(Line::styled(
            "No MCP connections yet.",
            Style::default().fg(Color::DarkGray),
        ));
    } else {
        lines.extend(app.mcp_servers.iter().map(|server| {
            Line::from(vec![
                Span::styled(
                    if server.enabled { "●" } else { "○" },
                    Style::default().fg(if server.enabled {
                        Color::White
                    } else {
                        Color::DarkGray
                    }),
                ),
                Span::styled(
                    format!("  {}", server.name),
                    Style::default().add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    format!("  {:?}", server.transport),
                    Style::default().fg(Color::DarkGray),
                ),
            ])
        }));
    }
    frame.render_widget(
        Paragraph::new(lines).block(
            Block::default()
                .title(" Connections ")
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::DarkGray)),
        ),
        area,
    );
}

fn render_extensions(frame: &mut ratatui::Frame<'_>, area: Rect, app: &App) {
    let installed_skills = app
        .skills
        .iter()
        .filter(|skill| {
            !matches!(
                skill.name.as_str(),
                "focused-validation" | "repository-map" | "security-review"
            )
        })
        .count();
    frame.render_widget(
        Paragraph::new(vec![
            Line::styled(
                "Extend the agent without rebuilding the application.",
                Style::default()
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD),
            ),
            Line::raw(""),
            Line::from(vec![
                Span::styled(
                    installed_skills.to_string(),
                    Style::default().add_modifier(Modifier::BOLD),
                ),
                Span::styled(" installed skills", Style::default().fg(Color::Gray)),
            ]),
            Line::from(vec![
                Span::styled(
                    app.mcp_servers.len().to_string(),
                    Style::default().add_modifier(Modifier::BOLD),
                ),
                Span::styled(" connected tool servers", Style::default().fg(Color::Gray)),
            ]),
            Line::raw(""),
            Line::styled(
                "Try: Install the skill from https://github.com/owner/repo",
                Style::default().fg(Color::DarkGray),
            ),
            Line::styled(
                "Try: Connect GitHub using GITHUB_PAT",
                Style::default().fg(Color::DarkGray),
            ),
        ])
        .block(
            Block::default()
                .title(" Extensions ")
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::DarkGray)),
        ),
        area,
    );
}

fn render_terminal(frame: &mut ratatui::Frame<'_>, area: Rect, app: &App) {
    let mut values = Vec::new();
    for message in &app.messages {
        for block in &message.content {
            match block {
                MessageContent::ToolCall {
                    name, arguments, ..
                } if is_process_tool(name) => values.push(format!("> {name} {arguments}")),
                MessageContent::ToolResult { name, result, .. } if is_process_tool(name) => {
                    values.push(format!("{name}: {result}"));
                }
                MessageContent::ToolError { name, error, .. } if is_process_tool(name) => {
                    values.push(format!("{name} failed: {error}"));
                }
                _ => {}
            }
        }
    }
    if values.is_empty() {
        values.push(
            "No command has run in this conversation. Ask the agent to run a command or test."
                .to_string(),
        );
    }
    render_named_list(frame, area, " Process and test output ", values);
}

fn is_process_tool(name: &str) -> bool {
    name.starts_with("shell.") || name.starts_with("process.") || name.starts_with("git.")
}

fn render_context(frame: &mut ratatui::Frame<'_>, area: Rect, app: &App) {
    let mut values = vec![format!("Project: {}", app.project_root)];
    values.extend(
        app.workspace_roots
            .iter()
            .map(|path| format!("Granted: {path}")),
    );
    if let Some(conversation) = &app.conversation {
        values.push(format!("Conversation: {}", conversation.id));
    }
    for message in &app.messages {
        for block in &message.content {
            if let MessageContent::FileReference { path, .. } = block {
                values.push(format!("@{path}"));
            }
        }
    }
    values.sort();
    values.dedup();
    render_named_list(frame, area, " Active context ", values);
}

fn render_settings(frame: &mut ratatui::Frame<'_>, area: Rect, app: &App) {
    let mut lines = vec![
        Line::styled(
            "Model",
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        ),
        Line::styled(
            format!(
                "  provider  {}",
                app.provider.as_deref().unwrap_or("not connected")
            ),
            Style::default().fg(Color::Gray),
        ),
        Line::styled(
            format!(
                "  model     {}",
                app.model.as_deref().unwrap_or("not selected")
            ),
            Style::default().fg(Color::Gray),
        ),
        Line::styled(
            format!(
                "  pack      {}",
                app.model_pack
                    .as_deref()
                    .map_or_else(|| "off".to_string(), |_| selected_model_label(app))
            ),
            Style::default().fg(Color::Gray),
        ),
        Line::styled(
            format!(
                "  reasoning {}",
                app.reasoning_level.as_deref().unwrap_or("model default")
            ),
            Style::default().fg(Color::Gray),
        ),
        Line::raw(""),
        Line::styled(
            "Workspace",
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        ),
        Line::styled(
            format!("  project   {}", app.project_root),
            Style::default().fg(Color::Gray),
        ),
        Line::styled(
            format!("  roots     {}", app.workspace_roots.len()),
            Style::default().fg(Color::Gray),
        ),
        Line::raw(""),
        Line::styled(
            "Capabilities",
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        ),
        Line::styled(
            format!(
                "  {} tools   {} skills   {} MCP servers   {} agent roles",
                app.tools.len(),
                app.skills.len(),
                app.mcp_servers.len(),
                app.agent_definitions.len()
            ),
            Style::default().fg(Color::Gray),
        ),
        Line::raw(""),
        Line::styled(
            format!("Permissions  {}", app.permissions.len()),
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        ),
    ];
    if app.permissions.is_empty() {
        lines.push(Line::styled(
            "  No persistent permission overrides.",
            Style::default().fg(Color::DarkGray),
        ));
    } else {
        lines.extend(app.permissions.iter().take(10).map(|rule| {
            Line::styled(
                format!("  {:?}  {:?}  {}", rule.scope, rule.effect, rule.tool_name),
                Style::default().fg(Color::Gray),
            )
        }));
    }
    lines.extend([
        Line::raw(""),
        Line::styled(
            "/models  /packs  /pack  /reasoning  /dirs  /permissions  /skills  /tools  /mcp",
            Style::default().fg(Color::DarkGray),
        ),
        Line::styled("Esc returns to chat", Style::default().fg(Color::DarkGray)),
    ]);
    frame.render_widget(
        Paragraph::new(Text::from(lines))
            .wrap(Wrap { trim: false })
            .block(
                Block::default()
                    .title(" settings ")
                    .borders(Borders::TOP)
                    .border_style(Style::default().fg(Color::DarkGray)),
            ),
        area,
    );
}

fn message_contains_text(message: &Message, expected: &str) -> bool {
    message
        .content
        .iter()
        .any(|content| matches!(content, MessageContent::Text { text } if text == expected))
}

fn cube_loader_line(elapsed: Duration) -> Line<'static> {
    const SYMBOLS: [&str; 4] = ["·", "▪", "■", "█"];
    const COLORS: [Color; 4] = [Color::DarkGray, Color::Gray, Color::White, Color::White];
    let step = elapsed.as_millis() / CUBE_LOADER_STEP.as_millis();
    let center = isize::try_from(step % (CUBE_LOADER_COUNT as u128 + 4)).unwrap_or_default();
    let spans = (0..CUBE_LOADER_COUNT)
        .flat_map(|index| {
            let index = isize::try_from(index).unwrap_or_default();
            let distance = usize::try_from((index - center).abs()).unwrap_or(usize::MAX);
            let level = 3_usize.saturating_sub(distance);
            [
                Span::styled(
                    SYMBOLS[level],
                    Style::default()
                        .fg(COLORS[level])
                        .add_modifier(Modifier::BOLD),
                ),
                Span::raw(" "),
            ]
        })
        .collect::<Vec<_>>();
    Line::from(spans)
}

fn runtime_trace_lines(app: &App) -> Vec<Line<'static>> {
    let limit = if app.busy { 7 } else { 4 };
    let mut entries = app
        .runtime_trace
        .iter()
        .filter(|entry| app.busy || entry.category != "tool")
        .rev()
        .take(limit)
        .collect::<Vec<_>>();
    entries.reverse();
    entries
        .into_iter()
        .map(|entry| {
            let elapsed = entry.elapsed.unwrap_or_else(|| entry.started_at.elapsed());
            let mut spans = vec![
                Span::styled(
                    format!("   {:<5}", entry.category),
                    Style::default().fg(Color::DarkGray),
                ),
                Span::styled(
                    entry.name.clone(),
                    Style::default()
                        .fg(Color::Gray)
                        .add_modifier(Modifier::BOLD),
                ),
            ];
            if let Some(target) = entry.target.as_deref().filter(|target| !target.is_empty()) {
                spans.push(Span::styled(" · ", Style::default().fg(Color::DarkGray)));
                spans.push(Span::styled(
                    target.to_string(),
                    Style::default().fg(Color::DarkGray),
                ));
            }
            spans.extend([
                Span::styled(" · ", Style::default().fg(Color::DarkGray)),
                Span::styled(
                    entry.status.clone(),
                    Style::default().fg(if entry.elapsed.is_some() {
                        Color::Gray
                    } else {
                        Color::White
                    }),
                ),
                Span::styled(
                    format!(" · {}", trace_elapsed(elapsed)),
                    Style::default().fg(Color::DarkGray),
                ),
            ]);
            Line::from(spans)
        })
        .collect()
}

fn trace_elapsed(elapsed: Duration) -> String {
    let milliseconds = elapsed.as_millis();
    if milliseconds == 0 {
        "<1ms".to_string()
    } else if milliseconds < 1_000 {
        format!("{milliseconds}ms")
    } else {
        format!("{:.1}s", elapsed.as_secs_f64())
    }
}

fn render_chat(frame: &mut ratatui::Frame<'_>, area: Rect, app: &App) {
    let mut lines = Vec::new();
    for message in &app.messages {
        let prompt_card = message.role == MessageRole::User;
        if prompt_card {
            lines.push(band_line(
                "YOU",
                area.width,
                PRIMARY_ACCENT,
                PANEL_BG,
                Color::Gray,
            ));
        } else if message.role == MessageRole::Assistant {
            lines.push(Line::styled(
                "   ASSISTANT",
                Style::default()
                    .fg(Color::DarkGray)
                    .add_modifier(Modifier::BOLD),
            ));
        }
        let mut image = 0;
        let mut video = 0;
        let mut audio = 0;
        let mut file = 0;
        for block in &message.content {
            if let MessageContent::FileReference { path, mime_type } = block {
                let kind = mime_type.as_deref().map_or_else(
                    || attachment_kind(Path::new(path)),
                    |mime| {
                        if mime.starts_with("image/") {
                            AttachmentKind::Image
                        } else if mime.starts_with("video/") {
                            AttachmentKind::Video
                        } else if mime.starts_with("audio/") {
                            AttachmentKind::Audio
                        } else {
                            AttachmentKind::File
                        }
                    },
                );
                let count = match kind {
                    AttachmentKind::Image => {
                        image += 1;
                        image
                    }
                    AttachmentKind::Video => {
                        video += 1;
                        video
                    }
                    AttachmentKind::Audio => {
                        audio += 1;
                        audio
                    }
                    AttachmentKind::File => {
                        file += 1;
                        file
                    }
                };
                let name = Path::new(path)
                    .file_name()
                    .and_then(|value| value.to_str())
                    .unwrap_or("attachment");
                lines.push(if prompt_card {
                    band_line(
                        &format!("[{} {count}] {name}", kind.label()),
                        area.width,
                        PRIMARY_ACCENT,
                        PANEL_BG,
                        Color::Gray,
                    )
                } else {
                    Line::styled(
                        format!("   [{} {count}] {name}", kind.label()),
                        Style::default().fg(Color::Gray),
                    )
                });
            } else {
                for line in render_content_block_with_details(block, app.tool_details_expanded) {
                    if prompt_card {
                        let text = line
                            .spans
                            .iter()
                            .map(|span| span.content.as_ref())
                            .collect::<String>();
                        lines.extend(wrapped_band_lines(
                            &text,
                            area.width,
                            PRIMARY_ACCENT,
                            PANEL_BG,
                            Color::White,
                        ));
                    } else {
                        lines.push(prefixed_line("   ", line));
                    }
                }
            }
        }
        if prompt_card {
            lines.push(band_line(
                "",
                area.width,
                PRIMARY_ACCENT,
                PANEL_BG,
                Color::White,
            ));
        }
        lines.push(Line::raw(""));
    }
    if let Some(prompt) = &app.pending_prompt {
        lines.push(band_line(
            "YOU",
            area.width,
            PRIMARY_ACCENT,
            PANEL_BG,
            Color::White,
        ));
        for line in prompt.lines() {
            lines.extend(wrapped_band_lines(
                line,
                area.width,
                PRIMARY_ACCENT,
                PANEL_BG,
                Color::White,
            ));
        }
        lines.push(band_line(
            "",
            area.width,
            PRIMARY_ACCENT,
            PANEL_BG,
            Color::White,
        ));
        lines.push(Line::raw(""));
    }
    if app.busy
        && app.streaming_text.is_empty()
        && let Some(started) = app.loader_started
    {
        lines.push(Line::styled(
            "   ASSISTANT",
            Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::BOLD),
        ));
        lines.push(cube_loader_line(started.elapsed()));
        lines.push(Line::raw(""));
    }
    let stream_is_committed = app.active_run.is_some_and(|run_id| {
        app.messages
            .iter()
            .rev()
            .find(|message| message.role == MessageRole::Assistant)
            .and_then(|message| message.run_id)
            == Some(run_id)
    });
    if !app.streaming_text.is_empty() && !stream_is_committed {
        lines.push(Line::styled(
            "   ASSISTANT",
            Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::BOLD),
        ));
        lines.extend(
            app.streaming_text
                .lines()
                .map(|line| Line::raw(format!("   {line}"))),
        );
    }
    let trace = runtime_trace_lines(app);
    if !trace.is_empty() {
        lines.push(Line::raw(""));
        lines.extend(trace);
        lines.push(Line::raw(""));
    }
    if let Some(error) = &app.last_error {
        lines.push(Line::raw(""));
        for line in error.lines() {
            lines.extend(wrapped_band_lines(
                line,
                area.width,
                ERROR_ACCENT,
                SUBTLE_BG,
                Color::Gray,
            ));
        }
        lines.push(Line::raw(""));
    }
    if app.chat_scroll_offset > 0 {
        lines.insert(
            0,
            Line::styled(
                format!(
                    "↑ {} lines from latest · End to return",
                    app.chat_scroll_offset
                ),
                Style::default().fg(Color::DarkGray),
            ),
        );
    }
    let paragraph = Paragraph::new(Text::from(lines)).wrap(Wrap { trim: false });
    let visible = usize::from(area.height);
    let wrapped_lines = paragraph.line_count(area.width);
    let maximum_scroll = u16::try_from(wrapped_lines.saturating_sub(visible)).unwrap_or(u16::MAX);
    let scroll = maximum_scroll.saturating_sub(app.chat_scroll_offset.min(maximum_scroll));
    frame.render_widget(paragraph.scroll((scroll, 0)), area);
}

fn band_line(
    text: &str,
    width: u16,
    rail: Color,
    background: Color,
    foreground: Color,
) -> Line<'static> {
    let available = usize::from(width.saturating_sub(4));
    let content = format!("  {text:<available$} ");
    Line::from(vec![
        Span::styled("▎", Style::default().fg(rail).bg(background)),
        Span::styled(content, Style::default().fg(foreground).bg(background)),
    ])
}

fn wrapped_band_lines(
    text: &str,
    width: u16,
    rail: Color,
    background: Color,
    foreground: Color,
) -> Vec<Line<'static>> {
    let available = usize::from(width.saturating_sub(5)).max(1);
    if text.is_empty() {
        return vec![band_line("", width, rail, background, foreground)];
    }
    let chars = text.chars().collect::<Vec<_>>();
    chars
        .chunks(available)
        .map(|chunk| {
            band_line(
                &chunk.iter().collect::<String>(),
                width,
                rail,
                background,
                foreground,
            )
        })
        .collect()
}

fn prefixed_line(prefix: &'static str, line: Line<'static>) -> Line<'static> {
    let mut spans = Vec::with_capacity(line.spans.len() + 1);
    spans.push(Span::styled(prefix, Style::default().fg(Color::DarkGray)));
    spans.extend(line.spans);
    Line::from(spans)
}

#[allow(dead_code)]
fn render_sidebar(frame: &mut ratatui::Frame<'_>, area: Rect, app: &App) {
    let sections = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(10),
            Constraint::Length(9),
            Constraint::Min(7),
        ])
        .split(area);
    let (input, output, cached) = if app.busy {
        (
            app.live_input_tokens,
            app.live_output_tokens,
            app.live_cached_tokens,
        )
    } else {
        (
            total_input_tokens(&app.metrics),
            app.metrics.usage.output_tokens,
            app.metrics.usage.cached_tokens,
        )
    };
    frame.render_widget(
        Paragraph::new(vec![
            Line::from(vec![
                Span::styled("IN     ", Style::default().fg(Color::White)),
                Span::styled(
                    input.to_string(),
                    Style::default().add_modifier(Modifier::BOLD),
                ),
            ]),
            Line::from(vec![
                Span::styled("OUT    ", Style::default().fg(Color::White)),
                Span::styled(
                    output.to_string(),
                    Style::default().add_modifier(Modifier::BOLD),
                ),
            ]),
            Line::from(vec![
                Span::styled("CACHE  ", Style::default().fg(Color::Gray)),
                Span::raw(cached.to_string()),
            ]),
            Line::raw(format!("model calls  {}", app.metrics.model_calls)),
            Line::raw(format!("tool calls   {}", app.metrics.tool_calls)),
            Line::raw(format!(
                "cost         ${}.{:06}",
                app.metrics.cost_microusd / 1_000_000,
                app.metrics.cost_microusd % 1_000_000
            )),
        ])
        .block(
            Block::default()
                .title(" Usage ")
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::White)),
        ),
        sections[0],
    );
    let running_agents = app
        .agents
        .iter()
        .filter(|agent| !agent.status.is_terminal())
        .count();
    let running_tasks = app
        .tasks
        .iter()
        .filter(|task| !task.status.is_terminal())
        .count();
    frame.render_widget(
        Paragraph::new(vec![
            Line::from(vec![
                Span::styled("routing  ", Style::default().fg(Color::Gray)),
                Span::styled(
                    if app.automatic_agent {
                        "automatic"
                    } else {
                        app.selected_agent.as_str()
                    },
                    Style::default().add_modifier(Modifier::BOLD),
                ),
            ]),
            Line::raw(format!("active agents   {running_agents}")),
            Line::raw(format!("active tasks    {running_tasks}")),
            Line::raw(format!("available roles {}", app.agent_definitions.len())),
            Line::raw(if app.busy {
                "state           working"
            } else {
                "state           ready"
            }),
        ])
        .block(
            Block::default()
                .title(" Pipeline ")
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Gray)),
        ),
        sections[1],
    );
    frame.render_widget(
        Paragraph::new(vec![
            Line::raw(format!(
                "skills      {} ({} active)",
                app.skills.len(),
                app.last_active_skills.len()
            )),
            Line::raw(format!("tools       {}", app.tools.len())),
            Line::raw(format!("MCP servers {}", app.mcp_servers.len())),
            Line::raw(format!("changes     {}", app.changes.len())),
            Line::raw(""),
            Line::styled("Ctrl+K commands", Style::default().fg(Color::White)),
            Line::raw("Ctrl+M models"),
            Line::raw("Ctrl+A routing"),
            Line::raw("F1 help"),
        ])
        .block(
            Block::default()
                .title(" Workspace ")
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Gray)),
        ),
        sections[2],
    );
}

fn render_changes(frame: &mut ratatui::Frame<'_>, area: Rect, app: &App) {
    let mut lines = Vec::new();
    if app.changes.is_empty() {
        lines.push(Line::styled(
            "No model-applied file changes are recorded for this server.",
            Style::default().fg(Color::DarkGray),
        ));
    }
    for change in &app.changes {
        let color = if change.state == FileChangeState::Applied {
            Color::Gray
        } else {
            Color::DarkGray
        };
        lines.push(Line::styled(
            format!(
                "{:?}  {}  agent {}",
                change.state, change.relative_path, change.agent_id
            ),
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        ));
        if let Some(patch) = &change.patch {
            lines.extend(patch.lines().take(80).map(|line| {
                let color = if line.starts_with('+') && !line.starts_with("+++") {
                    Color::White
                } else if line.starts_with('-') && !line.starts_with("---") {
                    Color::Red
                } else {
                    Color::Gray
                };
                Line::styled(line.to_string(), Style::default().fg(color))
            }));
        } else {
            lines.push(Line::styled(
                "No reversible text patch is available.",
                Style::default().fg(Color::Red),
            ));
        }
        lines.push(Line::raw(""));
    }
    let visible = usize::from(area.height.saturating_sub(2));
    let scroll = u16::try_from(lines.len().saturating_sub(visible)).unwrap_or(u16::MAX);
    frame.render_widget(
        Paragraph::new(Text::from(lines))
            .wrap(Wrap { trim: false })
            .scroll((scroll, 0))
            .block(
                Block::default()
                    .title(" Changes · /undo · /redo ")
                    .borders(Borders::ALL),
            ),
        area,
    );
}

#[cfg(test)]
fn render_content_block(block: &MessageContent) -> Vec<Line<'static>> {
    render_content_block_with_details(block, false)
}

fn render_content_block_with_details(
    block: &MessageContent,
    show_tool_details: bool,
) -> Vec<Line<'static>> {
    match block {
        MessageContent::Text { text } => text
            .lines()
            .map(|line| Line::raw(line.to_string()))
            .collect(),
        MessageContent::ReasoningSummary { .. } => Vec::new(),
        MessageContent::ContextSummary { .. } => vec![Line::styled(
            "context compacted",
            Style::default().fg(Color::DarkGray),
        )],
        MessageContent::FileReference { path, .. } => {
            vec![Line::styled(
                format!("@{path}"),
                Style::default().fg(Color::Gray),
            )]
        }
        MessageContent::ToolCall { name, .. } => vec![Line::styled(
            format!("› {name}"),
            Style::default().fg(Color::DarkGray),
        )],
        MessageContent::ToolResult { name, result, .. } => {
            let mut lines = vec![Line::styled(
                format!("✓ {name}  {}", tool_result_summary(result)),
                Style::default().fg(Color::Gray),
            )];
            if show_tool_details {
                lines.extend(tool_result_details(result).lines().map(|line| {
                    Line::styled(format!("    {line}"), Style::default().fg(Color::DarkGray))
                }));
            }
            lines
        }
        MessageContent::ToolError { name, error, .. } => vec![Line::styled(
            format!("× {name}  {error}"),
            Style::default().fg(Color::LightRed),
        )],
        MessageContent::ApprovalRequest { summary, .. } => vec![Line::styled(
            format!("approval needed  {summary}"),
            Style::default().fg(Color::White),
        )],
        MessageContent::ApprovalResult { decision, .. } => {
            vec![Line::styled(
                format!("approval  {decision}"),
                Style::default().fg(Color::DarkGray),
            )]
        }
    }
}

fn tool_result_details(result: &Value) -> String {
    let mut safe = result.clone();
    if let Some(object) = safe.pointer_mut("/output").and_then(Value::as_object_mut) {
        object.remove("data_url");
        for key in ["content", "stdout", "stderr"] {
            if let Some(value) = object.get(key).and_then(Value::as_str) {
                let original_length = value.chars().count();
                let shortened = value.chars().take(2_000).collect::<String>();
                if shortened.chars().count() < original_length {
                    object.insert(
                        key.to_string(),
                        Value::String(format!("{shortened}\n… truncated for display")),
                    );
                }
            }
        }
    }
    serde_json::to_string_pretty(&safe).unwrap_or_else(|_| safe.to_string())
}

fn tool_result_summary(result: &Value) -> String {
    let entries = result
        .pointer("/output/entries")
        .or_else(|| result.get("entries"))
        .and_then(Value::as_array);
    if let Some(entries) = entries {
        let truncated = result
            .pointer("/output/truncated")
            .or_else(|| result.get("truncated"))
            .and_then(Value::as_bool)
            .unwrap_or(false);
        return format!(
            "{} item{}{}",
            entries.len(),
            if entries.len() == 1 { "" } else { "s" },
            if truncated { " · more available" } else { "" }
        );
    }
    let mutations = result
        .get("file_mutations")
        .and_then(Value::as_array)
        .map_or(0, Vec::len);
    if mutations > 0 {
        return format!(
            "{mutations} file change{}",
            if mutations == 1 { "" } else { "s" }
        );
    }
    if let Some(content) = result
        .pointer("/output/content")
        .or_else(|| result.get("content"))
        .and_then(Value::as_str)
    {
        return format!("{} characters", content.chars().count());
    }
    "done".to_string()
}

fn render_agents(frame: &mut ratatui::Frame<'_>, area: Rect, app: &App) {
    let sections = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Percentage(42), Constraint::Percentage(58)])
        .split(area);
    let rows = app.agents.iter().map(|agent| {
        let policy = app
            .role_policies
            .iter()
            .find(|policy| policy.policy.role == agent.role);
        let policy_label = policy.map_or_else(
            || reasoning_label(agent.reasoning.level.as_deref(), None),
            |policy| {
                format!(
                    "{} · {}",
                    reasoning_label(
                        policy.policy.reasoning_effort.as_deref(),
                        Some(policy.policy.thinking)
                    ),
                    policy.policy.tool_profile.label()
                )
            },
        );
        Row::new(vec![
            Cell::from(agent.canonical_path.clone()),
            Cell::from(agent.role.clone()),
            Cell::from(format!("{:?}", agent.status)),
            Cell::from(format!("{}/{}", agent.provider, agent.model)),
            Cell::from(policy_label),
        ])
    });
    frame.render_widget(
        Table::new(
            rows,
            [
                Constraint::Percentage(24),
                Constraint::Percentage(20),
                Constraint::Percentage(12),
                Constraint::Percentage(24),
                Constraint::Percentage(20),
            ],
        )
        .header(
            Row::new(["Path", "Role", "Status", "Pinned provider/model", "Policy"]).style(
                Style::default()
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD),
            ),
        )
        .block(
            Block::default()
                .title(if app.agents.is_empty() {
                    " Active pipeline · starts automatically when needed "
                } else {
                    " Active and completed pipeline "
                })
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Gray)),
        ),
        sections[0],
    );
    if area.width >= 112 {
        let rows = app.role_policies.iter().map(|descriptor| {
            let policy = &descriptor.policy;
            let primary = descriptor.primary.as_ref().map_or_else(
                || match policy.execution {
                    opensrc_runtime::RoleExecutionKind::Deterministic => "runtime".to_string(),
                    opensrc_runtime::RoleExecutionKind::Hybrid => "runtime + summary".to_string(),
                    opensrc_runtime::RoleExecutionKind::Llm => "unavailable".to_string(),
                },
                |assignment| assignment.display_name.clone(),
            );
            let fallback = if descriptor.fallbacks.is_empty() {
                "—".to_string()
            } else {
                descriptor
                    .fallbacks
                    .iter()
                    .map(|assignment| assignment.display_name.as_str())
                    .collect::<Vec<_>>()
                    .join(" → ")
            };
            Row::new(vec![
                Cell::from(policy.role.clone()),
                Cell::from(primary),
                Cell::from(fallback),
                Cell::from(reasoning_label(
                    policy.reasoning_effort.as_deref(),
                    Some(policy.thinking),
                )),
                Cell::from(enum_label(&policy.context_policy.inheritance)),
                Cell::from(policy.tool_profile.label()),
                Cell::from(if policy.writable_paths.is_empty() {
                    "read-only".to_string()
                } else {
                    policy.writable_paths.join(", ")
                }),
                Cell::from(format!(
                    "{}/{}",
                    enum_label(&policy.cost_class),
                    enum_label(&policy.latency_class)
                )),
            ])
        });
        frame.render_widget(
            Table::new(
                rows,
                [
                    Constraint::Length(25),
                    Constraint::Length(20),
                    Constraint::Length(22),
                    Constraint::Length(18),
                    Constraint::Length(18),
                    Constraint::Length(18),
                    Constraint::Length(16),
                    Constraint::Min(13),
                ],
            )
            .header(
                Row::new([
                    "Role",
                    "Primary",
                    "Fallback",
                    "Thinking/effort",
                    "Context",
                    "Tools",
                    "Writable",
                    "Cost/latency",
                ])
                .style(
                    Style::default()
                        .fg(Color::White)
                        .add_modifier(Modifier::BOLD),
                ),
            )
            .block(
                Block::default()
                    .title(format!(
                        " Routing policy v1 ({}) · choose model before run to override ",
                        app.role_policies.len()
                    ))
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(Color::DarkGray)),
            ),
            sections[1],
        );
    } else {
        let policies = app
            .role_policies
            .iter()
            .map(|descriptor| {
                let policy = &descriptor.policy;
                let primary = descriptor
                    .primary
                    .as_ref()
                    .map_or("runtime", |value| value.display_name.as_str());
                ListItem::new(Line::from(vec![
                    Span::styled(
                        format!("{:<28}", policy.role),
                        Style::default()
                            .fg(Color::White)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::raw(format!(
                        "{primary} · {} · {} · {}",
                        reasoning_label(policy.reasoning_effort.as_deref(), Some(policy.thinking)),
                        policy.tool_profile.label(),
                        enum_label(&policy.context_policy.inheritance)
                    )),
                ]))
            })
            .collect::<Vec<_>>();
        frame.render_widget(
            List::new(policies).block(
                Block::default()
                    .title(" Routing policy v1 · Ctrl+M overrides model ")
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(Color::DarkGray)),
            ),
            sections[1],
        );
    }
}

fn reasoning_label(
    effort: Option<&str>,
    thinking: Option<opensrc_runtime::ThinkingMode>,
) -> String {
    let thinking = match thinking {
        Some(opensrc_runtime::ThinkingMode::Disabled) => "non-thinking",
        Some(opensrc_runtime::ThinkingMode::Always) => "always thinking",
        Some(opensrc_runtime::ThinkingMode::Enabled) => "thinking",
        None => "default",
    };
    effort.map_or_else(
        || thinking.to_string(),
        |effort| format!("{thinking} · {effort}"),
    )
}

fn enum_label(value: &impl std::fmt::Debug) -> String {
    let raw = format!("{value:?}");
    raw.chars()
        .enumerate()
        .flat_map(|(index, character)| {
            if index > 0 && character.is_ascii_uppercase() {
                vec![' ', character.to_ascii_lowercase()]
            } else {
                vec![character.to_ascii_lowercase()]
            }
        })
        .collect()
}

fn render_skills(frame: &mut ratatui::Frame<'_>, area: Rect, app: &App) {
    let items = app
        .skills
        .iter()
        .map(|skill| {
            let active = app.active_skills.iter().any(|name| name == &skill.name)
                || app
                    .last_active_skills
                    .iter()
                    .any(|name| name == &skill.name);
            ListItem::new(Line::from(vec![
                Span::styled(
                    if active { "● " } else { "○ " },
                    Style::default().fg(if active {
                        Color::White
                    } else {
                        Color::DarkGray
                    }),
                ),
                Span::styled(
                    format!("{:<28}", skill.name),
                    Style::default()
                        .fg(Color::White)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::raw(skill.description.clone()),
            ]))
        })
        .collect::<Vec<_>>();
    frame.render_widget(
        List::new(items).block(
            Block::default()
                .title(format!(
                    " Skills ({}) · triggers auto-load · /skill <name> activates explicitly ",
                    app.skills.len()
                ))
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::White)),
        ),
        area,
    );
}

fn render_tools(frame: &mut ratatui::Frame<'_>, area: Rect, app: &App) {
    let rows = app.tools.iter().map(|tool| {
        Row::new(vec![
            Cell::from(tool.name.clone()),
            Cell::from(format!("{:?}", tool.risk)),
            Cell::from(format!("{:?}", tool.approval_rule)),
            Cell::from(if tool.supports_parallel { "yes" } else { "no" }),
            Cell::from(tool.description.clone()),
        ])
    });
    frame.render_widget(
        Table::new(
            rows,
            [
                Constraint::Length(24),
                Constraint::Length(12),
                Constraint::Length(12),
                Constraint::Length(9),
                Constraint::Min(24),
            ],
        )
        .header(
            Row::new(["Tool", "Risk", "Approval", "Parallel", "Purpose"]).style(
                Style::default()
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD),
            ),
        )
        .block(
            Block::default()
                .title(format!(
                    " Connected tools ({}) · automatically exposed by agent policy ",
                    app.tools.len()
                ))
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::White)),
        ),
        area,
    );
}

fn render_tasks(frame: &mut ratatui::Frame<'_>, area: Rect, app: &App) {
    let rows = if app.tasks.is_empty() {
        vec![Row::new(vec![
            Cell::from(
                "No active tasks — automatic mode creates a plan when the request needs one",
            ),
            Cell::from("Ready"),
            Cell::from("-"),
        ])]
    } else {
        app.tasks
            .iter()
            .map(|task| {
                Row::new(vec![
                    Cell::from(task.description.clone()),
                    Cell::from(format!("{:?}", task.status)),
                    Cell::from(task.priority.to_string()),
                ])
            })
            .collect()
    };
    frame.render_widget(
        Table::new(
            rows,
            [
                Constraint::Percentage(70),
                Constraint::Percentage(20),
                Constraint::Percentage(10),
            ],
        )
        .header(
            Row::new(["Task", "Status", "P"]).style(
                Style::default()
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD),
            ),
        )
        .block(
            Block::default()
                .title(" Automatic plan and task pipeline ")
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Gray)),
        ),
        area,
    );
}

fn render_sessions(frame: &mut ratatui::Frame<'_>, area: Rect, app: &App) {
    render_named_list(
        frame,
        area,
        " Sessions (newest first) ",
        app.conversations
            .iter()
            .map(|value| {
                format!(
                    "{}  {}",
                    value.updated_at.format("%Y-%m-%d %H:%M"),
                    value.title.as_deref().unwrap_or("untitled")
                )
            })
            .collect(),
    );
}

fn render_metrics(frame: &mut ratatui::Frame<'_>, area: Rect, app: &App) {
    let metrics = &app.metrics;
    let usage = &metrics.usage;
    if area.height < 14 {
        frame.render_widget(
            Paragraph::new(format!(
                "calls: {} model · {} tool · {} failed\nagents: {} · messages: {}\ntokens: {} in · {} out · {} cached\nlatency: {} ms · cost: {}",
                metrics.model_calls,
                metrics.tool_calls,
                metrics.failed_tools,
                metrics.agents,
                metrics.inter_agent_messages,
                total_input_tokens(metrics),
                usage.output_tokens,
                usage.cached_tokens,
                metrics.timing.total_ms,
                format_microusd(metrics.cost_microusd)
            ))
            .block(
                Block::default()
                    .title(" Metrics ")
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(Color::White)),
            ),
            area,
        );
        return;
    }
    let sections = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(13), Constraint::Min(5)])
        .split(area);
    frame.render_widget(
        Paragraph::new(format!(
            "calls     {} model · {} tool · {} failed\npipeline  {} agents · {} inter-agent messages\n\ninput     {} tokens\n          {} instructions · {} user · {} repository\n          {} schemas · {} tool output · {} compaction · {} inherited\noutput    {} tokens\ncache     {} tokens\nlatency   {} ms\ncost      {}",
            metrics.model_calls,
            metrics.tool_calls,
            metrics.failed_tools,
            metrics.agents,
            metrics.inter_agent_messages,
            total_input_tokens(metrics),
            usage.base_instruction_tokens,
            usage.user_tokens,
            usage.repository_context_tokens,
            usage.tool_schema_tokens,
            usage.tool_output_tokens,
            usage.compaction_tokens,
            usage.subagent_inheritance_tokens,
            usage.output_tokens,
            usage.cached_tokens,
            metrics.timing.total_ms,
            format_microusd(metrics.cost_microusd)
        ))
        .block(
            Block::default()
                .title(" Runtime usage ")
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::White)),
        ),
        sections[0],
    );
    render_routing_benchmarks(frame, sections[1], &app.routing_benchmarks);
}

fn render_routing_benchmarks(
    frame: &mut ratatui::Frame<'_>,
    area: Rect,
    benchmarks: &[RoutingBenchmarkAggregate],
) {
    let block = Block::default()
        .title(" Routing benchmarks · mean by role and route ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Gray));
    if benchmarks.is_empty() {
        frame.render_widget(
            Paragraph::new("No benchmark results recorded yet.")
                .style(Style::default().fg(Color::DarkGray))
                .block(block),
            area,
        );
        return;
    }
    if area.width < 86 {
        let lines = benchmarks
            .iter()
            .map(|aggregate| {
                Line::from(format!(
                    "{} · {} · {} · {} ms · {} · n={}",
                    aggregate.role,
                    aggregate.model,
                    format_benchmark_quality(&aggregate.mean_metrics),
                    aggregate.mean_metrics.latency_ms,
                    format_microusd(aggregate.mean_metrics.cost_microusd),
                    aggregate.samples
                ))
            })
            .collect::<Vec<_>>();
        frame.render_widget(
            Paragraph::new(lines)
                .wrap(Wrap { trim: false })
                .block(block),
            area,
        );
        return;
    }
    let rows = benchmarks.iter().map(|aggregate| {
        Row::new(vec![
            Cell::from(aggregate.role.clone()),
            Cell::from(format!("{}/{}", aggregate.provider, aggregate.model)),
            Cell::from(aggregate.samples.to_string()),
            Cell::from(format_benchmark_quality(&aggregate.mean_metrics)),
            Cell::from(format!("{} ms", aggregate.mean_metrics.latency_ms)),
            Cell::from(format_microusd(aggregate.mean_metrics.cost_microusd)),
        ])
    });
    let header = Row::new([
        "Role",
        "Provider / model",
        "Samples",
        "Quality",
        "Latency",
        "Cost",
    ])
    .style(
        Style::default()
            .fg(Color::White)
            .add_modifier(Modifier::BOLD),
    );
    frame.render_widget(
        Table::new(
            rows,
            [
                Constraint::Length(20),
                Constraint::Min(28),
                Constraint::Length(8),
                Constraint::Length(9),
                Constraint::Length(12),
                Constraint::Length(13),
            ],
        )
        .header(header)
        .column_spacing(1)
        .block(block),
        area,
    );
}

fn routing_quality_bps(metrics: &RoutingBenchmarkMetrics) -> Option<u16> {
    let scores = [
        metrics.architecture_quality_bps,
        metrics.repository_investigation_accuracy_bps,
        metrics.patch_success_bps,
        metrics.test_pass_rate_bps,
        metrics.tool_call_correctness_bps,
        metrics.frontend_implementation_quality_bps,
        metrics.accessibility_finding_quality_bps,
        metrics.review_precision_bps,
        metrics.security_review_precision_bps,
    ];
    let (sum, count) = scores
        .into_iter()
        .flatten()
        .fold((0_u64, 0_u64), |(sum, count), score| {
            (sum.saturating_add(u64::from(score)), count + 1)
        });
    (count > 0)
        .then(|| u16::try_from(sum / count).unwrap_or(RoutingBenchmarkMetrics::MAX_BASIS_POINTS))
}

fn format_benchmark_quality(metrics: &RoutingBenchmarkMetrics) -> String {
    routing_quality_bps(metrics).map_or_else(
        || "—".to_string(),
        |score| format!("{}.{:02}%", score / 100, score % 100),
    )
}

fn format_microusd(value: u64) -> String {
    format!("${}.{:06}", value / 1_000_000, value % 1_000_000)
}

fn total_input_tokens(metrics: &PerformanceSnapshot) -> u64 {
    let usage = &metrics.usage;
    usage
        .base_instruction_tokens
        .saturating_add(usage.user_tokens)
        .saturating_add(usage.repository_context_tokens)
        .saturating_add(usage.tool_schema_tokens)
        .saturating_add(usage.tool_output_tokens)
        .saturating_add(usage.compaction_tokens)
        .saturating_add(usage.subagent_inheritance_tokens)
}

fn render_named_list(frame: &mut ratatui::Frame<'_>, area: Rect, title: &str, values: Vec<String>) {
    let items = if values.is_empty() {
        vec![ListItem::new("No records.")]
    } else {
        values.into_iter().map(ListItem::new).collect()
    };
    frame.render_widget(
        List::new(items).block(Block::default().title(title).borders(Borders::ALL)),
        area,
    );
}

fn render_editor(frame: &mut ratatui::Frame<'_>, area: Rect, app: &App) {
    render_editor_with_mode(frame, area, app, false);
}

fn render_editor_with_mode(frame: &mut ratatui::Frame<'_>, area: Rect, app: &App, welcome: bool) {
    let has_attachments = !app.attachments.is_empty();
    let show_welcome_placeholder = welcome && app.editor.text.is_empty();
    let block = Block::default().style(Style::default().bg(PANEL_BG));
    let input_area = Rect::new(
        area.x.saturating_add(3),
        area.y.saturating_add(1),
        area.width.saturating_sub(5),
        area.height.saturating_sub(3),
    );
    frame.render_widget(block, area);
    frame.render_widget(
        Paragraph::new("▎\n▎\n▎\n▎\n▎\n▎").style(Style::default().fg(PRIMARY_ACCENT).bg(PANEL_BG)),
        Rect::new(area.x, area.y, 1, area.height),
    );
    frame.render_widget(
        Paragraph::new(composer_status_line(app)).style(Style::default().bg(PANEL_BG)),
        Rect::new(
            area.x.saturating_add(3),
            area.bottom().saturating_sub(2),
            area.width.saturating_sub(5),
            1,
        ),
    );
    frame.render_widget(
        Paragraph::new(Text::from(if show_welcome_placeholder {
            vec![Line::styled(
                "Ask anything…  “Fix broken tests”",
                Style::default().fg(Color::DarkGray),
            )]
        } else {
            let mut input = Vec::new();
            if has_attachments {
                input.push(attachment_line(&app.attachments));
            }
            input.extend(editor_text(&app.editor, app.overlay.is_none() && !app.busy).lines);
            input
        }))
        .wrap(Wrap { trim: false })
        .style(Style::default().bg(PANEL_BG)),
        input_area,
    );
    if app.overlay.is_none() && !app.busy {
        let before = &app.editor.text[..app.editor.cursor];
        let row = before.lines().count().saturating_sub(1);
        let column = before
            .lines()
            .next_back()
            .map_or(0, |line| line.chars().count());
        let x = area
            .x
            .saturating_add(3)
            .saturating_add(u16::try_from(column).unwrap_or(u16::MAX))
            .min(area.right().saturating_sub(1));
        let y = area
            .y
            .saturating_add(1)
            .saturating_add(u16::from(has_attachments))
            .saturating_add(u16::try_from(row).unwrap_or(u16::MAX))
            .min(area.bottom().saturating_sub(2));
        frame.set_cursor_position((x, y));
    }
}

fn composer_status_line(app: &App) -> Line<'static> {
    let mode = match app.mode {
        Some(ExecutionMode::Direct) => "Direct".to_string(),
        Some(ExecutionMode::Focused) => "Focused".to_string(),
        Some(ExecutionMode::Agentic) => "Agentic".to_string(),
        None if app.automatic_agent => "Auto".to_string(),
        None => app.selected_agent.clone(),
    };
    let model = selected_model_label(app);
    let provider = if app.model_pack.is_some() {
        "model pack".to_string()
    } else {
        provider_display_name(app.provider.as_deref())
    };
    Line::from(vec![
        Span::styled(
            mode,
            Style::default()
                .fg(PRIMARY_ACCENT)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled("  ·  ", Style::default().fg(Color::DarkGray)),
        Span::styled(
            model,
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!("  {provider}"),
            Style::default().fg(Color::DarkGray),
        ),
    ])
}

fn selected_model_label(app: &App) -> String {
    if let Some(id) = app.model_pack.as_deref() {
        return app.model_pack_descriptor(id).map_or_else(
            || format!("{id} · pack"),
            |descriptor| {
                format!(
                    "{} · {} models",
                    descriptor.pack.name,
                    descriptor.pack.members.len()
                )
            },
        );
    }
    app.model
        .clone()
        .unwrap_or_else(|| "Choose model".to_string())
}

fn provider_display_name(provider: Option<&str>) -> String {
    match provider.unwrap_or_default() {
        "openrouter" => "OpenRouter".to_string(),
        "aicredits" => "AICredits".to_string(),
        "" => "Connect provider".to_string(),
        value => value
            .split(['-', '_'])
            .filter(|part| !part.is_empty())
            .map(|part| {
                let mut chars = part.chars();
                chars.next().map_or_else(String::new, |first| {
                    first.to_uppercase().collect::<String>() + chars.as_str()
                })
            })
            .collect::<Vec<_>>()
            .join(" "),
    }
}

fn attachment_line(attachments: &[PendingAttachment]) -> Line<'static> {
    let mut image = 0;
    let mut video = 0;
    let mut audio = 0;
    let mut file = 0;
    let mut spans = vec![Span::styled(
        " Attached ",
        Style::default()
            .fg(Color::DarkGray)
            .add_modifier(Modifier::BOLD),
    )];
    for attachment in attachments {
        let count = match attachment.kind {
            AttachmentKind::Image => {
                image += 1;
                image
            }
            AttachmentKind::Video => {
                video += 1;
                video
            }
            AttachmentKind::Audio => {
                audio += 1;
                audio
            }
            AttachmentKind::File => {
                file += 1;
                file
            }
        };
        spans.push(Span::styled(
            format!(" {} {} ", attachment.kind.label(), count),
            Style::default()
                .fg(Color::White)
                .bg(match attachment.kind {
                    AttachmentKind::Image
                    | AttachmentKind::Video
                    | AttachmentKind::Audio
                    | AttachmentKind::File => Color::DarkGray,
                })
                .add_modifier(Modifier::BOLD),
        ));
        spans.push(Span::raw(" "));
    }
    Line::from(spans)
}

fn editor_text(editor: &PromptEditor, show_cursor: bool) -> Text<'static> {
    let selection = editor.selected_range();
    let mut offset = 0;
    let lines = editor
        .text
        .split('\n')
        .map(|line| {
            let mut spans = line
                .char_indices()
                .map(|(index, character)| {
                    let at_cursor = show_cursor && editor.cursor == offset + index;
                    let selected = selection
                        .as_ref()
                        .is_some_and(|range| range.contains(&(offset + index)));
                    Span::styled(
                        character.to_string(),
                        if at_cursor {
                            Style::default()
                                .fg(Color::Black)
                                .bg(Color::White)
                                .add_modifier(Modifier::BOLD)
                        } else if selected {
                            Style::default().fg(Color::Black).bg(Color::Gray)
                        } else {
                            Style::default()
                        },
                    )
                })
                .collect::<Vec<_>>();
            if show_cursor && editor.cursor == offset + line.len() {
                spans.push(Span::styled(
                    " ",
                    Style::default()
                        .fg(Color::Black)
                        .bg(Color::White)
                        .add_modifier(Modifier::BOLD),
                ));
            }
            offset += line.len() + 1;
            Line::from(spans)
        })
        .collect::<Vec<_>>();
    Text::from(lines)
}

fn render_footer(frame: &mut ratatui::Frame<'_>, area: Rect, app: &App) {
    let columns = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(55), Constraint::Percentage(45)])
        .split(area);
    frame.render_widget(
        Paragraph::new(app.project_root.as_str()).style(Style::default().fg(Color::DarkGray)),
        columns[0],
    );
    frame.render_widget(
        Paragraph::new(shortcut_line(columns[1].width)).alignment(Alignment::Right),
        columns[1],
    );
}

fn shortcut_line(width: u16) -> Line<'static> {
    if width < 42 {
        return Line::from(vec![
            Span::styled("^K", Style::default().fg(Color::White)),
            Span::styled(" cmd  ", Style::default().fg(Color::DarkGray)),
            Span::styled("^M", Style::default().fg(Color::White)),
            Span::styled(" model  ", Style::default().fg(Color::DarkGray)),
            Span::styled("^A", Style::default().fg(Color::White)),
            Span::styled(" agent", Style::default().fg(Color::DarkGray)),
        ]);
    }
    Line::from(vec![
        Span::styled("ctrl+k", Style::default().fg(Color::White)),
        Span::styled(" commands    ", Style::default().fg(Color::DarkGray)),
        Span::styled("ctrl+m", Style::default().fg(Color::White)),
        Span::styled(" models    ", Style::default().fg(Color::DarkGray)),
        Span::styled("ctrl+a", Style::default().fg(Color::White)),
        Span::styled(" agents  ", Style::default().fg(Color::DarkGray)),
        Span::styled("ctrl+o", Style::default().fg(Color::White)),
        Span::styled(" details  ", Style::default().fg(Color::DarkGray)),
        Span::styled("/settings", Style::default().fg(Color::Gray)),
    ])
}

fn render_overlay(frame: &mut ratatui::Frame<'_>, area: Rect, overlay: &Overlay) {
    let popup = overlay_rect(area, overlay);
    let (title, accent) = match overlay {
        Overlay::Help => ("Help", MODAL_BORDER),
        Overlay::Error(_) => ("Error", ERROR_ACCENT),
        Overlay::DeleteConversation(_) => ("Delete conversation", ERROR_ACCENT),
        Overlay::Setup(_) => ("Connect a provider", MODAL_BORDER),
        Overlay::Approval(_) => ("Permission required", Color::Rgb(190, 190, 190)),
        Overlay::ApprovalEditor { .. } => ("Edit tool arguments", MODAL_BORDER),
        Overlay::Picker(picker) => (picker.title.trim(), MODAL_BORDER),
    };
    render_modal_surface(frame, area, popup, title, accent);

    match overlay {
        Overlay::Help => render_help_modal(frame, popup),
        Overlay::Error(error) => render_error_modal(frame, popup, error),
        Overlay::DeleteConversation(conversation) => {
            render_delete_conversation_modal(frame, popup, conversation);
        }
        Overlay::Setup(setup) => render_setup_modal(frame, popup, setup),
        Overlay::Approval(approval) => render_approval_modal(frame, popup, approval),
        Overlay::ApprovalEditor { approval, editor } => {
            render_approval_editor_modal(frame, popup, approval, editor);
        }
        Overlay::Picker(picker) => render_picker_modal(frame, popup, picker),
    }
}

fn render_help_modal(frame: &mut ratatui::Frame<'_>, popup: Rect) {
    let body = render_modal_footer(
        frame,
        popup,
        vec![modal_shortcuts(&[
            ("Enter/Esc", "Close"),
            ("Ctrl+K", "Commands"),
            ("Ctrl+M", "Models"),
            ("Ctrl+A", "Agents"),
        ])],
    );
    let mut lines = vec![
        modal_section("ESSENTIAL"),
        modal_shortcuts(&[
            ("Ctrl+Enter", "Send"),
            ("Enter", "New line"),
            ("Ctrl+C", "Cancel; twice to quit"),
        ]),
        modal_shortcuts(&[
            ("Ctrl+N", "New session"),
            ("Ctrl+Left/Right", "Change view"),
            ("F1", "Help"),
        ]),
        Line::raw(""),
        modal_section("EDITING"),
        modal_shortcuts(&[
            ("Shift+Arrows", "Select"),
            ("Ctrl+Shift+C", "Copy selection/latest response"),
            ("Ctrl+E", "External editor"),
        ]),
        modal_shortcuts(&[
            ("Ctrl+Z/Y", "Undo/redo"),
            ("Up/Down", "History or chat scroll"),
        ]),
        modal_shortcuts(&[
            ("Ctrl+O", "Expand/collapse tool details"),
            ("Sessions + Delete", "Delete a selected session"),
        ]),
        Line::raw(""),
        modal_section("SLASH COMMANDS"),
    ];
    lines.extend(builtin_commands().into_iter().map(|command| {
        Line::from(vec![
            Span::styled(
                format!("{:<18}", command.usage),
                Style::default()
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(command.summary, Style::default().fg(MODAL_MUTED)),
        ])
    }));
    frame.render_widget(
        Paragraph::new(Text::from(lines))
            .wrap(Wrap { trim: false })
            .style(Style::default().fg(Color::Gray).bg(MODAL_BG)),
        body,
    );
}

fn render_error_modal(frame: &mut ratatui::Frame<'_>, popup: Rect, error: &str) {
    let body = render_modal_footer(
        frame,
        popup,
        vec![modal_shortcuts(&[("Enter/Esc", "Close")])],
    );
    frame.render_widget(
        Paragraph::new(Text::from(vec![
            modal_section("WHAT HAPPENED"),
            Line::raw(""),
            Line::styled(error.to_string(), Style::default().fg(Color::White)),
            Line::raw(""),
            Line::styled(
                "The conversation is still available after this dialog closes.",
                Style::default().fg(MODAL_MUTED),
            ),
        ]))
        .wrap(Wrap { trim: false })
        .style(Style::default().bg(MODAL_BG)),
        body,
    );
}

fn render_delete_conversation_modal(
    frame: &mut ratatui::Frame<'_>,
    popup: Rect,
    conversation: &Conversation,
) {
    let body = render_modal_footer(
        frame,
        popup,
        vec![modal_shortcuts(&[
            ("Y/Enter", "Delete permanently"),
            ("N/Esc", "Cancel"),
        ])],
    );
    let title = conversation
        .title
        .as_deref()
        .unwrap_or("Untitled conversation");
    frame.render_widget(
        Paragraph::new(Text::from(vec![
            modal_section("PERMANENT ACTION"),
            Line::raw(""),
            Line::styled(title.to_string(), Style::default().fg(Color::White)),
            Line::raw(""),
            Line::styled(
                "This removes its messages, runs, tool history, changes, and approvals.",
                Style::default().fg(MODAL_MUTED),
            ),
        ]))
        .wrap(Wrap { trim: false })
        .style(Style::default().bg(MODAL_BG)),
        body,
    );
}

fn render_approval_modal(frame: &mut ratatui::Frame<'_>, popup: Rect, approval: &Approval) {
    let arguments = serde_json::to_string_pretty(&approval.arguments)
        .unwrap_or_else(|_| approval.arguments.to_string());
    let body = render_modal_footer(
        frame,
        popup,
        vec![
            modal_section("ALLOW"),
            modal_shortcuts(&[
                ("Enter/Y", "Allow once"),
                ("R", "Allow for this run"),
                ("P", "Allow for this project"),
            ]),
            modal_shortcuts(&[
                ("a", "Always allow this command/pattern"),
                ("Shift+A", "Always allow all commands"),
            ]),
            modal_section("DENY OR MODIFY"),
            modal_shortcuts(&[
                ("N/Esc", "Deny once"),
                ("D", "Always deny this command/pattern"),
            ]),
            modal_shortcuts(&[("E", "Edit arguments before allowing once")]),
        ],
    );
    let reasons = if approval.reasons.is_empty() {
        "The active permission policy requires confirmation.".to_string()
    } else {
        approval.reasons.join("\n")
    };
    let mut lines = vec![
        Line::styled(
            "Review this action before the agent continues.",
            Style::default().fg(MODAL_MUTED),
        ),
        Line::raw(""),
        modal_section("TOOL"),
        Line::styled(
            approval.tool_name.clone(),
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        ),
        Line::raw(""),
        modal_section("ARGUMENTS"),
    ];
    lines.extend(
        arguments
            .lines()
            .map(|line| Line::styled(line.to_string(), Style::default().fg(Color::Gray))),
    );
    lines.extend([Line::raw(""), modal_section("WHY APPROVAL IS NEEDED")]);
    lines.extend(
        reasons
            .lines()
            .map(|line| Line::styled(line.to_string(), Style::default().fg(Color::Gray))),
    );
    frame.render_widget(
        Paragraph::new(Text::from(lines))
            .wrap(Wrap { trim: false })
            .style(Style::default().bg(MODAL_BG)),
        body,
    );
}

fn render_approval_editor_modal(
    frame: &mut ratatui::Frame<'_>,
    popup: Rect,
    approval: &Approval,
    editor: &PromptEditor,
) {
    let body = render_modal_footer(
        frame,
        popup,
        vec![modal_shortcuts(&[
            ("Ctrl+Enter", "Save and allow once"),
            ("Esc", "Back without changes"),
        ])],
    );
    let sections = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(3), Constraint::Min(1)])
        .split(body);
    frame.render_widget(
        Paragraph::new(Text::from(vec![
            modal_section("TOOL"),
            Line::styled(
                approval.tool_name.clone(),
                Style::default()
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD),
            ),
            Line::styled(
                "Edit the JSON arguments below.",
                Style::default().fg(MODAL_MUTED),
            ),
        ]))
        .style(Style::default().bg(MODAL_BG)),
        sections[0],
    );
    frame.render_widget(
        Paragraph::new(editor_text(editor, true))
            .wrap(Wrap { trim: false })
            .block(
                Block::default()
                    .title(Line::from(Span::styled(
                        " JSON ",
                        Style::default()
                            .fg(MODAL_MUTED)
                            .add_modifier(Modifier::BOLD),
                    )))
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(Color::DarkGray))
                    .style(Style::default().bg(MODAL_INSET_BG)),
            ),
        sections[1],
    );
}

fn render_picker_modal(frame: &mut ratatui::Frame<'_>, popup: Rect, picker: &PickerState) {
    let body = render_modal_footer(
        frame,
        popup,
        vec![modal_shortcuts(&[
            ("Up/Down", "Move"),
            ("Enter", "Choose"),
            ("Esc", "Close"),
        ])],
    );
    let sections = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(3), Constraint::Min(1)])
        .split(body);
    let query = picker.query.to_ascii_lowercase();
    let matching = picker
        .options
        .iter()
        .filter(|option| {
            query.is_empty()
                || option.label.to_ascii_lowercase().contains(&query)
                || option
                    .auxiliary
                    .as_deref()
                    .is_some_and(|value| value.to_ascii_lowercase().contains(&query))
        })
        .collect::<Vec<_>>();
    let selected = picker.selected.min(matching.len().saturating_sub(1));
    frame.render_widget(
        Paragraph::new(Text::from(vec![
            Line::from(vec![
                Span::styled(
                    "SEARCH  ",
                    Style::default()
                        .fg(MODAL_MUTED)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    if picker.query.is_empty() {
                        "Type to filter..."
                    } else {
                        picker.query.as_str()
                    },
                    Style::default().fg(if picker.query.is_empty() {
                        Color::DarkGray
                    } else {
                        Color::White
                    }),
                ),
            ]),
            Line::styled(
                format!(
                    "{} option{}",
                    matching.len(),
                    if matching.len() == 1 { "" } else { "s" }
                ),
                Style::default().fg(Color::DarkGray),
            ),
        ]))
        .style(Style::default().bg(MODAL_BG)),
        sections[0],
    );

    let visible = usize::from(sections[1].height.saturating_sub(1)).max(1);
    let start = selected.saturating_sub(visible.saturating_sub(1));
    let items = matching
        .iter()
        .enumerate()
        .skip(start)
        .take(visible)
        .map(|(index, option)| {
            let marker = if index == selected { ">" } else { " " };
            let suffix = option
                .auxiliary
                .as_deref()
                .map(|value| format!("  {value}"))
                .unwrap_or_default();
            ListItem::new(format!("{marker} {}{suffix}", option.label)).style(
                if index == selected {
                    Style::default()
                        .fg(Color::Black)
                        .bg(MODAL_SELECTION)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(Color::Gray).bg(MODAL_BG)
                },
            )
        })
        .collect::<Vec<_>>();
    frame.render_widget(
        List::new(items)
            .block(
                Block::default()
                    .borders(Borders::TOP)
                    .border_style(Style::default().fg(Color::DarkGray)),
            )
            .style(Style::default().bg(MODAL_BG)),
        sections[1],
    );
}

fn render_setup_modal(frame: &mut ratatui::Frame<'_>, popup: Rect, setup: &SetupState) {
    let template = &PROVIDER_TEMPLATES[setup.template];
    let credential = if setup.credential_mode == CredentialMode::ApiKey {
        "*".repeat(setup.credential.chars().count())
    } else {
        setup.credential.clone()
    };
    let credential_label = if setup.credential_mode == CredentialMode::ApiKey {
        "API key (saved securely)"
    } else {
        "Environment variable (saved)"
    };
    let fields = [
        (credential_label, credential),
        ("Model", setup.model.clone()),
        ("Base URL", setup.base_url.clone()),
    ];
    let body = render_modal_footer(
        frame,
        popup,
        vec![
            modal_shortcuts(&[
                ("Tab/Enter", "Next field"),
                ("Shift+Tab", "Previous"),
                ("F2", "Provider"),
            ]),
            modal_shortcuts(&[
                ("Ctrl+E", "API key/environment"),
                ("Ctrl+Enter", "Test and connect"),
            ]),
        ],
    );
    let mut lines = vec![
        Line::styled(
            "Connect directly without editing configuration files.",
            Style::default().fg(MODAL_MUTED),
        ),
        Line::raw(""),
        modal_section("PROVIDER"),
        Line::from(vec![
            Span::styled("<  ", Style::default().fg(MODAL_MUTED)),
            Span::styled(
                format!(
                    "{}  {}/{}",
                    template.name,
                    setup.template + 1,
                    PROVIDER_TEMPLATES.len()
                ),
                Style::default()
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled("  >", Style::default().fg(MODAL_MUTED)),
        ]),
        Line::styled(
            match setup.credential_mode {
                CredentialMode::ApiKey => "Authentication: API key",
                CredentialMode::Environment => "Authentication: environment variable",
            },
            Style::default().fg(Color::DarkGray),
        ),
        Line::raw(""),
    ];
    for (index, (label, value)) in fields.iter().enumerate() {
        let marker = if setup.field == index { ">" } else { " " };
        let value = if setup.field == index {
            format!("{value}|")
        } else {
            value.clone()
        };
        let field_style = if setup.field == index {
            Style::default()
                .fg(Color::Black)
                .bg(MODAL_SELECTION)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::Gray).bg(MODAL_BG)
        };
        lines.push(Line::styled(format!("{marker} {label}"), field_style));
        lines.push(Line::styled(format!("  {value}"), field_style));
    }
    lines.extend([
        Line::raw(""),
        Line::styled(
            if setup.submitting {
                "Testing provider connection..."
            } else if setup.credential_mode == CredentialMode::ApiKey {
                "Raw keys stay in memory and are never displayed."
            } else {
                "Only the environment-variable name is stored, never its value."
            },
            Style::default().fg(MODAL_MUTED),
        ),
    ]);
    frame.render_widget(
        Paragraph::new(Text::from(lines))
            .wrap(Wrap { trim: false })
            .style(Style::default().bg(MODAL_BG)),
        body,
    );
}

fn overlay_rect(area: Rect, overlay: &Overlay) -> Rect {
    let available_width = if area.width > 4 {
        area.width.saturating_sub(4)
    } else {
        area.width
    };
    let available_height = if area.height > 2 {
        area.height.saturating_sub(2)
    } else {
        area.height
    };
    let bounded = |desired: u16, minimum: u16, available: u16| {
        desired.min(available).max(minimum.min(available))
    };
    let (width, height) = match overlay {
        Overlay::Help => (
            bounded(96, 56, available_width),
            bounded(36, 18, available_height),
        ),
        Overlay::Error(error) => {
            let longest = error
                .lines()
                .map(str::chars)
                .map(Iterator::count)
                .max()
                .unwrap_or(0);
            let width = u16::try_from(longest.saturating_add(8)).unwrap_or(u16::MAX);
            let lines = u16::try_from(error.lines().count()).unwrap_or(u16::MAX);
            (
                bounded(width, 48, available_width),
                bounded(lines.saturating_add(10), 12, available_height),
            )
        }
        Overlay::DeleteConversation(_) => (
            bounded(72, 46, available_width),
            bounded(16, 12, available_height),
        ),
        Overlay::Setup(_) => (
            bounded(82, 52, available_width),
            bounded(24, 20, available_height),
        ),
        Overlay::Approval(approval) => {
            let arguments = serde_json::to_string_pretty(&approval.arguments)
                .unwrap_or_else(|_| approval.arguments.to_string());
            let content_lines = arguments
                .lines()
                .count()
                .saturating_add(approval.reasons.len());
            let height = u16::try_from(content_lines.saturating_add(22)).unwrap_or(u16::MAX);
            (
                bounded(90, 58, available_width),
                bounded(height, 24, available_height),
            )
        }
        Overlay::ApprovalEditor { editor, .. } => {
            let lines = u16::try_from(editor.text.lines().count()).unwrap_or(u16::MAX);
            (
                bounded(90, 58, available_width),
                bounded(lines.saturating_add(11), 18, available_height),
            )
        }
        Overlay::Picker(picker) => {
            let height = u16::try_from(picker.options.len().saturating_add(9)).unwrap_or(u16::MAX);
            (
                bounded(72, 46, available_width),
                bounded(height, 12, available_height),
            )
        }
    };
    centered_fixed_rect(width, height, area)
}

fn render_modal_surface(
    frame: &mut ratatui::Frame<'_>,
    area: Rect,
    popup: Rect,
    title: &str,
    accent: Color,
) {
    frame.render_widget(
        Block::default().style(
            Style::default()
                .fg(Color::Rgb(70, 70, 70))
                .add_modifier(Modifier::DIM),
        ),
        area,
    );
    let shadow_x = popup.x.saturating_add(2).min(area.right());
    let shadow_y = popup.y.saturating_add(1).min(area.bottom());
    let shadow = Rect::new(
        shadow_x,
        shadow_y,
        popup.width.min(area.right().saturating_sub(shadow_x)),
        popup.height.min(area.bottom().saturating_sub(shadow_y)),
    );
    if shadow.width > 0 && shadow.height > 0 {
        frame.render_widget(Clear, shadow);
        frame.render_widget(
            Block::default().style(Style::default().bg(MODAL_SHADOW)),
            shadow,
        );
    }
    frame.render_widget(Clear, popup);
    frame.render_widget(
        Block::default()
            .title(Line::from(Span::styled(
                format!(" {title} "),
                Style::default()
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD),
            )))
            .borders(Borders::ALL)
            .border_style(Style::default().fg(accent))
            .style(Style::default().fg(Color::Gray).bg(MODAL_BG)),
        popup,
    );
}

fn modal_inner(area: Rect) -> Rect {
    Rect::new(
        area.x.saturating_add(2),
        area.y.saturating_add(2),
        area.width.saturating_sub(4),
        area.height.saturating_sub(4),
    )
}

fn render_modal_footer(
    frame: &mut ratatui::Frame<'_>,
    popup: Rect,
    lines: Vec<Line<'static>>,
) -> Rect {
    let inner = modal_inner(popup);
    let content_width = usize::from(inner.width.max(1));
    let visual_lines = lines.iter().fold(0_usize, |total, line| {
        total.saturating_add(line.width().max(1).div_ceil(content_width))
    });
    let line_count = u16::try_from(visual_lines).unwrap_or(u16::MAX);
    let footer_height = line_count.saturating_add(1).min(inner.height);
    let footer = Rect::new(
        inner.x,
        inner.bottom().saturating_sub(footer_height),
        inner.width,
        footer_height,
    );
    if footer.height > 0 {
        frame.render_widget(
            Paragraph::new(Text::from(lines))
                .block(
                    Block::default()
                        .borders(Borders::TOP)
                        .border_style(Style::default().fg(Color::DarkGray)),
                )
                .wrap(Wrap { trim: true })
                .style(Style::default().bg(MODAL_BG)),
            footer,
        );
    }
    Rect::new(
        inner.x,
        inner.y,
        inner.width,
        inner.height.saturating_sub(footer_height),
    )
}

fn modal_section(label: &str) -> Line<'static> {
    Line::styled(
        label.to_string(),
        Style::default()
            .fg(MODAL_MUTED)
            .add_modifier(Modifier::BOLD),
    )
}

fn modal_shortcuts(items: &[(&str, &str)]) -> Line<'static> {
    let mut spans = Vec::with_capacity(items.len().saturating_mul(3));
    for (index, (key, label)) in items.iter().enumerate() {
        if index > 0 {
            spans.push(Span::styled("   ", Style::default().bg(MODAL_BG)));
        }
        spans.push(Span::styled(
            format!("[{key}]"),
            Style::default()
                .fg(Color::White)
                .bg(MODAL_INSET_BG)
                .add_modifier(Modifier::BOLD),
        ));
        spans.push(Span::styled(
            format!(" {label}"),
            Style::default().fg(MODAL_MUTED).bg(MODAL_BG),
        ));
    }
    Line::from(spans)
}

#[allow(dead_code)]
fn render_overlay_legacy(frame: &mut ratatui::Frame<'_>, area: Rect, overlay: &Overlay) {
    let popup = if let Overlay::Picker(picker) = overlay {
        let height = u16::try_from(picker.options.len().saturating_add(5))
            .unwrap_or(u16::MAX)
            .min(area.height.saturating_sub(4))
            .max(8);
        centered_fixed_rect(area.width.saturating_sub(4).min(64), height, area)
    } else {
        centered_rect(78, 74, area)
    };
    frame.render_widget(Clear, popup);
    match overlay {
        Overlay::Help => {
            let commands = builtin_commands()
                .into_iter()
                .map(|command| format!("{:<16} {}", command.usage, command.summary))
                .collect::<Vec<_>>()
                .join("\n");
            frame.render_widget(
                Paragraph::new(format!(
                    "Keyboard\n\
                     Ctrl+Enter send | Enter newline | Ctrl+C cancel / twice quit\n\
                     Ctrl+N new session | Ctrl+P/F1 help | Ctrl+K command palette\n\
                     Ctrl+Left/Right views\n\
                     Ctrl+M model | Ctrl+A agent | Ctrl+S sessions | Ctrl+D changes\n\
                     Ctrl+T terminal | Ctrl+L logs | Ctrl+Z/Y editor undo/redo\n\n\
                     Shift+Arrows select | Ctrl+Shift+C copy selection/latest response\n\
                     Ctrl+E external editor\n\n\
                     Connected slash commands\n{commands}"
                ))
                .wrap(Wrap { trim: false })
                .block(Block::default().title(" Help ").borders(Borders::ALL)),
                popup,
            );
        }
        Overlay::Error(error) => {
            frame.render_widget(
                Paragraph::new(error.as_str())
                    .wrap(Wrap { trim: false })
                    .block(
                        Block::default()
                            .title(" Error - Enter/Esc close ")
                            .borders(Borders::ALL)
                            .border_style(Style::default().fg(Color::Red)),
                    ),
                popup,
            );
        }
        Overlay::DeleteConversation(conversation) => {
            frame.render_widget(
                Paragraph::new(format!(
                    "Delete `{}` permanently?\n\nThis removes its messages, runs, tool history, changes, and approvals.\n\ny/Enter delete | n/Esc cancel",
                    conversation.title.as_deref().unwrap_or("Untitled conversation")
                ))
                .wrap(Wrap { trim: false })
                .block(Block::default().title(" Delete conversation ").borders(Borders::ALL)),
                popup,
            );
        }
        Overlay::Setup(setup) => render_setup(frame, popup, setup),
        Overlay::Approval(approval) => {
            let arguments = serde_json::to_string_pretty(&approval.arguments)
                .unwrap_or_else(|_| approval.arguments.to_string());
            frame.render_widget(
                Paragraph::new(format!(
                    "{}\n\nTool: {}\nArguments:\n{}\n\nReasons: {}\n\n\
                     y/Enter allow once | r run | p project | a always\n\
                     n/Esc deny once | d always deny | e edit arguments",
                    "This action needs your approval.",
                    approval.tool_name,
                    arguments,
                    approval.reasons.join("; ")
                ))
                .wrap(Wrap { trim: false })
                .block(
                    Block::default()
                        .title(" Approval required ")
                        .borders(Borders::ALL)
                        .border_style(Style::default().fg(Color::Gray)),
                ),
                popup,
            );
        }
        Overlay::ApprovalEditor { approval, editor } => {
            frame.render_widget(
                Paragraph::new(editor_text(editor, true))
                    .wrap(Wrap { trim: false })
                    .block(
                        Block::default()
                            .title(format!(
                                " Edit {} arguments - Ctrl+Enter allow / Esc back ",
                                approval.tool_name
                            ))
                            .borders(Borders::ALL)
                            .border_style(Style::default().fg(Color::Gray)),
                    ),
                popup,
            );
        }
        Overlay::Picker(picker) => {
            let title_width = picker.title.trim().chars().count();
            let title_gap = usize::from(popup.width)
                .saturating_sub(title_width)
                .saturating_sub(7);
            let mut items = vec![
                ListItem::new(Line::from(vec![
                    Span::styled(
                        format!(" {} ", picker.title.trim()),
                        Style::default()
                            .fg(Color::White)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::raw(" ".repeat(title_gap)),
                    Span::styled("esc", Style::default().fg(Color::DarkGray)),
                ])),
                ListItem::new(Line::from(vec![
                    Span::styled(" Search  ", Style::default().fg(MENU_ACCENT)),
                    Span::styled(
                        if picker.query.is_empty() {
                            "type to filter"
                        } else {
                            picker.query.as_str()
                        },
                        Style::default().fg(Color::DarkGray),
                    ),
                ])),
                ListItem::new(Line::styled(
                    " Recent",
                    Style::default().fg(Color::DarkGray),
                )),
            ];
            items.extend(
                picker
                    .options
                    .iter()
                    .filter(|option| {
                        let query = picker.query.to_ascii_lowercase();
                        query.is_empty()
                            || option.label.to_ascii_lowercase().contains(&query)
                            || option
                                .auxiliary
                                .as_deref()
                                .is_some_and(|value| value.to_ascii_lowercase().contains(&query))
                    })
                    .enumerate()
                    .map(|(index, option)| {
                        let marker = if index == picker.selected { "●" } else { " " };
                        let suffix = option
                            .auxiliary
                            .as_deref()
                            .map(|value| format!("  {value}"))
                            .unwrap_or_default();
                        ListItem::new(format!("{marker} {}{suffix}", option.label)).style(
                            if index == picker.selected {
                                Style::default()
                                    .fg(Color::Black)
                                    .bg(MENU_ACCENT)
                                    .add_modifier(Modifier::BOLD)
                            } else {
                                Style::default().fg(Color::Gray)
                            },
                        )
                    })
                    .collect::<Vec<_>>(),
            );
            frame.render_widget(List::new(items).style(Style::default().bg(PANEL_BG)), popup);
        }
    }
}

fn centered_fixed_rect(width: u16, height: u16, area: Rect) -> Rect {
    Rect::new(
        area.x.saturating_add(area.width.saturating_sub(width) / 2),
        area.y
            .saturating_add(area.height.saturating_sub(height) / 2),
        width,
        height,
    )
}

fn render_setup(frame: &mut ratatui::Frame<'_>, area: Rect, setup: &SetupState) {
    let template = &PROVIDER_TEMPLATES[setup.template];
    let credential = if setup.credential_mode == CredentialMode::ApiKey {
        "•".repeat(setup.credential.chars().count())
    } else {
        setup.credential.clone()
    };
    let credential_label = if setup.credential_mode == CredentialMode::ApiKey {
        "API key (saved securely)"
    } else {
        "Environment variable (saved)"
    };
    let fields = [
        (credential_label, credential),
        ("Model", setup.model.clone()),
        ("Base URL", setup.base_url.clone()),
    ];
    let mut lines = vec![
        Line::styled(
            "No provider is configured. Connect one without editing JSON.",
            Style::default().add_modifier(Modifier::BOLD),
        ),
        Line::raw(""),
        Line::from(vec![
            Span::raw("Provider  "),
            Span::styled(
                format!("◀ {} ▶", template.name),
                Style::default().fg(Color::White),
            ),
            Span::raw("   F2 cycles"),
        ]),
        Line::raw("Ctrl+E toggles API-key/environment authentication."),
        Line::raw(""),
    ];
    for (index, (label, value)) in fields.iter().enumerate() {
        let marker = if setup.field == index { ">" } else { " " };
        let value = if setup.field == index {
            format!("{value}█")
        } else {
            value.clone()
        };
        lines.push(Line::styled(
            format!("{marker} {label}: {value}"),
            Style::default().fg(if setup.field == index {
                Color::Gray
            } else {
                Color::White
            }),
        ));
    }
    lines.extend([
        Line::raw(""),
        Line::raw("Tab/Enter next field · Ctrl+Enter test and connect"),
        Line::raw(if setup.submitting {
            "Testing provider connection…"
        } else if setup.credential_mode == CredentialMode::ApiKey {
            "Raw keys are held in memory and never displayed; use environment mode to persist."
        } else {
            "The config stores only the environment-variable name, never its value."
        }),
    ]);
    frame.render_widget(
        Paragraph::new(Text::from(lines))
            .wrap(Wrap { trim: false })
            .block(
                Block::default()
                    .title(" First-run provider setup ")
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(Color::White)),
            ),
        area,
    );
}

fn centered_rect(percent_x: u16, percent_y: u16, area: Rect) -> Rect {
    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - percent_y) / 2),
            Constraint::Percentage(percent_y),
            Constraint::Percentage((100 - percent_y) / 2),
        ])
        .split(area);
    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(vertical[1])[1]
}

struct TerminalSession {
    terminal: Terminal<CrosstermBackend<Stdout>>,
}

impl TerminalSession {
    fn enter() -> Result<Self> {
        enable_raw_mode()?;
        let mut stdout = io::stdout();
        execute!(
            stdout,
            EnterAlternateScreen,
            EnableBracketedPaste,
            EnableMouseCapture
        )?;
        let backend = CrosstermBackend::new(stdout);
        let terminal = Terminal::new(backend)?;
        Ok(Self { terminal })
    }

    fn suspend(&mut self) -> Result<()> {
        disable_raw_mode()?;
        execute!(
            self.terminal.backend_mut(),
            DisableBracketedPaste,
            DisableMouseCapture,
            LeaveAlternateScreen
        )?;
        self.terminal.show_cursor()?;
        Ok(())
    }

    fn resume(&mut self) -> Result<()> {
        enable_raw_mode()?;
        execute!(
            self.terminal.backend_mut(),
            EnterAlternateScreen,
            EnableBracketedPaste,
            EnableMouseCapture
        )?;
        self.terminal.clear()?;
        Ok(())
    }
}

impl Drop for TerminalSession {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(
            self.terminal.backend_mut(),
            DisableBracketedPaste,
            DisableMouseCapture,
            LeaveAlternateScreen
        );
        let _ = self.terminal.show_cursor();
    }
}

#[cfg(test)]
mod tests {
    use super::{
        App, AttachmentKind, ClientEvent, ModelCapabilities, ModelDescriptor,
        ModelTaskRequirements, Overlay, PROVIDER_TEMPLATES, PickerKind, PickerOption, PickerState,
        PromptEditor, SetupState, approval_decision_for_key, attachment_line, capture_editor_drop,
        chat_error_message, command_suggestions, complete_editor, composer_status_line,
        cube_loader_line, dropped_files, editor_text, friendly_chat_error, handle_key,
        handle_slash_command, latest_copyable_response, load_snapshot, model_matches_task,
        model_task_requirements, reasoning_levels, render, render_content_block,
        render_content_block_with_details, render_overlay, runtime_trace_lines,
        submit_conversation_selection, submit_prompt,
    };
    use async_trait::async_trait;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use opensrc_core::{
        ApprovalDecision, CanonicalModelRequest, Event, Message, MessageContent, MessageRole,
        ModelEvent, ProviderAdapter, ProviderCapabilities, ProviderError, builtin_commands,
    };
    use opensrc_runtime::{AgentLimits, ProviderRouter, Runtime, SkillRegistry, ToolExecutor};
    use opensrc_server::ServerState;
    use opensrc_store::Store;
    use ratatui::{Terminal, backend::TestBackend, style::Color};
    use std::path::Path;
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};
    use tokio::sync::mpsc;

    struct StreamingFixture {
        requests: Arc<Mutex<Vec<CanonicalModelRequest>>>,
    }

    fn rendered_terminal(terminal: &Terminal<TestBackend>) -> String {
        terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect::<String>()
    }

    #[test]
    fn provider_setup_debug_output_redacts_the_credential() {
        const SENTINEL_SECRET: &str = "sk-opensource-sentinel-never-log";
        let setup = SetupState {
            credential: SENTINEL_SECRET.to_string(),
            ..SetupState::default()
        };

        let debug = format!("{setup:?}");
        assert!(!debug.contains(SENTINEL_SECRET));
        assert!(debug.contains("[REDACTED]"));
    }

    #[test]
    fn model_picker_capabilities_exclude_text_only_models_from_image_tasks() {
        let text_only = ModelDescriptor {
            provider: "groq".to_string(),
            id: "llama-3.3-70b-versatile".to_string(),
            capabilities: ModelCapabilities {
                chat: true,
                tools: true,
                multimodal: false,
            },
        };
        let vision = ModelDescriptor {
            provider: "groq".to_string(),
            id: "qwen/qwen3.6-27b".to_string(),
            capabilities: ModelCapabilities {
                chat: true,
                tools: true,
                multimodal: true,
            },
        };
        let requirements = ModelTaskRequirements {
            vision: true,
            tools: false,
        };

        assert!(!model_matches_task(&text_only, requirements));
        assert!(model_matches_task(&vision, requirements));
    }

    #[test]
    fn image_to_code_followup_requires_both_vision_and_tools() {
        let conversation_id = uuid::Uuid::new_v4();
        let history = vec![Message {
            id: uuid::Uuid::new_v4(),
            conversation_id,
            run_id: None,
            sequence: 1,
            role: MessageRole::User,
            content: vec![
                MessageContent::text("Analyze this image."),
                MessageContent::FileReference {
                    path: "C:/fixtures/reference.png".to_string(),
                    mime_type: Some("image/png".to_string()),
                },
            ],
            provider: None,
            model: None,
            continuation_id: None,
            created_at: chrono::Utc::now(),
        }];
        let requirements = model_task_requirements("now code it", &[], &history, None);
        assert!(requirements.vision);
        assert!(requirements.tools);
    }

    #[test]
    fn structured_execution_error_is_shown_instead_of_raw_http_conflict() {
        let message = chat_error_message(
            reqwest::StatusCode::UNPROCESSABLE_ENTITY,
            Some(&serde_json::json!({
                "error": {
                    "code": "execution_incomplete",
                    "message": "unchanged failed tool call was suppressed"
                }
            })),
        );
        assert!(message.contains("unchanged failed tool call was suppressed"));
        assert!(message.contains("422 Unprocessable Entity"));
        assert!(!message.contains("/v1/chat"));
    }

    #[test]
    fn copy_uses_streaming_or_latest_assistant_text() {
        let conversation_id = uuid::Uuid::new_v4();
        let message = |role, text: &str| Message {
            id: uuid::Uuid::new_v4(),
            conversation_id,
            run_id: None,
            sequence: 1,
            role,
            content: vec![MessageContent::text(text)],
            provider: None,
            model: None,
            continuation_id: None,
            created_at: chrono::Utc::now(),
        };
        let messages = vec![
            message(MessageRole::Assistant, "older"),
            message(MessageRole::User, "question"),
            message(MessageRole::Assistant, "latest"),
        ];

        assert_eq!(
            latest_copyable_response(&messages, "").as_deref(),
            Some("latest")
        );
        assert_eq!(
            latest_copyable_response(&messages, "streaming").as_deref(),
            Some("streaming")
        );
    }

    #[test]
    fn expanded_tool_details_show_metadata_without_embedded_media() {
        let lines = render_content_block_with_details(
            &MessageContent::ToolResult {
                provider_call_id: "provider-call".to_string(),
                canonical_call_id: "canonical-call".to_string(),
                name: "fs.view_image".to_string(),
                result: serde_json::json!({
                    "output": {
                        "path": "reference.png",
                        "mime_type": "image/png",
                        "data_url": "data:image/png;base64,secret"
                    }
                }),
                timing_ms: Some(1),
                approval_state: None,
            },
            true,
        );
        let rendered = lines
            .iter()
            .flat_map(|line| line.spans.iter())
            .map(|span| span.content.as_ref())
            .collect::<String>();
        assert!(rendered.contains("reference.png"));
        assert!(!rendered.contains("data:image"));
    }

    #[async_trait]
    impl ProviderAdapter for StreamingFixture {
        fn id(&self) -> &'static str {
            "tui-fixture"
        }

        fn capabilities(&self) -> ProviderCapabilities {
            ProviderCapabilities {
                supports_streaming: true,
                ..ProviderCapabilities::default()
            }
        }

        async fn execute(
            &self,
            request: CanonicalModelRequest,
        ) -> Result<Vec<ModelEvent>, ProviderError> {
            self.requests.lock().expect("capture").push(request);
            Ok(vec![
                ModelEvent::TextDelta {
                    text: "streamed ".to_string(),
                },
                ModelEvent::TextDelta {
                    text: "answer".to_string(),
                },
                ModelEvent::Completed {
                    response_id: Some("tui-fixture-1".to_string()),
                },
            ])
        }
    }

    #[test]
    fn editor_supports_multiline_cursor_undo_and_redo() {
        let mut editor = PromptEditor::default();
        editor.insert_str("first");
        editor.insert_char('\n');
        editor.insert_str("second");
        assert_eq!(editor.text, "first\nsecond");
        editor.undo();
        assert_eq!(editor.text, "first\n");
        editor.redo();
        assert_eq!(editor.text, "first\nsecond");
        editor.move_vertical(-1);
        editor.insert_char('!');
        assert_eq!(editor.text, "first!\nsecond");
        editor.prepare_selection(true);
        editor.left();
        editor.left();
        assert_eq!(editor.selected_text(), Some("t!"));
        editor.insert_str("T");
        assert_eq!(editor.text, "firsT\nsecond");

        let mut app = App::new(Path::new("."));
        app.editor.insert_str("/pro");
        complete_editor(&mut app);
        assert_eq!(app.editor.text, "/providers ");
    }

    #[test]
    fn dropped_media_becomes_numbered_attachments_without_paths_in_editor() {
        let directory =
            std::env::temp_dir().join(format!("opensource-drop-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&directory).expect("drop directory");
        let image = directory.join("first image.png");
        let video_one = directory.join("first video.mp4");
        let video_two = directory.join("second video.webm");
        for path in [&image, &video_one, &video_two] {
            std::fs::write(path, b"fixture").expect("media fixture");
        }
        let dropped = dropped_files(&format!(
            "\"{}\" \"{}\" \"{}\"",
            image.display(),
            video_one.display(),
            video_two.display()
        ));
        assert_eq!(dropped.len(), 3);
        assert_eq!(dropped[0].kind, AttachmentKind::Image);
        assert_eq!(dropped[1].kind, AttachmentKind::Video);
        let rendered = attachment_line(&dropped)
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<String>();
        assert!(rendered.contains("Image 1"));
        assert!(rendered.contains("Video 1"));
        assert!(rendered.contains("Video 2"));
        assert!(!rendered.contains(&directory.to_string_lossy().into_owned()));
        std::fs::remove_dir_all(directory).expect("cleanup");
    }

    #[test]
    fn character_delivered_single_quoted_drop_is_captured() {
        let path =
            std::env::temp_dir().join(format!("opensource-key-drop-{}.mp4", uuid::Uuid::new_v4()));
        std::fs::write(&path, b"video").expect("fixture");
        let mut app = App::new(Path::new("."));
        app.editor.insert_str(&format!("& '{}'", path.display()));
        assert!(capture_editor_drop(&mut app));
        assert!(app.editor.text.is_empty());
        assert_eq!(app.attachments.len(), 1);
        assert_eq!(app.attachments[0].kind, AttachmentKind::Video);
        std::fs::remove_file(path).expect("cleanup");
    }

    #[test]
    fn every_connected_command_suggests_and_tab_completes() {
        let mut app = App::new(Path::new("."));
        app.editor.insert_str("/");
        assert!(command_suggestions(&app).len() >= builtin_commands().len());
        for command in builtin_commands() {
            for name in std::iter::once(command.name).chain(command.aliases.iter().copied()) {
                app.editor = PromptEditor::default();
                app.editor.insert_str(name);
                app.suggestion_index = 0;
                complete_editor(&mut app);
                assert_eq!(app.editor.text, format!("{name} "));
            }
        }
        app.editor = PromptEditor::default();
        app.editor.insert_str("/m");
        let suggestions = command_suggestions(&app);
        assert!(suggestions.len() > 1);
        let client = reqwest::Client::new();
        let (tx, _rx) = mpsc::unbounded_channel();
        handle_key(
            &mut app,
            KeyEvent::new(KeyCode::Down, KeyModifiers::NONE),
            &client,
            "http://127.0.0.1:1",
            &tx,
        );
        assert_eq!(app.suggestion_index, 1);
        handle_key(
            &mut app,
            KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE),
            &client,
            "http://127.0.0.1:1",
            &tx,
        );
        assert!(app.editor.text.starts_with(&suggestions[1].value));
    }

    #[test]
    fn empty_prompt_arrows_and_page_keys_scroll_chat() {
        let mut app = App::new(Path::new("."));
        let client = reqwest::Client::new();
        let (tx, _rx) = mpsc::unbounded_channel();
        handle_key(
            &mut app,
            KeyEvent::new(KeyCode::Up, KeyModifiers::NONE),
            &client,
            "http://127.0.0.1:1",
            &tx,
        );
        assert_eq!(app.chat_scroll_offset, 3);
        handle_key(
            &mut app,
            KeyEvent::new(KeyCode::PageUp, KeyModifiers::NONE),
            &client,
            "http://127.0.0.1:1",
            &tx,
        );
        assert_eq!(app.chat_scroll_offset, 13);
        handle_key(
            &mut app,
            KeyEvent::new(KeyCode::Down, KeyModifiers::NONE),
            &client,
            "http://127.0.0.1:1",
            &tx,
        );
        assert_eq!(app.chat_scroll_offset, 10);
        handle_key(
            &mut app,
            KeyEvent::new(KeyCode::End, KeyModifiers::CONTROL),
            &client,
            "http://127.0.0.1:1",
            &tx,
        );
        assert_eq!(app.chat_scroll_offset, 0);
    }

    #[test]
    fn command_arguments_are_suggested_and_completed() {
        let mut app = App::new(Path::new("."));
        app.model = Some("gemini-3.1-pro-preview".to_string());
        app.editor.insert_str("/reasoning ");
        let suggestions = command_suggestions(&app);
        assert_eq!(
            suggestions
                .iter()
                .map(|option| option.label.as_str())
                .collect::<Vec<_>>(),
            vec!["high", "low", "medium"]
        );
        assert!(!suggestions.iter().any(|option| option.label == "minimal"));

        app.editor = PromptEditor::default();
        app.editor.insert_str("/mode f");
        let suggestions = command_suggestions(&app);
        assert_eq!(suggestions.len(), 1);
        assert_eq!(suggestions[0].value, "/mode focused");
        complete_editor(&mut app);
        assert_eq!(app.editor.text, "/mode focused");
    }

    #[test]
    fn model_pack_commands_select_and_describe_a_real_pack() {
        let mut app = App::new(Path::new("."));
        app.model_packs = vec![
            serde_json::from_value(serde_json::json!({
                "id": "efficient-trio",
                "name": "Efficient Trio",
                "description": "Planner, builder, and verifier specialists.",
                "strategy": "cost_optimized",
                "members": [
                    {"provider":"go","model":"glm","roles":["architect"],"stages":["plan"]},
                    {"provider":"go","model":"kimi","roles":["implementer"],"stages":["execute"]},
                    {"provider":"go","model":"deepseek","roles":["code-reviewer"],"stages":["validate"]}
                ],
                "generated": true,
                "available": true,
                "missing_providers": []
            }))
            .expect("model pack"),
        ];
        app.editor.insert_str("/pack e");
        let suggestions = command_suggestions(&app);
        assert_eq!(suggestions.len(), 1);
        assert_eq!(suggestions[0].value, "/pack efficient-trio");

        let client = reqwest::Client::new();
        let (tx, _rx) = mpsc::unbounded_channel();
        assert!(handle_slash_command(
            &mut app,
            "/pack efficient-trio",
            &client,
            "http://127.0.0.1:1",
            &tx
        ));
        assert_eq!(app.model_pack.as_deref(), Some("efficient-trio"));
        let status = composer_status_line(&app)
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<String>();
        assert!(status.contains("Efficient Trio"));
        assert!(status.contains("3 models"));
    }

    #[test]
    fn semantic_runtime_trace_updates_one_tool_line_without_reasoning_text() {
        let conversation_id = uuid::Uuid::new_v4();
        let run_id = uuid::Uuid::new_v4();
        let event = |id, kind: &str, payload| Event {
            id,
            conversation_id,
            run_id: Some(run_id),
            agent_id: None,
            task_id: None,
            kind: kind.to_string(),
            payload,
            idempotency_key: None,
            created_at: chrono::Utc::now(),
        };
        let mut app = App::new(Path::new("."));
        app.busy = true;
        app.apply_domain_event(&event(
            1,
            "model.event",
            serde_json::json!({
                "event": ModelEvent::ToolCall {
                    id: "call-1".to_string(),
                    name: "fs.read".to_string(),
                    arguments: serde_json::json!({
                        "path": "src/main.rs",
                        "private_reasoning": "must never render"
                    })
                }
            }),
        ));
        app.apply_domain_event(&event(
            2,
            "tool.completed",
            serde_json::json!({"call_id":"call-1","name":"fs.read"}),
        ));
        assert_eq!(app.runtime_trace.len(), 1);
        let rendered = runtime_trace_lines(&app)
            .iter()
            .flat_map(|line| line.spans.iter())
            .map(|span| span.content.as_ref())
            .collect::<String>();
        assert!(rendered.contains("tool"));
        assert!(rendered.contains("fs.read"));
        assert!(rendered.contains("src/main.rs"));
        assert!(rendered.contains("done"));
        assert!(!rendered.contains("private_reasoning"));
        assert!(!rendered.contains("must never render"));
    }

    #[test]
    fn routing_trace_replaces_failed_model_and_remains_visible() {
        let conversation_id = uuid::Uuid::new_v4();
        let run_id = uuid::Uuid::new_v4();
        let agent_id = uuid::Uuid::new_v4();
        let event = |id, kind: &str, payload| Event {
            id,
            conversation_id,
            run_id: Some(run_id),
            agent_id: Some(agent_id),
            task_id: None,
            kind: kind.to_string(),
            payload,
            idempotency_key: None,
            created_at: chrono::Utc::now(),
        };
        let mut app = App::new(Path::new("."));
        app.busy = true;
        app.apply_domain_event(&event(
            1,
            "routing.policy_selected",
            serde_json::json!({
                "role": "implementer",
                "reason": "role policy",
                "provider": "openrouter",
                "model": "kimi-primary"
            }),
        ));
        app.apply_domain_event(&event(
            2,
            "routing.model_pinned",
            serde_json::json!({
                "role": "implementer",
                "provider": "openrouter",
                "model": "kimi-primary"
            }),
        ));
        app.apply_domain_event(&event(
            3,
            "provider.fallback_selected",
            serde_json::json!({
                "failed_provider": "openrouter",
                "failed_model": "kimi-primary",
                "next_provider": "openrouter",
                "next_model": "kimi-fallback"
            }),
        ));
        app.apply_domain_event(&event(
            4,
            "agent.route_changed",
            serde_json::json!({
                "from_provider": "openrouter",
                "from_model": "kimi-primary",
                "to_provider": "openrouter",
                "to_model": "kimi-fallback"
            }),
        ));
        app.apply_domain_event(&event(
            5,
            "routing.model_transition",
            serde_json::json!({
                "from_provider": "openrouter",
                "from_model": "kimi-primary",
                "to_provider": "openrouter",
                "to_model": "kimi-fallback",
                "reason": "fallback",
                "pinned_for_remaining_agent_cycles": true
            }),
        ));

        assert_eq!(app.runtime_trace.len(), 1);
        assert_eq!(
            app.runtime_trace[0].target.as_deref(),
            Some("openrouter/kimi-primary -> openrouter/kimi-fallback")
        );
        assert_eq!(app.runtime_trace[0].status, "fallback active (pinned)");
        app.busy = false;
        let rendered = runtime_trace_lines(&app)
            .iter()
            .flat_map(|line| line.spans.iter())
            .map(|span| span.content.as_ref())
            .collect::<String>();
        assert!(rendered.contains("implementer"));
        assert!(rendered.contains("openrouter/kimi-primary"));
        assert!(rendered.contains("openrouter/kimi-fallback"));
        assert!(rendered.contains("fallback active (pinned)"));
    }

    #[tokio::test]
    async fn model_pack_is_sent_in_chat_and_conversation_selection_payloads() {
        let store = Store::in_memory().expect("store");
        let conversation = store
            .create_conversation(".", Some("pack payload".to_string()))
            .expect("conversation");
        let chat_payload = Arc::new(Mutex::new(None::<serde_json::Value>));
        let selection_payload = Arc::new(Mutex::new(None::<serde_json::Value>));
        let chat_capture = chat_payload.clone();
        let selection_capture = selection_payload.clone();
        let selected_conversation = conversation.clone();
        let router = axum::Router::new()
            .route(
                "/v1/chat",
                axum::routing::post(move |axum::Json(payload): axum::Json<serde_json::Value>| {
                    *chat_capture.lock().expect("chat capture") = Some(payload);
                    async { axum::http::StatusCode::OK }
                }),
            )
            .route(
                "/v1/conversations/{id}/selection",
                axum::routing::post(move |axum::Json(payload): axum::Json<serde_json::Value>| {
                    *selection_capture.lock().expect("selection capture") = Some(payload);
                    let conversation = selected_conversation.clone();
                    async move { axum::Json(conversation) }
                }),
            );
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("listener");
        let address = listener.local_addr().expect("address");
        let server = tokio::spawn(async move {
            axum::serve(listener, router).await.expect("server");
        });
        let base_url = format!("http://{address}");
        let client = reqwest::Client::new();
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut app = App::new(Path::new("."));
        app.conversation = Some(conversation);
        app.provider = Some("go".to_string());
        app.model = Some("fallback".to_string());
        app.model_pack = Some("efficient-trio".to_string());

        submit_conversation_selection(&app, &client, &base_url, &tx);
        assert!(matches!(
            tokio::time::timeout(Duration::from_secs(2), rx.recv())
                .await
                .expect("selection timeout"),
            Some(ClientEvent::SelectionUpdated(_))
        ));
        app.editor.insert_str("use the selected pack");
        submit_prompt(&mut app, &client, &base_url, &tx);
        assert!(matches!(
            tokio::time::timeout(Duration::from_secs(2), rx.recv())
                .await
                .expect("chat timeout"),
            Some(ClientEvent::ChatFinished)
        ));

        assert_eq!(
            selection_payload
                .lock()
                .expect("selection")
                .as_ref()
                .expect("selection payload")["model_pack"],
            "efficient-trio"
        );
        assert_eq!(
            chat_payload
                .lock()
                .expect("chat")
                .as_ref()
                .expect("chat payload")["model_pack"],
            "efficient-trio"
        );
        assert!(
            chat_payload
                .lock()
                .expect("chat")
                .as_ref()
                .expect("chat payload")
                .get("model_override")
                .is_none()
        );
        server.abort();
    }

    #[test]
    fn openrouter_setup_uses_the_native_compatible_endpoint() {
        let template = PROVIDER_TEMPLATES
            .iter()
            .find(|template| template.id == "openrouter")
            .expect("OpenRouter template");
        assert_eq!(template.protocol, "openai_compatible");
        assert_eq!(template.family, Some("openrouter"));
        assert_eq!(template.base_url, "https://openrouter.ai/api/v1");
        assert_eq!(template.key_env, "OPENROUTER_API_KEY");
        assert!(!template.model.is_empty());
        assert_eq!(
            PROVIDER_TEMPLATES[SetupState::default().template].id,
            "openrouter"
        );
    }

    #[test]
    fn reasoning_values_follow_the_selected_model() {
        assert_eq!(
            reasoning_levels(Some("gemini-3-pro-preview"))
                .iter()
                .map(|(level, _)| *level)
                .collect::<Vec<_>>(),
            vec!["low", "high"]
        );
        assert_eq!(
            reasoning_levels(Some("gemini-3.6-flash"))
                .iter()
                .map(|(level, _)| *level)
                .collect::<Vec<_>>(),
            vec!["minimal", "low", "medium", "high"]
        );
        assert_eq!(
            reasoning_levels(Some("gpt-5-pro"))
                .iter()
                .map(|(level, _)| *level)
                .collect::<Vec<_>>(),
            vec!["high"]
        );
    }

    #[test]
    fn gateway_errors_explain_that_directory_setup_is_not_the_problem() {
        let message = friendly_chat_error("HTTP status server error (502 Bad Gateway)".to_string());
        assert!(message.contains("not a directory-access problem"));
        assert!(message.contains("automatic retries"));
    }

    #[test]
    fn approval_shortcuts_cover_every_permission_scope() {
        assert_eq!(
            approval_decision_for_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            Some(ApprovalDecision::AllowOnce)
        );
        assert_eq!(
            approval_decision_for_key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::NONE)),
            Some(ApprovalDecision::AllowRun)
        );
        assert_eq!(
            approval_decision_for_key(KeyEvent::new(KeyCode::Char('p'), KeyModifiers::NONE)),
            Some(ApprovalDecision::AllowProject)
        );
        assert_eq!(
            approval_decision_for_key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE)),
            Some(ApprovalDecision::AlwaysAllowPattern)
        );
        assert_eq!(
            approval_decision_for_key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::SHIFT)),
            Some(ApprovalDecision::AlwaysAllowAll)
        );
        assert_eq!(
            approval_decision_for_key(KeyEvent::new(KeyCode::Char('A'), KeyModifiers::SHIFT)),
            Some(ApprovalDecision::AlwaysAllowAll)
        );
        assert_eq!(
            approval_decision_for_key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE)),
            Some(ApprovalDecision::AlwaysDenyPattern)
        );
        assert_eq!(
            approval_decision_for_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
            Some(ApprovalDecision::DenyOnce)
        );
    }

    #[test]
    fn approval_popup_renders_complete_clean_permission_choices() {
        let store = Store::in_memory().expect("store");
        let conversation = store.create_conversation(".", None).expect("conversation");
        let run = store
            .create_run(
                conversation.id,
                "edit",
                opensrc_core::ExecutionMode::Focused,
            )
            .expect("run");
        let approval = store
            .create_approval(
                run.id,
                None,
                None,
                "shell.run",
                serde_json::json!({"command": "cargo test --workspace"}),
                vec!["This command starts a local process.".to_string()],
            )
            .expect("approval");
        let backend = TestBackend::new(110, 36);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal
            .draw(|frame| {
                let area = frame.area();
                render_overlay(frame, area, &Overlay::Approval(approval.clone()));
            })
            .expect("approval modal render");
        let rendered = rendered_terminal(&terminal);

        for expected in [
            "Permission required",
            "Allow once",
            "Allow for this run",
            "Allow for this project",
            "Always allow this command/pattern",
            "Always allow all commands",
            "Deny once",
            "Always deny this command/pattern",
            "Edit arguments before allowing once",
        ] {
            assert!(
                rendered.contains(expected),
                "approval popup should render `{expected}`"
            );
        }
        assert!(!rendered.contains('â'));
    }

    #[test]
    fn all_popup_types_share_adaptive_modal_chrome() {
        let store = Store::in_memory().expect("store");
        let conversation = store.create_conversation(".", None).expect("conversation");
        let run = store
            .create_run(
                conversation.id,
                "edit",
                opensrc_core::ExecutionMode::Focused,
            )
            .expect("run");
        let approval = store
            .create_approval(
                run.id,
                None,
                None,
                "patch.apply",
                serde_json::json!({"path": "src/main.rs"}),
                vec!["File mutation requires confirmation.".to_string()],
            )
            .expect("approval");
        let picker = PickerState {
            kind: PickerKind::Model,
            title: "Models",
            options: (0..24)
                .map(|index| PickerOption {
                    value: format!("model-{index}"),
                    label: format!("Model {index}"),
                    auxiliary: Some("provider".to_string()),
                })
                .collect(),
            selected: 23,
            query: String::new(),
        };
        let overlays = vec![
            (Overlay::Help, "Help", "ESSENTIAL"),
            (
                Overlay::Error("Provider connection failed.".to_string()),
                "Error",
                "WHAT HAPPENED",
            ),
            (
                Overlay::Setup(SetupState::default()),
                "Connect a provider",
                "PROVIDER",
            ),
            (Overlay::Picker(picker), "Models", "Model 23"),
            (
                Overlay::ApprovalEditor {
                    approval,
                    editor: PromptEditor {
                        text: "{\n  \"path\": \"src/main.rs\"\n}".to_string(),
                        cursor: 27,
                        ..PromptEditor::default()
                    },
                },
                "Edit tool arguments",
                "Save and allow once",
            ),
        ];

        for (overlay, title, detail) in overlays {
            let backend = TestBackend::new(78, 24);
            let mut terminal = Terminal::new(backend).expect("terminal");
            terminal
                .draw(|frame| {
                    let area = frame.area();
                    render_overlay(frame, area, &overlay);
                })
                .expect("modal render");
            let rendered = rendered_terminal(&terminal);
            assert!(rendered.contains(title), "missing modal title `{title}`");
            assert!(rendered.contains(detail), "missing modal detail `{detail}`");
            assert!(!rendered.contains('â'));
        }
    }

    #[test]
    fn metrics_view_renders_compact_per_role_routing_benchmarks() {
        let mut app = App::new(Path::new("."));
        app.view = super::View::Metrics;
        app.routing_benchmarks
            .push(opensrc_core::RoutingBenchmarkAggregate {
                policy_version: "1".to_string(),
                role: "architect".to_string(),
                provider: "deepseek".to_string(),
                model: "deepseek-v4-pro".to_string(),
                samples: 4,
                mean_metrics: opensrc_core::RoutingBenchmarkMetrics {
                    architecture_quality_bps: Some(9000),
                    review_precision_bps: Some(8000),
                    latency_ms: 1250,
                    cost_microusd: 2350,
                    ..opensrc_core::RoutingBenchmarkMetrics::default()
                },
            });
        let backend = TestBackend::new(120, 34);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal
            .draw(|frame| render(frame, &app))
            .expect("metrics render");
        let rendered = rendered_terminal(&terminal);

        for expected in [
            "Routing benchmarks",
            "architect",
            "deepseek/deepseek-v4-pro",
            "85.00%",
            "1250 ms",
            "$0.002350",
        ] {
            assert!(
                rendered.contains(expected),
                "metrics view should render `{expected}`"
            );
        }
    }

    #[test]
    fn renders_chat_and_compact_states() {
        for (width, height) in [(120, 34), (100, 30), (40, 14)] {
            let backend = TestBackend::new(width, height);
            let mut terminal = Terminal::new(backend).expect("terminal");
            let app = App::new(Path::new("."));
            terminal
                .draw(|frame| render(frame, &app))
                .expect("chat render");
            let rendered = terminal
                .backend()
                .buffer()
                .content()
                .iter()
                .map(ratatui::buffer::Cell::symbol)
                .collect::<String>();
            assert!(rendered.contains("Divit's OpenSource"));
            assert!(rendered.contains("quiet terminal agent"));
            assert!(rendered.contains("Ask anything"));
            assert!(rendered.contains("Auto"));
            assert!(!rendered.contains("Build"));
            assert!(!rendered.contains("ctrl+p"));
            assert!(!rendered.contains("tab agents"));
            if width >= 72 {
                assert!(rendered.contains("ctrl+k"));
                assert!(rendered.contains("ctrl+m"));
                assert!(rendered.contains("ctrl+a"));
            }
            assert!(!rendered.contains("Usage"));
            assert!(!rendered.contains("Pipeline"));
            assert!(!rendered.contains("Workspace"));
            assert!(!rendered.contains("ready"));
            assert!(!rendered.contains("Changes │"));
        }
        let backend = TestBackend::new(120, 34);
        let mut terminal = Terminal::new(backend).expect("terminal");
        let mut app = App::new(Path::new("."));
        app.editor.insert_str("/m");
        terminal
            .draw(|frame| render(frame, &app))
            .expect("suggestion render");
        let rendered = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect::<String>();
        assert!(rendered.contains("commands"));
    }

    #[test]
    fn chat_visually_separates_user_prompts_from_ai_responses() {
        let backend = TestBackend::new(100, 30);
        let mut terminal = Terminal::new(backend).expect("terminal");
        let mut app = App::new(Path::new("."));
        let now = chrono::Utc::now();
        app.messages = vec![
            Message {
                id: uuid::Uuid::new_v4(),
                conversation_id: uuid::Uuid::new_v4(),
                run_id: None,
                sequence: 1,
                role: MessageRole::User,
                content: vec![MessageContent::Text {
                    text: "Please inspect this project".to_string(),
                }],
                provider: None,
                model: None,
                continuation_id: None,
                created_at: now,
            },
            Message {
                id: uuid::Uuid::new_v4(),
                conversation_id: uuid::Uuid::new_v4(),
                run_id: None,
                sequence: 2,
                role: MessageRole::Assistant,
                content: vec![MessageContent::Text {
                    text: "I found the relevant files.".to_string(),
                }],
                provider: None,
                model: None,
                continuation_id: None,
                created_at: now,
            },
        ];
        terminal
            .draw(|frame| render(frame, &app))
            .expect("conversation render");
        let rendered = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect::<String>();
        assert!(rendered.contains("YOU"));
        assert!(rendered.contains("Please inspect this project"));
        assert!(rendered.contains("ASSISTANT"));
        assert!(rendered.contains("I found the relevant files."));
        assert!(rendered.contains("Divit's OpenSource"));
        assert!(rendered.contains("Auto"));
        assert!(!rendered.contains("▣"));
    }

    #[test]
    fn provider_failures_render_inline_instead_of_obscuring_chat() {
        let mut app = App::new(Path::new("."));
        app.busy = true;
        super::handle_client_event(
            &mut app,
            ClientEvent::ChatFailed("Invalid API key.".to_string()),
        );
        assert!(app.overlay.is_none());

        let backend = TestBackend::new(100, 30);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal
            .draw(|frame| render(frame, &app))
            .expect("inline error render");
        let rendered = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect::<String>();
        assert!(rendered.contains("Invalid API key."));
        assert!(rendered.contains("Auto"));
    }

    #[test]
    fn settings_command_opens_a_single_minimal_configuration_view() {
        let mut app = App::new(Path::new("."));
        let client = reqwest::Client::new();
        let (tx, _rx) = mpsc::unbounded_channel();
        assert!(handle_slash_command(
            &mut app,
            "/settings",
            &client,
            "http://127.0.0.1:1",
            &tx
        ));
        assert_eq!(app.view, super::View::Settings);

        let backend = TestBackend::new(100, 30);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal
            .draw(|frame| render(frame, &app))
            .expect("settings render");
        let rendered = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect::<String>();
        assert!(rendered.contains("Model"));
        assert!(rendered.contains("Capabilities"));
        assert!(rendered.contains("Permissions"));
        assert!(!rendered.contains("Chat │"));
    }

    #[test]
    fn chat_collapses_large_tool_payloads_into_quiet_status_lines() {
        let lines = render_content_block(&MessageContent::ToolResult {
            provider_call_id: "provider-1".to_string(),
            canonical_call_id: "call-1".to_string(),
            name: "fs.list".to_string(),
            result: serde_json::json!({
                "output": {
                    "entries": [
                        {"path":"C:\\one"},
                        {"path":"C:\\two"}
                    ],
                    "truncated": false
                }
            }),
            timing_ms: Some(1),
            approval_state: Some("not_required".to_string()),
        });
        let rendered = lines
            .iter()
            .flat_map(|line| line.spans.iter())
            .map(|span| span.content.as_ref())
            .collect::<String>();
        assert_eq!(rendered, "✓ fs.list  2 items");
        assert!(!rendered.contains("C:\\"));
    }

    #[test]
    fn pending_prompt_renders_with_an_animated_cube_wave() {
        let first = cube_loader_line(Duration::ZERO)
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<String>();
        let later = cube_loader_line(Duration::from_millis(270))
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<String>();
        assert_ne!(first, later);
        assert!(first.contains('█'));
        assert!(later.contains('█'));

        let backend = TestBackend::new(100, 30);
        let mut terminal = Terminal::new(backend).expect("terminal");
        let mut app = App::new(Path::new("."));
        app.busy = true;
        app.pending_prompt = Some("Show the prompt immediately".to_string());
        app.loader_started = Instant::now().checked_sub(Duration::from_millis(270));
        terminal
            .draw(|frame| render(frame, &app))
            .expect("pending prompt render");
        let rendered = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect::<String>();
        assert!(rendered.contains("Show the prompt immediately"));
        assert!(rendered.contains('█'));
        assert!(!rendered.contains("thinking"));
        assert!(!rendered.contains("processing"));
    }

    #[test]
    fn cursor_is_visible_and_late_stream_deltas_are_ignored() {
        let mut editor = PromptEditor::default();
        editor.insert_str("hello");
        let text = editor_text(&editor, true);
        assert_eq!(
            text.lines[0].spans.last().and_then(|span| span.style.bg),
            Some(Color::White)
        );

        let conversation_id = uuid::Uuid::new_v4();
        let run_id = uuid::Uuid::new_v4();
        let event = |text: &str| Event {
            id: 1,
            conversation_id,
            run_id: Some(run_id),
            agent_id: None,
            task_id: None,
            kind: "model.event".to_string(),
            payload: serde_json::json!({
                "event": ModelEvent::TextDelta {
                    text: text.to_string()
                }
            }),
            idempotency_key: None,
            created_at: chrono::Utc::now(),
        };
        let mut app = App::new(Path::new("."));
        app.busy = true;
        app.apply_domain_event(&event("first"));
        assert_eq!(app.streaming_text, "first");
        app.busy = false;
        app.streaming_text.clear();
        app.apply_domain_event(&event("late duplicate"));
        assert!(app.streaming_text.is_empty());
    }

    #[tokio::test]
    async fn prompt_reaches_chat_service_and_stream_events_render() {
        let store = Store::in_memory().expect("store");
        let requests = Arc::new(Mutex::new(Vec::new()));
        let providers = ProviderRouter::default();
        providers.register_with_model(
            Arc::new(StreamingFixture {
                requests: requests.clone(),
            }),
            "fixture-model",
        );
        let state = ServerState {
            runtime: Runtime::with_services(
                store.clone(),
                AgentLimits::default(),
                providers,
                ToolExecutor::default(),
            ),
            provider_config_path: None,
        };
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("listener");
        let address = listener.local_addr().expect("address");
        let server = tokio::spawn(async move {
            axum::serve(listener, opensrc_server::router(state))
                .await
                .expect("server");
        });
        let base_url = format!("http://{address}");

        let mut app = App::new(Path::new("."));
        app.provider = Some("tui-fixture".to_string());
        app.model = Some("fixture-model".to_string());
        app.mode = Some(opensrc_core::ExecutionMode::Direct);
        app.editor.insert_str("from the prompt editor");
        let client = reqwest::Client::new();
        let (tx, mut rx) = mpsc::unbounded_channel();
        submit_prompt(&mut app, &client, &base_url, &tx);
        assert!(app.busy);
        assert!(app.editor.text.is_empty());
        assert!(matches!(
            tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
                .await
                .expect("chat timeout"),
            Some(ClientEvent::ChatFinished)
        ));

        let captured = requests.lock().expect("requests");
        assert_eq!(captured.len(), 1);
        assert_eq!(
            captured[0].messages[0].content,
            vec![MessageContent::text("from the prompt editor")]
        );
        drop(captured);
        for event in store.events_after(0, 100).expect("events") {
            app.apply_domain_event(&event);
        }
        assert_eq!(app.streaming_text, "streamed answer");
        let backend = TestBackend::new(100, 30);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal
            .draw(|frame| render(frame, &app))
            .expect("stream render");
        let rendered = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect::<String>();
        assert!(rendered.contains("streamed answer"));
        assert_eq!(store.list_conversations(None).expect("sessions").len(), 1);
        assert_eq!(
            store
                .list_messages(store.list_conversations(None).expect("sessions")[0].id)
                .expect("messages")
                .len(),
            2
        );
        server.abort();
    }

    #[tokio::test]
    async fn approval_overlay_posts_a_real_decision() {
        let store = Store::in_memory().expect("store");
        let conversation = store.create_conversation(".", None).expect("conversation");
        let run = store
            .create_run(
                conversation.id,
                "edit",
                opensrc_core::ExecutionMode::Focused,
            )
            .expect("run");
        let approval = store
            .create_approval(
                run.id,
                None,
                None,
                "patch.apply",
                serde_json::json!({"path": "sample.txt"}),
                vec!["file mutation requires approval".to_string()],
            )
            .expect("approval");
        let state = ServerState {
            runtime: Runtime::new(store.clone(), AgentLimits::default()),
            provider_config_path: None,
        };
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("listener");
        let address = listener.local_addr().expect("address");
        let server = tokio::spawn(async move {
            axum::serve(listener, opensrc_server::router(state))
                .await
                .expect("server");
        });
        let mut app = App::new(Path::new("."));
        app.overlay = Some(Overlay::Approval(approval.clone()));
        let client = reqwest::Client::new();
        let (tx, mut rx) = mpsc::unbounded_channel();
        handle_key(
            &mut app,
            crossterm::event::KeyEvent::new(
                crossterm::event::KeyCode::Char('e'),
                crossterm::event::KeyModifiers::NONE,
            ),
            &client,
            &format!("http://{address}"),
            &tx,
        );
        let Some(Overlay::ApprovalEditor { editor, .. }) = app.overlay.as_mut() else {
            panic!("approval editor should open");
        };
        editor.text = serde_json::json!({"path": "edited.txt"}).to_string();
        editor.cursor = editor.text.len();
        handle_key(
            &mut app,
            crossterm::event::KeyEvent::new(
                crossterm::event::KeyCode::Enter,
                crossterm::event::KeyModifiers::CONTROL,
            ),
            &client,
            &format!("http://{address}"),
            &tx,
        );
        let event = tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
            .await
            .expect("decision timeout")
            .expect("decision event");
        assert!(matches!(
            event,
            ClientEvent::ApprovalDecided(ref decided)
                if decided.id == approval.id
                    && decided.status == opensrc_core::ApprovalStatus::Allowed
        ));
        assert_eq!(
            store
                .get_approval(approval.id)
                .expect("stored decision")
                .status,
            opensrc_core::ApprovalStatus::Allowed
        );
        assert_eq!(
            store
                .get_approval(approval.id)
                .expect("stored decision")
                .edited_arguments,
            Some(serde_json::json!({"path": "edited.txt"}))
        );
        server.abort();
    }

    #[tokio::test]
    async fn approval_shift_a_submits_always_allow_all() {
        let store = Store::in_memory().expect("store");
        let conversation = store.create_conversation(".", None).expect("conversation");
        let run = store
            .create_run(
                conversation.id,
                "run commands",
                opensrc_core::ExecutionMode::Focused,
            )
            .expect("run");
        let approval = store
            .create_approval(
                run.id,
                None,
                None,
                "shell.run",
                serde_json::json!({"command": "cargo test"}),
                vec!["process execution requires approval".to_string()],
            )
            .expect("approval");
        let state = ServerState {
            runtime: Runtime::new(store.clone(), AgentLimits::default()),
            provider_config_path: None,
        };
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("listener");
        let address = listener.local_addr().expect("address");
        let server = tokio::spawn(async move {
            axum::serve(listener, opensrc_server::router(state))
                .await
                .expect("server");
        });
        let mut app = App::new(Path::new("."));
        app.overlay = Some(Overlay::Approval(approval.clone()));
        let client = reqwest::Client::new();
        let (tx, mut rx) = mpsc::unbounded_channel();
        handle_key(
            &mut app,
            KeyEvent::new(KeyCode::Char('a'), KeyModifiers::SHIFT),
            &client,
            &format!("http://{address}"),
            &tx,
        );
        let event = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("decision timeout")
            .expect("decision event");
        assert!(matches!(
            event,
            ClientEvent::ApprovalDecided(ref decided)
                if decided.id == approval.id
                    && decided.decision == Some(ApprovalDecision::AlwaysAllowAll)
        ));
        let rules = store.list_permission_rules().expect("permission rules");
        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0].tool_name, "*");
        assert_eq!(rules[0].arguments_pattern, serde_json::Value::Null);
        server.abort();
    }

    #[tokio::test]
    async fn startup_snapshot_deserializes_real_server_resources() {
        let state = ServerState {
            runtime: Runtime::with_components(
                Store::in_memory().expect("store"),
                AgentLimits::default(),
                ProviderRouter::default(),
                ToolExecutor::default(),
                SkillRegistry::discover_many_with_builtins(Vec::<std::path::PathBuf>::new())
                    .expect("built-in skills"),
            ),
            provider_config_path: None,
        };
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("listener");
        let address = listener.local_addr().expect("address");
        let server = tokio::spawn(async move {
            axum::serve(listener, opensrc_server::router(state))
                .await
                .expect("server");
        });
        let snapshot = load_snapshot(
            &reqwest::Client::new(),
            &format!("http://{address}"),
            ".",
            None,
        )
        .await
        .expect("snapshot");
        assert!(!snapshot.tools.is_empty());
        assert!(!snapshot.agent_definitions.is_empty());
        assert!(!snapshot.skills.is_empty());
        assert!(snapshot.messages.is_empty());
        server.abort();
    }

    #[test]
    fn aicredits_setup_uses_the_gateway_api_and_named_family() {
        let template = PROVIDER_TEMPLATES
            .iter()
            .find(|template| template.id == "aicredits")
            .expect("AICredits template");
        assert_eq!(template.protocol, "openai_compatible");
        assert_eq!(template.family, Some("aicredits"));
        assert_eq!(template.base_url, "https://api.aicredits.in/v1");
        assert_eq!(template.key_env, "AICREDITS_API_KEY");
        assert_eq!(template.model, "google/gemini-2.5-flash");
    }

    #[test]
    fn provider_catalog_is_broad_unique_and_keeps_custom_and_local_paths() {
        let ids = PROVIDER_TEMPLATES
            .iter()
            .map(|template| template.id)
            .collect::<std::collections::BTreeSet<_>>();
        assert_eq!(ids.len(), PROVIDER_TEMPLATES.len());
        for expected in [
            "aicredits",
            "krutrim",
            "groq",
            "together",
            "fireworks",
            "mistral",
            "xai",
            "deepinfra",
            "nvidia",
            "ollama",
            "lm-studio",
            "vllm",
            "custom",
        ] {
            assert!(
                ids.contains(expected),
                "missing provider template {expected}"
            );
        }
        assert!(super::is_loopback_compatible_url(
            PROVIDER_TEMPLATES
                .iter()
                .find(|template| template.id == "lm-studio")
                .expect("LM Studio template")
                .base_url
        ));
    }
}
