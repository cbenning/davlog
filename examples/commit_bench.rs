//! Throwaway benchmark of commit cost. Run with:
//!   cargo run --release --example commit_bench
//! Decides the default commit mode: `per_request` stays the default while a single-path
//! commit is fast (about 5 ms on a 5,000-file tree) and a 1,000-write burst stays quick.

use std::path::Path;
use std::time::{Duration, Instant};

use davlog::repo::Repo;
use git2::Signature;

const TREE_FILES: usize = 5_000;
const SAMPLES: usize = 200;
const BURST: usize = 1_000;

fn write(root: &Path, rel: &str, body: &str) {
    let path = root.join(rel);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, body).unwrap();
}

fn report(label: &str, mut samples: Vec<Duration>) {
    samples.sort();
    let ms = |d: Duration| d.as_secs_f64() * 1000.0;
    let median = ms(samples[samples.len() / 2]);
    let p95 = ms(samples[samples.len() * 95 / 100]);
    let total: f64 = samples.iter().map(|d| ms(*d)).sum();
    println!("{label:<48} median {median:>8.2} ms   p95 {p95:>8.2} ms   total {total:>9.0} ms");
}

fn fresh(base: &Path, name: &str) -> (std::path::PathBuf, Repo, Signature<'static>) {
    let root = base.join(name);
    let repo = Repo::open_or_init(&root, "main").unwrap();
    let sig = Signature::now("bench", "bench@localhost").unwrap();
    for i in 0..TREE_FILES {
        write(
            &root,
            &format!("d{:02}/f{i:04}.md", i % 50),
            &format!("note {i}\n"),
        );
    }
    repo.commit_all("seed", &sig).unwrap();
    (root, repo, sig)
}

fn single_path_commits(
    root: &Path,
    repo: &Repo,
    sig: &Signature<'_>,
    add_all: bool,
) -> Vec<Duration> {
    (0..SAMPLES)
        .map(|n| {
            let rel = format!("d{:02}/f{:04}.md", n % 50, n);
            write(root, &rel, &format!("edit {n} {add_all}\n"));
            let start = Instant::now();
            if add_all {
                repo.commit_all("edit", sig).unwrap();
            } else {
                repo.commit_paths(&[rel], "edit", sig).unwrap();
            }
            start.elapsed()
        })
        .collect()
}

fn main() {
    davlog::repo::isolate_from_user_git_config();
    std::fs::create_dir_all("target").unwrap();
    let tmp = tempfile::tempdir_in("target").unwrap();
    println!("tree of {TREE_FILES} files, {SAMPLES} samples per row, burst of {BURST}\n");

    let (root, repo, sig) = fresh(tmp.path(), "paths");
    report(
        "single-path commit (no fsync)",
        single_path_commits(&root, &repo, &sig, false),
    );

    let (root, repo, sig) = fresh(tmp.path(), "addall");
    report(
        "add-all commit, one file touched (no fsync)",
        single_path_commits(&root, &repo, &sig, true),
    );

    let (root, repo, sig) = fresh(tmp.path(), "burst-many");
    for i in 0..BURST {
        write(&root, &format!("burst/f{i:04}.md"), &format!("burst {i}\n"));
    }
    let start = Instant::now();
    let mut each = Vec::new();
    for i in 0..BURST {
        let t = Instant::now();
        repo.commit_paths(&[format!("burst/f{i:04}.md")], "burst", &sig)
            .unwrap();
        each.push(t.elapsed());
    }
    report(&format!("burst: {BURST} single-path commits"), each);
    println!("{:<48} wall {:.1} s", "", start.elapsed().as_secs_f64());

    let (root, repo, sig) = fresh(tmp.path(), "burst-one");
    let paths: Vec<String> = (0..BURST).map(|i| format!("burst/f{i:04}.md")).collect();
    for p in &paths {
        write(&root, p, "burst\n");
    }
    let start = Instant::now();
    repo.commit_paths(&paths, "burst", &sig).unwrap();
    println!(
        "burst: one {BURST}-path (group) commit               wall {:.3} s",
        start.elapsed().as_secs_f64()
    );

    // git2 does not expose GIT_OPT_ENABLE_FSYNC_GITDIR; libgit2 also honours core.fsync config.
    let (root, repo, sig) = fresh(tmp.path(), "fsync");
    let mut cfg = davlog::config::GitConfig::default();
    cfg.config.insert("core.fsync".into(), "all".into());
    cfg.config
        .insert("core.fsyncobjectfiles".into(), "true".into());
    repo.apply_config(&cfg).unwrap();
    report(
        "single-path commit (fsync gitdir)",
        single_path_commits(&root, &repo, &sig, false),
    );
}
