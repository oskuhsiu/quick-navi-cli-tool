use crossterm::{
    cursor::{Hide, Show},
    event::{
        self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
        Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent,
        MouseEventKind,
    },
    execute,
    terminal::{
        BeginSynchronizedUpdate, EndSynchronizedUpdate, EnterAlternateScreen, LeaveAlternateScreen,
        disable_raw_mode, enable_raw_mode,
    },
};
use qnav::{
    actions::{ActionEvent, ActionOutcome, Actions},
    app::App,
    config,
    model::*,
    scan::Scanner,
    ui,
};
use std::{
    ffi::OsString,
    io::{self, IsTerminal, Write},
    path::PathBuf,
    sync::mpsc,
    time::Duration,
};

struct Options {
    config: PathBuf,
    result: Option<PathBuf>,
}
fn options() -> Result<Option<Options>, String> {
    let mut config_path = None;
    let mut result = None;
    let mut args = std::env::args_os().skip(1);
    while let Some(arg) = args.next() {
        match arg.to_str() {
            Some("--help" | "-h") => {
                let checkout = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
                let bash_wrapper = display_path(&checkout.join("shell/qnav.bash"));
                let zsh_wrapper = display_path(&checkout.join("shell/qnav.zsh"));
                let help = format!(
                    "qnav — terminal directory columns\n\nUsage: qnav [--config PATH]\n\nEnter changes the CALLING shell's directory, so it needs a shell function.\ncargo install installs the binary only; it does not set up your shell.\nAfter leaving qnav with Ctrl-C, source ONE wrapper for your shell:\n  Bash wrapper: {bash_wrapper}\n  Zsh wrapper:  {zsh_wrapper}\nUse: source '<absolute wrapper path>', then run qnav again.\nThese files are in this build's source checkout; if moved, use its new location.\nWithout shell setup, browsing, hd, pane, mkdir and git init still work.\n\n↑↓ select  ←→ columns  [ ] depth (1–9, default 3)  type to filter\nTab browse match  Esc clear/cancel  Ctrl-C exit\nMouse: left-click selects; wheel moves selection in hovered list\nEnter cd  Ctrl-O hd  Ctrl-P Herdr pane\nCtrl-A add root  Ctrl-N mkdir  Ctrl-G git init\nCtrl-R refresh branch  Ctrl-L reload all roots\n\nNo automatic roots, profile edits, background daemon, or symlink traversal."
                );
                println!(
                    "{}",
                    help.replace(
                        "Ctrl-O hd  Ctrl-P Herdr pane",
                        "Ctrl-O hd  Ctrl-L branch workspace  Ctrl-P Herdr pane",
                    )
                    .replace(
                        "Without shell setup, browsing, hd, pane, mkdir and git init still work.",
                        "Without shell setup, browsing, hd, branch workspace, pane, mkdir and git init still work.",
                    )
                    .replace(
                        "Ctrl-R refresh branch  Ctrl-L reload all roots",
                        "Ctrl-R refresh branch  Ctrl-F reload roots",
                    )
                );
                return Ok(None);
            }
            Some("--version" | "-V") => {
                println!("qnav {}", env!("CARGO_PKG_VERSION"));
                return Ok(None);
            }
            Some("--config") => {
                config_path = Some(PathBuf::from(args.next().ok_or("--config needs a path")?))
            }
            Some("--result-file") => {
                result = Some(PathBuf::from(
                    args.next().ok_or("--result-file needs a path")?,
                ))
            }
            _ => return Err(format!("unknown argument: {}", display_os(&arg))),
        }
    }
    Ok(Some(Options {
        config: config_path.map(Ok).unwrap_or_else(config::default_path)?,
        result,
    }))
}

struct TerminalGuard;
impl TerminalGuard {
    fn enter() -> io::Result<Self> {
        let guard = Self;
        enable_raw_mode()?;
        execute!(
            io::stdout(),
            EnterAlternateScreen,
            EnableBracketedPaste,
            EnableMouseCapture,
            Hide
        )?;
        Ok(guard)
    }
}
fn restore_terminal() {
    let _ = disable_raw_mode();
    let _ = execute!(
        io::stdout(),
        EndSynchronizedUpdate,
        DisableMouseCapture,
        DisableBracketedPaste,
        Show,
        LeaveAlternateScreen
    );
    let _ = io::stdout().flush();
}
impl Drop for TerminalGuard {
    fn drop(&mut self) {
        restore_terminal();
    }
}

