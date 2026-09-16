use std::{
    ffi::OsString,
    fs,
    os::unix::{
        ffi::{OsStrExt, OsStringExt},
        fs::PermissionsExt,
    },
    process::Command,
};

#[test]
fn bash_and_zsh_validate_exact_native_protocol_and_cleanup() {
    for shell in ["/bin/bash", "/bin/zsh"] {
        let temp = tempfile::tempdir().unwrap();
        let base = fs::canonicalize(temp.path()).unwrap();
        let bin = base.join("bin");
        fs::create_dir(&bin).unwrap();
        let results = base.join("results");
        fs::create_dir(&results).unwrap();
        let target = base.join(OsString::from_vec(
            b" -quotes'\";$(touch INJECTED) \n\n".to_vec(),
        ));
        fs::create_dir(&target).unwrap();
        fs::write(bin.join("qnav"), "#!/bin/sh\n[ \"$1\" = --result-file ] || exit 90\n[ \"$3\" = 'argument with spaces' ] || exit 91\nprintf '%s' \"$2\" > \"$RECORD\"\nmode=$(stat -f %Lp \"$2\" 2>/dev/null || stat -c %a \"$2\")\n[ \"$mode\" = 600 ] || exit 92\ncat \"$PAYLOAD\" > \"$2\"\nexit \"$FAKE_EXIT\"\n").unwrap();
        fs::set_permissions(bin.join("qnav"), fs::Permissions::from_mode(0o755)).unwrap();
        let mut valid = b"qnav-v1\0cd\0".to_vec();
        valid.extend_from_slice(target.as_os_str().as_bytes());
        valid.push(0);
        let mut trailing = valid.clone();
        trailing.extend_from_slice(b"x");
        let mut extra = valid.clone();
        extra.push(0);
        let mut incomplete = valid.clone();
        incomplete.pop();
        let mut bad_action = b"qnav-v1\0cd-hd\0".to_vec();
        bad_action.extend_from_slice(target.as_os_str().as_bytes());
        bad_action.push(0);
        let cases = vec![
            (valid.clone(), 0, true),
            (valid.clone(), 130, false),
            (valid, 7, false),
            (trailing, 0, false),
            (extra, 0, false),
            (incomplete, 0, false),
            (bad_action, 0, false),
            (b"qnav-v2\0cd\0/\0".to_vec(), 0, false),
            (b"qnav-v1\0cd\0relative\0".to_vec(), 0, false),
            (Vec::new(), 0, false),
            (
                b"qnav-v1\0cd\0/does-not-exist-qnav-test\0".to_vec(),
                0,
                false,
            ),
        ];
        for (payload, exit, should_cd) in cases {
            let payload_path = base.join("payload");
            fs::write(&payload_path, payload).unwrap();
            let wrapper = format!(
                "{}/shell/qnav.{}",
                env!("CARGO_MANIFEST_DIR"),
                if shell.ends_with("zsh") {
                    "zsh"
                } else {
                    "bash"
                }
            );
            let output = Command::new(shell).args(["-f", "-c", "source \"$WRAPPER\"; qnav 'argument with spaces'; qnav_result=$?; printf '%s\\0%s\\0' \"$qnav_result\" \"$PWD\""])
                .current_dir(&base).env("WRAPPER", wrapper).env("PATH", format!("{}:/usr/bin:/bin", bin.display()))
                .env_remove("BASH_ENV").env_remove("ENV")
                .env("TMPDIR", &results).env("RECORD", base.join("record")).env("PAYLOAD", payload_path).env("FAKE_EXIT", exit.to_string()).output().unwrap();
            assert!(output.status.success(), "{shell}: {:?}", output);
            let fields: Vec<_> = output.stdout.split(|b| *b == 0).collect();
            assert_eq!(
                fields[1],
                if should_cd {
                    target.as_os_str().as_bytes()
                } else {
                    base.as_os_str().as_bytes()
                },
                "{shell}: {:?}",
                output
            );
            if should_cd {
                assert_eq!(fields[0], b"0");
            } else if exit != 0 {
                assert_eq!(fields[0], exit.to_string().as_bytes());
            } else {
                assert_ne!(fields[0], b"0");
            }
            assert_eq!(fs::read_dir(&results).unwrap().count(), 0);
            assert!(!base.join("INJECTED").exists());
        }
        // macOS filesystems reject invalid UTF-8 filenames. Capture the final
        // builtin argv separately to verify lossless transport on this host.
        let native = b"/native-\xff\n\n";
        let mut payload = b"qnav-v1\0cd\0".to_vec();
        payload.extend_from_slice(native);
        payload.push(0);
        fs::write(base.join("payload"), payload).unwrap();
        let wrapper = format!(
            "{}/shell/qnav.{}",
            env!("CARGO_MANIFEST_DIR"),
            if shell.ends_with("zsh") {
                "zsh"
            } else {
                "bash"
            }
        );
        let output = Command::new(shell).args(["-f", "-c", "source \"$WRAPPER\"; builtin() { printf '%s\\0' \"$@\" > \"$CAPTURE\"; }; qnav 'argument with spaces'"])
            .current_dir(&base).env("WRAPPER", wrapper).env("PATH", format!("{}:/usr/bin:/bin", bin.display()))
            .env_remove("BASH_ENV").env_remove("ENV")
            .env("TMPDIR", &results).env("RECORD", base.join("record")).env("PAYLOAD", base.join("payload")).env("FAKE_EXIT", "0").env("CAPTURE", base.join("capture")).output().unwrap();
        assert!(output.status.success(), "{shell}: {:?}", output);
        let mut expected = b"cd\0--\0".to_vec();
        expected.extend_from_slice(native);
        expected.push(0);
        assert_eq!(fs::read(base.join("capture")).unwrap(), expected);
        assert_eq!(fs::read_dir(&results).unwrap().count(), 0);
    }
}
