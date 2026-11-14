use anyhow::{Context, Result};
use rig::prelude::*;
use rig::providers::{anthropic, ollama, openai};
use rig::tool::{server::ToolServerHandle, DynamicTool};
use rig_core::tool::managed::{ManagedToolSink, ManagedToolToken};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use crate::config::{AgentConfig, McpServerConfig, Provider, Wire};
use crate::supervise::Backoff;

#[derive(Clone, Debug)]
pub struct AuditContext {
    pub request_id: String,
    pub client_id: String,
    pub client_name: String,
    pub on_behalf_of: String,
}

impl Default for AuditContext {
    fn default() -> Self {
        Self {
            request_id: "startup".into(),
            client_id: "system".into(),
            client_name: "system".into(),
            on_behalf_of: "unattributed".into(),
        }
    }
}

tokio::task_local! {
    static CURRENT_AUDIT_CONTEXT: AuditContext;
}

pub async fn with_audit_context<F: std::future::Future>(
    context: AuditContext,
    future: F,
) -> F::Output {
    CURRENT_AUDIT_CONTEXT.scope(context, future).await
}

/// Every agent is built once at boot. Requests only borrow them.
pub type Registry = Arc<AgentRegistry>;

/// Limits that apply to one agent on top of the server-wide ones.
#[derive(Clone, Default)]
pub struct AgentLimits {
    /// Present when `max_concurrent_requests` is set for the agent.
    pub semaphore: Option<Arc<tokio::sync::Semaphore>>,
    /// Present when `request_timeout_seconds` is set for the agent.
    pub timeout: Option<std::time::Duration>,
}

pub struct AgentRegistry {
    agents: HashMap<String, Agent>,
    limits: HashMap<String, AgentLimits>,
    /// One task per MCP server. Each owns its connection (and, for stdio, the child process),
    /// which must stay alive for registered tools to keep working.
    supervisors: Mutex<Vec<tokio::task::JoinHandle<()>>>,
    stop: tokio::sync::watch::Sender<bool>,
    /// One liveness check per configured MCP server.
    health: Vec<ServerHealth>,
    /// Set when shutdown begins, while requests are still draining.
    draining: std::sync::atomic::AtomicBool,
}

/// Whether one MCP server is currently usable.
struct ServerHealth {
    ready: Box<dyn Fn() -> bool + Send + Sync>,
}

/// How many of the configured MCP servers are connected right now.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Readiness {
    pub total: usize,
    pub ready: usize,
    pub draining: bool,
}

impl Readiness {
    pub fn is_ready(&self) -> bool {
        !self.draining && self.ready == self.total
    }
}

impl AgentRegistry {
    /// Ready means every configured MCP server is connected. Provider reachability is
    /// deliberately not probed: that would spend tokens or fail on an air-gapped network for
    /// reasons unrelated to whether this gateway can serve.
    pub fn readiness(&self) -> Readiness {
        Readiness {
            total: self.health.len(),
            ready: self.health.iter().filter(|h| (h.ready)()).count(),
            draining: self.draining.load(std::sync::atomic::Ordering::SeqCst),
        }
    }

    /// Report not-ready from now on, so a load balancer stops sending traffic while in-flight
    /// requests finish. Servers keep running until [`AgentRegistry::shutdown`].
    pub fn begin_drain(&self) {
        self.draining
            .store(true, std::sync::atomic::Ordering::SeqCst);
    }

    #[cfg(test)]
    fn bare(agents: HashMap<String, Agent>) -> Self {
        Self {
            agents,
            limits: HashMap::new(),
            supervisors: Mutex::new(Vec::new()),
            stop: tokio::sync::watch::channel(false).0,
            health: Vec::new(),
            draining: std::sync::atomic::AtomicBool::new(false),
        }
    }

    /// Stop every supervised stdio child and wait for it to be reaped, up to `grace`. Called
    /// after the HTTP server has drained so no in-flight request loses its tool mid-call.
    pub async fn shutdown(&self, grace: std::time::Duration) {
        let _ = self.stop.send(true);
        let handles = std::mem::take(&mut *self.supervisors.lock().unwrap());
        stop_supervisors(handles, grace).await;
    }
}

async fn stop_supervisors(handles: Vec<tokio::task::JoinHandle<()>>, grace: std::time::Duration) {
    for handle in handles {
        if tokio::time::timeout(grace, handle).await.is_err() {
            tracing::warn!("a stdio MCP supervisor did not stop within the grace period");
        }
    }
}

impl AgentRegistry {
    pub fn get(&self, name: &str) -> Option<&Agent> {
        self.agents.get(name)
    }

    /// The agent's own limits; empty when it sets none.
    pub fn limits(&self, name: &str) -> AgentLimits {
        self.limits.get(name).cloned().unwrap_or_default()
    }

    pub fn keys(&self) -> impl Iterator<Item = &String> {
        self.agents.keys()
    }
}

pub async fn build(configs: &[AgentConfig]) -> Result<Registry> {
    let (stop, stop_rx) = tokio::sync::watch::channel(false);
    let mut supervisors: Vec<tokio::task::JoinHandle<()>> = Vec::new();
    let mut health: Vec<ServerHealth> = Vec::new();
    match build_agents(configs, &stop_rx, &mut supervisors, &mut health).await {
        Ok(agents) => Ok(Arc::new(AgentRegistry {
            agents,
            limits: configs
                .iter()
                .map(|cfg| {
                    (
                        cfg.name.clone(),
                        AgentLimits {
                            semaphore: cfg
                                .max_concurrent_requests
                                .map(|n| Arc::new(tokio::sync::Semaphore::new(n))),
                            timeout: cfg
                                .request_timeout_seconds
                                .map(std::time::Duration::from_secs),
                        },
                    )
                })
                .collect(),
            supervisors: Mutex::new(supervisors),
            stop,
            health,
            draining: std::sync::atomic::AtomicBool::new(false),
        })),
        Err(error) => {
            // A later agent failed after earlier ones already spawned children. Do not leave
            // them running behind a gateway that is about to exit with an error.
            let _ = stop.send(true);
            stop_supervisors(supervisors, std::time::Duration::from_secs(5)).await;
            Err(error)
        }
    }
}

