//! Bounded, cancellation-aware directory listing. Each lane owns one worker.
use crate::model::{CancelToken, ExplorationPolicy, NodeKey, WorkId};
use rustix::fs::{self, AtFlags, Dir, FileType, Mode, OFlags};
use std::{
    ffi::OsString,
    os::unix::ffi::OsStrExt,
    path::{Component, Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
        mpsc::{self, Receiver, SyncSender, TrySendError},
    },
    thread,
    time::{Duration, Instant},
};

#[derive(Debug)]
pub struct ScanRequest {
    pub scope: u64,
    pub key: NodeKey,
    pub id: WorkId,
    pub root_path: PathBuf,
    pub path: PathBuf,
    pub cancel: CancelToken,
    pub entry_budget: Arc<AtomicUsize>,
    pub list_children: bool,
}
#[derive(Debug)]
pub struct ScanEvent {
    pub scope: u64,
    pub key: NodeKey,
    pub id: WorkId,
    pub kind: ScanEventKind,
}
#[derive(Debug)]
pub enum ScanEventKind {
    Batch {
        names: Vec<OsString>,
        examined: usize,
    },
    Done,
    Failed(String),
    Partial(String),
}

pub struct Scanner {
    foreground: SyncSender<ScanRequest>,
    background: SyncSender<ScanRequest>,
    results: Receiver<ScanEvent>,
    shutdown: CancelToken,
}
impl Scanner {
    pub fn new(policy: ExplorationPolicy) -> Self {
        let (results_tx, results) = mpsc::sync_channel(8);
        let shutdown = CancelToken::default();
        let lane = |name: &str| {
            let (tx, rx) = mpsc::sync_channel::<ScanRequest>(32);
            let results = results_tx.clone();
            let policy = policy.clone();
            let shutdown = shutdown.clone();
            thread::Builder::new()
                .name(name.into())
                .spawn(move || {
                    while let Ok(request) = rx.recv() {
                        if shutdown.is_cancelled() {
                            break;
                        }
                        if request.cancel.is_cancelled() {
                            continue;
                        }
                        scan(&request, &policy, &results, &shutdown);
                    }
                })
                .expect("create fixed scanner worker");
            tx
        };
        let foreground = lane("qnav-foreground");
        let background = lane("qnav-background");
        Self {
            foreground,
            background,
            results,
            shutdown,
        }
    }
    pub fn try_submit(&self, request: ScanRequest, foreground: bool) -> Result<(), ScanRequest> {
        if request.cancel.is_cancelled() {
            return Err(request);
        }
        let sender = if foreground {
            &self.foreground
        } else {
            &self.background
        };
        sender.try_send(request).map_err(|error| match error {
            TrySendError::Full(r) | TrySendError::Disconnected(r) => r,
        })
    }
    pub fn try_recv(&self) -> Option<ScanEvent> {
        self.results.try_recv().ok()
    }
}
impl Drop for Scanner {
    fn drop(&mut self) {
        self.shutdown.cancel();
    }
}

fn open_directory(path: &Path) -> Result<std::os::fd::OwnedFd, String> {
    open_directory_checked(path, || false)
}

fn open_directory_checked(
    path: &Path,
    cancelled: impl Fn() -> bool,
) -> Result<std::os::fd::OwnedFd, String> {
    if !path.is_absolute() {
        return Err("directory path must be absolute".into());
    }
    let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
    if cancelled() {
        return Err("cancelled".into());
    }
    let mut fd = fs::open("/", flags, Mode::empty()).map_err(|e| e.to_string())?;
    for component in path.components() {
        if cancelled() {
            return Err("cancelled".into());
        }
        match component {
            Component::RootDir => {}
            Component::Normal(name) => {
                fd = fs::openat(&fd, name, flags, Mode::empty()).map_err(|e| e.to_string())?;
            }
            _ => return Err("directory path must contain only normal components".into()),
        }
    }
    Ok(fd)
}
pub fn validate_directory(path: &Path) -> Result<(), String> {
    open_directory(path).map(drop)
}

