//! Application configuration: TOML file plus environment overrides for secrets.
//!
//! Parsing is two-stage. Serde fills the `Raw*` structs (unknown keys are errors),
//! then `Config::parse` applies defaults, resolves secrets and validates.

use std::collections::BTreeMap;
use std::fmt;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use bytesize::ByteSize;
use serde::Deserialize;

use crate::brand;

const DEFAULT_MAX_FILE_SIZE: u64 = 100 * 1024 * 1024;
const DEFAULT_MIN_FREE_SPACE: u64 = 100 * 1024 * 1024;
const DEFAULT_MAX_CONNECTIONS: usize = 256;

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("cannot read {}: {source}", path.display())]
    Read {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("invalid config: {0}")]
    Parse(String),
    #[error("invalid config: {0}")]
    Invalid(String),
}

/// A secret value. `Debug` never prints it.
#[derive(Clone, PartialEq, Eq)]
pub struct Secret(String);

impl Secret {
    pub fn new(value: String) -> Self {
        Secret(value)
    }

    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret(<redacted>)")
    }
}

#[derive(Debug, Clone)]
pub struct Config {
    pub server: ServerConfig,
    pub limits: LimitsConfig,
    pub auth: AuthConfig,
    pub commit: CommitConfig,
    pub push: Option<PushConfig>,
    pub git: GitConfig,
    pub maintenance: MaintenanceConfig,
    pub notify: NotifyConfig,
}

#[derive(Debug, Clone)]
pub struct ServerConfig {
    pub listen: SocketAddr,
    pub repo_path: PathBuf,
    /// 0 means one worker per CPU.
    pub worker_threads: usize,
}

#[derive(Debug, Clone)]
pub struct LimitsConfig {
    /// `None` means unlimited.
    pub max_file_size: Option<u64>,
    /// Writes that add data get 507 when the disk has less free space than this. 0 = off.
    pub min_free_space: u64,
    /// Writes that add data get 507 when the working tree plus `.git` is bigger than this.
    pub max_repo_size: Option<u64>,
    /// Simultaneous connections served; further ones wait. 0 = unlimited.
    pub max_connections: usize,
    /// How long a client gets to send a request's headers (also bounds idle keep-alive).
    pub header_read_timeout: Duration,
    /// How long an upload may stall between chunks, or a client may stop reading a
    /// response, before the connection is cut off.
    pub body_idle_timeout: Duration,
}

#[derive(Debug, Clone)]
pub enum PasswordCheck {
    Plain(Secret),
    /// A PHC-format argon2 hash.
    Argon2(String),
}

