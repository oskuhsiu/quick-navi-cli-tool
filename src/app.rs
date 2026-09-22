//! Single owner of navigation, cached occurrences, and accepted asynchronous work.
use crate::matching;
use crate::model::*;
use crate::scan::{ScanEvent, ScanEventKind, ScanRequest, Scanner};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, atomic::AtomicUsize};
use std::time::{Duration, Instant};

pub const MAX_NODES: usize = 50_000;
pub const MAX_ENTRIES: usize = 1_000_000;

#[derive(Clone, Debug)]
pub struct RestoreIntent {
    pub key: NodeKey,
    pub root_path: PathBuf,
    pub browse: BrowseState,
}
struct PendingScan {
    id: WorkId,
    cancel: CancelToken,
    foreground: bool,
}
type RankedMatch = (u8, usize, usize, PathBuf, NodeKey);
struct MatchJob {
    id: WorkId,
    scope: u64,
    revision: u64,
    version: u64,
    cursor: Option<NodeKey>,
    ranked: BTreeSet<RankedMatch>,
}

pub struct App {
    pub roots: Vec<RootEntry>,
    pub nodes: BTreeMap<NodeKey, DirectoryNode>,
    pub navigation: Navigation,
    pub depth: usize,
    pub scope: u64,
    pub candidate_version: u64,
    pub work_ids: WorkIds,
    pub pending_action: Option<ActionRequest>,
    pub branch_picker: Option<BranchPicker>,
    pub add_root: Option<String>,
    pub restore: Option<RestoreIntent>,
    pub status: String,
    pub policy: ExplorationPolicy,
    pub max_nodes: usize,
    pending_scans: BTreeMap<NodeKey, PendingScan>,
    entries: Arc<AtomicUsize>,
    frontier: Option<NodeKey>,
    matcher: Option<MatchJob>,
    matching_id: Option<WorkId>,
    frontier_active: bool,
    matched_version: Option<(u64, u64, u64)>,
}