async fn build_agents(
    configs: &[AgentConfig],
    stop_rx: &tokio::sync::watch::Receiver<bool>,
    supervisors: &mut Vec<tokio::task::JoinHandle<()>>,
    health: &mut Vec<ServerHealth>,
) -> Result<HashMap<String, Agent>> {
    let mut agents = HashMap::new();

    for cfg in configs {
        let builder = match cfg.provider {
            // `from_env` reads OPENAI_API_KEY and, if set, OPENAI_BASE_URL —
            // which is how you point it at Azure, vLLM, or an internal gateway.
            Provider::Openai => {
                let mut config = openai::OpenAIConfig::from_env().context(
                    "an openai agent needs OPENAI_API_KEY (and optionally OPENAI_BASE_URL)",
                )?;
                // Rig defaults to the Responses API. Most OpenAI-compatible servers only
                // implement Chat Completions, so an agent can opt into that wire.
                if cfg.wire == Some(Wire::Chat) {
                    config = config.with_route(openai::Route::Chat);
                }
                AgentBuilder::new(config.client().completion(cfg.model.as_str()))
            }
            // `from_env` reads OLLAMA_API_BASE_URL and OLLAMA_API_KEY, both optional.
            Provider::Ollama => {
                let client = ollama::Ollama::from_env()
                    .context("an ollama agent could not read OLLAMA_API_BASE_URL")?;
                AgentBuilder::new(client.completion(cfg.model.as_str()))
            }
            // `from_env` reads ANTHROPIC_API_KEY and, if set, ANTHROPIC_BASE_URL.
            Provider::Anthropic => {
                let client = anthropic::Anthropic::from_env().context(
                    "an anthropic agent needs ANTHROPIC_API_KEY (and optionally ANTHROPIC_BASE_URL)",
                )?;
                AgentBuilder::new(client.completion(cfg.model.as_str()))
            }
        };

        let mut builder = builder.default_max_turns(cfg.max_turns);
        if !cfg.mcp_servers.is_empty() {
            let tool_server = rig::tool::server::ToolServer::new().run();
            for server in &cfg.mcp_servers {
                let context = || {
                    format!(
                        "connecting MCP server {} for agent '{}'",
                        server.display_name(),
                        cfg.name
                    )
                };
                let up = Arc::new(std::sync::atomic::AtomicBool::new(false));
                let started = if server.command.is_some() {
                    start_stdio(
                        server,
                        &tool_server,
                        &cfg.name,
                        stop_rx.clone(),
                        Backoff::default(),
                        up.clone(),
                    )
                    .await
                } else {
                    start_http(
                        server,
                        &tool_server,
                        &cfg.name,
                        stop_rx.clone(),
                        Backoff::default(),
                        Heartbeat::from_config(server),
                        up.clone(),
                    )
                    .await
                };
                supervisors.push(started.with_context(context)?);
                health.push(ServerHealth {
                    ready: Box::new(move || up.load(std::sync::atomic::Ordering::SeqCst)),
                });
            }
            builder = builder.tool_server_handle(tool_server);
        }
        let agent = build_agent(builder, cfg);
        agents.insert(cfg.name.clone(), agent);
    }

    Ok(agents)
}

#[cfg(test)]
pub(crate) fn test_registry(base_url: &str) -> Registry {
    let client = openai::OpenAIConfig::new("test-key")
        .with_base_url(base_url)
        .with_route(openai::Route::Chat)
        .client();
    let agent = AgentBuilder::new(client.completion("mock-model")).build();
    let mut agents = HashMap::new();
    agents.insert("mock".to_string(), agent);
    Arc::new(AgentRegistry::bare(agents))
}

/// [`test_registry`] with limits on its one agent, `mock`.
#[cfg(test)]
pub(crate) fn test_registry_with_limits(
    base_url: &str,
    max_concurrent: Option<usize>,
    timeout: Option<std::time::Duration>,
) -> Registry {
    let mut registry = Arc::try_unwrap(test_registry(base_url))
        .unwrap_or_else(|_| unreachable!("fresh registry has one owner"));
    registry.limits.insert(
        "mock".to_string(),
        AgentLimits {
            semaphore: max_concurrent.map(|n| Arc::new(tokio::sync::Semaphore::new(n))),
            timeout,
        },
    );
    Arc::new(registry)
}

/// A registry that reports the given MCP server states, so readiness can be tested without
/// running servers. Each flag stands for one configured server.
#[cfg(test)]
pub(crate) fn test_registry_with_servers(
    base_url: &str,
    servers: Vec<Arc<std::sync::atomic::AtomicBool>>,
) -> Registry {
    let mut registry = Arc::try_unwrap(test_registry(base_url))
        .unwrap_or_else(|_| unreachable!("fresh registry has one owner"));
    registry.health = servers
        .into_iter()
        .map(|up| ServerHealth {
            ready: Box::new(move || up.load(std::sync::atomic::Ordering::SeqCst)),
        })
        .collect();
    Arc::new(registry)
}

/// Like [`test_registry`], but the agent owns a tool server holding one permitted tool,
/// `get_time`. The tool counts its own invocations so a test can tell "the model asked"
/// apart from "the gateway actually ran it".
///
/// `allowlist` is applied through the real [`FilteredToolSink`], so a test built on this
/// exercises the same policy code the server uses, not a shortcut.
#[cfg(test)]
pub(crate) fn test_registry_with_tools(
    base_url: &str,
    allowlist: Option<Vec<String>>,
    invoked: Arc<std::sync::atomic::AtomicUsize>,
) -> Registry {
    let client = openai::OpenAIConfig::new("test-key")
        .with_base_url(base_url)
        .with_route(openai::Route::Chat)
        .client();
    let tool_server = rig::tool::server::ToolServer::new().run();
    let sink = FilteredToolSink {
        inner: tool_server.clone(),
        allowlist: allowlist.map(|names| names.into_iter().collect()),
        agent: "mock".to_string(),
        server: "http://127.0.0.1:9/mcp".to_string(),
    };
    let tool = DynamicTool::new(
        rig::message::ToolName::new("get_time").expect("valid tool name"),
        "Look up the current time in a city",
        serde_json::json!({
            "type": "object",
            "properties": {"city": {"type": "string"}},
            "required": ["city"]
        }),
        move |_| {
            let invoked = invoked.clone();
            Box::pin(async move {
                invoked.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Ok(rig::tool::ToolOutput::text("12:00"))
            })
        },
    );
    sink.add_managed_tools(vec![tool]);

    let agent = AgentBuilder::new(client.completion("mock-model"))
        .tool_server_handle(tool_server)
        .default_max_turns(4)
        .build();
    let mut agents = HashMap::new();
    agents.insert("mock".to_string(), agent);
    Arc::new(AgentRegistry::bare(agents))
}

fn build_agent(builder: AgentBuilder, cfg: &AgentConfig) -> Agent {
    match &cfg.preamble {
        Some(p) => builder.preamble(p).build(),
        None => builder.build(),
    }
}

fn mcp_handler(
    server: &McpServerConfig,
    tool_server: &rig::tool::server::ToolServerHandle,
    agent: &str,
) -> rig::tool::rmcp::McpClientHandler<FilteredToolSink> {
    use rig::tool::rmcp::{
        rmcp::model::{ClientCapabilities, ClientConfig, Implementation},
        McpClientHandler,
    };

    let client_info = ClientConfig::new(
        ClientCapabilities::default(),
        Implementation::new("riggate", env!("CARGO_PKG_VERSION")),
    );
    let allowlist = server
        .allowed_tools
        .as_ref()
        .map(|tools| tools.iter().cloned().collect());
    // The same sink wraps both transports, so tool policy and audit lines cannot depend on
    // how the server is reached.
    let sink = FilteredToolSink {
        inner: tool_server.clone(),
        allowlist,
        agent: agent.to_string(),
        server: server.display_name(),
    };
    McpClientHandler::new(client_info, sink)
}

async fn connect_mcp(
    server: &McpServerConfig,
    tool_server: &rig::tool::server::ToolServerHandle,
    agent: &str,
) -> Result<rig::tool::rmcp::McpConnection> {
    use rig::tool::rmcp::rmcp::transport::StreamableHttpClientTransport;

    let name = server.display_name();
    let url = server
        .url
        .clone()
        .context("MCP server has no url; stdio servers are supervised")?;
    let token = server
        .bearer_token_env
        .as_deref()
        .map(read_mcp_token)
        .transpose()?;
    let transport = StreamableHttpClientTransport::from_config(mcp_transport_config(url, token)?);
    let connection = mcp_handler(server, tool_server, agent)
        .connect(transport)
        .await
        .with_context(|| format!("MCP connection to {name} failed"))?;
    Ok(connection)
}