enum ConfigReply {
    Added(Result<(Vec<RootEntry>, RootEntry), String>),
    Reloaded(Result<Vec<RootEntry>, String>),
}
struct Runtime {
    app: App,
    actions: Actions,
    scanner: Scanner,
    options: Options,
    cwd: PathBuf,
    home: Option<PathBuf>,
    config_tx: mpsc::Sender<ConfigReply>,
    config_rx: mpsc::Receiver<ConfigReply>,
    config_busy: bool,
    exiting: bool,
    exit_code: i32,
    cd_path: Option<PathBuf>,
    uncertain: Vec<ActionIdentity>,
    retry_confirm: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ActionIdentity {
    kind: ActionKind,
    target: PathBuf,
    branch: Option<OsString>,
}

impl ActionIdentity {
    fn from_request(action: &ActionRequest) -> Self {
        Self {
            kind: action.kind,
            target: action.target.clone(),
            branch: action.branch.clone(),
        }
    }
}
impl Runtime {
    fn new(options: Options) -> Result<Self, String> {
        let cwd = std::env::current_dir().map_err(|e| e.to_string())?;
        let home = std::env::var_os("HOME").map(PathBuf::from);
        let policy = ExplorationPolicy { home: home.clone() };
        let loaded = config::load(&options.config);
        let mut app = App::new(loaded.as_ref().cloned().unwrap_or_default(), policy.clone());
        if let Err(e) = loaded {
            app.status = format!("Config error (original file preserved): {e}");
        }
        let (config_tx, config_rx) = mpsc::channel();
        Ok(Self {
            app,
            actions: Actions::new(),
            scanner: Scanner::new(policy),
            options,
            cwd,
            home,
            config_tx,
            config_rx,
            config_busy: false,
            exiting: false,
            exit_code: 0,
            cd_path: None,
            uncertain: Vec::new(),
            retry_confirm: false,
        })
    }
    fn key(&mut self, key: KeyEvent) {
        if key.kind == KeyEventKind::Release {
            return;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            self.exiting = true;
            self.exit_code = 130;
            self.app.add_root = None;
            self.app.cancel_branch_picker();
            if let Some(a) = &mut self.app.pending_action {
                match a.phase {
                    ActionPhase::Input(_) | ActionPhase::Confirm { .. } => {
                        self.app.pending_action = None
                    }
                    _ => {
                        a.cancel.cancel();
                        a.phase = ActionPhase::Cancelling;
                    }
                }
            }
            return;
        }
        if self.app.branch_picker.is_some() {
            match key.code {
                KeyCode::Esc => {
                    if self.app.cancel_branch_picker() {
                        self.app.status = "Branch picker cancelled".into();
                    }
                }
                KeyCode::Up => {
                    self.app.move_branch_picker(false);
                }
                KeyCode::Down => {
                    self.app.move_branch_picker(true);
                }
                KeyCode::Backspace | KeyCode::Delete => {
                    let mut query = self
                        .app
                        .branch_picker
                        .as_ref()
                        .map(|picker| picker.query.clone())
                        .unwrap_or_default();
                    query.pop();
                    self.app.edit_branch_picker(query);
                }
                KeyCode::Enter => self.select_branch_workspace(),
                KeyCode::Char(c)
                    if !key
                        .modifiers
                        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
                {
                    let mut query = self
                        .app
                        .branch_picker
                        .as_ref()
                        .map(|picker| picker.query.clone())
                        .unwrap_or_default();
                    query.push(c);
                    self.app.edit_branch_picker(query);
                }
                _ => {}
            }
            return;
        }
        if self.exiting || self.config_busy {
            return;
        }
        if self.app.add_root.is_some() {
            match key.code {
                KeyCode::Esc => self.app.add_root = None,
                KeyCode::Enter => {
                    self.config_busy = true;
                    self.app.status = "Saving root…".into();
                    let tx = self.config_tx.clone();
                    let file = self.options.config.clone();
                    let input = self.app.add_root.clone().unwrap();
                    let cwd = self.cwd.clone();
                    let home = self.home.clone();
                    std::thread::spawn(move || {
                        let _ = tx.send(ConfigReply::Added(config::add_root(
                            &file,
                            &input,
                            &cwd,
                            home.as_deref(),
                        )));
                    });
                }
                KeyCode::Backspace => {
                    self.app.add_root.as_mut().unwrap().pop();
                }
                KeyCode::Char(c)
                    if !key
                        .modifiers
                        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
                {
                    self.app.add_root.as_mut().unwrap().push(c)
                }
                _ => {}
            }
            return;
        }
        if let Some(a) = &mut self.app.pending_action {
            match &mut a.phase {
                ActionPhase::Input(input) => {
                    match key.code {
                        KeyCode::Esc => self.app.cancel_action(),
                        KeyCode::Enter => {
                            a.child_name = Some(input.clone());
                            self.run_action();
                        }
                        KeyCode::Backspace => {
                            input.pop();
                        }
                        KeyCode::Char(c)
                            if !key
                                .modifiers
                                .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
                        {
                            input.push(c)
                        }
                        _ => {}
                    }
                    return;
                }
                ActionPhase::Confirm { accepted, .. } => {
                    match key.code {
                        KeyCode::Esc => {
                            self.app.cancel_action();
                            self.retry_confirm = false;
                        }
                        KeyCode::Left | KeyCode::Right | KeyCode::Tab => *accepted = !*accepted,
                        KeyCode::Enter => {
                            if !*accepted {
                                self.app.cancel_action();
                                self.retry_confirm = false;
                            } else if self.retry_confirm {
                                self.retry_confirm = false;
                                self.check_action();
                            } else {
                                self.run_action();
                            }
                        }
                        _ => {}
                    }
                    return;
                }
                ActionPhase::Checking if key.code == KeyCode::Esc => {
                    self.app.cancel_action();
                    return;
                }
                ActionPhase::Running | ActionPhase::Cancelling if key.code == KeyCode::Esc => {
                    return;
                }
                _ => {}
            }
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            match key.code {
                KeyCode::Char('a') if !self.app.scope_locked() => {
                    self.app.add_root = Some(String::new())
                }
                KeyCode::Char('r') => self.app.refresh_branch(),
                KeyCode::Char('f') if !self.app.scope_locked() => {
                    self.config_busy = true;
                    self.app.status = "Reloading roots…".into();
                    let tx = self.config_tx.clone();
                    let path = self.options.config.clone();
                    std::thread::spawn(move || {
                        let _ = tx.send(ConfigReply::Reloaded(config::load(&path)));
                    });
                }
                KeyCode::Char('n') => self.start_action(ActionKind::Mkdir),
                KeyCode::Char('g') => self.start_action(ActionKind::GitInit),
                KeyCode::Char('o') => self.start_action(ActionKind::OpenHd),
                KeyCode::Char('p') => self.start_action(ActionKind::OpenPane),
                KeyCode::Char('l') => self.start_branch_picker(),
                _ => {}
            }
            return;
        }
        if key.modifiers.contains(KeyModifiers::ALT) {
            return;
        }
        match key.code {
            KeyCode::Up => self.app.move_vertical(false),
            KeyCode::Down => self.app.move_vertical(true),
            KeyCode::Left => self.app.move_horizontal(false),
            KeyCode::Right => self.app.move_horizontal(true),
            KeyCode::Char('[') => self.app.change_depth(false),
            KeyCode::Char(']') => self.app.change_depth(true),
            KeyCode::Esc => self.app.clear_filter(false),
            KeyCode::Tab => self.app.clear_filter(true),
            KeyCode::Backspace | KeyCode::Delete => {
                let mut q = self.app.query().to_owned();
                q.pop();
                self.app.edit_query(q);
            }
            KeyCode::Enter => {
                if self.options.result.is_none() {
                    self.app.status =
                        "Enter needs shell setup; Ctrl-C, then command qnav --help".into();
                } else {
                    self.start_action(ActionKind::ChangeDirectory);
                }
            }
            KeyCode::Char(c) if !c.is_control() => {
                let mut q = self.app.query().to_owned();
                q.push(c);
                self.app.edit_query(q);
            }
            _ => {}
        }
    }
    fn mouse(&mut self, mouse: MouseEvent, hits: &ui::HitMap) -> bool {
        if self.exiting
            || self.config_busy
            || self.app.add_root.is_some()
            || self.app.branch_picker.is_some()
            || self.app.pending_action.as_ref().is_some_and(|action| {
                matches!(
                    action.phase,
                    ActionPhase::Input(_) | ActionPhase::Confirm { .. } | ActionPhase::Cancelling
                )
            })
            || !mouse.modifiers.is_empty()
        {
            return false;
        }
        let Some((list, key)) = hits.hit(mouse.column, mouse.row) else {
            return false;
        };
        match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                let Some(key) = key else { return false };
                match list {
                    ui::MouseList::Column(column) => self.app.select_column_row(column, key),
                    ui::MouseList::Matches => self.app.select_match(key),
                }
            }
            MouseEventKind::ScrollDown | MouseEventKind::ScrollUp => {
                match list {
                    ui::MouseList::Column(column) => {
                        if !self.app.focus_column(column) {
                            return false;
                        }
                    }
                    ui::MouseList::Matches => {
                        if !matches!(self.app.navigation, Navigation::Filter(_)) {
                            return false;
                        }
                    }
                }
                self.app
                    .move_vertical(mouse.kind == MouseEventKind::ScrollDown);
                true
            }
            _ => false,
        }
    }
    fn paste(&mut self, text: String) {
        if self.exiting || self.config_busy {
            return;
        }
        if text.contains('\0') {
            self.app.status = "Paste rejected: NUL is not a valid path character".into();
            return;
        }
        if let Some(input) = &mut self.app.add_root {
            input.push_str(&text);
            return;
        }
        if self.app.branch_picker.is_some() {
            self.app.paste_branch_picker(&text);
            return;
        }
        if let Some(a) = &mut self.app.pending_action {
            if let ActionPhase::Input(input) = &mut a.phase {
                input.push_str(&text);
                return;
            }
            if !matches!(a.phase, ActionPhase::Checking | ActionPhase::Running) {
                return;
            }
        }
        self.app.paste_query(&text);
    }
    fn start_action(&mut self, kind: ActionKind) {
        if self.app.freeze_action(kind).is_none() {
            self.app.status = "Select a valid directory; only one action can run at a time".into();
            return;
        }
        let a = self.app.pending_action.as_mut().unwrap();
        let identity = ActionIdentity::from_request(a);
        if self.uncertain.contains(&identity) {
            a.phase=ActionPhase::Confirm{message:"Previous result was not confirmed and may already have taken effect. Retry may duplicate the operation.".into(),accepted:false};
            self.retry_confirm = true;
        } else {
            self.check_action();
        }
    }
    fn start_branch_picker(&mut self) {
        let Some(id) = self.app.start_branch_picker() else {
            self.app.status =
                "Select a valid directory; only one action or picker can run at a time".into();
            return;
        };
        let picker = self.app.branch_picker.as_ref().unwrap();
        self.app.status = format!(
            "Loading branches: {} · Esc cancel",
            display_path(&picker.target)
        );
        self.actions
            .start_branch_list(id, picker.target.clone(), picker.cancel.clone());
    }
    fn select_branch_workspace(&mut self) {
        let Some(selection) = self.app.take_branch_selection() else {
            if self
                .app
                .branch_picker
                .as_ref()
                .is_some_and(|picker| picker.phase == BranchPickerPhase::Ready)
            {
                self.app.status = "No matching branch selected".into();
            }
            return;
        };
        let branch = display_os(selection.branch.as_os_str());
        if self.app.freeze_branch_action(selection).is_none() {
            self.app.status = "Cannot start branch workspace action".into();
            return;
        }
        let action = self.app.pending_action.as_mut().unwrap();
        let identity = ActionIdentity::from_request(action);
        if self.uncertain.contains(&identity) {
            action.phase = ActionPhase::Confirm {
                message: "Previous result was not confirmed and may already have taken effect. Retry may duplicate the operation.".into(),
                accepted: false,
            };
            self.retry_confirm = true;
        } else {
            self.app.status = format!("Checking branch workspace: {branch} · Esc cancel");
            self.check_action();
        }
    }
    fn check_action(&mut self) {
        let a = self.app.pending_action.as_mut().unwrap();
        a.phase = ActionPhase::Checking;
        self.app.status = format!(
            "Checking {:?}: {} · Esc cancel",
            a.kind,
            display_path(&a.target)
        );
        if a.kind == ActionKind::OpenBranchWorkspace {
            if let Some(branch) = a.branch.clone() {
                self.actions
                    .start_branch_check(a.id, a.target.clone(), branch, a.cancel.clone());
            } else {
                self.app.status = "Branch selection is missing".into();
                self.app.pending_action = None;
            }
        } else {
            self.actions
                .start_check(a.id, a.kind, a.target.clone(), a.cancel.clone());
        }
    }
    fn run_action(&mut self) {
        let a = self.app.pending_action.as_mut().unwrap();
        a.phase = ActionPhase::Running;
        self.app.status = format!("Running {:?}: {}", a.kind, display_path(&a.target));
        if a.kind == ActionKind::OpenBranchWorkspace {
            if let Some(branch) = a.branch.clone() {
                self.actions
                    .start_branch_run(a.id, a.target.clone(), branch, a.cancel.clone());
            } else {
                self.app.status = "Branch selection is missing".into();
                self.app.pending_action = None;
            }
        } else {
            self.actions.start_run(
                a.id,
                a.kind,
                a.target.clone(),
                a.child_name.clone(),
                a.cancel.clone(),
            );
        }
    }
    fn action_event(&mut self, event: ActionEvent) {
        if self
            .app
            .branch_picker
            .as_ref()
            .is_some_and(|picker| picker.id == event.id)
        {
            match event.outcome {
                ActionOutcome::BranchesLoaded { branches } => {
                    self.app.set_branch_picker_entries(event.id, branches);
                    self.app.status = "Select branch · ↑↓ select · Enter open · Esc cancel".into();
                }
                ActionOutcome::InvalidTarget { message } => {
                    let picker = self.app.branch_picker.take().unwrap();
                    self.app.mark_unavailable(&picker.target, &message);
                    self.app.status = format!(
                        "Target unavailable: {} — {message}",
                        display_path(&picker.target)
                    );
                }
                ActionOutcome::Failed { message, .. } => {
                    self.app.branch_picker = None;
                    self.app.status = message;
                }
                ActionOutcome::Cancelled { .. } => {
                    self.app.branch_picker = None;
                    self.app.status = "Branch picker cancelled".into();
                }
                ActionOutcome::Checked { .. } | ActionOutcome::Completed { .. } => {
                    self.app.branch_picker = None;
                    self.app.status = "Branch picker received an unexpected result".into();
                }
            }
            return;
        }
        let Some(a) = self.app.pending_action.as_ref() else {
            return;
        };
        if a.id != event.id {
            return;
        }
        if matches!(a.phase, ActionPhase::Cancelling)
            && !matches!(event.outcome, ActionOutcome::Cancelled { .. })
        {
            // A completed result queued just before cancel is itself evidence the
            // worker has reaped the child; do not apply its business result.
            if matches!(
                event.outcome,
                ActionOutcome::Completed { .. }
                    | ActionOutcome::InvalidTarget { .. }
                    | ActionOutcome::Failed { .. }
                    | ActionOutcome::Checked { .. }
            ) {
                if matches!(
                    event.outcome,
                    ActionOutcome::Completed { .. }
                        | ActionOutcome::Failed {
                            uncertain: true,
                            ..
                        }
                ) {
                    let identity = ActionIdentity::from_request(a);
                    if !self.uncertain.contains(&identity) {
                        self.uncertain.push(identity);
                    }
                }
                self.app.status =
                    "Cancelled; an already-started operation may have taken effect".into();
                self.app.pending_action = None;
            }
            return;
        }
        match event.outcome {
            ActionOutcome::BranchesLoaded { .. } => {
                self.app.pending_action = None;
                self.app.status = "Branch list arrived after the picker closed".into();
            }
            ActionOutcome::InvalidTarget { message } => {
                let a = self.app.pending_action.take().unwrap();
                self.app.mark_unavailable(&a.target, &message);
                self.app.status = format!(
                    "Target unavailable: {} — {message}",
                    display_path(&a.target)
                );
            }
            ActionOutcome::Checked {
                confirmation,
                already_done,
            } => {
                if already_done {
                    self.app.pending_action = None;
                    self.app.status = "Already initialized at this repository root".into();
                    return;
                }
                let a = self.app.pending_action.as_mut().unwrap();
                if a.kind == ActionKind::Mkdir {
                    a.phase = ActionPhase::Input(String::new());
                } else if let Some(message) = confirmation {
                    a.phase = ActionPhase::Confirm {
                        message,
                        accepted: false,
                    };
                } else if a.kind == ActionKind::ChangeDirectory {
                    self.cd_path = Some(a.target.clone());
                    self.app.pending_action = None;
                    self.exiting = true;
                } else {
                    self.run_action();
                }
            }
            ActionOutcome::Completed { message, created } => {
                let a = self.app.pending_action.take().unwrap();
                if let Some(created) = created {
                    self.app.directory_created(&a, &created, message);
                } else {
                    self.app.status = message;
                }
                self.uncertain
                    .retain(|identity| *identity != ActionIdentity::from_request(&a));
            }
            ActionOutcome::Failed { message, uncertain } => {
                let a = self.app.pending_action.as_mut().unwrap();
                if uncertain {
                    let identity = ActionIdentity::from_request(a);
                    if !self.uncertain.contains(&identity) {
                        self.uncertain.push(identity);
                    }
                }
                self.app.status = format!(
                    "{}{}",
                    if uncertain {
                        "Unconfirmed; may have taken effect. "
                    } else {
                        ""
                    },
                    message
                );
                if a.kind == ActionKind::Mkdir && a.child_name.is_some() && !uncertain {
                    a.phase = ActionPhase::Input(a.child_name.clone().unwrap());
                } else {
                    self.app.pending_action = None;
                }
            }
            ActionOutcome::Cancelled { uncertain } => {
                let a = self.app.pending_action.take().unwrap();
                if uncertain {
                    let identity = ActionIdentity::from_request(&a);
                    if !self.uncertain.contains(&identity) {
                        self.uncertain.push(identity);
                    }
                }
                self.app.status = if uncertain {
                    "Cancelled; unconfirmed, may have taken effect".into()
                } else {
                    "Cancelled".into()
                };
            }
        }
    }
    fn update(&mut self) -> bool {
        let mut changed = false;
        while let Ok(reply) = self.config_rx.try_recv() {
            self.config_busy = false;
            changed = true;
            if self.exiting {
                continue;
            }
            match reply {
                ConfigReply::Added(Ok((roots, root))) => self.app.added_root(roots, root),
                ConfigReply::Reloaded(Ok(roots)) => self.app.reload_roots(roots),
                ConfigReply::Added(Err(e)) | ConfigReply::Reloaded(Err(e)) => {
                    self.app.status = format!("Config error: {e}")
                }
            }
        }
        while let Some(event) = self.actions.try_recv() {
            self.action_event(event);
            changed = true;
        }
        for _ in 0..8 {
            let Some(event) = self.scanner.try_recv() else {
                break;
            };
            changed |= self.app.accept_scan(event);
        }
        if !self.exiting {
            self.app.schedule(&self.scanner);
            changed |= self.app.match_tick();
        }
        changed
    }
}

