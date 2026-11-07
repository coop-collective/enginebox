//! Encryption at rest for members' own subscriptions
//! (docs/MEMBER-SUBSCRIPTIONS.md, "Encryption at rest").
//!
//! A member credential's provider files (Claude's `.credentials.json`,
//! Codex's `auth.json`, a setup token) are kept in `<home>/sealed.json`:
//! each file AES-256-GCM encrypted under a per-credential data key, which is
//! itself encrypted under the key-encryption key (KEK). The KEK is 32 bytes
//! read from `ENGINEBOX_CREDENTIALS_KEY_FILE`, a file outside the state
//! directory, so a copy of the state (a backup, a leaked disk image) does not
//! reveal any token. Plaintext exists only while the provider's own client
//! signs in or refreshes, or while access material is read for a run, always
//! under the credential's lease, and is removed again right after.
use anyhow::{Context, Result, ensure};
use base64::{Engine, engine::general_purpose::STANDARD};
use enginebox_agents::{delegation, security};
use ring::{
    aead::{AES_256_GCM, Aad, LessSafeKey, NONCE_LEN, Nonce, UnboundKey},
    rand::{SecureRandom, SystemRandom},
};
use serde_json::{Value, json};
use std::path::Path;

/// Where the sealed files live inside a credential's home.
pub const SEALED: &str = "sealed.json";

/// The provider files that hold credentials, relative to a credential home.
pub fn secret_files() -> [&'static str; 3] {
    [
        delegation::native_file("claude"),
        delegation::native_file("codex"),
        delegation::TOKEN_FILE,
    ]
}

/// The key-encryption key.
#[derive(Clone)]
pub struct Kek([u8; 32]);
impl std::fmt::Debug for Kek {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Kek(..)")
    }
}
impl Kek {
    pub fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }
    /// Read a KEK file: exactly 32 raw bytes (not text) or exactly 64 hex
    /// characters (one trailing newline allowed). The file must be
    /// owner-only, and must not live in the state directory `state_root` or
    /// in the controller's install tree (three levels above its executable:
    /// `<prefix>/releases/<version>/bin`), which are backed up and copied.
    pub fn from_file(path: &Path, state_root: &Path) -> Result<Self> {
        ensure!(
            path.is_absolute(),
            "The credentials key file must be an absolute path."
        );
        let canonical = path
            .canonicalize()
            .context("The credentials key file is missing.")?;
        ensure!(
            !canonical.starts_with(state_root),
            "The credentials key file must live outside the state directory."
        );
        if let Some(install) = std::env::current_exe()
            .ok()
            .and_then(|e| e.canonicalize().ok())
            .and_then(|e| e.ancestors().nth(3).map(Path::to_path_buf))
            .filter(|p| p.components().count() > 2)
        {
            ensure!(
                !canonical.starts_with(&install),
                "The credentials key file must live outside the install tree."
            );
        }
        let meta = std::fs::metadata(&canonical)?;
        ensure!(
            meta.is_file(),
            "The credentials key file must be a regular file."
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            ensure!(
                meta.permissions().mode() & 0o077 == 0,
                "The credentials key file must be readable by its owner only."
            );
        }
        let raw = std::fs::read(&canonical)?;
        let text = raw.strip_suffix(b"\n").unwrap_or(&raw);
        let bytes: Vec<u8> = if text.len() == 64 && text.iter().all(u8::is_ascii_hexdigit) {
            let text = std::str::from_utf8(text)?;
            (0..32)
                .map(|i| u8::from_str_radix(&text[i * 2..i * 2 + 2], 16))
                .collect::<std::result::Result<_, _>>()?
        } else {
            // Raw bytes only: a 32-character text key would have a fraction
            // of the strength it seems to have.
            ensure!(
                raw.len() == 32 && !raw.iter().all(|b| b.is_ascii_graphic() || *b == b' '),
                "The credentials key must be 32 random bytes or 64 hex characters."
            );
            raw
        };
        Ok(Self(
            bytes
                .try_into()
                .map_err(|_| anyhow::anyhow!("Invalid key."))?,
        ))
    }
    /// From `ENGINEBOX_CREDENTIALS_KEY_FILE`, when set.
    pub fn from_env(state_root: &Path) -> Result<Option<Self>> {
        match std::env::var_os("ENGINEBOX_CREDENTIALS_KEY_FILE") {
            None => Ok(None),
            Some(path) => Ok(Some(Self::from_file(Path::new(&path), state_root)?)),
        }
    }
}

