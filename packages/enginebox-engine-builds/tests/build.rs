#![cfg(unix)]
use enginebox_engine_builds::{
    build, files,
    model::{Entry, Layerfs, Recipe, Source, ToolFile},
    recipe,
};
use serde_json::json;
use std::{
    collections::BTreeMap,
    fs,
    path::Path,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

struct Fixture {
    _temp: tempfile::TempDir,
    root: std::path::PathBuf,
    state: std::path::PathBuf,
    recipe: Recipe,
    source: Source,
}
impl Fixture {
    fn new(script: &str) -> Self {
        let temp = tempfile::tempdir().unwrap();
        let base = fs::canonicalize(temp.path()).unwrap();
        let root = base.join("frozen");
        fs::create_dir(&root).unwrap();
        fs::write(root.join("project.godot"), "fixture").unwrap();
        let executable = base.join("engine");
        fs::write(&executable, format!("#!/bin/sh\n{script}")).unwrap();
        files::permissions(&executable, true).unwrap();
        let template = base.join("template");
        fs::write(&template, "template").unwrap();
        let recipe = Recipe {
            schema: 1,
            engine: "godot".into(),
            version: "4.test".into(),
            target: "web".into(),
            architecture: "wasm32".into(),
            executable: executable.clone(),
            executable_sha256: files::hash_file(&executable, &|| Ok(())).unwrap().0,
            inputs: vec![ToolFile {
                name: "export-template".into(),
                path: template.clone(),
                sha256: files::hash_file(&template, &|| Ok(())).unwrap().0,
            }],
            entrypoint: "index.html".into(),
            timeout_seconds: 10,
            max_log_bytes: 4096,
            environment: BTreeMap::new(),
            unity_method: None,
            unreal_project: None,
            cache_version: "1".into(),
        };
        let entries = vec![Entry::File {
            path: "project.godot".into(),
            sha256: files::hash_file(&root.join("project.godot"), &|| Ok(()))
                .unwrap()
                .0,
            size: 7,
            executable: false,
        }];
        let source = Source {
            schema: 1,
            project: "demo".into(),
            generation: "a".repeat(64),
            layerfs: Layerfs {
                revision: "f".repeat(40),
                project: "demo".into(),
                reference: "candidate".into(),
                root: "root".into(),
            },
            tree_hash: files::digest(&entries).unwrap(),
            entries,
        };
        Self {
            state: base.join("state"),
            _temp: temp,
            root,
            recipe,
            source,
        }
    }
    fn run(&self) -> anyhow::Result<enginebox_engine_builds::model::BuildResult> {
        build(&self.recipe, &self.source, &self.root, &self.state, None)
    }
}
const ENGINE: &str = r#"
case "$*" in
 *--version*) echo 4.test ;;
 *--import*) mkdir -p .godot; echo imported >> .godot/imports ;;
 *--export-release*) for arg in "$@"; do out="$arg"; done
   test -z "$JAM_ENGINEBOX_OWNER_TOKEN" || exit 99
   test -z "$AWS_SECRET_ACCESS_KEY" || exit 99
   echo '<html>playable</html>' > "$out"
   cp .godot/imports "$(dirname "$out")/imports.txt" ;;
