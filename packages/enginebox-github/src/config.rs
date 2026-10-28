//! Private runtime secret files, compatible with an operator's vault/credential mount.
use anyhow::{Context, Result, ensure};
use serde_json::Value;
use std::{fs, io::Read, path::Path};

pub fn load(path: &Path) -> Result<super::Config> {
    let mut value = enginebox_core::state::read_json(path)?;
    for kind in ["app", "repos", "runners"] {
        if value[kind].is_null() {
            continue;
        }
        for field in ["client_secret", "private_key", "webhook_secret"] {
            if let Some(reference) = value[kind][field].as_object() {
                ensure!(
                    reference.len() == 1,
                    "Secret reference must contain only file"
                );
                let path = reference
                    .get("file")
                    .and_then(Value::as_str)
                    .context("Secret reference needs an absolute file")?;
                let path = Path::new(path);
                ensure!(path.is_absolute(), "Secret file must be absolute");
                let meta =
                    fs::symlink_metadata(path).context("Cannot inspect GitHub secret file")?;
                ensure!(
                    meta.is_file() && !meta.file_type().is_symlink(),
                    "Secret must be a regular file"
                );
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    ensure!(
                        meta.permissions().mode() & 0o077 == 0,
                        "Secret file must have owner-only permissions"
                    );
                }
                let mut secret = String::new();
                fs::File::open(path)
                    .context("Cannot open GitHub secret file")?
                    .take(65537)
                    .read_to_string(&mut secret)
                    .context("Cannot read GitHub secret file")?;
                ensure!(
                    secret.len() <= 65536 && !secret.trim().is_empty() && !secret.contains('\0'),
                    "Invalid GitHub secret file"
                );
                value[kind][field] = Value::String(secret.trim_end_matches(['\n', '\r']).into());
            }
        }
    }
    serde_json::from_value(value).context("Invalid GitHub configuration")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn private_file_references_load_without_values_in_config() {
        for kind in ["repos", "app"] {
            let temp = tempfile::tempdir().unwrap();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(temp.path(), fs::Permissions::from_mode(0o700)).unwrap();
            }
            let secret = temp.path().join("secret");
            fs::write(&secret, "fixture-private-secret\n").unwrap();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(&secret, fs::Permissions::from_mode(0o600)).unwrap();
            }
            let config = temp.path().join("github.json");
            let mut value = json!({"schema":1,"callback_url":"https://example.test/api/github/callback","repos":{"client_id":"fixture","slug":"fixture","client_secret":{"file":secret},"private_key":{"file":secret},"webhook_secret":{"file":secret}},"pools":[]});
            if kind == "app" {
                let app = value.as_object_mut().unwrap().remove("repos").unwrap();
                value["app"] = app;
            }
            enginebox_core::state::write_json(&config, &value).unwrap();
            assert_eq!(
                load(&config).unwrap().app("repos").unwrap().client_secret,
                "fixture-private-secret"
            );
            assert!(
                !fs::read_to_string(&config)
                    .unwrap()
                    .contains("fixture-private-secret")
            );
            value[kind]["client_secret"] = json!({"file":"relative"});
            enginebox_core::state::write_json(&config, &value).unwrap();
            assert!(load(&config).is_err());
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                value[kind]["client_secret"] = json!({"file":secret});
                enginebox_core::state::write_json(&config, &value).unwrap();
                fs::set_permissions(&secret, fs::Permissions::from_mode(0o644)).unwrap();
                assert!(load(&config).is_err());
            }
        }
    }
}