fn key(bytes: &[u8]) -> Result<LessSafeKey> {
    Ok(LessSafeKey::new(
        UnboundKey::new(&AES_256_GCM, bytes).map_err(|_| anyhow::anyhow!("Invalid key."))?,
    ))
}
fn seal_bytes(key: &LessSafeKey, aad: &str, plaintext: &[u8]) -> Result<String> {
    let mut nonce = [0u8; NONCE_LEN];
    SystemRandom::new()
        .fill(&mut nonce)
        .map_err(|_| anyhow::anyhow!("No randomness."))?;
    let mut buffer = plaintext.to_vec();
    key.seal_in_place_append_tag(
        Nonce::assume_unique_for_key(nonce),
        Aad::from(aad.as_bytes()),
        &mut buffer,
    )
    .map_err(|_| anyhow::anyhow!("Encryption failed."))?;
    let mut out = nonce.to_vec();
    out.extend(buffer);
    Ok(STANDARD.encode(out))
}
fn open_bytes(key: &LessSafeKey, aad: &str, sealed: &str) -> Result<Vec<u8>> {
    let raw = STANDARD.decode(sealed)?;
    ensure!(raw.len() > NONCE_LEN, "Sealed value is truncated.");
    let (nonce, rest) = raw.split_at(NONCE_LEN);
    let mut buffer = rest.to_vec();
    let plain = key
        .open_in_place(
            Nonce::try_assume_unique_for_key(nonce).map_err(|_| anyhow::anyhow!("Bad nonce."))?,
            Aad::from(aad.as_bytes()),
            &mut buffer,
        )
        .map_err(|_| {
            anyhow::anyhow!("A sealed credential could not be decrypted with this key.")
        })?;
    Ok(plain.to_vec())
}

/// Seal credential `id`'s provider files: `sealed.json` becomes exactly the
/// plaintext files now present (a file the provider's client removed, such
/// as an old setup token, is gone from it too), under a fresh data key; the
/// plaintext is then removed. Without plaintext, the sealed set is kept.
pub fn seal_home(kek: &Kek, id: &str, home: &Path) -> Result<()> {
    let mut files = std::collections::BTreeMap::new();
    for relative in secret_files() {
        if let Some(bytes) = security::read_bytes(home, relative, security::LIMIT, false)? {
            files.insert(relative.to_owned(), bytes);
        }
    }
    if files.is_empty() {
        return Ok(());
    }
    let mut dek = [0u8; 32];
    SystemRandom::new()
        .fill(&mut dek)
        .map_err(|_| anyhow::anyhow!("No randomness."))?;
    let data_key = key(&dek)?;
    let mut sealed = serde_json::Map::new();
    for (relative, bytes) in &files {
        sealed.insert(
            relative.clone(),
            json!(seal_bytes(
                &data_key,
                &format!("enginebox-member-file:{id}:{relative}"),
                bytes
            )?),
        );
    }
    let wrapped = seal_bytes(&key(&kek.0)?, &format!("enginebox-member-dek:{id}"), &dek)?;
    security::write_json(
        home,
        SEALED,
        &json!({"schema":1,"algorithm":"AES-256-GCM","wrapped_key":wrapped,"files":sealed}),
    )?;
    wipe_plaintext(home)
}

/// Remove every plaintext provider file from a credential home.
pub fn wipe_plaintext(home: &Path) -> Result<()> {
    for relative in secret_files() {
        security::remove(home, relative)?;
    }
    Ok(())
}

/// The names of the sealed files (no decryption needed).
pub fn sealed_names(home: &Path) -> Vec<String> {
    security::read_json(home, SEALED)
        .ok()
        .flatten()
        .and_then(|v| v["files"].as_object().map(|o| o.keys().cloned().collect()))
        .unwrap_or_default()
}

/// Credential `id`'s sealed files, decrypted in memory (none when nothing
/// is sealed). Fails on a wrong key or a blob of another credential.
pub fn open_home(
    kek: &Kek,
    id: &str,
    home: &Path,
) -> Result<Option<std::collections::BTreeMap<String, Vec<u8>>>> {
    match security::read_json(home, SEALED)? {
        None => Ok(None),
        Some(sealed) => open_all(kek, id, &sealed).map(Some),
    }
}

fn open_all(
    kek: &Kek,
    id: &str,
    sealed: &Value,
) -> Result<std::collections::BTreeMap<String, Vec<u8>>> {
    ensure!(sealed["schema"] == 1, "Unknown sealed credential format.");
    let dek = open_bytes(
        &key(&kek.0)?,
        &format!("enginebox-member-dek:{id}"),
        sealed["wrapped_key"]
            .as_str()
            .context("Sealed key missing.")?,
    )?;
    let data_key = key(&dek)?;
    let mut out = std::collections::BTreeMap::new();
    for (relative, value) in sealed["files"]
        .as_object()
        .context("Sealed files missing.")?
    {
        ensure!(
            secret_files().contains(&relative.as_str()),
            "Unknown sealed credential file."
        );
        out.insert(
            relative.clone(),
            open_bytes(
                &data_key,
                &format!("enginebox-member-file:{id}:{relative}"),
                value.as_str().context("Sealed file missing.")?,
            )?,
        );
    }
    Ok(out)
}

