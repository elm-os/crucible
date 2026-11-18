//! `crucible proxy stdio -- <command> [args]`: the pumps between an MCP client on this process's standard
//! streams and an MCP server started as a child.
//!
//! Each chunk read from one side is written to the other side at once and flushed; nothing is buffered, split
//! or joined on the way, so there is no line limit. When the client closes its end, the child's standard input
//! is closed. The result is how the child ended, for the caller to exit the same way.

use std::time::Duration;

use bytes::BytesMut;
use crucible_env::{Child, ChildError, ChildProcess, ChildSignal, ChildSpec, Env, ExitStatus};
use crucible_records::Direction;
use futures::StreamExt as _;
use futures::stream::BoxStream;
use tokio::io::{AsyncRead, AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _};

use crate::core::StreamId;
use crate::live::LiveTap;

/// The size of one read. A larger message simply takes several chunks.
const CHUNK: usize = 64 * 1024;
/// How long the child's last output is waited for after it exited. A process the child left behind can hold
/// the pipe open; it must not hold the proxy.
const DRAIN_GRACE: Duration = Duration::from_secs(2);

/// The client's side: this process's standard streams in production.
pub struct ClientIo {
    /// What the client writes (standard input).
    pub input: Box<dyn AsyncRead + Send + Unpin>,
    /// What the client reads (standard output). Only bytes from the child are ever written here.
    pub output: Box<dyn AsyncWrite + Send + Unpin>,
    /// Where a captured child standard error is copied (standard error).
    pub error: Box<dyn AsyncWrite + Send + Unpin>,
}

impl std::fmt::Debug for ClientIo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClientIo").finish_non_exhaustive()
    }
}

/// Why the stdio proxy could not run.
#[derive(Debug, thiserror::Error)]
pub enum StdioError {
    /// The child could not be started.
    #[error(transparent)]
    Spawn(#[from] ChildError),
    /// Waiting for the child failed.
    #[error("waiting for the child: {0}")]
    Wait(std::io::Error),
}

/// Copies `from` to `to` chunk by chunk through the tap until `from` ends or `to` fails.
async fn pump(
    mut from: Box<dyn AsyncRead + Send + Unpin>,
    mut to: Box<dyn AsyncWrite + Send + Unpin>,
    tap: Option<(LiveTap, Direction)>,
) -> std::io::Result<()> {
    let mut buffer = BytesMut::with_capacity(CHUNK);
    loop {
        buffer.reserve(CHUNK);
        if from.read_buf(&mut buffer).await? == 0 {
            // End of input: dropping `to` closes the other side's pipe.
            return Ok(());
        }
        let chunk = buffer.split().freeze();
        let chunk = match &tap {
            Some((tap, direction)) => tap.forward(StreamId::STDIO, *direction, chunk),
            None => chunk,
        };
        to.write_all(&chunk).await?;
        to.flush().await?;
    }
}

/// Runs the proxy until the child exits and returns how it ended.
///
/// `signals` are forwarded to the child's process group as they arrive. Call this from a thread that lives as
/// long as the process (see `TokioChildProcess::with_parent_death_trampoline`).
pub async fn run(
    env: &Env,
    children: &dyn ChildProcess,
    spec: ChildSpec,
    client: ClientIo,
    mut signals: BoxStream<'static, ChildSignal>,
    tap: LiveTap,
) -> Result<ExitStatus, StdioError> {
    let mut child: Child = children.spawn(spec).await?;
    let (Some(stdin), Some(stdout)) = (child.stdin.take(), child.stdout.take()) else {
        return Err(StdioError::Wait(std::io::Error::other(
            "the child has no standard input or output",
        )));
    };
    let to_child = env.spawn(
        "proxy-stdio-in",
        pump(
            client.input,
            stdin,
            Some((tap.clone(), Direction::ClientToServer)),
        ),
    );
    let stderr = child
        .stderr
        .take()
        .map(|stderr| env.spawn("proxy-stdio-err", pump(stderr, client.error, None)));
    let mut from_child = env.spawn(
        "proxy-stdio-out",
        pump(
            stdout,
            client.output,
            Some((tap.clone(), Direction::ServerToClient)),
        ),
    );

    let mut output_done = false;
    let mut signals_done = false;
    let status = loop {
        tokio::select! {
            biased;
            signal = signals.next(), if !signals_done => match signal {
                Some(signal) => {
                    if let Err(error) = child.signal(signal) {
                        tracing::debug!(%error, "forwarding a signal to the child failed");
                    }
                }
                None => signals_done = true,
            },
            status = child.wait() => break status.map_err(StdioError::Wait)?,
            result = from_child.join(), if !output_done => {
                output_done = true;
                if let Ok(Err(error)) = result {
                    // The client is gone: nothing can be delivered, so the server is asked to stop.
                    tracing::debug!(%error, "writing to the client failed");
                    if let Err(error) = child.signal(ChildSignal::Terminate) {
                        tracing::debug!(%error, "stopping the child failed");
                    }
                }
            }
        }
    };
    if !output_done && env.timeout(DRAIN_GRACE, from_child.join()).await.is_err() {
        from_child.abort();
    }
    // The reader of the client's input may be parked in a read that never returns.
    to_child.abort();
    if let Some(stderr) = stderr {
        let mut stderr = stderr;
        if env.timeout(DRAIN_GRACE, stderr.join()).await.is_err() {
            stderr.abort();
        }
    }
    tap.session_end();
    Ok(status)
}
