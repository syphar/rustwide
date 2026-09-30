//! Command execution and sandboxing.

mod process_lines_actions;
mod sandbox;

pub use process_lines_actions::ProcessLinesActions;
pub use sandbox::*;

use crate::native;
use crate::workspace::Workspace;
use futures_util::{
    future::{self, FutureExt},
    stream::{self, TryStreamExt},
};
use log::{error, info};
use process_lines_actions::InnerState;
use std::ffi::{OsStr, OsString};
use std::fmt;
use std::path::{Path, PathBuf};
use std::process::{ExitStatus, Stdio};
use std::time::{Duration, Instant};
use std::{cell::RefCell, env::consts::EXE_SUFFIX, rc::Rc};
use std::{
    convert::AsRef,
    sync::{Arc, LazyLock, Mutex},
};
use tokio::{
    io::{AsyncBufReadExt, BufReader},
    process::Command as AsyncCommand,
    runtime::Runtime,
    time,
};
use tokio_stream::{StreamExt, wrappers::LinesStream};

// TODO: Migrate to asynchronous code and remove runtime
pub(super) static RUNTIME: LazyLock<Runtime> =
    LazyLock::new(|| Runtime::new().expect("Failed to construct tokio runtime"));

pub(crate) mod container_dirs {
    use std::path::{Path, PathBuf};
    use std::sync::LazyLock;

    macro_rules! path_const {
        ($v:ident, $n:ident, $p:expr) => {
            pub($v) static $n: LazyLock<PathBuf> = LazyLock::new(|| $p);
        };
    }

    #[cfg(windows)]
    path_const!(super, ROOT_DIR, Path::new(r"C:\rustwide").into());
    #[cfg(not(windows))]
    path_const!(super, ROOT_DIR, Path::new("/opt/rustwide").into());

    path_const!(crate, WORK_DIR, ROOT_DIR.join("workdir"));
    path_const!(crate, TARGET_DIR, ROOT_DIR.join("target"));
    path_const!(super, CARGO_HOME, ROOT_DIR.join("cargo-home"));
    path_const!(super, RUSTUP_HOME, ROOT_DIR.join("rustup-home"));
    path_const!(super, CARGO_BIN_DIR, CARGO_HOME.join("bin"));
}

