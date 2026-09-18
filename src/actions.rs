//! A single asynchronous action worker; subprocesses never write to the terminal.
use crate::{
    model::{ActionKind, BranchEntry, CancelToken, WorkId, display_os, display_path},
    scan::validate_directory,
};
use std::{
    collections::BTreeMap,
    ffi::{OsStr, OsString},
    fs::{self, OpenOptions},
    io::{Read, Write},
    os::unix::{
        ffi::{OsStrExt, OsStringExt},
        fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
        io::AsRawFd,
        process::CommandExt,
    },
    path::{Path, PathBuf},
    process::{Command, ExitStatus, Stdio},
    sync::mpsc::{self, Receiver, SyncSender},
    thread,
    time::{Duration, Instant},
};

#[derive(Debug)]
pub struct ActionEvent {
    pub id: WorkId,
    pub outcome: ActionOutcome,
}
#[derive(Debug)]
pub enum ActionOutcome {
    BranchesLoaded {
        branches: Vec<BranchEntry>,
    },
    InvalidTarget {
        message: String,
    },
    Checked {
        confirmation: Option<String>,
        already_done: bool,
    },
    Completed {
        message: String,
        created: Option<PathBuf>,
    },
    Failed {
        message: String,
        uncertain: bool,
    },
    Cancelled {
        uncertain: bool,
    },
}
struct Request {
    id: WorkId,
    kind: ActionKind,
    path: PathBuf,
    child_name: Option<String>,
    branch: Option<OsString>,
    cancel: CancelToken,
    check: bool,
    deadline: Instant,
}
pub struct Actions {
    sender: SyncSender<Request>,
    receiver: Receiver<ActionEvent>,
    events: mpsc::Sender<ActionEvent>,
    shutdown: CancelToken,
}
impl Default for Actions {
    fn default() -> Self {
        Self::new()
    }
}
impl Actions {
    pub fn new() -> Self {
        Self::with_environment(std::env::vars_os().collect())
    }
    /// Explicit environment snapshot for isolated callers/tests; never mutates process globals.
    pub fn with_environment(environment: BTreeMap<OsString, OsString>) -> Self {
        let (sender, requests) = mpsc::sync_channel::<Request>(1);
        let (events, receiver) = mpsc::channel();
        let shutdown = CancelToken::default();
        let stop = shutdown.clone();
        let event_sender = events.clone();
        thread::spawn(move || {
            let mut worker = Worker {
                environment,
                names: None,
                prepared: None,
                shutdown: stop,
            };
            while let Ok(request) = requests.recv() {
                let id = request.id;
                let outcome = worker.execute(request);
                if events.send(ActionEvent { id, outcome }).is_err() {
                    break;
                }
            }
        });
        Self {
            sender,
            receiver,
            events: event_sender,
            shutdown,
        }
    }
    pub fn start_check(&self, id: WorkId, kind: ActionKind, path: PathBuf, cancel: CancelToken) {
        self.submit(Request {
            id,
            kind,
            path,
            child_name: None,
            branch: None,
            cancel,
            check: true,
            deadline: Instant::now() + Duration::from_secs(3),
        });
    }
    pub fn start_run(
        &self,
        id: WorkId,
        kind: ActionKind,
        path: PathBuf,
        child_name: Option<String>,
        cancel: CancelToken,
    ) {
        self.submit(Request {
            id,
            kind,
            path,
            child_name,
            branch: None,
            cancel,
            check: false,
            deadline: Instant::now() + Duration::from_secs(30),
        });
    }
    pub fn start_branch_list(&self, id: WorkId, path: PathBuf, cancel: CancelToken) {
        self.submit(Request {
            id,
            kind: ActionKind::ListBranches,
            path,
            child_name: None,
            branch: None,
            cancel,
            check: true,
            deadline: Instant::now() + Duration::from_secs(3),
        });
    }
    pub fn start_branch_check(
        &self,
        id: WorkId,
        path: PathBuf,
        branch: OsString,
        cancel: CancelToken,
    ) {
        self.submit(Request {
            id,
            kind: ActionKind::OpenBranchWorkspace,
            path,
            child_name: None,
            branch: Some(branch),
            cancel,
            check: true,
            deadline: Instant::now() + Duration::from_secs(3),
        });
    }
    pub fn start_branch_run(
        &self,
        id: WorkId,
        path: PathBuf,
        branch: OsString,
        cancel: CancelToken,
    ) {
        self.submit(Request {
            id,
            kind: ActionKind::OpenBranchWorkspace,
            path,
            child_name: None,
            branch: Some(branch),
            cancel,
            check: false,
            deadline: Instant::now() + Duration::from_secs(30),
        });
    }
    fn submit(&self, request: Request) {
        let id = request.id;
        if self.sender.try_send(request).is_err() {
            let _ = self.events.send(ActionEvent {
                id,
                outcome: failed("Action worker is busy or unavailable", false),
            });
        }
    }
    pub fn try_recv(&self) -> Option<ActionEvent> {
        self.receiver.try_recv().ok()
    }
}
impl Drop for Actions {
    fn drop(&mut self) {
        self.shutdown.cancel();
    }
}

