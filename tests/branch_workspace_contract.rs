use qnav::{
    actions::{ActionOutcome, Actions},
    model::{ActionKind, CancelToken},
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

fn git_value(fixture: &Fixture, args: &[&str]) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(&fixture.root)
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().to_string()
}
fn git(fixture: &Fixture, args: &[&str]) {
    let mut full = vec!["-C", fixture.root.to_str().unwrap()];
    full.extend_from_slice(args);
    run_git(&full, &[]);
}
fn write_hd(fixture: &Fixture, script: &str) {
    let hd = fixture._temp.path().join("bin/hd");
    fs::write(&hd, script).unwrap();
    fs::set_permissions(hd, fs::Permissions::from_mode(0o755)).unwrap();
}
fn cleanup_check(
    actions: &Actions,
    fixture: &Fixture,
    kind: ActionKind,
    branch: &str,
) -> ActionOutcome {
    actions.start_branch_cleanup_check(
        40,
        kind,
        fixture.root.clone(),
        branch.into(),
        CancelToken::default(),
    );
    wait(actions).outcome
}
fn cleanup_run(
    actions: &Actions,
    fixture: &Fixture,
    kind: ActionKind,
    branch: &str,
) -> ActionOutcome {
    actions.start_branch_cleanup_run(
        40,
        kind,
        fixture.root.clone(),
        branch.into(),
        CancelToken::default(),
    );
    wait(actions).outcome
}
fn assert_ref_exists(fixture: &Fixture, branch: &str) {
    git(
        fixture,
        &["show-ref", "--verify", &format!("refs/heads/{branch}")],
    );
}
fn assert_safe_failure(outcome: ActionOutcome) {
    assert!(
        matches!(
            outcome,
            ActionOutcome::Failed {
                uncertain: false,
                ..
            }
        ),
        "{outcome:?}"
    );
}
fn assert_checked(outcome: ActionOutcome) -> String {
    match outcome {
        ActionOutcome::Checked {
            confirmation: Some(message),
            already_done: false,
        } => message,
        other => panic!("{other:?}"),
    }
}
fn use_git_only_path(fixture: &mut Fixture) {
    let bin = fixture._temp.path().join("bin");
    fs::remove_file(bin.join("hd")).unwrap();
    let git_path = Command::new("/usr/bin/which").arg("git").output().unwrap();
    assert!(git_path.status.success());
    let git_path = String::from_utf8(git_path.stdout).unwrap();
    let git_path = fs::canonicalize(git_path.trim()).unwrap();
    std::os::unix::fs::symlink(git_path, bin.join("git")).unwrap();
    fixture
        .environment
        .insert("PATH".into(), bin.into_os_string());
}
fn remove_workspace_hd(fixture: &Fixture) {
    write_hd(
        fixture,
        "#!/bin/sh\nprintf '%s\\0' \"$PWD\" \"$@\" > \"$RECORD\"\n[ \"$1\" = rm ] && [ \"$2\" = existing ] && [ \"$#\" = 2 ] || exit 91\nexec git worktree remove \"$WORKSPACE\"\n",
    );
}

#[test]
fn merged_branch_delete_confirms_baseline_and_needs_no_hd() {
    let mut fixture = Fixture::new();
    use_git_only_path(&mut fixture);
    let actions = Actions::with_environment(fixture.environment.clone());
    let confirmation = assert_checked(cleanup_check(
        &actions,
        &fixture,
        ActionKind::DeleteBranch,
        "feature",
    ));
    assert!(confirmation.contains("feature"));
    assert!(confirmation.contains(&git_value(&fixture, &["rev-parse", "HEAD"])));
    assert!(confirmation.contains(&git_value(&fixture, &["symbolic-ref", "HEAD"])));
    assert!(matches!(
        cleanup_run(&actions, &fixture, ActionKind::DeleteBranch, "feature"),
        ActionOutcome::Completed { created: None, .. }
    ));
    let status = Command::new("git")
        .arg("-C")
        .arg(&fixture.root)
        .args(["show-ref", "--verify", "refs/heads/feature"])
        .output()
        .unwrap();
    assert!(!status.status.success());
    assert!(!fixture.record.exists());
}

