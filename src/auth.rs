//! HTTP Basic authentication for the single configured user.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use argon2::password_hash::phc::PasswordHash;
use argon2::password_hash::{PasswordHasher, PasswordVerifier};
use argon2::Argon2;
use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;
use tokio::sync::Semaphore;
use tokio::time::Instant;

use crate::config::{AuthConfig, PasswordCheck};

fn ct_eq(a: &str, b: &str) -> bool {
    a.as_bytes().ct_eq(b.as_bytes()).into()
}

fn credentials(header: &str) -> Option<(String, String)> {
    let (scheme, encoded) = header.split_once(' ')?;
    if !scheme.eq_ignore_ascii_case("basic") {
        return None;
    }
    let decoded = STANDARD.decode(encoded.trim()).ok()?;
    let text = String::from_utf8(decoded).ok()?;
    let (user, pass) = text.split_once(':')?;
    Some((user.to_string(), pass.to_string()))
}

/// An argon2id hash of `password`, in the PHC string format `auth.password_hash` takes.
pub fn hash_password(password: &str) -> Result<String, argon2::password_hash::Error> {
    Ok(Argon2::default()
        .hash_password(password.as_bytes())?
        .to_string())
}

/// How long a verified login is remembered, so repeated requests carrying the same
/// `Authorization` header do not each pay for an argon2 hash.
const REMEMBER_FOR: Duration = Duration::from_secs(300);
/// Most logins remembered at once.
pub const MAX_REMEMBERED: usize = 128;
/// Most argon2 verifications running at once. Each uses about 19 MiB and a CPU core's
/// worth of time, so this also bounds what a flood of guesses can cost.
const MAX_PARALLEL_VERIFICATIONS: usize = 4;

/// Checks requests against the configured login. Plain passwords are compared directly.
/// Argon2 hashes are expensive, so they are verified off the async worker threads, a few at
/// a time, and a successful login is remembered for a few minutes. A wrong login is never
/// remembered, so every guess pays for a full verification.
pub struct Authenticator {
    cfg: Arc<AuthConfig>,
    salt: [u8; 16],
    remembered: Mutex<HashMap<[u8; 32], Instant>>,
    slots: Arc<Semaphore>,
    verifications: AtomicU64,
    max_remembered: usize,
}

impl Authenticator {
    pub fn new(cfg: AuthConfig) -> Self {
        Self::with_capacity(cfg, MAX_REMEMBERED)
    }

    fn with_capacity(cfg: AuthConfig, max_remembered: usize) -> Self {
        Authenticator {
            cfg: Arc::new(cfg),
            salt: uuid::Uuid::new_v4().into_bytes(),
            remembered: Mutex::new(HashMap::new()),
            slots: Arc::new(Semaphore::new(MAX_PARALLEL_VERIFICATIONS)),
            verifications: AtomicU64::new(0),
            max_remembered,
        }
    }

    /// How many argon2 verifications have run (for tests and diagnostics).
    pub fn verifications(&self) -> u64 {
        self.verifications.load(Ordering::Relaxed)
    }

    /// How many logins are currently remembered.
    pub fn remembered(&self) -> usize {
        self.remembered
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .len()
    }

    fn key(&self, header: &str) -> [u8; 32] {
        let mut hasher = Sha256::new();
        hasher.update(self.salt);
        hasher.update(header.as_bytes());
        hasher.finalize().into()
    }

