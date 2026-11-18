//! Child processes (contract 2.8). Used only by `crucible-proxy`.
//!
//! The trait is not a field of [`crate::Env`]: the one component that needs it receives an
//! `Arc<dyn ChildProcess>` next to its `Env`.

use std::fmt;
use std::io;
use std::path::PathBuf;

use async_trait::async_trait;
use tokio::io::{AsyncRead, AsyncWrite};

/// Where the child's standard error goes.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum StderrMode {
    /// The child writes to the parent's standard error.
    #[default]
    Inherit,
    /// The parent reads it through [`Child::stderr`].
    Pipe,
}

/// What to run. Standard input and output are always pipes.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct ChildSpec {
    /// The program: a path, or a name looked up on `PATH` (on Windows through `PATHEXT` too).
    pub program: String,
    /// Its arguments.
    pub args: Vec<String>,
    /// Variables set on top of the parent's environment, which the child inherits.
    pub env: Vec<(String, String)>,
    /// The working directory; the parent's when `None`.
    pub cwd: Option<PathBuf>,
    /// Where standard error goes.
    pub stderr: StderrMode,
}

/// A signal a parent forwards to its child.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum ChildSignal {
    /// `SIGTERM`.
    Terminate,
    /// `SIGINT`.
    Interrupt,
    /// `SIGHUP`.
    Hangup,
}

impl ChildSignal {
    /// The Unix signal number.
    pub const fn number(self) -> i32 {
        match self {
            Self::Hangup => 1,
            Self::Interrupt => 2,
            Self::Terminate => 15,
        }
    }
}

/// How a child ended.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum ExitStatus {
    /// It exited with this code.
    Code(i32),
    /// A signal with this number ended it (Unix only).
    Signal(i32),
}

impl ExitStatus {
    /// Whether the child exited with code 0.
    pub const fn success(self) -> bool {
        matches!(self, Self::Code(0))
    }
}

/// Why a child could not be started.
#[derive(Debug, thiserror::Error)]
pub enum ChildError {
    /// No such program on `PATH`. The message names what was searched.
    #[error("{0}")]
    NotFound(String),
    /// The operating system refused the start.
    #[error("starting {program}: {source}")]
    Spawn {
        /// The program as resolved.
        program: String,
        /// The operating system's error.
        source: io::Error,
    },
}

/// A readable end of one of the child's pipes.
pub type ChildOutput = Box<dyn AsyncRead + Send + Unpin>;
/// The writable end of the child's standard input. Dropping it closes the pipe.
pub type ChildInput = Box<dyn AsyncWrite + Send + Unpin>;

/// The operations on a running child, behind [`Child`].
#[async_trait]
pub trait ChildControl: fmt::Debug + Send {
    /// Waits for the child itself to exit; processes it started may outlive it. Cancel-safe, and later calls
    /// return the same status.
    async fn wait(&mut self) -> io::Result<ExitStatus>;

    /// Ends the child and every process it started, without waiting.
    fn kill(&mut self) -> io::Result<()>;

    /// Sends a signal to the child's process group. On Windows, where a process cannot be signalled, this
    /// ends the child as [`ChildControl::kill`] does.
    fn signal(&mut self, signal: ChildSignal) -> io::Result<()>;
}

/// A started child. Dropping it ends the child and every process it started.
pub struct Child {
    /// The child's standard input.
    pub stdin: Option<ChildInput>,
    /// The child's standard output.
    pub stdout: Option<ChildOutput>,
    /// The child's standard error, with [`StderrMode::Pipe`].
    pub stderr: Option<ChildOutput>,
    control: Box<dyn ChildControl>,
}

impl Child {
    /// Bundles a backend's child. Called by `ChildProcess` implementations.
    pub fn new(
        stdin: Option<ChildInput>,
        stdout: Option<ChildOutput>,
        stderr: Option<ChildOutput>,
        control: Box<dyn ChildControl>,
    ) -> Self {
        Self {
            stdin,
            stdout,
            stderr,
            control,
        }
    }

    /// See [`ChildControl::wait`].
    pub async fn wait(&mut self) -> io::Result<ExitStatus> {
        self.control.wait().await
    }

    /// See [`ChildControl::kill`].
    pub fn kill(&mut self) -> io::Result<()> {
        self.control.kill()
    }

    /// See [`ChildControl::signal`].
    pub fn signal(&mut self, signal: ChildSignal) -> io::Result<()> {
        self.control.signal(signal)
    }
}

impl fmt::Debug for Child {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Child")
            .field("stdin", &self.stdin.is_some())
            .field("stdout", &self.stdout.is_some())
            .field("stderr", &self.stderr.is_some())
            .field("control", &self.control)
            .finish()
    }
}

/// Starts child processes.
///
/// Production (`crucible-env-prod`): on Unix the child leads its own process group and, on Linux, gets a
/// parent-death signal; on Windows it starts suspended, joins a Job Object with kill-on-close and resumes, and
/// the program is resolved through `PATHEXT`. Simulation scripts an MCP server task with seeded chunk
/// splitting, stalls, crashes and exit codes.
#[async_trait]
pub trait ChildProcess: fmt::Debug + Send + Sync {
    /// Starts `spec`.
    async fn spawn(&self, spec: ChildSpec) -> Result<Child, ChildError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signal_numbers_are_the_posix_ones() {
        assert_eq!(ChildSignal::Hangup.number(), 1);
        assert_eq!(ChildSignal::Interrupt.number(), 2);
        assert_eq!(ChildSignal::Terminate.number(), 15);
        assert!(ExitStatus::Code(0).success());
        assert!(!ExitStatus::Code(3).success());
        assert!(!ExitStatus::Signal(15).success());
    }
}