#[test]
fn delete_rejects_unmerged_even_when_upstream_would_allow_git_dash_d() {
    let fixture = Fixture::new();
    git(&fixture, &["checkout", "-q", "feature"]);
    fs::write(fixture.root.join("new"), "new commit\n").unwrap();
    git(&fixture, &["add", "new"]);
    git(
        &fixture,
        &[
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.com",
            "commit",
            "-qm",
            "unmerged",
        ],
    );
    git(&fixture, &["branch", "upstream", "HEAD"]);
    git(
        &fixture,
        &["branch", "--set-upstream-to=upstream", "feature"],
    );
    git(&fixture, &["checkout", "-q", "-"]);
    let actions = Actions::with_environment(fixture.environment.clone());
    assert_safe_failure(cleanup_check(
        &actions,
        &fixture,
        ActionKind::DeleteBranch,
        "feature",
    ));
    assert_ref_exists(&fixture, "feature");
    assert!(!fixture.record.exists());
    // Git itself allows -d when merged to upstream, even though target HEAD lacks it.
    git(&fixture, &["branch", "-d", "feature"]);
}

#[test]
fn delete_respects_git_upstream_refusal_without_force_fallback() {
    let fixture = Fixture::new();
    let initial = git_value(&fixture, &["rev-parse", "HEAD"]);
    fs::write(fixture.root.join("README"), "updated\n").unwrap();
    git(&fixture, &["add", "README"]);
    git(
        &fixture,
        &[
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.com",
            "commit",
            "-qm",
            "new head",
        ],
    );
    git(&fixture, &["branch", "-f", "feature", "HEAD"]);
    git(&fixture, &["branch", "upstream", &initial]);
    git(
        &fixture,
        &["branch", "--set-upstream-to=upstream", "feature"],
    );
    let actions = Actions::with_environment(fixture.environment.clone());
    assert_checked(cleanup_check(
        &actions,
        &fixture,
        ActionKind::DeleteBranch,
        "feature",
    ));
    assert!(matches!(
        cleanup_run(&actions, &fixture, ActionKind::DeleteBranch, "feature"),
        ActionOutcome::Failed {
            uncertain: true,
            ..
        }
    ));
    assert_ref_exists(&fixture, "feature");
}

#[test]
fn delete_rejects_checked_out_stale_and_symbolic_local_branches() {
    for mode in ["checked-out", "stale", "symbolic"] {
        let fixture = Fixture::new();
        let branch = if mode == "symbolic" {
            git(
                &fixture,
                &["symbolic-ref", "refs/heads/alias", "refs/heads/feature"],
            );
            "alias"
        } else {
            "existing"
        };
        if mode == "stale" {
            fs::remove_dir_all(&fixture.linked).unwrap();
        }
        let actions = Actions::with_environment(fixture.environment.clone());
        assert_safe_failure(cleanup_check(
            &actions,
            &fixture,
            ActionKind::DeleteBranch,
            branch,
        ));
        assert_ref_exists(&fixture, branch);
        assert!(!fixture.record.exists());
    }
}