    pub async fn check(self: &Arc<Self>, header: Option<&str>) -> bool {
        let Some(header) = header else {
            return false;
        };
        if matches!(self.cfg.password, PasswordCheck::Plain(_)) {
            return check(&self.cfg, Some(header));
        }

        let key = self.key(header);
        {
            let mut map = self.remembered.lock().unwrap_or_else(|p| p.into_inner());
            match map.get(&key) {
                Some(expires) if *expires > Instant::now() => return true,
                Some(_) => {
                    map.remove(&key);
                }
                None => {}
            }
        }

        let Ok(permit) = self.slots.clone().acquire_owned().await else {
            return false;
        };
        self.verifications.fetch_add(1, Ordering::Relaxed);
        let (cfg, header) = (self.cfg.clone(), header.to_string());
        let ok = tokio::task::spawn_blocking(move || {
            let _permit = permit;
            check(&cfg, Some(&header))
        })
        .await
        .unwrap_or(false);

        if ok {
            let mut map = self.remembered.lock().unwrap_or_else(|p| p.into_inner());
            let now = Instant::now();
            map.retain(|_, expires| *expires > now);
            if map.len() >= self.max_remembered {
                if let Some(oldest) = map.iter().min_by_key(|(_, e)| **e).map(|(k, _)| *k) {
                    map.remove(&oldest);
                }
            }
            map.insert(key, now + REMEMBER_FOR);
        }
        ok
    }
}