/// Error happened while executing a command.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum CommandError {
    /// The command didn't output anything to stdout or stderr for more than the timeout, and it
    /// was killed. The timeout's value (in seconds) is the first value.
    #[error("no output for {0} seconds")]
    NoOutputFor(u64),

    /// The command took more time than the timeout to end, and it was killed. The timeout's value
    /// (in seconds) is the first value.
    #[error("command timed out after {0} seconds")]
    Timeout(u64),

    /// The command failed to execute.
    #[error("command failed: {status}\n\n{stderr}")]
    ExecutionFailed {
        /// the exit status we got from the command
        status: ExitStatus,
        /// the stderr output, if it was captured via `.run_capture()`
        stderr: String,
    },

    /// Killing the underlying process after the timeout failed.
    #[error("{0}")]
    KillAfterTimeoutFailed(#[source] KillFailedError),

    /// The sandbox ran out of memory and was killed.
    #[error("container ran out of memory")]
    SandboxOOM,

    /// A sandboxed command was spawned while another sandboxed command on the
    /// same sandbox was still running (typically from inside a
    /// [`process_lines`](struct.Command.html#method.process_lines) callback).
    /// The reused-container model serializes commands through a single
    /// `&mut Sandbox`, so this nesting is not supported.
    #[error("re-entrant sandboxed commands are not supported")]
    ReentrantSandbox,

    /// Pulling a sandbox image from the registry failed
    #[error("failed to pull the sandbox image from the registry: {0}")]
    SandboxImagePullFailed(#[source] Box<CommandError>),

    /// The sandbox image is missing from the local system.
    #[error("sandbox image missing from the local system: {0}")]
    SandboxImageMissing(#[source] Box<CommandError>),

    /// Failed to create the sandbox container
    #[error("sandbox container could not be created: {0}")]
    SandboxContainerCreate(#[source] Box<CommandError>),

    /// Running rustwide inside a Docker container requires the workspace directory to be mounted
    /// from the host system. This error happens if that's not true, for example if the workspace
    /// lives in a directory inside the container.
    #[error("the workspace is not mounted from outside the container")]
    WorkspaceNotMountedCorrectly,

    /// The data received from the `docker inspect` command is not valid.
    #[error("invalid output of `docker inspect`: {0}")]
    InvalidDockerInspectOutput(#[source] serde_json::Error),

    /// An I/O error occured while executing the command.
    #[error(transparent)]
    IO(#[from] std::io::Error),
}

/// Error happened while trying to kill a process.
#[derive(Debug, thiserror::Error)]
#[cfg_attr(unix, error(
    "failed to kill the process with PID {pid}{}",
    .errno.map(|e| format!(": {}", e.desc())).unwrap_or_default()
))]
#[cfg_attr(not(unix), error("failed to kill the process with PID {pid}"))]
pub struct KillFailedError {
    pub(crate) pid: u32,
    #[cfg(unix)]
    pub(crate) errno: Option<nix::errno::Errno>,
}

impl KillFailedError {
    /// Return the PID of the process that couldn't be killed.
    pub fn pid(&self) -> u32 {
        self.pid
    }

    /// Return the underlying error number provided by the operative system.
    #[cfg(any(unix, doc))]
    #[cfg_attr(docs_rs, doc(cfg(unix)))]
    pub fn errno(&self) -> Option<i32> {
        self.errno.map(|errno| errno as i32)
    }
}

/// Name and kind of a binary executed by [`Command`](struct.Command.html).
#[non_exhaustive]
#[derive(Debug)]
pub enum Binary {
    /// Global binary, available in `$PATH`. Rustwide doesn't apply any tweaks to its execution
    /// environment.
    Global(PathBuf),
    /// Binary installed and managed by Rustwide in its local rustup installation. Rustwide will
    /// tweak the environment to use the local rustup instead of the host system one, and will
    /// search the binary in the cargo home.
    ManagedByRustwide(PathBuf),
}

/// Trait representing a command that can be run by [`Command`](struct.Command.html).
pub trait Runnable {
    /// The name of the binary to execute.
    fn name(&self) -> Binary;

    /// Prepare the command for execution. This method is called as soon as a
    /// [`Command`](struct.Command.html) instance is created, and allows tweaking the command to
    /// better suit your binary, for example by adding default arguments or environment variables.
    ///
    /// The default implementation simply returns the provided command without changing anything in
    /// it.
    fn prepare_command<'w, 'pl>(&self, cmd: Command<'w, 'pl>) -> Command<'w, 'pl> {
        cmd
    }
}

impl Runnable for &str {
    fn name(&self) -> Binary {
        Binary::Global(self.into())
    }
}

impl Runnable for String {
    fn name(&self) -> Binary {
        Binary::Global(self.into())
    }
}

impl<B: Runnable> Runnable for &B {
    fn name(&self) -> Binary {
        Runnable::name(*self)
    }

    fn prepare_command<'w, 'pl>(&self, cmd: Command<'w, 'pl>) -> Command<'w, 'pl> {
        Runnable::prepare_command(*self, cmd)
    }
}

/// The `Command` is a builder to execute system commands and interact with them.
///
/// It's a more advanced version of [`std::process::Command`][std], featuring timeouts, realtime
/// output processing, output logging and sandboxing.
///
/// [std]: https://doc.rust-lang.org/std/process/struct.Command.html
#[must_use = "call `.run()` to run the command"]
#[allow(clippy::type_complexity)]
pub struct Command<'w, 'pl> {
    workspace: Option<&'w Workspace>,
    sandbox: Option<Rc<RefCell<Sandbox<'w>>>>,
    binary: Binary,
    args: Vec<OsString>,
    env: Vec<(OsString, OsString)>,
    process_lines: Option<&'pl mut dyn FnMut(&str, &mut ProcessLinesActions)>,
    current_directory: Option<PathBuf>,
    timeout: Option<Duration>,
    no_output_timeout: Option<Duration>,
    log_command: bool,
    log_output: bool,
    render_cargo_messages: bool,
    cargo_messages: Option<CargoMessages>,
}

// Custom Debug keeps command output focused: environment variables are shown as keys only,
// since values often contain secrets, and `sandbox`/`process_lines` are summarized as presence
// flags.
impl fmt::Debug for Command<'_, '_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Command")
            .field("is_sandboxed", &self.sandbox.is_some())
            .field("binary", &self.binary)
            .field("args", &self.args)
            .field("env", &self.env.iter().map(|(k, _)| k).collect::<Vec<_>>())
            .field("has_process_lines", &self.process_lines.is_some())
            .field("current_directory", &self.current_directory)
            .field("timeout", &self.timeout)
            .field("no_output_timeout", &self.no_output_timeout)
            .field("log_command", &self.log_command)
            .field("log_output", &self.log_output)
            .field("render_cargo_messages", &self.render_cargo_messages)
            .field("captures_cargo_messages", &self.cargo_messages.is_some())
            .finish()
    }
}

