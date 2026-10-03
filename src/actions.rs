//! A single asynchronous action worker; subprocesses never write to the terminal.
use crate::{
    model::{
        ActionKind, BranchCreate, BranchCreateMode, BranchEntry, CancelToken, WorkId, display_os,
        display_path,
    },
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
    BranchCreated {
        branch: OsString,
        message: String,
        complete: bool,
        workspace_uncertain: bool,
    },
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
    branch_create: Option<BranchCreate>,
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
            branch_create: None,
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
            branch_create: None,
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
            branch_create: None,
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
            branch_create: None,
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
            branch_create: None,
            cancel,
            check: false,
            deadline: Instant::now() + Duration::from_secs(30),
        });
    }
    pub fn start_branch_cleanup_check(
        &self,
        id: WorkId,
        kind: ActionKind,
        path: PathBuf,
        branch: OsString,
        cancel: CancelToken,
    ) {
        self.start_branch_cleanup(id, kind, path, branch, cancel, true);
    }
    pub fn start_branch_cleanup_run(
        &self,
        id: WorkId,
        kind: ActionKind,
        path: PathBuf,
        branch: OsString,
        cancel: CancelToken,
    ) {
        self.start_branch_cleanup(id, kind, path, branch, cancel, false);
    }
    fn start_branch_cleanup(
        &self,
        id: WorkId,
        kind: ActionKind,
        path: PathBuf,
        branch: OsString,
        cancel: CancelToken,
        check: bool,
    ) {
        self.submit(Request {
            id,
            kind,
            path,
            child_name: None,
            branch: Some(branch),
            branch_create: None,
            cancel,
            check,
            deadline: Instant::now() + Duration::from_secs(if check { 3 } else { 30 }),
        });
    }
    pub fn start_branch_create_check(
        &self,
        id: WorkId,
        path: PathBuf,
        create: BranchCreate,
        cancel: CancelToken,
    ) {
        self.start_branch_create(id, path, create, cancel, true);
    }
    pub fn start_branch_create_run(
        &self,
        id: WorkId,
        path: PathBuf,
        create: BranchCreate,
        cancel: CancelToken,
    ) {
        self.start_branch_create(id, path, create, cancel, false);
    }
    fn start_branch_create(
        &self,
        id: WorkId,
        path: PathBuf,
        create: BranchCreate,
        cancel: CancelToken,
        check: bool,
    ) {
        self.submit(Request {
            id,
            kind: ActionKind::CreateBranch,
            path,
            child_name: None,
            branch: None,
            branch_create: Some(create),
            cancel,
            check,
            deadline: Instant::now() + Duration::from_secs(if check { 3 } else { 30 }),
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
    branch_create: Option<BranchCreate>,
    base_oid: Option<OsString>,
    cleanup: Option<CleanupSnapshot>,
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
        if (matches!(
            request.kind,
            ActionKind::OpenHd
                | ActionKind::OpenPane
                | ActionKind::OpenBranchWorkspace
                | ActionKind::RemoveBranchWorkspace
        ) || request
            .branch_create
            .as_ref()
            .is_some_and(|create| create.mode == BranchCreateMode::Workspace))
            && request
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
            branch_create: request.branch_create.clone(),
            base_oid: None,
            cleanup: None,
        };
        let mut confirmation = None;
        let mut already_done = false;
        if matches!(
            request.kind,
            ActionKind::GitInit
                | ActionKind::OpenHd
                | ActionKind::ListBranches
                | ActionKind::OpenBranchWorkspace
                | ActionKind::CreateBranch
                | ActionKind::DeleteBranch
                | ActionKind::RemoveBranchWorkspace
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
                if request.kind == ActionKind::CreateBranch {
                    prepared.base_oid = match self.check_branch_create(&git, request) {
                        Ok(oid) => Some(oid),
                        Err(outcome) => return outcome,
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
                if request.kind.is_branch_cleanup() {
                    let snapshot = match self.inspect_cleanup(&git, request) {
                        Ok(snapshot) => snapshot,
                        Err(outcome) => return outcome,
                    };
                    let branch = request.branch.as_ref().unwrap();
                    confirmation = Some(if let Some(workspace) = &snapshot.workspace {
                        format!(
                            "Remove workspace {} for branch {} via hd? The branch will be retained.",
                            display_path(&workspace.root.path),
                            display_os(branch)
                        )
                    } else {
                        let (oid, name) = snapshot.baseline.as_ref().unwrap();
                        format!(
                            "Delete local branch {} merged into {} ({})?",
                            display_os(branch),
                            name.as_ref()
                                .map(|name| display_os(name))
                                .unwrap_or_else(|| "detached HEAD".into()),
                            display_os(oid)
                        )
                    });
                    prepared.branch = Some(branch.clone());
                    prepared.cleanup = Some(snapshot);
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
                    ActionKind::OpenHd
                        | ActionKind::ListBranches
                        | ActionKind::OpenBranchWorkspace
                        | ActionKind::CreateBranch
                        | ActionKind::DeleteBranch
                        | ActionKind::RemoveBranchWorkspace
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
                ActionKind::OpenHd
                    | ActionKind::OpenBranchWorkspace
                    | ActionKind::RemoveBranchWorkspace
            ) || request
                .branch_create
                .as_ref()
                .is_some_and(|create| create.mode == BranchCreateMode::Workspace)
            {
                prepared.tool = match find_executable("hd", &self.environment) {
                    Ok(p) => Some(p),
                    Err(e) if request.kind == ActionKind::RemoveBranchWorkspace => {
                        return failed(
                            format!("{e}; install hd or add it to PATH to remove workspaces"),
                            false,
                        );
                    }
                    Err(e) => return failed(e, false),
                };
                if request.kind == ActionKind::CreateBranch {
                    let help = match self.command(
                        prepared.tool.as_ref().unwrap(),
                        &[OsStr::new("--help")],
                        request,
                        Duration::from_secs(3),
                        false,
                    ) {
                        Ok(output) => output,
                        Err(outcome) => return outcome,
                    };
                    if !help.status.success()
                        || help.truncated
                        || !String::from_utf8_lossy(&help.stdout).contains("--base")
                    {
                        return failed(
                            "hd must support new --base; update hd or choose Create branch only",
                            false,
                        );
                    }
                }
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

    fn cleanup_output(
        &self,
        git: &Path,
        cwd: &Path,
        args: &[&OsStr],
        request: &Request,
    ) -> Result<Output, ActionOutcome> {
        let duration = if request.check {
            Duration::from_secs(3).min(request.deadline.saturating_duration_since(Instant::now()))
        } else {
            Duration::from_secs(3)
        };
        run_process(
            git,
            args,
            ProcessContext {
                cwd,
                environment: &self.environment,
                cancel: &request.cancel,
                shutdown: &self.shutdown,
            },
            duration,
            false,
        )
    }
    fn cleanup_value(
        &self,
        git: &Path,
        cwd: &Path,
        args: &[&OsStr],
        request: &Request,
    ) -> Result<OsString, ActionOutcome> {
        let output = self.cleanup_output(git, cwd, args, request)?;
        if !output.status.success() || output.truncated {
            return Err(failed(
                format!("Cannot inspect cleanup target: {}", output.diagnostic()),
                false,
            ));
        }
        let bytes = output.stdout.strip_suffix(b"\n").unwrap_or(&output.stdout);
        if bytes.is_empty() || bytes.contains(&0) {
            return Err(failed("Git returned an invalid cleanup identity", false));
        }
        Ok(OsString::from_vec(bytes.to_vec()))
    }
    fn repository_identity(
        &self,
        git: &Path,
        cwd: &Path,
        request: &Request,
    ) -> Result<RepositoryIdentity, ActionOutcome> {
        let mut paths = Vec::new();
        for option in ["--show-toplevel", "--git-common-dir", "--git-dir"] {
            let path = self.cleanup_value(
                git,
                cwd,
                &[
                    OsStr::new("rev-parse"),
                    OsStr::new("--path-format=absolute"),
                    OsStr::new(option),
                ],
                request,
            )?;
            let path = PathBuf::from(path);
            if !path.is_absolute() {
                return Err(failed("Git returned a relative cleanup identity", false));
            }
            paths.push(DirectoryIdentity::inspect(&path)?);
        }
        let mut paths = paths.into_iter();
        Ok(RepositoryIdentity {
            root: paths.next().unwrap(),
            common: paths.next().unwrap(),
            gitdir: paths.next().unwrap(),
        })
    }
    fn symbolic_ref(
        &self,
        git: &Path,
        cwd: &Path,
        name: &OsStr,
        request: &Request,
    ) -> Result<Option<OsString>, ActionOutcome> {
        let output = self.cleanup_output(
            git,
            cwd,
            &[OsStr::new("symbolic-ref"), OsStr::new("--quiet"), name],
            request,
        )?;
        if output.status.success() && !output.truncated {
            Ok(Some(OsString::from_vec(
                output
                    .stdout
                    .strip_suffix(b"\n")
                    .unwrap_or(&output.stdout)
                    .to_vec(),
            )))
        } else if output.status.code() == Some(1) && !output.truncated {
            Ok(None)
        } else {
            Err(failed("Cannot inspect symbolic cleanup ref", false))
        }
    }
    fn cleanup_worktrees(
        &self,
        git: &Path,
        request: &Request,
    ) -> Result<Vec<WorktreeInfo>, ActionOutcome> {
        let output = self.cleanup_output(
            git,
            &request.path,
            &[
                OsStr::new("worktree"),
                OsStr::new("list"),
                OsStr::new("--porcelain"),
                OsStr::new("-z"),
            ],
            request,
        )?;
        if !output.status.success() || output.truncated {
            return Err(failed("Cannot inspect cleanup worktrees", false));
        }
        let records = worktree_records(&output.stdout);
        if records.is_empty() {
            return Err(failed("Git returned no worktree records", false));
        }
        Ok(records)
    }
    fn inspect_cleanup(
        &self,
        git: &Path,
        request: &Request,
    ) -> Result<CleanupSnapshot, ActionOutcome> {
        let branch = request
            .branch
            .as_ref()
            .ok_or_else(|| failed("A branch is required", false))?;
        if branch.as_bytes().contains(&0) {
            return Err(failed("Branch name contains a NUL byte", false));
        }
        let mut full_ref = OsString::from("refs/heads/");
        full_ref.push(branch);
        let valid = self.cleanup_output(
            git,
            &request.path,
            &[OsStr::new("check-ref-format"), &full_ref],
            request,
        )?;
        if !valid.status.success() {
            return Err(failed("Invalid local branch name", false));
        }
        // Both commands accept a short branch argument; never let it become an option.
        let valid_short = self.cleanup_output(
            git,
            &request.path,
            &[
                OsStr::new("check-ref-format"),
                OsStr::new("--branch"),
                branch,
            ],
            request,
        )?;
        if !valid_short.status.success() {
            return Err(failed("Invalid local branch name", false));
        }
        if self
            .symbolic_ref(git, &request.path, &full_ref, request)?
            .is_some()
        {
            return Err(failed(
                "Symbolic local branches cannot be cleaned up",
                false,
            ));
        }
        let oid = self.cleanup_value(
            git,
            &request.path,
            &[
                OsStr::new("show-ref"),
                OsStr::new("--verify"),
                OsStr::new("--hash"),
                &full_ref,
            ],
            request,
        )?;
        if !matches!(oid.as_bytes().len(), 40 | 64)
            || !oid.as_bytes().iter().all(u8::is_ascii_hexdigit)
        {
            return Err(failed("Git returned an invalid branch commit", false));
        }
        let repository = self.repository_identity(git, &request.path, request)?;
        let worktrees = self.cleanup_worktrees(git, request)?;
        let matching: Vec<_> = worktrees
            .iter()
            .enumerate()
            .filter(|(_, info)| info.branch.as_ref() == Some(branch))
            .collect();
        let (baseline, workspace) = if request.kind == ActionKind::DeleteBranch {
            if !matching.is_empty() {
                return Err(failed(
                    "Branch is checked out or has stale worktree metadata; remove its workspace first",
                    false,
                ));
            }
            let head = self.cleanup_value(
                git,
                &request.path,
                &[
                    OsStr::new("rev-parse"),
                    OsStr::new("--verify"),
                    OsStr::new("HEAD^{commit}"),
                ],
                request,
            )?;
            let name = self.symbolic_ref(git, &request.path, OsStr::new("HEAD"), request)?;
            let merged = self.cleanup_output(
                git,
                &request.path,
                &[
                    OsStr::new("merge-base"),
                    OsStr::new("--is-ancestor"),
                    &oid,
                    &head,
                ],
                request,
            )?;
            if !merged.status.success() {
                return Err(failed(
                    "Branch is not merged into the selected checkout HEAD",
                    false,
                ));
            }
            (Some((head, name)), None)
        } else if request.kind == ActionKind::RemoveBranchWorkspace {
            if matching.len() != 1 {
                return Err(failed(
                    "Branch must have exactly one registered workspace (missing or ambiguous)",
                    false,
                ));
            }
            let (index, info) = matching[0];
            if index == 0 {
                return Err(failed("Cannot remove the primary worktree", false));
            }
            if info.stale || info.locked {
                return Err(failed(
                    "Workspace is stale, missing, or locked; inspect git worktree list",
                    false,
                ));
            }
            let workspace = self.repository_identity(git, &info.path, request)?;
            if workspace.root.path
                != fs::canonicalize(&info.path).map_err(|e| failed(e.to_string(), false))?
                || workspace.common != repository.common
            {
                return Err(failed(
                    "Workspace repository identity does not match the selected repository",
                    false,
                ));
            }
            if workspace.root.path == repository.root.path {
                return Err(failed("Cannot remove the selected target checkout", false));
            }
            let cwd = std::env::current_dir()
                .and_then(fs::canonicalize)
                .map_err(|e| {
                    failed(format!("Cannot inspect qnav current directory: {e}"), false)
                })?;
            if cwd.starts_with(&workspace.root.path) {
                return Err(failed(
                    "Cannot remove a workspace containing qnav's current directory",
                    false,
                ));
            }
            if workspace
                .root
                .path
                .to_str()
                .is_none_or(|path| path.contains(['\r', '\n']))
            {
                return Err(failed(
                    "hd/Herdr require a UTF-8 workspace path without CR or LF",
                    false,
                ));
            }
            if self
                .symbolic_ref(git, &info.path, OsStr::new("HEAD"), request)?
                .as_ref()
                != Some(&full_ref)
            {
                return Err(failed(
                    "Workspace HEAD no longer matches the selected branch",
                    false,
                ));
            }
            // Status can hide modified tracked files when these index flags are set.
            // Recurse into initialized submodules so their tracked contents are protected too.
            let index = self.cleanup_output(
                git,
                &info.path,
                &[
                    OsStr::new("ls-files"),
                    OsStr::new("--cached"),
                    OsStr::new("-v"),
                    OsStr::new("-z"),
                    OsStr::new("--recurse-submodules"),
                ],
                request,
            )?;
            if !index.status.success()
                || index.truncated
                || (!index.stdout.is_empty() && !index.stdout.ends_with(&[0]))
            {
                return Err(failed(
                    "Cannot inspect workspace index flags; preserve changes before removal",
                    false,
                ));
            }
            for record in index
                .stdout
                .split(|byte| *byte == 0)
                .filter(|record| !record.is_empty())
            {
                if record.len() < 3 || record[1] != b' ' {
                    return Err(failed(
                        "Cannot inspect workspace index flags; preserve changes before removal",
                        false,
                    ));
                }
                if record[0].is_ascii_lowercase() || record[0] == b'S' {
                    return Err(failed(
                        "Workspace has assume-unchanged or skip-worktree index flags; restore these flags and preserve changes before removal",
                        false,
                    ));
                }
            }
            let status = self.cleanup_output(
                git,
                &info.path,
                &[
                    OsStr::new("status"),
                    OsStr::new("--porcelain"),
                    OsStr::new("-z"),
                    OsStr::new("--untracked-files=all"),
                    OsStr::new("--ignored"),
                    OsStr::new("--ignore-submodules=none"),
                ],
                request,
            )?;
            if !status.status.success() || status.truncated || !status.stdout.is_empty() {
                return Err(failed(
                    "Workspace has tracked, untracked, ignored, or submodule changes; preserve them before removal",
                    false,
                ));
            }
            (None, Some(workspace))
        } else {
            return Err(failed("Invalid branch cleanup action", false));
        };
        Ok(CleanupSnapshot {
            repository,
            full_ref,
            oid,
            baseline,
            workspace,
        })
    }
    fn run_branch_cleanup(&self, prepared: &Prepared, request: &Request) -> ActionOutcome {
        let git = prepared.git.as_ref().unwrap();
        let expected = prepared.cleanup.as_ref().unwrap();
        match self.inspect_cleanup(git, request) {
            Ok(current) if current == *expected => {}
            Ok(_) => {
                return failed(
                    "Cleanup target changed since confirmation; reopen the branch picker",
                    false,
                );
            }
            Err(outcome) => return outcome,
        }
        let branch = request.branch.as_ref().unwrap();
        let (executable, args) = if request.kind == ActionKind::DeleteBranch {
            (
                git,
                vec![
                    OsStr::new("branch"),
                    OsStr::new("-d"),
                    OsStr::new("--"),
                    branch.as_os_str(),
                ],
            )
        } else {
            (
                prepared.tool.as_ref().unwrap(),
                vec![OsStr::new("rm"), branch.as_os_str()],
            )
        };
        let output = match self.command(executable, &args, request, Duration::from_secs(30), true) {
            Ok(output) => output,
            Err(outcome) => return outcome,
        };
        if !output.status.success() {
            return failed(
                format!(
                    "Cleanup failed and may have taken effect: {}",
                    output.diagnostic()
                ),
                true,
            );
        }
        if let Some(workspace) = &expected.workspace {
            let records = match self.cleanup_worktrees(git, request) {
                Ok(records) => records,
                Err(ActionOutcome::Cancelled { .. }) => {
                    return ActionOutcome::Cancelled { uncertain: true };
                }
                Err(_) => {
                    return failed(
                        "Workspace command succeeded but removal could not be verified",
                        true,
                    );
                }
            };
            let path_absent = matches!(fs::symlink_metadata(&workspace.root.path),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound);
            if !path_absent
                || records.iter().any(|info| {
                    info.branch.as_ref() == Some(branch)
                        || fs::canonicalize(&info.path).unwrap_or_else(|_| info.path.clone())
                            == workspace.root.path
                })
            {
                return failed(
                    "Workspace command succeeded but the workspace still exists or remains registered",
                    true,
                );
            }
            let retained = self.cleanup_value(
                git,
                &request.path,
                &[
                    OsStr::new("show-ref"),
                    OsStr::new("--verify"),
                    OsStr::new("--hash"),
                    &expected.full_ref,
                ],
                request,
            );
            match retained {
                Ok(oid) if oid == expected.oid => {}
                Err(ActionOutcome::Cancelled { .. }) => {
                    return ActionOutcome::Cancelled { uncertain: true };
                }
                _ => {
                    return failed(
                        "Workspace was removed but branch retention could not be verified",
                        true,
                    );
                }
            }
        }
        ActionOutcome::Completed {
            message: format!(
                "{} {}",
                if request.kind == ActionKind::DeleteBranch {
                    "Deleted branch"
                } else {
                    "Removed workspace for branch"
                },
                display_os(branch)
            ),
            created: None,
        }
    }

    fn check_branch_create(
        &self,
        git: &Path,
        request: &Request,
    ) -> Result<OsString, ActionOutcome> {
        let create = request
            .branch_create
            .as_ref()
            .ok_or_else(|| failed("Branch creation details are required", false))?;
        if create.destination.contains('\0') || create.base.as_bytes().contains(&0) {
            return Err(failed("Branch name contains a NUL byte", false));
        }
        let destination_ref = format!("refs/heads/{}", create.destination);
        for args in [
            vec![OsStr::new("check-ref-format"), OsStr::new(&destination_ref)],
            vec![
                OsStr::new("check-ref-format"),
                OsStr::new("--branch"),
                OsStr::new(&create.destination),
            ],
        ] {
            let output = self.command(git, &args, request, Duration::from_secs(3), false)?;
            if !output.status.success() {
                return Err(failed("Invalid destination branch name", false));
            }
        }
        let existing = self.command(
            git,
            &[
                OsStr::new("show-ref"),
                OsStr::new("--verify"),
                OsStr::new("--quiet"),
                OsStr::new(&destination_ref),
            ],
            request,
            Duration::from_secs(3),
            false,
        )?;
        if existing.status.success() {
            return Err(failed("Destination branch already exists", false));
        }
        if existing.status.code() != Some(1) {
            return Err(failed(
                format!(
                    "Cannot inspect destination branch: {}",
                    existing.diagnostic()
                ),
                false,
            ));
        }
        let mut base_ref = OsString::from("refs/heads/");
        base_ref.push(&create.base);
        let valid_base = self.command(
            git,
            &[OsStr::new("check-ref-format"), &base_ref],
            request,
            Duration::from_secs(3),
            false,
        )?;
        if !valid_base.status.success() {
            return Err(failed("Invalid base branch name", false));
        }
        base_ref.push("^{commit}");
        let base = self.command(
            git,
            &[
                OsStr::new("rev-parse"),
                OsStr::new("--verify"),
                OsStr::new("--end-of-options"),
                &base_ref,
            ],
            request,
            Duration::from_secs(3),
            false,
        )?;
        if !base.status.success() || base.truncated {
            return Err(failed(
                "Base branch no longer exists or does not point to a commit",
                false,
            ));
        }
        let oid = base.stdout.strip_suffix(b"\n").unwrap_or(&base.stdout);
        if !matches!(oid.len(), 40 | 64) || !oid.iter().all(u8::is_ascii_hexdigit) {
            return Err(failed("Git returned an invalid base commit", false));
        }
        Ok(OsString::from_vec(oid.to_vec()))
    }

    fn run_branch_create(&self, prepared: &Prepared, request: &Request) -> ActionOutcome {
        let create = prepared
            .branch_create
            .as_ref()
            .expect("creation precheck keeps request");
        let oid = prepared
            .base_oid
            .as_ref()
            .expect("creation precheck pins base");
        let output = match self.command(
            prepared.git.as_ref().unwrap(),
            &[
                OsStr::new("branch"),
                OsStr::new("--no-track"),
                OsStr::new("--"),
                OsStr::new(&create.destination),
                oid,
            ],
            request,
            Duration::from_secs(10),
            true,
        ) {
            Ok(output) => output,
            Err(outcome) => return outcome,
        };
        if !output.status.success() {
            return failed(
                format!("Branch creation failed: {}", output.diagnostic()),
                true,
            );
        }
        let prefix = format!("Branch {} created", create.destination);
        let (message, complete, workspace_uncertain) =
            if create.mode == BranchCreateMode::BranchOnly {
                (prefix, true, false)
            } else {
                match self.command(
                    prepared.tool.as_ref().unwrap(),
                    &[
                        OsStr::new("new"),
                        OsStr::new(&create.destination),
                        OsStr::new("--base"),
                        oid,
                    ],
                    request,
                    Duration::from_secs(30),
                    true,
                ) {
                    Ok(output) if output.status.success() => (
                        format!("{prefix}; workspace opened. {}", output.diagnostic()),
                        true,
                        false,
                    ),
                    Ok(output) => (
                        format!(
                            "{prefix}; workspace command failed and may have taken effect. {}",
                            output.diagnostic()
                        ),
                        false,
                        true,
                    ),
                    Err(ActionOutcome::Cancelled { uncertain }) => (
                        format!(
                            "{prefix}; workspace creation cancelled{}",
                            if uncertain {
                                " and may have taken effect"
                            } else {
                                " before starting"
                            }
                        ),
                        false,
                        uncertain,
                    ),
                    Err(ActionOutcome::Failed { message, uncertain }) => (
                        format!("{prefix}; workspace creation failed: {message}"),
                        false,
                        uncertain,
                    ),
                    Err(_) => (
                        format!("{prefix}; workspace result is unknown"),
                        false,
                        true,
                    ),
                }
            };
        ActionOutcome::BranchCreated {
            branch: OsString::from(&create.destination),
            message,
            complete,
            workspace_uncertain,
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
                && p.branch_create == request.branch_create
        }) else {
            return failed("Action has no matching completed precheck", false);
        };
        if request.kind.is_branch_cleanup() {
            return self.run_branch_cleanup(&prepared, request);
        }
        if request.kind == ActionKind::CreateBranch {
            return self.run_branch_create(&prepared, request);
        }
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
            ActionKind::ChangeDirectory
            | ActionKind::ListBranches
            | ActionKind::Mkdir
            | ActionKind::CreateBranch
            | ActionKind::DeleteBranch
            | ActionKind::RemoveBranchWorkspace => {
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

#[derive(Debug, PartialEq, Eq)]
struct DirectoryIdentity {
    path: PathBuf,
    device: u64,
    inode: u64,
}
impl DirectoryIdentity {
    fn inspect(path: &Path) -> Result<Self, ActionOutcome> {
        let path = fs::canonicalize(path)
            .map_err(|e| failed(format!("Cannot resolve cleanup directory: {e}"), false))?;
        let metadata = fs::metadata(&path)
            .map_err(|e| failed(format!("Cannot inspect cleanup directory: {e}"), false))?;
        if !metadata.is_dir() {
            return Err(failed("Cleanup target is not a directory", false));
        }
        Ok(Self {
            path,
            device: metadata.dev(),
            inode: metadata.ino(),
        })
    }
}
#[derive(Debug, PartialEq, Eq)]
struct RepositoryIdentity {
    root: DirectoryIdentity,
    common: DirectoryIdentity,
    gitdir: DirectoryIdentity,
}
#[derive(Debug, PartialEq, Eq)]
struct CleanupSnapshot {
    repository: RepositoryIdentity,
    full_ref: OsString,
    oid: OsString,
    baseline: Option<(OsString, Option<OsString>)>,
    workspace: Option<RepositoryIdentity>,
}
struct WorktreeInfo {
    path: PathBuf,
    branch: Option<OsString>,
    stale: bool,
    locked: bool,
}

fn worktree_records(output: &[u8]) -> Vec<WorktreeInfo> {
    let mut records = Vec::new();
    let mut current: Option<WorktreeInfo> = None;
    for field in output.split(|byte| *byte == 0) {
        if field.is_empty() {
            if let Some(mut info) = current.take() {
                info.stale |= !info.path.is_dir();
                records.push(info);
            }
        } else if let Some(path) = field.strip_prefix(b"worktree ") {
            current = Some(WorktreeInfo {
                path: PathBuf::from(OsString::from_vec(path.to_vec())),
                branch: None,
                stale: false,
                locked: false,
            });
        } else if let Some(info) = current.as_mut() {
            if let Some(name) = field.strip_prefix(b"branch refs/heads/") {
                info.branch = Some(OsString::from_vec(name.to_vec()));
            } else if field.starts_with(b"prunable") {
                info.stale = true;
            } else if field == b"locked" || field.starts_with(b"locked ") {
                info.locked = true;
            }
        }
    }
    if let Some(mut info) = current {
        info.stale |= !info.path.is_dir();
        records.push(info);
    }
    records
}
fn parse_worktrees(output: &[u8]) -> BTreeMap<OsString, WorktreeInfo> {
    let mut locations = BTreeMap::new();
    for info in worktree_records(output) {
        if let Some(branch) = info.branch.clone() {
            locations
                .entry(branch)
                .and_modify(|existing: &mut WorktreeInfo| {
                    existing.stale = true;
                })
                .or_insert(info);
        }
    }
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
