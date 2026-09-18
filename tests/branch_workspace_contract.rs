use qnav::{
    actions::{ActionOutcome, Actions},
    model::CancelToken,
};
use std::{
    collections::BTreeMap,
    ffi::OsString,
    fs,
    os::unix::{ffi::OsStrExt, fs::PermissionsExt},
    path::PathBuf,
    process::Command,
    thread,
    time::{Duration, Instant},
};

struct Fixture {
    _temp: tempfile::TempDir,
    root: PathBuf,
    linked: PathBuf,
    record: PathBuf,
    environment: BTreeMap<OsString, OsString>,
}

impl Fixture {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let base = fs::canonicalize(temp.path()).unwrap();
        let root = base.join("repo");
        let linked = base.join("linked");
        let bin = base.join("bin");
        let record = base.join("record");
        fs::create_dir(&root).unwrap();
        fs::create_dir(&bin).unwrap();
        run_git(&["init", "-q", root.to_str().unwrap()], &[]);
        fs::write(root.join("README"), "fixture\n").unwrap();
        run_git(&["-C", root.to_str().unwrap(), "add", "README"], &[]);
        run_git(
            &[
                "-C",
                root.to_str().unwrap(),
                "-c",
                "user.name=Test",
                "-c",
                "user.email=test@example.com",
                "commit",
                "-qm",
                "initial",
            ],
            &[],
        );
        run_git(&["-C", root.to_str().unwrap(), "branch", "existing"], &[]);
        run_git(
            &[
                "-C",
                root.to_str().unwrap(),
                "worktree",
                "add",
                "-q",
                linked.to_str().unwrap(),
                "existing",
            ],
            &[],
        );
        run_git(&["-C", root.to_str().unwrap(), "branch", "new-branch"], &[]);
        run_git(&["-C", root.to_str().unwrap(), "branch", "feature"], &[]);
        run_git(&["-C", root.to_str().unwrap(), "tag", "feature"], &[]);
        let inherited_path = std::env::var_os("PATH").unwrap_or_default();
        let path = format!("{}:{}", bin.display(), inherited_path.to_string_lossy());
        let environment = [
            ("PATH".into(), path.into()),
            ("HOME".into(), base.as_os_str().into()),
            ("RECORD".into(), record.as_os_str().into()),
            ("GIT_CONFIG_NOSYSTEM".into(), "1".into()),
        ]
        .into_iter()
        .collect();
        let hd = bin.join("hd");
        fs::write(
            &hd,
            "#!/bin/sh\nprintf '%s\\0' \"$PWD\" \"$@\" > \"$RECORD\"\n",
        )
        .unwrap();
        fs::set_permissions(hd, fs::Permissions::from_mode(0o755)).unwrap();
        Self {
            _temp: temp,
            root,
            linked,
            record,
            environment,
        }
    }
}

fn run_git(args: &[&str], env: &[(&str, &str)]) {
    let mut command = Command::new("git");
    command.args(args);
    for (name, value) in env {
        command.env(name, value);
    }
    let output = command.output().unwrap();
    assert!(
        output.status.success(),
        "git {:?}: {}",
        args,
        String::from_utf8_lossy(&output.stderr)
    );
}