impl<'w> Command<'w, '_> {
    /// Create a new, unsandboxed command.
    pub fn new<R: Runnable>(workspace: &'w Workspace, binary: R) -> Self {
        binary.prepare_command(Self::new_inner(binary.name(), Some(workspace), None))
    }

    /// Create a new command that runs inside an existing sandbox.
    ///
    /// By default the command's working directory is the sandbox's source directory; call
    /// [`current_directory`](#method.current_directory) to override it. Any explicit path
    /// must point inside the sandbox source directory — paths outside it will panic at
    /// runtime.
    pub fn new_in_sandbox<R: Runnable>(
        workspace: &'w Workspace,
        sandbox: Rc<RefCell<Sandbox<'w>>>,
        binary: R,
    ) -> Self {
        binary.prepare_command(Self::new_inner(
            binary.name(),
            Some(workspace),
            Some(sandbox),
        ))
    }

    pub(crate) fn new_workspaceless<R: Runnable>(binary: R) -> Self {
        binary.prepare_command(Self::new_inner(binary.name(), None, None))
    }

    fn new_inner(
        binary: Binary,
        workspace: Option<&'w Workspace>,
        sandbox: Option<Rc<RefCell<Sandbox<'w>>>>,
    ) -> Self {
        let (timeout, no_output_timeout) = if let Some(workspace) = workspace {
            (
                workspace.default_command_timeout(),
                workspace.default_command_no_output_timeout(),
            )
        } else {
            (None, None)
        };
        Command {
            workspace,
            sandbox,
            binary,
            args: Vec::new(),
            env: Vec::new(),
            process_lines: None,
            current_directory: None,
            timeout,
            no_output_timeout,
            log_output: true,
            log_command: true,
            render_cargo_messages: false,
            cargo_messages: None,
        }
    }

    /// Add a command-line argument to the command. This method can be called multiple times to add
    /// additional args.
    pub fn arg(mut self, arg: impl Into<OsString>) -> Self {
        self.args.push(arg.into());
        self
    }

    /// Add command-line arguments to the command. This method can be called multiple times to add
    /// additional args.
    pub fn args<S: Into<OsString>>(mut self, args: impl IntoIterator<Item = S>) -> Self {
        for arg in args {
            self = self.arg(arg);
        }
        self
    }

    /// Add an environment variable to the command.
    pub fn env<S1: Into<OsString>, S2: Into<OsString>>(mut self, key: S1, value: S2) -> Self {
        self.env.push((key.into(), value.into()));
        self
    }

    /// Change the directory where the command will be executed in.
    pub fn current_directory<P: Into<PathBuf>>(mut self, path: P) -> Self {
        self.current_directory = Some(path.into());
        self
    }

    /// Set the timeout of this command. If it runs for more time the process will be killed.
    ///
    /// Its default value is configured through
    /// [`WorkspaceBuilder::command_timeout`](../struct.WorkspaceBuilder.html#method.command_timeout).
    pub fn timeout(mut self, timeout: Option<Duration>) -> Self {
        self.timeout = timeout;
        self
    }

    /// Set the no output timeout of this command. If it doesn't output anything for more time the
    /// process will be killed.
    ///
    /// Its default value is configured through
    /// [`WorkspaceBuilder::command_no_output_timeout`](../struct.WorkspaceBuilder.html#method.command_no_output_timeout).
    pub fn no_output_timeout(mut self, timeout: Option<Duration>) -> Self {
        self.no_output_timeout = timeout;
        self
    }

    /// Set the function that will be called each time a line is outputted to either the standard
    /// output or the standard error. Only one function can be set at any time for a command.
    ///
    /// For sandboxed commands, the callback runs while the underlying [`Sandbox`] is mutably
    /// borrowed. Spawning another sandboxed command (e.g. via [`Build::cmd`](../build/struct.Build.html#method.cmd))
    /// from inside the callback is not supported with the reused-container model and will return
    /// [`CommandError::ReentrantSandbox`](enum.CommandError.html#variant.ReentrantSandbox).
    ///
    /// The method is useful to analyze the command's output without storing all of it in memory.
    /// This example builds a crate and detects compiler errors (ICEs):
    ///
    /// ```no_run
    /// # use rustwide::{cmd::Command, WorkspaceBuilder};
    /// # use std::error::Error;
    /// # fn main() -> Result<(), Box<dyn Error>> {
    /// # let workspace = WorkspaceBuilder::new("".as_ref(), "").init()?;
    /// let mut ice = false;
    /// Command::new(&workspace, "cargo")
    ///     .args(&["build", "--all"])
    ///     .process_lines(&mut |line, _| {
    ///         if line.contains("internal compiler error") {
    ///             ice = true;
    ///         }
    ///     })
    ///     .run()?;
    /// # Ok(())
    /// # }
    /// ```
    pub fn process_lines<'pl>(
        self,
        f: &'pl mut dyn FnMut(&str, &mut ProcessLinesActions),
    ) -> Command<'w, 'pl> {
        Command {
            process_lines: Some(f),
            ..self
        }
    }

    /// Enable or disable logging all the output lines to the [`log` crate][log]. By default
    /// logging is enabled.
    ///
    /// [log]: https://crates.io/crates/log
    pub fn log_output(mut self, log_output: bool) -> Self {
        self.log_output = log_output;
        self
    }

    /// Enable or disable logging the command name and args to the [`log` crate][log] before the
    /// exectuion. By default logging is enabled.
    ///
    /// [log]: https://crates.io/crates/log
    pub fn log_command(mut self, log_command: bool) -> Self {
        self.log_command = log_command;
        self
    }

    /// Render Cargo JSON messages before logging them.
    ///
    /// This is intended for commands run with Cargo's
    /// `--message-format=json` option. The original JSON line is still passed to
    /// [`process_lines`](Self::process_lines), allowing callers to deserialize it, while compiler
    /// diagnostics are rendered in the log output.
    pub(crate) fn render_cargo_messages(mut self) -> Self {
        self.render_cargo_messages = true;
        self
    }

    /// Store parsed Cargo JSON messages in `messages` as the command runs.
    ///
    /// This is intended for commands run with `--message-format=json`, such as those returned by
    /// [`Build::cargo_json`](crate::Build::cargo_json). Messages are captured even when the
    /// command fails, so callers can inspect compiler diagnostics after `run` returns an error.
    pub fn capture_cargo_messages(mut self, messages: &CargoMessages) -> Self {
        self.cargo_messages = Some(messages.clone());
        self
    }

    /// Run the prepared command and return an error if it fails (for example with a non-zero exit
    /// code or a timeout).
    pub fn run(self) -> Result<(), CommandError> {
        self.run_inner(false)?;
        Ok(())
    }

    /// Run the prepared command and return its output if it succeedes. If it fails (for example
    /// with a non-zero exit code or a timeout) an error will be returned instead.
    ///
    /// Even though the output will be captured and returned, if output logging is enabled (as it
    /// is by default) the output will be also logged. You can disable this behavior by calling the
    /// [`log_output`](struct.Command.html#method.log_output) method.
    pub fn run_capture(self) -> Result<ProcessOutput, CommandError> {
        self.run_inner(true)
    }

    #[cfg_attr(feature = "tracing", tracing::instrument(skip_all, level = "debug"))]
    fn run_inner(self, capture: bool) -> Result<ProcessOutput, CommandError> {
        if let Some(sandbox) = self.sandbox {
            let binary = match self.binary {
                Binary::Global(path) => path,
                Binary::ManagedByRustwide(path) => {
                    container_dirs::CARGO_BIN_DIR.join(exe_suffix(path.as_os_str()))
                }
            };

            let args = if self.render_cargo_messages {
                cargo_message_format_args(self.args)
            } else {
                self.args
            };
            let mut command = SandboxCommand::new(binary)
                .args(args)
                .env("SOURCE_DIR", &*container_dirs::WORK_DIR)
                .env("CARGO_HOME", &*container_dirs::CARGO_HOME)
                .env("RUSTUP_HOME", &*container_dirs::RUSTUP_HOME);

            for (key, value) in self.env {
                command = command.env(key, value);
            }

            if let Some(workdir) = self.current_directory {
                command = command.workdir(workdir);
            }

            if let Some(user) = native::current_user() {
                command = command.user(user.user_id, user.group_id);
            }

            sandbox
                .try_borrow_mut()
                .map_err(|_| CommandError::ReentrantSandbox)?
                .run(
                    command,
                    self.timeout,
                    self.no_output_timeout,
                    self.process_lines,
                    self.log_output,
                    self.log_command,
                    self.render_cargo_messages,
                    self.cargo_messages,
                    capture,
                )
        } else {
            let (binary, managed_by_rustwide) = match self.binary {
                // global paths should never be normalized
                Binary::Global(path) => (path, false),
                Binary::ManagedByRustwide(path) => {
                    // `cargo_home()` might a relative path
                    let cargo_home = crate::utils::normalize_path(
                        &self
                            .workspace
                            .expect("calling rustwide bins without a workspace is not supported")
                            .cargo_home(),
                    );
                    let binary = cargo_home.join("bin").join(exe_suffix(path.as_os_str()));
                    (binary, true)
                }
            };

            let args = if self.render_cargo_messages {
                cargo_message_format_args(self.args)
            } else {
                self.args
            };
            let cmdstr = format_command(binary.as_os_str(), &args);
            let mut cmd = AsyncCommand::new(binary);
            cmd.args(&args);

            if managed_by_rustwide {
                let workspace = self
                    .workspace
                    .expect("calling rustwide bins without a workspace is not supported");
                let cargo_home = workspace
                    .cargo_home()
                    .to_str()
                    .expect("bad cargo home")
                    .to_string();
                let rustup_home = workspace
                    .rustup_home()
                    .to_str()
                    .expect("bad rustup home")
                    .to_string();
                cmd.env(
                    "CARGO_HOME",
                    crate::utils::normalize_path(cargo_home.as_ref()),
                );
                cmd.env(
                    "RUSTUP_HOME",
                    crate::utils::normalize_path(rustup_home.as_ref()),
                );
            }
            for (k, v) in &self.env {
                cmd.env(k, v);
            }

            if let Some(ref current_directory) = self.current_directory {
                cmd.current_dir(current_directory);
            }

            if self.log_command {
                info!("running `{}`", cmdstr.to_string_lossy());
            }

            let out = RUNTIME
                .block_on(log_command(
                    cmd,
                    self.process_lines,
                    capture,
                    self.timeout,
                    self.no_output_timeout,
                    self.log_output,
                    self.render_cargo_messages,
                    self.cargo_messages,
                ))
                .map_err(|e| {
                    error!("error running command: {e}");
                    e
                })?;

            if out.status.success() {
                Ok(out.into())
            } else {
                Err(CommandError::ExecutionFailed {
                    status: out.status,
                    stderr: out.stderr.join("\n"),
                })
            }
        }
    }
}

