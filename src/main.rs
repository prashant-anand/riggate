mod agent;
mod config;
mod routes;

use anyhow::{Context, Result};
use std::path::PathBuf;
use tracing::info;

const USAGE: &str = "\
sallyport — a single-binary agent server for air-gapped networks

USAGE:
    sallyport serve [--config <path>]
    sallyport check [--config <path>]

OPTIONS:
    --config <path>   Config file (default: sallyport.toml)

ENDPOINTS:
    GET  /healthz                 liveness probe
    GET  /v1/models               configured agents, OpenAI shape
    POST /v1/chat/completions     run an agent, OpenAI shape
";

fn arg_value(args: &[String], flag: &str) -> Option<PathBuf> {
    args.iter()
        .position(|a| a == flag)
        .and_then(|i| args.get(i + 1))
        .map(PathBuf::from)
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "sallyport=info".into()),
        )
        .init();

    let args: Vec<String> = std::env::args().skip(1).collect();
    let path = arg_value(&args, "--config").unwrap_or_else(|| PathBuf::from("sallyport.toml"));

    match args.first().map(String::as_str) {
        Some("serve") => serve(&path).await,
        Some("check") => check(&path),
        Some("--version" | "-V") => {
            println!("sallyport {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        _ => {
            print!("{USAGE}");
            Ok(())
        }
    }
}

fn check(path: &std::path::Path) -> Result<()> {
    let cfg = config::Config::load(path)?;
    println!("config ok — {} agent(s)", cfg.agents.len());
    for a in &cfg.agents {
        println!("  {} -> {} ({:?})", a.name, a.model, a.provider);
    }
    Ok(())
}

async fn serve(path: &std::path::Path) -> Result<()> {
    let cfg = config::Config::load(path)?;
    let registry = agent::build(&cfg.agents)?;
    let app = routes::router(registry);

    let listener = tokio::net::TcpListener::bind(&cfg.bind)
        .await
        .with_context(|| format!("binding {}", cfg.bind))?;
    info!(bind = %cfg.bind, agents = cfg.agents.len(), "sallyport listening");

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown())
        .await?;
    Ok(())
}

async fn shutdown() {
    let _ = tokio::signal::ctrl_c().await;
    info!("shutting down");
}