/// True if the `Authorization` header carries the configured username and password.
/// Both the username and the password are always checked, in constant time.
pub fn check(auth: &AuthConfig, header: Option<&str>) -> bool {
    let Some((user, pass)) = header.and_then(credentials) else {
        return false;
    };
    let user_ok = ct_eq(&user, &auth.username);
    let pass_ok = match &auth.password {
        PasswordCheck::Plain(secret) => ct_eq(&pass, secret.expose()),
        PasswordCheck::Argon2(hash) => PasswordHash::new(hash)
            .map(|parsed| {
                Argon2::default()
                    .verify_password(pass.as_bytes(), &parsed)
                    .is_ok()
            })
            .unwrap_or(false),
    };
    user_ok & pass_ok
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{AuthConfig, PasswordCheck, Secret};
    use base64::engine::general_purpose::STANDARD;
    use base64::Engine;

    fn plain() -> AuthConfig {
        AuthConfig {
            username: "notes".into(),
            password: PasswordCheck::Plain(Secret::new("pw".into())),
            failed_login_delay: std::time::Duration::ZERO,
        }
    }

    fn basic(user: &str, pass: &str) -> String {
        format!("Basic {}", STANDARD.encode(format!("{user}:{pass}")))
    }

    #[test]
    fn accepts_correct_credentials() {
        assert!(check(&plain(), Some(&basic("notes", "pw"))));
    }

    #[test]
    fn scheme_is_case_insensitive() {
        let h = basic("notes", "pw").replacen("Basic", "bAsIc", 1);
        assert!(check(&plain(), Some(&h)));
    }

    #[test]
    fn rejects_wrong_user_wrong_password_and_both() {
        assert!(!check(&plain(), Some(&basic("other", "pw"))));
        assert!(!check(&plain(), Some(&basic("notes", "nope"))));
        assert!(!check(&plain(), Some(&basic("other", "nope"))));
    }

    #[test]
    fn rejects_malformed_headers() {
        assert!(!check(&plain(), None));
        assert!(!check(&plain(), Some("")));
        assert!(!check(&plain(), Some("Bearer abc")));
        assert!(!check(&plain(), Some("Basic !!!not-base64")));
        assert!(!check(
            &plain(),
            Some(&format!("Basic {}", STANDARD.encode("nocolon")))
        ));
    }

    #[test]
    fn password_may_contain_colons() {
        let auth = AuthConfig {
            username: "u".into(),
            password: PasswordCheck::Plain(Secret::new("a:b:c".into())),
            failed_login_delay: std::time::Duration::ZERO,
        };
        assert!(check(&auth, Some(&basic("u", "a:b:c"))));
    }

    #[test]
    fn argon2_hash_is_verified() {
        let hash = argon2::Argon2::default()
            .hash_password(b"pw")
            .unwrap()
            .to_string();
        let auth = AuthConfig {
            username: "notes".into(),
            password: PasswordCheck::Argon2(hash),
            failed_login_delay: std::time::Duration::ZERO,
        };
        assert!(check(&auth, Some(&basic("notes", "pw"))));
        assert!(!check(&auth, Some(&basic("notes", "wrong"))));
    }

    #[test]
    fn hash_password_produces_a_hash_that_check_accepts() {
        let hash = hash_password("correct horse").unwrap();
        assert!(hash.starts_with("$argon2"), "{hash}");
        let auth = AuthConfig {
            username: "notes".into(),
            password: PasswordCheck::Argon2(hash),
            failed_login_delay: std::time::Duration::ZERO,
        };
        assert!(check(&auth, Some(&basic("notes", "correct horse"))));
        assert!(!check(&auth, Some(&basic("notes", "correct horse "))));
    }

    fn argon2_auth() -> std::sync::Arc<Authenticator> {
        let hash = hash_password("pw").unwrap();
        std::sync::Arc::new(Authenticator::new(AuthConfig {
            username: "notes".into(),
            password: PasswordCheck::Argon2(hash),
            failed_login_delay: std::time::Duration::ZERO,
        }))
    }

    #[tokio::test]
    async fn the_same_good_login_is_verified_once_not_on_every_request() {
        let auth = argon2_auth();
        let header = basic("notes", "pw");
        for _ in 0..6 {
            assert!(auth.check(Some(&header)).await);
        }
        assert_eq!(auth.verifications(), 1);
    }

    #[tokio::test]
    async fn a_wrong_password_is_checked_every_time_and_never_remembered() {
        let auth = argon2_auth();
        let header = basic("notes", "nope");
        for _ in 0..3 {
            assert!(!auth.check(Some(&header)).await);
        }
        assert_eq!(auth.verifications(), 3);
        // and a good login right after still works and is verified on its own
        assert!(auth.check(Some(&basic("notes", "pw"))).await);
        assert_eq!(auth.verifications(), 4);
    }

    #[tokio::test(start_paused = true)]
    async fn a_remembered_login_expires() {
        let auth = argon2_auth();
        let header = basic("notes", "pw");
        assert!(auth.check(Some(&header)).await);
        tokio::time::advance(std::time::Duration::from_secs(60)).await;
        assert!(auth.check(Some(&header)).await);
        assert_eq!(auth.verifications(), 1, "still remembered after a minute");
        tokio::time::advance(std::time::Duration::from_secs(10 * 60)).await;
        assert!(auth.check(Some(&header)).await);
        assert_eq!(auth.verifications(), 2, "verified again after it expired");
    }

    #[tokio::test]
    async fn plain_passwords_never_trigger_a_hash_verification() {
        let auth = std::sync::Arc::new(Authenticator::new(plain()));
        assert!(auth.check(Some(&basic("notes", "pw"))).await);
        assert!(!auth.check(Some(&basic("notes", "x"))).await);
        assert!(!auth.check(None).await);
        assert_eq!(auth.verifications(), 0);
    }

    #[tokio::test]
    async fn many_simultaneous_wrong_guesses_all_get_answered() {
        let auth = argon2_auth();
        let tasks: Vec<_> = (0..8)
            .map(|i| {
                let auth = auth.clone();
                tokio::spawn(async move {
                    auth.check(Some(&basic("notes", &format!("guess{i}"))))
                        .await
                })
            })
            .collect();
        for t in tasks {
            assert!(!t.await.unwrap());
        }
        assert_eq!(auth.verifications(), 8);
    }

    #[tokio::test]
    async fn the_remembered_logins_are_bounded() {
        let auth = std::sync::Arc::new(Authenticator::with_capacity(
            AuthConfig {
                username: "notes".into(),
                password: PasswordCheck::Argon2(hash_password("pw").unwrap()),
                failed_login_delay: std::time::Duration::ZERO,
            },
            3,
        ));
        // many distinct but valid headers (extra spaces after "Basic") must not grow the
        // memory without limit
        for i in 0..8 {
            let header = format!("Basic{}{}", " ".repeat(i + 1), STANDARD.encode("notes:pw"));
            assert!(auth.check(Some(&header)).await);
        }
        assert_eq!(auth.remembered(), 3);
    }

    #[test]
    fn garbage_argon2_hash_never_authenticates() {
        let auth = AuthConfig {
            username: "notes".into(),
            password: PasswordCheck::Argon2("not a hash".into()),
            failed_login_delay: std::time::Duration::ZERO,
        };
        assert!(!check(&auth, Some(&basic("notes", "not a hash"))));
    }
}
