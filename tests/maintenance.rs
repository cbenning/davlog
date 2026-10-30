mod common;

use std::collections::BTreeSet;
use std::path::Path;

use common::*;
use davlog::repo::{MaintenanceLimits, Repo};
use git2::{Oid, Repository};

const LOW: MaintenanceLimits = MaintenanceLimits {
    loose_objects: 10,
    packs: 1,
};

fn commits(repo: &Repo, dir: &Path, n: usize) {
    for i in 0..n {
        let body = format!(
            "line one\nline two\nedit number {i}\n{}\n",
            "padding ".repeat(60)
        );
        write(dir, &format!("notes/n{}.md", i % 7), body.as_bytes());
        repo.commit_paths(
            &[format!("notes/n{}.md", i % 7)],
            &format!("edit {i}"),
            &sig(),
        )
        .unwrap();
    }
}

fn fresh(dir: &Path) -> Repository {
    Repository::open(dir).unwrap()
}

fn all_oids(dir: &Path) -> BTreeSet<Oid> {
    let repo = fresh(dir);
    let odb = repo.odb().unwrap();
    let mut ids = BTreeSet::new();
    odb.foreach(|id| {
        ids.insert(*id);
        true
    })
    .unwrap();
    ids
}

fn loose_count(dir: &Path) -> usize {
    let mut n = 0;
    for entry in std::fs::read_dir(dir.join(".git/objects"))
        .unwrap()
        .flatten()
    {
        let name = entry.file_name().to_string_lossy().to_string();
        if name.len() == 2 && name.chars().all(|c| c.is_ascii_hexdigit()) {
            // the directory may be removed by a repack while we count
            n += std::fs::read_dir(entry.path())
                .map(|d| d.count())
                .unwrap_or(0);
        }
    }
    n
}

fn packs(dir: &Path) -> Vec<String> {
    let mut v: Vec<String> = std::fs::read_dir(dir.join(".git/objects/pack"))
        .map(|d| {
            d.flatten()
                .map(|e| e.file_name().to_string_lossy().to_string())
                .collect()
        })
        .unwrap_or_default();
    v.retain(|n| n.ends_with(".pack"));
    v.sort();
    v
}

fn objects_dir_bytes(dir: &Path) -> u64 {
    davlog::disk::dir_size(&dir.join(".git/objects"))
}

/// Every commit, tree and blob reachable from HEAD can be read.
fn assert_history_readable(dir: &Path, expected_commits: usize) {
    let repo = fresh(dir);
    let mut walk = repo.revwalk().unwrap();
    walk.push_head().unwrap();
    let mut n = 0;
    for id in walk {
        let commit = repo.find_commit(id.unwrap()).unwrap();
        commit
            .tree()
            .unwrap()
            .walk(git2::TreeWalkMode::PreOrder, |_, entry| {
                if entry.kind() == Some(git2::ObjectType::Blob) {
                    repo.find_blob(entry.id()).unwrap();
                }
                git2::TreeWalkResult::Ok
            })
            .unwrap();
        n += 1;
    }
    assert_eq!(n, expected_commits);
}

#[test]
fn packs_loose_objects_without_losing_anything() {
    let (dir, repo) = temp_repo("main");
    commits(&repo, dir.path(), 60);
    let before_ids = all_oids(dir.path());
    let before_loose = loose_count(dir.path());
    let before_bytes = objects_dir_bytes(dir.path());
    assert!(before_loose > 100, "{before_loose}");

    let report = repo.maintain(&LOW).unwrap();
    assert!(report.ran);
    assert_eq!(report.loose_before, before_loose);

    assert_eq!(all_oids(dir.path()), before_ids, "no object was lost");
    assert_eq!(loose_count(dir.path()), 0);
    assert_eq!(packs(dir.path()).len(), 1);
    assert!(
        objects_dir_bytes(dir.path()) < before_bytes,
        "delta compression made it smaller"
    );
    assert_history_readable(dir.path(), 60);
    assert!(!repo.is_dirty().unwrap());
}

#[test]
fn does_nothing_below_the_limits() {
    let (dir, repo) = temp_repo("main");
    commits(&repo, dir.path(), 3);
    let before = loose_count(dir.path());
    let report = repo
        .maintain(&MaintenanceLimits {
            loose_objects: 10_000,
            packs: 20,
        })
        .unwrap();
    assert!(!report.ran);
    assert_eq!(loose_count(dir.path()), before);
    assert!(packs(dir.path()).is_empty());
}