fn run(options: Options) -> Result<i32, String> {
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        return Err("qnav requires an interactive terminal; use --help for usage".into());
    }
    let mut runtime = Runtime::new(options)?;
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        restore_terminal();
        previous(info);
    }));
    let guard = TerminalGuard::enter().map_err(|e| e.to_string())?;
    let backend = ratatui::backend::CrosstermBackend::new(io::stdout());
    let mut terminal = ratatui::Terminal::new(backend).map_err(|e| e.to_string())?;
    let mut dirty = true;
    let mut hits = ui::HitMap::default();
    loop {
        dirty |= runtime.update();
        if runtime.exiting && runtime.app.pending_action.is_none() && !runtime.config_busy {
            break;
        }
        if dirty {
            execute!(io::stdout(), BeginSynchronizedUpdate).map_err(|e| e.to_string())?;
            terminal
                .draw(|frame| {
                    hits = ui::draw(frame, &runtime.app);
                })
                .map_err(|e| e.to_string())?;
            execute!(io::stdout(), EndSynchronizedUpdate).map_err(|e| e.to_string())?;
            dirty = false;
        }
        let busy = runtime.app.working()
            || runtime.app.pending_action.is_some()
            || runtime.app.branch_picker.is_some()
            || runtime.config_busy;
        let wait_ms = if runtime.app.matching_in_progress() {
            0
        } else if busy {
            8
        } else {
            250
        };
        if event::poll(Duration::from_millis(wait_ms)).map_err(|e| e.to_string())? {
            match event::read().map_err(|e| e.to_string())? {
                Event::Key(key) => runtime.key(key),
                Event::Paste(text) => runtime.paste(text),
                Event::Mouse(mouse) => {
                    dirty |= runtime.mouse(mouse, &hits);
                    continue;
                }
                Event::Resize(..) => {}
                _ => continue,
            };
            dirty = true;
        }
    }
    drop(guard);
    if let Some(path) = runtime.cd_path {
        qnav::actions::write_cd_result(runtime.options.result.as_ref().unwrap(), &path)?;
    }
    Ok(runtime.exit_code)
}
fn main() {
    let result = options().and_then(|o| o.map(run).unwrap_or(Ok(0)));
    let code = match result {
        Ok(code) => code,
        Err(e) => {
            eprintln!("qnav: {}", display_os(e.as_ref()));
            1
        }
    };
    std::process::exit(code);
}