struct Prepared {
    id: WorkId,
    kind: ActionKind,
    path: PathBuf,
    git: Option<PathBuf>,
    tool: Option<PathBuf>,
    branch: Option<OsString>,
    branch_mode: Option<BranchWorkspaceMode>,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum BranchWorkspaceMode {
    Open,
    New,
}
struct Worker {
    environment: BTreeMap<OsString, OsString>,
    names: Option<Vec<String>>,
    prepared: Option<Prepared>,
    shutdown: CancelToken,
}
impl Worker {
    fn execute(&mut self, request: Request) -> ActionOutcome {
        if request.cancel.is_cancelled() || self.shutdown.is_cancelled() {
            return ActionOutcome::Cancelled { uncertain: false };
        }
        if let Err(message) = validate_directory(&request.path) {
            return ActionOutcome::InvalidTarget { message };
        }
        if matches!(
            request.kind,
            ActionKind::OpenHd | ActionKind::OpenPane | ActionKind::OpenBranchWorkspace
        ) && request
            .path
            .to_str()
            .is_none_or(|p| p.contains(['\r', '\n']))
        {
            return failed("hd/Herdr require a UTF-8 path without CR or LF", false);
        }
        if request.check {
            self.check(&request)
        } else {
            self.run(&request)
        }
    }
    fn command(
        &self,
        executable: &Path,
        args: &[&OsStr],
        request: &Request,
        duration: Duration,
        side_effect: bool,
    ) -> Result<Output, ActionOutcome> {
        let duration = if request.check {
            duration.min(request.deadline.saturating_duration_since(Instant::now()))
        } else {
            duration
        };
        run_process(
            executable,
            args,
            ProcessContext {
                cwd: &request.path,
                environment: &self.environment,
                cancel: &request.cancel,
                shutdown: &self.shutdown,
            },
            duration,
            side_effect,
        )
    }
    fn check(&mut self, request: &Request) -> ActionOutcome {
        self.prepared = None;
        let mut prepared = Prepared {
            id: request.id,
            kind: request.kind,
            path: request.path.clone(),
            git: None,
            tool: None,
            branch: None,
            branch_mode: None,
        };
        let mut confirmation = None;
        let mut already_done = false;
        if matches!(
            request.kind,
            ActionKind::GitInit
                | ActionKind::OpenHd
                | ActionKind::ListBranches
                | ActionKind::OpenBranchWorkspace
        ) {
            let injected: Vec<_> = self
                .environment
                .keys()
                .filter(|name| {
                    name.as_bytes().starts_with(b"GIT_CONFIG_KEY_")
                        || name.as_bytes().starts_with(b"GIT_CONFIG_VALUE_")
                })
                .map(|name| display_os(name))
                .collect();
            if !injected.is_empty() {
                return failed(
                    format!(
                        "Repository-binding Git environment is set: {}",
                        injected.join(", ")
                    ),
                    false,
                );
            }
            let git = match find_executable("git", &self.environment) {
                Ok(p) => p,
                Err(e) => return failed(e, false),
            };
            if self.names.is_none() {
                let output = match self.command(
                    &git,
                    &[OsStr::new("rev-parse"), OsStr::new("--local-env-vars")],
                    request,
                    Duration::from_secs(3),
                    false,
                ) {
                    Ok(o) => o,
                    Err(e) => return e,
                };
                if !output.status.success() || output.truncated {
                    return failed("Cannot inspect Git environment variable names", false);
                }
                let text = match std::str::from_utf8(&output.stdout) {
                    Ok(s) => s,
                    Err(_) => {
                        return failed("Git returned invalid environment variable names", false);
                    }
                };
                let names: Vec<_> = text.lines().map(str::to_owned).collect();
                if names.is_empty()
                    || names.iter().any(|s| {
                        s.is_empty() || !s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
                    })
                {
                    return failed("Git returned invalid environment variable names", false);
                }
                self.names = Some(names);
            }
            let rejected: Vec<_> = self
                .environment
                .keys()
                .filter(|name| {
                    self.names
                        .as_ref()
                        .unwrap()
                        .iter()
                        .any(|n| name.as_os_str() == OsStr::new(n))
                        || name.as_bytes().starts_with(b"GIT_CONFIG_KEY_")
                        || name.as_bytes().starts_with(b"GIT_CONFIG_VALUE_")
                })
                .map(|name| display_os(name))
                .collect();
            if !rejected.is_empty() {
                return failed(
                    format!(
                        "Repository-binding Git environment is set: {}",
                        rejected.join(", ")
                    ),
                    false,
                );
            }
            let output = match self.command(
                &git,
                &[OsStr::new("rev-parse"), OsStr::new("--show-toplevel")],
                request,
                Duration::from_secs(3),
                false,
            ) {
                Ok(o) => o,
                Err(e) => return e,
            };
            if output.status.success() {
                if output.truncated {
                    return failed("Git root output was truncated", false);
                }
                let bytes = output.stdout.strip_suffix(b"\n").unwrap_or(&output.stdout);
                use std::os::unix::ffi::OsStringExt;
                let root = PathBuf::from(OsString::from_vec(bytes.to_vec()));
                if !root.is_absolute() {
                    return failed("Git returned an invalid root", false);
                }
                if request.kind == ActionKind::ListBranches {
                    return match self.branch_entries(&git, request) {
                        Ok(branches) => ActionOutcome::BranchesLoaded { branches },
                        Err(outcome) => outcome,
                    };
                }
                if request.kind == ActionKind::OpenBranchWorkspace {
                    let Some(branch) = request.branch.as_ref() else {
                        return failed("A branch is required", false);
                    };
                    if branch.as_bytes().contains(&0) {
                        return failed("Branch name contains a NUL byte", false);
                    }
                    let branches = match self.branch_entries(&git, request) {
                        Ok(branches) => branches,
                        Err(outcome) => return outcome,
                    };
                    let Some(entry) = branches.into_iter().find(|entry| entry.name == *branch)
                    else {
                        return failed(
                            format!(
                                "Branch {} no longer exists; reopen the branch picker",
                                display_os(branch.as_os_str())
                            ),
                            false,
                        );
                    };
                    if entry.stale_worktree {
                        return failed(
                            format!(
                                "Branch {} has stale worktree metadata; run git worktree prune",
                                display_os(branch.as_os_str())
                            ),
                            false,
                        );
                    }
                    prepared.branch = Some(branch.clone());
                    prepared.branch_mode = Some(if entry.worktree.is_some() {
                        BranchWorkspaceMode::Open
                    } else {
                        BranchWorkspaceMode::New
                    });
                }
                if request.kind == ActionKind::GitInit {
                    already_done = root == request.path;
                    if !already_done {
                        confirmation = Some(format!(
                            "Create a nested Git repository in {}? Outer repository: {}",
                            display_path(&request.path),
                            display_path(&root)
                        ));
                    }
                }
            } else {
                let diagnostic = String::from_utf8_lossy(&output.stderr);
                if output.status.code() != Some(128)
                    || !diagnostic.starts_with("fatal: not a git repository")
                {
                    return failed(
                        format!("Cannot inspect Git repository: {}", output.diagnostic()),
                        false,
                    );
                }
                if matches!(
                    request.kind,
                    ActionKind::OpenHd | ActionKind::ListBranches | ActionKind::OpenBranchWorkspace
                ) {
                    return failed(
                        if request.kind == ActionKind::OpenHd {
                            "hd requires a Git repository; use Ctrl-P or Ctrl-G"
                        } else {
                            "Branch workspace requires a Git repository"
                        },
                        false,
                    );
                }
                confirmation = Some(format!(
                    "Initialize a Git repository in {}?",
                    display_path(&request.path)
                ));
            }
            prepared.git = Some(git);
            if matches!(
                request.kind,
                ActionKind::OpenHd | ActionKind::OpenBranchWorkspace
            ) {
                prepared.tool = match find_executable("hd", &self.environment) {
                    Ok(p) => Some(p),
                    Err(e) => return failed(e, false),
                };
            }
        } else if request.kind == ActionKind::OpenPane {
            prepared.tool = match find_executable("herdr", &self.environment) {
                Ok(p) => Some(p),
                Err(e) => return failed(e, false),
            };
        }
        self.prepared = Some(prepared);
        ActionOutcome::Checked {
            confirmation,
            already_done,
        }
    }

