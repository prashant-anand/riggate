use anyhow::{bail, Context, Result};
use serde::Deserialize;
use std::path::Path;

fn default_bind() -> String {
    "127.0.0.1:8080".to_string()
}

#[derive(Debug, Deserialize)]
pub struct Config {
    /// Address to bind. Defaults to loopback — you opt in to exposure.
    #[serde(default = "default_bind")]
    pub bind: String,
    #[serde(default)]
    pub agents: Vec<AgentConfig>,
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
        for a in &cfg.agents {
            if a.name.trim().is_empty() {
                bail!("an agent has an empty name");
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
}