impl App {
    pub fn new(roots: Vec<RootEntry>, policy: ExplorationPolicy) -> Self {
        let nodes = roots
            .iter()
            .map(|r| {
                let k = NodeKey::root(r.id);
                (k.clone(), DirectoryNode::new(k))
            })
            .collect();
        Self {
            roots,
            nodes,
            navigation: Navigation::Browse(BrowseState::default()),
            depth: 3,
            scope: 0,
            candidate_version: 0,
            work_ids: WorkIds::default(),
            pending_action: None,
            branch_picker: None,
            add_root: None,
            restore: None,
            status: String::new(),
            policy,
            max_nodes: MAX_NODES,
            pending_scans: BTreeMap::new(),
            entries: Arc::new(AtomicUsize::new(MAX_ENTRIES)),
            frontier: None,
            matcher: None,
            matching_id: None,
            frontier_active: false,
            matched_version: None,
        }
    }
    pub fn path(&self, key: &NodeKey) -> Option<PathBuf> {
        self.roots
            .iter()
            .find(|r| r.id == key.root)
            .map(|r| r.path.join(&key.relative))
    }
    pub fn valid(&self, key: &NodeKey) -> bool {
        key.depth() <= self.depth
            && key
                .chain()
                .iter()
                .all(|k| self.nodes.get(k).is_some_and(|n| n.verified))
    }
    pub fn target(&self) -> Option<NodeKey> {
        let key = match &self.navigation {
            Navigation::Browse(b) => b.chain.get(b.focus),
            Navigation::Filter(s) => match &s.selection {
                MatchSelection::Selected(k) => Some(k),
                _ => None,
            },
        }?;
        self.valid(key).then(|| key.clone())
    }
    pub fn query(&self) -> &str {
        match &self.navigation {
            Navigation::Filter(s) => &s.query,
            _ => "",
        }
    }
    pub fn scope_locked(&self) -> bool {
        self.pending_action.is_some() || self.branch_picker.is_some() || self.add_root.is_some()
    }
    pub fn browse_projection(&self) -> BrowseState {
        match &self.navigation {
            Navigation::Browse(b) => b.clone(),
            Navigation::Filter(_) => self
                .target()
                .map(|k| BrowseState::at(&k))
                .unwrap_or_default(),
        }
    }
    pub fn column_rows(&self, column: usize, browse: &BrowseState) -> Vec<NodeKey> {
        if column == 0 {
            return self.roots.iter().map(|r| NodeKey::root(r.id)).collect();
        }
        browse
            .chain
            .get(column - 1)
            .and_then(|p| self.nodes.get(p))
            .map(|n| {
                n.children
                    .values()
                    .filter(|k| k.depth() <= self.depth)
                    .cloned()
                    .collect()
            })
            .unwrap_or_default()
    }
    pub fn focus_column(&mut self, column: usize) -> bool {
        let Navigation::Browse(b) = &mut self.navigation else {
            return false;
        };
        if column > b.chain.len() || column > self.depth {
            return false;
        }
        self.restore = None;
        b.focus = column;
        true
    }
    pub fn select_column_row(&mut self, column: usize, key: &NodeKey) -> bool {
        let Navigation::Browse(b) = &self.navigation else {
            return false;
        };
        if column > b.chain.len()
            || column > self.depth
            || !self.column_rows(column, b).contains(key)
        {
            return false;
        }
        self.focus_column(column);
        if let Navigation::Browse(b) = &mut self.navigation {
            b.chain.truncate(column);
            b.chain.push(key.clone());
        }
        true
    }
    pub fn select_match(&mut self, key: &NodeKey) -> bool {
        let Navigation::Filter(s) = &mut self.navigation else {
            return false;
        };
        if !s.matches.contains(key) {
            return false;
        }
        self.restore = None;
        s.selection = MatchSelection::Selected(key.clone());
        true
    }
    pub fn move_vertical(&mut self, down: bool) {
        self.restore = None;
        match &self.navigation {
            Navigation::Browse(b) => {
                let rows = self.column_rows(b.focus, b);
                let selected = b
                    .chain
                    .get(b.focus)
                    .and_then(|k| rows.iter().position(|r| r == k));
                if let Some(index) = move_index(rows.len(), selected, down)
                    && let Navigation::Browse(b) = &mut self.navigation
                {
                    b.chain.truncate(b.focus);
                    b.chain.push(rows[index].clone());
                }
            }
            Navigation::Filter(s) => {
                let selected = match &s.selection {
                    MatchSelection::Selected(k) => s.matches.iter().position(|m| m == k),
                    _ => None,
                };
                if let Some(index) = move_index(s.matches.len(), selected, down)
                    && let Navigation::Filter(s) = &mut self.navigation
                {
                    s.selection = MatchSelection::Selected(s.matches[index].clone());
                }
            }
        }
    }
    pub fn move_horizontal(&mut self, right: bool) {
        self.restore = None;
        match &mut self.navigation {
            Navigation::Browse(b) => {
                if right && b.focus < b.chain.len() && b.focus < self.depth {
                    b.focus += 1;
                } else if !right {
                    b.focus = b.focus.saturating_sub(1);
                }
            }
            Navigation::Filter(s) => {
                if right {
                    s.viewport = (s.viewport + 1).min(self.depth);
                } else {
                    s.viewport = s.viewport.saturating_sub(1);
                }
            }
        }
    }
    pub fn edit_query(&mut self, query: String) {
        self.restore = None;
        if query.is_empty() {
            self.clear_filter(false);
            return;
        }
        match &mut self.navigation {
            Navigation::Browse(b) => {
                self.navigation = Navigation::Filter(SearchState {
                    query,
                    revision: 1,
                    matches: vec![],
                    selection: MatchSelection::Waiting,
                    snapshot: b.clone(),
                    viewport: 0,
                })
            }
            Navigation::Filter(s) => {
                s.query = query;
                s.revision += 1;
                s.matches.clear();
                s.selection = MatchSelection::Waiting;
            }
        }
        self.matcher = None;
        self.matching_id = None;
        self.matched_version = None;
        self.frontier_active = true;
        self.match_tick();
    }
    pub fn paste_query(&mut self, text: &str) {
        if text.chars().any(char::is_control) {
            self.status =
                "Paste rejected: search must be a single line without control characters".into();
            return;
        }
        let mut query = self.query().to_owned();
        query.push_str(text);
        self.edit_query(query);
    }
    pub fn clear_filter(&mut self, adopt: bool) {
        let Navigation::Filter(s) = &self.navigation else {
            return;
        };
        let browse = if adopt {
            let Some(k) = self.target() else { return };
            BrowseState::at(&k)
        } else {
            s.snapshot.clone()
        };
        // Keep a listing already supplying the adopted/snapshotted ancestry.
        // Cancelling it would erase the valid match the user just adopted.
        let cancel: Vec<_> = self
            .pending_scans
            .iter()
            .filter(|(k, p)| {
                !p.foreground
                    && !browse
                        .chain
                        .iter()
                        .any(|selected| *k != selected && k.contains(selected))
            })
            .map(|(k, _)| k.clone())
            .collect();
        for key in cancel {
            self.cancel_listing(&key);
        }
        self.navigation = Navigation::Browse(browse);
        self.matcher = None;
        self.matching_id = None;
        self.matched_version = None;
        self.restore = None;
        self.clip_navigation();
    }
    pub fn change_depth(&mut self, increase: bool) {
        if self.scope_locked() {
            self.status = "Finish or cancel the pending action first".into();
            return;
        }
        let next = if increase {
            (self.depth + 1).min(9)
        } else {
            self.depth.saturating_sub(1).max(1)
        };
        if next == self.depth {
            return;
        }
        self.cancel_scans(true);
        self.depth = next;
        self.scope += 1;
        self.restore = None;
        self.entries = Arc::new(AtomicUsize::new(MAX_ENTRIES));
        self.clip_navigation();
        self.changed();
    }
    fn clip_navigation(&mut self) {
        let valid: BTreeSet<_> = self
            .nodes
            .iter()
            .filter(|(k, n)| k.depth() <= self.depth && n.verified)
            .map(|(k, _)| k.clone())
            .collect();
        let clip = |b: &mut BrowseState| {
            let n = b.chain.iter().take_while(|k| valid.contains(*k)).count();
            b.chain.truncate(n.min(self.depth + 1));
            b.focus = b.focus.min(self.depth).min(b.chain.len());
        };
        match &mut self.navigation {
            Navigation::Browse(b) => clip(b),
            Navigation::Filter(s) => {
                clip(&mut s.snapshot);
                s.matches.retain(|k| valid.contains(k));
                if matches!(&s.selection,MatchSelection::Selected(k) if !valid.contains(k)) {
                    s.selection = MatchSelection::ExplicitNone;
                }
            }
        }
    }
    pub fn branch(&self) -> Option<PathBuf> {
        let key = match &self.navigation {
            Navigation::Browse(b) => {
                if b.focus == 0 {
                    b.chain.first().cloned()
                } else {
                    b.chain.get(b.focus - 1).cloned()
                }
            }
            Navigation::Filter(_) => self.target().map(|k| k.parent().unwrap_or(k)),
        }?;
        self.path(&key)
    }
    pub fn refresh_branch(&mut self) {
        if self.scope_locked() {
            return;
        }
        let Some(branch) = self.branch() else {
            self.status = "No current branch; Ctrl-L reloads all roots".into();
            return;
        };
        self.invalidate_path(&branch, true);
        self.status = format!("Refreshing branch: {}", display_path(&branch));
    }
    fn restore_key(&self) -> Option<NodeKey> {
        self.target().or_else(|| match &self.navigation {
            Navigation::Browse(b) => b.chain.last().cloned(),
            _ => None,
        })
    }
    fn save_restore(&mut self) {
        if let Some(key) = self.restore_key()
            && let Some(root_path) = self
                .roots
                .iter()
                .find(|r| r.id == key.root)
                .map(|r| r.path.clone())
        {
            self.restore = Some(RestoreIntent {
                key,
                root_path,
                browse: self.browse_projection(),
            });
        }
    }
    pub fn invalidate_path(&mut self, path: &Path, save_restore: bool) {
        if save_restore
            && self
                .restore_key()
                .and_then(|k| self.path(&k))
                .is_some_and(|p| p.starts_with(path))
        {
            self.save_restore();
        }
        let affected: Vec<_> = self
            .nodes
            .keys()
            .filter(|k| self.path(k).is_some_and(|p| p.starts_with(path)))
            .cloned()
            .collect();
        for key in affected {
            if let Some(p) = self.pending_scans.remove(&key) {
                p.cancel.cancel();
            }
            let node_path = self.path(&key).unwrap();
            if node_path == path || key.depth() == 0 {
                if let Some(n) = self.nodes.get_mut(&key) {
                    n.children.clear();
                    n.listing = ListingState::Unread;
                    n.verified = false;
                }
            } else {
                self.nodes.remove(&key);
            }
        }
        self.remove_dangling_children();
        self.clip_navigation();
        self.entries = Arc::new(AtomicUsize::new(MAX_ENTRIES));
        self.changed();
    }
    pub fn reload_roots(&mut self, roots: Vec<RootEntry>) {
        if self.scope_locked() {
            return;
        }
        self.save_restore();
        let unchanged: BTreeSet<_> = roots
            .iter()
            .filter(|r| self.roots.iter().any(|old| old == *r))
            .map(|r| r.id)
            .collect();
        self.cancel_scans(true);
        self.scope += 1;
        self.roots = roots;
        self.nodes = self
            .roots
            .iter()
            .map(|r| {
                let k = NodeKey::root(r.id);
                let mut n = DirectoryNode::new(k.clone());
                n.verified = false;
                (k, n)
            })
            .collect();
        if self
            .restore
            .as_ref()
            .is_some_and(|r| !unchanged.contains(&r.key.root))
        {
            self.restore = None;
        }
        match &mut self.navigation {
            Navigation::Browse(b) => {
                if b.chain
                    .first()
                    .is_some_and(|k| !unchanged.contains(&k.root))
                {
                    *b = BrowseState::default();
                }
            }
            Navigation::Filter(s) => {
                if s.snapshot
                    .chain
                    .first()
                    .is_some_and(|k| !unchanged.contains(&k.root))
                {
                    s.snapshot = BrowseState::default();
                }
                s.matches.clear();
                s.selection = MatchSelection::ExplicitNone;
            }
        }
        self.entries = Arc::new(AtomicUsize::new(MAX_ENTRIES));
        self.changed();
        self.status = "All roots reloaded; refreshing needed columns".into();
    }
    pub fn added_root(&mut self, roots: Vec<RootEntry>, root: RootEntry) {
        self.add_root = None;
        self.pending_action = None;
        self.reload_roots(roots);
        if let Some(n) = self.nodes.get_mut(&NodeKey::root(root.id)) {
            n.verified = true;
        }
        self.restore = None;
        self.navigation = Navigation::Browse(BrowseState::at(&NodeKey::root(root.id)));
        self.status = format!("Root added: {}", display_path(&root.path));
    }
    pub fn mark_unavailable(&mut self, path: &Path, reason: &str) {
        self.invalidate_path(path, false);
        let affected: Vec<_> = self
            .nodes
            .keys()
            .filter(|k| self.path(k).is_some_and(|p| p.starts_with(path)))
            .cloned()
            .collect();
        for key in affected {
            if key.depth() == 0 {
                let node = self.nodes.get_mut(&key).unwrap();
                node.verified = false;
                node.listing = ListingState::Failed(reason.into());
            } else {
                self.nodes.remove(&key);
            }
        }
        if self
            .restore
            .as_ref()
            .and_then(|r| self.path(&r.key))
            .is_some_and(|p| p.starts_with(path))
        {
            self.restore = None;
        }
        self.remove_dangling_children();
        self.clip_navigation();
        self.changed();
    }
    pub fn directory_created(&mut self, request: &ActionRequest, created: &Path, message: String) {
        self.invalidate_path(&request.target, false);
        let child = request
            .source
            .child(created.file_name().expect("created child has a name"));
        let root = self
            .roots
            .iter()
            .find(|r| r.id == request.source.root)
            .expect("scope stays locked through action");
        let shown = child.depth() <= self.depth && self.policy.allows(&root.path, created);
        let desired = if shown { child } else { request.source.clone() };
        self.navigation = Navigation::Browse(BrowseState::at(&request.source));
        self.restore = Some(RestoreIntent {
            key: desired.clone(),
            root_path: root.path.clone(),
            browse: BrowseState::at(&desired),
        });
        self.status = if shown {
            message
        } else {
            format!("{message}; outside depth or excluded — staying at parent")
        };
    }