#[test]
fn delete_revalidates_ref_head_and_exact_request_after_confirmation() {
    for mode in [
        "ref",
        "head",
        "head-name",
        "branch",
        "id",
        "kind",
        "path",
        "disappeared",
    ] {
        let fixture = Fixture::new();
        let actions = Actions::with_environment(fixture.environment.clone());
        assert_checked(cleanup_check(
            &actions,
            &fixture,
            ActionKind::DeleteBranch,
            "feature",
        ));
        if mode == "ref" || mode == "head" {
            fs::write(fixture.root.join("README"), "changed\n").unwrap();
            git(&fixture, &["add", "README"]);
            git(
                &fixture,
                &[
                    "-c",
                    "user.name=Test",
                    "-c",
                    "user.email=test@example.com",
                    "commit",
                    "-qm",
                    "changed",
                ],
            );
            if mode == "ref" {
                git(&fixture, &["branch", "-f", "feature", "HEAD"]);
            }
        } else if mode == "head-name" {
            git(&fixture, &["checkout", "-q", "new-branch"]);
        } else if mode == "disappeared" {
            git(&fixture, &["branch", "-d", "feature"]);
        }
        let path = if mode == "path" {
            fixture.linked.clone()
        } else {
            fixture.root.clone()
        };
        actions.start_branch_cleanup_run(
            if mode == "id" { 41 } else { 40 },
            if mode == "kind" {
                ActionKind::RemoveBranchWorkspace
            } else {
                ActionKind::DeleteBranch
            },
            path,
            if mode == "branch" {
                "new-branch"
            } else {
                "feature"
            }
            .into(),
            CancelToken::default(),
        );
        assert_safe_failure(wait(&actions).outcome);
        if mode != "disappeared" {
            assert_ref_exists(&fixture, "feature");
        }
        assert!(!fixture.record.exists());
    }
}

#[test]
fn workspace_remove_uses_exact_hd_arguments_and_original_nested_cwd_and_keeps_branch() {
    let mut fixture = Fixture::new();
    let nested = fixture.root.join("nested");
    fs::create_dir(&nested).unwrap();
    fixture
        .environment
        .insert("WORKSPACE".into(), fixture.linked.as_os_str().into());
    remove_workspace_hd(&fixture);
    let actions = Actions::with_environment(fixture.environment.clone());
    actions.start_branch_cleanup_check(
        40,
        ActionKind::RemoveBranchWorkspace,
        nested.clone(),
        "existing".into(),
        CancelToken::default(),
    );
    let message = assert_checked(wait(&actions).outcome);
    assert!(message.contains(&fixture.linked.to_string_lossy().to_string()));
    actions.start_branch_cleanup_run(
        40,
        ActionKind::RemoveBranchWorkspace,
        nested.clone(),
        "existing".into(),
        CancelToken::default(),
    );
    assert!(matches!(
        wait(&actions).outcome,
        ActionOutcome::Completed { .. }
    ));
    let bytes = fs::read(&fixture.record).unwrap();
    let fields: Vec<_> = bytes.split(|byte| *byte == 0).collect();
    assert_eq!(
        fields,
        vec![nested.as_os_str().as_bytes(), b"rm", b"existing", b""]
    );
    assert!(!fixture.linked.exists());
    assert!(
        !git_value(&fixture, &["worktree", "list", "--porcelain"])
            .contains(fixture.linked.to_str().unwrap())
    );
    assert_ref_exists(&fixture, "existing");
}

#[test]
fn workspace_remove_rejects_primary_selected_subdir_locked_stale_and_ambiguous() {
    for mode in [
        "primary",
        "selected-subdir",
        "locked",
        "stale",
        "ambiguous",
        "detached-primary",
    ] {
        let fixture = Fixture::new();
        let mut target = fixture.root.clone();
        let mut branch = "existing".to_string();
        match mode {
            "primary" => branch = git_value(&fixture, &["branch", "--show-current"]),
            "selected-subdir" => {
                target = fixture.linked.join("subdir");
                fs::create_dir(&target).unwrap();
            }
            "locked" => git(
                &fixture,
                &["worktree", "lock", fixture.linked.to_str().unwrap()],
            ),
            "stale" => fs::remove_dir_all(&fixture.linked).unwrap(),
            "ambiguous" => {
                let other = fixture._temp.path().join("other");
                git(
                    &fixture,
                    &[
                        "worktree",
                        "add",
                        "--force",
                        "-q",
                        other.to_str().unwrap(),
                        "existing",
                    ],
                );
            }
            "detached-primary" => {
                git(&fixture, &["checkout", "--detach", "-q"]);
                // The detached first record must still keep the linked worktree at index 1.
                let actions = Actions::with_environment(fixture.environment.clone());
                assert_checked(cleanup_check(
                    &actions,
                    &fixture,
                    ActionKind::RemoveBranchWorkspace,
                    "existing",
                ));
                continue;
            }
            _ => unreachable!(),
        }
        let actions = Actions::with_environment(fixture.environment.clone());
        actions.start_branch_cleanup_check(
            40,
            ActionKind::RemoveBranchWorkspace,
            target,
            branch.into(),
            CancelToken::default(),
        );
        assert_safe_failure(wait(&actions).outcome);
        assert!(!fixture.record.exists());
    }
}

