//! Keeps one stdio MCP server alive: restart with backoff, and stop on shutdown.
//!
//! The loop is generic over how a connection is made so its policy (fail closed on the first
//! connect, back off afterwards, reset the backoff only after a healthy run, stop promptly on
//! shutdown) is testable without spawning anything.

use anyhow::Result;
use std::future::Future;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
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

/// One live connection to a server, plus whatever process backs it.
pub trait Session {
    /// Resolves when the server is gone: connection closed or process exited.
    fn closed(&mut self) -> impl Future<Output = ()> + Send;
    /// Tear everything down and wait for it. Also called after the server died on its own,
    /// to sweep up what it left behind.
    fn stop(self) -> impl Future<Output = ()> + Send;
}

/// Run until shutdown. `first` receives the outcome of the very first connect so boot can
/// refuse to serve with a server that never came up, matching the HTTP transport. Failures
/// after that are logged and retried.
pub async fn supervise<S, F, Fut>(
    agent: String,
    server: String,
    mut connect: F,
    mut shutdown: watch::Receiver<bool>,
    first: oneshot::Sender<Result<()>>,
    backoff: Backoff,
    ready: Arc<AtomicBool>,
) where
    S: Session,
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<S>>,
{
    let mut first = Some(first);
    let mut attempt: u32 = 0;
    loop {
        let started = std::time::Instant::now();
        match connect().await {
            Ok(mut session) => {
                if let Some(tx) = first.take() {
                    let _ = tx.send(Ok(()));
                }
                ready.store(true, Ordering::SeqCst);
                tracing::info!(agent = %agent, server = %server, "stdio MCP server ready");
                let shutting_down = tokio::select! {
                    _ = session.closed() => {
                        tracing::warn!(agent = %agent, server = %server, uptime_ms = started.elapsed().as_millis() as u64, "stdio MCP server exited; restarting");
                        false
                    }
                    _ = stopped(&mut shutdown) => true,
                };
                ready.store(false, Ordering::SeqCst);
                // Dropping the connection retires its tools before the next ones register,
                // and the sweep kills anything the dead server left running.
                session.stop().await;
                if shutting_down {
                    tracing::info!(agent = %agent, server = %server, "stdio MCP server stopped");
                    return;
                }
                if started.elapsed() >= backoff.healthy_after {
                    attempt = 0;
                }
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

    struct NeverUp;
    impl Session for NeverUp {
        async fn closed(&mut self) {}
        async fn stop(self) {}
    }

    /// A session that "dies" after `lifetime`, or never if `None`. Counts how many were made.
    struct Timed {
        lifetime: Option<Duration>,
        stopped: Arc<std::sync::atomic::AtomicUsize>,
    }
    impl Session for Timed {
        async fn closed(&mut self) {
            match self.lifetime {
                Some(d) => tokio::time::sleep(d).await,
                None => std::future::pending().await,
            }
        }
        async fn stop(self) {
            self.stopped.fetch_add(1, Ordering::SeqCst);
        }
    }

    #[tokio::test]
    async fn a_session_that_ends_is_reconnected_and_readiness_follows() {
        use std::sync::atomic::AtomicUsize;
        let (stop_tx, stop_rx) = watch::channel(false);
        let (tx, rx) = oneshot::channel();
        let connects = Arc::new(AtomicUsize::new(0));
        let stopped = Arc::new(AtomicUsize::new(0));
        let ready = Arc::new(AtomicBool::new(false));
        let (c, st) = (connects.clone(), stopped.clone());
        let task = tokio::spawn(supervise(
            "a".into(),
            "s".into(),
            move || {
                // The first two sessions die quickly; the third stays up.
                let n = c.fetch_add(1, Ordering::SeqCst);
                let lifetime = (n < 2).then(|| Duration::from_millis(30));
                let stopped = st.clone();
                async move { Ok::<_, anyhow::Error>(Timed { lifetime, stopped }) }
            },
            stop_rx,
            tx,
            Backoff {
                base: Duration::from_millis(10),
                max: Duration::from_millis(20),
                healthy_after: Duration::from_secs(60),
            },
            ready.clone(),
        ));
        rx.await.unwrap().unwrap();
        for _ in 0..200 {
            if connects.load(Ordering::SeqCst) >= 3 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(connects.load(Ordering::SeqCst), 3);
        // Give the third connect a moment to publish readiness, then it must hold.
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(
            ready.load(Ordering::SeqCst),
            "ready while the third session is up"
        );
        assert_eq!(
            connects.load(Ordering::SeqCst),
            3,
            "a healthy session is not restarted"
        );

        stop_tx.send(true).unwrap();
        task.await.unwrap();
        assert!(!ready.load(Ordering::SeqCst));
        assert_eq!(
            stopped.load(Ordering::SeqCst),
            3,
            "every session is stopped, dead or alive"
        );
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
                async { Err::<NeverUp, _>(anyhow::anyhow!("boom")) }
            },
            stop_rx,
            tx,
            Backoff::default(),
            Default::default(),
        )
        .await;
        assert!(rx.await.unwrap().unwrap_err().to_string().contains("boom"));
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn stderr_lines_are_logged_with_attribution_and_cannot_forge_log_lines() {
        let capture = crate::testlog::Capture::start();
        let input = b"hello\nfake line\x1b[31m\nINFO forged\n";
        forward_stderr(&input[..], "agent1".into(), "stdio:x".into()).await;
        let out = capture.logs();
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