    fn branch_entries(
        &self,
        git: &Path,
        request: &Request,
    ) -> Result<Vec<BranchEntry>, ActionOutcome> {
        let refs = self.command(
            git,
            &[
                OsStr::new("for-each-ref"),
                OsStr::new("--format=%(refname)%00"),
                OsStr::new("refs/heads"),
            ],
            request,
            Duration::from_secs(3),
            false,
        )?;
        if !refs.status.success() || refs.truncated {
            return Err(failed(
                format!("Cannot list Git branches: {}", refs.diagnostic()),
                false,
            ));
        }
        let names: Vec<OsString> = refs
            .stdout
            .split(|byte| *byte == 0)
            .map(|name| name.strip_prefix(b"\n").unwrap_or(name))
            .filter_map(|name| name.strip_prefix(b"refs/heads/"))
            .filter(|name| !name.is_empty())
            .map(|name| OsString::from_vec(name.to_vec()))
            .collect();
        let worktrees = self.command(
            git,
            &[
                OsStr::new("worktree"),
                OsStr::new("list"),
                OsStr::new("--porcelain"),
                OsStr::new("-z"),
            ],
            request,
            Duration::from_secs(3),
            false,
        )?;
        if !worktrees.status.success() || worktrees.truncated {
            return Err(failed(
                format!("Cannot list Git worktrees: {}", worktrees.diagnostic()),
                false,
            ));
        }
        let locations = parse_worktrees(&worktrees.stdout);
        let mut branches: Vec<_> = names
            .into_iter()
            .map(|name| {
                let stale_worktree = locations.get(&name).is_some_and(|info| info.stale);
                BranchEntry {
                    worktree: locations
                        .get(&name)
                        .filter(|info| !info.stale)
                        .map(|info| info.path.clone()),
                    stale_worktree,
                    name,
                }
            })
            .collect();
        branches.sort_by(|left, right| left.name.cmp(&right.name));
        Ok(branches)
    }