esac
"#;
#[test]
fn exact_cache_reuses_artifact_but_verifies_every_file() {
    let fixture = Fixture::new(ENGINE);
    let first = fixture.run().unwrap();
    assert!(!first.cache_hit);
    let second = fixture.run().unwrap();
    assert!(second.cache_hit);
    assert_eq!(first.artifact_id, second.artifact_id);
    assert!(!fixture.root.join(".godot").exists());
    assert!(!fixture.root.join("export_presets.cfg").exists());
    fs::write(first.files_root.join("index.html"), "tampered").unwrap();
    assert!(fixture.run().unwrap_err().to_string().contains("integrity"));
}
#[test]
fn changed_generation_builds_again_reusing_only_engine_cache() {
    let mut fixture = Fixture::new(ENGINE);
    let first = fixture.run().unwrap();
    fixture.source.generation = "b".repeat(64);
    let second = fixture.run().unwrap();
    assert!(!second.cache_hit);
    assert_ne!(first.artifact_id, second.artifact_id);
    assert_eq!(
        fs::read_to_string(second.files_root.join("imports.txt")).unwrap(),
        "imported\nimported\n"
    );
    assert_eq!(
        fs::read_to_string(first.files_root.join("imports.txt")).unwrap(),
        "imported\n"
    );
}
#[test]
fn source_toolchain_and_paths_fail_closed() {
    let fixture = Fixture::new(ENGINE);
    fs::write(fixture.root.join("extra"), "not manifested").unwrap();
    assert!(fixture.run().is_err());
    fs::remove_file(fixture.root.join("extra")).unwrap();
    fs::write(&fixture.recipe.inputs[0].path, "changed template").unwrap();
    assert!(
        fixture
            .run()
            .unwrap_err()
            .to_string()
            .contains("fingerprint")
    );
    assert!(files::relative("../outside").is_err());
    assert!(files::relative("C:/outside").is_err());
    assert!(files::relative("a\\b").is_err());
}
#[test]
fn expired_replaced_and_cancelled_leases_never_commit() {
    let fixture = Fixture::new("case \"$*\" in *--version*) echo 4.test;; *) sleep 8;; esac");
    let lease = fixture.root.parent().unwrap().join("lease.json");
    let expires = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
        + 10_000;
    fs::write(
        &lease,
        json!({"attempt":"one","expiresAtMs":expires}).to_string(),
    )
    .unwrap();
    let replacement = lease.clone();
    let writer = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(250));
        let temp = replacement.with_extension("next");
        fs::write(
            &temp,
            json!({"attempt":"two","expiresAtMs":expires}).to_string(),
        )
        .unwrap();
        fs::rename(temp, replacement).unwrap();
    });
    let began = std::time::Instant::now();
    let result = build(
        &fixture.recipe,
        &fixture.source,
        &fixture.root,
        &fixture.state,
        Some(&lease),
    );
    writer.join().unwrap();
    assert!(result.is_err());
    assert!(began.elapsed() < Duration::from_secs(3));
    assert_eq!(
        fs::read_dir(fixture.state.join("artifacts"))
            .unwrap()
            .count(),
        0
    );
    fs::write(&lease, json!({"attempt":"one","expiresAtMs":1}).to_string()).unwrap();
    assert!(
        build(
            &fixture.recipe,
            &fixture.source,
            &fixture.root,
            &fixture.state,
            Some(&lease)
        )
        .is_err()
    );
    fs::write(
        &lease,
        json!({"attempt":"one","expiresAtMs":expires,"cancelled":true}).to_string(),
    )
    .unwrap();
    assert!(
        build(
            &fixture.recipe,
            &fixture.source,
            &fixture.root,
            &fixture.state,
            Some(&lease)
        )
        .is_err()
    );
}
#[test]
fn timeout_missing_entry_and_unsafe_output_never_commit() {
    for script in [
        "case \"$*\" in *--version*) echo 4.test;; *) sleep 3;; esac",
        "echo 4.test",
        "case \"$*\" in *--version*) echo 4.test;; *--export-release*) for arg in \"$@\"; do out=\"$arg\"; done; ln -s /etc/passwd \"$out\";; esac",
    ] {
        let mut fixture = Fixture::new(script);
        fixture.recipe.timeout_seconds = 1;
        assert!(fixture.run().is_err());
        assert_eq!(
            fs::read_dir(fixture.state.join("artifacts"))
                .unwrap()
                .count(),
            0
        );
    }
}
#[test]
fn canonical_hash_matches_protocol_and_engine_commands_are_argument_arrays() {
    assert_eq!(
        files::canonical(&json!({"z":[{"b":2,"a":1}],"a":"unicode 🎮"})),
        "{\"a\":\"unicode 🎮\",\"z\":[{\"a\":1,\"b\":2}]}"
    );
    let mut fixture = Fixture::new(ENGINE);
    fixture.recipe.engine = "unity".into();
    fixture.recipe.unity_method = Some("Enginebox.Build.Perform".into());
    fixture.recipe.inputs[0].name = "toolchain-lock".into();
    recipe::validate(&fixture.recipe).unwrap();
    let commands = recipe::commands(
        &fixture.recipe,
        Path::new("/project with space"),
        Path::new("/out with space"),
    )
    .unwrap();
    assert!(commands[0].contains(&"/project with space".into()));
    assert!(commands[0].contains(&"WebGL".into()));
    fixture
        .recipe
        .environment
        .insert("JAM_ENGINEBOX_OWNER_TOKEN".into(), "forbidden".into());
    assert!(recipe::validate(&fixture.recipe).is_err());
}
#[test]
fn source_links_cannot_escape_and_generated_caches_cannot_be_published_as_source() {
    let mut fixture = Fixture::new(ENGINE);
    std::os::unix::fs::symlink("/etc/passwd", fixture.root.join("escape")).unwrap();
    fixture.source.entries.insert(
        0,
        Entry::Symlink {
            path: "escape".into(),
            target: "/etc/passwd".into(),
        },
    );
    fixture.source.tree_hash = files::digest(&fixture.source.entries).unwrap();
    assert!(fixture.run().is_err());
    fs::remove_file(fixture.root.join("escape")).unwrap();
    fixture.source.entries.remove(0);
    fs::create_dir(fixture.root.join(".godot")).unwrap();
    fs::write(fixture.root.join(".godot/cache"), "x").unwrap();
    fixture.source.entries.insert(
        0,
        Entry::File {
            path: ".godot/cache".into(),
            sha256: files::hash_file(&fixture.root.join(".godot/cache"), &|| Ok(()))
                .unwrap()
                .0,
            size: 1,
            executable: false,
        },
    );
    fixture.source.tree_hash = files::digest(&fixture.source.entries).unwrap();
    assert!(fixture.run().unwrap_err().to_string().contains("generated"));
}

