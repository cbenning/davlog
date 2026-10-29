//! Every place the project name appears in code. Change the name here.

pub const NAME: &str = "davgit";
pub const ENV_PREFIX: &str = "DAVGIT";
pub const DEFAULT_CONFIG_PATH: &str = "/etc/davgit/davgit.toml";
pub const USER_AGENT: &str = concat!("davgit/", env!("CARGO_PKG_VERSION"));
pub const DEFAULT_AUTHOR_NAME: &str = "davgit";
pub const DEFAULT_AUTHOR_EMAIL: &str = "davgit@localhost";
pub const DEFAULT_MESSAGE_TEMPLATE: &str = "davgit: {summary}";
/// Directory inside `.git` that holds upload temp files.
pub const TMP_DIR_NAME: &str = "davgit-tmp";
