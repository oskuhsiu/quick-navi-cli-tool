use qnav::{
    actions::{ActionEvent, ActionOutcome, Actions, write_cd_result},
    model::{ActionKind, CancelToken},
};
use std::{
    collections::BTreeMap,
    ffi::OsString,
    fs,
    os::unix::{
        ffi::OsStrExt,
        fs::{PermissionsExt, symlink},
    },
    path::{Path, PathBuf},
    thread,
    time::{Duration, Instant},
};

struct Fixture {
    _temp: tempfile::TempDir,
    root: PathBuf,
    bin: PathBuf,
    environment: BTreeMap<OsString, OsString>,
}
impl Fixture {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(temp.path()).unwrap();
        let bin = root.join("bin");
        fs::create_dir(&bin).unwrap();
        let environment = [
            ("PATH", format!("{}:/usr/bin:/bin", bin.display())),
            ("HOME", root.display().to_string()),
            ("RECORD", root.join("record").display().to_string()),
        ]
        .into_iter()
        .map(|(k, v)| (k.into(), v.into()))
        .collect();
        Self {
            _temp: temp,
            root,
            bin,
            environment,
        }
    }
    fn script(&self, name: &str, body: &str) {
        let path = self.bin.join(name);
        fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
    }
    fn git(&self) {
        self.script("git", "if [ \"$2\" = --local-env-vars ]; then printf '%s\\n' GIT_DIR GIT_WORK_TREE GIT_INDEX_FILE; exit 0; fi\nif [ \"$2\" = --show-toplevel ]; then printf '%s\\n' \"$PWD\"; exit 0; fi\nprintf '%s\\0' \"$PWD\" \"$@\" > \"$RECORD\"");
    }
}
fn wait(actions: &Actions) -> ActionEvent {
    let deadline = Instant::now() + Duration::from_secs(8);
    loop {
        if let Some(event) = actions.try_recv() {
            return event;
        }
        assert!(Instant::now() < deadline, "action did not complete");
        thread::sleep(Duration::from_millis(5));
    }
}
fn checked(actions: &Actions, id: u64, kind: ActionKind, root: &Path) {
    actions.start_check(id, kind, root.into(), CancelToken::default());
    let event = wait(actions);
    assert_eq!(event.id, id);
    assert!(
        matches!(event.outcome, ActionOutcome::Checked { .. }),
        "{:?}",
        event.outcome
    );
}

#[test]
fn shell_result_requires_existing_private_regular_file() {
    let fixture = Fixture::new();
    let result = fixture.root.join("result");
    assert!(write_cd_result(&result, &fixture.root).is_err());
    fs::write(&result, b"old data longer than output").unwrap();
    fs::set_permissions(&result, fs::Permissions::from_mode(0o644)).unwrap();
    assert!(write_cd_result(&result, &fixture.root).is_err());
    assert_eq!(fs::read(&result).unwrap(), b"old data longer than output");
    fs::set_permissions(&result, fs::Permissions::from_mode(0o600)).unwrap();
    let target = fixture.root.join("target\n\n");
    fs::create_dir(&target).unwrap();
    write_cd_result(&result, &target).unwrap();
    let mut expected = b"qnav-v1\0cd\0".to_vec();
    expected.extend_from_slice(target.as_os_str().as_bytes());
    expected.push(0);
    assert_eq!(fs::read(&result).unwrap(), expected);
    let link = fixture.root.join("link");
    symlink(&result, &link).unwrap();
    assert!(write_cd_result(&link, &target).is_err());
    assert!(write_cd_result(&result, Path::new("relative")).is_err());
}

#[test]
fn mkdir_preserves_names_rejects_components_and_rechecks_parent() {
    let fixture = Fixture::new();
    let actions = Actions::with_environment(fixture.environment.clone());
    for name in ["", ".", "..", "a/b", "\0"] {
        actions.start_run(
            1,
            ActionKind::Mkdir,
            fixture.root.clone(),
            Some(name.into()),
            CancelToken::default(),
        );
        assert!(matches!(
            wait(&actions).outcome,
            ActionOutcome::Failed {
                uncertain: false,
                ..
            }
        ));
    }
    let name = " spaced\n[] ";
    actions.start_run(
        2,
        ActionKind::Mkdir,
        fixture.root.clone(),
        Some(name.into()),
        CancelToken::default(),
    );
    assert!(matches!(
        wait(&actions).outcome,
        ActionOutcome::Completed {
            created: Some(_),
            ..
        }
    ));
    assert!(fixture.root.join(name).is_dir());
    actions.start_run(
        3,
        ActionKind::Mkdir,
        fixture.root.clone(),
        Some(name.into()),
        CancelToken::default(),
    );
    assert!(matches!(
        wait(&actions).outcome,
        ActionOutcome::Failed { .. }
    ));
    let parent = fixture.root.join("parent");
    fs::create_dir(&parent).unwrap();
    checked(&actions, 4, ActionKind::Mkdir, &parent);
    fs::remove_dir(&parent).unwrap();
    symlink(&fixture.root, &parent).unwrap();
    actions.start_run(
        4,
        ActionKind::Mkdir,
        parent,
        Some("must-not-exist".into()),
        CancelToken::default(),
    );
    assert!(matches!(
        wait(&actions).outcome,
        ActionOutcome::InvalidTarget { .. }
    ));
    assert!(!fixture.root.join("must-not-exist").exists());
}

