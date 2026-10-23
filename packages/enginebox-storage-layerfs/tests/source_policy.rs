#![cfg(unix)]
use enginebox_storage_layerfs::{
    config::{Config, identifier},
    source::{self, allowed_path, safe_link},
};
use std::{fs, path::Path};

fn fixture(path: &Path) {
    source::git(path, &["init", "-b", "main"]).unwrap();
    fs::write(
        path.join("package.json"),
        "{\"name\":\"fixture\",\"private\":true}",
    )
    .unwrap();
    fs::write(path.join("hello.txt"), "hello\n").unwrap();
    source::git(path, &["add", "."]).unwrap();
    source::git(path, &["commit", "-m", "seed"]).unwrap();
}
#[test]
fn reviewed_commit_excludes_working_tree_and_git_credentials() {
    let repo = tempfile::tempdir().unwrap();
    fixture(repo.path());
    source::git(
        repo.path(),
        &[
            "config",
            "http.extraHeader",
            "Authorization: synthetic-test-canary",
        ],
    )
    .unwrap();
    fs::write(repo.path().join(".env"), "synthetic-untracked-secret").unwrap();
    fs::write(repo.path().join("hello.txt"), "uncommitted change").unwrap();
    fs::create_dir_all(repo.path().join(".git/hooks")).unwrap();
    fs::write(
        repo.path().join(".git/hooks/post-checkout"),
        "synthetic-hook",
    )
    .unwrap();
    let preview = source::preview(repo.path(), "HEAD").unwrap();
    assert_eq!(preview.files, vec!["hello.txt", "package.json"]);
    let stage = tempfile::tempdir().unwrap();
    source::stage(repo.path(), &preview, stage.path()).unwrap();
    let imported = stage.path().join("source");
    assert_eq!(
        String::from_utf8(source::git(&imported, &["rev-parse", "HEAD"]).unwrap())
            .unwrap()
            .trim(),
        preview.commit
    );
    assert_eq!(
        String::from_utf8(
            source::git(&imported, &["rev-parse", "--is-shallow-repository"]).unwrap()
        )
        .unwrap()
        .trim(),
        "true"
    );
    assert_eq!(
        fs::read_to_string(imported.join("hello.txt")).unwrap(),
        "hello\n"
    );
    assert!(!imported.join(".env").exists());
    assert!(!imported.join(".git/hooks/post-checkout").exists());
    assert!(
        !fs::read_to_string(imported.join(".git/config"))
            .unwrap()
            .contains("canary")
    );
    assert!(
        source::git(&imported, &["status", "--porcelain"])
            .unwrap()
            .is_empty()
    );
    fs::write(repo.path().join("hello.txt"), "next commit").unwrap();
    source::git(repo.path(), &["add", "hello.txt"]).unwrap();
    source::git(repo.path(), &["commit", "-m", "next"]).unwrap();
    assert_ne!(
        preview.approval,
        source::preview(repo.path(), "HEAD").unwrap().approval
    );
    assert_eq!(
        preview.approval,
        source::preview(repo.path(), &preview.commit)
            .unwrap()
            .approval
    );
}

#[test]
fn shallow_import_preserves_commit_and_ignores_export_attributes_without_old_objects() {
    let repo = tempfile::tempdir().unwrap();
    fixture(repo.path());
    fs::write(
        repo.path().join("historic.txt"),
        "synthetic-old-history-secret",
    )
    .unwrap();
    source::git(repo.path(), &["add", "historic.txt"]).unwrap();
    source::git(repo.path(), &["commit", "-m", "old history"]).unwrap();
    let old =
        String::from_utf8(source::git(repo.path(), &["rev-parse", "HEAD:historic.txt"]).unwrap())
            .unwrap()
            .trim()
            .to_owned();
    source::git(repo.path(), &["rm", "historic.txt"]).unwrap();
    fs::write(
        repo.path().join(".gitattributes"),
        "hello.txt export-ignore\nversion.txt export-subst\n",
    )
    .unwrap();
    fs::write(repo.path().join("version.txt"), "$Format:%H$").unwrap();
    source::git(repo.path(), &["add", "."]).unwrap();
    source::git(repo.path(), &["commit", "-m", "current"]).unwrap();
    let preview = source::preview(repo.path(), "HEAD").unwrap();
    let stage = tempfile::tempdir().unwrap();
    source::stage(repo.path(), &preview, stage.path()).unwrap();
    let imported = stage.path().join("source");
    assert_eq!(
        fs::read_to_string(imported.join("hello.txt")).unwrap(),
        "hello\n"
    );
    assert_eq!(
        fs::read_to_string(imported.join("version.txt")).unwrap(),
        "$Format:%H$"
    );
    assert!(
        source::git(&imported, &["status", "--porcelain"])
            .unwrap()
            .is_empty()
    );
    assert!(source::git(&imported, &["cat-file", "-e", &old]).is_err());
    assert_eq!(
        String::from_utf8(source::git(&imported, &["rev-list", "--count", "HEAD"]).unwrap())
            .unwrap()
            .trim(),
        "1"
    );
}

