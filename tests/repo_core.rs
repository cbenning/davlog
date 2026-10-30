mod common;

use common::*;
use davlog::config::GitConfig;
use davlog::repo::{ForeignState, Repo};

fn s(v: &[&str]) -> Vec<String> {
    v.iter().map(|x| x.to_string()).collect()
}

#[test]
fn init_creates_unborn_branch() {
    let (_dir, repo) = temp_repo("main");
    assert_eq!(repo.head_branch().unwrap(), Some("main".to_string()));
    assert_eq!(commit_count(&repo), 0);
    assert!(repo.head_commit_time().unwrap().is_none());
}

#[test]
fn first_commit_on_unborn_head() {
    let (dir, repo) = temp_repo("main");
    write(dir.path(), "a.md", b"hello");
    let oid = repo.commit_paths(&s(&["a.md"]), "first", &sig()).unwrap();
    assert!(oid.is_some());
    assert_eq!(commit_count(&repo), 1);
    assert_eq!(parent_count_of_head(&repo), 0);
    assert_eq!(head_paths(&repo), vec!["a.md"]);
    assert!(repo.head_commit_time().unwrap().is_some());
}

#[test]
fn commit_paths_stages_only_named_paths() {
    let (dir, repo) = temp_repo("main");
    write(dir.path(), "a.md", b"a");
    write(dir.path(), "b.md", b"b");
    repo.commit_paths(&s(&["a.md"]), "only a", &sig()).unwrap();
    assert_eq!(head_paths(&repo), vec!["a.md"]);
    assert!(repo.is_dirty().unwrap());
}

#[test]
fn commit_paths_handles_delete() {
    let (dir, repo) = temp_repo("main");
    write(dir.path(), "a.md", b"a");
    repo.commit_paths(&s(&["a.md"]), "add", &sig()).unwrap();
    std::fs::remove_file(dir.path().join("a.md")).unwrap();
    let oid = repo.commit_paths(&s(&["a.md"]), "rm", &sig()).unwrap();
    assert!(oid.is_some());
    assert!(head_paths(&repo).is_empty());
}

#[test]
fn commit_paths_handles_directory_move() {
    let (dir, repo) = temp_repo("main");
    write(dir.path(), "old/x.md", b"x");
    write(dir.path(), "old/sub/y.md", b"y");
    repo.commit_paths(&s(&["old"]), "add dir", &sig()).unwrap();
    assert_eq!(head_paths(&repo), vec!["old/sub/y.md", "old/x.md"]);

    std::fs::rename(dir.path().join("old"), dir.path().join("new")).unwrap();
    repo.commit_paths(&s(&["old", "new"]), "move dir", &sig())
        .unwrap();
    assert_eq!(head_paths(&repo), vec!["new/sub/y.md", "new/x.md"]);
    assert!(!repo.is_dirty().unwrap());
}

#[test]
fn identical_content_makes_no_commit() {
    let (dir, repo) = temp_repo("main");
    write(dir.path(), "a.md", b"same");
    repo.commit_paths(&s(&["a.md"]), "add", &sig()).unwrap();
    write(dir.path(), "a.md", b"same");
    let oid = repo.commit_paths(&s(&["a.md"]), "again", &sig()).unwrap();
    assert!(oid.is_none());
    assert_eq!(commit_count(&repo), 1);
}

#[test]
fn zero_byte_file_commits_once() {
    let (dir, repo) = temp_repo("main");
    write(dir.path(), "empty.md", b"");
    assert!(repo
        .commit_paths(&s(&["empty.md"]), "empty", &sig())
        .unwrap()
        .is_some());
    assert!(repo
        .commit_paths(&s(&["empty.md"]), "empty again", &sig())
        .unwrap()
        .is_none());
    assert_eq!(commit_count(&repo), 1);
    assert_eq!(head_paths(&repo), vec!["empty.md"]);
}

#[test]
fn gitignored_file_is_not_committed() {
    let (dir, repo) = temp_repo("main");
    write(dir.path(), ".gitignore", b"*.log\n");
    repo.commit_paths(&s(&[".gitignore"]), "ignore", &sig())
        .unwrap();
    write(dir.path(), "a.log", b"noise");
    let oid = repo.commit_paths(&s(&["a.log"]), "log", &sig()).unwrap();
    assert!(oid.is_none());
    assert!(dir.path().join("a.log").exists());
    assert_eq!(head_paths(&repo), vec![".gitignore"]);
}

