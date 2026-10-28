use anyhow::{bail, Context, Result};
use serde::Deserialize;
use std::path::Path;

fn default_bind() -> String {
    "127.0.0.1:8080".to_string()
}

fn default_max_request_bytes() -> usize {
    1_048_576
}

fn default_max_concurrent_requests() -> usize {
    64
}

fn default_request_timeout_seconds() -> u64 {
    120
}

#[derive(Debug, Deserialize)]
pub struct Config {
    /// Address to bind. Defaults to loopback — you opt in to exposure.
    #[serde(default = "default_bind")]
    pub bind: String,
    #[serde(default)]
    pub agents: Vec<AgentConfig>,
    #[serde(default = "default_max_request_bytes")]
    pub max_request_bytes: usize,
    #[serde(default = "default_max_concurrent_requests")]
    pub max_concurrent_requests: usize,
    #[serde(default = "default_request_timeout_seconds")]
    pub request_timeout_seconds: u64,
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum Provider {
    /// Hosted OpenAI, Azure OpenAI, or any OpenAI-compatible endpoint.
    Openai,
    /// A local Ollama server inside your network.
    Ollama,
}

#[derive(Debug, Deserialize)]
pub struct AgentConfig {
    /// The name clients send in the `model` field.
    pub name: String,
    pub provider: Provider,
    /// Upstream model id, e.g. "gpt-5.5" or "llama3.1:8b".
    pub model: String,
    /// System prompt.
    #[serde(default)]
    pub preamble: Option<String>,
    /// Maximum model calls per request, including tool follow-ups.
    #[serde(default = "default_max_turns")]
    pub max_turns: usize,
    /// MCP servers whose tools should be available to this agent.
    #[serde(default)]
    pub mcp_servers: Vec<McpServerConfig>,
}

fn default_max_turns() -> usize {
    10
}

#[derive(Debug, Deserialize)]
pub struct McpServerConfig {
    /// Streamable HTTP endpoint for the MCP server.
    pub url: String,
    /// Environment variable containing a bearer token for this server.
    #[serde(default)]
    pub bearer_token_env: Option<String>,
    /// If set, only these discovered tool names are registered.
    #[serde(default)]
    pub allowed_tools: Option<Vec<String>>,
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        let raw = std::fs::read_to_string(path)
            .with_context(|| format!("reading config at {}", path.display()))?;
        Self::parse(&raw).with_context(|| format!("in config at {}", path.display()))
    }