#[test]
fn git_guard_rejects_empty_values_and_injection_without_leaking_values() {
    for name in [
        "GIT_DIR",
        "GIT_WORK_TREE",
        "GIT_CONFIG_KEY_0",
        "GIT_CONFIG_VALUE_12",
    ] {
        for value in ["", "secret-do-not-print"] {
            let mut fixture = Fixture::new();
            fixture.git();
            fixture.environment.insert(name.into(), value.into());
            let actions = Actions::with_environment(fixture.environment);
            actions.start_check(
                7,
                ActionKind::GitInit,
                fixture.root.clone(),
                CancelToken::default(),
            );
            match wait(&actions).outcome {
                ActionOutcome::Failed {
                    message,
                    uncertain: false,
                } => {
                    assert!(message.contains(name));
                    assert!(!message.contains("secret-do-not-print"));
                }
                other => panic!("{other:?}"),
            }
            assert!(!fixture.root.join("record").exists());
        }
    }
}

#[test]
fn external_actions_use_fixed_argv_and_checked_identity() {
    let fixture = Fixture::new();
    fixture.git();
    fixture.script("hd", "printf '%s\\0' \"$PWD\" \"$#\" > \"$RECORD\"");
    fixture.script("herdr", "printf '%s\\0' \"$PWD\" \"$@\" > \"$RECORD\"");
    let actions = Actions::with_environment(fixture.environment.clone());
    checked(&actions, 1, ActionKind::OpenHd, &fixture.root);
    actions.start_run(
        1,
        ActionKind::OpenHd,
        fixture.root.clone(),
        None,
        CancelToken::default(),
    );
    assert!(matches!(
        wait(&actions).outcome,
        ActionOutcome::Completed { .. }
    ));
    let record = fs::read(fixture.root.join("record")).unwrap();
    let fields: Vec<_> = record.split(|b| *b == 0).collect();
    assert_eq!(fields[0], fixture.root.as_os_str().as_bytes());
    assert_eq!(fields[1], b"0");
    checked(&actions, 2, ActionKind::OpenPane, &fixture.root);
    actions.start_run(
        2,
        ActionKind::OpenPane,
        fixture.root.clone(),
        None,
        CancelToken::default(),
    );
    assert!(matches!(
        wait(&actions).outcome,
        ActionOutcome::Completed { .. }
    ));
    let record = fs::read(fixture.root.join("record")).unwrap();
    let fields: Vec<_> = record.split(|b| *b == 0).collect();
    assert_eq!(
        &fields[1..7],
        &[
            b"pane".as_slice(),
            b"split",
            b"--current",
            b"--direction",
            b"right",
            b"--cwd"
        ]
    );
    assert_eq!(fields[7], fixture.root.as_os_str().as_bytes());
    assert_eq!(fields[8], b"--focus");
    checked(&actions, 3, ActionKind::OpenPane, &fixture.root);
    actions.start_run(
        4,
        ActionKind::OpenPane,
        fixture.root.clone(),
        None,
        CancelToken::default(),
    );
    assert!(matches!(
        wait(&actions).outcome,
        ActionOutcome::Failed {
            uncertain: false,
            ..
        }
    ));
}

