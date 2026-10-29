//! Writing GitHub Actions secrets through the repository GitHub App
//! (docs/CONNECTIONS.md, "GitHub Actions secrets").
//!
//! GitHub accepts a secret only encrypted to the repository's (or
//! environment's) Actions public key with a libsodium sealed box
//! (X25519 + XSalsa20-Poly1305). The value is encrypted here, sent once, and
//! never returned, logged or included in an error. The App needs the
//! repository permission **Secrets: read and write**.
use super::{GitHub, api, full_name, text};
use anyhow::{Context, Result, ensure};
use base64::Engine;
use serde_json::{Value, json};

/// Largest value accepted (GitHub's own limit is 48 KB).
pub const MAX_VALUE_BYTES: usize = 16 * 1024;

/// A GitHub secret name: letters, digits and underscores, not starting with a
/// digit or `GITHUB_`, at most 200 characters (stored uppercase by GitHub; we
/// require uppercase so the name is exactly what workflows reference).
pub fn valid_secret_name(name: &str) -> bool {
    (1..=200).contains(&name.len())
        && !name.as_bytes()[0].is_ascii_digit()
        && !name.starts_with("GITHUB_")
        && name
            .bytes()
            .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_')
}
/// A deployment environment name.
pub fn valid_environment(name: &str) -> bool {
    (1..=255).contains(&name.len())
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
}

/// Encrypt `value` to a base64 Curve25519 public key as a libsodium sealed
/// box, returning base64 ciphertext.
pub fn seal(public_key_base64: &str, value: &[u8]) -> Result<String> {
    let engine = base64::engine::general_purpose::STANDARD;
    let key: [u8; 32] = engine
        .decode(public_key_base64)
        .ok()
        .and_then(|k| k.try_into().ok())
        .context("The repository public key is not a 32-byte Curve25519 key")?;
    let public = crypto_box::PublicKey::from(key);
    let sealed = public
        .seal(&mut crypto_box::aead::OsRng, value)
        .map_err(|_| anyhow::anyhow!("Secret encryption failed"))?;
    Ok(engine.encode(sealed))
}

impl GitHub {
    /// Create or update one Actions secret on a selected repository, or on
    /// one of its deployment environments, through an installation token
    /// limited to that repository and `secrets: write`.
    pub fn set_actions_secret(
        &self,
        repository: &str,
        environment: Option<&str>,
        name: &str,
        value: &str,
    ) -> Result<Value> {
        full_name(repository)?;
        ensure!(
            valid_secret_name(name),
            "Secret names use A-Z, 0-9 and _, do not start with a digit or GITHUB_, and have at most 200 characters"
        );
        if let Some(environment) = environment {
            ensure!(
                valid_environment(environment),
                "Environment names use A-Z a-z 0-9 . _ - (at most 255)"
            );
        }
        ensure!(
            !value.is_empty() && value.len() <= MAX_VALUE_BYTES,
            "Secret values must be 1-16384 bytes"
        );
        let selected = {
            let data = self.data.lock().unwrap();
            data.repositories
                .values()
                .find(|r| r.full_name == repository)
                .context("Select this repository in the GitHub panel first")?
                .clone()
        };
        let app = self.app("repos")?;
        let response = self.api.call(
            "POST",
            &format!(
                "/app/installations/{}/access_tokens",
                selected.installation_id
            ),
            &api::jwt(app)?,
            Some(json!({"repository_ids":[selected.id],"permissions":{"secrets":"write"}})),
        ).context("The GitHub App could not get secrets access; it needs the Secrets: read and write repository permission")?;
        let token = text(&response, "token")?.to_owned();
        let base = match environment {
            None => format!("/repos/{}/actions/secrets", selected.full_name),
            Some(environment) => format!(
                "/repos/{}/environments/{environment}/secrets",
                selected.full_name
            ),
        };
        let key = self
            .api
            .call("GET", &format!("{base}/public-key"), &token, None)?;
        let key_id = text(&key, "key_id")?.to_owned();
        ensure!(
            !key_id.is_empty() && key_id.len() <= 128,
            "Invalid repository public key"
        );
        let encrypted = seal(text(&key, "key")?, value.as_bytes())?;
        self.api.call(
            "PUT",
            &format!("{base}/{name}"),
            &token,
            Some(json!({"encrypted_value":encrypted,"key_id":key_id})),
        )?;
        Ok(json!({
            "repository": selected.full_name,
            "environment": environment,
            "name": name,
            "key_id": key_id,
            "set": true,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn sealed_boxes_open_with_the_fixture_key_and_never_contain_the_value() {
        // Fixture key pair; GitHub holds the secret half in production.
        let secret = crypto_box::SecretKey::from([7u8; 32]);
        let public =
            base64::engine::general_purpose::STANDARD.encode(secret.public_key().as_bytes());
        let value = "cf-deploy-token-value-0001";
        let sealed = seal(&public, value.as_bytes()).unwrap();
        assert!(!sealed.contains(value));
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(&sealed)
            .unwrap();
        // Sealed box: 32-byte ephemeral key + 16-byte tag + message.
        assert_eq!(bytes.len(), 32 + 16 + value.len());
        assert_eq!(secret.unseal(&bytes).unwrap(), value.as_bytes());
        // Each seal uses a fresh ephemeral key.
        assert_ne!(sealed, seal(&public, value.as_bytes()).unwrap());
        assert!(seal("not-base64!", b"x").is_err());
        assert!(
            seal(
                &base64::engine::general_purpose::STANDARD.encode([1u8; 16]),
                b"x"
            )
            .is_err()
        );
    }
    #[test]
    fn names_and_environments_are_validated() {
        assert!(valid_secret_name("CLOUDFLARE_API_TOKEN"));
        for bad in [
            "",
            "github_token",
            "GITHUB_TOKEN",
            "1TOKEN",
            "A-B",
            &"A".repeat(201),
        ] {
            assert!(!valid_secret_name(bad), "{bad}");
        }
        assert!(valid_environment("production") && !valid_environment("prod/../x"));
        let (_root, github) = crate::tests::service();
        let error = github
            .set_actions_secret("owner/repo", None, "TOKEN", "value-not-echoed")
            .unwrap_err()
            .to_string();
        assert!(error.contains("Select this repository") && !error.contains("value-not-echoed"));
    }
}