/// A running stdio server: the MCP connection and the process behind it.
struct StdioSession {
    connection: Option<rig::tool::rmcp::McpConnection>,
    process: crate::child::ServerProcess,
}

impl crate::supervise::Session for StdioSession {
    async fn closed(&mut self) {
        let connection = self.connection.as_ref().expect("connection until stop");
        // Either end ending counts. The process can die while a grandchild still holds the
        // pipe open, in which case the connection alone would never report it.
        tokio::select! {
            _ = connection.closed() => {}
            _ = self.process.exited() => {}
        }
    }

    async fn stop(mut self) {
        if let Some(connection) = self.connection.take() {
            // Closing drops our end of stdin, which is the polite request to exit.
            connection.close().await;
        }
        self.process.stop().await;
    }
}

/// Spawn the child once and complete the MCP handshake. Called again by the supervisor
/// after every exit, so it builds everything it needs fresh: a command, a handler, a pipe.
async fn connect_stdio_once(
    server: &McpServerConfig,
    tool_server: &rig::tool::server::ToolServerHandle,
    agent: &str,
) -> Result<StdioSession> {
    let name = server.display_name();
    let command = stdio_command(server, |var| std::env::var(var).ok());
    let spawned = crate::child::ServerProcess::spawn(command)
        .with_context(|| format!("spawning MCP server {name}"))?;
    let mut process = spawned.process;
    if let Some(stderr) = spawned.stderr {
        tokio::spawn(crate::supervise::forward_stderr(
            stderr,
            agent.to_string(),
            name.clone(),
        ));
    }
    match mcp_handler(server, tool_server, agent)
        .connect((spawned.stdout, spawned.stdin))
        .await
    {
        Ok(connection) => Ok(StdioSession {
            connection: Some(connection),
            process,
        }),
        Err(error) => {
            // The handshake failed; do not leave the child (or its children) behind.
            process.stop().await;
            Err(anyhow::Error::from(error).context(format!("MCP connection to {name} failed")))
        }
    }
}

/// Start supervising a server and wait for its first handshake, so the gateway does not
/// begin serving with a tool server that never came up. After that, exits are retried.
async fn start_supervised<S, F, Fut>(
    agent: &str,
    name: String,
    connect: F,
    stop: tokio::sync::watch::Receiver<bool>,
    backoff: Backoff,
    ready: Arc<std::sync::atomic::AtomicBool>,
) -> Result<tokio::task::JoinHandle<()>>
where
    S: crate::supervise::Session + Send + 'static,
    F: FnMut() -> Fut + Send + 'static,
    Fut: std::future::Future<Output = Result<S>> + Send + 'static,
{
    let (first_tx, first_rx) = tokio::sync::oneshot::channel();
    let handle = tokio::spawn(crate::supervise::supervise(
        agent.to_string(),
        name,
        connect,
        stop,
        first_tx,
        backoff,
        ready,
    ));
    match first_rx.await {
        Ok(Ok(())) => Ok(handle),
        Ok(Err(error)) => {
            let _ = handle.await;
            Err(error)
        }
        Err(_) => anyhow::bail!("MCP supervisor ended before the first connection"),
    }
}

async fn start_stdio(
    server: &McpServerConfig,
    tool_server: &rig::tool::server::ToolServerHandle,
    agent: &str,
    stop: tokio::sync::watch::Receiver<bool>,
    backoff: Backoff,
    ready: Arc<std::sync::atomic::AtomicBool>,
) -> Result<tokio::task::JoinHandle<()>> {
    let (server_cfg, tools, agent_name) = (server.clone(), tool_server.clone(), agent.to_string());
    start_supervised(
        agent,
        server.display_name(),
        move || {
            let (server, tools, agent) = (server_cfg.clone(), tools.clone(), agent_name.clone());
            async move { connect_stdio_once(&server, &tools, &agent).await }
        },
        stop,
        backoff,
        ready,
    )
    .await
}

/// Same supervision for a Streamable HTTP server: a connection that closes, or stops
/// answering the heartbeat, is reconnected with backoff.
async fn start_http(
    server: &McpServerConfig,
    tool_server: &rig::tool::server::ToolServerHandle,
    agent: &str,
    stop: tokio::sync::watch::Receiver<bool>,
    backoff: Backoff,
    heartbeat: Option<Heartbeat>,
    ready: Arc<std::sync::atomic::AtomicBool>,
) -> Result<tokio::task::JoinHandle<()>> {
    let (server_cfg, tools, agent_name) = (server.clone(), tool_server.clone(), agent.to_string());
    start_supervised(
        agent,
        server.display_name(),
        move || {
            let (server, tools, agent) = (server_cfg.clone(), tools.clone(), agent_name.clone());
            async move {
                connect_mcp(&server, &tools, &agent)
                    .await
                    .map(|connection| HttpSession {
                        connection,
                        heartbeat,
                    })
            }
        },
        stop,
        backoff,
        ready,
    )
    .await
}

/// How an HTTP server's liveness is checked. The rmcp client retries a lost connection
/// internally and never reports it closed, so without this a server that went away would
/// stay "ready" with its tools registered until a call happened to fail.
#[derive(Clone, Copy, Debug)]
pub struct Heartbeat {
    pub interval: std::time::Duration,
    pub timeout: std::time::Duration,
    /// Consecutive failed checks before the server is declared gone.
    pub failures: u32,
}

impl Heartbeat {
    /// The server's configured heartbeat, or `None` when it is turned off
    /// (`heartbeat_interval_seconds = 0`).
    pub fn from_config(server: &McpServerConfig) -> Option<Heartbeat> {
        let interval = server.heartbeat_interval_seconds.unwrap_or(15);
        if interval == 0 {
            return None;
        }
        Some(Heartbeat {
            interval: std::time::Duration::from_secs(interval),
            timeout: std::time::Duration::from_secs(server.heartbeat_timeout_seconds.unwrap_or(5)),
            failures: 2,
        })
    }
}

struct HttpSession {
    connection: rig::tool::rmcp::McpConnection,
    /// `None` when checks are turned off; only a clean close is then noticed.
    heartbeat: Option<Heartbeat>,
}

impl HttpSession {
    /// Resolves once `failures` checks in a row have failed or timed out. A `tools/list`
    /// is the probe: every MCP server implements it and it needs no extra permission.
    async fn unresponsive(&self) {
        let Some(heartbeat) = self.heartbeat else {
            return std::future::pending().await;
        };
        let mut failed = 0;
        loop {
            tokio::time::sleep(heartbeat.interval).await;
            let alive = matches!(
                tokio::time::timeout(heartbeat.timeout, self.connection.tools()).await,
                Ok(Ok(_))
            );
            failed = if alive { 0 } else { failed + 1 };
            if failed >= heartbeat.failures {
                return;
            }
        }
    }
}

impl crate::supervise::Session for HttpSession {
    async fn closed(&mut self) {
        tokio::select! {
            _ = self.connection.closed() => {}
            _ = self.unresponsive() => {}
        }
    }

    async fn stop(self) {
        self.connection.close().await;
    }
}

/// The complete environment a stdio child receives: the literal `env` entries plus the
/// `inherit_env` names that are set in the gateway. Nothing else. This is a pure function
/// of the config and a lookup so the fail-closed behavior is testable without touching the
/// real process environment.
fn child_environment(
    server: &McpServerConfig,
    lookup: impl Fn(&str) -> Option<String>,
) -> Vec<(String, String)> {
    let mut vars: Vec<(String, String)> = server
        .inherit_env
        .iter()
        .filter_map(|name| lookup(name).map(|value| (name.clone(), value)))
        .collect();
    vars.extend(server.env.iter().map(|(k, v)| (k.clone(), v.clone())));
    vars
}

