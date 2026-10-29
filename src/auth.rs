//! HTTP Basic authentication for the single configured user.

use argon2::password_hash::phc::PasswordHash;
use argon2::password_hash::{PasswordHasher, PasswordVerifier};
use argon2::Argon2;
use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use subtle::ConstantTimeEq;

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