    fn run(&mut self, request: &Request) -> ActionOutcome {
        if request.kind == ActionKind::Mkdir {
            let Some(name) = &request.child_name else {
                return failed("Directory name is required", false);
            };
            if name.is_empty() || name == "." || name == ".." || name.contains(['/', '\0']) {
                return failed("Directory name must be one nonempty path component", false);
            }
            if request.cancel.is_cancelled() || self.shutdown.is_cancelled() {
                return ActionOutcome::Cancelled { uncertain: false };
            }
            let path = request.path.join(name);
            return match fs::create_dir(&path) {
                Ok(()) => ActionOutcome::Completed {
                    message: format!("Created {}", display_path(&path)),
                    created: Some(path),
                },
                Err(e) => failed(format!("Cannot create directory: {e}"), false),
            };
        }
        let Some(prepared) = self.prepared.take().filter(|p| {
            p.id == request.id
                && p.kind == request.kind
                && p.path == request.path
                && p.branch == request.branch
        }) else {
            return failed("Action has no matching completed precheck", false);
        };
        let (executable, args, duration): (&Path, Vec<&OsStr>, Duration) = match request.kind {
            ActionKind::GitInit => (
                prepared.git.as_ref().unwrap(),
                vec![OsStr::new("init")],
                Duration::from_secs(10),
            ),
            ActionKind::OpenHd => (
                prepared.tool.as_ref().unwrap(),
                Vec::new(),
                Duration::from_secs(30),
            ),
            ActionKind::OpenBranchWorkspace => {
                let mode = prepared
                    .branch_mode
                    .expect("branch workspace precheck sets a mode");
                let branch = prepared
                    .branch
                    .as_ref()
                    .expect("branch workspace precheck keeps the branch");
                let command = match mode {
                    BranchWorkspaceMode::Open => OsStr::new("open"),
                    BranchWorkspaceMode::New => OsStr::new("new"),
                };
                (
                    prepared.tool.as_ref().unwrap(),
                    vec![command, branch.as_os_str()],
                    Duration::from_secs(30),
                )
            }
            ActionKind::OpenPane => (
                prepared.tool.as_ref().unwrap(),
                vec![
                    OsStr::new("pane"),
                    OsStr::new("split"),
                    OsStr::new("--current"),
                    OsStr::new("--direction"),
                    OsStr::new("right"),
                    OsStr::new("--cwd"),
                    request.path.as_os_str(),
                    OsStr::new("--focus"),
                ],
                Duration::from_secs(10),
            ),
            ActionKind::ChangeDirectory | ActionKind::ListBranches | ActionKind::Mkdir => {
                return failed(
                    "Change-directory results are handled by the shell boundary",
                    false,
                );
            }
        };
        match self.command(executable, &args, request, duration, true) {
            Ok(output) if output.status.success() => ActionOutcome::Completed {
                message: format!("Action completed. {}", output.diagnostic()),
                created: None,
            },
            Ok(output) => failed(
                format!(
                    "Action failed; it may already have taken effect. {}",
                    output.diagnostic()
                ),
                true,
            ),
            Err(outcome) => outcome,
        }
    }
}
fn failed(message: impl Into<String>, uncertain: bool) -> ActionOutcome {
    ActionOutcome::Failed {
        message: message.into(),
        uncertain,
    }
}

struct WorktreeInfo {
    path: PathBuf,
    stale: bool,
}

fn parse_worktrees(output: &[u8]) -> BTreeMap<OsString, WorktreeInfo> {
    let mut locations = BTreeMap::new();
    let mut worktree = None;
    let mut branch = None;
    let mut stale = false;
    let mut finish =
        |worktree: &mut Option<PathBuf>, branch: &mut Option<OsString>, stale: &mut bool| {
            if let (Some(path), Some(name)) = (worktree.take(), branch.take()) {
                locations.insert(
                    name,
                    WorktreeInfo {
                        stale: *stale || !path.is_dir(),
                        path,
                    },
                );
            } else {
                worktree.take();
                branch.take();
            }
            *stale = false;
        };
    for field in output.split(|byte| *byte == 0) {
        if field.is_empty() {
            finish(&mut worktree, &mut branch, &mut stale);
        } else if let Some(path) = field.strip_prefix(b"worktree ") {
            worktree = Some(PathBuf::from(OsString::from_vec(path.to_vec())));
        } else if let Some(name) = field.strip_prefix(b"branch refs/heads/") {
            branch = Some(OsString::from_vec(name.to_vec()));
        } else if field.starts_with(b"prunable") {
            stale = true;
        }
    }
    finish(&mut worktree, &mut branch, &mut stale);
    locations
}

fn find_executable(
    name: &str,
    environment: &BTreeMap<OsString, OsString>,
) -> Result<PathBuf, String> {
    let search = environment
        .get(OsStr::new("PATH"))
        .ok_or_else(|| format!("{name} is unavailable: PATH is unset"))?;
    for directory in std::env::split_paths(search) {
        let path = directory.join(name);
        if fs::metadata(&path).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0) {
            return fs::canonicalize(path).map_err(|e| format!("Cannot resolve {name}: {e}"));
        }
    }
    Err(format!("No external {name} executable on PATH"))
}