/// Build the child command with `env_clear()` first: `Command` inherits the parent's
/// environment unless told otherwise, which would hand `OPENAI_API_KEY` and
/// `RIGGATE_API_KEY` to every MCP server.
fn stdio_command(
    server: &McpServerConfig,
    lookup: impl Fn(&str) -> Option<String>,
) -> tokio::process::Command {
    let mut command = tokio::process::Command::new(server.command.as_deref().unwrap_or_default());
    command
        .args(&server.args)
        .env_clear()
        .envs(child_environment(server, lookup));
    if let Some(dir) = &server.working_dir {
        command.current_dir(dir);
    }
    command
}

struct FilteredToolSink {
    inner: ToolServerHandle,
    allowlist: Option<HashSet<String>>,
    agent: String,
    /// Which MCP server these tools came from. The allowlist is per server, so a log line
    /// without it cannot be reconciled against the config it was written from.
    server: String,
}

impl FilteredToolSink {
    fn filter(&self, tools: Vec<DynamicTool>) -> Vec<DynamicTool> {
        let discovered = tools.len();
        let allowed: Vec<DynamicTool> = tools
            .into_iter()
            .filter_map(|tool| {
                let name = tool.name();
                let is_allowed = tool_allowed(self.allowlist.as_ref(), name.as_ref());
                if is_allowed {
                    self.instrument_tool(tool).into()
                } else {
                    let context = CURRENT_AUDIT_CONTEXT.try_with(Clone::clone).unwrap_or_default();
                    tracing::info!(request_id = %context.request_id, client_id = %context.client_id, client_name = %context.client_name, on_behalf_of = %context.on_behalf_of, agent = %self.agent, server = %self.server, tool = %name, decision = "denied", outcome = "ok", error_kind = "", duration_ms = 0_u64, args_sha256 = "", "MCP tool audit");
                    None
                }
            })
            .collect();
        // Denying by default is only usable if the operator can see what was denied. This is
        // the line that tells them which names to put in allowed_tools.
        if self.allowlist.is_none() && discovered > 0 {
            tracing::warn!(
                agent = %self.agent,
                server = %self.server,
                discovered,
                "MCP server offers tools but allowed_tools is not set; none are registered. Name them explicitly, or set allowed_tools = [\"*\"]"
            );
        }
        allowed
    }

    fn instrument_tool(&self, tool: DynamicTool) -> DynamicTool {
        let definition = tool.definition();
        let tool_name = definition.name.to_string();
        let agent = self.agent.clone();
        let server = self.server.clone();
        let source = tool.clone();
        let live_source = tool;
        DynamicTool::new(
            definition.name,
            definition.description,
            definition.parameters,
            move |arguments| {
                let source = source.clone();
                let agent = agent.clone();
                let server = server.clone();
                let tool_name = tool_name.clone();
                Box::pin(async move {
                    let context = CURRENT_AUDIT_CONTEXT.try_with(Clone::clone).unwrap_or_default();
                    let args_sha256 = canonical_args_sha256(&arguments);
                    let started = std::time::Instant::now();
                    tracing::info!(request_id = %context.request_id, client_id = %context.client_id, client_name = %context.client_name, on_behalf_of = %context.on_behalf_of, agent = %agent, server = %server, tool = %tool_name, decision = "allowed", outcome = "ok", error_kind = "", duration_ms = 0_u64, args_sha256 = %args_sha256, "MCP tool audit decision");
                    let result = source.run(arguments).await;
                    let error_kind = if result.is_err() { "tool_error" } else { "" };
                    tracing::info!(request_id = %context.request_id, client_id = %context.client_id, client_name = %context.client_name, on_behalf_of = %context.on_behalf_of, agent = %agent, server = %server, tool = %tool_name, decision = "allowed", outcome = if result.is_ok() { "ok" } else { "error" }, error_kind, duration_ms = started.elapsed().as_millis() as u64, args_sha256 = %args_sha256, "MCP tool audit invocation");
                    result
                })
            },
        )
        .with_annotations(definition.annotations)
        .with_liveness(move || live_source.is_live())
    }
}

fn canonical_args_sha256(arguments: &Value) -> String {
    let canonical = serde_json::to_vec(arguments).unwrap_or_default();
    format!("{:x}", Sha256::digest(canonical))
}

impl ManagedToolSink for FilteredToolSink {
    fn add_managed_tools(&self, tools: Vec<DynamicTool>) -> HashMap<String, ManagedToolToken> {
        self.inner.add_managed_tools(self.filter(tools))
    }

    fn reconcile_managed_tools(
        &self,
        expected: HashMap<String, ManagedToolToken>,
        tools: Vec<DynamicTool>,
    ) -> HashMap<String, ManagedToolToken> {
        self.inner
            .reconcile_managed_tools(expected, self.filter(tools))
    }
}

/// Every tool is denied unless it is named, or unless the allowlist is exactly `["*"]`.
/// A missing `allowed_tools` therefore registers nothing: exposure is a decision written in
/// the config, never the absence of a field.
const ALLOW_ALL: &str = "*";

fn tool_allowed(allowlist: Option<&HashSet<String>>, name: &str) -> bool {
    allowlist.is_some_and(|allowed| allowed.contains(ALLOW_ALL) || allowed.contains(name))
}

fn read_mcp_token(variable: &str) -> Result<String> {
    let token = std::env::var(variable)
        .with_context(|| format!("reading MCP bearer token from {variable}"))?;
    if token.is_empty() {
        anyhow::bail!("MCP bearer token environment variable {variable} is empty");
    }
    Ok(token)
}

fn mcp_transport_config(
    url: String,
    token: Option<String>,
) -> Result<
    rig::tool::rmcp::rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig,