struct InnerProcessOutput {
    status: ExitStatus,
    stdout: Vec<String>,
    stderr: Vec<String>,
}

impl From<InnerProcessOutput> for ProcessOutput {
    fn from(orig: InnerProcessOutput) -> ProcessOutput {
        ProcessOutput {
            stdout: orig.stdout,
            stderr: orig.stderr,
        }
    }
}

/// Output of a [`Command`](struct.Command.html) when it was executed with the
/// [`run_capture`](struct.Command.html#method.run_capture) method.
#[derive(Debug)]
pub struct ProcessOutput {
    stdout: Vec<String>,
    stderr: Vec<String>,
}

/// Storage for parsed messages emitted by Cargo with `--message-format=json`.
///
/// Unlike [`crate::logging::LogStorage`], this stores structured JSON values rather than rendered
/// log lines. It can be cloned and shared with the command while it runs.
#[derive(Clone, Default)]
pub struct CargoMessages {
    inner: Arc<Mutex<Vec<serde_json::Value>>>,
}

impl CargoMessages {
    /// Create an empty Cargo message storage.
    pub fn new() -> Self {
        Self::default()
    }

    /// Return all captured Cargo messages.
    pub fn messages(&self) -> Vec<serde_json::Value> {
        self.inner.lock().unwrap().clone()
    }