#[test]
fn git_classification_distinguishes_nonrepo_nested_root_and_fatal() {
    let fixture = Fixture::new();
    let actions = Actions::with_environment(fixture.environment.clone());
    // No subprocess is run by constructing the backend.
    assert!(!fixture.root.join("record").exists());
    fixture.git();
    actions.start_check(
        1,
        ActionKind::GitInit,
        fixture.root.clone(),
        CancelToken::default(),
    );
    assert!(matches!(
        wait(&actions).outcome,
        ActionOutcome::Checked {
            already_done: true,
            ..
        }
    ));
    fixture.script("git", "if [ \"$2\" = --local-env-vars ]; then echo GIT_DIR; elif [ \"$2\" = --show-toplevel ]; then dirname \"$PWD\"; else exit 99; fi");
    actions.start_check(
        2,
        ActionKind::GitInit,
        fixture.root.clone(),
        CancelToken::default(),
    );
    match wait(&actions).outcome {
        ActionOutcome::Checked {
            confirmation: Some(s),
            already_done: false,
        } => assert!(s.contains("nested") && s.contains("Outer repository")),
        other => panic!("{other:?}"),
    }
    fixture.script(
        "git",
        "echo 'fatal: not a git repository (or any of the parent directories): .git' >&2; exit 128",
    );
    actions.start_check(
        3,
        ActionKind::GitInit,
        fixture.root.clone(),
        CancelToken::default(),
    );
    assert!(matches!(
        wait(&actions).outcome,
        ActionOutcome::Checked {
            confirmation: Some(_),
            already_done: false
        }
    ));
    actions.start_check(
        4,
        ActionKind::OpenHd,
        fixture.root.clone(),
        CancelToken::default(),
    );
    assert!(matches!(
        wait(&actions).outcome,
        ActionOutcome::Failed {
            uncertain: false,
            ..
        }
    ));
    fixture.script("git", "echo 'fatal: permission denied' >&2; exit 128");
    actions.start_check(
        5,
        ActionKind::GitInit,
        fixture.root.clone(),
        CancelToken::default(),
    );
    assert!(matches!(
        wait(&actions).outcome,
        ActionOutcome::Failed {
            uncertain: false,
            ..
        }
    ));
}