> {
    use rig::tool::rmcp::rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;

    let config = StreamableHttpClientTransportConfig::with_uri(url);
    match token {
        Some(token) if token.is_empty() => anyhow::bail!("MCP bearer token cannot be empty"),
        Some(token) => Ok(config.auth_header(token)),
        None => Ok(config),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stdio_server(env: &[(&str, &str)], inherit: &[&str]) -> McpServerConfig {
        McpServerConfig {
            url: None,
            bearer_token_env: None,
            command: Some("/bin/true".to_string()),
            args: vec![],
            working_dir: None,
            env: env
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            inherit_env: inherit.iter().map(|s| s.to_string()).collect(),
            allowed_tools: None,
            ..Default::default()
        }
    }

    fn gateway_env(name: &str) -> Option<String> {
        match name {
            "OPENAI_API_KEY" => Some("sk-secret".into()),
            "RIGGATE_API_KEY" => Some("gw-secret".into()),
            "HOME" => Some("/home/x".into()),
            _ => None,
        }
    }

    #[test]
    fn child_inherits_nothing_by_default() {
        let server = stdio_server(&[], &[]);
        assert!(child_environment(&server, gateway_env).is_empty());
    }

    #[test]
    fn child_gets_only_named_variables() {
        let server = stdio_server(&[("TZ", "UTC")], &["HOME", "NOT_SET"]);
        let vars = child_environment(&server, gateway_env);
        assert_eq!(
            vars,
            vec![
                ("HOME".to_string(), "/home/x".to_string()),
                ("TZ".to_string(), "UTC".to_string())
            ]
        );
        assert!(!vars.iter().any(|(k, _)| k.ends_with("_API_KEY")));
    }

    #[tokio::test]
    async fn spawned_child_does_not_see_gateway_secrets() {
        // Set a secret in the real process env, then spawn a real child that dumps its env.
        // SAFETY: test-only; the variable name is unique to this test.
        unsafe { std::env::set_var("RIGGATE_TEST_LEAK_CANARY", "leaked") };
        let mut server = stdio_server(&[("KEPT", "yes")], &[]);
        server.command = Some("/usr/bin/env".to_string());
        let out = stdio_command(&server, |v| std::env::var(v).ok())
            .output()
            .await
            .expect("run env");
        let text = String::from_utf8_lossy(&out.stdout);
        assert!(text.contains("KEPT=yes"), "env was: {text}");
        assert!(!text.contains("RIGGATE_TEST_LEAK_CANARY"), "leaked: {text}");
        assert!(!text.contains("PATH="), "PATH leaked: {text}");
    }

    #[test]
    fn denies_every_tool_when_no_allowlist_is_configured() {
        assert!(!tool_allowed(None, "anything"));
    }

    #[test]
    fn wildcard_allowlist_is_the_only_way_to_grant_every_tool() {
        let all = HashSet::from([ALLOW_ALL.to_string()]);
        assert!(tool_allowed(Some(&all), "anything"));
        assert!(tool_allowed(Some(&all), "delete"));
    }

    #[test]
    fn allowlist_matches_exact_tool_names_and_can_deny_all() {
        let allowed = HashSet::from(["search".to_string()]);
        assert!(tool_allowed(Some(&allowed), "search"));
        assert!(!tool_allowed(Some(&allowed), "delete"));
        assert!(!tool_allowed(Some(&HashSet::new()), "search"));
    }

    #[test]
    fn mcp_transport_sends_configured_token_as_bearer_auth() {
        let config = mcp_transport_config("http://localhost/mcp".into(), Some("secret".into()))
            .expect("valid token");
        assert_eq!(config.auth_header.as_deref(), Some("secret"));
    }

    #[test]
    fn mcp_transport_rejects_empty_token() {
        assert!(mcp_transport_config("http://localhost/mcp".into(), Some(String::new())).is_err());
    }

    #[test]
    fn filtered_sink_registers_nothing_when_no_allowlist_is_configured() {
        let handle = rig::tool::server::ToolServer::new().run();
        let sink = FilteredToolSink {
            inner: handle.clone(),
            allowlist: None,
            agent: "test".to_string(),
            server: "http://127.0.0.1:9/mcp".to_string(),
        };
        sink.add_managed_tools(vec![DynamicTool::new(
            rig::message::ToolName::new("search").expect("valid tool name"),
            "test tool",
            serde_json::json!({"type": "object"}),
            |_| Box::pin(async { Ok(rig::tool::ToolOutput::text("ok")) }),
        )]);

        assert!(!handle.snapshot().contains("search"));
    }

    #[test]
    fn tool_invocation_emits_audit_record_with_client_and_on_behalf_of() {
        let sink = FilteredToolSink {
            inner: rig::tool::server::ToolServer::new().run(),
            allowlist: Some(HashSet::from(["lookup".to_string()])),
            agent: "claims-agent".into(),
            server: TEST_SERVER.into(),
        };
        let tool = sink.instrument_tool(named_tool("lookup"));
        let args = serde_json::json!({"claim_id":"C-42"});
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        let logs = capture_logs(|| {
            assert!(runtime
                .block_on(with_audit_context(
                    AuditContext {
                        request_id: "chatcmpl-test".into(),
                        client_id: "claims-app".into(),
                        client_name: "Claims application".into(),
                        on_behalf_of: "user-42".into(),
                    },
                    tool.run(args),
                ))
                .is_ok());
        });
        assert!(logs.contains("client_id=claims-app"), "captured: {logs}");
        assert!(logs.contains("on_behalf_of=user-42"), "captured: {logs}");
        assert!(logs.contains("decision=\"allowed\""), "captured: {logs}");
        assert!(
            logs.contains("MCP tool audit invocation"),
            "captured: {logs}"
        );
        assert!(!logs.contains("C-42"), "raw arguments leaked: {logs}");
    }

    #[test]
    fn filtered_sink_registers_every_tool_for_the_wildcard() {
        let handle = rig::tool::server::ToolServer::new().run();
        let sink = FilteredToolSink {
            inner: handle.clone(),
            allowlist: Some(HashSet::from([ALLOW_ALL.to_string()])),
            agent: "test".to_string(),
            server: "http://127.0.0.1:9/mcp".to_string(),
        };
        let tool = |name: &'static str| {
            DynamicTool::new(
                rig::message::ToolName::new(name).expect("valid tool name"),
                "test tool",
                serde_json::json!({"type": "object"}),
                |_| Box::pin(async { Ok(rig::tool::ToolOutput::text("ok")) }),
            )
        };
        sink.add_managed_tools(vec![tool("search"), tool("delete")]);

        let catalog = handle.snapshot();
        assert!(catalog.contains("search"));
        assert!(catalog.contains("delete"));
    }

    #[test]
    fn filtered_sink_registers_only_allowlisted_mcp_tools() {
        let handle = rig::tool::server::ToolServer::new().run();
        let sink = FilteredToolSink {
            inner: handle.clone(),
            allowlist: Some(HashSet::from(["search".to_string()])),
            agent: "test".to_string(),
            server: "http://127.0.0.1:9/mcp".to_string(),
        };
        let tool = |name: &'static str| {
            DynamicTool::new(
                rig::message::ToolName::new(name).expect("valid tool name"),
                "test tool",
                serde_json::json!({"type": "object"}),
                |_| Box::pin(async { Ok(rig::tool::ToolOutput::text("ok")) }),
            )
        };

        let expected = sink.add_managed_tools(vec![tool("search"), tool("delete")]);
        assert!(handle.snapshot().contains("search"));
        sink.reconcile_managed_tools(expected, vec![tool("delete")]);

        let catalog = handle.snapshot();
        assert!(!catalog.contains("search"));
        assert!(!catalog.contains("delete"));
    }

    /// Captures what `tracing` actually emits. The security claim is "a denied tool is
    /// visible in the log", and a test that only inspects the tool catalog cannot see a log
    /// line at all. See `testlog` for why this does not use a per-test subscriber.
    use crate::testlog::capture_logs;

    const TEST_SERVER: &str = "http://mcp.internal:8931/mcp";

    fn named_tool(name: &'static str) -> DynamicTool {
        DynamicTool::new(
            rig::message::ToolName::new(name).expect("valid tool name"),
            "test tool",
            serde_json::json!({"type": "object"}),
            |_| Box::pin(async { Ok(rig::tool::ToolOutput::text("ok")) }),
        )
    }

    /// The negative case that decides a security review: an MCP server advertises a tool,
    /// `allowed_tools` omits it, so it must never be registered — and the refusal must be
    /// greppable, naming the server and the tool.
    ///
    /// Deliberately model-free. Asking a language model to reach for a forbidden tool would
    /// make this assertion depend on model behaviour, which is not stable across runs or
    /// models, and a flaky security test teaches you to re-run until it goes green.
    #[test]
    fn denied_tool_emits_audit_record_with_decision_denied() {
        let handle = rig::tool::server::ToolServer::new().run();
        let sink = FilteredToolSink {
            inner: handle.clone(),
            allowlist: Some(HashSet::from(["search".to_string()])),
            agent: "bank-agent".to_string(),
            server: TEST_SERVER.to_string(),
        };

        let logs = capture_logs(|| {
            sink.add_managed_tools(vec![named_tool("search"), named_tool("delete")]);
        });

        assert!(
            handle.snapshot().contains("search"),
            "the allowlisted tool must still be registered"
        );
        assert!(
            !handle.snapshot().contains("delete"),
            "a tool outside allowed_tools must never reach the catalog"
        );

        assert!(
            logs.contains("tool=delete") && logs.contains("decision=\"denied\""),
            "the denial must name the tool that was refused; captured:\n{logs}"
        );
        assert!(
            logs.contains(TEST_SERVER),
            "the denial must name the MCP server, or it cannot be reconciled against the \
             config that produced it; captured:\n{logs}"
        );
        assert!(
            logs.contains("agent=bank-agent"),
            "the denial must name the agent; captured:\n{logs}"
        );
    }

    /// Deny-by-default is only usable if an operator who forgot `allowed_tools` is told so,
    /// and told which server to go and fix.
    #[test]
    fn an_absent_allowlist_registers_nothing_and_warns_with_the_server_to_fix() {
        let handle = rig::tool::server::ToolServer::new().run();
        let sink = FilteredToolSink {
            inner: handle.clone(),
            allowlist: None,
            agent: "bank-agent".to_string(),
            server: TEST_SERVER.to_string(),
        };

        let logs = capture_logs(|| {
            sink.add_managed_tools(vec![named_tool("search")]);
        });

        assert!(!handle.snapshot().contains("search"));
        assert!(
            logs.contains("allowed_tools is not set"),
            "an operator who omitted the field must be told why nothing registered; \
             captured:\n{logs}"
        );
        assert!(
            logs.contains(TEST_SERVER),
            "the warning must name the server to fix; captured:\n{logs}"
        );
    }

    // A fake stdio MCP server. Each start appends its pid to $FAKE_LOG; with $FAKE_CRASH_ONCE
    // set it dies right after its first tools/list, which is what the restart test needs.
    const FAKE_SERVER: &str = r#"
