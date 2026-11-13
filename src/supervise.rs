//! Keeps one stdio MCP server alive: restart with backoff, and stop on shutdown.
//!
//! The loop is generic over how a connection is made so its policy (fail closed on the first
//! connect, back off afterwards, reset the backoff only after a healthy run, stop promptly on
//! shutdown) is testable without spawning anything.

use anyhow::Result;
use rig::tool::rmcp::McpConnection;
use std::future::Future;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
use tokio::sync::{oneshot, watch};

/// Restart timing. A child that dies instantly must not be respawned in a hot loop.
#[derive(Clone, Copy, Debug)]
pub struct Backoff {
    pub base: Duration,
    pub max: Duration,
    /// A connection that lasted at least this long counts as healthy and resets the backoff.
    pub healthy_after: Duration,
}

impl Default for Backoff {
    fn default() -> Self {
        Self {
            base: Duration::from_secs(1),
            max: Duration::from_secs(30),
            healthy_after: Duration::from_secs(60),
        }
    }
}

impl Backoff {
    /// Delay before restart number `attempt` (1-based): base, 2*base, 4*base ... capped.
    pub fn delay(&self, attempt: u32) -> Duration {
        let shift = attempt.saturating_sub(1).min(20);
        self.base
            .checked_mul(1u32 << shift)
            .unwrap_or(self.max)
            .min(self.max)
    }
}

/// Resolve when shutdown is requested. A named function so the `watch::Ref` guard is dropped
/// inside it; held across a `select!` it would make the supervisor future non-`Send`.
async fn stopped(shutdown: &mut watch::Receiver<bool>) {
    let _ = shutdown.wait_for(|stop| *stop).await;
}

/// Run until shutdown. `first` receives the outcome of the very first connect so boot can
/// refuse to serve with a server that never came up, matching the HTTP transport. Failures
/// after that are logged and retried.
pub async fn supervise<F, Fut>(
    agent: String,
    server: String,
    mut connect: F,
    mut shutdown: watch::Receiver<bool>,
    first: oneshot::Sender<Result<()>>,
    backoff: Backoff,
) where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<McpConnection>>,
{
    let mut first = Some(first);
    let mut attempt: u32 = 0;
    loop {
        let started = std::time::Instant::now();
        match connect().await {
            Ok(connection) => {
                if let Some(tx) = first.take() {
                    let _ = tx.send(Ok(()));
                }
                tracing::info!(agent = %agent, server = %server, "stdio MCP server ready");
                tokio::select! {
                    _ = connection.closed() => {
                        tracing::warn!(agent = %agent, server = %server, uptime_ms = started.elapsed().as_millis() as u64, "stdio MCP server exited; restarting");
                    }
                    _ = stopped(&mut shutdown) => {
                        connection.close().await;
                        tracing::info!(agent = %agent, server = %server, "stdio MCP server stopped");
                        return;
                    }
                }
                if started.elapsed() >= backoff.healthy_after {
                    attempt = 0;
                }
                // Dropping the dead connection retires its tools before the new ones register.
                drop(connection);
            }
            Err(error) => {
                if let Some(tx) = first.take() {
                    let _ = tx.send(Err(error));
                    return;
                }
                tracing::warn!(agent = %agent, server = %server, error = %format!("{error:#}"), "stdio MCP server failed to start");
            }
        }
        attempt = attempt.saturating_add(1);
        let delay = backoff.delay(attempt);
        tracing::info!(agent = %agent, server = %server, attempt, delay_ms = delay.as_millis() as u64, "restarting stdio MCP server after backoff");
        tokio::select! {
            _ = tokio::time::sleep(delay) => {}
            _ = stopped(&mut shutdown) => return,
        }
    }
}

/// Longest stderr chunk logged as one event. A child that never prints a newline must not
/// grow gateway memory; it is logged in bounded pieces instead.
const MAX_STDERR_LINE: usize = 8 * 1024;

/// Forward a child's stderr into tracing, one event per line, tagged with where it came from.
/// Stderr is the only place most MCP servers explain themselves; letting it fall through to
/// the gateway's own stderr loses the agent/server attribution and bypasses log targets.
pub async fn forward_stderr<R>(stderr: R, agent: String, server: String)
where
    R: tokio::io::AsyncRead + Unpin,
{
    let mut reader = BufReader::new(stderr);
    let mut buf = Vec::new();
    loop {
        buf.clear();
        match (&mut reader)
            .take(MAX_STDERR_LINE as u64)
            .read_until(b'\n', &mut buf)
            .await
        {
            Ok(0) | Err(_) => return,
            Ok(_) => {
                let text = String::from_utf8_lossy(&buf);
                let line = text.trim_end_matches(['\n', '\r']);
                if line.is_empty() {
                    continue;
                }
                // Debug formatting escapes control characters, so a child cannot forge log
                // lines by embedding newlines or terminal escapes.
                tracing::info!(agent = %agent, server = %server, stream = "stderr", line = ?line, "MCP server output");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_doubles_and_caps() {
        let b = Backoff {
            base: Duration::from_secs(1),
            max: Duration::from_secs(10),
            healthy_after: Duration::from_secs(60),
        };
        let d: Vec<u64> = (1..=6).map(|a| b.delay(a).as_secs()).collect();
        assert_eq!(d, vec![1, 2, 4, 8, 10, 10]);
        assert_eq!(b.delay(u32::MAX), Duration::from_secs(10));
    }

    #[tokio::test]
    async fn first_connect_failure_is_reported_and_not_retried() {
        let (_stop_tx, stop_rx) = watch::channel(false);
        let (tx, rx) = oneshot::channel();
        let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let seen = calls.clone();
        supervise(
            "a".into(),
            "s".into(),
            move || {
                seen.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                async { Err(anyhow::anyhow!("boom")) }
            },
            stop_rx,
            tx,
            Backoff::default(),
        )
        .await;
        assert!(rx.await.unwrap().unwrap_err().to_string().contains("boom"));
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[derive(Clone, Default)]
    struct Capture(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);
    impl std::io::Write for Capture {
        fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(b);
            Ok(b.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Capture {
        type Writer = Capture;
        fn make_writer(&'a self) -> Capture {
            self.clone()
        }
    }

    #[tokio::test]
    async fn stderr_lines_are_logged_with_attribution_and_cannot_forge_log_lines() {
        let capture = Capture::default();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(capture.clone())
            .with_ansi(false)
            .finish();
        let _guard = tracing::subscriber::set_default(subscriber);
        let input = b"hello\nfake line\x1b[31m\nINFO forged\n";
        forward_stderr(&input[..], "agent1".into(), "stdio:x".into()).await;
        let out = String::from_utf8(capture.0.lock().unwrap().clone()).unwrap();
        assert!(out.contains("agent1") && out.contains("stdio:x"), "{out}");
        assert!(out.contains("hello"), "{out}");
        assert!(
            !out.contains('\x1b'),
            "control characters must be escaped: {out}"
        );
        assert_eq!(out.lines().count(), 3, "one event per stderr line: {out}");
    }

    #[tokio::test]
    async fn stderr_is_bounded_per_event() {
        // A 20 KiB line without a newline must be consumed in bounded pieces, not buffered whole.
        let data = vec![b'x'; 20 * 1024];
        forward_stderr(&data[..], "a".into(), "s".into()).await;
    }
}
