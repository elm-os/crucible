//! Child processes on tokio, in a process group (Unix) or a Job Object (Windows) through `process-wrap`.
//!
//! - Unix: the child leads a new process group, so a forwarded signal and the kill on drop reach every process
//!   it started. On Linux a child started through the trampoline also gets `SIGTERM` when this process dies,
//!   however it dies ([`parent_death_trampoline`]).
//! - Windows: the child starts suspended, joins a Job Object with kill-on-close and resumes, so the whole tree
//!   ends when this process does. The program is resolved through `PATH` and `PATHEXT`, so `.cmd` shims work.

use std::ffi::OsString;
use std::io;
use std::path::{Path, PathBuf};

use async_trait::async_trait;
use crucible_env::{
    Child, ChildControl, ChildError, ChildInput, ChildOutput, ChildProcess, ChildSignal, ChildSpec,
    ExitStatus, StderrMode,
};
use process_wrap::tokio::{ChildWrapper, CommandWrap};

/// `PATHEXT` when the variable is unset.
const DEFAULT_PATHEXT: &str = ".COM;.EXE;.BAT;.CMD";

/// The platform rules of [`resolve_program`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PathStyle {
    /// `:` separates `PATH`; a name is tried as given.
    Unix,
    /// `;` separates `PATH`; a name is also tried with each `PATHEXT` extension.
    Windows,
}

impl PathStyle {
    /// The style of the platform this was compiled for.
    pub const fn native() -> Self {
        if cfg!(windows) {
            Self::Windows
        } else {
            Self::Unix
        }
    }
}

/// Finds the file a program name stands for. Pure: `is_file` answers whether a candidate exists.
///
/// A program with a directory separator is not searched for on `PATH`. On Windows every candidate is also
/// tried with each `PATHEXT` extension: after the name as given when it already has an extension, and before it
/// when it has none (so `npx` finds `npx.cmd`, not the extensionless shell script next to it).
pub fn resolve_program(
    program: &str,
    path: Option<&str>,
    pathext: Option<&str>,
    style: PathStyle,
    is_file: &dyn Fn(&Path) -> bool,
) -> Result<PathBuf, ChildError> {
    let windows = style == PathStyle::Windows;
    let has_separator = program.contains('/') || (windows && program.contains(['\\', ':']));
    let extensions: Vec<&str> = if windows {
        pathext
            .unwrap_or(DEFAULT_PATHEXT)
            .split(';')
            .filter(|ext| !ext.is_empty())
            .collect()
    } else {
        Vec::new()
    };
    let file_name = program.rsplit(['/', '\\']).next().unwrap_or(program);
    let has_extension = file_name.rfind('.').is_some_and(|dot| dot > 0);
    let candidates = |base: PathBuf| -> Vec<PathBuf> {
        let with_extensions = extensions.iter().map(|ext| {
            let mut name = base.clone().into_os_string();
            name.push(ext);
            PathBuf::from(name)
        });
        if has_extension {
            std::iter::once(base.clone())
                .chain(with_extensions)
                .collect()
        } else {
            with_extensions
                .chain(std::iter::once(base.clone()))
                .collect()
        }
    };

    if has_separator {
        if !windows {
            // The operating system reports a missing file itself.
            return Ok(PathBuf::from(program));
        }
        return candidates(PathBuf::from(program))
            .into_iter()
            .find(|candidate| is_file(candidate))
            .ok_or_else(|| {
                ChildError::NotFound(format!(
                    "command not found: {program} (also tried with the PATHEXT extensions {})",
                    extensions.join(";")
                ))
            });
    }
    let separator = if windows { ';' } else { ':' };
    let path = path.unwrap_or("");
    for dir in path.split(separator).filter(|dir| !dir.is_empty()) {
        let found = candidates(Path::new(dir).join(program))
            .into_iter()
            .find(|candidate| is_file(candidate));
        if let Some(found) = found {
            return Ok(found);
        }
    }
    let searched = if windows {
        format!("PATH with the PATHEXT extensions {}", extensions.join(";"))
    } else {
        "PATH".to_owned()
    };
    Err(ChildError::NotFound(format!(
        "command not found: {program} (searched {searched}); give the full path of the program or add its \
         directory to PATH"
    )))
}

/// The production [`ChildProcess`].
#[derive(Clone, Debug, Default)]
pub struct TokioChildProcess {
    trampoline: Option<(PathBuf, Vec<String>)>,
}

impl TokioChildProcess {
    /// Children start directly, with no parent-death signal.
    pub fn new() -> Self {
        Self::default()
    }