import json, os, sys
with open(os.environ["FAKE_LOG"], "a") as f:
    f.write(str(os.getpid()) + "\n")
sys.stderr.write("fake server booted\n"); sys.stderr.flush()
import signal, subprocess
if os.environ.get("FAKE_GRANDCHILD"):
    gc = subprocess.Popen(["sleep", "300"])
    with open(os.environ["FAKE_GRANDCHILD"], "w") as f:
        f.write(str(gc.pid))
if os.environ.get("FAKE_STUBBORN"):
    # Serves normally, but once stdin closes it keeps running until SIGTERM, which it records.
    def term(*_):
        open(os.environ["FAKE_STUBBORN"], "w").close()
        os._exit(0)
    signal.signal(signal.SIGTERM, term)
crash = os.environ.get("FAKE_CRASH_ONCE")
first = not os.path.exists(crash) if crash else False
for line in sys.stdin:
    msg = json.loads(line)
    method, mid = msg.get("method"), msg.get("id")
    if mid is None:
        continue
    if method == "initialize":
        result = {"protocolVersion": msg["params"]["protocolVersion"],
                  "capabilities": {"tools": {}},
                  "serverInfo": {"name": "fake", "version": "0"}}
    elif method == "tools/list":
        result = {"tools": [{"name": "get_time", "description": "t",
                             "inputSchema": {"type": "object"}}]}
    else:
        result = {}
    sys.stdout.write(json.dumps({"jsonrpc": "2.0", "id": mid, "result": result}) + "\n")
    sys.stdout.flush()
    if method == "tools/list" and first:
        open(crash, "w").close()
        os._exit(1)
if os.environ.get("FAKE_STUBBORN"):
    import time
    while True:
        time.sleep(0.1)