#[test]
fn workspace_remove_protects_tracked_untracked_and_ignored_files() {
    for mode in ["tracked", "untracked", "ignored"] {
        let fixture = Fixture::new();
        if mode == "tracked" {
            fs::write(fixture.linked.join("README"), "changed\n").unwrap();
        } else {
            if mode == "ignored" {
                fs::write(fixture.root.join(".git/info/exclude"), "build\n").unwrap();
            }
            fs::write(
                fixture.linked.join(if mode == "ignored" {
                    "build"
                } else {
                    "untracked"
                }),
                "preserve\n",
            )
            .unwrap();
        }
        let actions = Actions::with_environment(fixture.environment.clone());
        assert_safe_failure(cleanup_check(
            &actions,
            &fixture,
            ActionKind::RemoveBranchWorkspace,
            "existing",
        ));
        assert!(fixture.linked.exists());
        assert!(!fixture.record.exists());
    }
}

#[test]
fn workspace_remove_revalidates_dirty_path_ref_and_repository_changes() {
    for mode in ["dirty", "path", "ref", "repository"] {
        let fixture = Fixture::new();
        let actions = Actions::with_environment(fixture.environment.clone());
        assert_checked(cleanup_check(
            &actions,
            &fixture,
            ActionKind::RemoveBranchWorkspace,
            "existing",
        ));
        match mode {
            "dirty" => fs::write(fixture.linked.join("precious"), "preserve\n").unwrap(),
            "path" => {
                let moved = fixture._temp.path().join("moved");
                git(
                    &fixture,
                    &[
                        "worktree",
                        "move",
                        fixture.linked.to_str().unwrap(),
                        moved.to_str().unwrap(),
                    ],
                );
            }
            "ref" => run_git(
                &[
                    "-C",
                    fixture.linked.to_str().unwrap(),
                    "symbolic-ref",
                    "HEAD",
                    "refs/heads/feature",
                ],
                &[],
            ),
            "repository" => {
                fs::rename(&fixture.root, fixture._temp.path().join("saved")).unwrap();
                run_git(&["init", "-q", fixture.root.to_str().unwrap()], &[]);
            }
            _ => unreachable!(),
        }
        assert_safe_failure(cleanup_run(
            &actions,
            &fixture,
            ActionKind::RemoveBranchWorkspace,
            "existing",
        ));
        assert!(!fixture.record.exists());
    }
}

#[test]
fn workspace_tool_missing_noop_partial_failure_and_cancellation_are_reported() {
    for mode in ["missing", "noop", "partial", "cancel"] {
        let mut fixture = Fixture::new();
        fixture
            .environment
            .insert("WORKSPACE".into(), fixture.linked.as_os_str().into());
        match mode {
            "missing" => use_git_only_path(&mut fixture),
            "partial" => write_hd(
                &fixture,
                "#!/bin/sh\nprintf started > \"$RECORD\"\ngit worktree remove \"$WORKSPACE\"\nexit 2\n",
            ),
            "cancel" => write_hd(
                &fixture,
                "#!/bin/sh\nprintf started > \"$RECORD\"\nsleep 20\n",
            ),
            _ => {}
        }
        let actions = Actions::with_environment(fixture.environment.clone());
        let outcome = cleanup_check(
            &actions,
            &fixture,
            ActionKind::RemoveBranchWorkspace,
            "existing",
        );
        if mode == "missing" {
            match outcome {
                ActionOutcome::Failed {
                    message,
                    uncertain: false,
                } => assert!(message.contains("hd")),
                other => panic!("{other:?}"),
            }
            continue;
        }
        assert_checked(outcome);
        let cancel = CancelToken::default();
        actions.start_branch_cleanup_run(
            40,
            ActionKind::RemoveBranchWorkspace,
            fixture.root.clone(),
            "existing".into(),
            cancel.clone(),
        );
        if mode == "cancel" {
            let deadline = Instant::now() + Duration::from_secs(5);
            while !fixture.record.exists() {
                assert!(Instant::now() < deadline);
                thread::sleep(Duration::from_millis(5));
            }
            cancel.cancel();
            assert!(matches!(
                wait(&actions).outcome,
                ActionOutcome::Cancelled { uncertain: true }
            ));
        } else {
            assert!(matches!(
                wait(&actions).outcome,
                ActionOutcome::Failed {
                    uncertain: true,
                    ..
                }
            ));
        }
        assert_ref_exists(&fixture, "existing");
        assert_eq!(fixture.linked.exists(), mode != "partial");
    }
}

