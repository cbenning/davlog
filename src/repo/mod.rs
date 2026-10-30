//! Thin wrapper around `git2::Repository`. Every git operation goes through one
//! mutex, so callers never race on the index. Methods are synchronous and block;
//! async callers wrap them in `spawn_blocking`.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, Once, TryLockError};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub mod auth;
mod maintenance;
mod remote;

use anyhow::{bail, Context, Result};

use git2::{ConfigLevel, IndexAddOption, Oid, Repository, RepositoryInitOptions, Signature};
pub use maintenance::{MaintenanceLimits, MaintenanceReport};
pub use remote::{remote_has_branch, Divergence, PushError, PushResult};
use tracing::warn;

use crate::config::GitConfig;

/// Make git behave only as the app config says: ignore the host's system, global,
/// XDG and program-data git config, and set network timeouts. Process-global; must run before any repo is opened.
pub fn isolate_from_user_git_config() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        for level in [
            ConfigLevel::System,
            ConfigLevel::Global,
            ConfigLevel::XDG,
            ConfigLevel::ProgramData,
        ] {
            // SAFETY: called once, before repositories are opened by this process.
            let _ = unsafe { git2::opts::set_search_path(level, "") };
        }
        // libgit2 has no network timeouts by default, so a dead remote would hang a
        // push forever. Connecting gets 30 s; a silent server gets 120 s.
        // SAFETY: as above, set once before any repository is opened.
        unsafe {
            let _ = git2::opts::set_server_connect_timeout_in_milliseconds(30_000);
            let _ = git2::opts::set_server_timeout_in_milliseconds(120_000);
        }
    });
}

/// libgit2 refuses a repository directory owned by another user. Say how to fix it.
pub fn owner_error(path: &Path) -> anyhow::Error {
    anyhow::anyhow!(
        "{} is not owned by the user running {}. Change its owner to that user (in Docker, `chown 10001 <dir>` on the host) or run the server as the directory's owner",
        path.display(),
        crate::brand::NAME
    )
}

/// Git state that the server did not create and must not touch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ForeignState {
    None,
    RebaseOrMerge,
    IndexLock,
}

#[derive(Clone)]
pub struct Repo {
    inner: Arc<Inner>,
}

struct Inner {
    repo: Mutex<Repository>,
    workdir: PathBuf,
    gitdir: PathBuf,
    /// Only one maintenance run at a time.
    maintenance: Mutex<()>,
}

impl Repo {
    /// Open the repository at `path`, or create one whose unborn HEAD is `branch`.
    /// An existing repository keeps whatever HEAD it has.
    pub fn open_or_init(path: &Path, branch: &str) -> Result<Repo> {
        std::fs::create_dir_all(path).with_context(|| format!("create {}", path.display()))?;
        let repo = match Repository::open(path) {
            Ok(repo) => repo,
            Err(e) if e.code() == git2::ErrorCode::NotFound => {
                let mut opts = RepositoryInitOptions::new();
                opts.initial_head(branch);
                Repository::init_opts(path, &opts).context("init repository")?
            }
            Err(e) if e.code() == git2::ErrorCode::Owner => return Err(owner_error(path)),
            Err(e) => return Err(e).context("open repository"),
        };
        let workdir = repo
            .workdir()
            .context("repository has no working tree")?
            .to_path_buf();
        let gitdir = repo.path().to_path_buf();
        Ok(Repo {
            inner: Arc::new(Inner {
                repo: Mutex::new(repo),
                workdir,
                gitdir,
                maintenance: Mutex::new(()),
            }),
        })
    }

    pub fn workdir(&self) -> &Path {
        &self.inner.workdir
    }

    fn lock(&self) -> MutexGuard<'_, Repository> {
        self.inner.repo.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Run `f` with the repository while holding the mutex.
    pub fn with_repository<T>(&self, f: impl FnOnce(&Repository) -> T) -> T {
        f(&self.lock())
    }