    pub fn start_branch_picker(&mut self) -> Option<WorkId> {
        if self.scope_locked() {
            return None;
        }
        let source = self.target()?;
        let target = self.path(&source)?;
        let id = self.work_ids.allocate()?;
        self.branch_picker = Some(BranchPicker {
            id,
            source,
            target,
            phase: BranchPickerPhase::Loading,
            branches: Vec::new(),
            query: String::new(),
            selected: 0,
            create: None,
            create_open: false,
            select_on_load: None,
            cancel: CancelToken::default(),
        });
        Some(id)
    }

    pub fn set_branch_picker_entries(&mut self, id: WorkId, branches: Vec<BranchEntry>) -> bool {
        let Some(picker) = &mut self.branch_picker else {
            return false;
        };
        if picker.id != id {
            return false;
        }
        picker.branches = branches;
        picker.phase = BranchPickerPhase::Ready;
        picker.selected = 0;
        if let Some(name) = picker.select_on_load.take() {
            picker.query.clear();
            picker.selected = picker
                .branches
                .iter()
                .position(|branch| branch.name == name)
                .unwrap_or(0);
        }
        true
    }

    pub fn start_branch_create(&mut self) -> bool {
        if self.pending_action.is_some() {
            return false;
        }
        let Some(picker) = &mut self.branch_picker else {
            return false;
        };
        if picker.phase != BranchPickerPhase::Ready {
            return false;
        }
        let Some(base) = picker.selected_entry().map(|entry| entry.name.clone()) else {
            self.status = "Select a local branch to use as the base".into();
            return false;
        };
        let create = picker.create.get_or_insert(BranchCreate {
            base: base.clone(),
            destination: String::new(),
            mode: BranchCreateMode::Workspace,
        });
        create.base = base;
        picker.create_open = true;
        self.status.clear();
        true
    }