#[test]
fn cleanup_rejects_unborn_head_and_git_binding_environment() {
    let fixture = Fixture::new();
    git(&fixture, &["symbolic-ref", "HEAD", "refs/heads/unborn"]);
    let actions = Actions::with_environment(fixture.environment.clone());
    assert_safe_failure(cleanup_check(
        &actions,
        &fixture,
        ActionKind::DeleteBranch,
        "feature",
    ));
    assert_ref_exists(&fixture, "feature");
    for kind in [ActionKind::DeleteBranch, ActionKind::RemoveBranchWorkspace] {
        let mut environment = fixture.environment.clone();
        environment.insert("GIT_WORK_TREE".into(), fixture.linked.as_os_str().into());
        let actions = Actions::with_environment(environment);
        match cleanup_check(&actions, &fixture, kind, "feature") {
            ActionOutcome::Failed {
                message,
                uncertain: false,
            } => assert!(message.contains("Repository-binding")),
            other => panic!("{other:?}"),
        }
    }
    assert!(!fixture.record.exists());
}

#[test]
fn workspace_remove_protects_qnav_process_cwd_without_global_chdir() {
    let fixture = Fixture::new();
    let nested = fixture.linked.join("nested");
    fs::create_dir(&nested).unwrap();
    let output = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "workspace_containing_qnav_cwd_child",
            "--nocapture",
        ])
        .current_dir(&nested)
        .env_clear()
        .envs(&fixture.environment)
        .env("QNAV_CLEANUP_CWD_TEST", &fixture.root)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{} {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(fixture.linked.exists());
    assert!(!fixture.record.exists());
}

#[test]
fn workspace_containing_qnav_cwd_child() {
    let Some(root) = std::env::var_os("QNAV_CLEANUP_CWD_TEST") else {
        return;
    };
    let actions = Actions::with_environment(std::env::vars_os().collect());
    actions.start_branch_cleanup_check(
        40,
        ActionKind::RemoveBranchWorkspace,
        root.into(),
        "existing".into(),
        CancelToken::default(),
    );
    match wait(&actions).outcome {
        ActionOutcome::Failed {
            message,
            uncertain: false,
        } => assert!(message.contains("qnav's current directory"), "{message}"),
        other => panic!("{other:?}"),
    }
}

#[test]
fn deletion_rejects_replaced_repository_with_identical_ref_and_head() {
    let fixture = Fixture::new();
    let actions = Actions::with_environment(fixture.environment.clone());
    assert_checked(cleanup_check(
        &actions,
        &fixture,
        ActionKind::DeleteBranch,
        "feature",
    ));
    let saved = fixture._temp.path().join("saved");
    fs::rename(&fixture.root, &saved).unwrap();
    run_git(
        &[
            "clone",
            "-q",
            saved.to_str().unwrap(),
            fixture.root.to_str().unwrap(),
        ],
        &[],
    );
    git(
        &fixture,
        &["branch", "feature", "refs/remotes/origin/feature"],
    );
    assert_safe_failure(cleanup_run(
        &actions,
        &fixture,
        ActionKind::DeleteBranch,
        "feature",
    ));
    assert_ref_exists(&fixture, "feature");
    assert!(!fixture.record.exists());
}