#[derive(Debug, Clone)]
pub struct AuthConfig {
    pub username: String,
    pub password: PasswordCheck,
    /// Pause before answering a wrong password. Slows guessing, but holds the connection
    /// while it waits. Zero means no delay.
    pub failed_login_delay: Duration,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CommitMode {
    PerRequest,
    Debounce,
    Interval,
}

#[derive(Debug, Clone)]
pub struct CommitConfig {
    pub mode: CommitMode,
    pub debounce: Duration,
    pub max_wait: Duration,
    pub interval: Duration,
    pub author_name: String,
    pub author_email: String,
    pub message_template: String,
    pub wait_for_commit: bool,
}

impl Default for CommitConfig {
    fn default() -> Self {
        CommitConfig {
            mode: CommitMode::PerRequest,
            debounce: Duration::from_secs(5),
            max_wait: Duration::from_secs(60),
            interval: Duration::from_secs(300),
            author_name: brand::DEFAULT_AUTHOR_NAME.to_string(),
            author_email: brand::DEFAULT_AUTHOR_EMAIL.to_string(),
            message_template: brand::DEFAULT_MESSAGE_TEMPLATE.to_string(),
            wait_for_commit: true,
        }
    }
}

#[derive(Debug, Clone)]
pub enum PushAuth {
    None,
    SshKey {
        path: PathBuf,
        passphrase: Option<Secret>,
        known_hosts: PathBuf,
    },
    Token {
        token: Secret,
    },
}

#[derive(Debug, Clone)]
pub struct PushConfig {
    pub remote: String,
    pub url: Option<String>,
    pub branch: String,
    pub interval: Duration,
    pub auth: PushAuth,
}

/// Extra git settings written to the repo-local config at startup.
#[derive(Debug, Clone, Default)]
pub struct GitConfig {
    pub config: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EmailTls {
    Starttls,
    Implicit,
    None,
}

#[derive(Debug, Clone)]
pub struct EmailConfig {
    pub smtp_host: String,
    pub smtp_port: u16,
    pub tls: EmailTls,
    pub username: String,
    pub password: Secret,
    pub from: String,
    pub to: Vec<String>,
}

/// Background repository upkeep (packing loose objects).
#[derive(Debug, Clone)]
pub struct MaintenanceConfig {
    pub enabled: bool,
    /// How often to check whether a repack is due.
    pub interval: Duration,
    /// Repack when there are more loose objects than this.
    pub loose_object_limit: usize,
    /// Repack when there are more packs than this.
    pub pack_limit: usize,
}

#[derive(Debug, Clone)]
pub struct NotifyConfig {
    pub rate_limit: Duration,
    pub email: Vec<EmailConfig>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Raw {
    server: RawServer,
    #[serde(default)]
    limits: RawLimits,
    auth: RawAuth,
    #[serde(default)]
    commit: RawCommit,
    push: Option<RawPush>,
    #[serde(default)]
    git: RawGit,
    #[serde(default)]
    maintenance: RawMaintenance,
    #[serde(default)]
    notify: RawNotify,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawServer {
    listen: SocketAddr,
    repo_path: PathBuf,
    #[serde(default)]
    worker_threads: usize,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct RawLimits {
    max_file_size: Option<ByteSize>,
    min_free_space: Option<ByteSize>,
    max_repo_size: Option<ByteSize>,
    max_connections: Option<usize>,
    #[serde(default, with = "humantime_serde")]
    header_read_timeout: Option<Duration>,
    #[serde(default, with = "humantime_serde")]
    body_idle_timeout: Option<Duration>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawAuth {
    username: String,
    password_file: Option<PathBuf>,
    password_hash: Option<String>,
    #[serde(default, with = "humantime_serde")]
    failed_login_delay: Option<Duration>,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct RawMaintenance {
    enabled: Option<bool>,
    #[serde(default, with = "humantime_serde")]
    interval: Option<Duration>,
    loose_object_limit: Option<usize>,
    pack_limit: Option<usize>,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct RawCommit {
    mode: Option<CommitMode>,
    #[serde(default, with = "humantime_serde")]
    debounce: Option<Duration>,
    #[serde(default, with = "humantime_serde")]
    max_wait: Option<Duration>,
    #[serde(default, with = "humantime_serde")]
    interval: Option<Duration>,
    author_name: Option<String>,
    author_email: Option<String>,
    message_template: Option<String>,
    wait_for_commit: Option<bool>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawPush {
    enabled: Option<bool>,
    remote: Option<String>,
    url: Option<String>,
    branch: Option<String>,
    #[serde(default, with = "humantime_serde")]
    interval: Option<Duration>,
    ssh_key: Option<PathBuf>,
    ssh_passphrase_file: Option<PathBuf>,
    known_hosts: Option<PathBuf>,
    token_file: Option<PathBuf>,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct RawGit {
    #[serde(default)]
    config: BTreeMap<String, String>,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct RawNotify {
    #[serde(default, with = "humantime_serde")]
    rate_limit: Option<Duration>,
    #[serde(default)]
    email: Vec<RawEmail>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawEmail {
    smtp_host: String,
    smtp_port: Option<u16>,
    tls: Option<EmailTls>,
    username: String,
    password_file: Option<PathBuf>,
    from: String,
    to: Vec<String>,
}

type Env<'a> = &'a dyn Fn(&str) -> Option<String>;

fn env_secret(env: Env, name: &str) -> Option<Secret> {
    env(&format!("{}_{}", brand::ENV_PREFIX, name)).map(Secret::new)
}

fn file_secret(path: &Path) -> Result<Secret, ConfigError> {
    let text = std::fs::read_to_string(path).map_err(|source| ConfigError::Read {
        path: path.to_path_buf(),
        source,
    })?;
    Ok(Secret::new(text.trim_end_matches(['\r', '\n']).to_string()))
}

fn invalid<T>(msg: impl Into<String>) -> Result<T, ConfigError> {
    Err(ConfigError::Invalid(msg.into()))
}

fn validate_git_key(key: &str) -> Result<(), ConfigError> {
    let lower = key.to_ascii_lowercase();
    if !key.contains('.') || key.starts_with('.') || key.ends_with('.') {
        return invalid(format!(
            "git.config key `{key}` must look like `section.name`"
        ));
    }
    if lower.starts_with("remote.") {
        return invalid(format!(
            "git.config key `{key}` is not allowed: remote settings live in [push]"
        ));
    }
    if lower == "core.bare" || lower == "core.worktree" {
        return invalid(format!(
            "git.config key `{key}` is not allowed: it would break the server"
        ));
    }
    Ok(())
}

impl Config {
    pub fn load(path: &Path, env: Env) -> Result<Config, ConfigError> {
        let text = std::fs::read_to_string(path).map_err(|source| ConfigError::Read {
            path: path.to_path_buf(),
            source,
        })?;
        Config::parse(&text, env)
    }

    pub fn parse(text: &str, env: Env) -> Result<Config, ConfigError> {
        let raw: Raw = toml::from_str(text).map_err(|e| ConfigError::Parse(e.to_string()))?;

        let max_file_size = match raw.limits.max_file_size.map(|b| b.as_u64()) {
            None => Some(DEFAULT_MAX_FILE_SIZE),
            Some(0) => None,
            Some(n) => Some(n),
        };
        let limits = LimitsConfig {
            max_file_size,
            min_free_space: raw
                .limits
                .min_free_space
                .map_or(DEFAULT_MIN_FREE_SPACE, |b| b.as_u64()),
            max_repo_size: raw
                .limits
                .max_repo_size
                .map(|b| b.as_u64())
                .filter(|n| *n > 0),
            max_connections: raw
                .limits
                .max_connections
                .unwrap_or(DEFAULT_MAX_CONNECTIONS),
            header_read_timeout: raw
                .limits
                .header_read_timeout
                .unwrap_or(Duration::from_secs(30)),
            body_idle_timeout: raw
                .limits
                .body_idle_timeout
                .unwrap_or(Duration::from_secs(60)),
        };

        let password = if let Some(secret) = env_secret(env, "PASSWORD") {
            PasswordCheck::Plain(secret)
        } else {
            match (&raw.auth.password_file, &raw.auth.password_hash) {
                (Some(_), Some(_)) => {
                    return invalid("auth.password_file and auth.password_hash are both set")
                }
                (Some(path), None) => PasswordCheck::Plain(file_secret(path)?),
                (None, Some(hash)) => PasswordCheck::Argon2(hash.clone()),
                (None, None) => {
                    return invalid(format!(
                    "no auth password: set auth.password_file, auth.password_hash or {}_PASSWORD",
                    brand::ENV_PREFIX
                ))
                }
            }
        };

        if raw.auth.username.trim().is_empty() {
            return invalid("auth.username must not be empty");
        }
        match &password {
            PasswordCheck::Plain(secret) if secret.expose().is_empty() => {
                return invalid("the auth password must not be empty");
            }
            PasswordCheck::Argon2(hash) if hash.trim().is_empty() => {
                return invalid("auth.password_hash must not be empty");
            }
            _ => {}
        }

        let c = raw.commit;
        let d = CommitConfig::default();
        let commit = CommitConfig {
            mode: c.mode.unwrap_or(d.mode),
            debounce: c.debounce.unwrap_or(d.debounce),
            max_wait: c.max_wait.unwrap_or(d.max_wait),
            interval: c.interval.unwrap_or(d.interval),
            author_name: c.author_name.unwrap_or(d.author_name),
            author_email: c.author_email.unwrap_or(d.author_email),
            message_template: c.message_template.unwrap_or(d.message_template),
            wait_for_commit: c.wait_for_commit.unwrap_or(d.wait_for_commit),
        };

        if commit.interval.is_zero() {
            return invalid("commit.interval must be greater than zero");
        }
        if commit.author_name.trim().is_empty() {
            return invalid("commit.author_name must not be empty");
        }
        if commit.author_email.trim().is_empty() {
            return invalid("commit.author_email must not be empty");
        }

        let push = match raw.push {
            Some(p) if p.enabled != Some(false) => Some(build_push(p, env)?),
            _ => None,
        };

        for key in raw.git.config.keys() {
            validate_git_key(key)?;
        }

        let mut email = Vec::new();
        for (i, e) in raw.notify.email.into_iter().enumerate() {
            let password = match env_secret(env, "SMTP_PASSWORD") {
                Some(s) => s,
                None => match &e.password_file {
                    Some(path) => file_secret(path)?,
                    None => {
                        return invalid(format!(
                            "notify.email[{i}]: smtp password missing: set password_file or {}_SMTP_PASSWORD",
                            brand::ENV_PREFIX
                        ))
                    }
                },
            };
            if e.to.is_empty() {
                return invalid(format!("notify.email[{i}]: `to` must not be empty"));
            }
            let tls = e.tls.unwrap_or(EmailTls::Starttls);
            let smtp_port = e.smtp_port.unwrap_or(match tls {
                EmailTls::Starttls => 587,
                EmailTls::Implicit => 465,
                EmailTls::None => 25,
            });
            email.push(EmailConfig {
                smtp_host: e.smtp_host,
                smtp_port,
                tls,
                username: e.username,
                password,
                from: e.from,
                to: e.to,
            });
        }

        if raw.maintenance.interval.is_some_and(|d| d.is_zero()) {
            return invalid("maintenance.interval must be greater than zero");
        }

        Ok(Config {
            server: ServerConfig {
                listen: raw.server.listen,
                repo_path: raw.server.repo_path,
                worker_threads: raw.server.worker_threads,
            },
            limits,
            auth: AuthConfig {
                username: raw.auth.username,
                password,
                failed_login_delay: raw.auth.failed_login_delay.unwrap_or(Duration::ZERO),
            },
            commit,
            push,
            git: GitConfig {
                config: raw.git.config,
            },
            maintenance: MaintenanceConfig {
                enabled: raw.maintenance.enabled.unwrap_or(true),
                interval: raw
                    .maintenance
                    .interval
                    .unwrap_or(Duration::from_secs(6 * 3600)),
                loose_object_limit: raw.maintenance.loose_object_limit.unwrap_or(2000),
                pack_limit: raw.maintenance.pack_limit.unwrap_or(20),
            },
            notify: NotifyConfig {
                rate_limit: raw
                    .notify
                    .rate_limit
                    .unwrap_or(Duration::from_secs(24 * 3600)),
                email,
            },
        })
    }
}

fn build_push(p: RawPush, env: Env) -> Result<PushConfig, ConfigError> {
    let env_token = env_secret(env, "PUSH_TOKEN");
    if p.ssh_key.is_some() && (env_token.is_some() || p.token_file.is_some()) {
        return invalid("push.ssh_key and a push token are both set: choose one");
    }
    let token = match env_token {
        Some(s) => Some(s),
        None => p.token_file.as_deref().map(file_secret).transpose()?,
    };
    let auth = match (p.ssh_key, token) {
        (Some(_), Some(_)) => unreachable!("checked above"),
        (Some(path), None) => {
            let Some(known_hosts) = p.known_hosts else {
                return invalid("push.known_hosts is required with push.ssh_key");
            };
            let passphrase = match env_secret(env, "PUSH_SSH_PASSPHRASE") {
                Some(s) => Some(s),
                None => p
                    .ssh_passphrase_file
                    .as_deref()
                    .map(file_secret)
                    .transpose()?,
            };
            PushAuth::SshKey {
                path,
                passphrase,
                known_hosts,
            }
        }
        (None, Some(token)) => PushAuth::Token { token },
        (None, None) => PushAuth::None,
    };
    let interval = p.interval.unwrap_or(Duration::from_secs(600));
    if interval.is_zero() {
        return invalid("push.interval must be greater than zero");
    }
    Ok(PushConfig {
        remote: p.remote.unwrap_or_else(|| "origin".to_string()),
        url: p.url,
        branch: p.branch.unwrap_or_else(|| "main".to_string()),
        interval,
        auth,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn pw_env(key: &str) -> Option<String> {
        (key == "DAVLOG_PASSWORD").then(|| "hunter2".to_string())
    }

    fn no_env(_: &str) -> Option<String> {
        None
    }

    const BASE: &str = r#"
[server]
listen = "127.0.0.1:8080"
repo_path = "/srv/notes"

[auth]
username = "notes"
"#;

    fn parse(extra: &str) -> Result<Config, ConfigError> {
        Config::parse(&format!("{BASE}\n{extra}"), &pw_env)
    }

    #[test]
    fn defaults_match_design() {
        let cfg = parse("").unwrap();
        assert_eq!(cfg.commit.mode, CommitMode::PerRequest);
        assert!(cfg.commit.wait_for_commit);
        assert_eq!(cfg.limits.max_file_size, Some(100 * 1024 * 1024));
        assert!(cfg.push.is_none());
        assert_eq!(cfg.commit.author_name, crate::brand::DEFAULT_AUTHOR_NAME);
        assert_eq!(cfg.commit.author_email, crate::brand::DEFAULT_AUTHOR_EMAIL);
        assert_eq!(
            cfg.commit.message_template,
            crate::brand::DEFAULT_MESSAGE_TEMPLATE
        );
        assert_eq!(cfg.server.worker_threads, 0);
    }

    #[test]
    fn hardening_defaults() {
        let cfg = parse("").unwrap();
        assert_eq!(
            cfg.auth.failed_login_delay,
            Duration::ZERO,
            "no delay unless asked for"
        );
        assert_eq!(cfg.limits.min_free_space, 100 * 1024 * 1024);
        assert_eq!(cfg.limits.max_repo_size, None);
        assert_eq!(cfg.limits.max_connections, 256);
        assert_eq!(cfg.limits.header_read_timeout, Duration::from_secs(30));
        assert_eq!(cfg.limits.body_idle_timeout, Duration::from_secs(60));
        assert!(cfg.maintenance.enabled);
        assert_eq!(cfg.maintenance.interval, Duration::from_secs(6 * 3600));
        assert_eq!(cfg.maintenance.loose_object_limit, 2000);
        assert_eq!(cfg.maintenance.pack_limit, 20);
    }

    #[test]
    fn hardening_settings_parse() {
        let cfg = Config::parse(
            &format!(
                "{BASE}\nfailed_login_delay = \"250ms\"\n[limits]\nmin_free_space = \"1GiB\"\nmax_repo_size = \"20GiB\"\nmax_connections = 0\nheader_read_timeout = \"5s\"\nbody_idle_timeout = \"10s\"\n[maintenance]\nenabled = false\ninterval = \"1h\"\nloose_object_limit = 50\npack_limit = 3\n"
            ),
            &pw_env,
        )
        .unwrap();
        assert_eq!(cfg.auth.failed_login_delay, Duration::from_millis(250));
        assert_eq!(cfg.limits.min_free_space, 1024 * 1024 * 1024);
        assert_eq!(cfg.limits.max_repo_size, Some(20 * 1024 * 1024 * 1024));
        assert_eq!(cfg.limits.max_connections, 0);
        assert_eq!(cfg.limits.header_read_timeout, Duration::from_secs(5));
        assert!(!cfg.maintenance.enabled);
        assert_eq!(cfg.maintenance.interval, Duration::from_secs(3600));
        assert_eq!(cfg.maintenance.loose_object_limit, 50);
        assert_eq!(cfg.maintenance.pack_limit, 3);
    }

    #[test]
    fn min_free_space_and_max_repo_size_zero_mean_off() {
        let cfg = parse("[limits]\nmin_free_space = \"0\"\nmax_repo_size = \"0\"").unwrap();
        assert_eq!(cfg.limits.min_free_space, 0);
        assert_eq!(cfg.limits.max_repo_size, None);
    }

    #[test]
    fn an_empty_username_or_password_is_rejected() {
        let empty_pw = |k: &str| (k == "DAVLOG_PASSWORD").then(String::new);
        let err = Config::parse(BASE, &empty_pw).unwrap_err();
        assert!(err.to_string().contains("password"), "{err}");

        let err = Config::parse(
            &BASE.replace("username = \"notes\"", "username = \"\""),
            &pw_env,
        )
        .unwrap_err();
        assert!(err.to_string().contains("username"), "{err}");

        let err = Config::parse(&format!("{BASE}\npassword_hash = \"\"\n"), &no_env).unwrap_err();
        assert!(err.to_string().contains("password"), "{err}");

        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("pw");
        std::fs::write(&file, "\n").unwrap();
        let toml = format!("{BASE}\npassword_file = \"{}\"\n", file.display());
        let err = Config::parse(&toml, &no_env).unwrap_err();
        assert!(err.to_string().contains("password"), "{err}");
    }

    #[test]
    fn zero_intervals_are_rejected() {
        for (extra, key) in [
            ("[push]\nurl = \"x\"\ninterval = \"0s\"", "push.interval"),
            ("[maintenance]\ninterval = \"0s\"", "maintenance.interval"),
            ("[commit]\ninterval = \"0s\"", "commit.interval"),
        ] {
            let err = parse(extra).unwrap_err();
            assert!(err.to_string().contains(key), "{key}: {err}");
        }
    }

    #[test]
    fn an_empty_commit_author_is_rejected() {
        let err = parse("[commit]\nauthor_name = \" \"").unwrap_err();
        assert!(err.to_string().contains("author_name"), "{err}");
        let err = parse("[commit]\nauthor_email = \"\"").unwrap_err();
        assert!(err.to_string().contains("author_email"), "{err}");
    }

    #[test]
    fn max_file_size_zero_is_unlimited() {
        let cfg = parse("[limits]\nmax_file_size = \"0\"").unwrap();
        assert_eq!(cfg.limits.max_file_size, None);
    }

    #[test]
    fn push_enabled_false_yields_none() {
        let cfg = parse("[push]\nenabled = false\nurl = \"file:///tmp/x\"").unwrap();
        assert!(cfg.push.is_none());
    }

    #[test]
    fn push_section_gets_defaults() {
        let cfg = parse("[push]\nurl = \"file:///tmp/x\"").unwrap();
        let push = cfg.push.unwrap();
        assert_eq!(push.remote, "origin");
        assert_eq!(push.branch, "main");
        assert_eq!(push.interval, Duration::from_secs(600));
        assert_eq!(push.url.as_deref(), Some("file:///tmp/x"));
        assert!(matches!(push.auth, PushAuth::None));
    }

    #[test]
    fn debounce_mode_requires_valid_durations() {
        let cfg =
            parse("[commit]\nmode = \"debounce\"\ndebounce = \"5s\"\nmax_wait = \"60s\"").unwrap();
        assert_eq!(cfg.commit.mode, CommitMode::Debounce);
        assert_eq!(cfg.commit.debounce, Duration::from_secs(5));
        assert_eq!(cfg.commit.max_wait, Duration::from_secs(60));

        let err = parse("[commit]\nmode = \"debounce\"\ndebounce = \"banana\"").unwrap_err();
        assert!(err.to_string().contains("debounce"), "{err}");
    }

    #[test]
    fn env_secret_overrides_file() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("pw");
        std::fs::write(&file, "from-file\n").unwrap();
        let toml = format!(
            "[server]\nlisten = \"127.0.0.1:1\"\nrepo_path = \"/x\"\n[auth]\nusername = \"u\"\npassword_file = \"{}\"\n",
            file.display()
        );

        let from_file = Config::parse(&toml, &no_env).unwrap();
        match from_file.auth.password {
            PasswordCheck::Plain(s) => assert_eq!(s.expose(), "from-file"),
            other => panic!("unexpected {other:?}"),
        }

        let from_env = Config::parse(&toml, &pw_env).unwrap();
        match from_env.auth.password {
            PasswordCheck::Plain(s) => assert_eq!(s.expose(), "hunter2"),
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn password_hash_is_accepted() {
        let toml =
            format!("{BASE}\npassword_hash = \"$argon2id$v=19$m=19456,t=2,p=1$c2FsdHNhbHQ$abc\"\n");
        let cfg = Config::parse(&toml, &no_env).unwrap();
        assert!(matches!(cfg.auth.password, PasswordCheck::Argon2(_)));
    }

    #[test]
    fn missing_password_is_error() {
        let err = Config::parse(BASE, &no_env).unwrap_err();
        assert!(err.to_string().contains("password"), "{err}");
    }

    #[test]
    fn secret_debug_is_redacted() {
        let s = Secret::new("hunter2".to_string());
        assert!(!format!("{s:?}").contains("hunter2"));
    }

    #[test]
    fn unknown_key_is_rejected() {
        let err = parse("[commit]\nmodee = \"debounce\"").unwrap_err();
        assert!(err.to_string().contains("modee"), "{err}");
    }

    #[test]
    fn git_config_table_parses() {
        let cfg =
            parse("[git.config]\n\"core.autocrlf\" = \"false\"\n\"user.name\" = \"x\"").unwrap();
        assert_eq!(cfg.git.config.get("core.autocrlf").unwrap(), "false");
        assert_eq!(cfg.git.config.len(), 2);
    }

    #[test]
    fn git_config_rejects_remote_and_bare_keys() {
        for key in ["remote.origin.url", "core.bare", "core.worktree", "nodot"] {
            let err = parse(&format!("[git.config]\n\"{key}\" = \"v\"")).unwrap_err();
            assert!(err.to_string().contains(key), "{key}: {err}");
        }
    }

    #[test]
    fn push_ssh_requires_known_hosts() {
        let err = parse("[push]\nurl = \"git@h:r.git\"\nssh_key = \"/k\"").unwrap_err();
        assert!(err.to_string().contains("known_hosts"), "{err}");
    }

    #[test]
    fn push_ssh_and_token_together_is_error() {
        let err = parse(
            "[push]\nurl = \"x\"\nssh_key = \"/k\"\nknown_hosts = \"/kh\"\ntoken_file = \"/t\"",
        )
        .unwrap_err();
        assert!(err.to_string().contains("ssh_key"), "{err}");
    }

    #[test]
    fn email_section_parses() {
        let cfg = parse(
            r#"
[notify]
rate_limit = "12h"

[[notify.email]]
smtp_host = "smtp.example.com"
smtp_port = 587
tls = "starttls"
username = "alerts@example.com"
from = "davlog <alerts@example.com>"
to = ["me@example.com"]
"#,
        )
        .unwrap_err();
        // no SMTP password configured anywhere: must be an error naming it
        assert!(cfg.to_string().contains("smtp"), "{cfg}");

        let env = |k: &str| {
            (k == "DAVLOG_SMTP_PASSWORD" || k == "DAVLOG_PASSWORD").then(|| "p".to_string())
        };
        let cfg = Config::parse(
            &format!(
                "{BASE}\n[notify]\nrate_limit = \"12h\"\n[[notify.email]]\nsmtp_host = \"smtp.example.com\"\nsmtp_port = 587\ntls = \"starttls\"\nusername = \"a\"\nfrom = \"d <a@x>\"\nto = [\"me@example.com\"]\n"
            ),
            &env,
        )
        .unwrap();
        assert_eq!(cfg.notify.rate_limit, Duration::from_secs(12 * 3600));
        assert_eq!(cfg.notify.email.len(), 1);
        assert_eq!(cfg.notify.email[0].tls, EmailTls::Starttls);
    }

    #[test]
    fn example_config_parses() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("config.example.toml");
        let env = |k: &str| {
            (k == "DAVLOG_PASSWORD" || k == "DAVLOG_SMTP_PASSWORD").then(|| "p".to_string())
        };
        Config::load(&path, &env).unwrap();
    }
}