"#;

    struct Fake {
        dir: std::path::PathBuf,
        server: McpServerConfig,
    }

    fn fake(name: &str, crash_once: bool) -> Fake {
        let dir = std::env::temp_dir().join(format!("riggate-{name}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let script = dir.join("server.py");
        std::fs::write(&script, FAKE_SERVER).unwrap();
        let mut env = std::collections::BTreeMap::new();
        env.insert(
            "FAKE_LOG".to_string(),
            dir.join("log").display().to_string(),
        );
        if crash_once {
            env.insert(
                "FAKE_CRASH_ONCE".to_string(),
                dir.join("crashed").display().to_string(),
            );
        }
        let server = McpServerConfig {
            url: None,
            bearer_token_env: None,
            // Bare name on purpose: resolved on the gateway's PATH although the child gets none.
            command: Some("python3".to_string()),
            args: vec!["-I".into(), script.display().to_string()],
            working_dir: None,
            env,
            inherit_env: vec![],
            allowed_tools: Some(vec!["*".to_string()]),
            ..Default::default()
        };
        Fake { dir, server }
    }

    fn starts(dir: &std::path::Path) -> Vec<u32> {
        std::fs::read_to_string(dir.join("log"))
            .unwrap_or_default()
            .lines()
            .filter_map(|l| l.parse().ok())
            .collect()
    }

    fn pid_alive(pid: u32) -> bool {
        std::path::Path::new(&format!("/proc/{pid}")).exists()
            && !std::fs::read_to_string(format!("/proc/{pid}/stat"))
                .map(|s| s.contains(") Z"))
                .unwrap_or(true)
    }

    async fn wait_for<F: Fn() -> bool>(what: &str, f: F) {
        for _ in 0..200 {
            if f() {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        panic!("timed out waiting for {what}");
    }

    async fn tool_names(tools: &ToolServerHandle) -> Vec<String> {
        tools
            .tool_defs(None)
            .await
            .unwrap_or_default()
            .into_iter()
            .map(|d| d.name.to_string())
            .collect()
    }

    fn fast_heartbeat() -> Heartbeat {
        Heartbeat {
            interval: std::time::Duration::from_millis(100),
            timeout: std::time::Duration::from_millis(500),
            failures: 2,
        }
    }

    fn fast() -> Backoff {
        Backoff {
            base: std::time::Duration::from_millis(50),
            max: std::time::Duration::from_millis(200),
            healthy_after: std::time::Duration::from_secs(60),
        }
    }

    #[tokio::test]
    async fn stdio_server_registers_tools_and_is_reaped_on_shutdown() {
        let f = fake("shutdown", false);
        let tools = rig::tool::server::ToolServer::new().run();
        let (stop, stop_rx) = tokio::sync::watch::channel(false);
        let handle = start_stdio(&f.server, &tools, "a", stop_rx, fast(), Default::default())
            .await
            .unwrap();
        assert_eq!(tool_names(&tools).await, vec!["get_time"]);
        let pid = starts(&f.dir)[0];
        assert!(pid_alive(pid));

        stop.send(true).unwrap();
        handle.await.unwrap();
        wait_for("child to be reaped", || !pid_alive(pid)).await;
        let _ = std::fs::remove_dir_all(&f.dir);
    }

    #[tokio::test]
    async fn a_crashed_stdio_server_is_restarted_and_its_tools_return() {
        let f = fake("restart", true);
        let tools = rig::tool::server::ToolServer::new().run();
        let (stop, stop_rx) = tokio::sync::watch::channel(false);
        let handle = start_stdio(&f.server, &tools, "a", stop_rx, fast(), Default::default())
            .await
            .unwrap();

        wait_for("a second start", || starts(&f.dir).len() >= 2).await;
        for _ in 0..200 {
            if tool_names(&tools).await == vec!["get_time"] {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        assert_eq!(tool_names(&tools).await, vec!["get_time"]);
        let pids = starts(&f.dir);
        assert!(!pid_alive(pids[0]), "the crashed child must not linger");

        stop.send(true).unwrap();
        handle.await.unwrap();
        wait_for("restarted child to be reaped", || !pid_alive(pids[1])).await;
        let _ = std::fs::remove_dir_all(&f.dir);
    }

    fn flag() -> Arc<std::sync::atomic::AtomicBool> {
        Arc::new(std::sync::atomic::AtomicBool::new(false))
    }

    fn is_up(up: &Arc<std::sync::atomic::AtomicBool>) -> bool {
        up.load(std::sync::atomic::Ordering::SeqCst)
    }

    #[tokio::test]
    async fn stdio_readiness_is_up_while_running_and_down_once_stopped() {
        let f = fake("ready-steady", false);
        let tools = rig::tool::server::ToolServer::new().run();
        let (stop, stop_rx) = tokio::sync::watch::channel(false);
        let up = flag();
        let handle = start_stdio(&f.server, &tools, "a", stop_rx, fast(), up.clone())
            .await
            .unwrap();
        assert!(is_up(&up), "ready as soon as start_stdio returns");

        stop.send(true).unwrap();
        handle.await.unwrap();
        assert!(!is_up(&up), "not ready once stopped");
        let _ = std::fs::remove_dir_all(&f.dir);
    }

    #[tokio::test]
    async fn stdio_readiness_drops_while_a_crashed_server_is_restarting() {
        // The fake dies right after its handshake, so readiness is only observed to change,
        // never asserted at a fixed instant. The 600 ms backoff keeps the down window wide.
        let f = fake("ready-crash", true);
        let tools = rig::tool::server::ToolServer::new().run();
        let (stop, stop_rx) = tokio::sync::watch::channel(false);
        let up = flag();
        let slow = Backoff {
            base: std::time::Duration::from_millis(600),
            max: std::time::Duration::from_millis(600),
            healthy_after: std::time::Duration::from_secs(60),
        };
        let handle = start_stdio(&f.server, &tools, "a", stop_rx, slow, up.clone())
            .await
            .unwrap();

        wait_for("the crash to be noticed", || !is_up(&up)).await;
        wait_for("the restart to be ready", || is_up(&up)).await;

        stop.send(true).unwrap();
        handle.await.unwrap();
        let _ = std::fs::remove_dir_all(&f.dir);
    }

    #[tokio::test]
    async fn an_unreachable_http_server_still_fails_startup() {
        let server = McpServerConfig {
            url: Some("http://127.0.0.1:1/mcp".to_string()),
            bearer_token_env: None,
            command: None,
            args: vec![],
            working_dir: None,
            env: Default::default(),
            inherit_env: vec![],
            allowed_tools: Some(vec!["*".to_string()]),
            ..Default::default()
        };
        let tools = rig::tool::server::ToolServer::new().run();
        let (_stop, stop_rx) = tokio::sync::watch::channel(false);
        let err = start_http(
            &server,
            &tools,
            "a",
            stop_rx,
            fast(),
            Some(fast_heartbeat()),
            flag(),
        )
        .await
        .unwrap_err();
        assert!(format!("{err:#}").contains("MCP connection to"), "{err:#}");
    }

    #[tokio::test]
    async fn a_missing_command_fails_startup_instead_of_serving_without_it() {
        let mut server = fake("missing", false).server;
        server.command = Some("/nonexistent/riggate-mcp".to_string());
        let tools = rig::tool::server::ToolServer::new().run();
        let (_stop, stop_rx) = tokio::sync::watch::channel(false);
        let err = start_stdio(&server, &tools, "a", stop_rx, fast(), Default::default())
            .await
            .unwrap_err();
        assert!(
            format!("{err:#}").contains("spawning MCP server"),
            "{err:#}"
        );
    }

    #[tokio::test]
    async fn a_child_that_ignores_stdin_eof_is_sent_sigterm_before_being_killed() {
        let mut f = fake("sigterm", false);
        let marker = f.dir.join("got-sigterm");
        f.server
            .env
            .insert("FAKE_STUBBORN".into(), marker.display().to_string());
        let tools = rig::tool::server::ToolServer::new().run();
        let (stop, stop_rx) = tokio::sync::watch::channel(false);
        let handle = start_stdio(&f.server, &tools, "a", stop_rx, fast(), Default::default())
            .await
            .unwrap();
        let pid = starts(&f.dir)[0];
        let started = std::time::Instant::now();

        stop.send(true).unwrap();
        handle.await.unwrap();
        assert!(
            marker.exists(),
            "the child should have been asked to stop with SIGTERM"
        );
        wait_for("child to be reaped", || !pid_alive(pid)).await;
        assert!(started.elapsed() < std::time::Duration::from_secs(10));
        let _ = std::fs::remove_dir_all(&f.dir);
    }

    #[tokio::test]
    async fn grandchildren_do_not_outlive_the_server() {
        let mut f = fake("group", false);
        let gc_file = f.dir.join("grandchild");
        f.server
            .env
            .insert("FAKE_GRANDCHILD".into(), gc_file.display().to_string());
        let tools = rig::tool::server::ToolServer::new().run();
        let (stop, stop_rx) = tokio::sync::watch::channel(false);
        let handle = start_stdio(&f.server, &tools, "a", stop_rx, fast(), Default::default())
            .await
            .unwrap();
        let gc: u32 = std::fs::read_to_string(&gc_file)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        assert!(
            pid_alive(gc),
            "the grandchild should be running while the server is up"
        );

        stop.send(true).unwrap();
        handle.await.unwrap();
        wait_for("grandchild to be reaped", || !pid_alive(gc)).await;
        let _ = std::fs::remove_dir_all(&f.dir);
    }

    #[tokio::test]
    async fn grandchildren_of_a_crashed_server_are_cleaned_up_before_the_restart() {
        let mut f = fake("group-crash", true);
        let gc_file = f.dir.join("grandchild");
        f.server
            .env
            .insert("FAKE_GRANDCHILD".into(), gc_file.display().to_string());
        let tools = rig::tool::server::ToolServer::new().run();
        let (stop, stop_rx) = tokio::sync::watch::channel(false);
        let handle = start_stdio(&f.server, &tools, "a", stop_rx, fast(), Default::default())
            .await
            .unwrap();
        let first_gc: u32 = std::fs::read_to_string(&gc_file)
            .unwrap()
            .trim()
            .parse()
            .unwrap();

        wait_for("a second start", || starts(&f.dir).len() >= 2).await;
        wait_for("the first grandchild to be reaped", || !pid_alive(first_gc)).await;

        stop.send(true).unwrap();
        handle.await.unwrap();
        let _ = std::fs::remove_dir_all(&f.dir);
    }

    // --- a real Streamable HTTP MCP server, started and stopped on demand ---

    use rig::tool::rmcp::rmcp as mcp;

    #[derive(Clone)]
    struct TimeServer;

    impl mcp::ServerHandler for TimeServer {
        fn get_info(&self) -> mcp::model::ServerConfig {
            mcp::model::ServerConfig::new(
                mcp::model::ServerCapabilities::builder()
                    .enable_tools()
                    .build(),
            )
        }

        async fn list_tools(
            &self,
            _request: Option<mcp::model::PaginatedRequestParams>,
            _context: mcp::service::RequestContext<mcp::RoleServer>,
        ) -> Result<mcp::model::ListToolsResult, mcp::ErrorData> {
            let schema: mcp::model::JsonObject =
                serde_json::from_value(serde_json::json!({"type": "object"})).unwrap();
            Ok(mcp::model::ListToolsResult {
                tools: vec![mcp::model::Tool::new("get_time", "the time", schema)],
                ..Default::default()
            })
        }
    }

    struct HttpMcp {
        cancel: tokio_util::sync::CancellationToken,
        task: tokio::task::JoinHandle<()>,
    }

    impl HttpMcp {
        /// Serve on a fresh port. Binding port 0 here, rather than finding a free port and
        /// binding it later, leaves no window for another test to take it.
        async fn start_new() -> (HttpMcp, std::net::SocketAddr) {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            (HttpMcp::serve(listener), addr)
        }

        /// Serve again on a port this test used before. The old socket may take a moment to
        /// release, so retry the bind briefly instead of failing on the first `AddrInUse`.
        async fn restart_on(addr: std::net::SocketAddr) -> HttpMcp {
            for _ in 0..100 {
                if let Ok(listener) = tokio::net::TcpListener::bind(addr).await {
                    return HttpMcp::serve(listener);
                }
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
            panic!("could not rebind {addr}");
        }

        fn serve(listener: tokio::net::TcpListener) -> HttpMcp {
            use mcp::transport::streamable_http_server::{
                session::local::LocalSessionManager, StreamableHttpServerConfig,
                StreamableHttpService,
            };
            let cancel = tokio_util::sync::CancellationToken::new();
            let service: StreamableHttpService<TimeServer, LocalSessionManager> =
                StreamableHttpService::new(
                    || Ok(TimeServer),
                    Default::default(),
                    StreamableHttpServerConfig::default()
                        .with_sse_keep_alive(None)
                        .with_cancellation_token(cancel.child_token()),
                );
            let app = axum::Router::new().nest_service("/mcp", service);
            let token = cancel.clone();
            let task = tokio::spawn(async move {
                let _ = axum::serve(listener, app)
                    .with_graceful_shutdown(async move { token.cancelled_owned().await })
                    .await;
            });
            HttpMcp { cancel, task }
        }

        async fn stop(self) {
            self.cancel.cancel();
            let _ = tokio::time::timeout(std::time::Duration::from_secs(5), self.task).await;
        }
    }

    fn http_server(addr: std::net::SocketAddr) -> McpServerConfig {
        McpServerConfig {
            url: Some(format!("http://{addr}/mcp")),
            bearer_token_env: None,
            command: None,
            args: vec![],
            working_dir: None,
            env: Default::default(),
            inherit_env: vec![],
            allowed_tools: Some(vec!["*".to_string()]),
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn an_http_mcp_server_registers_its_tools_and_is_ready() {
        let (upstream, addr) = HttpMcp::start_new().await;
        let tools = rig::tool::server::ToolServer::new().run();
        let (stop, stop_rx) = tokio::sync::watch::channel(false);
        let up = flag();
        let handle = start_http(
            &http_server(addr),
            &tools,
            "a",
            stop_rx,
            fast(),
            Some(fast_heartbeat()),
            up.clone(),
        )
        .await
        .unwrap();
        assert!(is_up(&up));
        assert_eq!(tool_names(&tools).await, vec!["get_time"]);

        stop.send(true).unwrap();
        handle.await.unwrap();
        assert!(!is_up(&up));
        upstream.stop().await;
    }

    #[tokio::test]
    async fn a_healthy_http_mcp_server_is_not_restarted_by_the_heartbeat() {
        // Runs on the test thread's runtime, so the supervisor's log lines land in the capture.
        let capture = crate::testlog::Capture::start();
        let (upstream, addr) = HttpMcp::start_new().await;
        let tools = rig::tool::server::ToolServer::new().run();
        let (stop, stop_rx) = tokio::sync::watch::channel(false);
        let up = flag();
        let handle = start_http(
            &http_server(addr),
            &tools,
            "a",
            stop_rx,
            fast(),
            Some(fast_heartbeat()),
            up.clone(),
        )
        .await
        .unwrap();

        // Roughly ten heartbeats at 100 ms.
        tokio::time::sleep(std::time::Duration::from_millis(1_000)).await;
        assert!(is_up(&up));
        let logs = capture.logs();
        assert!(
            !logs.contains("restarting"),
            "a healthy server was restarted:\n{logs}"
        );

        stop.send(true).unwrap();
        handle.await.unwrap();
        upstream.stop().await;
    }

    #[test]
    fn heartbeat_follows_the_server_config_and_zero_turns_it_off() {
        let mut server = http_server("127.0.0.1:1".parse().unwrap());
        let default = Heartbeat::from_config(&server).unwrap();
        assert_eq!(default.interval, std::time::Duration::from_secs(15));
        assert_eq!(default.timeout, std::time::Duration::from_secs(5));
        assert_eq!(default.failures, 2);

        server.heartbeat_interval_seconds = Some(3);
        server.heartbeat_timeout_seconds = Some(1);
        let tuned = Heartbeat::from_config(&server).unwrap();
        assert_eq!(tuned.interval, std::time::Duration::from_secs(3));
        assert_eq!(tuned.timeout, std::time::Duration::from_secs(1));

        server.heartbeat_interval_seconds = Some(0);
        assert!(Heartbeat::from_config(&server).is_none());
    }

    #[tokio::test]
    async fn with_the_heartbeat_off_a_vanished_http_server_is_not_noticed() {
        // The documented cost of turning checks off: only a clean close is detected.
        let (upstream, addr) = HttpMcp::start_new().await;
        let tools = rig::tool::server::ToolServer::new().run();
        let (stop, stop_rx) = tokio::sync::watch::channel(false);
        let up = flag();
        let handle = start_http(
            &http_server(addr),
            &tools,
            "a",
            stop_rx,
            fast(),
            None,
            up.clone(),
        )
        .await
        .unwrap();
        upstream.stop().await;
        tokio::time::sleep(std::time::Duration::from_millis(800)).await;
        assert!(
            is_up(&up),
            "with no heartbeat nothing reports the server gone"
        );

        stop.send(true).unwrap();
        handle.await.unwrap();
    }

    #[tokio::test]
    async fn an_http_mcp_server_that_goes_away_and_returns_is_reconnected() {
        let (upstream, addr) = HttpMcp::start_new().await;
        let tools = rig::tool::server::ToolServer::new().run();
        let (stop, stop_rx) = tokio::sync::watch::channel(false);
        let up = flag();
        let handle = start_http(
            &http_server(addr),
            &tools,
            "a",
            stop_rx,
            fast(),
            Some(fast_heartbeat()),
            up.clone(),
        )
        .await
        .unwrap();
        assert!(is_up(&up));

        upstream.stop().await;
        wait_for("readiness to drop after the server went away", || {
            !is_up(&up)
        })
        .await;

        let upstream = HttpMcp::restart_on(addr).await;
        wait_for("readiness to return after the server came back", || {
            is_up(&up)
        })
        .await;
        assert_eq!(tool_names(&tools).await, vec!["get_time"]);

        stop.send(true).unwrap();
        handle.await.unwrap();
        upstream.stop().await;
    }
}
