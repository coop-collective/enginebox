use crate::{
    GIB,
    capacity::{capacity, inventory, number, resource_bytes, resource_fits},
    model::{Host, Resources, identifier, load_hosts, preset, sizes},
    state,
    templates::Registry,
    transport::{EngineBox, operation_lock},
};
use anyhow::{Context, Result, bail, ensure};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    path::Path,
    sync::{Arc, Mutex, MutexGuard, RwLock},
    time::{Duration, Instant},
};
type LifecycleGuard = Arc<dyn Fn(&str, &str) -> Result<()> + Send + Sync>;
type PostStart = Arc<dyn Fn(&str, &str) -> Result<Value> + Send + Sync>;
pub(crate) enum CopyMode<'a> {
    Start(Option<&'a Value>),
    Reserved(&'a Value),
}
#[derive(Clone)]
pub struct Fleet {
    pub registry: Registry,
    pub(crate) inner: Arc<Inner>,
}
pub(crate) struct Inner {
    pub hosts: BTreeMap<String, Host>,
    locks: BTreeMap<String, Mutex<()>>,
    guard: RwLock<Option<LifecycleGuard>>,
    post_start: RwLock<Option<PostStart>>,
}
impl Fleet {
    pub fn load(path: &Path, state_root: &Path) -> Result<Self> {
        Self::from_hosts(load_hosts(path)?, state_root)
    }
    pub fn from_hosts(hosts: Vec<Host>, state_root: &Path) -> Result<Self> {
        ensure!(!hosts.is_empty(), "Fleet requires hosts");
        let mut map = BTreeMap::new();
        let mut targets = std::collections::HashSet::new();
        for h in hosts {
            h.validate()?;
            ensure!(
                !map.contains_key(&h.id) && targets.insert(h.target.clone()),
                "Duplicate fleet host"
            );
            map.insert(h.id.clone(), h);
        }
        Ok(Self {
            registry: Registry::new(state_root)?,
            inner: Arc::new(Inner {
                locks: map.keys().map(|id| (id.clone(), Mutex::new(()))).collect(),
                hosts: map,
                guard: RwLock::new(None),
                post_start: RwLock::new(None),
            }),
        })
    }
    pub fn host(&self, id: &str) -> Result<Host> {
        self.inner.hosts.get(id).cloned().context("Unknown host")
    }
    pub fn hosts(&self) -> Vec<Host> {
        self.inner.hosts.values().cloned().collect()
    }
    pub(crate) fn controller_lock(&self, id: &str) -> Result<MutexGuard<'_, ()>> {
        self.inner
            .locks
            .get(id)
            .context("Unknown host")?
            .lock()
            .map_err(|_| anyhow::anyhow!("Host operation mutex poisoned"))
    }
    pub fn set_lifecycle_guard(&self, guard: LifecycleGuard) {
        *self.inner.guard.write().expect("guard lock") = Some(guard);
    }
    pub fn set_post_start(&self, hook: PostStart) {
        *self.inner.post_start.write().expect("hook lock") = Some(hook);
    }
    pub fn account_workspace(&self, id: &str, name: &str) -> Result<Host> {
        let host = self.host(id)?;
        let facts = inventory(&host)?;
        let vm = private_workspace(&host, name, &facts)?;
        ensure!(
            vm["status"] == "Running",
            "Resume workspace before connecting provider accounts"
        );
        Ok(host)
    }
    /// The template a running private workspace was created from, if recorded.
    pub fn workspace_template(&self, id: &str, name: &str) -> Result<Option<String>> {
        let host = self.host(id)?;
        let facts = inventory(&host)?;
        let vm = private_workspace(&host, name, &facts)?;
        Ok(vm["template"].as_str().map(str::to_owned))
    }
    /// Explicit, resumable v0.2 guest migration. No provider homes or native CLIs change.
    pub fn sync_guest_tools(&self, id: &str, name: &str) -> Result<Value> {
        let host = self.host(id)?;
        let _controller = self.controller_lock(id)?;
        let _lock = operation_lock(&host)?;
        self.account_workspace(id, name)?;
        crate::guest::ensure_remote_guest_tools(&host.engine_box(), name, true)
    }
    pub fn summary(&self) -> Result<Value> {
        let mut hosts = Vec::new();
        let mut templates: BTreeMap<String, Value> = BTreeMap::new();
        for batch in self.hosts().chunks(8) {
            std::thread::scope(|scope| {
                let tasks = batch
                    .iter()
                    .map(|h| scope.spawn(move || self.host_status(h)))
                    .collect::<Vec<_>>();
                for task in tasks {
                    if let Ok(value) = task.join() {
                        hosts.push(value);
                    }
                }
            });
        }
        for h in self.hosts() {
            for t in self.templates_for(&h)? {
                let id = t["id"].as_str().unwrap().to_owned();
                let row=templates.entry(id.clone()).or_insert_with(||json!({"id":id,"label":t["label"],"minimum":t["minimum"],"recommended":t["recommended"],"host_ids":[],"host_resources":{}}));
                row["host_ids"].as_array_mut().unwrap().push(json!(h.id));
                row["host_resources"][&h.id] =
                    json!({"minimum":t["minimum"],"recommended":t["recommended"]});
            }
        }
        Ok(
            json!({"hosts":hosts,"templates":templates.into_values().collect::<Vec<_>>(),"sizes":sizes()}),
        )
    }
    fn host_status(&self, h: &Host) -> Value {
        let status = (|| -> Result<Value> {
            let facts = inventory(h)?;
            let instances = facts["instances"]
                .as_array()
                .context("Invalid inventory")?
                .iter()
                .filter(|i| {
                    i["project"] == h.project
                        && i["name"].as_str().is_some_and(|s| s.starts_with("ebox-"))
                })
                .cloned()
                .collect::<Vec<_>>();
            Ok(
                json!({"id":h.id,"label":h.label,"online":true,"capacity":capacity(h,&facts)?,"warm_pools":crate::warm::summary(self,h,&facts),"instances":instances}),
            )
        })();
        status.unwrap_or_else(|_|json!({"id":h.id,"label":h.label,"online":false,"error":"Host inventory unavailable. Check SSH, Incus access and storage configuration.","instances":[]}))
    }
    pub fn catalog(&self) -> Result<Value> {
        let mut records = self.registry.list()?;
        for r in &mut records {
            let mut nodes = Vec::new();
            for h in self.hosts() {
                let mut node = json!({"host_id":h.id,"label":h.label,"status":"pending"});
                if self.registry.current(r, &h) {
                    node["status"] = json!("ready");
                } else {
                    let path = self.registry.sync_path(r["id"].as_str().unwrap(), &h);
                    if path.exists()
                        && let Ok(s) = state::read_json(&path)
                        && s["manifest_digest"] == r["manifest_digest"]
                        && s["host_binding_digest"] == state::digest(&h.binding())
                    {
                        node["status"] = s["status"].clone();
                        if let Some(e) = s.get("error") {
                            node["error"] = e.clone();
                        }
                    }
                }
                nodes.push(node);
            }
            for receipt in r["deployments"].as_array_mut().unwrap() {
                receipt["in_sync"] = json!(
                    receipt["in_sync"] == true
                        && self
                            .inner
                            .hosts
                            .get(receipt["host_id"].as_str().unwrap_or(""))
                            .is_some_and(
                                |h| receipt["host_binding_digest"] == state::digest(&h.binding())
                            )
                );
            }
            r["sync"] = json!({"desired_revision":r["revision"],"nodes":nodes});
        }
        Ok(json!({"templates":records}))
    }
    pub fn register_template(&self, v: Value) -> Result<Value> {
        self.registry.register(v)
    }
    pub fn import_template(&self, url: &str) -> Result<Value> {
        self.registry
            .register(crate::templates::fetch_manifest(url)?)
    }
    pub fn templates_for(&self, h: &Host) -> Result<Vec<Value>> {
        let records = self.registry.list()?;
        let mut out = Vec::new();
        for t in &h.templates {
            if records.iter().any(|r| r["id"] == t.id) {
                continue;
            }
            let min = t.minimum.unwrap_or(Resources {
                cpu: 2,
                memory_gib: 2,
                disk_gib: h.disk_gib,
            });
            let rec = t.recommended.unwrap_or(Resources {
                cpu: 4,
                memory_gib: 4,
                disk_gib: h.disk_gib,
            });
            out.push(json!({"id":t.id,"label":if t.label.is_empty(){&t.id}else{&t.label},"source":t.source,"snapshot":t.snapshot,"minimum":min,"recommended":rec}));
        }
        for d in self.registry.deployments(Some(&h.id))? {
            if records
                .iter()
                .any(|r| r["id"] == d["id"] && r["manifest_digest"] == d["manifest_digest"])
                && d["host_binding_digest"] == state::digest(&h.binding())
            {
                out.push(json!({"id":d["id"],"label":d["label"],"source":d["source"],"snapshot":d["snapshot"],"minimum":d["minimum"],"recommended":d["recommended"],"revision":d["revision"],"manifest_digest":d["manifest_digest"]}));
            }
        }
        Ok(out)
    }
    pub fn template(&self, h: &Host, id: &str) -> Result<Value> {
        self.templates_for(h)?
            .into_iter()
            .find(|t| t["id"] == id)
            .context("Select a template installed at the current revision on this host")
    }
    pub fn validate_operation(&self, id: &str, operation: &str, params: &Value) -> Result<()> {
        let h = self.host(id)?;
        let map = params
            .as_object()
            .context("Operation parameters must be an object")?;
        match operation {
            "create" | "allocate" => {
                ensure!(
                    map.keys()
                        .all(|k| ["name", "template", "size", "resources"].contains(&k.as_str())),
                    "Unknown create fields"
                );
                identifier(required(params, "name")?, true)?;
                let t = self.template(&h, required(params, "template")?)?;
                let size = optional_string(params, "size")?;
                preset(&h, &t, size, params.get("resources"))?;
            }
            "install_template" => {
                ensure!(map.len() == 1, "Provide only template ID");
                self.registry.get(required(params, "template")?)?;
            }
            "pause" | "resume" | "hibernate" | "stop" => {
                ensure!(map.len() == 1, "Provide only name");
                identifier(required(params, "name")?, true)?;
            }
            "snapshot" | "restore" => {
                ensure!(
                    map.keys()
                        .all(|k| ["name", "snapshot", "stateful"].contains(&k.as_str())),
                    "Unknown snapshot fields"
                );
                identifier(required(params, "name")?, true)?;
                identifier(required(params, "snapshot")?, false)?;
                if let Some(v) = params.get("stateful") {
                    ensure!(v.is_boolean(), "stateful must be boolean");
                }
            }
            _ => bail!("Unsupported fleet operation"),
        };
        Ok(())
    }
    pub fn operate(&self, id: &str, operation: &str, params: &Value) -> Result<Value> {
        self.validate_operation(id, operation, params)?;
        let h = self.host(id)?;
        let _controller = self.controller_lock(id)?;
        if operation == "install_template" {
            return self.registry.install(&h, required(params, "template")?);
        }
        let _lock = operation_lock(&h)?;
        let name = required(params, "name")?;
        let mut result = match operation {
            "create" => self.create_locked(
                &h,
                name,
                required(params, "template")?,
                optional_string(params, "size")?,
                params.get("resources"),
                None,
            )?,
            "allocate" => crate::warm::allocate(
                self,
                &h,
                name,
                required(params, "template")?,
                optional_string(params, "size")?,
                params.get("resources"),
            )?,
            _ => {
                let facts = inventory(&h)?;
                let vm = private_workspace(&h, name, &facts)?;
                if let Some(guard) = self.inner.guard.read().unwrap().clone() {
                    guard(id, name)?;
                }
                // Locks in each provider home are held by native children even across controller upgrades.
                crate::guest::assert_remote_idle(&h, name)?;
                if operation == "resume" && vm["status"] == "Stopped" {
                    let room = capacity(&h, &facts)?;
                    ensure!(
                        number(&vm["cpu"]) <= number(&room["cpu_available"])
                            && number(&vm["memory_bytes"])
                                <= number(&room["memory_budget_available_bytes"]),
                        "Insufficient capacity to resume while preserving reserves"
                    );
                }
                if operation == "hibernate" {
                    let room = capacity(&h, &facts)?;
                    ensure!(
                        number(&vm["state_disk_bytes"]) >= number(&vm["memory_bytes"]) + GIB,
                        "Memory-state disk is too small"
                    );
                    ensure!(
                        number(&room["pool_free_bytes"]).min(number(&room["disk_free_bytes"]))
                            >= number(&vm["memory_bytes"]) + h.reserve_disk_gib * GIB,
                        "Insufficient free disk to hibernate"
                    );
                }
                let bx = h.engine_box();
                let mut value = match operation {
                    "snapshot" => bx.snapshot(name, required(params, "snapshot")?)?,
                    "restore" => bx.restore(
                        name,
                        required(params, "snapshot")?,
                        params["stateful"].as_bool().unwrap_or(true),
                    )?,
                    _ => bx.lifecycle(name, operation)?,
                };
                if operation == "resume" {
                    value["readiness"] = wait_ready(&bx, name)?;
                }
                value
            }
        };
        if ["create", "allocate", "resume"].contains(&operation)
            && let Some(hook) = self.inner.post_start.read().unwrap().clone()
        {
            let actual = result["name"].as_str().unwrap_or(name);
            result["agents"] = hook(id, actual).unwrap_or_else(
                |_| json!({"providers":[],"error":"Agent access synchronization will retry"}),
            );
        }
        Ok(result)
    }
    pub(crate) fn create_locked(
        &self,
        h: &Host,
        name: &str,
        id: &str,
        size: Option<&str>,
        custom: Option<&Value>,
        pool: Option<&Value>,
    ) -> Result<Value> {
        self.copy_locked(h, name, id, size, custom, CopyMode::Start(pool))
    }
    pub(crate) fn copy_locked(
        &self,
        h: &Host,
        name: &str,
        id: &str,
        size: Option<&str>,
        custom: Option<&Value>,
        mode: CopyMode<'_>,
    ) -> Result<Value> {
        let facts = inventory(h)?;
        check_name(h, name, &facts)?;
        let t = self.template(h, id)?;
        let r = preset(h, &t, size, custom)?;
        ensure!(
            resource_fits(h, &facts, r)?,
            "Insufficient CPU, memory, disk or instance slots; preserve reserves"
        );
        let bx = h.engine_box();
        let snap = template_snapshot(h, &t)?;
        if matches!(mode, CopyMode::Reserved(_)) {
            // Existing inventory gives description claims precedence over pool
            // config. Reject inherited assignments before copy so rollback also
            // sees this clone as reserved if the response is lost.
            let source = bx.instance(required(&t, "source")?)?;
            for object in [&source, &snap] {
                ensure!(
                    !object["description"]
                        .as_str()
                        .is_some_and(|s| s.starts_with("enginebox-pool:v1:"))
                        && ["config", "expanded_config"]
                            .iter()
                            .all(|section| object[*section].as_object().is_none_or(|config| {
                                !config.keys().any(|key| {
                                    key.starts_with("user.enginebox.allocation.")
                                        || key.starts_with("user.enginebox.pool.")
                                        || key == "user.enginebox.workspace"
                                })
                            })),
                    "Prepared allocation requires an unassigned template source and snapshot"
                );
            }
        }
        let (_, root) = root_device(&bx, &snap)?;
        let source_vm = facts["instances"]
            .as_array()
            .unwrap()
            .iter()
            .find(|i| i["project"] == h.project && i["name"] == t["source"]);
        let source_disk = resource_bytes(
            &root["size"],
            source_vm.map(|v| number(&v["disk_bytes"])).unwrap_or(0),
        );
        ensure!(
            source_disk > 0 && r.disk_gib * GIB >= source_disk,
            "Requested disk must cover the template; automatic shrinking is unsupported"
        );
        let started = Instant::now();
        let mut args = vec![
            "copy".into(),
            format!("{}/{}", required(&t, "source")?, required(&t, "snapshot")?),
            name.into(),
            "--stateless".into(),
            "--storage".into(),
            h.pool.clone(),
            "-c".into(),
            format!("limits.cpu={}", r.cpu),
            "-c".into(),
            format!("limits.memory={}GiB", r.memory_gib),
            "-c".into(),
            "user.enginebox.fleet=true".into(),
            "-c".into(),
            format!("user.enginebox.template={id}"),
            "-c".into(),
            "user.enginebox.clean-base=false".into(),
            "-c".into(),
            "boot.autostart=false".into(),
        ];
        if let CopyMode::Start(Some(pool)) = &mode {
            for (k, v) in pool.as_object().context("Invalid pool metadata")? {
                args.extend([
                    "-c".into(),
                    format!(
                        "user.enginebox.pool.{k}={}",
                        v.as_str().context("Pool metadata must be text")?
                    ),
                ]);
            }
        }
        if let CopyMode::Reserved(receipt) = &mode {
            // The reservation is part of the atomic clone request, so even a
            // failed/timed-out copy never publishes an ordinary private VM.
            // Existing controllers already refuse preparing pool instances.
            for (key, value) in [
                ("user.enginebox.pool.state", "preparing"),
                ("user.enginebox.pool.size", "layerfs-preparation"),
                ("user.enginebox.workspace", ""),
                ("user.enginebox.allocation.id", required(receipt, "id")?),
                (
                    "user.enginebox.allocation.project",
                    required(receipt, "storage_project")?,
                ),
                ("user.enginebox.owner", h.project.as_str()),
            ] {
                args.extend(["-c".into(), format!("{key}={value}")]);
            }
        }
        bx.run(&args, Duration::from_secs(300))?;
        let copy = started.elapsed().as_secs_f64();
        let clone = bx.instance(name)?;
        let prepared_uuid = if let CopyMode::Reserved(receipt) = &mode {
            ensure!(
                clone["status"] == "Stopped"
                    && clone["stateful"] != true
                    && clone["config"]["user.enginebox.allocation.id"] == receipt["id"]
                    && clone["config"]["user.enginebox.owner"] == h.project
                    && clone["config"]["user.enginebox.pool.state"] == "preparing"
                    && clone["config"]["boot.autostart"] == "false",
                "Clone custody changed before disk configuration; inspect without mutation"
            );
            Some(
                clone["config"]["volatile.uuid"]
                    .as_str()
                    .filter(|value| uuid::Uuid::parse_str(value).is_ok())
                    .context("Prepared VM has no valid persistent Incus identity")?
                    .to_owned(),
            )
        } else {
            None
        };
        let (root_name, root) = root_device(&bx, &clone)?;
        ensure!(
            root["pool"] == h.pool,
            "Clone storage differs from admitted pool; VM remains stopped for inspection"
        );
        let disk_mode = if clone["devices"].get(&root_name).is_some() {
            "set"
        } else {
            "override"
        };
        bx.call(&[
            "config",
            "device",
            disk_mode,
            name,
            &root_name,
            &format!("size={}GiB", r.disk_gib),
            &format!("size.state={}GiB", r.memory_gib + 2),
        ])?;
        if let CopyMode::Reserved(receipt) = mode {
            let clone = bx.instance(name)?;
            ensure!(
                clone["status"] == "Stopped"
                    && clone["stateful"] != true
                    && clone["config"]["user.enginebox.allocation.id"] == receipt["id"]
                    && clone["config"]["user.enginebox.owner"] == h.project
                    && clone["config"]["user.enginebox.pool.state"] == "preparing"
                    && clone["config"]["volatile.uuid"].as_str() == prepared_uuid.as_deref()
                    && clone["config"]["boot.autostart"] == "false",
                "Prepared VM identity or reservation changed; retain it for inspection"
            );
            let uuid = prepared_uuid.context("Missing prepared VM identity")?;
            return Ok(
                json!({"name":name,"status":"Stopped","startup":"prepared","instance_uuid":uuid,"copy_seconds":copy,"ready":false,"attachment_required":true,"reservation_id":receipt["id"],"template":t,"source_snapshot_digest":state::digest(&snap)}),
            );
        }
        bx.call(&["start", name])?;
        // Warm reserves perform their readiness wait outside the host lease.
        let readiness = if matches!(mode, CopyMode::Start(None)) {
            wait_ready(&bx, name)?
        } else {
            Value::Null
        };
        Ok(
            json!({"name":name,"status":"Running","copy_seconds":copy,"ready_seconds":started.elapsed().as_secs_f64(),"readiness":readiness}),
        )
    }
    pub fn reconcile(&self) -> Result<Value> {
        let mut changed = Vec::new();
        let mut failures = Vec::new();
        let records = self.registry.list()?;
        for h in self.hosts() {
            for r in &records {
                if self.registry.current(r, &h) {
                    continue;
                }
                let id = required(r, "id")?;
                let path = self.registry.sync_path(id, &h);
                if let Ok(s) = state::read_json(&path)
                    && s["status"] == "failed"
                    && s["manifest_digest"] == r["manifest_digest"]
                    && s["host_binding_digest"] == state::digest(&h.binding())
                    && let Some(t) = s["updated_at"]
                        .as_str()
                        .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
                    && chrono::Utc::now().signed_duration_since(t).num_seconds() < 60
                {
                    continue;
                }
                {
                    let _controller = self.controller_lock(&h.id)?;
                    match self.registry.install(&h,id){Ok(v)=>changed.push(v),Err(e)=>failures.push(json!({"host_id":h.id,"template":id,"error":e.to_string().chars().take(500).collect::<String>()}))};
                }
                break;
            }
            match crate::warm::reconcile_once(self,&h){Ok(v)if !v.is_null()=>changed.push(v),Ok(_)=>{},Err(e)=>failures.push(json!({"host_id":h.id,"error":e.to_string().chars().take(500).collect::<String>()}))};
        }
        Ok(json!({"changed":changed,"failures":failures}))
    }
}
pub fn required<'a>(v: &'a Value, k: &str) -> Result<&'a str> {
    v[k].as_str()
        .with_context(|| format!("Provide string field {k}"))
}
fn optional_string<'a>(v: &'a Value, k: &str) -> Result<Option<&'a str>> {
    v.get(k)
        .map(|v| v.as_str().context("Optional field must be a string"))
        .transpose()
}
pub fn check_name(h: &Host, name: &str, facts: &Value) -> Result<()> {
    identifier(name, true)?;
    ensure!(
        !facts["instances"]
            .as_array()
            .context("Invalid inventory")?
            .iter()
            .any(
                |v| v["project"] == h.project && (v["name"] == name || v["workspace_name"] == name)
            ),
        "Instance or workspace name already exists"
    );
    Ok(())
}
pub fn private_workspace<'a>(h: &Host, name: &str, facts: &'a Value) -> Result<&'a Value> {
    identifier(name, true)?;
    let v = facts["instances"]
        .as_array()
        .context("Invalid inventory")?
        .iter()
        .find(|v| v["project"] == h.project && v["name"] == name)
        .context("Workspace not found")?;
    ensure!(
        v["managed"] == true && v["pool"] == h.pool,
        "Workspace must be fleet-managed in its admitted pool"
    );
    ensure!(
        !["preparing", "ready", "quarantined"]
            .contains(&v["warm_pool_state"].as_str().unwrap_or("")),
        "Unassigned warm reserves cannot be used as private workspaces"
    );
    Ok(v)
}
pub fn template_snapshot(h: &Host, t: &Value) -> Result<Value> {
    let snap = h.engine_box().query(&format!(
        "/1.0/instances/{}/snapshots/{}?project={}",
        required(t, "source")?,
        required(t, "snapshot")?,
        h.project
    ))?;
    crate::transport::ensure_no_external_checkout(&snap, "template clone")?;
    ensure!(
        snap["config"]["user.enginebox.clean-base"] == "true"
            && snap["stateful"] != true
            && snap.get("type").is_none_or(|t| t == "virtual-machine"),
        "Reusable template must be a marked pre-login disk-only VM snapshot"
    );
    Ok(snap)
}
pub fn root_device(bx: &EngineBox, vm: &Value) -> Result<(String, Value)> {
    let mut devices = vm["expanded_devices"].as_object().cloned();
    if devices.is_none() {
        let mut d = serde_json::Map::new();
        for p in vm["profiles"].as_array().into_iter().flatten() {
            let profile = bx.query(&format!(
                "/1.0/profiles/{}?project={}",
                p.as_str().context("Invalid profile")?,
                bx.project
            ))?;
            if let Some(v) = profile["devices"].as_object() {
                d.extend(v.clone());
            }
        }
        if let Some(v) = vm["devices"].as_object() {
            d.extend(v.clone());
        }
        devices = Some(d);
    }
    let roots = devices
        .unwrap()
        .into_iter()
        .filter(|(_, d)| d["type"] == "disk" && d["path"] == "/")
        .collect::<Vec<_>>();
    ensure!(
        roots.len() == 1,
        "Instance must resolve exactly one root disk"
    );
    Ok(roots[0].clone())
}
pub fn wait_ready(bx: &EngineBox, name: &str) -> Result<Value> {
    wait_ready_mode(bx, name, true)
}
pub fn wait_ready_unlocked(bx: &EngineBox, name: &str) -> Result<Value> {
    wait_ready_mode(bx, name, false)
}
fn wait_ready_mode(bx: &EngineBox, name: &str, host_lock_held: bool) -> Result<Value> {
    let deadline = Instant::now() + Duration::from_secs(180);
    let expected = serde_json::to_vec(&bx.guest)?;
    let mut tools_ready = false;
    while Instant::now() < deadline {
        if !tools_ready {
            if crate::guest::ensure_remote_guest_tools(bx, name, host_lock_held).is_err() {
                std::thread::sleep(Duration::from_secs(1));
                continue;
            }
            tools_ready = true;
        }
        if let Ok(data) = bx.execute_with_input(
            name,
            &["enginebox-host".into(), "readiness".into()],
            &expected,
            Duration::from_secs(15),
        ) && let Ok(v) = serde_json::from_slice::<Value>(&data)
            && v["ready"] == true
        {
            return Ok(v);
        }
        std::thread::sleep(Duration::from_secs(1));
    }
    bail!(
        "VM started but identity, manifest, network and DNS did not become ready within 180 seconds; inspect before retrying"
    )
}
