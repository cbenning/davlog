mod common;

use std::io::Cursor;
use std::path::Path;

use common::*;
use davgit::bootstrap::{
    apply, bootstrap_flag, inspect, resolve, Action, Choice, Reason, ResolveError, Situation,
    Terminal, EXIT_NEEDS_BOOTSTRAP_ANSWER,
};
use davgit::config::Config;

fn cfg(repo_path: &Path, push_url: Option<&str>) -> Config {
    let push = push_url
        .map(|u| format!("[push]\nurl = \"{u}\"\nbranch = \"main\"\n"))
        .unwrap_or_default();
    let toml = format!(
        "[server]\nlisten = \"127.0.0.1:0\"\nrepo_path = \"{}\"\n[auth]\nusername = \"u\"\n{push}",
        repo_path.display()
    );
    Config::parse(&toml, &|k: &str| {
        (k == "DAVGIT_PASSWORD").then(|| "p".to_string())
    })
    .unwrap()
}

fn remote_with_history() -> (tempfile::TempDir, String) {
    let (rdir, url) = bare_remote();
    other_writer_pushes(&url, "remote.md", "from remote", "remote history");
    (rdir, url)
}

fn needs_answer(s: Situation) -> Reason {
    match s {
        Situation::NeedsAnswer(r) => r,
        other => panic!("expected a question, got {other:?}"),
    }
}

#[test]
fn existing_repo_needs_no_question() {
    let (dir, _repo) = temp_repo("main");
    assert_eq!(
        inspect(&cfg(dir.path(), None)).unwrap(),
        Situation::ExistingRepo
    );
}

#[test]
fn empty_dir_without_a_remote_initialises() {
    setup();
    let dir = tempfile::tempdir().unwrap();
    assert_eq!(
        inspect(&cfg(dir.path(), None)).unwrap(),
        Situation::EmptyDirNoRemoteHistory
    );
}

#[test]
fn missing_dir_counts_as_empty() {
    setup();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("not-yet");
    assert_eq!(
        inspect(&cfg(&path, None)).unwrap(),
        Situation::EmptyDirNoRemoteHistory
    );
}

#[test]
fn empty_dir_and_empty_remote_initialises() {
    setup();
    let (_r, url) = bare_remote();
    let dir = tempfile::tempdir().unwrap();
    assert_eq!(
        inspect(&cfg(dir.path(), Some(&url))).unwrap(),
        Situation::EmptyDirNoRemoteHistory
    );
}

#[test]
fn empty_dir_and_remote_with_history_needs_answer() {
    setup();
    let (_r, url) = remote_with_history();
    let dir = tempfile::tempdir().unwrap();
    let reason = needs_answer(inspect(&cfg(dir.path(), Some(&url))).unwrap());
    assert_eq!(reason, Reason::RemoteHasHistory);
}

#[test]
fn files_without_repo_needs_answer() {
    setup();
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "notes.md", b"mine");
    let reason = needs_answer(inspect(&cfg(dir.path(), None)).unwrap());
    assert_eq!(reason, Reason::FilesButNoRepo);
}

#[test]
fn unrelated_histories_needs_answer() {
    setup();
    let (_r, url) = remote_with_history();
    let (dir, repo) = temp_repo("main");
    commit(&repo, dir.path(), "local.md", "local", "local history");
    let reason = needs_answer(inspect(&cfg(dir.path(), Some(&url))).unwrap());
    assert_eq!(reason, Reason::UnrelatedHistories);
}

#[test]
fn empty_existing_repo_with_remote_history_needs_answer() {
    setup();
    let (_r, url) = remote_with_history();
    let (dir, _repo) = temp_repo("main");
    let reason = needs_answer(inspect(&cfg(dir.path(), Some(&url))).unwrap());
    assert_eq!(reason, Reason::RemoteHasHistory);
}

#[test]
fn existing_repo_with_related_history_is_fine() {
    setup();
    let (_r, url) = bare_remote();
    let (dir, repo) = temp_repo("main");
    let pc = push_cfg(&url);
    repo.ensure_remote("origin", &url).unwrap();
    commit(&repo, dir.path(), "a.md", "one", "first");
    repo.push_branch(&pc).unwrap();
    other_writer_pushes(&url, "t.md", "t", "diverging later");
    assert_eq!(
        inspect(&cfg(dir.path(), Some(&url))).unwrap(),
        Situation::ExistingRepo,
        "divergence after a shared history is the pusher's business"
    );
}

#[test]
fn unreachable_remote_for_empty_dir_is_an_error_naming_the_remote() {
    setup();
    let dir = tempfile::tempdir().unwrap();
    let err = inspect(&cfg(dir.path(), Some("/definitely/not/a/repo"))).unwrap_err();
    assert!(format!("{err:#}").contains("remote"), "{err:#}");
}

#[test]
fn unambiguous_situations_resolve_without_a_flag() {
    assert_eq!(
        resolve(Situation::ExistingRepo, None, None).unwrap(),
        Action::UseExisting
    );
    assert_eq!(
        resolve(Situation::EmptyDirNoRemoteHistory, None, None).unwrap(),
        Action::Init
    );
}

#[test]
fn flag_answers_without_prompt() {
    let q = Situation::NeedsAnswer(Reason::RemoteHasHistory);
    assert_eq!(
        resolve(q.clone(), Some(Choice::Clone), None).unwrap(),
        Action::Clone
    );
    assert_eq!(
        resolve(q.clone(), Some(Choice::Init), None).unwrap(),
        Action::Init
    );
    assert_eq!(
        resolve(q, Some(Choice::Abort), None).unwrap(),
        Action::Abort
    );
}

