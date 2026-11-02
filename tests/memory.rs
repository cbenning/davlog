//! Memory must not grow with the number of commits. This is its own test binary with one
//! test so nothing else in the process allocates while it measures.

mod common;

use common::*;

fn anon_mib() -> f64 {
    let status = std::fs::read_to_string("/proc/self/status").unwrap();
    let kb: f64 = status
        .lines()
        .find_map(|l| l.strip_prefix("RssAnon:"))
        .and_then(|v| v.trim().trim_end_matches("kB").trim().parse().ok())
        .unwrap();
    kb / 1024.0
}

#[test]
fn memory_stays_flat_as_commits_accumulate() {
    let (dir, repo) = temp_repo("main");
    // 20 folders of 100 files: each commit rewrites a folder's tree of about 3 KB
    let commit = |n: usize| {
        let rel = format!("d{}/f{}.md", n % 20, (n / 20) % 100);
        let body = format!("edit {n}\n{}", "x".repeat(3000));
        write(dir.path(), &rel, body.as_bytes());
        repo.commit_paths(&[rel], &format!("edit {n}"), &sig())
            .unwrap();
    };
    for n in 0..2400 {
        commit(n); // build the folders up and let allocator pools settle
    }
    let before = anon_mib();
    for n in 2400..4400 {
        commit(n);
    }
    let grown = anon_mib() - before;
    // Before the fix this grew by about 10 MiB per 2,000 commits.
    assert!(
        grown < 4.0,
        "memory grew by {grown:.1} MiB over 2,000 commits"
    );
}
