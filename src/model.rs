//! Shared domain values. Paths are never reconstructed from presentation text.
use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

pub type RootId = u64;
pub type WorkId = u64;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RootEntry {
    pub id: RootId,
    pub path: PathBuf,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct NodeKey {
    pub root: RootId,
    pub relative: PathBuf,
}
impl NodeKey {
    pub fn root(id: RootId) -> Self {
        Self {
            root: id,
            relative: PathBuf::new(),
        }
    }
    pub fn depth(&self) -> usize {
        self.relative.components().count()
    }
    pub fn child(&self, name: &OsStr) -> Self {
        Self {
            root: self.root,
            relative: self.relative.join(name),
        }
    }
    pub fn parent(&self) -> Option<Self> {
        self.relative.parent().map(|p| Self {
            root: self.root,
            relative: p.into(),
        })
    }
    pub fn contains(&self, other: &Self) -> bool {
        self.root == other.root && other.relative.starts_with(&self.relative)
    }
    pub fn chain(&self) -> Vec<Self> {
        let mut chain = vec![self.clone()];
        while let Some(p) = chain.last().and_then(Self::parent) {
            chain.push(p);
        }
        chain.reverse();
        chain
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ListingState {
    Unread,
    Loading(WorkId),
    Ready,
    Partial(String),
    Failed(String),
}

#[derive(Clone, Debug)]
pub struct DirectoryNode {
    pub key: NodeKey,
    pub children: BTreeMap<std::ffi::OsString, NodeKey>,
    pub listing: ListingState,
    pub verified: bool,
}
impl DirectoryNode {
    pub fn new(key: NodeKey) -> Self {
        Self {
            key,
            children: BTreeMap::new(),
            listing: ListingState::Unread,
            verified: true,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BrowseState {
    pub chain: Vec<NodeKey>,
    pub focus: usize,
}
impl BrowseState {
    pub fn at(key: &NodeKey) -> Self {
        Self {
            chain: key.chain(),
            focus: key.depth(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MatchSelection {
    Waiting,
    Selected(NodeKey),
    ExplicitNone,
}

#[derive(Clone, Debug)]
pub struct SearchState {
    pub query: String,
    pub revision: u64,
    pub matches: Vec<NodeKey>,
    pub selection: MatchSelection,
    pub snapshot: BrowseState,
    pub viewport: usize,
}
#[derive(Clone, Debug)]
pub enum Navigation {
    Browse(BrowseState),
    Filter(SearchState),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ActionKind {
    ChangeDirectory,
    Mkdir,
    GitInit,
    OpenHd,
    OpenPane,
    ListBranches,
    OpenBranchWorkspace,
    CreateBranch,
    DeleteBranch,
    RemoveBranchWorkspace,
}

impl ActionKind {
    pub fn is_branch_cleanup(self) -> bool {
        matches!(self, Self::DeleteBranch | Self::RemoveBranchWorkspace)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BranchCreateMode {
    Workspace,
    BranchOnly,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BranchCreate {
    pub base: OsString,
    pub destination: String,
    pub mode: BranchCreateMode,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BranchEntry {
    pub name: OsString,
    pub worktree: Option<PathBuf>,
    pub stale_worktree: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BranchPickerPhase {
    Loading,
    Ready,
}

#[derive(Clone, Debug)]
pub struct BranchPicker {
    pub id: WorkId,
    pub source: NodeKey,
    pub target: PathBuf,
    pub phase: BranchPickerPhase,
    pub branches: Vec<BranchEntry>,
    pub query: String,
    pub selected: usize,
    pub create: Option<BranchCreate>,
    pub create_open: bool,
    pub select_on_load: Option<OsString>,
    pub cancel: CancelToken,
}

impl BranchPicker {
    pub fn visible_indices(&self) -> Vec<usize> {
        let query = self.query.to_lowercase();
        self.branches
            .iter()
            .enumerate()
            .filter(|(_, branch)| {
                query.is_empty()
                    || display_os(branch.name.as_os_str())
                        .to_lowercase()
                        .contains(&query)
            })
            .map(|(index, _)| index)
            .collect()
    }

    pub fn selected_entry(&self) -> Option<&BranchEntry> {
        self.visible_indices()
            .get(self.selected)
            .and_then(|index| self.branches.get(*index))
    }
}

#[derive(Clone, Debug)]
pub struct BranchSelection {
    pub source: NodeKey,
    pub target: PathBuf,
    pub branch: OsString,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ActionPhase {
    Checking,
    Input(String),
    Confirm { message: String, accepted: bool },
    Running,
    Cancelling,
}
#[derive(Clone, Debug)]
pub struct ActionRequest {
    pub id: WorkId,
    pub kind: ActionKind,
    pub source: NodeKey,
    pub target: PathBuf,
    pub phase: ActionPhase,
    pub child_name: Option<String>,
    pub branch: Option<OsString>,
    pub branch_create: Option<BranchCreate>,
    pub cancel: CancelToken,
}
#[derive(Clone, Debug, Default)]
pub struct CancelToken(Arc<AtomicBool>);
impl CancelToken {
    pub fn cancel(&self) {
        self.0.store(true, Ordering::Release);
    }
    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }
}

#[derive(Clone, Debug, Default)]
pub struct WorkIds {
    next: WorkId,
}
impl WorkIds {
    pub fn allocate(&mut self) -> Option<WorkId> {
        self.next = self.next.checked_add(1)?;
        Some(self.next)
    }
}

#[derive(Clone, Debug)]
pub struct ExplorationPolicy {
    pub home: Option<PathBuf>,
}
impl ExplorationPolicy {
    pub fn allows(&self, root: &Path, path: &Path) -> bool {
        if path == root {
            return true;
        }
        let excluded = [
            ".git",
            ".cache",
            "node_modules",
            ".venv",
            "venv",
            ".gradle",
            ".next",
            "build",
            "dist",
            "target",
            "vendor",
        ];
        if path
            .file_name()
            .is_some_and(|n| excluded.iter().any(|e| n == OsStr::new(e)))
        {
            return false;
        }
        let mut absolute = vec![
            PathBuf::from("/Library"),
            PathBuf::from("/Applications"),
            PathBuf::from("/System/Library"),
        ];
        if let Some(home) = &self.home {
            absolute.push(home.join("Library"));
            absolute.push(home.join("Applications"));
        }
        !absolute
            .iter()
            .any(|p| !root.starts_with(p) && path.starts_with(p))
    }
}

/// Printable, lossless distinction for invalid UTF-8 and terminal control bytes.
pub fn display_path(path: &Path) -> String {
    display_os(path.as_os_str())
}
pub fn display_os(value: &OsStr) -> String {
    use std::os::unix::ffi::OsStrExt;
    let mut bytes = value.as_bytes();
    let mut out = String::new();
    while !bytes.is_empty() {
        match std::str::from_utf8(bytes) {
            Ok(s) => {
                escape_text(s, &mut out);
                break;
            }
            Err(e) => {
                let n = e.valid_up_to();
                if n > 0 {
                    escape_text(
                        std::str::from_utf8(&bytes[..n]).expect("valid prefix"),
                        &mut out,
                    );
                }
                let bad = e.error_len().unwrap_or(bytes.len() - n);
                for b in &bytes[n..n + bad] {
                    out.push_str(&format!("\\x{b:02x}"));
                }
                bytes = &bytes[n + bad..];
            }
        }
    }
    out
}
fn escape_text(s: &str, out: &mut String) {
    for c in s.chars() {
        match c {
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\\' => out.push_str("\\\\"),
            c if c.is_control() => out.extend(c.escape_unicode()),
            c => out.push(c),
        }
    }
}
