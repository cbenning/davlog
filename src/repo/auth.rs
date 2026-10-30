//! Credentials and host-key checks for pushing and fetching.

use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use git2::cert::Cert;
use git2::{CertificateCheckStatus, Cred, CredentialType, Error, ErrorClass, ErrorCode};
use sha2::{Digest, Sha256};
use tracing::warn;

use crate::config::PushAuth;

/// Hostname without brackets or port: `[h]:22`, `h:22` and `h` all give `h`.
fn bare_host(host: &str) -> &str {
    if let Some(rest) = host.strip_prefix('[') {
        return rest.split(']').next().unwrap_or(rest);
    }
    match host.matches(':').count() {
        1 => host.split(':').next().unwrap_or(host),
        _ => host,
    }
}

/// True if `known_hosts` has an entry for `host` whose key hashes (SHA-256) to `sha256`.
/// Unknown hosts and changed keys never match. Hashed (`|1|...`) entries, `@revoked`
/// and `@cert-authority` markers are not supported and never match.
pub fn host_key_matches(known_hosts: &str, host: &str, sha256: &[u8; 32]) -> bool {
    let wanted = bare_host(host);
    known_hosts.lines().any(|line| {
        let line = line.trim();
        if line.is_empty()
            || line.starts_with('#')
            || line.starts_with('@')
            || line.starts_with('|')
        {
            return false;
        }
        let mut fields = line.split_whitespace();
        let (Some(hosts), Some(_keytype), Some(key)) =
            (fields.next(), fields.next(), fields.next())
        else {
            return false;
        };
        if !hosts.split(',').any(|h| bare_host(h) == wanted) {
            return false;
        }
        let Ok(blob) = STANDARD.decode(key) else {
            return false;
        };
        let digest: [u8; 32] = Sha256::digest(&blob).into();
        digest == *sha256
    })
}

pub fn has_hashed_entries(known_hosts: &str) -> bool {
    known_hosts
        .lines()
        .any(|l| l.trim_start().starts_with("|1|"))
}

/// Credentials for libgit2's callback. Gives up after a few attempts so a wrong key
/// cannot loop forever.
pub fn credentials_callback(
    auth: &PushAuth,
) -> impl FnMut(&str, Option<&str>, CredentialType) -> Result<Cred, Error> {
    let auth = auth.clone();
    let mut attempts = 0u32;
    move |_url, username_from_url, allowed| {
        attempts += 1;
        if attempts > 3 {
            return Err(Error::new(
                ErrorCode::Auth,
                ErrorClass::Ssh,
                "the configured credentials were rejected",
            ));
        }
        let user = username_from_url.unwrap_or("git");
        match &auth {
            PushAuth::SshKey {
                path, passphrase, ..
            } => {
                if allowed.contains(CredentialType::USERNAME) {
                    return Cred::username(user);
                }
                if allowed.contains(CredentialType::SSH_KEY) {
                    return Cred::ssh_key(
                        user,
                        None,
                        path,
                        passphrase.as_ref().map(|p| p.expose()),
                    );
                }
            }
            PushAuth::Token { token } => {
                if allowed.contains(CredentialType::USER_PASS_PLAINTEXT) {
                    return Cred::userpass_plaintext(user, token.expose());
                }
            }
            PushAuth::None => {}
        }
        Err(Error::new(
            ErrorCode::Auth,
            ErrorClass::Net,
            "the remote asked for credentials this configuration cannot provide",
        ))
    }
}

/// Host-key check for SSH remotes. Other transports use libgit2's own validation.
/// On rejection the reason is stored in `rejected`, because libgit2 reports only a
/// generic error for a failed callback.
pub fn certificate_callback<'a>(
    auth: &PushAuth,
    rejected: &'a std::cell::RefCell<Option<String>>,
) -> impl FnMut(&Cert<'_>, &str) -> Result<CertificateCheckStatus, Error> + 'a {
    let known_hosts = match auth {
        PushAuth::SshKey { known_hosts, .. } => Some(known_hosts.clone()),
        _ => None,
    };
    move |cert, host| {
        let (Some(path), Some(hostkey)) = (&known_hosts, cert.as_hostkey()) else {
            return Ok(CertificateCheckStatus::CertificatePassthrough);
        };
        let fail = |msg: String| {
            *rejected.borrow_mut() = Some(msg.clone());
            Err(Error::new(ErrorCode::Certificate, ErrorClass::Ssh, msg))
        };
        let text = match std::fs::read_to_string(path) {
            Ok(t) => t,
            Err(e) => return fail(format!("cannot read known_hosts {}: {e}", path.display())),
        };
        if has_hashed_entries(&text) {
            warn!("known_hosts contains hashed entries, which are not supported and are ignored");
        }
        let Some(sha256) = hostkey.hash_sha256() else {
            return fail("the server did not provide a SHA-256 host key hash".to_string());
        };
        if host_key_matches(&text, host, sha256) {
            Ok(CertificateCheckStatus::CertificateOk)
        } else {
            fail(format!(
                "host key for {host} is not in {} or has changed",
                path.display()
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::engine::general_purpose::STANDARD;
    use base64::Engine;
    use sha2::{Digest, Sha256};

    fn line(hosts: &str, key: &[u8]) -> String {
        format!("{hosts} ssh-ed25519 {}\n", STANDARD.encode(key))
    }

    fn fingerprint(key: &[u8]) -> [u8; 32] {
        Sha256::digest(key).into()
    }

    #[test]
    fn host_key_matches_plain_known_hosts_line() {
        let key = b"the host key blob";
        let file = format!("# comment\n\n{}", line("git.example.com,10.0.0.1", key));
        assert!(host_key_matches(
            &file,
            "git.example.com",
            &fingerprint(key)
        ));
        assert!(host_key_matches(&file, "10.0.0.1", &fingerprint(key)));
    }

    #[test]
    fn host_key_matches_bracketed_host_with_port() {
        let key = b"k";
        let file = line("[git.example.com]:2222", key);
        assert!(host_key_matches(
            &file,
            "git.example.com",
            &fingerprint(key)
        ));
        assert!(host_key_matches(
            &file,
            "git.example.com:2222",
            &fingerprint(key)
        ));
    }

    #[test]
    fn host_key_rejects_unknown_host() {
        let key = b"k";
        let file = line("git.example.com", key);
        assert!(!host_key_matches(
            &file,
            "evil.example.org",
            &fingerprint(key)
        ));
    }

    #[test]
    fn host_key_rejects_changed_key() {
        let file = line("git.example.com", b"original");
        assert!(!host_key_matches(
            &file,
            "git.example.com",
            &fingerprint(b"replaced")
        ));
    }

    #[test]
    fn hashed_and_marker_entries_never_match() {
        let key = b"k";
        let file = format!(
            "|1|c2FsdA==|aGFzaA== ssh-ed25519 {b}\n@revoked git.example.com ssh-ed25519 {b}\n@cert-authority git.example.com ssh-ed25519 {b}\n",
            b = STANDARD.encode(key)
        );
        assert!(!host_key_matches(
            &file,
            "git.example.com",
            &fingerprint(key)
        ));
        assert!(has_hashed_entries(&file));
        assert!(!has_hashed_entries("git.example.com ssh-ed25519 AAAA\n"));
    }

    #[test]
    fn garbage_lines_are_ignored() {
        let key = b"k";
        let file = format!(
            "only-one-field\nhost ssh-ed25519 !!!notbase64\n{}",
            line("h", key)
        );
        assert!(host_key_matches(&file, "h", &fingerprint(key)));
    }
}
