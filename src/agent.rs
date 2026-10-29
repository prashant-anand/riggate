use anyhow::{Context, Result};
use rig::prelude::*;
use rig::providers::{ollama, openai};
use rig::tool::{server::ToolServerHandle, DynamicTool};
use rig_core::tool::managed::{ManagedToolSink, ManagedToolToken};
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use crate::config::{AgentConfig, McpServerConfig, Provider};

/// Every agent is built once at boot. Requests only borrow them.
pub type Registry = Arc<AgentRegistry>;

pub struct AgentRegistry {
    agents: HashMap<String, Agent>,
    // MCP services must stay alive for registered tools to keep working.
    _mcp_services: Mutex<Vec<Box<dyn Send>>>,
}

impl AgentRegistry {
    pub fn get(&self, name: &str) -> Option<&Agent> {
        self.agents.get(name)
    }

    pub fn keys(&self) -> impl Iterator<Item = &String> {
        self.agents.keys()
    }
}

pub async fn build(configs: &[AgentConfig]) -> Result<Registry> {
    let mut agents = HashMap::new();
    let mut mcp_services: Vec<Box<dyn Send>> = Vec::new();

    for cfg in configs {
        let builder = match cfg.provider {
            // `from_env` reads OPENAI_API_KEY and, if set, OPENAI_BASE_URL —
            // which is how you point it at Azure, vLLM, or an internal gateway.
            Provider::Openai => {
                let client = openai::OpenAI::from_env().context(
                    "an openai agent needs OPENAI_API_KEY (and optionally OPENAI_BASE_URL)",
                )?;
                AgentBuilder::new(client.completion(cfg.model.as_str()))
            }
            // `from_env` reads OLLAMA_API_BASE_URL and OLLAMA_API_KEY, both optional.
            Provider::Ollama => {
                let client = ollama::Ollama::from_env()
                    .context("an ollama agent could not read OLLAMA_API_BASE_URL")?;
                AgentBuilder::new(client.completion(cfg.model.as_str()))
            }
        };

        let mut builder = builder.default_max_turns(cfg.max_turns);
        if !cfg.mcp_servers.is_empty() {
            let tool_server = rig::tool::server::ToolServer::new().run();
            for server in &cfg.mcp_servers {
                mcp_services.push(
                    connect_mcp(server, &tool_server, &cfg.name)
                        .await
                        .with_context(|| {
                            format!(
                                "connecting MCP server {} for agent '{}'",
                                server.url, cfg.name
                            )
                        })?,
                );
            }
            builder = builder.tool_server_handle(tool_server);
        }
        let agent = build_agent(builder, cfg);
        agents.insert(cfg.name.clone(), agent);
    }

    Ok(Arc::new(AgentRegistry {
        agents,
        _mcp_services: Mutex::new(mcp_services),
    }))
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
    Arc::new(AgentRegistry {
        agents,
        _mcp_services: Mutex::new(Vec::new()),
    })
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
    Arc::new(AgentRegistry {
        agents,
        _mcp_services: Mutex::new(Vec::new()),
    })
}

fn build_agent(builder: AgentBuilder, cfg: &AgentConfig) -> Agent {
    match &cfg.preamble {
        Some(p) => builder.preamble(p).build(),
        None => builder.build(),
    }
}

async fn connect_mcp(
    server: &McpServerConfig,
    tool_server: &rig::tool::server::ToolServerHandle,
    agent: &str,
) -> Result<Box<dyn Send>> {
    use rig::tool::rmcp::{
        rmcp::{
            model::{ClientCapabilities, ClientConfig, Implementation},
            transport::StreamableHttpClientTransport,
        },
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
    let sink = FilteredToolSink {
        inner: tool_server.clone(),
        allowlist,
        agent: agent.to_string(),
        server: server.url.clone(),
    };
    let handler = McpClientHandler::new(client_info, sink);
    let token = server
        .bearer_token_env
        .as_deref()
        .map(read_mcp_token)
        .transpose()?;
    let transport_config = mcp_transport_config(server.url.clone(), token)?;
    let transport = StreamableHttpClientTransport::from_config(transport_config);
    let connection = handler
        .connect(transport)
        .await
        .with_context(|| format!("MCP connection to {} failed", server.url))?;
    Ok(Box::new(connection))
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
                tracing::info!(agent = %self.agent, server = %self.server, tool = %name, allowed = is_allowed, "MCP tool policy evaluated");
                is_allowed.then(|| self.instrument_tool(tool))
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
                    tracing::info!(agent = %agent, server = %server, tool = %tool_name, "MCP tool invocation started");
                    let result = source.run(arguments).await;
                    tracing::info!(agent = %agent, server = %server, tool = %tool_name, succeeded = result.is_ok(), "MCP tool invocation finished");
                    result
                })
            },
        )
        .with_annotations(definition.annotations)
        .with_liveness(move || live_source.is_live())
    }
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

    /// Captures what `tracing` actually emits.
    ///
    /// The security claim is "a denied tool is visible in the log", and a test that only
    /// inspects the tool catalog cannot see a log line at all. `tracing` caches callsite
    /// interest globally, so a callsite first evaluated while no subscriber was installed
    /// can stay permanently disabled and the assertion would fail on a test-ordering
    /// accident. `rebuild_interest_cache` is the documented reset, and it only takes effect
    /// inside the subscriber's scope, so it is called there.
    fn capture_logs(f: impl FnOnce()) -> String {
        use std::io::Write;
        use std::sync::{Arc, Mutex};

        #[derive(Clone)]
        struct Buffer(Arc<Mutex<Vec<u8>>>);

        impl Write for Buffer {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                self.0
                    .lock()
                    .expect("log buffer is not poisoned")
                    .extend_from_slice(bytes);
                Ok(bytes.len())
            }

            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }

        impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Buffer {
            type Writer = Buffer;

            fn make_writer(&'a self) -> Self::Writer {
                self.clone()
            }
        }

        let buffer = Buffer(Arc::new(Mutex::new(Vec::new())));
        let subscriber = tracing_subscriber::fmt()
            .with_writer(buffer.clone())
            .with_ansi(false)
            .with_max_level(tracing::Level::TRACE)
            .finish();
        tracing::subscriber::with_default(subscriber, || {
            tracing::callsite::rebuild_interest_cache();
            f();
        });

        let captured = buffer.0.lock().expect("log buffer is not poisoned").clone();
        String::from_utf8_lossy(&captured).into_owned()
    }

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
    fn a_tool_offered_but_not_allowlisted_is_refused_and_the_refusal_names_server_and_tool() {
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
            logs.contains("tool=delete") && logs.contains("allowed=false"),
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
}