struct Output {
    status: ExitStatus,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    truncated: bool,
}
impl Output {
    fn diagnostic(&self) -> String {
        let mut bytes = self.stdout.clone();
        bytes.extend_from_slice(&self.stderr);
        let mut text = display_os(OsStr::from_bytes(&bytes));
        if self.truncated {
            text.push_str(" [output truncated]");
        }
        text
    }
}
struct ProcessContext<'a> {
    cwd: &'a Path,
    environment: &'a BTreeMap<OsString, OsString>,
    cancel: &'a CancelToken,
    shutdown: &'a CancelToken,
}
fn run_process(
    executable: &Path,
    args: &[&OsStr],
    context: ProcessContext<'_>,
    duration: Duration,
    side_effect: bool,
) -> Result<Output, ActionOutcome> {
    let ProcessContext {
        cwd,
        environment,
        cancel,
        shutdown,
    } = context;
    if cancel.is_cancelled() || shutdown.is_cancelled() {
        return Err(ActionOutcome::Cancelled { uncertain: false });
    }
    if duration.is_zero() {
        return Err(failed("Action deadline exceeded before launch", false));
    }
    let mut command = Command::new(executable);
    command
        .args(args)
        .current_dir(cwd)
        .env_clear()
        .envs(environment)
        .env("LC_ALL", "C")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0);
    let mut child = command.spawn().map_err(|e| {
        failed(
            format!("Cannot launch {}: {e}", display_path(executable)),
            false,
        )
    })?;
    let group = child.id() as i32;
    let mut stdout = child.stdout.take().unwrap();
    let mut stderr = child.stderr.take().unwrap();
    for fd in [stdout.as_raw_fd(), stderr.as_raw_fd()] {
        if unsafe { libc::fcntl(fd, libc::F_SETFL, libc::O_NONBLOCK) } < 0 {
            unsafe {
                libc::kill(-group, libc::SIGKILL);
            }
            let _ = child.wait();
            return Err(failed(
                "Cannot configure nonblocking action output",
                side_effect,
            ));
        }
    }
    let deadline = Instant::now() + duration;
    let mut out = Vec::new();
    let mut err = Vec::new();
    let mut truncated = false;
    loop {
        drain(&mut stdout, &mut out, err.len(), &mut truncated);
        drain(&mut stderr, &mut err, out.len(), &mut truncated);
        let cancelled = cancel.is_cancelled() || shutdown.is_cancelled();
        if cancelled || Instant::now() >= deadline {
            terminate_group(group, &mut child, || {
                drain(&mut stdout, &mut out, err.len(), &mut truncated);
                drain(&mut stderr, &mut err, out.len(), &mut truncated);
            });
            return Err(if cancelled {
                ActionOutcome::Cancelled {
                    uncertain: side_effect,
                }
            } else {
                failed("Action deadline exceeded; execution stopped", side_effect)
            });
        }
        match child.try_wait() {
            Ok(Some(status)) => {
                let out_eof = drain(&mut stdout, &mut out, err.len(), &mut truncated);
                let err_eof = drain(&mut stderr, &mut err, out.len(), &mut truncated);
                if !(out_eof && err_eof) {
                    // A child may have exited while descendants retain its pipe ends.
                    // Reap the leader and bound cleanup of its own process group.
                    terminate_group(group, &mut child, || {
                        drain(&mut stdout, &mut out, err.len(), &mut truncated);
                        drain(&mut stderr, &mut err, out.len(), &mut truncated);
                    });
                }
                drain(&mut stdout, &mut out, err.len(), &mut truncated);
                drain(&mut stderr, &mut err, out.len(), &mut truncated);
                if cancel.is_cancelled() || shutdown.is_cancelled() {
                    return Err(ActionOutcome::Cancelled {
                        uncertain: side_effect,
                    });
                }
                return Ok(Output {
                    status,
                    stdout: out,
                    stderr: err,
                    truncated,
                });
            }
            Ok(None) => thread::sleep(Duration::from_millis(5)),
            Err(e) => {
                unsafe {
                    libc::kill(-group, libc::SIGKILL);
                }
                let _ = child.wait();
                return Err(failed(format!("Cannot wait for action: {e}"), side_effect));
            }
        }
    }
}
/// Only called with the group created for this owned child by `process_group(0)`.
/// Both a running leader and an exited leader with inherited pipes use one cleanup path.
fn terminate_group(group: i32, child: &mut std::process::Child, mut drain_output: impl FnMut()) {
    unsafe {
        libc::kill(-group, libc::SIGTERM);
    }
    let grace = Instant::now() + Duration::from_millis(250);
    while Instant::now() < grace {
        drain_output();
        let _ = child.try_wait();
        std::thread::sleep(Duration::from_millis(5));
    }
    unsafe {
        libc::kill(-group, libc::SIGKILL);
    }
    let _ = child.wait();
}
fn drain(
    reader: &mut impl Read,
    buffer: &mut Vec<u8>,
    other_length: usize,
    truncated: &mut bool,
) -> bool {
    let mut chunk = [0u8; 8192];
    // Bound each poll even when a child produces output faster than it can be read.
    for _ in 0..8 {
        match reader.read(&mut chunk) {
            Ok(0) => return true,
            Ok(n) => {
                let keep = n.min(65536usize.saturating_sub(buffer.len() + other_length));
                buffer.extend_from_slice(&chunk[..keep]);
                *truncated |= keep < n;
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => break,
        }
    }
    false
}

pub fn write_cd_result(result_file: &Path, target: &Path) -> Result<(), String> {
    if !target.is_absolute() || target.as_os_str().as_bytes().contains(&0) {
        return Err("Shell target must be an absolute native path".into());
    }
    validate_directory(target)?;
    let mut file = OpenOptions::new()
        .write(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(result_file)
        .map_err(|e| format!("Cannot open shell result: {e}"))?;
    let metadata = file
        .metadata()
        .map_err(|e| format!("Cannot inspect shell result: {e}"))?;
    if !metadata.is_file()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.mode() & 0o7777 != 0o600
        || metadata.nlink() != 1
    {
        return Err("Shell result must be an owned, private 0600 regular file".into());
    }
    file.set_len(0)
        .and_then(|_| file.write_all(b"qnav-v1\0cd\0"))
        .and_then(|_| file.write_all(target.as_os_str().as_bytes()))
        .and_then(|_| file.write_all(&[0]))
        .and_then(|_| file.flush())
        .map_err(|e| format!("Cannot write shell result: {e}"))
}