    pub fn freeze_branch_create(&mut self) -> Option<WorkId> {
        if self.pending_action.is_some() {
            return None;
        }
        let picker = self.branch_picker.as_ref()?;
        if picker.phase != BranchPickerPhase::Ready || !picker.create_open {
            return None;
        }
        let create = picker.create.clone()?;
        if create.destination.is_empty() {
            self.status = "Enter a new branch name".into();
            return None;
        }
        let id = self.work_ids.allocate()?;
        self.pending_action = Some(ActionRequest {
            id,
            kind: ActionKind::CreateBranch,
            source: picker.source.clone(),
            target: picker.target.clone(),
            phase: ActionPhase::Checking,
            child_name: None,
            branch: None,
            branch_create: Some(create),
            cancel: CancelToken::default(),
        });
        Some(id)
    }

    pub fn edit_branch_picker(&mut self, query: String) -> bool {
        let Some(picker) = &mut self.branch_picker else {
            return false;
        };
        if picker.phase != BranchPickerPhase::Ready {
            return false;
        }
        picker.query = query;
        picker.selected = 0;
        true
    }

    pub fn paste_branch_picker(&mut self, text: &str) -> bool {
        if self.pending_action.is_some() {
            return false;
        }
        if text.chars().any(char::is_control) {
            self.status = "Paste rejected: branch name/filter must be a single line".into();
            return false;
        }
        let Some(picker) = &mut self.branch_picker else {
            return false;
        };
        if picker.phase != BranchPickerPhase::Ready {
            return false;
        }
        if picker.create_open {
            if let Some(create) = &mut picker.create {
                create.destination.push_str(text);
            }
        } else {
            picker.query.push_str(text);
            picker.selected = 0;
        }
        true
    }

