use std::{os::unix::fs::symlink, path::Path, process::Command};

#[test]
fn informational_flags_do_not_require_a_cd_result() {
    let tmp = tempfile::tempdir().unwrap();
    symlink(env!("CARGO_BIN_EXE_qnav"), tmp.path().join("qnav")).unwrap();
    let search = format!("{}:/usr/bin:/bin", tmp.path().display());
    for shell in ["bash", "zsh"] {
        let wrapper = Path::new(env!("CARGO_MANIFEST_DIR")).join(format!("shell/qnav.{shell}"));
        for flag in ["--help", "--version"] {
            let result = Command::new(shell)
                .args([
                    "-f",
                    "-c",
                    "source \"$QNAV_TEST_WRAPPER\"; qnav \"$QNAV_TEST_FLAG\"",
                ])
                .env("QNAV_TEST_WRAPPER", &wrapper)
                .env("QNAV_TEST_FLAG", flag)
                .env("PATH", &search)
                .env_remove("BASH_ENV")
                .env_remove("ENV")
                .output()
                .unwrap();
            assert!(
                result.status.success(),
                "{shell} {flag}: {}",
                String::from_utf8_lossy(&result.stderr)
            );
            assert!(String::from_utf8_lossy(&result.stdout).contains("qnav"));
            if flag == "--help" {
                let help = String::from_utf8_lossy(&result.stdout);
                assert!(
                    help.contains("cargo install installs the binary only"),
                    "{shell} {flag}: {help}"
                );
                assert!(help.contains(wrapper.to_str().unwrap()));
                assert!(help.contains("Ctrl-D delete branch merged into this checkout HEAD"));
                assert!(help.contains("Ctrl-X remove workspace with hd rm; keeps branch"));
            }
        }
    }
}