#[test]
fn cancellation_reaps_group_and_output_is_bounded_and_escaped() {
    let fixture = Fixture::new();
    fixture.git();
    fixture.script("hd", "trap '' TERM\nprintf '%s' \"$$\" > \"$RECORD\"\nwhile :; do printf '\x1b[31m1234567890123456789012345678901234567890'; done");
    let actions = Actions::with_environment(fixture.environment.clone());
    checked(&actions, 1, ActionKind::OpenHd, &fixture.root);
    let cancel = CancelToken::default();
    actions.start_run(
        1,
        ActionKind::OpenHd,
        fixture.root.clone(),
        None,
        cancel.clone(),
    );
    let deadline = Instant::now() + Duration::from_secs(3);
    while !fixture.root.join("record").exists() {
        assert!(Instant::now() < deadline);
        thread::sleep(Duration::from_millis(5));
    }
    let pid: i32 = fs::read_to_string(fixture.root.join("record"))
        .unwrap()
        .parse()
        .unwrap();
    cancel.cancel();
    assert!(matches!(
        wait(&actions).outcome,
        ActionOutcome::Cancelled { uncertain: true }
    ));
    assert_eq!(unsafe { libc::kill(pid, 0) }, -1);
    fixture.script("hd", "i=0; while [ \"$i\" -lt 20000 ]; do printf '\x1b[31m1234567890'; printf 'stderr' >&2; i=$((i+1)); done");
    checked(&actions, 2, ActionKind::OpenHd, &fixture.root);
    actions.start_run(
        2,
        ActionKind::OpenHd,
        fixture.root.clone(),
        None,
        CancelToken::default(),
    );
    match wait(&actions).outcome {
        ActionOutcome::Completed { message, .. } => {
            assert!(message.contains("truncated"));
            assert!(!message.contains('\x1b'));
            assert!(message.contains("\\u{1b}"));
            assert!(message.len() < 500_000);
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn check_timeout_and_exited_leader_pipe_descendant_are_bounded() {
    let fixture = Fixture::new();
    fixture.script("git", "trap '' TERM\nwhile :; do sleep 1; done");
    let actions = Actions::with_environment(fixture.environment.clone());
    let start = Instant::now();
    actions.start_check(
        1,
        ActionKind::GitInit,
        fixture.root.clone(),
        CancelToken::default(),
    );
    assert!(matches!(
        wait(&actions).outcome,
        ActionOutcome::Failed {
            uncertain: false,
            ..
        }
    ));
    assert!(start.elapsed() < Duration::from_secs(5));
    fixture.git();
    fixture.script("hd", "(trap '' TERM; sleep 30) &\nexit 0");
    checked(&actions, 2, ActionKind::OpenHd, &fixture.root);
    let start = Instant::now();
    actions.start_run(
        2,
        ActionKind::OpenHd,
        fixture.root.clone(),
        None,
        CancelToken::default(),
    );
    assert!(matches!(
        wait(&actions).outcome,
        ActionOutcome::Completed { .. }
    ));
    assert!(start.elapsed() < Duration::from_secs(2));
}

#[test]
fn real_git_initialization_and_nested_precheck_stay_in_fixture() {
    let fixture = Fixture::new();
    let actions = Actions::with_environment(fixture.environment.clone());
    actions.start_check(
        1,
        ActionKind::GitInit,
        fixture.root.clone(),
        CancelToken::default(),
    );
    let event = wait(&actions);
    assert!(
        matches!(
            event.outcome,
            ActionOutcome::Checked {
                confirmation: Some(_),
                already_done: false
            }
        ),
        "{:?}",
        event.outcome
    );
    assert!(!fixture.root.join(".git").exists());
    actions.start_run(
        1,
        ActionKind::GitInit,
        fixture.root.clone(),
        None,
        CancelToken::default(),
    );
    assert!(matches!(
        wait(&actions).outcome,
        ActionOutcome::Completed { .. }
    ));
    assert!(fixture.root.join(".git").is_dir());
    let nested = fixture.root.join("nested");
    fs::create_dir(&nested).unwrap();
    actions.start_check(
        2,
        ActionKind::GitInit,
        nested.clone(),
        CancelToken::default(),
    );
    assert!(matches!(
        wait(&actions).outcome,
        ActionOutcome::Checked {
            confirmation: Some(_),
            already_done: false
        }
    ));
    assert!(!nested.join(".git").exists());
}

#[test]
fn cancelled_precheck_is_reaped_and_failed_probe_never_authorizes_run() {
    let fixture = Fixture::new();
    fixture.script(
        "git",
        "printf '%s' \"$$\" > \"$RECORD\"; trap '' TERM; while :; do sleep 1; done",
    );
    let actions = Actions::with_environment(fixture.environment.clone());
    let cancel = CancelToken::default();
    actions.start_check(1, ActionKind::GitInit, fixture.root.clone(), cancel.clone());
    let deadline = Instant::now() + Duration::from_secs(3);
    while !fixture.root.join("record").exists() {
        assert!(Instant::now() < deadline);
        thread::sleep(Duration::from_millis(5));
    }
    let pid: i32 = fs::read_to_string(fixture.root.join("record"))
        .unwrap()
        .parse()
        .unwrap();
    cancel.cancel();
    assert!(matches!(
        wait(&actions).outcome,
        ActionOutcome::Cancelled { uncertain: false }
    ));
    assert_eq!(unsafe { libc::kill(pid, 0) }, -1);
    fixture.script("git", "echo 'secret-do-not-print' >&2; exit 42");
    actions.start_check(
        2,
        ActionKind::GitInit,
        fixture.root.clone(),
        CancelToken::default(),
    );
    match wait(&actions).outcome {
        ActionOutcome::Failed {
            message,
            uncertain: false,
        } => assert!(!message.contains("secret-do-not-print")),
        other => panic!("{other:?}"),
    }
    actions.start_run(
        2,
        ActionKind::GitInit,
        fixture.root.clone(),
        None,
        CancelToken::default(),
    );
    assert!(matches!(
        wait(&actions).outcome,
        ActionOutcome::Failed {
            uncertain: false,
            ..
        }
    ));
    let cancel = CancelToken::default();
    cancel.cancel();
    actions.start_run(
        3,
        ActionKind::Mkdir,
        fixture.root.clone(),
        Some("cancelled".into()),
        cancel,
    );
    assert!(matches!(
        wait(&actions).outcome,
        ActionOutcome::Cancelled { uncertain: false }
    ));
    assert!(!fixture.root.join("cancelled").exists());
}

#[test]
fn external_text_interfaces_reject_newlines_and_missing_executables() {
    let mut fixture = Fixture::new();
    let target = fixture.root.join("newline\n");
    fs::create_dir(&target).unwrap();
    fixture
        .environment
        .insert("PATH".into(), fixture.bin.as_os_str().into());
    let actions = Actions::with_environment(fixture.environment);
    for kind in [ActionKind::OpenHd, ActionKind::OpenPane] {
        actions.start_check(1, kind, target.clone(), CancelToken::default());
        match wait(&actions).outcome {
            ActionOutcome::Failed {
                message,
                uncertain: false,
            } => assert!(message.contains("CR or LF")),
            other => panic!("{other:?}"),
        }
        actions.start_check(2, kind, fixture.root.clone(), CancelToken::default());
        assert!(matches!(
            wait(&actions).outcome,
            ActionOutcome::Failed {
                uncertain: false,
                ..
            }
        ));
    }
}