    pub fn move_branch_picker(&mut self, down: bool) -> bool {
        let Some(picker) = &mut self.branch_picker else {
            return false;
        };
        if picker.phase != BranchPickerPhase::Ready {
            return false;
        }
        let visible = picker.visible_indices();
        if let Some(index) = move_index(visible.len(), Some(picker.selected), down) {
            picker.selected = index;
            true
        } else {
            false
        }
    }

    pub fn cancel_branch_picker(&mut self) -> bool {
        let Some(picker) = self.branch_picker.take() else {
            return false;
        };
        picker.cancel.cancel();
        true
    }

    pub fn take_branch_selection(&mut self) -> Option<BranchSelection> {
        let picker = self.branch_picker.as_ref()?;
        if picker.phase != BranchPickerPhase::Ready {
            return None;
        }
        let branch = picker.selected_entry()?.name.clone();
        let selection = BranchSelection {
            source: picker.source.clone(),
            target: picker.target.clone(),
            branch,
        };
        self.branch_picker.take();
        Some(selection)
    }
    fn changed(&mut self) {
        self.candidate_version += 1;
        self.matcher = None;
        self.matching_id = None;
        self.matched_version = None;
        self.frontier = None;
        self.frontier_active = true;
    }
    fn remove_dangling_children(&mut self) {
        let keys: BTreeSet<_> = self.nodes.keys().cloned().collect();
        for n in self.nodes.values_mut() {
            n.children.retain(|_, k| keys.contains(k));
        }
    }
    fn cancel_scans(&mut self, all: bool) {
        let keys: Vec<_> = self
            .pending_scans
            .iter()
            .filter(|(_, p)| all || !p.foreground)
            .map(|(k, _)| k.clone())
            .collect();
        for key in keys {
            self.cancel_listing(&key);
        }
    }
    fn cancel_listing(&mut self, key: &NodeKey) {
        let keys: Vec<_> = self
            .nodes
            .keys()
            .filter(|k| key.contains(k))
            .cloned()
            .collect();
        for k in keys {
            if let Some(p) = self.pending_scans.remove(&k) {
                p.cancel.cancel();
            }
            if &k == key {
                if let Some(n) = self.nodes.get_mut(&k) {
                    n.children.clear();
                    n.listing = ListingState::Unread;
                }
            } else {
                self.nodes.remove(&k);
            }
        }
        self.remove_dangling_children();
        self.changed();
    }
    pub fn prepare_scan(&mut self, key: NodeKey, foreground: bool) -> Option<ScanRequest> {
        if key.depth() > self.depth {
            return None;
        }
        if let Some(p) = self.pending_scans.get(&key) {
            if !foreground || p.foreground {
                return None;
            }
            // This column already has usable rows; let its existing producer
            // finish. Reserve the foreground lane for a genuinely waiting view.
            if self.nodes.get(&key).is_some_and(|n| !n.children.is_empty()) {
                return None;
            }
            self.cancel_listing(&key);
        }
        if self.pending_scans.len() >= 64 {
            return None;
        }
        let n = self.nodes.get(&key)?;
        if n.listing != ListingState::Unread || (key.depth() == self.depth && n.verified) {
            return None;
        }
        let id = self.work_ids.allocate()?;
        let cancel = CancelToken::default();
        let path = self.path(&key)?;
        let root_path = self.roots.iter().find(|r| r.id == key.root)?.path.clone();
        self.nodes.get_mut(&key)?.listing = ListingState::Loading(id);
        self.pending_scans.insert(
            key.clone(),
            PendingScan {
                id,
                cancel: cancel.clone(),
                foreground,
            },
        );
        Some(ScanRequest {
            scope: self.scope,
            key: key.clone(),
            id,
            root_path,
            path,
            cancel,
            entry_budget: self.entries.clone(),
            list_children: key.depth() < self.depth,
        })
    }
    pub fn reject_submission(&mut self, request: &ScanRequest) {
        if self
            .pending_scans
            .get(&request.key)
            .is_some_and(|p| p.id == request.id)
        {
            self.pending_scans.remove(&request.key);
            request.cancel.cancel();
            if let Some(n) = self.nodes.get_mut(&request.key) {
                n.listing = ListingState::Unread;
            }
        }
    }
    pub fn schedule(&mut self, scanner: &Scanner) {
        let mut foreground = self.browse_projection().chain;
        if let Some(r) = &self.restore {
            foreground.extend(r.key.chain());
        }
        foreground.dedup();
        for key in foreground {
            if let Some(req) = self.prepare_scan(key, true)
                && let Err(req) = scanner.try_submit(req, true)
            {
                self.reject_submission(&req);
                break;
            }
        }
        if !matches!(self.navigation, Navigation::Filter(_)) || !self.frontier_active {
            return;
        }
        let start = Instant::now();
        for _ in 0..256 {
            if start.elapsed() >= Duration::from_millis(2) || self.pending_scans.len() >= 64 {
                break;
            }
            let key = match &self.frontier {
                Some(cursor) => self
                    .nodes
                    .range((
                        std::ops::Bound::Excluded(cursor),
                        std::ops::Bound::Unbounded,
                    ))
                    .next()
                    .map(|(k, _)| k.clone()),
                None => self.nodes.keys().next().cloned(),
            };
            let Some(key) = key else {
                self.frontier = None;
                self.frontier_active = false;
                break;
            };
            self.frontier = Some(key.clone());
            if let Some(req) = self.prepare_scan(key, false)
                && let Err(req) = scanner.try_submit(req, false)
            {
                self.reject_submission(&req);
                self.frontier = None;
                break;
            }
        }
    }
    pub fn accept_scan(&mut self, event: ScanEvent) -> bool {
        if event.scope != self.scope
            || !self
                .pending_scans
                .get(&event.key)
                .is_some_and(|p| p.id == event.id)
        {
            return false;
        }
        if !self
            .nodes
            .get(&event.key)
            .is_some_and(|n| n.listing == ListingState::Loading(event.id))
        {
            return false;
        }
        let mut candidates_changed = !self.nodes[&event.key].verified;
        match event.kind {
            ScanEventKind::Batch { names, .. } => {
                candidates_changed |= !names.is_empty();
                self.nodes.get_mut(&event.key).unwrap().verified = true;
                for name in names {
                    let key = event.key.child(&name);
                    if key.depth() > self.depth {
                        continue;
                    }
                    if self.nodes.len() >= self.max_nodes + self.roots.len()
                        && !self.nodes.contains_key(&key)
                        && !self.evict_unprotected(&event.key)
                    {
                        self.nodes.get_mut(&event.key).unwrap().listing =
                            ListingState::Partial("directory capacity reached".into());
                        if let Some(p) = self.pending_scans.remove(&event.key) {
                            p.cancel.cancel();
                        }
                        break;
                    }
                    self.nodes
                        .entry(key.clone())
                        .or_insert_with(|| DirectoryNode::new(key.clone()));
                    self.nodes
                        .get_mut(&event.key)
                        .unwrap()
                        .children
                        .insert(name, key);
                }
            }
            ScanEventKind::Done => {
                let n = self.nodes.get_mut(&event.key).unwrap();
                n.listing = ListingState::Ready;
                n.verified = true;
                self.pending_scans.remove(&event.key);
            }
            ScanEventKind::Failed(reason) => {
                self.cancel_listing(&event.key);
                let n = self.nodes.get_mut(&event.key).unwrap();
                n.listing = ListingState::Failed(reason);
                n.verified = false;
                candidates_changed = true;
            }
            ScanEventKind::Partial(reason) => {
                let n = self.nodes.get_mut(&event.key).unwrap();
                n.listing = ListingState::Partial(reason);
                self.pending_scans.remove(&event.key);
            }
        }
        if candidates_changed {
            self.changed();
        }
        self.try_restore();
        self.clip_navigation();
        true
    }
    fn evict_unprotected(&mut self, incoming: &NodeKey) -> bool {
        if matches!(self.navigation, Navigation::Filter(_)) {
            return false;
        }
        let chain = self.browse_projection().chain;
        let candidate = self
            .nodes
            .iter()
            .find(|(k, n)| {
                !k.contains(incoming)
                    && !chain.iter().any(|c| k.contains(c))
                    && !self.restore.as_ref().is_some_and(|r| k.contains(&r.key))
                    && !n.children.is_empty()
                    && n.listing == ListingState::Ready
            })
            .map(|(k, _)| k.clone());
        if let Some(k) = candidate {
            self.cancel_listing(&k);
            true
        } else {
            false
        }
    }
    fn try_restore(&mut self) {
        let Some(r) = &self.restore else {
            return;
        };
        if !self
            .roots
            .iter()
            .any(|root| root.id == r.key.root && root.path == r.root_path)
        {
            self.restore = None;
            return;
        }
        if !r.key.chain().iter().all(|k| self.valid(k)) {
            return;
        }
        if let Navigation::Filter(s) = &self.navigation {
            let Some(path) = self.path(&r.key) else {
                return;
            };
            if matching::rank(
                &display_os(path.file_name().unwrap_or(path.as_os_str())),
                &s.query,
            )
            .is_none()
            {
                self.restore = None;
                return;
            }
        }
        let r = self.restore.take().unwrap();
        match &mut self.navigation {
            Navigation::Browse(b) => *b = r.browse,
            Navigation::Filter(s) => s.selection = MatchSelection::Selected(r.key),
        }
    }
    pub fn completion(&self) -> &'static str {
        let mut incomplete = false;
        let mut partial = false;
        for (k, n) in &self.nodes {
            if k.depth() > self.depth {
                continue;
            }
            if matches!(
                n.listing,
                ListingState::Partial(_) | ListingState::Failed(_)
            ) {
                partial = true;
            }
            if (k.depth() < self.depth || !n.verified)
                && matches!(n.listing, ListingState::Unread | ListingState::Loading(_))
            {
                incomplete = true;
            }
        }
        if incomplete {
            "searching"
        } else if partial {
            "partial"
        } else {
            "complete"
        }
    }
    pub fn match_tick(&mut self) -> bool {
        let Navigation::Filter(s) = &self.navigation else {
            return false;
        };
        let stamp = (self.scope, s.revision, self.candidate_version);
        if self.matched_version == Some(stamp) {
            return false;
        }
        if self.matcher.is_none() {
            let Some(id) = self.work_ids.allocate() else {
                return false;
            };
            self.matching_id = Some(id);
            self.matcher = Some(MatchJob {
                id,
                scope: self.scope,
                revision: s.revision,
                version: self.candidate_version,
                cursor: None,
                ranked: BTreeSet::new(),
            });
        }
        let mut job = self.matcher.take().unwrap();
        let started = Instant::now();
        let mut finished = false;
        while started.elapsed() < Duration::from_millis(4) {
            let next = match &job.cursor {
                Some(c) => self
                    .nodes
                    .range((std::ops::Bound::Excluded(c), std::ops::Bound::Unbounded))
                    .next(),
                None => self.nodes.iter().next(),
            };
            let Some((key, node)) = next else {
                finished = true;
                break;
            };
            job.cursor = Some(key.clone());
            // Invalidating a parent removes its descendant occurrences. The
            // candidate index therefore already guarantees a verified ancestry;
            // rebuilding and looking up that chain for every keystroke is redundant.
            if !node.verified || key.depth() > self.depth {
                continue;
            }
            let root_index = self.roots.iter().position(|r| r.id == key.root).unwrap();
            let path = self.roots[root_index].path.join(&key.relative);
            let name = display_os(path.file_name().unwrap_or(path.as_os_str()));
            if let Some(rank) = matching::rank(&name, &s.query) {
                job.ranked
                    .insert((rank, key.depth(), root_index, path, key.clone()));
            }
        }
        if !finished {
            self.matcher = Some(job);
            return false;
        }
        let matches = job.ranked.into_iter().map(|r| r.4).collect();
        self.commit_matches(job.id, job.scope, job.revision, job.version, matches)
    }
    fn commit_matches(
        &mut self,
        id: WorkId,
        scope: u64,
        revision: u64,
        version: u64,
        matches: Vec<NodeKey>,
    ) -> bool {
        let Navigation::Filter(s) = &mut self.navigation else {
            return false;
        };
        if self.matching_id != Some(id)
            || (scope, revision, version) != (self.scope, s.revision, self.candidate_version)
        {
            return false;
        }
        s.matches = matches;
        if s.selection == MatchSelection::Waiting {
            if let Some(k) = s.matches.first() {
                s.selection = MatchSelection::Selected(k.clone());
            }
        } else if matches!(&s.selection,MatchSelection::Selected(k) if !s.matches.contains(k)) {
            s.selection = MatchSelection::ExplicitNone;
        }
        self.matched_version = Some((scope, revision, version));
        true
    }
    pub fn freeze_action(&mut self, kind: ActionKind) -> Option<WorkId> {
        if self.scope_locked() {
            return None;
        }
        let source = self.target()?;
        let target = self.path(&source)?;
        let id = self.work_ids.allocate()?;
        self.pending_action = Some(ActionRequest {
            id,
            kind,
            source,
            target,
            phase: ActionPhase::Checking,
            child_name: None,
            branch: None,
            branch_create: None,
            cancel: CancelToken::default(),
        });
        Some(id)
    }

    pub fn freeze_branch_action(&mut self, selection: BranchSelection) -> Option<WorkId> {
        if self.scope_locked() {
            return None;
        }
        let id = self.work_ids.allocate()?;
        self.pending_action = Some(ActionRequest {
            id,
            kind: ActionKind::OpenBranchWorkspace,
            source: selection.source,
            target: selection.target,
            phase: ActionPhase::Checking,
            child_name: None,
            branch: Some(selection.branch),
            branch_create: None,
            cancel: CancelToken::default(),
        });
        Some(id)
    }
    pub fn cancel_action(&mut self) {
        let Some(a) = &mut self.pending_action else {
            return;
        };
        match a.phase {
            ActionPhase::Input(_) | ActionPhase::Confirm { .. } => self.pending_action = None,
            ActionPhase::Checking | ActionPhase::Running => {
                a.cancel.cancel();
                a.phase = ActionPhase::Cancelling;
            }
            _ => {}
        }
    }
    pub fn pending_scan_count(&self) -> usize {
        self.pending_scans.len()
    }
    pub fn matching_in_progress(&self) -> bool {
        self.matcher.is_some()
    }
    pub fn working(&self) -> bool {
        !self.pending_scans.is_empty()
            || self.matcher.is_some()
            || (self.frontier_active && matches!(self.navigation, Navigation::Filter(_)))
    }
}