#[test]
fn names_with_glob_characters_are_literal() {
    let (dir, repo) = temp_repo("main");
    write(dir.path(), "a[1]*?.md", b"one");
    write(dir.path(), "other.md", b"two");
    repo.commit_paths(&s(&["a[1]*?.md"]), "glob", &sig())
        .unwrap();
    assert_eq!(head_paths(&repo), vec!["a[1]*?.md"]);
}

#[test]
fn spaces_hashes_and_unicode_names_commit() {
    let (dir, repo) = temp_repo("main");
    write(dir.path(), "a b#c%d é.md", b"x");
    repo.commit_paths(&s(&["a b#c%d é.md"]), "odd", &sig())
        .unwrap();
    assert_eq!(head_paths(&repo), vec!["a b#c%d é.md"]);
}

#[test]
fn commit_all_commits_everything_and_skips_when_clean() {
    let (dir, repo) = temp_repo("main");
    write(dir.path(), "a.md", b"a");
    write(dir.path(), "d/b.md", b"b");
    assert!(repo.commit_all("all", &sig()).unwrap().is_some());
    assert_eq!(head_paths(&repo), vec!["a.md", "d/b.md"]);
    assert!(!repo.is_dirty().unwrap());
    assert!(repo.commit_all("again", &sig()).unwrap().is_none());
    std::fs::remove_file(dir.path().join("a.md")).unwrap();
    assert!(repo.commit_all("rm", &sig()).unwrap().is_some());
    assert_eq!(head_paths(&repo), vec!["d/b.md"]);
}

#[test]
fn open_existing_repo_leaves_head_alone() {
    setup();
    let dir = tempfile::tempdir().unwrap();
    drop(Repo::open_or_init(dir.path(), "dev").unwrap());
    let repo = Repo::open_or_init(dir.path(), "main").unwrap();
    assert_eq!(repo.head_branch().unwrap(), Some("dev".to_string()));
}

#[test]
fn foreign_state_detects_rebase_dir_and_index_lock() {
    let (dir, repo) = temp_repo("main");
    assert_eq!(repo.foreign_state(), ForeignState::None);

    let rebase = dir.path().join(".git/rebase-merge");
    std::fs::create_dir(&rebase).unwrap();
    assert_eq!(repo.foreign_state(), ForeignState::RebaseOrMerge);
    std::fs::remove_dir(&rebase).unwrap();

    let merge_head = dir.path().join(".git/MERGE_HEAD");
    std::fs::write(&merge_head, "x").unwrap();
    assert_eq!(repo.foreign_state(), ForeignState::RebaseOrMerge);
    std::fs::remove_file(&merge_head).unwrap();

    let lock = dir.path().join(".git/index.lock");
    std::fs::write(&lock, "").unwrap();
    assert_eq!(repo.foreign_state(), ForeignState::IndexLock);
    std::fs::remove_file(&lock).unwrap();
    assert_eq!(repo.foreign_state(), ForeignState::None);
}

#[test]
fn foreign_state_ignores_own_lock() {
    let (dir, repo) = temp_repo("main");
    let other = repo.clone();
    let lock = dir.path().join(".git/index.lock");
    repo.with_repository(|_| {
        std::fs::write(&lock, "").unwrap();
        assert_eq!(other.foreign_state(), ForeignState::None);
    });
    std::fs::remove_file(&lock).unwrap();
}

#[test]
fn apply_config_writes_repo_local_values_idempotently() {
    let (dir, repo) = temp_repo("main");
    let mut cfg = GitConfig::default();
    cfg.config
        .insert("core.autocrlf".to_string(), "false".to_string());
    repo.apply_config(&cfg).unwrap();
    repo.apply_config(&cfg).unwrap();
    let value = repo.with_repository(|r| {
        r.config()
            .unwrap()
            .snapshot()
            .unwrap()
            .get_string("core.autocrlf")
            .unwrap()
    });
    assert_eq!(value, "false");
    let text = std::fs::read_to_string(dir.path().join(".git/config")).unwrap();
    assert_eq!(text.matches("autocrlf").count(), 1, "{text}");
}

