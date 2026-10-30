//! First-start repository setup. The server owns the repository, so it creates one
//! when none exists. When the right thing is ambiguous it asks the operator: on a
//! terminal it prompts, otherwise it exits with a distinct code and names the flag
//! and environment variable that answer the question.

use std::cell::RefCell;
use std::io::{BufRead, Write};
use std::path::Path;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use git2::build::RepoBuilder;
use git2::{FetchOptions, RemoteCallbacks};

use crate::brand;
use crate::config::Config;
use crate::repo::{self, auth, remote_has_branch, Repo};

/// Exit code when the server needs a bootstrap answer and cannot ask.
pub const EXIT_NEEDS_BOOTSTRAP_ANSWER: i32 = 10;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reason {
    RemoteHasHistory,
    FilesButNoRepo,
    UnrelatedHistories,
}

impl Reason {
    fn describe(&self) -> &'static str {
        match self {
            Reason::RemoteHasHistory => "the remote branch already has history",
            Reason::FilesButNoRepo => "the directory has files but is not a git repository",
            Reason::UnrelatedHistories => {
                "the local repository and the remote branch have unrelated histories"
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Situation {
    ExistingRepo,
    EmptyDirNoRemoteHistory,
    NeedsAnswer(Reason),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Choice {
    Clone,
    Init,
    Abort,
}

impl Choice {
    pub fn parse(text: &str) -> Option<Choice> {
        match text.trim().to_ascii_lowercase().as_str() {
            "clone" | "c" => Some(Choice::Clone),
            "init" | "i" => Some(Choice::Init),
            "abort" | "a" => Some(Choice::Abort),
            _ => None,
        }
    }
}

/// What to do about the repository.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    UseExisting,
    Init,
    Clone,
    Abort,
}

#[derive(Debug, thiserror::Error)]
pub enum ResolveError {
    /// The server must be told what to do, and could not ask.
    #[error("{0}")]
    NeedsAnswer(String),
    #[error("{0:#}")]
    Other(anyhow::Error),
}

impl ResolveError {
    pub fn exit_code(&self) -> i32 {
        match self {
            ResolveError::NeedsAnswer(_) => EXIT_NEEDS_BOOTSTRAP_ANSWER,
            ResolveError::Other(_) => 1,
        }
    }
}

/// A terminal to prompt on. Pass it only when stdin and stdout are TTYs.
pub struct Terminal<'a> {
    input: &'a mut dyn BufRead,
    output: &'a mut dyn Write,
}

impl<'a> Terminal<'a> {
    pub fn new(input: &'a mut dyn BufRead, output: &'a mut dyn Write) -> Self {
        Terminal { input, output }
    }
}

fn dir_is_empty(path: &Path) -> Result<bool> {
    match std::fs::read_dir(path) {
        Ok(mut entries) => Ok(entries.next().is_none()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(true),
        Err(e) => Err(e).with_context(|| format!("read {}", path.display())),
    }
}

fn branch_of(cfg: &Config) -> &str {
    cfg.push.as_ref().map_or("main", |p| p.branch.as_str())
}

/// How long startup waits for the remote when it only wants to compare histories.
const REMOTE_CHECK_TIMEOUT: Duration = Duration::from_secs(5);

/// Fetch the remote branch, but give up waiting after `limit`. A fetch that is still
/// stuck keeps its own thread until libgit2's network timeout ends it.
fn fetch_within(repo: &Repo, push: &crate::config::PushConfig, limit: Duration) -> bool {
    let (tx, rx) = std::sync::mpsc::channel();
    let (repo, push) = (repo.clone(), push.clone());
    std::thread::spawn(move || {
        let _ = tx.send(repo.fetch_branch(&push).is_ok());
    });
    rx.recv_timeout(limit).unwrap_or(false)
}

/// Look at the directory and the remote and decide whether a question is needed.
pub fn inspect(cfg: &Config) -> Result<Situation> {
    let path = &cfg.server.repo_path;
    let push = cfg.push.as_ref().filter(|p| p.url.is_some());

    let opened = git2::Repository::open(path);
    if matches!(&opened, Err(e) if e.code() == git2::ErrorCode::Owner) {
        return Err(repo::owner_error(path));
    }
    if opened.is_ok() {
        let Some(push) = push else {
            return Ok(Situation::ExistingRepo);
        };
        let url = push.url.as_deref().unwrap_or_default();
        let repo = Repo::open_or_init(path, &push.branch)?;
        repo.ensure_remote(&push.remote, url)?;
        // An unreachable or unresponsive remote must not hold up starting an existing
        // repository, so this check is best-effort and gives up after a few seconds.
        if !fetch_within(&repo, push, REMOTE_CHECK_TIMEOUT) {
            return Ok(Situation::ExistingRepo);
        }
        return repo.with_repository(|r| {
            let local = r.refname_to_id(&format!("refs/heads/{}", push.branch)).ok();
            let remote = r
                .refname_to_id(&format!("refs/remotes/{}/{}", push.remote, push.branch))
                .ok();
            Ok(match (local, remote) {
                (_, None) => Situation::ExistingRepo,
                (None, Some(_)) => Situation::NeedsAnswer(Reason::RemoteHasHistory),
                (Some(l), Some(rm)) => match r.merge_base(l, rm) {
                    Ok(_) => Situation::ExistingRepo,
                    Err(e) if e.code() == git2::ErrorCode::NotFound => {
                        Situation::NeedsAnswer(Reason::UnrelatedHistories)
                    }
                    Err(e) => return Err(anyhow::Error::from(e)),
                },
            })
        });
    }

    if !dir_is_empty(path)? {
        return Ok(Situation::NeedsAnswer(Reason::FilesButNoRepo));
    }
    if let Some(push) = push {
        let has_history = remote_has_branch(push).with_context(|| {
            format!(
                "cannot reach the remote to check whether it already has history; fix [push] or answer with --bootstrap clone|init|abort ({}_BOOTSTRAP)",
                brand::ENV_PREFIX
            )
        })?;
        if has_history {
            return Ok(Situation::NeedsAnswer(Reason::RemoteHasHistory));
        }
    }
    Ok(Situation::EmptyDirNoRemoteHistory)
}

fn needs_answer_message(reason: &Reason) -> String {
    format!(
        "{name}: cannot decide how to set up the repository: {reason}.\n\
         Choose one with --bootstrap <choice> or the {env}_BOOTSTRAP environment variable:\n  \
         clone  clone the remote branch into the directory\n  \
         init   initialise a repository and keep the local files\n  \
         abort  exit without changing anything",
        name = brand::NAME,
        reason = reason.describe(),
        env = brand::ENV_PREFIX,
    )
}

fn action_for(choice: Choice) -> Action {
    match choice {
        Choice::Clone => Action::Clone,
        Choice::Init => Action::Init,
        Choice::Abort => Action::Abort,
    }
}

/// Turn a situation into an action. Unambiguous situations need no answer; otherwise
/// the flag decides, then the terminal, and without either the caller must exit with
/// `ResolveError::exit_code`.
pub fn resolve(
    situation: Situation,
    flag: Option<Choice>,
    terminal: Option<&mut Terminal<'_>>,
) -> Result<Action, ResolveError> {
    let reason = match situation {
        Situation::ExistingRepo => return Ok(Action::UseExisting),
        Situation::EmptyDirNoRemoteHistory => return Ok(Action::Init),
        Situation::NeedsAnswer(reason) => reason,
    };
    if let Some(choice) = flag {
        return Ok(action_for(choice));
    }
    let Some(term) = terminal else {
        return Err(ResolveError::NeedsAnswer(needs_answer_message(&reason)));
    };
    loop {
        let prompt = format!(
            "{}.\nWhat should {} do?\n  clone  clone the remote branch into the directory\n  init   initialise a repository and keep the local files\n  abort  exit without changing anything\nChoice [clone/init/abort]: ",
            reason.describe(),
            brand::NAME
        );
        term.output
            .write_all(prompt.as_bytes())
            .and_then(|()| term.output.flush())
            .map_err(|e| ResolveError::Other(e.into()))?;
        let mut line = String::new();
        let read = term
            .input
            .read_line(&mut line)
            .map_err(|e| ResolveError::Other(e.into()))?;
        if read == 0 {
            return Err(ResolveError::NeedsAnswer(needs_answer_message(&reason)));
        }
        if let Some(choice) = Choice::parse(&line) {
            return Ok(action_for(choice));
        }
        let _ = writeln!(term.output, "Please answer clone, init or abort.");
    }
}

/// Read the answer from `--bootstrap <choice>` / `--bootstrap=<choice>`, falling back to
/// the `<PREFIX>_BOOTSTRAP` environment variable. The flag wins.
pub fn bootstrap_flag(
    args: &[String],
    env: &dyn Fn(&str) -> Option<String>,
) -> Result<Option<Choice>> {
    let mut value: Option<String> = None;
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        if arg == "--bootstrap" {
            value = Some(
                iter.next()
                    .context("--bootstrap needs a value: clone, init or abort")?
                    .clone(),
            );
        } else if let Some(v) = arg.strip_prefix("--bootstrap=") {
            value = Some(v.to_string());
        }
    }
    let value = value.or_else(|| {
        env(&format!("{}_BOOTSTRAP", brand::ENV_PREFIX)).filter(|v| !v.trim().is_empty())
    });
    match value {
        None => Ok(None),
        Some(text) => match Choice::parse(&text) {
            Some(choice) => Ok(Some(choice)),
            None => bail!("invalid bootstrap choice {text:?}: use clone, init or abort"),
        },
    }
}

/// Carry out the action and return the repository, with the configured remote added.
pub fn apply(cfg: &Config, action: Action) -> Result<Repo> {
    let path = &cfg.server.repo_path;
    let push = cfg.push.as_ref().filter(|p| p.url.is_some());
    match action {
        Action::Abort => bail!("abort is not an action that sets up a repository"),
        Action::UseExisting | Action::Init => {}
        Action::Clone => {
            let push = push.context("cloning needs a [push] section with a url")?;
            if !dir_is_empty(path)? {
                bail!(
                    "{} is not empty; refusing to clone into it. Choose init to keep its files",
                    path.display()
                );
            }
            std::fs::create_dir_all(path)?;
            let rejected = RefCell::new(None);
            let mut callbacks = RemoteCallbacks::new();
            callbacks.credentials(auth::credentials_callback(&push.auth));
            callbacks.certificate_check(auth::certificate_callback(&push.auth, &rejected));
            let mut fetch = FetchOptions::new();
            fetch.remote_callbacks(callbacks);
            let remote_name = push.remote.clone();
            let mut builder = RepoBuilder::new();
            builder
                .branch(&push.branch)
                .fetch_options(fetch)
                .remote_create(move |repo, _name, url| repo.remote(&remote_name, url));
            builder
                .clone(push.url.as_deref().unwrap_or_default(), path)
                .with_context(|| match rejected.borrow().as_ref() {
                    Some(why) => format!("clone failed: {why}"),
                    None => "clone failed".to_string(),
                })?;
        }
    }
    let repo = Repo::open_or_init(path, branch_of(cfg))?;
    if let Some(push) = push {
        repo.ensure_remote(&push.remote, push.url.as_deref().unwrap_or_default())?;
    }
    Ok(repo)
}