fn move_index(len: usize, current: Option<usize>, down: bool) -> Option<usize> {
    if len == 0 {
        return None;
    }
    Some(match current {
        None => {
            if down {
                0
            } else {
                len - 1
            }
        }
        Some(i) => {
            if down {
                (i + 1).min(len - 1)
            } else {
                i.saturating_sub(1)
            }
        }
    })
}

#[cfg(test)]
mod matching_contract {
    use super::*;
    #[test]
    fn delayed_results_require_every_generation_field_and_work_id() {
        let mut app = App::new(
            vec![RootEntry {
                id: 1,
                path: "/work".into(),
            }],
            ExplorationPolicy { home: None },
        );
        app.edit_query("work".into());
        let id = app.work_ids.allocate().unwrap();
        app.matching_id = Some(id);
        let scope = app.scope;
        let version = app.candidate_version;
        for (i, s, q, c) in [
            (id - 1, scope, 1, version),
            (id, scope + 1, 1, version),
            (id, scope, 2, version),
            (id, scope, 1, version + 1),
        ] {
            assert!(!app.commit_matches(i, s, q, c, vec![]));
        }
        assert!(app.commit_matches(id, scope, 1, version, vec![NodeKey::root(1)]));
        app.changed();
        assert!(!app.commit_matches(id, scope, 1, version, vec![]));
        app.clear_filter(false);
        app.edit_query("work".into());
        assert!(!app.commit_matches(id, scope, 1, version, vec![]));
    }
}