    /// On Linux, children start through `exe args... <parent pid> <program> <program args...>`, where `exe`
    /// must hand everything after `args` to [`parent_death_trampoline`]. The child then gets `SIGTERM` when
    /// this process dies. The signal is tied to the thread that calls [`ChildProcess::spawn`]: call it from a
    /// thread that lives as long as the process (the main thread, or a runtime worker), never from a blocking
    /// pool thread. Other platforms ignore the trampoline.
    #[must_use]
    pub fn with_parent_death_trampoline(mut self, exe: PathBuf, args: Vec<String>) -> Self {
        self.trampoline = Some((exe, args));
        self
    }
}

/// The body of the trampoline process: `args` is `<parent pid> <program> <program args...>`. Asks the kernel
/// for `SIGTERM` when the parent dies, then replaces this process with the program. Returns only on failure.
#[cfg(unix)]
pub fn parent_death_trampoline(args: &[String]) -> io::Error {
    use std::os::unix::process::CommandExt as _;

    let (Some(parent), Some(program)) = (args.first(), args.get(1)) else {
        return io::Error::other("the trampoline needs a parent pid and a program");
    };
    #[cfg(target_os = "linux")]
    {
        let Ok(parent) = parent.parse::<i32>() else {
            return io::Error::other("the trampoline's parent pid is not a number");
        };
        if let Err(errno) = nix::sys::prctl::set_pdeathsig(nix::sys::signal::Signal::SIGTERM) {
            return io::Error::from(errno);
        }
        // The parent may have died before the request took effect; the signal would then never come.
        if nix::unistd::getppid().as_raw() != parent {
            return io::Error::other("the parent process exited before the child started");
        }
    }
    #[cfg(not(target_os = "linux"))]
    let _unused = parent;
    std::process::Command::new(program).args(&args[2..]).exec()
}

fn exit_status(status: std::process::ExitStatus) -> ExitStatus {
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt as _;
        if let Some(signal) = status.signal() {
            return ExitStatus::Signal(signal);
        }
    }
    ExitStatus::Code(status.code().unwrap_or(1))
}

#[derive(Debug)]
struct Control {
    child: Box<dyn ChildWrapper>,
    exited: Option<ExitStatus>,
}

#[async_trait]
impl ChildControl for Control {
    async fn wait(&mut self) -> io::Result<ExitStatus> {
        if let Some(status) = self.exited {
            return Ok(status);
        }
        // The innermost child: the group and job wrappers wait for every process of the tree, and a lingering
        // grandchild must not delay reporting the child's own exit.
        let status = exit_status(self.child.inner_mut().wait().await?);
        self.exited = Some(status);
        Ok(status)
    }

    fn kill(&mut self) -> io::Result<()> {
        self.child.start_kill()
    }

    fn signal(&mut self, signal: ChildSignal) -> io::Result<()> {
        #[cfg(unix)]
        {
            self.child.signal(signal.number())
        }
        #[cfg(not(unix))]
        {
            let _unused = signal;
            self.child.start_kill()
        }
    }
}

impl Drop for Control {
    fn drop(&mut self) {
        // Ends whatever is left of the process group or job. An empty group reports an error; ignore it.
        let _already_gone = self.child.start_kill();
    }
}

/// Starts `argv` with piped standard input and output, in its own process group (Unix) or Job Object
/// (Windows).
fn start(argv: &[OsString], spec: &ChildSpec) -> io::Result<Box<dyn ChildWrapper>> {
    let Some((program, args)) = argv.split_first() else {
        return Err(io::Error::other("no program to start"));
    };
    let mut command = CommandWrap::with_new(program, |command| {
        command
            .args(args)
            .envs(spec.env.iter().map(|(name, value)| (name, value)))
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(match spec.stderr {
                StderrMode::Inherit => std::process::Stdio::inherit(),
                StderrMode::Pipe => std::process::Stdio::piped(),
            });
        if let Some(cwd) = &spec.cwd {
            command.current_dir(cwd);
        }
    });
    #[cfg(unix)]
    command.wrap(process_wrap::tokio::ProcessGroup::leader());
    #[cfg(windows)]
    {
        // `KillOnDrop` is what makes the Job Object kill-on-close; `JobObject` starts the child suspended,
        // assigns it and resumes it.
        command.wrap(process_wrap::tokio::KillOnDrop);
        command.wrap(process_wrap::tokio::JobObject);
    }
    command.spawn()
}

