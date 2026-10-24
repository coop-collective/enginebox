use crate::{
    files,
    model::{Recipe, Toolchain},
};
use anyhow::{Context, Result, ensure};
use enginebox_agents::security;
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
};

pub fn validate(recipe: &Recipe) -> Result<()> {
    ensure!(recipe.schema == 1, "Unsupported recipe schema");
    ensure!(
        !recipe.version.is_empty()
            && recipe.version.len() <= 128
            && !recipe.version.chars().any(char::is_control),
        "Invalid engine version"
    );
    ensure!(
        recipe.executable.is_absolute() && files::hash_string(&recipe.executable_sha256),
        "Recipe executable requires an absolute path and SHA256"
    );
    ensure!(
        (1..=7200).contains(&recipe.timeout_seconds)
            && (1024..=16 * 1024 * 1024).contains(&recipe.max_log_bytes),
        "Invalid build bounds"
    );
    ensure!(
        files::identifier(&recipe.cache_version),
        "Invalid cache version"
    );
    files::relative(&recipe.entrypoint)?;
    let mut names = BTreeSet::new();
    ensure!(recipe.inputs.len() <= 64, "Too many toolchain inputs");
    for input in &recipe.inputs {
        ensure!(
            files::identifier(&input.name)
                && names.insert(&input.name)
                && input.path.is_absolute()
                && files::hash_string(&input.sha256),
            "Invalid toolchain input"
        );
    }
    ensure!(
        recipe.environment.len() <= 32,
        "Too many environment overrides"
    );
    for (key, value) in &recipe.environment {
        // Only engine-specific configuration. Owner/operator, provider and fleet secrets
        // must never be delegated to a project build process.
        ensure!(
            matches!(
                key.as_str(),
                "PATH"
                    | "LANG"
                    | "LC_ALL"
                    | "SDKROOT"
                    | "DEVELOPER_DIR"
                    | "DOTNET_ROOT"
                    | "JAVA_HOME"
                    | "ANDROID_HOME"
                    | "ANDROID_SDK_ROOT"
                    | "UNITY_LICENSE_FILE"
            ) && value.len() <= 4096
                && !value.contains('\0'),
            "Unsupported build environment key"
        );
    }
    match recipe.engine.as_str() {
        "godot" => {
            ensure!(
                matches!(
                    (recipe.target.as_str(), recipe.architecture.as_str()),
                    ("web", "wasm32") | ("linux", "x86_64") | ("linux", "arm64")
                ),
                "Unsupported Godot target"
            );
            ensure!(
                recipe.inputs.iter().any(|i| i.name == "export-template"),
                "Godot requires the export-template input"
            );
            ensure!(
                recipe.unity_method.is_none() && recipe.unreal_project.is_none(),
                "Unexpected engine options"
            );
            ensure!(
                recipe.target != "web" || recipe.entrypoint.ends_with(".html"),
                "Web entrypoint must be HTML"
            );
        }
        "unity" => {
            ensure!(
                matches!(
                    (recipe.target.as_str(), recipe.architecture.as_str()),
                    ("web", "wasm32") | ("linux", "x86_64")
                ),
                "Unsupported Unity target"
            );
            let method = recipe
                .unity_method
                .as_deref()
                .context("Unity requires a protected static build method")?;
            ensure!(
                method.len() <= 200
                    && method.contains('.')
                    && method
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '.'),
                "Invalid Unity build method"
            );
            ensure!(recipe.unreal_project.is_none(), "Unexpected Unreal option");
            ensure!(
                recipe.inputs.iter().any(|i| i.name == "toolchain-lock"),
                "Unity requires a hashed toolchain-lock describing editor modules/plugins/SDKs"
            );
        }
        "unreal" => {
            ensure!(
                recipe.target == "linux" && recipe.architecture == "x86_64",
                "Unsupported Unreal target"
            );
            let project = recipe
                .unreal_project
                .as_deref()
                .context("Unreal requires its project file")?;
            files::relative(project)?;
            ensure!(project.ends_with(".uproject"), "Invalid Unreal project");
            ensure!(recipe.unity_method.is_none(), "Unexpected Unity option");
            ensure!(
                recipe.inputs.iter().any(|i| i.name == "toolchain-lock"),
                "Unreal requires a hashed toolchain-lock describing engine/compiler/plugins/SDKs"
            );
        }
        _ => anyhow::bail!("Unsupported engine"),
    }
    Ok(())
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ToolchainLock {
    schema: u32,
    root: std::path::PathBuf,
    entries: Vec<crate::model::Entry>,
}
fn verify_toolchain_lock(
    input: &crate::model::ToolFile,
    executable: &Path,
    check: &dyn Fn() -> Result<()>,
) -> Result<()> {
    let lock: ToolchainLock = crate::read_json(&input.path, 32 * 1024 * 1024)?;
    ensure!(
        lock.schema == 1 && lock.root.is_absolute(),
        "Invalid immutable toolchain lock"
    );
    security::check_components(&lock.root)?;
    let root = files::absolute(&lock.root)?;
    if input.name == "toolchain-lock" {
        ensure!(
            executable.starts_with(&root),
            "Engine executable must belong to its locked installation"
        );
    }
    files::verify_source(&root, &lock.entries, check)
        .context("Installed toolchain differs from its complete lock")
}
pub fn fingerprint(recipe: &Recipe, check: &dyn Fn() -> Result<()>) -> Result<(String, Toolchain)> {
    validate(recipe)?;
    security::check_components(&recipe.executable)?;
    ensure!(
        files::hash_file(&recipe.executable, check)?.0 == recipe.executable_sha256,
        "Engine executable fingerprint mismatch"
    );
    let mut inputs = vec![];
    for input in &recipe.inputs {
        security::check_components(&input.path)?;
        ensure!(
            files::hash_file(&input.path, check)?.0 == input.sha256,
            "Toolchain input fingerprint mismatch: {}",
            input.name
        );
        if input.name == "toolchain-lock" || input.name.starts_with("toolchain-lock-") {
            verify_toolchain_lock(input, &recipe.executable, check)?;
        }
        inputs.push(json!({"name":input.name,"sha256":input.sha256}));
    }
    inputs.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
    let toolchain = Toolchain {
        engine: recipe.engine.clone(),
        version: recipe.version.clone(),
        target: recipe.target.clone(),
        architecture: recipe.architecture.clone(),
        fingerprint: files::digest(
            &json!({"adapter":1,"engine":recipe.engine,"version":recipe.version,"target":recipe.target,"architecture":recipe.architecture,"executable":recipe.executable_sha256,"inputs":inputs}),
        )?,
    };
    Ok((
        files::digest(&json!({"adapter":1,"recipe":recipe}))?,
        toolchain,
    ))
}
pub fn caches(recipe: &Recipe) -> &'static [&'static str] {
    match recipe.engine.as_str() {
        "godot" => &[".godot"],
        "unity" => &["Library"],
        "unreal" => &["DerivedDataCache"],
        _ => &[],
    }
}
pub fn environment(recipe: &Recipe, work: &Path) -> Result<BTreeMap<String, String>> {
    let mut env = BTreeMap::from([
        (
            "PATH".into(),
            "/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin".into(),
        ),
        ("LANG".into(), "C.UTF-8".into()),
    ]);
    for (key, value) in &recipe.environment {
        env.insert(key.clone(), value.clone());
    }
    for (key, dir) in [
        ("HOME", "home"),
        ("USERPROFILE", "home"),
        ("XDG_DATA_HOME", "data"),
        ("XDG_CACHE_HOME", "cache"),
        ("XDG_CONFIG_HOME", "config"),
        ("TMPDIR", "tmp"),
        ("TMP", "tmp"),
        ("TEMP", "tmp"),
    ] {
        let path = work.join(dir);
        security::private_dir(&path)?;
        env.insert(key.into(), path.to_string_lossy().into_owned());
    }
    if recipe.engine == "unreal" {
        let cache = work.join("source/DerivedDataCache");
        security::private_dir(&cache)?;
        env.insert(
            "UE-LocalDataCachePath".into(),
            cache.to_string_lossy().into_owned(),
        );
        env.insert("UE-SharedDataCachePath".into(), "None".into());
    }
    Ok(env)
}
fn path_string(path: &Path) -> Result<String> {
    Ok(path.to_str().context("Non-UTF8 build path")?.into())
}
pub fn commands(recipe: &Recipe, source: &Path, output: &Path) -> Result<Vec<Vec<String>>> {
    let executable = path_string(&recipe.executable)?;
    let root = path_string(source)?;
    let out = path_string(output)?;
    Ok(match recipe.engine.as_str() {
        "godot" => vec![
            vec![executable.clone(), "--headless".into(), "--version".into()],
            vec![
                executable.clone(),
                "--headless".into(),
                "--path".into(),
                root.clone(),
                "--editor".into(),
                "--import".into(),
            ],
            vec![
                executable,
                "--headless".into(),
                "--path".into(),
                root,
                "--export-release".into(),
                "Enginebox".into(),
                path_string(&output.join(&recipe.entrypoint))?,
            ],
        ],
        "unity" => vec![vec![
            executable,
            "-batchmode".into(),
            "-nographics".into(),
            "-quit".into(),
            "-projectPath".into(),
            root,
            "-executeMethod".into(),
            recipe.unity_method.clone().unwrap(),
            "-buildTarget".into(),
            if recipe.target == "web" {
                "webgl"
            } else {
                "linux64"
            }
            .into(),
            "-logFile".into(),
            "-".into(),
            "-engineboxOutput".into(),
            out,
        ]],
        "unreal" => vec![vec![
            executable,
            "BuildCookRun".into(),
            "-unattended".into(),
            "-nop4".into(),
            format!(
                "-project={}",
                path_string(&source.join(recipe.unreal_project.as_ref().unwrap()))?
            ),
            "-platform=Linux".into(),
            "-clientconfig=Development".into(),
            "-build".into(),
            "-cook".into(),
            "-stage".into(),
            "-pak".into(),
            "-archive".into(),
            format!("-archivedirectory={out}"),
            "-utf8output".into(),
        ]],
        _ => unreachable!(),
    })
}
pub fn prepare(recipe: &Recipe, source: &Path) -> Result<()> {
    if recipe.engine == "godot" {
        ensure!(
            source.join("project.godot").is_file(),
            "Missing project.godot"
        );
        let template = recipe
            .inputs
            .iter()
            .find(|i| i.name == "export-template")
            .unwrap();
        let quoted = serde_json::to_string(&path_string(&template.path)?)?;
        let platform = if recipe.target == "web" {
            "Web"
        } else {
            "Linux"
        };
        let options = if recipe.target == "web" {
            "variant/extensions_support=false\nvariant/thread_support=false\nhtml/export_icon=false\n".into()
        } else {
            format!(
                "binary_format/architecture=\"{}\"\nbinary_format/embed_pck=true\n",
                recipe.architecture
            )
        };
        // Replace only the private materialized copy. Never overwrite the frozen source.
        let path = source.join("export_presets.cfg");
        if std::fs::symlink_metadata(&path).is_ok() {
            std::fs::remove_file(&path)?;
        }
        std::fs::write(
            path,
            format!(
                "[preset.0]\nname=\"Enginebox\"\nplatform=\"{platform}\"\nrunnable=true\nexport_filter=\"all_resources\"\ninclude_filter=\"\"\nexclude_filter=\"\"\nexport_path=\"\"\n[preset.0.options]\ncustom_template/debug={quoted}\ncustom_template/release={quoted}\n{options}"
            ),
        )?;
    }
    if let Some(project) = &recipe.unreal_project {
        ensure!(source.join(project).is_file(), "Missing Unreal project");
    }
    Ok(())
}
pub fn public_fingerprint(recipe: &Recipe) -> Result<Value> {
    let (recipe, toolchain) = fingerprint(recipe, &|| Ok(()))?;
    Ok(json!({"recipe":recipe,"toolchain":toolchain}))
}
