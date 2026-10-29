mod agent;
mod auth;
mod config;
mod routes;

use anyhow::{Context, Result};
use std::path::PathBuf;
use tracing::info;

const USAGE: &str = "\
riggate — a single-binary Rig agent server for air-gapped networks

USAGE:
    riggate serve [--config <path>]
    riggate check [--config <path>]

OPTIONS:
    --config <path>   Config file (default: riggate.toml)

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
    // Colour only when stderr is a terminal. Redirected logs go to a file, journald or a log
    // pipeline, where escape sequences are noise in a line an operator needs to grep.
    tracing_subscriber::fmt()
        .with_ansi(std::io::IsTerminal::is_terminal(&std::io::stderr()))
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "riggate=info".into()),
        )
        .init();

    let args: Vec<String> = std::env::args().skip(1).collect();
    let path = arg_value(&args, "--config").unwrap_or_else(|| PathBuf::from("riggate.toml"));

    match args.first().map(String::as_str) {
        Some("serve") => serve(&path).await,
        Some("check") => check(&path),
        Some("--version" | "-V") => {
            println!("riggate {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        _ => {
            print!("{USAGE}");
            std::process::exit(2)
        }
    }
}

fn check(path: &std::path::Path) -> Result<()> {
    let cfg = config::Config::load(path)?;
    println!("config ok — {} agent(s)", cfg.agents.len());
    for a in &cfg.agents {
        println!(
            "  {} -> {} ({:?}), max_turns={}, mcp_servers={}",
            a.name,
            a.model,
            a.provider,
            a.max_turns,
            a.mcp_servers.len()
        );
    }
    Ok(())
}

async fn serve(path: &std::path::Path) -> Result<()> {
    let cfg = config::Config::load(path)?;
    let api_key = std::env::var("RIGGATE_API_KEY")
        .ok()
        .filter(|key| !key.is_empty());
    if auth::bind_requires_auth(&cfg.bind) && api_key.is_none() {
        anyhow::bail!("RIGGATE_API_KEY is required when binding outside loopback");
    }
    let registry = agent::build(&cfg.agents).await?;
    let app = routes::router(
        registry,
        api_key,
        routes::RequestLimits {
            max_request_bytes: cfg.max_request_bytes,
            max_concurrent_requests: cfg.max_concurrent_requests,
            timeout: std::time::Duration::from_secs(cfg.request_timeout_seconds),
            strict_requests: cfg.strict_requests,
        },
    );

    let listener = tokio::net::TcpListener::bind(&cfg.bind)
        .await
        .with_context(|| format!("binding {}", cfg.bind))?;
    info!(bind = %cfg.bind, agents = cfg.agents.len(), "riggate listening");

    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown())
    .await?;
    Ok(())
}

async fn shutdown() {
    let _ = tokio::signal::ctrl_c().await;
    info!("shutting down");
}
