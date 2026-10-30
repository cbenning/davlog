//! Every place the project name appears in code. Change the name here.

pub const NAME: &str = "davlog";
pub const ENV_PREFIX: &str = "DAVLOG";
pub const DEFAULT_CONFIG_PATH: &str = "/etc/davlog/davlog.toml";
pub const USER_AGENT: &str = concat!("davlog/", env!("CARGO_PKG_VERSION"));
pub const DEFAULT_AUTHOR_NAME: &str = "davlog";
pub const DEFAULT_AUTHOR_EMAIL: &str = "davlog@localhost";
pub const DEFAULT_MESSAGE_TEMPLATE: &str = "davlog: {summary}";
/// Directory inside `.git` that holds upload temp files.
pub const TMP_DIR_NAME: &str = "davlog-tmp";