#[async_trait]
impl ChildProcess for TokioChildProcess {
    async fn spawn(&self, spec: ChildSpec) -> Result<Child, ChildError> {
        let path = std::env::var("PATH").ok();
        let pathext = std::env::var("PATHEXT").ok();
        let program = resolve_program(
            &spec.program,
            path.as_deref(),
            pathext.as_deref(),
            PathStyle::native(),
            &|candidate| candidate.is_file(),
        )?;

        let direct: Vec<OsString> = std::iter::once(program.clone().into_os_string())
            .chain(spec.args.iter().map(OsString::from))
            .collect();
        let through_trampoline = match &self.trampoline {
            Some((exe, args)) if cfg!(target_os = "linux") => {
                let argv: Vec<OsString> = std::iter::once(exe.clone().into_os_string())
                    .chain(args.iter().map(OsString::from))
                    .chain(std::iter::once(std::process::id().to_string().into()))
                    .chain(direct.iter().cloned())
                    .collect();
                match start(&argv, &spec) {
                    Ok(child) => Some(child),
                    Err(error) => {
                        // For example the executable was replaced while running. The child still starts; it
                        // only loses the parent-death signal.
                        tracing::warn!(%error, "the child trampoline could not start; starting the child directly");
                        None
                    }
                }
            }
            _ => None,
        };
        let mut child = match through_trampoline {
            Some(child) => child,
            None => start(&direct, &spec).map_err(|source| ChildError::Spawn {
                program: program.display().to_string(),
                source,
            })?,
        };
        let stdin = child
            .stdin()
            .take()
            .map(|pipe| Box::new(pipe) as ChildInput);
        let stdout = child
            .stdout()
            .take()
            .map(|pipe| Box::new(pipe) as ChildOutput);
        let stderr = child
            .stderr()
            .take()
            .map(|pipe| Box::new(pipe) as ChildOutput);
        Ok(Child::new(
            stdin,
            stdout,
            stderr,
            Box::new(Control {
                child,
                exited: None,
            }),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn resolve(
        program: &str,
        path: &str,
        pathext: Option<&str>,
        style: PathStyle,
        files: &[&str],
    ) -> Result<String, String> {
        // Compare with forward slashes and without case, as the Windows filesystem does.
        let normal = |p: &Path| p.to_string_lossy().replace('\\', "/").to_lowercase();
        resolve_program(program, Some(path), pathext, style, &|candidate| {
            files
                .iter()
                .any(|file| file.to_lowercase() == normal(candidate))
        })
        .map(|found| normal(&found))
        .map_err(|e| e.to_string())
    }

    #[test]
    fn unix_searches_path_in_order_and_leaves_paths_alone() {
        let files = ["/usr/bin/node", "/opt/bin/node", "/opt/bin/uvx"];
        let path = "/opt/bin:/usr/bin";
        assert_eq!(
            resolve("node", path, None, PathStyle::Unix, &files).unwrap(),
            "/opt/bin/node"
        );
        assert_eq!(
            resolve("uvx", "/usr/bin::/opt/bin", None, PathStyle::Unix, &files).unwrap(),
            "/opt/bin/uvx"
        );
        assert_eq!(
            resolve("./server", path, None, PathStyle::Unix, &files).unwrap(),
            "./server"
        );
        let error = resolve("nope", path, None, PathStyle::Unix, &files).unwrap_err();
        assert!(
            error.contains("command not found: nope") && error.contains("PATH"),
            "{error}"
        );
    }

    #[test]
    fn windows_resolves_shims_through_pathext() {
        let files = [
            "C:/node/npx",
            "C:/node/npx.cmd",
            "C:/node/node.exe",
            "C:/tools/run.py",
            "C:/tools/run.py.exe",
        ];
        let path = "C:/empty;C:/node;C:/tools";
        let win = PathStyle::Windows;
        // The extensionless shell script next to the shim is not picked.
        assert_eq!(
            resolve("npx", path, None, win, &files).unwrap(),
            "c:/node/npx.cmd"
        );
        assert_eq!(
            resolve("node", path, None, win, &files).unwrap(),
            "c:/node/node.exe"
        );
        // A name with an extension is tried as given first.
        assert_eq!(
            resolve("run.py", path, None, win, &files).unwrap(),
            "c:/tools/run.py"
        );
        assert_eq!(
            resolve("npx.cmd", path, None, win, &files).unwrap(),
            "c:/node/npx.cmd"
        );
        // PATHEXT order decides; an empty entry is skipped.
        assert_eq!(
            resolve(
                "npx",
                path,
                Some(";.CMD;.EXE"),
                win,
                &["C:/node/npx.EXE", "C:/node/npx.CMD"]
            )
            .unwrap(),
            "c:/node/npx.cmd"
        );
        // A path is not searched for, but still gets the extensions.
        assert_eq!(
            resolve("C:\\node\\npx", path, None, win, &files).unwrap(),
            "c:/node/npx.cmd"
        );
        let error = resolve("nope", path, None, win, &files).unwrap_err();
        assert!(error.contains(".COM;.EXE;.BAT;.CMD"), "{error}");
        let error = resolve("C:\\node\\nope", path, None, win, &files).unwrap_err();
        assert!(error.contains("command not found"), "{error}");
    }
}