#[test]
fn symlink_chains_cannot_escape_after_a_prefix_link_is_resolved() {
    let repo = tempfile::tempdir().unwrap();
    fixture(repo.path());
    fs::create_dir(repo.path().join("x")).unwrap();
    fs::create_dir(repo.path().join("y")).unwrap();
    fs::write(repo.path().join("y/file"), "data").unwrap();
    std::os::unix::fs::symlink("../y", repo.path().join("x/a")).unwrap();
    std::os::unix::fs::symlink("x/a/../../outside", repo.path().join("escape")).unwrap();
    source::git(repo.path(), &["add", "."]).unwrap();
    source::git(repo.path(), &["commit", "-m", "links"]).unwrap();
    assert!(
        source::preview(repo.path(), "HEAD")
            .unwrap_err()
            .to_string()
            .contains("chain escapes")
    );
}

#[test]
fn replacement_refs_cannot_change_an_approved_commit() {
    let repo = tempfile::tempdir().unwrap();
    fixture(repo.path());
    let original = source::preview(repo.path(), "HEAD").unwrap();
    fs::write(repo.path().join("hello.txt"), "replacement content").unwrap();
    source::git(repo.path(), &["add", "hello.txt"]).unwrap();
    source::git(repo.path(), &["commit", "-m", "replacement"]).unwrap();
    source::git(repo.path(), &["replace", &original.commit, "HEAD"]).unwrap();
    assert_eq!(
        source::preview(repo.path(), &original.commit)
            .unwrap()
            .approval,
        original.approval
    );
    let stage = tempfile::tempdir().unwrap();
    source::stage(repo.path(), &original, stage.path()).unwrap();
    assert_eq!(
        fs::read_to_string(stage.path().join("source/hello.txt")).unwrap(),
        "hello\n"
    );
}
#[test]
fn tracked_secret_paths_and_submodules_are_refused() {
    let repo = tempfile::tempdir().unwrap();
    fixture(repo.path());
    fs::write(repo.path().join(".env.production"), "synthetic-secret").unwrap();
    source::git(repo.path(), &["add", ".env.production"]).unwrap();
    source::git(repo.path(), &["commit", "-m", "secret"]).unwrap();
    assert!(
        source::preview(repo.path(), "HEAD")
            .unwrap_err()
            .to_string()
            .contains("secrets")
    );
    for path in [
        "../file",
        "/absolute",
        ".git/config",
        "x/.codex/auth.json",
        "keys/id_ed25519",
        "x/.npmrc",
    ] {
        assert!(allowed_path(path).is_err(), "{path}");
    }
    for path in [
        ".env.example",
        "src/main.rs",
        "node_modules/typescript/package.json",
    ] {
        allowed_path(path).unwrap();
    }
    assert!(safe_link("link", "../../outside").is_err());
    assert!(safe_link("link", "/etc/passwd").is_err());
    assert!(safe_link("a/link", "../.ssh/key").is_err());
    safe_link("src/link", "../package.json").unwrap();
    let clean = tempfile::tempdir().unwrap();
    fixture(clean.path());
    let commit = source::preview(clean.path(), "HEAD").unwrap().commit;
    source::git(
        clean.path(),
        &[
            "update-index",
            "--add",
            "--cacheinfo",
            &format!("160000,{commit},submodule"),
        ],
    )
    .unwrap();
    source::git(clean.path(), &["commit", "-m", "submodule"]).unwrap();
    assert!(source::preview(clean.path(), "HEAD").is_err());
}
#[test]
fn private_configuration_requires_pins_and_budgets_without_feature_opt_in() {
    let mut config:Config=serde_json::from_value(serde_json::json!({"schema":1,"root":"/var/lib/enginebox-layerfs","sdk":"/opt/layerfs/sdk/js","daemon":"/opt/layerfs/layerfs-daemon",
        "native_sha256":"a".repeat(64),"daemon_sha256":"b".repeat(64),"image":format!("sha256:{}","c".repeat(64)),
        "max_workspaces":2,"cpus_per_workspace":1,"memory_mib_per_workspace":1024,"disk_reserve_gib":4,"import_limit_gib":2,
        "pnpm_store":null,"pnpm_metadata":null})).unwrap();
    config.validate().unwrap();
    assert!(!config.experimental);
    assert_eq!(config.docker_socket, Path::new("/var/run/docker.sock"));
    assert!(
        serde_json::to_value(&config)
            .unwrap()
            .get("docker_socket")
            .is_none()
    );
    config.docker_socket = "/run/enginebox-validation/docker.sock".into();
    config.validate().unwrap();
    let serialized = serde_json::to_value(&config).unwrap();
    assert_eq!(
        serialized["docker_socket"],
        "/run/enginebox-validation/docker.sock"
    );
    assert_eq!(
        serde_json::from_value::<Config>(serialized)
            .unwrap()
            .docker_socket,
        config.docker_socket
    );
    for invalid in [
        "tcp://localhost:2375",
        "relative.sock",
        "/run/../tmp/docker.sock",
        "/run/bad\tname.sock",
    ] {
        config.docker_socket = invalid.into();
        assert!(config.validate().is_err(), "{invalid}");
    }
    config.docker_socket = "/var/run/docker.sock".into();
    config.experimental = true;
    config.validate().unwrap();
    config.schema = 2;
    assert!(config.validate().is_err());
    config.schema = 1;
    config.image = "runtime:latest".into();
    assert!(config.validate().is_err());
    config.image = format!("sha256:{}", "c".repeat(64));
    config.max_workspaces = 0;
    assert!(config.validate().is_err());
    config.max_workspaces = 2;
    config.pnpm_store = Some("cache-v1".into());
    assert!(config.validate().is_err());
    for name in ["../other", "UPPER", "a/b", "--flag", ""] {
        assert!(identifier(name).is_err());
    }
}