#[test]
fn no_tty_and_no_flag_asks_via_exit_code_and_names_flag_and_env() {
    let q = Situation::NeedsAnswer(Reason::FilesButNoRepo);
    match resolve(q, None, None) {
        Err(e @ ResolveError::NeedsAnswer(_)) => {
            assert_eq!(e.exit_code(), EXIT_NEEDS_BOOTSTRAP_ANSWER);
            assert_eq!(EXIT_NEEDS_BOOTSTRAP_ANSWER, 10);
            let text = e.to_string();
            assert!(text.contains("--bootstrap"), "{text}");
            assert!(text.contains("DAVGIT_BOOTSTRAP"), "{text}");
            assert!(text.contains("clone") && text.contains("init") && text.contains("abort"));
        }
        other => panic!("expected NeedsAnswer, got {other:?}"),
    }
}

#[test]
fn tty_prompt_reads_choice_from_input() {
    let q = Situation::NeedsAnswer(Reason::RemoteHasHistory);
    let mut out = Vec::new();
    let mut input = Cursor::new(b"clone\n".to_vec());
    let mut term = Terminal::new(&mut input, &mut out);
    assert_eq!(
        resolve(q.clone(), None, Some(&mut term)).unwrap(),
        Action::Clone
    );
    assert!(String::from_utf8(out.clone()).unwrap().contains("remote"));

    let mut out = Vec::new();
    let mut input = Cursor::new(b"what\n\nI\n".to_vec());
    let mut term = Terminal::new(&mut input, &mut out);
    assert_eq!(
        resolve(q.clone(), None, Some(&mut term)).unwrap(),
        Action::Init
    );

    let mut out = Vec::new();
    let mut input = Cursor::new(b"".to_vec());
    let mut term = Terminal::new(&mut input, &mut out);
    assert!(
        matches!(
            resolve(q, None, Some(&mut term)),
            Err(ResolveError::NeedsAnswer(_))
        ),
        "end of input without an answer behaves like no TTY"
    );
}

#[test]
fn flag_beats_env_and_env_is_used_when_flag_absent() {
    let args = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    let env = |k: &str| (k == "DAVGIT_BOOTSTRAP").then(|| "abort".to_string());
    let none = |_: &str| None;
    assert_eq!(bootstrap_flag(&args(&[]), &none).unwrap(), None);
    assert_eq!(
        bootstrap_flag(&args(&[]), &env).unwrap(),
        Some(Choice::Abort)
    );
    assert_eq!(
        bootstrap_flag(&args(&["--bootstrap", "clone"]), &env).unwrap(),
        Some(Choice::Clone)
    );
    assert_eq!(
        bootstrap_flag(&args(&["--bootstrap=init"]), &env).unwrap(),
        Some(Choice::Init)
    );
    assert!(bootstrap_flag(&args(&["--bootstrap", "maybe"]), &none).is_err());
    assert!(bootstrap_flag(&args(&["--bootstrap"]), &none).is_err());
}

#[test]
fn clone_populates_worktree_from_local_bare_remote() {
    setup();
    let (_r, url) = remote_with_history();
    let dir = tempfile::tempdir().unwrap();
    let repo = apply(&cfg(dir.path(), Some(&url)), Action::Clone).unwrap();
    assert_eq!(
        std::fs::read_to_string(dir.path().join("remote.md")).unwrap(),
        "from remote"
    );
    assert_eq!(repo.head_branch().unwrap(), Some("main".to_string()));
    assert!(!repo.is_dirty().unwrap());
}

#[test]
fn clone_refuses_a_non_empty_directory() {
    setup();
    let (_r, url) = remote_with_history();
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "mine.md", b"mine");
    assert!(apply(&cfg(dir.path(), Some(&url)), Action::Clone).is_err());
    assert_eq!(
        std::fs::read_to_string(dir.path().join("mine.md")).unwrap(),
        "mine"
    );
}

#[test]
fn clone_without_a_remote_is_an_error() {
    setup();
    let dir = tempfile::tempdir().unwrap();
    assert!(apply(&cfg(dir.path(), None), Action::Clone).is_err());
}

#[test]
fn init_keeps_existing_files_and_adds_the_remote() {
    setup();
    let (_r, url) = bare_remote();
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "mine.md", b"mine");
    let repo = apply(&cfg(dir.path(), Some(&url)), Action::Init).unwrap();
    assert_eq!(
        std::fs::read_to_string(dir.path().join("mine.md")).unwrap(),
        "mine"
    );
    assert_eq!(repo.head_branch().unwrap(), Some("main".to_string()));
    assert!(
        repo.is_dirty().unwrap(),
        "files stay uncommitted until the first commit"
    );
    repo.with_repository(|r| assert_eq!(r.find_remote("origin").unwrap().url().unwrap(), url));
}

#[test]
fn use_existing_does_not_touch_head() {
    let (dir, _repo) = temp_repo("dev");
    let repo = apply(&cfg(dir.path(), None), Action::UseExisting).unwrap();
    assert_eq!(repo.head_branch().unwrap(), Some("dev".to_string()));
}

#[test]
fn abort_is_not_applied() {
    setup();
    let dir = tempfile::tempdir().unwrap();
    assert!(apply(&cfg(dir.path(), None), Action::Abort).is_err());
}
