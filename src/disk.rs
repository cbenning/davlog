//! Protection against filling the disk: a free-space floor and an optional cap on the
//! size of the repository. Writes that add data are refused with 507 while either is
//! violated; deletes and reads still work, so a full disk can be dealt with.

use std::fmt;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tracing::debug;

const REFRESH_EVERY: Duration = Duration::from_secs(60);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiskProblem {
    LowSpace { free: u64, min: u64 },
    RepoTooBig { size: u64, max: u64 },
}

impl fmt::Display for DiskProblem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DiskProblem::LowSpace { free, min } => write!(
                f,
                "only {free} bytes are free and the floor is {min} (limits.min_free_space)"
            ),
            DiskProblem::RepoTooBig { size, max } => write!(
                f,
                "the repository is {size} bytes and the cap is {max} (limits.max_repo_size)"
            ),
        }
    }
}

pub struct DiskGuard {
    path: PathBuf,
    min_free: u64,
    max_repo: Option<u64>,
    repo_size: AtomicU64,
}

impl DiskGuard {
    /// `path` is the repository's working tree. With a size cap, the size is measured now
    /// and then refreshed by `run_refresh`.
    pub fn new(path: PathBuf, min_free: u64, max_repo: Option<u64>) -> DiskGuard {
        let guard = DiskGuard {
            path,
            min_free,
            max_repo,
            repo_size: AtomicU64::new(0),
        };
        guard.refresh_repo_size();
        guard
    }

    /// Bytes available to this process on the repository's filesystem.
    pub fn free_space(&self) -> std::io::Result<u64> {
        let stat = rustix::fs::statvfs(&self.path)?;
        Ok(stat.f_bavail.saturating_mul(stat.f_frsize))
    }

    pub fn check(&self) -> Result<(), DiskProblem> {
        if self.min_free > 0 {
            // If the filesystem cannot be asked, do not block writes over it.
            if let Ok(free) = self.free_space() {
                if free < self.min_free {
                    return Err(DiskProblem::LowSpace {
                        free,
                        min: self.min_free,
                    });
                }
            }
        }
        if let Some(max) = self.max_repo {
            let size = self.repo_size.load(Ordering::Relaxed);
            if size > max {
                return Err(DiskProblem::RepoTooBig { size, max });
            }
        }
        Ok(())
    }

    /// Re-measure the repository. Does nothing without a size cap.
    pub fn refresh_repo_size(&self) {
        if self.max_repo.is_some() {
            self.repo_size
                .store(dir_size(&self.path), Ordering::Relaxed);
        }
    }

    /// Re-measure every minute until `stop` resolves.
    pub async fn run_refresh(self: Arc<Self>, stop: impl Future<Output = ()>) {
        tokio::pin!(stop);
        if self.max_repo.is_none() {
            // nothing to measure, but stay alive until shutdown so the task is not mistaken
            // for one that died
            stop.await;
            return;
        }
        let mut ticker = tokio::time::interval(REFRESH_EVERY);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        ticker.tick().await; // the first tick is immediate; `new` just measured
        loop {
            tokio::select! {
                _ = ticker.tick() => {
                    let guard = self.clone();
                    let _ = tokio::task::spawn_blocking(move || guard.refresh_repo_size()).await;
                    debug!("repository size refreshed");
                }
                _ = &mut stop => break,
            }
        }
    }
}

/// Total size of the regular files under `root`, symlinks not followed.
pub fn dir_size(root: &Path) -> u64 {
    let mut total = 0u64;
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let Ok(meta) = std::fs::symlink_metadata(entry.path()) else {
                continue;
            };
            if meta.is_dir() {
                stack.push(entry.path());
            } else if meta.is_file() {
                total = total.saturating_add(meta.len());
            }
        }
    }
    total
}

#[cfg(test)]
mod tests {
    use super::*;

    fn guard(dir: &std::path::Path, min_free: u64, max_repo: Option<u64>) -> DiskGuard {
        DiskGuard::new(dir.to_path_buf(), min_free, max_repo)
    }

    #[test]
    fn free_space_is_reported() {
        let dir = tempfile::tempdir().unwrap();
        assert!(guard(dir.path(), 0, None).free_space().unwrap() > 0);
    }

    #[test]
    fn plenty_of_space_and_no_cap_is_fine() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(guard(dir.path(), 0, None).check(), Ok(()));
        assert_eq!(guard(dir.path(), 1024, None).check(), Ok(()));
    }

    #[test]
    fn too_little_free_space_is_a_problem() {
        let dir = tempfile::tempdir().unwrap();
        let g = guard(dir.path(), u64::MAX, None);
        assert!(matches!(g.check(), Err(DiskProblem::LowSpace { .. })));
    }

    #[test]
    fn repo_size_cap_counts_everything_including_dot_git_and_ignores_symlinks() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".git/objects")).unwrap();
        std::fs::create_dir_all(dir.path().join("sub")).unwrap();
        std::fs::write(dir.path().join("a.bin"), vec![0u8; 4000]).unwrap();
        std::fs::write(dir.path().join("sub/b.bin"), vec![0u8; 3000]).unwrap();
        std::fs::write(dir.path().join(".git/objects/o"), vec![0u8; 2000]).unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("huge"), vec![0u8; 1_000_000]).unwrap();
        std::os::unix::fs::symlink(outside.path().join("huge"), dir.path().join("link")).unwrap();

        assert_eq!(dir_size(dir.path()), 9000);

        let g = guard(dir.path(), 0, Some(8999));
        assert!(matches!(
            g.check(),
            Err(DiskProblem::RepoTooBig {
                size: 9000,
                max: 8999
            })
        ));

        // growth is noticed on the next refresh, not before
        let roomy = guard(dir.path(), 0, Some(9500));
        assert_eq!(roomy.check(), Ok(()));
        std::fs::write(dir.path().join("more.bin"), vec![0u8; 1000]).unwrap();
        assert_eq!(roomy.check(), Ok(()), "not measured again yet");
        roomy.refresh_repo_size();
        assert!(matches!(
            roomy.check(),
            Err(DiskProblem::RepoTooBig { size: 10000, .. })
        ));
        std::fs::remove_file(dir.path().join("more.bin")).unwrap();

        let ok = guard(dir.path(), 0, Some(9000));
        ok.refresh_repo_size();
        assert_eq!(ok.check(), Ok(()));
    }

    #[test]
    fn construction_measures_the_repo_when_a_cap_is_set() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.bin"), vec![0u8; 5000]).unwrap();
        let g = guard(dir.path(), 0, Some(1000));
        assert!(matches!(g.check(), Err(DiskProblem::RepoTooBig { .. })));
    }
}