#[test]
fn delete_rechecks_branch_oid_even_when_head_and_merge_safety_are_unchanged() {
    let fixture = Fixture::new();
    git(
        &fixture,
        &[
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.com",
            "commit",
            "--allow-empty",
            "-qm",
            "new baseline",
        ],
    );
    let actions = Actions::with_environment(fixture.environment.clone());
    assert_checked(cleanup_check(
        &actions,
        &fixture,
        ActionKind::DeleteBranch,
        "feature",
    ));
    git(&fixture, &["branch", "-f", "feature", "HEAD"]);
    match cleanup_run(&actions, &fixture, ActionKind::DeleteBranch, "feature") {
        ActionOutcome::Failed {
            message,
            uncertain: false,
        } => assert!(message.contains("changed since confirmation")),
        other => panic!("{other:?}"),
    }
    assert_ref_exists(&fixture, "feature");
}

#[test]
fn workspace_remove_rechecks_branch_oid_with_a_clean_unchanged_workspace_tree() {
    let fixture = Fixture::new();
    git(
        &fixture,
        &[
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.com",
            "commit",
            "--allow-empty",
            "-qm",
            "new baseline",
        ],
    );
    let actions = Actions::with_environment(fixture.environment.clone());
    assert_checked(cleanup_check(
        &actions,
        &fixture,
        ActionKind::RemoveBranchWorkspace,
        "existing",
    ));
    git(
        &fixture,
        &[
            "update-ref",
            "refs/heads/existing",
            &git_value(&fixture, &["rev-parse", "HEAD"]),
        ],
    );
    match cleanup_run(
        &actions,
        &fixture,
        ActionKind::RemoveBranchWorkspace,
        "existing",
    ) {
        ActionOutcome::Failed {
            message,
            uncertain: false,
        } => assert!(message.contains("changed since confirmation")),
        other => panic!("{other:?}"),
    }
    assert!(fixture.linked.exists());
    assert!(!fixture.record.exists());
}

#[test]
fn workspace_remove_protects_hidden_tracked_changes_and_flags_added_after_confirmation() {
    for flag in ["--assume-unchanged", "--skip-worktree"] {
        for after_confirmation in [false, true] {
            let fixture = Fixture::new();
            let actions = Actions::with_environment(fixture.environment.clone());
            if after_confirmation {
                assert_checked(cleanup_check(
                    &actions,
                    &fixture,
                    ActionKind::RemoveBranchWorkspace,
                    "existing",
                ));
            }
            run_git(
                &[
                    "-C",
                    fixture.linked.to_str().unwrap(),
                    "update-index",
                    flag,
                    "README",
                ],
                &[],
            );
            fs::write(
                fixture.linked.join("README"),
                "hidden changes to preserve\n",
            )
            .unwrap();
            let status = Command::new("git")
                .arg("-C")
                .arg(&fixture.linked)
                .args([
                    "status",
                    "--porcelain",
                    "-z",
                    "--untracked-files=all",
                    "--ignored",
                    "--ignore-submodules=none",
                ])
                .output()
                .unwrap();
            assert!(status.status.success());
            assert!(
                status.stdout.is_empty(),
                "fixture must reproduce Git status hiding changes"
            );
            let outcome = if after_confirmation {
                cleanup_run(
                    &actions,
                    &fixture,
                    ActionKind::RemoveBranchWorkspace,
                    "existing",
                )
            } else {
                cleanup_check(
                    &actions,
                    &fixture,
                    ActionKind::RemoveBranchWorkspace,
                    "existing",
                )
            };
            match outcome {
                ActionOutcome::Failed {
                    message,
                    uncertain: false,
                } => {
                    assert!(message.contains("index flags"), "{message}");
                    assert!(message.contains("preserve changes"), "{message}");
                }
                other => panic!("{other:?}"),
            }
            assert_eq!(
                fs::read_to_string(fixture.linked.join("README")).unwrap(),
                "hidden changes to preserve\n"
            );
            assert_ref_exists(&fixture, "existing");
            assert!(!fixture.record.exists());
        }
    }
}