#[test]
fn crlf_file_roundtrips_byte_identical_when_autocrlf_false() {
    let crlf = b"a\r\nb\r\n";
    let commit_with = |autocrlf: &str| {
        let (dir, repo) = temp_repo("main");
        let mut cfg = GitConfig::default();
        cfg.config
            .insert("core.autocrlf".to_string(), autocrlf.to_string());
        repo.apply_config(&cfg).unwrap();
        write(dir.path(), "n.md", crlf);
        repo.commit_paths(&s(&["n.md"]), "crlf", &sig()).unwrap();
        head_blob(&repo, "n.md")
    };
    assert_eq!(commit_with("false"), crlf);
    // proves the setting is what made the difference
    assert_eq!(commit_with("true"), b"a\nb\n");
}

#[test]
fn legal_but_awkward_linux_filenames_commit() {
    let names = [
        "trailing.dot.",
        "trailing space ",
        "what?.md",
        "a:b.md",
        "back\\slash.md",
        "CON.md",
        "aux",
        "star*.md",
        "quote\".md",
        "lt<gt>.md",
        "pipe|.md",
        "tab\tname.md",
        "newline\nname.md",
        "-leading-dash.md",
        ".hidden",
        "dir. /file.md",
        "GIT~2.md",
        "\u{1F4DD} emoji.md",
    ];
    let mut failed = Vec::new();
    for name in names {
        let (dir, repo) = temp_repo("main");
        write(dir.path(), name, b"x");
        match repo.commit_paths(&[name.to_string()], "awkward", &sig()) {
            Ok(Some(_)) => {}
            other => failed.push(format!("{name:?}: {other:?}")),
        }
    }
    assert!(failed.is_empty(), "libgit2 refused:\n{}", failed.join("\n"));
}

/// An outside tool (the git CLI, a person repairing the repo) commits through its own
/// handle. davlog's next commit must build on that, not on its cached index.
#[test]
fn a_commit_made_outside_davlog_is_not_undone_by_the_next_davlog_commit() {
    let (dir, repo) = temp_repo("main");
    write(dir.path(), "a.md", b"a");
    repo.commit_paths(&s(&["a.md"]), "a", &sig()).unwrap();

    let outside = git2::Repository::open(dir.path()).unwrap();
    write(dir.path(), "b.md", b"b");
    let mut index = outside.index().unwrap();
    index.add_path(std::path::Path::new("b.md")).unwrap();
    index.write().unwrap();
    let tree = outside.find_tree(index.write_tree().unwrap()).unwrap();
    let parent = outside.head().unwrap().peel_to_commit().unwrap();
    outside
        .commit(
            Some("HEAD"),
            &sig(),
            &sig(),
            "b, made outside",
            &tree,
            &[&parent],
        )
        .unwrap();

    write(dir.path(), "c.md", b"c");
    repo.commit_paths(&s(&["c.md"]), "c", &sig()).unwrap();
    assert_eq!(head_paths(&repo), vec!["a.md", "b.md", "c.md"]);
    assert!(!repo.is_dirty().unwrap());
}

#[test]
fn an_unreadable_file_does_not_block_the_other_paths_in_the_same_commit() {
    use std::os::unix::fs::PermissionsExt;
    let (dir, repo) = temp_repo("main");
    write(dir.path(), "bad.md", b"secret");
    write(dir.path(), "good.md", b"fine");
    std::fs::set_permissions(
        dir.path().join("bad.md"),
        std::fs::Permissions::from_mode(0o000),
    )
    .unwrap();

    let oid = repo
        .commit_paths(&s(&["bad.md", "good.md"]), "mixed", &sig())
        .unwrap();
    std::fs::set_permissions(
        dir.path().join("bad.md"),
        std::fs::Permissions::from_mode(0o644),
    )
    .unwrap();
    assert!(oid.is_some(), "the readable file was committed");
    assert_eq!(head_paths(&repo), vec!["good.md"]);

    // once readable again, a later change to it commits normally
    repo.commit_paths(&s(&["bad.md"]), "now readable", &sig())
        .unwrap();
    assert_eq!(head_paths(&repo), vec!["bad.md", "good.md"]);
}

#[test]
fn only_unreadable_paths_is_an_error_not_a_silent_success() {
    use std::os::unix::fs::PermissionsExt;
    let (dir, repo) = temp_repo("main");
    write(dir.path(), "bad.md", b"secret");
    std::fs::set_permissions(
        dir.path().join("bad.md"),
        std::fs::Permissions::from_mode(0o000),
    )
    .unwrap();
    let result = repo.commit_paths(&s(&["bad.md"]), "only bad", &sig());
    std::fs::set_permissions(
        dir.path().join("bad.md"),
        std::fs::Permissions::from_mode(0o644),
    )
    .unwrap();
    assert!(result.is_err(), "{result:?}");
}