    /// Write the `[git.config]` entries to the repo-local config. Idempotent.
    pub fn apply_config(&self, cfg: &GitConfig) -> Result<()> {
        let repo = self.lock();
        let mut local = repo.config()?.open_level(ConfigLevel::Local)?;
        for (key, value) in &cfg.config {
            local
                .set_str(key, value)
                .with_context(|| format!("set git config {key}"))?;
        }
        Ok(())
    }

    /// Stage the named paths (files, directories, deletions) and commit.
    /// Returns `None` when nothing changed.
    pub fn commit_paths<S: AsRef<str>>(
        &self,
        paths: &[S],
        message: &str,
        author: &Signature<'_>,
    ) -> Result<Option<Oid>> {
        let repo = self.lock();
        let mut index = repo.index()?;
        // The repository caches its index. A person using git in this directory (the
        // repair flow) changes the one on disk, so reload it or this commit would build
        // on stale content and record their changes as deletions.
        index.read(true)?;
        let mut failed = 0;
        for path in paths {
            let path = path.as_ref();
            match stage_with_retry(&repo, &mut index, &self.inner.workdir, path) {
                Ok(()) => {}
                Err(e) if is_transient(&e) => {
                    // Other requests are still changing it. Whichever one it is records its
                    // own change when it finishes, which triggers the next commit.
                    warn!(
                        path,
                        "left for the next commit: it kept changing while being staged"
                    );
                }
                Err(e) => {
                    // One unreadable file must not block every other change from being committed.
                    failed += 1;
                    warn!(path, error = %format!("{e:#}"), "skipping a path that could not be staged");
                }
            }
        }
        if failed > 0 && failed == paths.len() {
            bail!("none of the {failed} changed path(s) could be staged; see the log for why");
        }
        finish_commit(&repo, &mut index, message, author)
    }

    /// Stage everything in the working tree and commit. Returns `None` when clean.
    pub fn commit_all(&self, message: &str, author: &Signature<'_>) -> Result<Option<Oid>> {
        let repo = self.lock();
        let mut index = repo.index()?;
        index.read(true)?; // see commit_paths
        index.add_all(["*"], IndexAddOption::DEFAULT, None)?;
        index.update_all(["*"], None)?;
        finish_commit(&repo, &mut index, message, author)
    }

    /// True if the working tree or index differs from HEAD (ignored files excluded).
    pub fn is_dirty(&self) -> Result<bool> {
        let repo = self.lock();
        let mut opts = git2::StatusOptions::new();
        opts.include_untracked(true)
            .recurse_untracked_dirs(true)
            .include_ignored(false);
        let statuses = repo.statuses(Some(&mut opts))?;
        Ok(!statuses.is_empty())
    }

    /// Short name of the branch HEAD points at, even if unborn. `None` if detached.
    pub fn head_branch(&self) -> Result<Option<String>> {
        let repo = self.lock();
        let head = repo.find_reference("HEAD")?;
        Ok(head
            .symbolic_target()?
            .and_then(|t| t.strip_prefix("refs/heads/"))
            .map(str::to_string))
    }

