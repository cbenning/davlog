//! Fetch, push and divergence detection against the backup remote. The server never
//! rebases, merges or fast-forwards the working tree: fetch only updates the
//! remote-tracking ref, and push never uses a force refspec.

use std::cell::RefCell;

use anyhow::{bail, Result};
use git2::{
    AutotagOption, Direction, ErrorClass, ErrorCode, FetchOptions, Oid, PushOptions,
    RemoteCallbacks,
};

use super::auth;
use super::Repo;
use crate::config::PushConfig;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PushResult {
    /// The remote already has our tip (or there is nothing committed yet).
    UpToDate,
    Pushed,
    /// The remote has commits we do not have. Nothing was sent.
    NonFastForward,
}

#[derive(Debug, Clone, thiserror::Error)]
pub enum PushError {
    #[error("authentication failed: {0}")]
    Auth(String),
    #[error("host key is not trusted: {0}")]
    HostKey(String),
    #[error("network error: {0}")]
    Network(String),
    #[error("{0}")]
    Other(String),
}

impl PushError {
    pub fn from_git2(e: &git2::Error) -> Self {
        let msg = e.message().to_string();
        match (e.code(), e.class()) {
            (ErrorCode::Auth, _) => PushError::Auth(msg),
            (ErrorCode::Certificate, _) => PushError::HostKey(msg),
            (ErrorCode::NotFound, _) => PushError::Network(msg),
            (
                _,
                ErrorClass::Net
                | ErrorClass::Http
                | ErrorClass::Ssh
                | ErrorClass::Ssl
                | ErrorClass::Os,
            ) => PushError::Network(msg),
            _ => PushError::Other(msg),
        }
    }
}

#[derive(Debug, Clone)]
pub struct Divergence {
    pub ahead: usize,
    pub behind: usize,
    pub remote_tip: Option<Oid>,
    /// Short hash and first line of the remote tip's message.
    pub remote_tip_summary: String,
}

#[derive(Default)]
struct Observed {
    host_key_rejected: RefCell<Option<String>>,
    push_rejection: RefCell<Option<String>>,
}

impl Observed {
    /// Turn a libgit2 error into a `PushError`, using what the callbacks recorded.
    fn classify(&self, e: &git2::Error) -> PushError {
        match self.host_key_rejected.borrow().as_ref() {
            Some(msg) => PushError::HostKey(msg.clone()),
            None => PushError::from_git2(e),
        }
    }
}

fn callbacks<'a>(cfg: &PushConfig, observed: &'a Observed) -> RemoteCallbacks<'a> {
    let mut cb = RemoteCallbacks::new();
    cb.credentials(auth::credentials_callback(&cfg.auth));
    cb.certificate_check(auth::certificate_callback(
        &cfg.auth,
        &observed.host_key_rejected,
    ));
    cb.push_update_reference(|_refname, status| {
        if let Some(msg) = status {
            *observed.push_rejection.borrow_mut() = Some(msg.to_string());
        }
        Ok(())
    });
    cb
}

fn looks_like_non_fast_forward(msg: &str) -> bool {
    let m = msg.to_ascii_lowercase();
    m.contains("fast-forward") || m.contains("fast forward") || m.contains("fetch first")
}

fn local_ref(cfg: &PushConfig) -> String {
    format!("refs/heads/{}", cfg.branch)
}

fn tracking_ref(cfg: &PushConfig) -> String {
    format!("refs/remotes/{}/{}", cfg.remote, cfg.branch)
}

/// Does the configured remote have the configured branch? Asks the remote directly,
/// so it works before any local repository exists.
pub fn remote_has_branch(cfg: &PushConfig) -> Result<bool, PushError> {
    let url = cfg
        .url
        .as_deref()
        .ok_or_else(|| PushError::Other("no remote url is configured".to_string()))?;
    let observed = Observed::default();
    let mut remote = git2::Remote::create_detached(url).map_err(|e| PushError::from_git2(&e))?;
    let mut conn = remote
        .connect_auth(Direction::Fetch, Some(callbacks(cfg, &observed)), None)
        .map_err(|e| observed.classify(&e))?;
    let wanted = local_ref(cfg);
    let heads = conn.remote().list().map_err(|e| PushError::from_git2(&e))?;
    Ok(heads.iter().any(|h| h.name() == wanted))
}

impl Repo {
    /// Add the remote if it is missing. Errors if it exists with a different URL.
    pub fn ensure_remote(&self, name: &str, url: &str) -> Result<()> {
        let repo = self.lock();
        let existing = match repo.find_remote(name) {
            Ok(existing) => existing,
            Err(e) if e.code() == ErrorCode::NotFound => {
                repo.remote(name, url)?;
                return Ok(());
            }
            Err(e) => return Err(e.into()),
        };
        let current = existing.url()?;
        if current != url {
            bail!("remote {name} already points at {current:?}, but the config says {url}");
        }
        Ok(())
    }