fn wait(actions: &Actions) -> qnav::actions::ActionEvent {
    let deadline = Instant::now() + Duration::from_secs(8);
    loop {
        if let Some(event) = actions.try_recv() {
            return event;
        }
        assert!(Instant::now() < deadline, "action did not complete");
        thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn branch_listing_reports_worktrees_and_dispatches_open_or_new() {
    let fixture = Fixture::new();
    let actions = Actions::with_environment(fixture.environment.clone());
    actions.start_branch_list(1, fixture.root.clone(), CancelToken::default());
    let event = wait(&actions);
    let branches = match event.outcome {
        ActionOutcome::BranchesLoaded { branches } => branches,
        other => panic!("{other:?}"),
    };
    let existing = branches
        .iter()
        .find(|branch| branch.name == "existing")
        .unwrap();
    assert_eq!(existing.worktree.as_deref(), Some(fixture.linked.as_path()));
    let new_branch = branches
        .iter()
        .find(|branch| branch.name == "new-branch")
        .unwrap();
    assert_eq!(new_branch.worktree, None);
    let feature = branches
        .iter()
        .find(|branch| branch.name == "feature")
        .unwrap();
    assert_eq!(feature.worktree, None);
    assert!(!feature.stale_worktree);

    actions.start_branch_check(
        2,
        fixture.root.clone(),
        "existing".into(),
        CancelToken::default(),
    );
    assert!(matches!(
        wait(&actions).outcome,
        ActionOutcome::Checked { .. }
    ));
    actions.start_branch_run(
        2,
        fixture.root.clone(),
        "existing".into(),
        CancelToken::default(),
    );
    assert!(matches!(
        wait(&actions).outcome,
        ActionOutcome::Completed { .. }
    ));
    let fields: Vec<_> = fs::read(&fixture.record)
        .unwrap()
        .split(|byte| *byte == 0)
        .map(|field| field.to_vec())
        .collect();
    assert_eq!(fields[0], fixture.root.as_os_str().as_bytes());
    assert_eq!(fields[1], b"open");
    assert_eq!(fields[2], b"existing");

    actions.start_branch_check(
        3,
        fixture.root.clone(),
        "new-branch".into(),
        CancelToken::default(),
    );
    assert!(matches!(
        wait(&actions).outcome,
        ActionOutcome::Checked { .. }
    ));
    actions.start_branch_run(
        3,
        fixture.root.clone(),
        "new-branch".into(),
        CancelToken::default(),
    );
    assert!(matches!(
        wait(&actions).outcome,
        ActionOutcome::Completed { .. }
    ));
    let fields: Vec<_> = fs::read(&fixture.record)
        .unwrap()
        .split(|byte| *byte == 0)
        .map(|field| field.to_vec())
        .collect();
    assert_eq!(fields[1], b"new");
    assert_eq!(fields[2], b"new-branch");
}

#[test]
fn branch_check_rejects_a_deleted_branch_instead_of_creating_it() {
    let fixture = Fixture::new();
    let actions = Actions::with_environment(fixture.environment.clone());
    actions.start_branch_list(1, fixture.root.clone(), CancelToken::default());
    assert!(matches!(
        wait(&actions).outcome,
        ActionOutcome::BranchesLoaded { .. }
    ));
    run_git(
        &[
            "-C",
            fixture.root.to_str().unwrap(),
            "branch",
            "-D",
            "new-branch",
        ],
        &[],
    );
    actions.start_branch_check(
        2,
        fixture.root.clone(),
        "new-branch".into(),
        CancelToken::default(),
    );
    match wait(&actions).outcome {
        ActionOutcome::Failed {
            message,
            uncertain: false,
        } => assert!(message.contains("no longer exists")),
        other => panic!("{other:?}"),
    }
}

#[test]
fn stale_worktree_metadata_is_visible_and_rejected_before_hd() {
    let fixture = Fixture::new();
    fs::remove_dir_all(&fixture.linked).unwrap();
    let actions = Actions::with_environment(fixture.environment.clone());
    actions.start_branch_list(1, fixture.root.clone(), CancelToken::default());
    let branches = match wait(&actions).outcome {
        ActionOutcome::BranchesLoaded { branches } => branches,
        other => panic!("{other:?}"),
    };
    let stale = branches
        .iter()
        .find(|branch| branch.name == "existing")
        .unwrap();
    assert!(stale.stale_worktree);
    assert_eq!(stale.worktree, None);
    actions.start_branch_check(2, fixture.root, "existing".into(), CancelToken::default());
    match wait(&actions).outcome {
        ActionOutcome::Failed {
            message,
            uncertain: false,
        } => assert!(message.contains("stale worktree metadata")),
        other => panic!("{other:?}"),
    }
    assert!(!fixture.record.exists());
}