#[test]
fn running_again_on_a_packed_repo_keeps_one_pack_and_every_object() {
    let (dir, repo) = temp_repo("main");
    commits(&repo, dir.path(), 30);
    repo.maintain(&LOW).unwrap();
    let packs_after_first = packs(dir.path());
    let ids = all_oids(dir.path());
    // forced: even with a single pack and nothing loose
    repo.maintain(&MaintenanceLimits {
        loose_objects: 0,
        packs: 0,
    })
    .unwrap();
    // The pack's name can differ between runs (delta search is multi-threaded), so check
    // what matters: still exactly one pack, holding the same objects.
    assert_eq!(packs_after_first.len(), 1);
    assert_eq!(packs(dir.path()).len(), 1);
    assert_eq!(all_oids(dir.path()), ids);
    assert_history_readable(dir.path(), 30);
}

#[test]
fn unreachable_objects_are_kept() {
    let (dir, repo) = temp_repo("main");
    commits(&repo, dir.path(), 20);
    let garbage = repo.with_repository(|r| r.blob(b"nobody points at me").unwrap());
    repo.maintain(&LOW).unwrap();
    let fresh_repo = fresh(dir.path());
    assert_eq!(
        fresh_repo.find_blob(garbage).unwrap().content(),
        b"nobody points at me"
    );
}

#[test]
fn several_packs_are_consolidated_into_one() {
    let (dir, repo) = temp_repo("main");
    for round in 0..3 {
        commits(&repo, dir.path(), 15);
        // each round packs only what is loose, leaving several packs when run with a pack limit above 3
        repo.maintain(&MaintenanceLimits {
            loose_objects: 5,
            packs: 100,
        })
        .unwrap();
        let _ = round;
    }
    // with the limit above the pack count nothing consolidates, so each run with loose
    // objects above the limit still makes a pack; consolidation happens when packs > limit
    let ids = all_oids(dir.path());
    repo.maintain(&MaintenanceLimits {
        loose_objects: 100_000,
        packs: 1,
    })
    .unwrap();
    assert_eq!(packs(dir.path()).len(), 1);
    assert_eq!(all_oids(dir.path()), ids);
    assert_history_readable(dir.path(), 45);
}

#[test]
fn a_pack_with_a_keep_file_is_left_alone() {
    let (dir, repo) = temp_repo("main");
    commits(&repo, dir.path(), 20);
    repo.maintain(&LOW).unwrap();
    let kept = packs(dir.path()).remove(0);
    std::fs::write(
        dir.path().join(format!(
            ".git/objects/pack/{}",
            kept.replace(".pack", ".keep")
        )),
        "",
    )
    .unwrap();
    commits(&repo, dir.path(), 20);
    repo.maintain(&MaintenanceLimits {
        loose_objects: 5,
        packs: 0,
    })
    .unwrap();
    assert!(packs(dir.path()).contains(&kept), "a kept pack survives");
    assert_history_readable(dir.path(), 40);
}

#[test]
fn commits_made_while_maintenance_runs_are_never_lost() {
    let (dir, repo) = temp_repo("main");
    commits(&repo, dir.path(), 20);
    let committing = repo.clone();
    let path = dir.path().to_path_buf();
    let writer = std::thread::spawn(move || {
        for i in 0..120 {
            let rel = format!("live/f{}.md", i % 11);
            write(
                &path,
                &rel,
                format!("live edit {i}\n{}", "x".repeat(200)).as_bytes(),
            );
            committing
                .commit_paths(&[rel], &format!("live {i}"), &sig())
                .unwrap();
        }
    });
    let mut runs = 0;
    while !writer.is_finished() {
        repo.maintain(&MaintenanceLimits {
            loose_objects: 5,
            packs: 2,
        })
        .unwrap();
        runs += 1;
    }
    writer.join().unwrap();
    assert!(runs >= 1);
    repo.maintain(&MaintenanceLimits {
        loose_objects: 0,
        packs: 0,
    })
    .unwrap();
    assert_history_readable(dir.path(), 140);
    assert!(!repo.is_dirty().unwrap());
}

#[test]
fn an_empty_repository_is_fine() {
    let (dir, repo) = temp_repo("main");
    let report = repo
        .maintain(&MaintenanceLimits {
            loose_objects: 0,
            packs: 0,
        })
        .unwrap();
    assert!(!report.ran || report.objects == 0);
    assert!(packs(dir.path()).is_empty());
}

#[test]
fn stale_verification_directories_are_cleaned_up() {
    let (dir, repo) = temp_repo("main");
    commits(&repo, dir.path(), 15);
    let stale = dir.path().join(".git/davlog-tmp/verify-stale/objects/pack");
    std::fs::create_dir_all(&stale).unwrap();
    std::fs::write(stale.join("leftover"), "x").unwrap();
    repo.maintain(&LOW).unwrap();
    assert!(!dir.path().join(".git/davlog-tmp/verify-stale").exists());
}