    /// Push the configured branch with a plain (never forced) refspec.
    pub fn push_branch(&self, cfg: &PushConfig) -> Result<PushResult, PushError> {
        let repo = self.lock();
        let local = match repo.refname_to_id(&local_ref(cfg)) {
            Ok(oid) => oid,
            Err(e) if e.code() == ErrorCode::NotFound => return Ok(PushResult::UpToDate),
            Err(e) => return Err(PushError::from_git2(&e)),
        };
        let observed = Observed::default();
        let mut remote = repo
            .find_remote(&cfg.remote)
            .map_err(|e| PushError::from_git2(&e))?;

        let remote_tip = {
            let mut conn = remote
                .connect_auth(Direction::Push, Some(callbacks(cfg, &observed)), None)
                .map_err(|e| observed.classify(&e))?;
            let heads = conn.remote().list().map_err(|e| PushError::from_git2(&e))?;
            let wanted = local_ref(cfg);
            heads.iter().find(|h| h.name() == wanted).map(|h| h.oid())
        };
        if remote_tip == Some(local) {
            return Ok(PushResult::UpToDate);
        }

        let mut opts = PushOptions::new();
        opts.remote_callbacks(callbacks(cfg, &observed));
        let refspec = format!("{0}:{0}", local_ref(cfg));
        match remote.push(&[refspec], Some(&mut opts)) {
            Ok(()) => {}
            Err(e) if e.code() == ErrorCode::NotFastForward => {
                return Ok(PushResult::NonFastForward)
            }
            Err(e) => {
                let rejection = observed.push_rejection.borrow().clone();
                return Err(match rejection {
                    Some(msg) if looks_like_non_fast_forward(&msg) => {
                        return Ok(PushResult::NonFastForward)
                    }
                    Some(msg) => PushError::Other(msg),
                    None => observed.classify(&e),
                });
            }
        }
        if let Some(msg) = observed.push_rejection.borrow().clone() {
            return if looks_like_non_fast_forward(&msg) {
                Ok(PushResult::NonFastForward)
            } else {
                Err(PushError::Other(msg))
            };
        }
        // keep the remote-tracking ref in step, as `git push` does
        let _ = repo.reference(&tracking_ref(cfg), local, true, "davgit: push");
        Ok(PushResult::Pushed)
    }

    /// Update the remote-tracking ref only. The local branch and working tree are untouched.
    pub fn fetch_branch(&self, cfg: &PushConfig) -> Result<(), PushError> {
        let repo = self.lock();
        let observed = Observed::default();
        let mut remote = repo
            .find_remote(&cfg.remote)
            .map_err(|e| PushError::from_git2(&e))?;
        let mut opts = FetchOptions::new();
        opts.remote_callbacks(callbacks(cfg, &observed));
        opts.download_tags(AutotagOption::None);
        let refspec = format!("+{}:{}", local_ref(cfg), tracking_ref(cfg));
        remote
            .fetch(&[refspec], Some(&mut opts), None)
            .map_err(|e| observed.classify(&e))
    }

    /// Ahead/behind counts against the remote-tracking ref (call `fetch_branch` first).
    pub fn divergence(&self, cfg: &PushConfig) -> Result<Divergence> {
        let repo = self.lock();
        let local = repo.refname_to_id(&local_ref(cfg))?;
        let remote_tip = match repo.refname_to_id(&tracking_ref(cfg)) {
            Ok(oid) => Some(oid),
            Err(e) if e.code() == ErrorCode::NotFound => None,
            Err(e) => return Err(e.into()),
        };
        let Some(tip) = remote_tip else {
            let mut walk = repo.revwalk()?;
            walk.push(local)?;
            return Ok(Divergence {
                ahead: walk.count(),
                behind: 0,
                remote_tip: None,
                remote_tip_summary: "(the remote has no such branch)".to_string(),
            });
        };
        let (ahead, behind) = repo.graph_ahead_behind(local, tip)?;
        let commit = repo.find_commit(tip)?;
        let first_line = commit.summary()?.unwrap_or("").to_string();
        Ok(Divergence {
            ahead,
            behind,
            remote_tip: Some(tip),
            remote_tip_summary: format!("{} {first_line}", &tip.to_string()[..7]),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use git2::Error;

    #[test]
    fn push_error_maps_auth_cert_net() {
        let e = |code, class| PushError::from_git2(&Error::new(code, class, "m"));
        assert!(matches!(
            e(ErrorCode::Auth, ErrorClass::Ssh),
            PushError::Auth(_)
        ));
        assert!(matches!(
            e(ErrorCode::Certificate, ErrorClass::Ssl),
            PushError::HostKey(_)
        ));
        assert!(matches!(
            e(ErrorCode::GenericError, ErrorClass::Net),
            PushError::Network(_)
        ));
        assert!(matches!(
            e(ErrorCode::GenericError, ErrorClass::Http),
            PushError::Network(_)
        ));
        assert!(matches!(
            e(ErrorCode::NotFound, ErrorClass::Os),
            PushError::Network(_)
        ));
        assert!(matches!(
            e(ErrorCode::GenericError, ErrorClass::Config),
            PushError::Other(_)
        ));
    }

    #[test]
    fn non_fast_forward_messages_are_recognised() {
        assert!(looks_like_non_fast_forward("non-fast-forward"));
        assert!(looks_like_non_fast_forward("rejected (fetch first)"));
        assert!(!looks_like_non_fast_forward("pre-receive hook declined"));
    }
}