#[cfg(test)]
mod tests {
    use super::*;
    fn runtime() -> (Runtime, tempfile::TempDir) {
        let tmp = tempfile::tempdir().unwrap();
        let mut rt = Runtime::new(Options {
            config: tmp.path().join("missing.json"),
            result: None,
        })
        .unwrap();
        rt.actions = Actions::with_environment(std::collections::BTreeMap::new());
        rt.app = App::new(
            vec![
                RootEntry {
                    id: 1,
                    path: "/work/a".into(),
                },
                RootEntry {
                    id: 2,
                    path: "/work/b".into(),
                },
            ],
            ExplorationPolicy { home: None },
        );
        (rt, tmp)
    }
    fn hit_map(rt: &Runtime) -> ui::HitMap {
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 24)).unwrap();
        let mut hits = ui::HitMap::default();
        terminal
            .draw(|frame| hits = ui::draw(frame, &rt.app))
            .unwrap();
        hits
    }
    fn mouse(kind: MouseEventKind) -> MouseEvent {
        MouseEvent {
            kind,
            column: 2,
            row: 3,
            modifiers: KeyModifiers::NONE,
        }
    }
    #[test]
    fn mouse_routes_clicks_and_wheels_but_ignores_motion_release_and_blank_clicks() {
        let (mut rt, _tmp) = runtime();
        let hits = hit_map(&rt);
        for kind in [
            MouseEventKind::Moved,
            MouseEventKind::Up(MouseButton::Left),
            MouseEventKind::Down(MouseButton::Right),
            MouseEventKind::Drag(MouseButton::Left),
        ] {
            assert!(!rt.mouse(mouse(kind), &hits));
            assert_eq!(rt.app.target(), None);
        }
        let click = mouse(MouseEventKind::Down(MouseButton::Left));
        let mut modified = click;
        modified.modifiers = KeyModifiers::CONTROL;
        assert!(!rt.mouse(modified, &hits));
        assert!(rt.mouse(click, &hits));
        assert_eq!(rt.app.target(), Some(NodeKey::root(2)));
        assert!(rt.app.pending_action.is_none());
        assert!(!rt.exiting);
        let mut blank = click;
        blank.row = 4;
        assert!(!rt.mouse(blank, &hits));
        assert!(rt.mouse(mouse(MouseEventKind::ScrollUp), &hits));
        assert_eq!(rt.app.target(), Some(NodeKey::root(1)));
    }
    #[test]
    fn modal_and_busy_states_block_mouse_and_running_actions_keep_their_target() {
        let (mut rt, _tmp) = runtime();
        rt.app.move_vertical(true);
        let hits = hit_map(&rt);
        let click = mouse(MouseEventKind::Down(MouseButton::Left));
        rt.config_busy = true;
        assert!(!rt.mouse(click, &hits));
        rt.config_busy = false;
        rt.app.add_root = Some(String::new());
        assert!(!rt.mouse(click, &hits));
        rt.app.add_root = None;
        rt.app.freeze_action(ActionKind::GitInit).unwrap();
        for phase in [
            ActionPhase::Input(String::new()),
            ActionPhase::Confirm {
                message: "confirm".into(),
                accepted: false,
            },
            ActionPhase::Cancelling,
        ] {
            rt.app.pending_action.as_mut().unwrap().phase = phase;
            assert!(!rt.mouse(click, &hits));
            assert!(!rt.mouse(mouse(MouseEventKind::ScrollDown), &hits));
            assert_eq!(rt.app.target(), Some(NodeKey::root(1)));
        }
        rt.app.pending_action.as_mut().unwrap().phase = ActionPhase::Running;
        assert!(rt.mouse(click, &hits));
        assert_eq!(rt.app.target(), Some(NodeKey::root(2)));
        assert_eq!(
            rt.app.pending_action.as_ref().unwrap().target,
            PathBuf::from("/work/a")
        );
        rt.exiting = true;
        assert!(!rt.mouse(mouse(MouseEventKind::ScrollUp), &hits));
    }
    #[test]
    fn uncertain_operations_remain_guarded_after_another_uncertain_target() {
        let (mut rt, _tmp) = runtime();
        for root in [1, 2] {
            rt.app.navigation = Navigation::Browse(BrowseState::at(&NodeKey::root(root)));
            let id = rt.app.freeze_action(ActionKind::OpenPane).unwrap();
            rt.action_event(ActionEvent {
                id,
                outcome: ActionOutcome::Failed {
                    message: "response lost".into(),
                    uncertain: true,
                },
            });
        }
        rt.app.navigation = Navigation::Browse(BrowseState::at(&NodeKey::root(1)));
        rt.start_action(ActionKind::OpenPane);
        assert!(rt.retry_confirm);
        assert!(matches!(
            rt.app.pending_action.as_ref().unwrap().phase,
            ActionPhase::Confirm {
                accepted: false,
                ..
            }
        ));
    }
    #[test]
    fn late_old_action_never_changes_new_request_and_cancel_reaps_before_unlock() {
        let (mut rt, _tmp) = runtime();
        rt.app.move_vertical(true);
        let old = rt.app.freeze_action(ActionKind::GitInit).unwrap();
        rt.app.cancel_action();
        assert!(rt.app.scope_locked());
        rt.action_event(ActionEvent {
            id: old + 1,
            outcome: ActionOutcome::Completed {
                message: "wrong".into(),
                created: None,
            },
        });
        assert!(rt.app.scope_locked());
        rt.action_event(ActionEvent {
            id: old,
            outcome: ActionOutcome::Cancelled { uncertain: false },
        });
        assert!(!rt.app.scope_locked());
        let new = rt.app.freeze_action(ActionKind::GitInit).unwrap();
        rt.action_event(ActionEvent {
            id: old,
            outcome: ActionOutcome::Checked {
                confirmation: Some("stale".into()),
                already_done: false,
            },
        });
        assert_eq!(rt.app.pending_action.as_ref().unwrap().id, new);
        assert_eq!(
            rt.app.pending_action.as_ref().unwrap().phase,
            ActionPhase::Checking
        );
    }
    #[test]
    fn input_paste_is_literal_and_confirmation_defaults_to_cancel() {
        let (mut rt, _tmp) = runtime();
        rt.app.add_root = Some(String::new());
        rt.paste("a[]\nb".into());
        assert_eq!(rt.app.add_root.as_deref(), Some("a[]\nb"));
        assert_eq!(rt.app.depth, 3);
        rt.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        rt.app.move_vertical(true);
        let id = rt.app.freeze_action(ActionKind::GitInit).unwrap();
        rt.action_event(ActionEvent {
            id,
            outcome: ActionOutcome::Checked {
                confirmation: Some("Confirm".into()),
                already_done: false,
            },
        });
        rt.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(rt.app.pending_action.is_none());
    }
    #[test]
    fn branch_picker_escape_cancels_without_starting_an_action() {
        let (mut rt, _tmp) = runtime();
        rt.app.move_vertical(true);
        let target = rt.app.target();
        assert!(rt.app.start_branch_picker().is_some());
        let cancel = rt.app.branch_picker.as_ref().unwrap().cancel.clone();
        rt.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(cancel.is_cancelled());
        assert!(rt.app.branch_picker.is_none());
        assert!(rt.app.pending_action.is_none());
        assert_eq!(rt.app.target(), target);
    }
    #[test]
    fn branch_picker_shortcut_uses_a_key_outside_herdr_prefix() {
        let (mut rt, _tmp) = runtime();
        rt.app.move_vertical(true);
        rt.key(KeyEvent::new(KeyCode::Char('l'), KeyModifiers::CONTROL));
        assert!(rt.app.branch_picker.is_some());
    }
    #[test]
    fn branch_picker_enter_closes_picker_before_checking_selected_branch() {
        let (mut rt, _tmp) = runtime();
        rt.app.move_vertical(true);
        let id = rt.app.start_branch_picker().unwrap();
        rt.app.set_branch_picker_entries(
            id,
            vec![BranchEntry {
                name: "feature/demo".into(),
                worktree: None,
                stale_worktree: false,
            }],
        );
        rt.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(rt.app.branch_picker.is_none());
        let action = rt.app.pending_action.as_ref().unwrap();
        assert_eq!(action.kind, ActionKind::OpenBranchWorkspace);
        assert_eq!(action.branch.as_deref(), Some("feature/demo".as_ref()));
        assert_eq!(action.phase, ActionPhase::Checking);
    }
    #[test]
    fn uncertain_branch_workspace_requires_confirmation_before_retry() {
        let (mut rt, _tmp) = runtime();
        rt.app.move_vertical(true);
        let source = rt.app.target().unwrap();
        let target = rt.app.path(&source).unwrap();
        let id = rt
            .app
            .freeze_branch_action(BranchSelection {
                source: source.clone(),
                target: target.clone(),
                branch: "feature/demo".into(),
            })
            .unwrap();
        rt.action_event(ActionEvent {
            id,
            outcome: ActionOutcome::Failed {
                message: "workspace result was lost".into(),
                uncertain: true,
            },
        });
        assert!(rt.app.pending_action.is_none());
        let picker_id = rt.app.start_branch_picker().unwrap();
        rt.app.set_branch_picker_entries(
            picker_id,
            vec![BranchEntry {
                name: "feature/demo".into(),
                worktree: None,
                stale_worktree: false,
            }],
        );
        rt.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(matches!(
            rt.app.pending_action.as_ref().map(|action| &action.phase),
            Some(ActionPhase::Confirm {
                accepted: false,
                ..
            })
        ));
        assert!(rt.retry_confirm);
    }
    #[test]
    fn failed_directory_validation_disables_the_cached_target() {
        let (mut rt, _tmp) = runtime();
        rt.app = App::new(
            vec![RootEntry {
                id: 1,
                path: _tmp.path().canonicalize().unwrap().join("missing-target"),
            }],
            ExplorationPolicy { home: None },
        );
        rt.app.move_vertical(true);
        rt.start_action(ActionKind::OpenPane);
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        loop {
            if let Some(event) = rt.actions.try_recv() {
                rt.action_event(event);
                break;
            }
            assert!(std::time::Instant::now() < deadline);
            std::thread::yield_now();
        }
        assert_eq!(rt.app.target(), None);
        assert!(rt.app.freeze_action(ActionKind::OpenPane).is_none());
    }
}