    pub fn head_commit_time(&self) -> Result<Option<SystemTime>> {
        let repo = self.lock();
        let commit = match repo.head() {
            Ok(head) => head.peel_to_commit()?,
            Err(e) if e.code() == git2::ErrorCode::UnbornBranch => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        let secs = commit.time().seconds().max(0) as u64;
        Ok(Some(UNIX_EPOCH + Duration::from_secs(secs)))
    }

    /// Rebase, merge or index-lock state that a person left in `.git`. If the repo
    /// mutex is held, the server's own operation is running and the lock is ours.
    pub fn foreign_state(&self) -> ForeignState {
        let _guard = match self.inner.repo.try_lock() {
            Ok(g) => g,
            Err(TryLockError::WouldBlock) => return ForeignState::None,
            Err(TryLockError::Poisoned(p)) => p.into_inner(),
        };
        let git = &self.inner.gitdir;
        // Anything a person leaves half-done: a rebase, a merge, a cherry-pick or revert
        // (and the sequencer that drives multi-commit ones).
        if [
            "rebase-merge",
            "rebase-apply",
            "MERGE_HEAD",
            "CHERRY_PICK_HEAD",
            "REVERT_HEAD",
            "sequencer",
        ]
        .iter()
        .any(|name| git.join(name).exists())
        {
            ForeignState::RebaseOrMerge
        } else if git.join("index.lock").exists() {
            ForeignState::IndexLock
        } else {
            ForeignState::None
        }
    }
}

/// A file that vanished or changed while it was being read: other requests are working in
/// the same folder, which is normal under load.
fn is_transient(e: &anyhow::Error) -> bool {
    e.chain().any(|cause| {
        cause.downcast_ref::<git2::Error>().is_some_and(|g| {
            g.code() == git2::ErrorCode::NotFound
                || g.class() == git2::ErrorClass::Filesystem
                // a file removed mid-read reports a bare OS error; a permission error is
                // the same class but will not go away by itself
                || (g.class() == git2::ErrorClass::Os
                    && !g.message().to_ascii_lowercase().contains("permission denied"))
        }) || cause
            .downcast_ref::<std::io::Error>()
            .is_some_and(|io| io.kind() == std::io::ErrorKind::NotFound)
    })
}

/// Stage a path, trying again a few times if it changes underneath us.
fn stage_with_retry(
    repo: &Repository,
    index: &mut git2::Index,
    workdir: &Path,
    rel: &str,
) -> Result<()> {
    const ATTEMPTS: u32 = 8;
    let mut attempt = 1;
    loop {
        match stage_path(repo, index, workdir, rel) {
            Err(e) if is_transient(&e) && attempt < ATTEMPTS => {
                std::thread::sleep(Duration::from_millis(5 * u64::from(attempt)));
                attempt += 1;
            }
            other => return other,
        }
    }
}

fn stage_path(repo: &Repository, index: &mut git2::Index, workdir: &Path, rel: &str) -> Result<()> {
    let full = workdir.join(rel);
    match std::fs::symlink_metadata(&full) {
        Ok(md) if md.is_dir() => {
            index.add_all(
                [rel],
                IndexAddOption::DEFAULT | IndexAddOption::DISABLE_PATHSPEC_MATCH,
                None,
            )?;
            remove_missing_under(index, workdir, rel)?;
        }
        Ok(_) => {
            let tracked = index.get_path(Path::new(rel), 0).is_some();
            if !tracked && repo.is_path_ignored(rel)? {
                warn!(
                    path = rel,
                    "not committing: path is ignored by gitignore rules"
                );
            } else {
                index.add_path(Path::new(rel))?;
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let _ = index.remove_path(Path::new(rel));
            remove_missing_under(index, workdir, rel)?;
        }
        Err(e) => return Err(e).with_context(|| format!("stat {rel}")),
    }
    Ok(())
}

/// Drop index entries below `dir` whose file no longer exists.
fn remove_missing_under(index: &mut git2::Index, workdir: &Path, dir: &str) -> Result<()> {
    let prefix = format!("{}/", dir.trim_end_matches('/'));
    let gone: Vec<PathBuf> = index
        .iter()
        .filter_map(|e| String::from_utf8(e.path).ok())
        .filter(|p| p.starts_with(&prefix))
        .filter(|p| std::fs::symlink_metadata(workdir.join(p)).is_err())
        .map(PathBuf::from)
        .collect();
    for path in gone {
        index.remove_path(&path)?;
    }
    Ok(())
}

fn finish_commit(
    repo: &Repository,
    index: &mut git2::Index,
    message: &str,
    author: &Signature<'_>,
) -> Result<Option<Oid>> {
    index.write()?;
    let parent = match repo.head() {
        Ok(head) => Some(head.peel_to_commit()?),
        Err(e) if e.code() == git2::ErrorCode::UnbornBranch => None,
        Err(e) => return Err(e.into()),
    };
    match &parent {
        Some(p) if p.tree_id() == index.write_tree()? => return Ok(None),
        None if index.is_empty() => return Ok(None),
        _ => {}
    }
    let tree = repo.find_tree(index.write_tree()?)?;
    let parents: Vec<&git2::Commit> = parent.iter().collect();
    let oid = repo.commit(Some("HEAD"), author, author, message, &tree, &parents)?;
    Ok(Some(oid))
}