/// Write credential `id`'s sealed files back as plaintext for the provider's
/// own client (a refresh, or reading access material). The caller holds the
/// credential's lease and calls [`seal_home`] when done, also on failure.
pub fn unseal_home(kek: &Kek, id: &str, home: &Path) -> Result<()> {
    let Some(files) = open_home(kek, id, home)? else {
        return Ok(());
    };
    for (relative, bytes) in files {
        security::write_bytes(home, &relative, &bytes, security::LIMIT)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn sealed_homes_hold_no_plaintext_and_need_the_right_key() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().canonicalize().unwrap().join("home");
        security::private_dir(&home).unwrap();
        let kek = Kek::from_bytes([7; 32]);
        let secret = "sk-ant-oat01-plaintext-0123456789";
        security::write_json(
            &home,
            ".claude/.credentials.json",
            &json!({"claudeAiOauth":{"accessToken":secret,"refreshToken":"rt-plaintext-0123456789"}}),
        )
        .unwrap();
        seal_home(&kek, "a".repeat(32).as_str(), &home).unwrap();
        assert!(!home.join(".claude/.credentials.json").exists());
        let on_disk = std::fs::read_to_string(home.join(SEALED)).unwrap();
        assert!(!on_disk.contains(secret) && !on_disk.contains("rt-plaintext"));
        // The wrong key, or another credential's ID (a swapped blob), fails.
        assert!(unseal_home(&Kek::from_bytes([8; 32]), &"a".repeat(32), &home).is_err());
        assert!(unseal_home(&kek, &"b".repeat(32), &home).is_err());
        assert!(!home.join(".claude/.credentials.json").exists());
        unseal_home(&kek, &"a".repeat(32), &home).unwrap();
        let back = security::read_json(&home, ".claude/.credentials.json")
            .unwrap()
            .unwrap();
        assert_eq!(back["claudeAiOauth"]["accessToken"], secret);
        seal_home(&kek, &"a".repeat(32), &home).unwrap();
        assert!(!home.join(".claude/.credentials.json").exists());
    }
    #[test]
    fn the_key_file_lives_outside_the_state_and_is_private() {
        use std::os::unix::fs::PermissionsExt;
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let state = root.join("state");
        std::fs::create_dir(&state).unwrap();
        let inside = state.join("kek");
        let outside = root.join("kek");
        for path in [&inside, &outside] {
            std::fs::write(path, "ab".repeat(32)).unwrap();
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        assert!(Kek::from_file(&inside, &state).is_err());
        assert!(Kek::from_file(&outside, &state).is_ok());
        std::fs::set_permissions(&outside, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(Kek::from_file(&outside, &state).is_err());
    }
    #[test]
    fn sealing_replaces_the_set_and_keys_are_strict() {
        use std::os::unix::fs::PermissionsExt;
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let home = root.join("home");
        security::private_dir(&home).unwrap();
        let kek = Kek::from_bytes([7; 32]);
        let id = "c".repeat(32);
        security::write_json(
            &home,
            delegation::TOKEN_FILE,
            &json!({"value":"old-token-0123456789"}),
        )
        .unwrap();
        security::write_json(
            &home,
            ".claude/.credentials.json",
            &json!({"claudeAiOauth":{}}),
        )
        .unwrap();
        seal_home(&kek, &id, &home).unwrap();
        assert_eq!(sealed_names(&home).len(), 2);
        // A later login without the old token: sealing drops it.
        unseal_home(&kek, &id, &home).unwrap();
        security::remove(&home, delegation::TOKEN_FILE).unwrap();
        seal_home(&kek, &id, &home).unwrap();
        assert_eq!(
            sealed_names(&home),
            vec![".claude/.credentials.json".to_owned()]
        );
        let files = open_home(&kek, &id, &home).unwrap().unwrap();
        assert!(!files.contains_key(delegation::TOKEN_FILE));
        // Keys: 64 hex characters or 32 non-text bytes; never 32 characters of text.
        let state = root.join("state");
        std::fs::create_dir(&state).unwrap();
        let file = root.join("key");
        let write = |bytes: &[u8]| {
            std::fs::write(&file, bytes).unwrap();
            std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600)).unwrap();
        };
        write(format!("{}\n", "0f".repeat(32)).as_bytes());
        assert!(Kek::from_file(&file, &state).is_ok());
        write(&[
            0u8, 200, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23,
            24, 25, 26, 27, 28, 29, 30, 31, 32,
        ]);
        assert!(Kek::from_file(&file, &state).is_ok());
        write(b"correct-horse-battery-staple-123");
        assert!(Kek::from_file(&file, &state).is_err());
        write("ab".repeat(31).as_bytes());
        assert!(Kek::from_file(&file, &state).is_err());
    }
}
