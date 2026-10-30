mod agent;
mod auth;
mod config;
mod routes;

use anyhow::{Context, Result};
use opentelemetry::KeyValue;
use opentelemetry_appender_tracing::layer::{OpenTelemetryTracingBridge, TracingSpanAttributes};
use opentelemetry_sdk::{logs::SdkLoggerProvider, Resource};
use std::io::{self, Write};
use std::path::PathBuf;
use tracing::info;
use tracing_subscriber::fmt::MakeWriter;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{Layer, Registry};

const USAGE: &str = "\
riggate — a single-binary Rig agent server for air-gapped networks

USAGE:
    riggate serve [--config <path>]
    riggate check [--config <path>]

OPTIONS:
    --config <path>   Config file (default: riggate.toml)

ENDPOINTS:
    GET  /healthz                 liveness probe
    GET  /readyz                  startup readiness probe
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
    let args: Vec<String> = std::env::args().skip(1).collect();
    let path = arg_value(&args, "--config").unwrap_or_else(|| PathBuf::from("riggate.toml"));
    if matches!(args.first().map(String::as_str), Some("--version" | "-V")) {
        println!("riggate {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }
    match args.first().map(String::as_str) {
        Some("serve") => serve(config::Config::load(&path)?).await,
        Some("check") => check(&config::Config::load(&path)?),
        _ => {
            print!("{USAGE}");
            std::process::exit(2)
        }
    }
}

fn check(cfg: &config::Config) -> Result<()> {
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

async fn serve(cfg: config::Config) -> Result<()> {
    let otlp_shutdown_timeout = std::time::Duration::from_secs(cfg.shutdown_grace_seconds);
    let otel_provider = init_logging(&cfg.log_targets)?;

    if otel_provider.is_some() {
        if let Err(error) = probe_otlp_endpoint().await {
            eprintln!("OTLP collector TCP check failed; continuing: {error}");
        }
    }

    let result = serve_initialized(cfg).await;

    if let Some(provider) = otel_provider {
        match tokio::task::spawn_blocking(move || {
            provider.shutdown_with_timeout(otlp_shutdown_timeout)
        })
        .await
        {
            Ok(Ok(())) => {}
            Ok(Err(error)) => eprintln!("failed to flush OTLP logs during shutdown: {error}"),
            Err(error) => eprintln!("failed to join OTLP log shutdown worker: {error}"),
        }
    }

    result
}

async fn probe_otlp_endpoint() -> Result<()> {
    let endpoint = std::env::var("OTEL_EXPORTER_OTLP_LOGS_ENDPOINT")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .or_else(|| {
            std::env::var("OTEL_EXPORTER_OTLP_ENDPOINT")
                .ok()
                .filter(|value| !value.trim().is_empty())
        })
        .unwrap_or_else(|| "http://localhost:4318".to_string());
    let url = url::Url::parse(&endpoint)
        .with_context(|| format!("invalid OTLP endpoint URL {endpoint:?}"))?;
    if !matches!(url.scheme(), "http" | "https") {
        anyhow::bail!("OTLP endpoint must use http or https");
    }
    let host = url
        .host_str()
        .context("OTLP endpoint URL has no host")?
        .to_string();
    let port = url
        .port_or_known_default()
        .context("OTLP endpoint URL has no known port")?;
    tokio::time::timeout(
        std::time::Duration::from_secs(3),
        tokio::net::TcpStream::connect((host.as_str(), port)),
    )
    .await
    .context("timed out connecting to OTLP collector")??;
    Ok(())
}

async fn serve_initialized(cfg: config::Config) -> Result<()> {
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

    // A request can be mid-stream when the signal arrives, so stop accepting and let it finish.
    // The grace period exists because the alternative to waiting forever is the service
    // manager's SIGKILL, which loses the same work with less explanation.
    let grace = std::time::Duration::from_secs(cfg.shutdown_grace_seconds);
    let (signal_tx, signal_rx) = tokio::sync::watch::channel(false);
    tokio::spawn(async move {
        shutdown().await;
        let _ = signal_tx.send(true);
    });

    let mut serve_rx = signal_rx.clone();
    let server = axum::serve(
        listener,
        app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
    .with_graceful_shutdown(async move {
        let _ = serve_rx.wait_for(|fired| *fired).await;
    });

    let mut drain_rx = signal_rx;
    let drain = async move {
        let _ = drain_rx.wait_for(|fired| *fired).await;
        tokio::time::sleep(grace).await;
    };

    let server_result = tokio::select! {
        result = server => result,
        _ = drain => {
            tracing::warn!(
                grace_seconds = grace.as_secs(),
                "shutdown grace period elapsed with requests still open; exiting"
            );
            Ok(())
        }
    };

    server_result?;
    Ok(())
}

#[derive(Clone)]
struct FanoutMakeWriter {
    targets: Vec<config::LogTarget>,
}

impl<'a> MakeWriter<'a> for FanoutMakeWriter {
    type Writer = FanoutWriter;

    fn make_writer(&'a self) -> Self::Writer {
        FanoutWriter {
            writers: self
                .targets
                .iter()
                .map(|target| match target {
                    config::LogTarget::Stdout => Box::new(io::stdout()) as Box<dyn Write + Send>,
                    config::LogTarget::Stderr => Box::new(io::stderr()) as Box<dyn Write + Send>,
                    config::LogTarget::Otlp => unreachable!("OTLP is not a stream writer"),
                })
                .collect(),
        }
    }
}

struct FanoutWriter {
    writers: Vec<Box<dyn Write + Send>>,
}

impl Write for FanoutWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        for writer in &mut self.writers {
            writer.write_all(bytes)?;
        }
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        for writer in &mut self.writers {
            writer.flush()?;
        }
        Ok(())
    }
}

fn init_logging(targets: &[config::LogTarget]) -> Result<Option<SdkLoggerProvider>> {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| "riggate=info".into());
    let local_targets: Vec<_> = targets
        .iter()
        .copied()
        .filter(|target| *target != config::LogTarget::Otlp)
        .collect();
    let mut layers: Vec<Box<dyn Layer<Registry> + Send + Sync>> = Vec::new();

    if !local_targets.is_empty() {
        let writer = FanoutMakeWriter {
            targets: local_targets,
        };
        layers.push(
            tracing_subscriber::fmt::layer()
                .json()
                .with_writer(writer)
                .with_filter(filter.clone())
                .boxed(),
        );
    }

    let provider = if targets.contains(&config::LogTarget::Otlp) {
        let exporter = opentelemetry_otlp::LogExporter::builder()
            .with_http()
            .build()
            .context("building OTLP log exporter")?;
        let provider = SdkLoggerProvider::builder()
            .with_resource(
                Resource::builder()
                    .with_service_name("riggate")
                    .with_attribute(KeyValue::new("service.version", env!("CARGO_PKG_VERSION")))
                    .build(),
            )
            .with_batch_exporter(exporter)
            .build();
        let bridge = OpenTelemetryTracingBridge::builder(&provider)
            .with_tracing_span_attributes(TracingSpanAttributes::allowlist([
                "request_id",
                "agent",
                "status",
                "outcome",
                "duration_ms",
                "prompt_tokens",
                "completion_tokens",
            ]))
            .build()
            .with_filter(filter);
        layers.push(bridge.boxed());
        Some(provider)
    } else {
        None
    };

    tracing_subscriber::registry()
        .with(layers)
        .try_init()
        .map_err(|error| anyhow::anyhow!("initializing logging: {error}"))?;
    Ok(provider)
}

async fn shutdown() {
    wait_for_signal().await;
    info!("shutting down");
}

/// SIGINT from a terminal, SIGTERM from systemd, Docker or a process supervisor. Handling only
/// the first means the second kills the process mid-stream with no drain at all.
#[cfg(unix)]
async fn wait_for_signal() {
    use tokio::signal::unix::{signal, SignalKind};
    let mut terminate = signal(SignalKind::terminate()).expect("install SIGTERM handler");
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        _ = terminate.recv() => {}
    }
}

#[cfg(not(unix))]
async fn wait_for_signal() {
    let _ = tokio::signal::ctrl_c().await;
}