#[test]
fn layerfs_materialization_contains_files_and_modes_but_no_git_database() {
    use std::os::unix::fs::PermissionsExt;
    let repo = tempfile::tempdir().unwrap();
    fixture(repo.path());
    fs::write(repo.path().join("binary.bin"), [0, 1, 255]).unwrap();
    fs::write(repo.path().join("script"), "#!/bin/sh\nexit 0\n").unwrap();
    fs::set_permissions(
        repo.path().join("script"),
        fs::Permissions::from_mode(0o755),
    )
    .unwrap();
    std::os::unix::fs::symlink("hello.txt", repo.path().join("link")).unwrap();
    source::git(repo.path(), &["add", "."]).unwrap();
    source::git(repo.path(), &["commit", "-m", "files"]).unwrap();
    let preview = source::preview(repo.path(), "HEAD").unwrap();
    let staging = tempfile::tempdir().unwrap();
    source::stage_files(repo.path(), &preview, staging.path()).unwrap();
    let imported = staging.path().join("source");
    assert!(!imported.join(".git").exists());
    assert_eq!(fs::read(imported.join("binary.bin")).unwrap(), [0, 1, 255]);
    assert!(
        fs::metadata(imported.join("script"))
            .unwrap()
            .permissions()
            .mode()
            & 0o111
            != 0
    );
    assert!(
        fs::symlink_metadata(imported.join("link"))
            .unwrap()
            .file_type()
            .is_symlink()
    );
}
#[test]
fn lfs_pointer_is_not_imported_as_if_it_were_the_asset() {
    let repo = tempfile::tempdir().unwrap();
    fixture(repo.path());
    fs::write(
        repo.path().join("asset.bin"),
        format!(
            "version https://git-lfs.github.com/spec/v1\noid sha256:{}\nsize 12345\n",
            "a".repeat(64)
        ),
    )
    .unwrap();
    source::git(repo.path(), &["add", "."]).unwrap();
    source::git(repo.path(), &["commit", "-m", "pointer"]).unwrap();
    assert!(
        source::preview(repo.path(), "HEAD")
            .unwrap_err()
            .to_string()
            .contains("Git LFS")
    );
}
