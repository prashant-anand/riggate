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

fn default_shutdown_grace_seconds() -> u64 {
    10
}

fn default_log_targets() -> Vec<LogTarget> {
    vec![LogTarget::Stderr]
}

#[derive(Debug, Deserialize)]
pub struct Config {
    /// Address to bind. Defaults to loopback — you opt in to exposure.
    #[serde(default = "default_bind")]
    pub bind: String,
    #[serde(default)]
    pub agents: Vec<AgentConfig>,
    #[serde(default)]
    pub clients: Vec<ClientConfig>,
    #[serde(default = "default_max_request_bytes")]
    pub max_request_bytes: usize,
    #[serde(default = "default_max_concurrent_requests")]
    pub max_concurrent_requests: usize,
    #[serde(default = "default_request_timeout_seconds")]
    pub request_timeout_seconds: u64,
    /// How long open requests may finish after SIGINT/SIGTERM before the process exits.
    /// A stream can be mid-answer, so a shutdown that never waits loses work; a shutdown that
    /// waits forever is a SIGKILL from the service manager.
    #[serde(default = "default_shutdown_grace_seconds")]
    pub shutdown_grace_seconds: u64,
    #[serde(default)]
    pub strict_requests: bool,
    #[serde(default = "default_log_targets")]
    pub log_targets: Vec<LogTarget>,
}