fn send(
    request: &ScanRequest,
    sender: &SyncSender<ScanEvent>,
    shutdown: &CancelToken,
    kind: ScanEventKind,
) -> bool {
    let mut event = ScanEvent {
        scope: request.scope,
        key: request.key.clone(),
        id: request.id,
        kind,
    };
    loop {
        if request.cancel.is_cancelled() || shutdown.is_cancelled() {
            return false;
        }
        match sender.try_send(event) {
            Ok(()) => return true,
            Err(TrySendError::Disconnected(_)) => return false,
            Err(TrySendError::Full(returned)) => {
                event = returned;
                thread::sleep(Duration::from_millis(1));
            }
        }
    }
}
fn scan(
    request: &ScanRequest,
    policy: &ExplorationPolicy,
    sender: &SyncSender<ScanEvent>,
    shutdown: &CancelToken,
) {
    let emit = |kind| send(request, sender, shutdown, kind);
    // Validate the declared root/key boundary, including every pruned ancestor,
    // before opening any descendant. openat never follows replaced symlinks.
    if !request.root_path.is_absolute()
        || request.root_path.join(&request.key.relative) != request.path
        || request
            .key
            .relative
            .components()
            .any(|c| !matches!(c, Component::Normal(_)))
    {
        emit(ScanEventKind::Failed("invalid scan path identity".into()));
        return;
    }
    let mut candidate = request.root_path.clone();
    for component in request.key.relative.components() {
        candidate.push(component);
        if !policy.allows(&request.root_path, &candidate) {
            emit(ScanEventKind::Failed("directory is excluded".into()));
            return;
        }
    }
    let fd = match open_directory_checked(&request.path, || {
        request.cancel.is_cancelled() || shutdown.is_cancelled()
    }) {
        Ok(fd) => fd,
        Err(e) => {
            emit(ScanEventKind::Failed(e));
            return;
        }
    };
    if !request.list_children {
        emit(ScanEventKind::Done);
        return;
    }
    if request.cancel.is_cancelled() || shutdown.is_cancelled() {
        return;
    }
    let mut dir = match Dir::read_from(&fd) {
        Ok(dir) => dir,
        Err(e) => {
            emit(ScanEventKind::Failed(e.to_string()));
            return;
        }
    };
    let mut names = Vec::new();
    let mut examined = 0;
    let mut started = Instant::now();
    let terminal = loop {
        if request.cancel.is_cancelled() || shutdown.is_cancelled() {
            return;
        }
        let entry = match dir.next() {
            None => break ScanEventKind::Done,
            Some(Err(e)) => break ScanEventKind::Partial(e.to_string()),
            Some(Ok(entry)) => entry,
        };
        let bytes = entry.file_name().to_bytes();
        if bytes == b"." || bytes == b".." {
            continue;
        }
        if request
            .entry_budget
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |remaining| {
                remaining.checked_sub(1)
            })
            .is_err()
        {
            break ScanEventKind::Partial("directory entry budget exhausted".into());
        }
        examined += 1;
        let name = std::ffi::OsStr::from_bytes(bytes);
        if policy.allows(&request.root_path, &request.path.join(name)) {
            match fs::statat(&fd, name, AtFlags::SYMLINK_NOFOLLOW) {
                Ok(stat) if FileType::from_raw_mode(stat.st_mode) == FileType::Directory => {
                    names.push(name.to_os_string())
                }
                Ok(_) => {}
                Err(e) => break ScanEventKind::Partial(e.to_string()),
            }
        }
        if examined == 256 || started.elapsed() >= Duration::from_millis(8) {
            if !emit(ScanEventKind::Batch {
                names: std::mem::take(&mut names),
                examined,
            }) {
                return;
            }
            examined = 0;
            started = Instant::now();
        }
    };
    if examined > 0 && !emit(ScanEventKind::Batch { names, examined }) {
        return;
    }
    emit(terminal);
}