#[test]
fn workspace_remove_protects_index_flags_inside_initialized_submodules() {
    for flag in ["--assume-unchanged", "--skip-worktree"] {
        let fixture = Fixture::new();
        run_git(
            &[
                "-C",
                fixture.linked.to_str().unwrap(),
                "-c",
                "protocol.file.allow=always",
                "submodule",
                "add",
                "-q",
                fixture.root.to_str().unwrap(),
                "nested",
            ],
            &[],
        );
        run_git(
            &[
                "-C",
                fixture.linked.to_str().unwrap(),
                "-c",
                "user.name=Test",
                "-c",
                "user.email=test@example.com",
                "commit",
                "-qm",
                "add submodule",
            ],
            &[],
        );
        let actions = Actions::with_environment(fixture.environment.clone());
        assert_checked(cleanup_check(
            &actions,
            &fixture,
            ActionKind::RemoveBranchWorkspace,
            "existing",
        ));
        let submodule = fixture.linked.join("nested");
        run_git(
            &[
                "-C",
                submodule.to_str().unwrap(),
                "update-index",
                flag,
                "README",
            ],
            &[],
        );
        fs::write(submodule.join("README"), "hidden submodule changes\n").unwrap();
        let status = Command::new("git")
            .arg("-C")
            .arg(&fixture.linked)
            .args(["status", "--porcelain", "-z", "--ignore-submodules=none"])
            .output()
            .unwrap();
        assert!(status.status.success());
        assert!(
            status.stdout.is_empty(),
            "fixture must reproduce hidden submodule modifications"
        );
        match cleanup_run(
            &actions,
            &fixture,
            ActionKind::RemoveBranchWorkspace,
            "existing",
        ) {
            ActionOutcome::Failed {
                message,
                uncertain: false,
            } => assert!(message.contains("index flags"), "{message}"),
            other => panic!("{other:?}"),
        }
        assert_eq!(
            fs::read_to_string(submodule.join("README")).unwrap(),
            "hidden submodule changes\n"
        );
        assert!(!fixture.record.exists());
    }
}

#[test]
fn workspace_remove_refuses_failed_or_incomplete_index_flag_inspection() {
    for mode in ["failure", "truncated", "malformed"] {
        let mut fixture = Fixture::new();
        let real_git = Command::new("/usr/bin/which").arg("git").output().unwrap();
        assert!(real_git.status.success());
        let real_git = String::from_utf8(real_git.stdout)
            .unwrap()
            .trim()
            .to_string();
        fixture
            .environment
            .insert("REAL_GIT".into(), real_git.into());
        let inspection = match mode {
            "failure" => "exit 7",
            "truncated" => {
                r#"i=0; while [ "$i" -lt 10000 ]; do printf 'H README\0'; i=$((i + 1)); done; exit 0"#
            }
            "malformed" => "printf incomplete; exit 0",
            _ => unreachable!(),
        };
        let wrapper = fixture._temp.path().join("bin/git");
        fs::write(
            &wrapper,
            format!(
                r#"#!/bin/sh
if [ "$1" = ls-files ]; then {inspection}; fi
exec "$REAL_GIT" "$@"
"#
            ),
        )
        .unwrap();
        fs::set_permissions(&wrapper, fs::Permissions::from_mode(0o755)).unwrap();
        let actions = Actions::with_environment(fixture.environment.clone());
        match cleanup_check(
            &actions,
            &fixture,
            ActionKind::RemoveBranchWorkspace,
            "existing",
        ) {
            ActionOutcome::Failed {
                message,
                uncertain: false,
            } => assert!(
                message.contains("Cannot inspect workspace index flags"),
                "{message}"
            ),
            other => panic!("{other:?}"),
        }
        assert!(fixture.linked.exists());
        assert!(!fixture.record.exists());
    }
}