#[derive(Debug, Deserialize)]
pub struct ClientConfig {
    pub id: String,
    #[serde(default)]
    pub name: Option<String>,
    pub token_env: String,
    #[serde(default)]
    pub allowed_agents: Option<Vec<String>>,
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum LogTarget {
    Stdout,
    Stderr,
    Otlp,
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum Provider {
    /// Hosted OpenAI, Azure OpenAI, or any OpenAI-compatible endpoint.
    Openai,
    /// A local Ollama server inside your network.
    Ollama,
    /// Anthropic Claude, or an Anthropic-compatible endpoint set with `ANTHROPIC_BASE_URL`.
    Anthropic,
}

/// Which OpenAI endpoint an `openai` agent talks to.
#[derive(Debug, Clone, Copy, Default, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum Wire {
    /// `POST /responses`, OpenAI's current API. The default.
    #[default]
    Responses,
    /// `POST /chat/completions`, which every OpenAI-compatible server serves.
    Chat,
}

#[derive(Debug, Deserialize)]
pub struct AgentConfig {
    /// The name clients send in the `model` field.
    pub name: String,
    pub provider: Provider,
    /// Upstream model id, e.g. "gpt-5.5" or "llama3.1:8b".
    pub model: String,
    /// Which OpenAI endpoint to use; only valid with `provider = "openai"`. Servers such as vLLM
    /// or LiteLLM usually offer only `chat`.
    #[serde(default)]
    pub wire: Option<Wire>,
    /// System prompt.
    #[serde(default)]
    pub preamble: Option<String>,
    /// Maximum model calls per request, including tool follow-ups.
    #[serde(default = "default_max_turns")]
    pub max_turns: usize,
    /// MCP servers whose tools should be available to this agent.
    #[serde(default)]
    pub mcp_servers: Vec<McpServerConfig>,
    /// Concurrent runs allowed for this agent, in addition to the server-wide
    /// `max_concurrent_requests`. Cannot exceed it. Unset means only the server-wide limit.
    #[serde(default)]
    pub max_concurrent_requests: Option<usize>,
    /// Deadline in seconds for one run of this agent, in addition to the server-wide
    /// `request_timeout_seconds`. Cannot exceed it. Unset means only the server-wide limit.
    #[serde(default)]
    pub request_timeout_seconds: Option<u64>,
}

fn default_max_turns() -> usize {
    10
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct McpServerConfig {
    /// Streamable HTTP endpoint for the MCP server. Exactly one of `url` or `command`.
    #[serde(default)]
    pub url: Option<String>,
    /// Environment variable containing a bearer token for this server (HTTP only).
    #[serde(default)]
    pub bearer_token_env: Option<String>,
    /// Program to run as a stdio MCP server. Exactly one of `url` or `command`. A bare name
    /// is looked up on the gateway's own `PATH`; the child does not receive `PATH` unless
    /// `inherit_env` names it (needed by launchers such as `npx` or `uvx` that start more
    /// programs). An absolute path avoids depending on the gateway's `PATH` at all.
    #[serde(default)]
    pub command: Option<String>,
    /// Arguments for `command`.
    #[serde(default)]
    pub args: Vec<String>,
    /// Working directory for `command`; defaults to the gateway's.
    #[serde(default)]
    pub working_dir: Option<String>,
    /// Variables set in the child, as literal values. Nothing else reaches the child except
    /// what `inherit_env` names.
    #[serde(default)]
    pub env: std::collections::BTreeMap<String, String>,
    /// Names of gateway variables copied into the child. Everything not named here or in
    /// `env` is withheld, so provider and gateway keys never leak by default.
    #[serde(default)]
    pub inherit_env: Vec<String>,
    /// If set, only these discovered tool names are registered.
    #[serde(default)]
    pub allowed_tools: Option<Vec<String>>,
    /// Seconds between liveness checks of a `url` server; `0` turns them off. Default 15.
    /// A server that vanishes is noticed after roughly twice this plus the timeout.
    #[serde(default)]
    pub heartbeat_interval_seconds: Option<u64>,
    /// Seconds a liveness check may take before it counts as failed. Default 5.
    #[serde(default)]
    pub heartbeat_timeout_seconds: Option<u64>,
}

impl McpServerConfig {
    /// Human-readable identity used in logs and errors: the URL, or `stdio:<command>`.
    pub fn display_name(&self) -> String {
        match (&self.url, &self.command) {
            (Some(url), _) => url.clone(),
            (None, Some(command)) => format!("stdio:{command}"),
            (None, None) => "<unconfigured>".to_string(),
        }
    }

    fn validate(&self, agent: &str) -> Result<()> {
        match (&self.url, &self.command) {
            (Some(_), Some(_)) => bail!(
                "agent '{agent}' has an MCP server with both url and command; set exactly one"
            ),
            (None, None) => bail!(
                "agent '{agent}' has an MCP server with neither url nor command; set exactly one"
            ),
            (Some(url), None) => {
                if url.trim().is_empty() {
                    bail!("agent '{agent}' has an MCP server with an empty url");
                }
                if self.heartbeat_timeout_seconds == Some(0) {
                    bail!(
                        "agent '{agent}' has an MCP server with heartbeat_timeout_seconds = 0; use heartbeat_interval_seconds = 0 to turn checks off"
                    );
                }
                if self.has_stdio_fields() {
                    bail!(
                        "agent '{agent}' sets args, working_dir, env or inherit_env on a url MCP server; those only apply to command"
                    );
                }
            }
            (None, Some(command)) => {
                if command.trim().is_empty() {
                    bail!("agent '{agent}' has an MCP server with an empty command");
                }
                if self.heartbeat_interval_seconds.is_some()
                    || self.heartbeat_timeout_seconds.is_some()
                {
                    bail!(
                        "agent '{agent}' sets a heartbeat on a command MCP server; a child's exit is detected directly, so heartbeats only apply to url"
                    );
                }
                if self.bearer_token_env.is_some() {
                    bail!(
                        "agent '{agent}' sets bearer_token_env on a command MCP server; it only applies to url"
                    );
                }
                if self
                    .working_dir
                    .as_deref()
                    .is_some_and(|dir| dir.trim().is_empty())
                {
                    bail!("agent '{agent}' has an MCP server with an empty working_dir");
                }
                for name in self.env.keys().chain(self.inherit_env.iter()) {
                    if name.trim().is_empty() || name.contains('=') || name.contains('\0') {
                        bail!(
                            "agent '{agent}' has an MCP server with an invalid environment variable name {name:?}"
                        );
                    }
                }
                if let Some(name) = self.inherit_env.iter().find(|n| self.env.contains_key(*n)) {
                    bail!(
                        "agent '{agent}' names {name} in both env and inherit_env; choose one source"
                    );
                }
            }
        }
        Ok(())
    }

    fn has_stdio_fields(&self) -> bool {
        !self.args.is_empty()
            || self.working_dir.is_some()
            || !self.env.is_empty()
            || !self.inherit_env.is_empty()
    }
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
        if cfg.shutdown_grace_seconds == 0 {
            bail!("shutdown_grace_seconds must be greater than zero");
        }
        if cfg.log_targets.is_empty() {
            bail!("log_targets must contain at least one target");
        }
        if cfg
            .log_targets
            .iter()
            .enumerate()
            .any(|(i, target)| cfg.log_targets[..i].contains(target))
        {
            bail!("log_targets contains a duplicate target");
        }
        for a in &cfg.agents {
            if a.name.trim().is_empty() {
                bail!("an agent has an empty name");
            }
            if a.max_turns == 0 {
                bail!("agent '{}' max_turns must be greater than zero", a.name);
            }
            // An agent limit is a further restriction. One above the server-wide value would
            // never take effect, and silently ignoring a configured number hides a mistake.
            if let Some(limit) = a.max_concurrent_requests {
                if limit == 0 {
                    bail!(
                        "agent '{}' max_concurrent_requests must be greater than zero",
                        a.name
                    );
                }
                if limit > cfg.max_concurrent_requests {
                    bail!(
                        "agent '{}' max_concurrent_requests ({limit}) exceeds the server-wide limit ({}); it can only lower it",
                        a.name,
                        cfg.max_concurrent_requests
                    );
                }
            }
            if let Some(seconds) = a.request_timeout_seconds {
                if seconds == 0 {
                    bail!(
                        "agent '{}' request_timeout_seconds must be greater than zero",
                        a.name
                    );
                }
                if seconds > cfg.request_timeout_seconds {
                    bail!(
                        "agent '{}' request_timeout_seconds ({seconds}) exceeds the server-wide timeout ({}); it can only lower it",
                        a.name,
                        cfg.request_timeout_seconds
                    );
                }
            }
            if a.wire.is_some() && a.provider != Provider::Openai {
                bail!(
                    "agent '{}' sets wire, which only applies to provider = \"openai\"",
                    a.name
                );
            }
            for server in &a.mcp_servers {
                server.validate(&a.name)?;
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
                // "*" means every tool, so mixing it with names hides which decision was
                // actually made. Accept it alone or not at all.
                if server.allowed_tools.as_ref().is_some_and(|tools| {
                    tools.len() > 1 && tools.iter().any(|name| name.trim() == "*")
                }) {
                    bail!(
                        "agent '{}' mixes \"*\" with tool names in an MCP allowlist; use \"*\" alone or name the tools",
                        a.name
                    );
                }
            }
        }
        let agent_names: std::collections::HashSet<_> =
            cfg.agents.iter().map(|agent| agent.name.as_str()).collect();
        let mut client_ids = std::collections::HashSet::new();
        let mut token_envs = std::collections::HashSet::new();
        for client in &cfg.clients {
            if client.id.trim().is_empty() {
                bail!("a client has an empty id");
            }
            if !client_ids.insert(&client.id) {
                bail!("duplicate client id '{}'", client.id);
            }
            if client.token_env.trim().is_empty() {
                bail!("client '{}' has an empty token_env", client.id);
            }
            if !token_envs.insert(&client.token_env) {
                bail!("duplicate client token_env '{}'", client.token_env);
            }
            if let Some(agents) = &client.allowed_agents {
                for agent in agents {
                    if !agent_names.contains(agent.as_str()) {
                        bail!("client '{}' allows unknown agent '{}'", client.id, agent);
                    }
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
    fn parses_client_identity_and_agent_allowlist() {
        let raw = format!(
            "{GOOD}\n[[clients]]\nid=\"claims\"\nname=\"Claims App\"\ntoken_env=\"CLAIMS_TOKEN\"\nallowed_agents=[\"assistant\"]\n"
        );
        let cfg = Config::parse(&raw).expect("client config should parse");
        assert_eq!(cfg.clients[0].id, "claims");
        assert_eq!(cfg.clients[0].name.as_deref(), Some("Claims App"));
        assert_eq!(
            cfg.clients[0].allowed_agents.as_deref(),
            Some(["assistant".to_string()].as_slice())
        );
    }

    #[test]
    fn rejects_duplicate_client_ids_and_token_environment_names() {
        let duplicate_id = format!("{GOOD}\n[[clients]]\nid=\"a\"\ntoken_env=\"TOKEN_A\"\n[[clients]]\nid=\"a\"\ntoken_env=\"TOKEN_B\"\n");
        assert!(Config::parse(&duplicate_id)
            .unwrap_err()
            .to_string()
            .contains("duplicate client id"));
        let duplicate_env = format!("{GOOD}\n[[clients]]\nid=\"a\"\ntoken_env=\"TOKEN\"\n[[clients]]\nid=\"b\"\ntoken_env=\"TOKEN\"\n");
        assert!(Config::parse(&duplicate_env)
            .unwrap_err()
            .to_string()
            .contains("duplicate client token_env"));
    }

    #[test]
    fn rejects_unknown_client_agent_allowlist_entry() {
        let raw = format!(
            "{GOOD}\n[[clients]]\nid=\"a\"\ntoken_env=\"TOKEN\"\nallowed_agents=[\"missing\"]\n"
        );
        assert!(Config::parse(&raw)
            .unwrap_err()
            .to_string()
            .contains("unknown agent 'missing'"));
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
    fn accepts_the_anthropic_provider() {
        let cfg = Config::parse(
            "[[agents]]\nname=\"a\"\nprovider=\"anthropic\"\nmodel=\"claude-sonnet-5-5\"\n",
        )
        .unwrap();
        assert_eq!(cfg.agents[0].provider, Provider::Anthropic);
    }

    #[test]
    fn wire_defaults_to_unset_and_accepts_chat_and_responses() {
        let base = "[[agents]]\nname=\"a\"\nprovider=\"openai\"\nmodel=\"m\"\n";
        assert_eq!(Config::parse(base).unwrap().agents[0].wire, None);
        for (value, expected) in [("chat", Wire::Chat), ("responses", Wire::Responses)] {
            let cfg = Config::parse(&format!("{base}wire=\"{value}\"\n")).unwrap();
            assert_eq!(cfg.agents[0].wire, Some(expected));
        }
    }

    #[test]
    fn rejects_an_unknown_wire() {
        let raw = "[[agents]]\nname=\"a\"\nprovider=\"openai\"\nmodel=\"m\"\nwire=\"soap\"\n";
        assert!(Config::parse(raw).is_err());
    }

    #[test]
    fn rejects_wire_on_a_provider_that_has_no_such_choice() {
        for provider in ["ollama", "anthropic"] {
            let raw = format!(
                "[[agents]]\nname=\"a\"\nprovider=\"{provider}\"\nmodel=\"m\"\nwire=\"chat\"\n"
            );
            let err = Config::parse(&raw).unwrap_err().to_string();
            assert!(err.contains("wire"), "{err}");
        }
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
        assert_eq!(cfg.log_targets, vec![LogTarget::Stderr]);
        assert!(!cfg.strict_requests);
    }

    #[test]
    fn parses_multiple_log_targets() {
        let raw = "log_targets = [\"stdout\", \"otlp\"]\n[[agents]]\nname=\"a\"\nprovider=\"ollama\"\nmodel=\"m\"\n";
        let cfg = Config::parse(raw).expect("logging targets should parse");
        assert_eq!(cfg.log_targets, vec![LogTarget::Stdout, LogTarget::Otlp]);
    }

    #[test]
    fn rejects_empty_log_targets() {
        let raw = "log_targets = []\n[[agents]]\nname=\"a\"\nprovider=\"ollama\"\nmodel=\"m\"\n";
        assert!(Config::parse(raw)
            .unwrap_err()
            .to_string()
            .contains("log_targets"));
    }

    #[test]
    fn rejects_duplicate_log_targets() {
        let raw = "log_targets = [\"stderr\", \"stderr\"]\n[[agents]]\nname=\"a\"\nprovider=\"ollama\"\nmodel=\"m\"\n";
        assert!(Config::parse(raw)
            .unwrap_err()
            .to_string()
            .contains("duplicate target"));
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
    fn defaults_shutdown_grace_to_ten_seconds() {
        let raw = "[[agents]]\nname=\"a\"\nprovider=\"ollama\"\nmodel=\"m\"\n";
        assert_eq!(Config::parse(raw).unwrap().shutdown_grace_seconds, 10);
    }

    #[test]
    fn rejects_zero_shutdown_grace() {
        let raw = "shutdown_grace_seconds = 0\n[[agents]]\nname=\"a\"\nprovider=\"ollama\"\nmodel=\"m\"\n";
        assert!(Config::parse(raw)
            .unwrap_err()
            .to_string()
            .contains("shutdown_grace_seconds"));
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

    #[test]
    fn accepts_the_wildcard_allowlist_alone() {
        let raw = "[[agents]]\nname=\"a\"\nprovider=\"ollama\"\nmodel=\"m\"\n[[agents.mcp_servers]]\nurl=\"http://localhost/mcp\"\nallowed_tools=[\"*\"]\n";
        let cfg = Config::parse(raw).expect("wildcard alone should parse");
        assert_eq!(
            cfg.agents[0].mcp_servers[0].allowed_tools.as_deref(),
            Some(["*".to_string()].as_slice())
        );
    }

    #[test]
    fn rejects_mixing_the_wildcard_with_named_tools() {
        let raw = "[[agents]]\nname=\"a\"\nprovider=\"ollama\"\nmodel=\"m\"\n[[agents.mcp_servers]]\nurl=\"http://localhost/mcp\"\nallowed_tools=[\"*\", \"search\"]\n";
        assert!(Config::parse(raw)
            .unwrap_err()
            .to_string()
            .contains("mixes"));
    }

    fn mcp(body: &str) -> String {
        format!("[[agents]]\nname=\"a\"\nprovider=\"ollama\"\nmodel=\"m\"\n[[agents.mcp_servers]]\n{body}\n")
    }

    #[test]
    fn accepts_stdio_mcp_server() {
        let cfg = Config::parse(&mcp(
            "command=\"/usr/bin/x\"\nargs=[\"--a\"]\nworking_dir=\"/tmp\"\ninherit_env=[\"HOME\"]\nenv={TZ=\"UTC\"}\nallowed_tools=[\"t\"]",
        ))
        .unwrap();
        let s = &cfg.agents[0].mcp_servers[0];
        assert_eq!(s.display_name(), "stdio:/usr/bin/x");
        assert_eq!(s.env["TZ"], "UTC");
    }

    #[test]
    fn rejects_bad_mcp_transport_combinations() {
        for (body, want) in [
            ("url=\"http://x\"\ncommand=\"y\"", "both url and command"),
            ("allowed_tools=[\"t\"]", "neither url nor command"),
            ("command=\" \"", "empty command"),
            ("url=\"http://x\"\nargs=[\"a\"]", "only apply to command"),
            (
                "url=\"http://x\"\ninherit_env=[\"A\"]",
                "only apply to command",
            ),
            (
                "command=\"y\"\nbearer_token_env=\"T\"",
                "only applies to url",
            ),
            (
                "command=\"y\"\nenv={\"A=B\"=\"1\"}",
                "invalid environment variable name",
            ),
            (
                "command=\"y\"\ninherit_env=[\"\"]",
                "invalid environment variable name",
            ),
            (
                "command=\"y\"\nenv={A=\"1\"}\ninherit_env=[\"A\"]",
                "both env and inherit_env",
            ),
            ("command=\"y\"\nworking_dir=\" \"", "empty working_dir"),
        ] {
            let err = Config::parse(&mcp(body)).unwrap_err().to_string();
            assert!(err.contains(want), "{body}: got {err}");
        }
    }

    #[test]
    fn agent_limits_parse_and_must_only_lower_the_server_wide_ones() {
        let base = "max_concurrent_requests=4\nrequest_timeout_seconds=30\n[[agents]]\nname=\"a\"\nprovider=\"ollama\"\nmodel=\"m\"\n";
        let ok = Config::parse(&format!(
            "{base}max_concurrent_requests=2\nrequest_timeout_seconds=10\n"
        ))
        .unwrap();
        assert_eq!(ok.agents[0].max_concurrent_requests, Some(2));
        assert_eq!(ok.agents[0].request_timeout_seconds, Some(10));
        let unset = Config::parse(base).unwrap();
        assert_eq!(unset.agents[0].max_concurrent_requests, None);
        // Equal to the server-wide value is allowed.
        Config::parse(&format!("{base}max_concurrent_requests=4\n")).unwrap();

        for (extra, want) in [
            ("max_concurrent_requests=0", "must be greater than zero"),
            ("max_concurrent_requests=5", "exceeds the server-wide limit"),
            ("request_timeout_seconds=0", "must be greater than zero"),
            (
                "request_timeout_seconds=31",
                "exceeds the server-wide timeout",
            ),
        ] {
            let err = Config::parse(&format!("{base}{extra}\n"))
                .unwrap_err()
                .to_string();
            assert!(err.contains(want), "{extra}: got {err}");
        }
    }

    #[test]
    fn heartbeat_settings_apply_to_url_servers_only_and_zero_means_off() {
        let url = |extra: &str| mcp(&format!("url=\"http://x\"\n{extra}"));
        let cfg = Config::parse(&url("heartbeat_interval_seconds=0")).unwrap();
        assert_eq!(
            cfg.agents[0].mcp_servers[0].heartbeat_interval_seconds,
            Some(0)
        );
        let cfg = Config::parse(&url(
            "heartbeat_interval_seconds=5\nheartbeat_timeout_seconds=2",
        ))
        .unwrap();
        assert_eq!(
            cfg.agents[0].mcp_servers[0].heartbeat_timeout_seconds,
            Some(2)
        );
        let err = Config::parse(&url("heartbeat_timeout_seconds=0"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("heartbeat_timeout_seconds = 0"), "{err}");
        let err = Config::parse(&mcp("command=\"y\"\nheartbeat_interval_seconds=5"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("only apply to url"), "{err}");
    }
}
