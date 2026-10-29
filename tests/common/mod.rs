#![allow(dead_code)]

use std::path::Path;

use davgit::repo::Repo;
use git2::Signature;
use tempfile::TempDir;

pub fn setup() {
    davgit::repo::isolate_from_user_git_config();
}

pub fn temp_repo(branch: &str) -> (TempDir, Repo) {
    setup();
    let dir = tempfile::tempdir().unwrap();
    let repo = Repo::open_or_init(dir.path(), branch).unwrap();
    (dir, repo)
}

pub fn write(root: &Path, rel: &str, bytes: &[u8]) {
    let path = root.join(rel);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, bytes).unwrap();
}

pub fn sig() -> Signature<'static> {
    Signature::now("tester", "tester@localhost").unwrap()
}

/// Number of commits reachable from HEAD (0 on an unborn branch).
pub fn commit_count(repo: &Repo) -> usize {
    repo.with_repository(|r| {
        let Ok(head) = r.head() else { return 0 };
        let mut walk = r.revwalk().unwrap();
        walk.push(head.target().unwrap()).unwrap();
        walk.count()
    })
}

/// Sorted file paths in HEAD's tree.
pub fn head_paths(repo: &Repo) -> Vec<String> {
    repo.with_repository(|r| {
        let Ok(head) = r.head() else { return vec![] };
        let tree = head.peel_to_tree().unwrap();
        let mut out = vec![];
        tree.walk(git2::TreeWalkMode::PreOrder, |dir, entry| {
            if entry.kind() == Some(git2::ObjectType::Blob) {
                out.push(format!("{dir}{}", entry.name().unwrap()));
            }
            git2::TreeWalkResult::Ok
        })
        .unwrap();
        out.sort();
        out
    })
}

pub fn head_blob(repo: &Repo, path: &str) -> Vec<u8> {
    repo.with_repository(|r| {
        let tree = r.head().unwrap().peel_to_tree().unwrap();
        let entry = tree.get_path(Path::new(path)).unwrap();
        r.find_blob(entry.id()).unwrap().content().to_vec()
    })
}

pub fn parent_count_of_head(repo: &Repo) -> usize {
    repo.with_repository(|r| r.head().unwrap().peel_to_commit().unwrap().parent_count())
}
