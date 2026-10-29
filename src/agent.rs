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
}

impl FilteredToolSink {
    fn filter(&self, tools: Vec<DynamicTool>) -> Vec<DynamicTool> {
        let discovered = tools.len();
        let allowed: Vec<DynamicTool> = tools
            .into_iter()
            .filter_map(|tool| {
                let name = tool.name();
                let is_allowed = tool_allowed(self.allowlist.as_ref(), name.as_ref());
                tracing::info!(agent = %self.agent, tool = %name, allowed = is_allowed, "MCP tool policy evaluated");
                is_allowed.then(|| self.instrument_tool(tool))
            })
            .collect();
        // Denying by default is only usable if the operator can see what was denied. This is
        // the line that tells them which names to put in allowed_tools.
        if self.allowlist.is_none() && discovered > 0 {
            tracing::warn!(
                agent = %self.agent,
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
        let source = tool.clone();
        let live_source = tool;
        DynamicTool::new(
            definition.name,
            definition.description,
            definition.parameters,
            move |arguments| {
                let source = source.clone();
                let agent = agent.clone();
                let tool_name = tool_name.clone();
                Box::pin(async move {
                    tracing::info!(agent = %agent, tool = %tool_name, allowed = true, "MCP tool invocation started");
                    let result = source.run(arguments).await;
                    tracing::info!(agent = %agent, tool = %tool_name, succeeded = result.is_ok(), "MCP tool invocation finished");
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
}
