use anyhow::{Context, Result};
use rig::prelude::*;
use rig::providers::{ollama, openai};
use std::collections::HashMap;
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
                mcp_services.push(connect_mcp(server, &tool_server).await.with_context(|| {
                    format!("connecting MCP server {} for agent '{}'", server.url, cfg.name)
                })?);
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

fn build_agent(builder: AgentBuilder, cfg: &AgentConfig) -> Agent {
    match &cfg.preamble {
        Some(p) => builder.preamble(p).build(),
        None => builder.build(),
    }
}

async fn connect_mcp(
    server: &McpServerConfig,
    tool_server: &rig::tool::server::ToolServerHandle,
) -> Result<Box<dyn Send>> {
    use rig::tool::rmcp::{
        rmcp::{
            model::{ClientCapabilities, ClientInfo, Implementation},
            transport::StreamableHttpClientTransport,
        },
        McpClientHandler,
    };

    let client_info = ClientInfo::new(
        ClientCapabilities::default(),
        Implementation::new("riggate", env!("CARGO_PKG_VERSION")),
    );
    let handler = McpClientHandler::new(client_info, tool_server.clone());
    let transport = StreamableHttpClientTransport::from_uri(server.url.clone());
    let connection = handler
        .connect(transport)
        .await
        .with_context(|| format!("MCP connection to {} failed", server.url))?;
    Ok(Box::new(connection))
}
