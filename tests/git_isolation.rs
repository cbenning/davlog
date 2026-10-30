//! Own test binary: libgit2 reads HOME once per process, so this must run before
//! any other libgit2 call in the process.

#[test]
fn host_gitconfig_is_ignored() {
    let home = tempfile::tempdir().unwrap();
    std::fs::write(
        home.path().join(".gitconfig"),
        "[core]\n\tautocrlf = true\n[user]\n\tname = Host User\n",
    )
    .unwrap();
    std::env::set_var("HOME", home.path());
    std::env::set_var("XDG_CONFIG_HOME", home.path().join("xdg"));

    // sanity: without isolation libgit2 sees the host config
    let before = git2::Config::open_default().unwrap();
    assert_eq!(before.get_string("user.name").unwrap(), "Host User");

    davlog::repo::isolate_from_user_git_config();

    let after = git2::Config::open_default().unwrap();
    assert!(after.get_string("user.name").is_err());
    assert!(after.get_string("core.autocrlf").is_err());

    let dir = tempfile::tempdir().unwrap();
    let repo = davlog::repo::Repo::open_or_init(dir.path(), "main").unwrap();
    let seen = repo.with_repository(|r| {
        r.config()
            .unwrap()
            .snapshot()
            .unwrap()
            .get_string("user.name")
            .is_ok()
    });
    assert!(!seen, "repo config must not see the host's user.name");
}