    /// Remove and return all captured Cargo messages.
    pub fn take_messages(&self) -> Vec<serde_json::Value> {
        std::mem::take(&mut *self.inner.lock().unwrap())
    }

    fn push(&self, message: serde_json::Value) {
        self.inner.lock().unwrap().push(message);
    }
}

impl ProcessOutput {
    /// Return a list of the lines printed by the process on the standard output.
    pub fn stdout_lines(&self) -> &[String] {
        &self.stdout
    }

    /// Return a list of the lines printed by the process on the standard error.
    pub fn stderr_lines(&self) -> &[String] {
        &self.stderr
    }
}

enum OutputKind {
    Stdout,
    Stderr,
}

impl OutputKind {
    fn prefix(&self) -> &'static str {
        match *self {
            OutputKind::Stdout => "stdout",
            OutputKind::Stderr => "stderr",
        }
    }
}

#[allow(clippy::type_complexity)]
async fn log_command(
    mut cmd: AsyncCommand,
    mut process_lines: Option<&mut dyn FnMut(&str, &mut ProcessLinesActions)>,
    capture: bool,
    timeout: Option<Duration>,
    no_output_timeout: Option<Duration>,
    log_output: bool,
    render_cargo_messages: bool,
    cargo_messages: Option<CargoMessages>,
) -> Result<InnerProcessOutput, CommandError> {
    let timeout = timeout.unwrap_or_else(|| Duration::from_secs(u32::MAX as u64));
    let no_output_timeout = no_output_timeout.unwrap_or(timeout);

    let mut child = cmd.stdout(Stdio::piped()).stderr(Stdio::piped()).spawn()?;
    let child_id = child.id().unwrap();

    let stdout = LinesStream::new(BufReader::new(child.stdout.take().unwrap()).lines())
        .map(|line| (OutputKind::Stdout, line));
    let stderr = LinesStream::new(BufReader::new(child.stderr.take().unwrap()).lines())
        .map(|line| (OutputKind::Stderr, line));

    let start = Instant::now();
    let mut actions = ProcessLinesActions::new();

    let output = stream::select(stdout, stderr)
        .timeout(no_output_timeout)
        .map(move |result| match result {
            // If the timeout elapses, kill the process
            Err(_timeout) => Err(match native::kill_process(child_id) {
                Ok(()) => CommandError::NoOutputFor(no_output_timeout.as_secs()),
                Err(err) => CommandError::KillAfterTimeoutFailed(err),
            }),

            // If an error occurred reading the line, flatten the error
            Ok((_, Err(read_err))) => Err(read_err.into()),

            // If the read was successful, return the `OutputKind` and the read line
            Ok((out_kind, Ok(line))) => Ok((out_kind, line)),
        })
        .and_then(move |(kind, line): (OutputKind, String)| {
            // If the process is in a tight output loop the timeout on the process might fail to
            // be executed, so this extra check prevents the process to run without limits.
            if start.elapsed() > timeout {
                return future::err(CommandError::Timeout(timeout.as_secs()));
            }

            let cargo_message = (render_cargo_messages || cargo_messages.is_some())
                .then(|| parse_cargo_message(&line))
                .flatten();
            if let Some(message) = &cargo_message {
                if let Some(messages) = &cargo_messages {
                    messages.push(message.clone());
                }
                if render_cargo_messages {
                    render_cargo_message(message, &mut actions);
                }
            }

            if let Some(f) = &mut process_lines {
                f(&line, &mut actions);
            }
            // this is done here to avoid duplicating the output line
            let lines = match actions.take_lines() {
                InnerState::Removed => Vec::new(),
                InnerState::Original => vec![line],
                InnerState::Replaced(new_lines) => new_lines,
            };

            if log_output {
                for line in &lines {
                    info!("[{}] {}", kind.prefix(), line);
                }
            }

            future::ok((kind, lines))
        })
        .try_fold(
            (Vec::<String>::new(), Vec::<String>::new()),
            move |(mut stdout, mut stderr), (kind, mut lines)| async move {
                // If stdio/stdout is supposed to be captured, append it to
                // the accumulated stdio/stdout
                if capture {
                    match kind {
                        OutputKind::Stdout => stdout.append(&mut lines),
                        OutputKind::Stderr => stderr.append(&mut lines),
                    }
                }

                Ok((stdout, stderr))
            },
        );

    let child = time::timeout(timeout, child.wait()).map(move |result| {
        match result {
            // If the timeout elapses, kill the process
            Err(_timeout) => Err(match native::kill_process(child_id) {
                Ok(()) => CommandError::Timeout(timeout.as_secs()),
                Err(err) => CommandError::KillAfterTimeoutFailed(err),
            }),

            // If an error occurred with the child
            Ok(Err(err)) => Err(err.into()),

            // If the read was successful, return the process's exit status
            Ok(Ok(exit_status)) => Ok(exit_status),
        }
    });

    let ((stdout, stderr), status) = {
        let (output, child) = future::join(output, child).await;
        let (stdout, stderr) = output?;

        ((stdout, stderr), child?)
    };

    Ok(InnerProcessOutput {
        status,
        stdout,
        stderr,
    })
}