#[test]
fn toolchain_lock_verifies_library_bytes_not_just_launcher_or_version() {
    let mut fixture = Fixture::new(ENGINE);
    let install = fixture.root.parent().unwrap().join("installed");
    fs::create_dir(&install).unwrap();
    let executable = install.join("Editor");
    fs::copy(&fixture.recipe.executable, &executable).unwrap();
    fs::write(install.join("library"), "original").unwrap();
    let entries = ["Editor", "library"].map(|path| {
        let file = install.join(path);
        let meta = fs::metadata(&file).unwrap();
        Entry::File {
            path: path.into(),
            sha256: files::hash_file(&file, &|| Ok(())).unwrap().0,
            size: meta.len(),
            executable: files::executable(&meta),
        }
    });
    let lock = fixture.root.parent().unwrap().join("toolchain.json");
    fs::write(
        &lock,
        json!({"schema":1,"root":install,"entries":entries}).to_string(),
    )
    .unwrap();
    fixture.recipe.engine = "unity".into();
    fixture.recipe.unity_method = Some("Build.Perform".into());
    fixture.recipe.executable = executable;
    fixture.recipe.inputs = vec![ToolFile {
        name: "toolchain-lock".into(),
        path: lock.clone(),
        sha256: files::hash_file(&lock, &|| Ok(())).unwrap().0,
    }];
    recipe::fingerprint(&fixture.recipe, &|| Ok(())).unwrap();
    fs::write(install.join("library"), "modified").unwrap();
    assert!(recipe::fingerprint(&fixture.recipe, &|| Ok(())).is_err());
}