    /// Parse and validate. Split from `load` so it is testable without a file.
    pub fn parse(raw: &str) -> Result<Self> {
        let cfg: Config = toml::from_str(raw)?;
        if cfg.agents.is_empty() {
            bail!("config has no [[agents]] entries");
        }
        if cfg.max_request_bytes == 0 {
            bail!("max_request_bytes must be greater than zero");
        }
        if cfg.max_concurrent_requests == 0 {
            bail!("max_concurrent_requests must be greater than zero");
        }
        if cfg.request_timeout_seconds == 0 {
            bail!("request_timeout_seconds must be greater than zero");
        }
        for a in &cfg.agents {
            if a.name.trim().is_empty() {
                bail!("an agent has an empty name");
            }
            if a.max_turns == 0 {
                bail!("agent '{}' max_turns must be greater than zero", a.name);
            }
            for server in &a.mcp_servers {
                if server.url.trim().is_empty() {
                    bail!("agent '{}' has an MCP server with an empty url", a.name);
                }
                if server
                    .bearer_token_env
                    .as_deref()
                    .is_some_and(|name| name.trim().is_empty())
                {
                    bail!(
                        "agent '{}' has an MCP server with an empty bearer_token_env",
                        a.name
                    );
                }
                if server
                    .allowed_tools
                    .as_ref()
                    .is_some_and(|tools| tools.iter().any(|name| name.trim().is_empty()))
                {
                    bail!(
                        "agent '{}' has an MCP allowlist with an empty tool name",
                        a.name
                    );
                }
            }
        }
        Ok(cfg)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const GOOD: &str = r#"
bind = "0.0.0.0:9000"

[[agents]]
name = "assistant"
provider = "openai"
model = "gpt-5.5"

[[agents]]
name = "local"
provider = "ollama"
model = "llama3.1:8b"
preamble = "Be brief."
"#;

    #[test]
    fn parses_two_agents() {
        let cfg = Config::parse(GOOD).expect("should parse");
        assert_eq!(cfg.bind, "0.0.0.0:9000");
        assert_eq!(cfg.agents.len(), 2);
        assert_eq!(cfg.agents[1].provider, Provider::Ollama);
        assert_eq!(cfg.agents[1].preamble.as_deref(), Some("Be brief."));
    }

    #[test]
    fn default_bind_is_loopback() {
        let cfg = Config::parse("[[agents]]\nname=\"a\"\nprovider=\"openai\"\nmodel=\"m\"\n")
            .expect("should parse");
        assert_eq!(cfg.bind, "127.0.0.1:8080");
    }

    #[test]
    fn rejects_empty_agent_list() {
        let err = Config::parse("bind = \"127.0.0.1:1\"")
            .unwrap_err()
            .to_string();
        assert!(err.contains("no [[agents]]"), "got: {err}");
    }

    #[test]
    fn rejects_unknown_provider() {
        assert!(
            Config::parse("[[agents]]\nname=\"a\"\nprovider=\"gemini\"\nmodel=\"m\"\n").is_err()
        );
    }

    #[test]
    fn rejects_zero_max_turns() {
        let raw = "[[agents]]\nname=\"a\"\nprovider=\"openai\"\nmodel=\"m\"\nmax_turns=0\n";
        assert!(Config::parse(raw)
            .unwrap_err()
            .to_string()
            .contains("max_turns"));
    }

    #[test]
    fn rejects_blank_agent_name() {
        let raw = "[[agents]]\nname=\"  \"\nprovider=\"openai\"\nmodel=\"m\"\n";
        assert!(Config::parse(raw)
            .unwrap_err()
            .to_string()
            .contains("empty name"));
    }

    #[test]
    fn rejects_blank_mcp_url() {
        let raw = "[[agents]]\nname=\"a\"\nprovider=\"openai\"\nmodel=\"m\"\n[[agents.mcp_servers]]\nurl=\" \"\n";
        assert!(Config::parse(raw)
            .unwrap_err()
            .to_string()
            .contains("empty url"));
    }

    #[test]
    fn defaults_request_limits_to_safe_nonzero_values() {
        let cfg = Config::parse("[[agents]]\nname=\"a\"\nprovider=\"ollama\"\nmodel=\"m\"\n")
            .expect("defaults should parse");
        assert_eq!(cfg.max_request_bytes, 1_048_576);
        assert_eq!(cfg.max_concurrent_requests, 64);
        assert_eq!(cfg.request_timeout_seconds, 120);
    }

    #[test]
    fn rejects_zero_request_timeout() {
        let raw =
            "request_timeout_seconds=0\n[[agents]]\nname=\"a\"\nprovider=\"ollama\"\nmodel=\"m\"\n";
        assert!(Config::parse(raw)
            .unwrap_err()
            .to_string()
            .contains("request_timeout_seconds"));
    }

    #[test]
    fn parses_mcp_auth_and_tool_allowlist() {
        let raw = "[[agents]]\nname=\"a\"\nprovider=\"ollama\"\nmodel=\"m\"\n[[agents.mcp_servers]]\nurl=\"http://localhost/mcp\"\nbearer_token_env=\"MCP_TOKEN\"\nallowed_tools=[\"search\", \"read\"]\n";
        let cfg = Config::parse(raw).expect("MCP policy should parse");
        let server = &cfg.agents[0].mcp_servers[0];
        assert_eq!(server.bearer_token_env.as_deref(), Some("MCP_TOKEN"));
        assert_eq!(
            server.allowed_tools.as_deref(),
            Some(["search".to_string(), "read".to_string()].as_slice())
        );
    }

    #[test]
    fn rejects_empty_mcp_allowlist_tool() {
        let raw = "[[agents]]\nname=\"a\"\nprovider=\"ollama\"\nmodel=\"m\"\n[[agents.mcp_servers]]\nurl=\"http://localhost/mcp\"\nallowed_tools=[\" \"]\n";
        assert!(Config::parse(raw)
            .unwrap_err()
            .to_string()
            .contains("empty tool name"));
    }
}