fn parse_cargo_message(line: &str) -> Option<serde_json::Value> {
    let message = serde_json::from_str::<serde_json::Value>(line).ok()?;
    message.get("reason").and_then(serde_json::Value::as_str)?;
    Some(message)
}

fn render_cargo_message(message: &serde_json::Value, actions: &mut ProcessLinesActions) {
    let reason = message
        .get("reason")
        .and_then(serde_json::Value::as_str)
        .expect("Cargo messages must have a string reason");

    if reason != "compiler-message" {
        actions.remove_line();
        return;
    }

    match message
        .pointer("/message/rendered")
        .and_then(serde_json::Value::as_str)
    {
        Some(rendered) => actions.replace_with_lines(rendered.lines()),
        None => actions.remove_line(),
    }
}

fn cargo_message_format_args(mut args: Vec<OsString>) -> Vec<OsString> {
    let position = args
        .iter()
        .position(|arg| arg == "--")
        .unwrap_or(args.len());
    args.insert(position, "--message-format=json".into());
    args
}

fn format_command<S1, S2, I>(binary: S1, args: I) -> OsString
where
    S1: AsRef<OsStr>,
    S2: AsRef<OsStr>,
    I: IntoIterator<Item = S2>,
{
    let binary = binary.as_ref();
    let binary_name = Path::new(binary).file_name().unwrap_or(binary);

    let mut command = OsString::from(format!("{:?}", binary_name));

    for arg in args {
        command.push(format!(" {:?}", arg.as_ref()));
    }
    command
}

