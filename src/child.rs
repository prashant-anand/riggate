//! A stdio MCP server's process: started as the leader of its own process group, watched for
//! exit directly, and stopped in a defined order.
//!
//! rmcp's own child transport hides the child, which causes two problems here. Exit is only
//! noticed when the pipe closes, so a grandchild that inherited stdout keeps a dead server
//! looking alive; and cleanup only reaches the one process Riggate spawned, so a launcher
//! such as `npx` or `uvx` leaves the real server behind. Owning the child fixes both.

use anyhow::{Context, Result};
use process_wrap::tokio::{ChildWrapper, CommandWrap};
use std::time::Duration;
use tokio::process::{ChildStderr, ChildStdin, ChildStdout};

/// How long each stage of a stop gets before the next, harsher one. Matches the MCP stdio
/// shutdown sequence: close stdin and wait, then SIGTERM and wait, then SIGKILL.
const STOP_STAGE: Duration = Duration::from_secs(3);

pub struct ServerProcess {
    child: Box<dyn ChildWrapper>,
    /// The group id. The child is the group leader, so this is its pid.
    #[cfg(unix)]
    pgid: Option<i32>,
}

pub struct Spawned {
    pub process: ServerProcess,
    pub stdin: ChildStdin,
    pub stdout: ChildStdout,
    pub stderr: Option<ChildStderr>,
}

impl ServerProcess {
    pub fn spawn(command: tokio::process::Command) -> Result<Spawned> {
        let mut wrap = CommandWrap::from(command);
        #[cfg(unix)]
        wrap.wrap(process_wrap::tokio::ProcessGroup::leader());
        wrap.command_mut()
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        let mut child = wrap.spawn().context("spawn failed")?;
        let stdin = child.inner_mut().stdin().take().context("no stdin")?;
        let stdout = child.inner_mut().stdout().take().context("no stdout")?;
        let stderr = child.inner_mut().stderr().take();
        #[cfg(unix)]
        let pgid = child.id().and_then(|pid| i32::try_from(pid).ok());
        Ok(Spawned {
            process: ServerProcess {
                child,
                #[cfg(unix)]
                pgid,
            },
            stdin,
            stdout,
            stderr,
        })
    }

    /// Resolves when the process itself exits, whatever happens to its pipes.
    pub async fn exited(&mut self) {
        let _ = self.child.wait().await;
    }

    /// Stop the server and everything it started. Call after the MCP connection has closed
    /// (which drops our end of stdin, the polite signal). Safe on a process that already
    /// exited.
    pub async fn stop(&mut self) {
        if tokio::time::timeout(STOP_STAGE, self.child.wait())
            .await
            .is_err()
        {
            tracing::info!("MCP server ignored stdin close; sending SIGTERM");
            self.signal_group(Signal::Term);
            if tokio::time::timeout(STOP_STAGE, self.child.wait())
                .await
                .is_err()
            {
                tracing::warn!("MCP server ignored SIGTERM; killing it");
            }
        }
        // Whatever the child left running goes too, including after a clean exit.
        self.signal_group(Signal::Kill);
        let _ = self.child.wait().await;
    }
}

enum Signal {
    Term,
    Kill,
}

impl ServerProcess {
    #[cfg(unix)]
    fn signal_group(&self, signal: Signal) {
        let sig = match signal {
            Signal::Term => libc::SIGTERM,
            Signal::Kill => libc::SIGKILL,
        };
        if let Some(pgid) = self.pgid {
            // SAFETY: killpg takes no pointers; an empty or stale group yields ESRCH, which
            // is the normal outcome after a clean exit.
            unsafe { libc::killpg(pgid, sig) };
        }
    }

    #[cfg(not(unix))]
    fn signal_group(&self, _signal: Signal) {}
}

impl Drop for ServerProcess {
    fn drop(&mut self) {
        // Paths that end without stop(): a failed handshake, an aborted task.
        self.signal_group(Signal::Kill);
    }
}
