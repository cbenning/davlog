use std::io::Write;
use std::process::{Command, Stdio};

#[test]
fn hash_password_subcommand_prints_a_verifiable_hash() {
    let mut child = Command::new(env!("CARGO_BIN_EXE_davlog"))
        .arg("hash-password")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(b"s3cret\n").unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success());
    let hash = String::from_utf8(out.stdout).unwrap();
    let hash = hash.trim();
    assert!(hash.starts_with("$argon2"), "{hash}");

    // the printed hash works as `auth.password_hash`
    let toml = format!(
        "[server]\nlisten = \"127.0.0.1:0\"\nrepo_path = \"/x\"\n[auth]\nusername = \"u\"\npassword_hash = \"{hash}\"\n"
    );
    assert!(davlog::config::Config::parse(&toml, &|_: &str| None).is_ok());
}

#[test]
fn hash_password_refuses_an_empty_password() {
    let mut child = Command::new(env!("CARGO_BIN_EXE_davlog"))
        .arg("hash-password")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(b"\n").unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(!out.status.success());
    assert!(out.stdout.is_empty());
}