fn exe_suffix(file: &OsStr) -> OsString {
    let mut path = OsString::from(file);
    path.push(EXE_SUFFIX);
    path
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cmd::process_lines_actions::InnerState;

    #[test]
    fn formats_only_the_program_and_arguments() {
        let args = ["argument", "argument with spaces"];

        assert_eq!(
            format_command(OsStr::new("/path/to/program"), args),
            r#""program" "argument" "argument with spaces""#
        );
    }

    #[test]
    fn renders_compiler_diagnostics_from_cargo_json() {
        let mut actions = ProcessLinesActions::new();
        render_cargo_message(
            &parse_cargo_message(
                r#"{"reason":"compiler-message","message":{"rendered":"error: something went wrong\n  --> src/lib.rs:1:1\n"}}"#,
            )
            .unwrap(),
            &mut actions,
        );

        assert_eq!(
            actions.take_lines(),
            InnerState::Replaced(vec![
                "error: something went wrong".into(),
                "  --> src/lib.rs:1:1".into(),
            ])
        );
    }

    #[test]
    fn hides_non_diagnostic_cargo_json_messages() {
        let mut actions = ProcessLinesActions::new();
        render_cargo_message(
            &parse_cargo_message(r#"{"reason":"compiler-artifact"}"#).unwrap(),
            &mut actions,
        );

        assert_eq!(actions.take_lines(), InnerState::Removed);
    }

    #[test]
    fn preserves_non_json_output_from_the_built_binary() {
        let mut actions = ProcessLinesActions::new();
        assert!(parse_cargo_message("Hello, world!").is_none());

        assert_eq!(actions.take_lines(), InnerState::Original);
    }

    #[test]
    fn stores_parsed_cargo_messages() {
        let messages = CargoMessages::new();
        messages
            .push(parse_cargo_message(r#"{"reason":"build-finished","success":false}"#).unwrap());

        assert_eq!(messages.messages().len(), 1);
        assert_eq!(messages.take_messages()[0]["success"], false);
        assert!(messages.messages().is_empty());
    }

    #[test]
    fn adds_cargo_message_format_before_program_arguments() {
        assert_eq!(
            cargo_message_format_args(
                ["run", "--release", "--", "argument"]
                    .into_iter()
                    .map(Into::into)
                    .collect(),
            ),
            [
                "run",
                "--release",
                "--message-format=json",
                "--",
                "argument",
            ]
            .map(OsString::from)
        );
    }
}
