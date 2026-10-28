use anyhow::{Context, Result};
use rig::prelude::*;
use rig::providers::{ollama, openai};
use std::collections::HashMap;
use std::sync::Arc;

use crate::config::{AgentConfig, Provider};

/// Every agent is built once at boot. Requests only borrow them.
pub type Registry = Arc<HashMap<String, Agent>>;

pub fn build(configs: &[AgentConfig]) -> Result<Registry> {
    let mut map = HashMap::new();

    for cfg in configs {
        let agent = match cfg.provider {
            // `from_env` reads OPENAI_API_KEY and, if set, OPENAI_BASE_URL —
            // which is how you point it at Azure, vLLM, or an internal gateway.
            Provider::Openai => {
                let client = openai::OpenAI::from_env().context(
                    "an openai agent needs OPENAI_API_KEY (and optionally OPENAI_BASE_URL)",
                )?;
                let builder = AgentBuilder::new(client.completion(cfg.model.as_str()));
                build_agent(builder, cfg)
            }
            // `from_env` reads OLLAMA_API_BASE_URL and OLLAMA_API_KEY, both optional.
            Provider::Ollama => {
                let client = ollama::Ollama::from_env()
                    .context("an ollama agent could not read OLLAMA_API_BASE_URL")?;
                let builder = AgentBuilder::new(client.completion(cfg.model.as_str()));
                build_agent(builder, cfg)
            }
        };

        map.insert(cfg.name.clone(), agent);
    }

    Ok(Arc::new(map))
}

fn build_agent(builder: AgentBuilder, cfg: &AgentConfig) -> Agent {
    match &cfg.preamble {
        Some(p) => builder.preamble(p).build(),
        None => builder.build(),
    }
}
