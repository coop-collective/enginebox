use crate::{
    model::{Host, Resources, identifier},
    state,
};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    io::Read,
    net::{IpAddr, ToSocketAddrs},
    path::{Path, PathBuf},
    time::Duration,
};
const MAX_BYTES: usize = 1024 * 1024;

/// Decode untrusted JSON before conversion to Value can erase duplicate keys.
/// The public API can use this on the entire request envelope as well as a manifest.
pub fn decode_json(data: &[u8]) -> Result<Value> {
    ensure!(data.len() <= MAX_BYTES, "JSON exceeds 1 MiB");
    struct Unique(Value);
    impl<'de> serde::Deserialize<'de> for Unique {
        fn deserialize<D: serde::Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
            struct Visitor;
            impl<'de> serde::de::Visitor<'de> for Visitor {
                type Value = Unique;
                fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                    f.write_str("JSON with unique object keys")
                }
                fn visit_bool<E: serde::de::Error>(
                    self,
                    v: bool,
                ) -> std::result::Result<Unique, E> {
                    Ok(Unique(json!(v)))
                }
                fn visit_i64<E: serde::de::Error>(self, v: i64) -> std::result::Result<Unique, E> {
                    Ok(Unique(json!(v)))
                }
                fn visit_u64<E: serde::de::Error>(self, v: u64) -> std::result::Result<Unique, E> {
                    Ok(Unique(json!(v)))
                }
                fn visit_f64<E: serde::de::Error>(self, v: f64) -> std::result::Result<Unique, E> {
                    serde_json::Number::from_f64(v)
                        .map(|v| Unique(Value::Number(v)))
                        .ok_or_else(|| E::custom("Non-finite JSON number"))
                }
                fn visit_str<E: serde::de::Error>(self, v: &str) -> std::result::Result<Unique, E> {
                    Ok(Unique(json!(v)))
                }
                fn visit_string<E: serde::de::Error>(
                    self,
                    v: String,
                ) -> std::result::Result<Unique, E> {
                    Ok(Unique(json!(v)))
                }
                fn visit_none<E: serde::de::Error>(self) -> std::result::Result<Unique, E> {
                    Ok(Unique(Value::Null))
                }
                fn visit_unit<E: serde::de::Error>(self) -> std::result::Result<Unique, E> {
                    Ok(Unique(Value::Null))
                }
                fn visit_seq<A: serde::de::SeqAccess<'de>>(
                    self,
                    mut seq: A,
                ) -> std::result::Result<Unique, A::Error> {
                    let mut values = Vec::new();
                    while let Some(v) = seq.next_element::<Unique>()? {
                        values.push(v.0);
                    }
                    Ok(Unique(Value::Array(values)))
                }
                fn visit_map<A: serde::de::MapAccess<'de>>(
                    self,
                    mut map: A,
                ) -> std::result::Result<Unique, A::Error> {
                    let mut values = serde_json::Map::new();
                    while let Some((k, v)) = map.next_entry::<String, Unique>()? {
                        if values.insert(k, v.0).is_some() {
                            return Err(serde::de::Error::custom("Duplicate JSON object key"));
                        }
                    }
                    Ok(Unique(Value::Object(values)))
                }
            }
            d.deserialize_any(Visitor)
        }
    }
    Ok(serde_json::from_slice::<Unique>(data)?.0)
}

pub fn decode_manifest_json(data: &[u8]) -> Result<Value> {
    validate_manifest(decode_json(data)?)
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Options {
    #[serde(default = "yes")]
    browser: bool,
    #[serde(default)]
    desktop: bool,
    #[serde(default)]
    guest_computer_mcp: bool,
}
fn yes() -> bool {
    true
}
impl Default for Options {
    fn default() -> Self {
        Self {
            browser: true,
            desktop: false,
            guest_computer_mcp: false,
        }
    }
}
fn recipe() -> String {
    "native-clients-v1".into()
}
fn source_image() -> String {
    "images:ubuntu/24.04/cloud".into()
}
fn clients() -> Vec<String> {
    vec!["claude".into(), "codex".into(), "cursor".into()]
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    schema_version: u32,
    id: String,
    revision: u32,
    label: String,
    #[serde(default)]
    description: String,
    #[serde(default = "recipe")]
    recipe: String,
    #[serde(default = "source_image")]
    source_image: String,
    minimum: Resources,
    recommended: Resources,
    #[serde(default = "clients")]
    native_clients: Vec<String>,
    #[serde(default)]
    config: Options,
}
pub fn validate_manifest(value: Value) -> Result<Value> {
    ensure!(
        serde_json::to_vec(&value)?.len() <= MAX_BYTES,
        "Template exceeds 1 MiB"
    );
    let mut m: Manifest = serde_json::from_value(value)?;
    ensure!(
        m.schema_version == 1 && (1..=2147483647).contains(&m.revision),
        "Unsupported manifest schema/revision"
    );
    identifier(&m.id, false)?;
    ensure!(
        !m.label.trim().is_empty()
            && m.label.chars().count() <= 120
            && !m.label.chars().any(char::is_control),
        "Template label must be a bounded single line"
    );
    ensure!(
        m.description.chars().count() <= 2000
            && !m.description.chars().any(|c| c.is_control() && c != '\n'),
        "Invalid template description"
    );
    ensure!(
        m.recipe == recipe() && m.source_image == source_image(),
        "Only native-clients-v1 on Ubuntu 24.04 cloud is qualified"
    );
    m.minimum.validate(None)?;
    m.recommended.validate(Some(m.minimum))?;
    ensure!(
        !m.config.guest_computer_mcp || m.config.desktop,
        "Guest computer MCP requires desktop"
    );
    for c in &m.native_clients {
        ensure!(
            ["codex", "claude", "cursor"].contains(&c.as_str()),
            "Unsupported native client"
        );
    }
    m.native_clients.sort();
    ensure!(
        !m.native_clients.windows(2).any(|p| p[0] == p[1]),
        "Duplicate native client"
    );
    Ok(serde_json::to_value(m)?)
}
pub fn manifest_fields(record: &Value) -> Value {
    let mut v = record.clone();
    if let Some(m) = v.as_object_mut() {
        m.retain(|k, _| {
            [
                "schema_version",
                "id",
                "revision",
                "label",
                "description",
                "recipe",
                "source_image",
                "minimum",
                "recommended",
                "native_clients",
                "config",
            ]
            .contains(&k.as_str())
        });
    }
    v
}
pub fn public_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v) => {
            let o = v.octets();
            !v.is_private()
                && !v.is_loopback()
                && !v.is_link_local()
                && !v.is_broadcast()
                && !v.is_documentation()
                && !v.is_multicast()
                && o[0] != 0
                && o[0] < 240
                && !(o[0] == 100 && (64..=127).contains(&o[1]))
                && !(o[0] == 192 && o[1] == 0 && o[2] == 0)
                && !(o[0] == 192 && o[1] == 88 && o[2] == 99)
                && !(o[0] == 198 && (o[1] == 18 || o[1] == 19))
        }
        IpAddr::V6(v) => {
            if let Some(v4) = v.to_ipv4_mapped() {
                return public_ip(IpAddr::V4(v4));
            }
            let s = v.segments();
            (s[0] & 0xe000) == 0x2000
                && !(s[0] == 0x2001 && (s[1] == 0xdb8 || s[1] < 0x200))
                && s[0] != 0x2002
                && !(s[0] == 0x3fff && (s[1] & 0xf000) == 0)
        }
    }
}
pub fn fetch_manifest(input: &str) -> Result<Value> {
    ensure!(
        input.len() <= 2048 && !input.chars().any(|c| c.is_whitespace() || c.is_control()),
        "Invalid HTTPS template URL"
    );
    let url = url::Url::parse(input)?;
    ensure!(
        url.scheme() == "https"
            && url.port_or_known_default() == Some(443)
            && url.username().is_empty()
            && url.password().is_none()
            && url.fragment().is_none(),
        "Template URL requires HTTPS port 443 without credentials or fragment"
    );
    let host = url
        .host_str()
        .context("Template URL lacks hostname")?
        .to_owned();
    // Resolve once, validate every answer, and pin those addresses for the TLS client.
    // DNS is on a bounded worker so system resolver stalls cannot occupy API workers indefinitely.
    let (tx, rx) = std::sync::mpsc::channel();
    let query = host.clone();
    std::thread::spawn(move || {
        let _ = tx.send(
            (query.as_str(), 443)
                .to_socket_addrs()
                .map(|i| i.collect::<Vec<_>>()),
        );
    });
    let addresses = rx
        .recv_timeout(Duration::from_secs(5))
        .context("Template DNS timed out")??;
    ensure!(
        !addresses.is_empty() && addresses.iter().all(|a| public_ip(a.ip())),
        "Template URLs must resolve only to public Internet addresses"
    );
    let client = reqwest::blocking::Client::builder()
        .https_only(true)
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(15))
        .connect_timeout(Duration::from_secs(5))
        .resolve_to_addrs(&host, &addresses)
        .build()?;
    let mut response = client
        .get(url)
        .header("Accept", "application/json")
        .header("Accept-Encoding", "identity")
        .header("User-Agent", "EngineBox-template-import/1")
        .send()
        .map_err(|_| anyhow::anyhow!("Template HTTPS download failed or timed out"))?;
    ensure!(
        response.status() == reqwest::StatusCode::OK,
        "Template download returned HTTP {}; redirects are not followed",
        response.status().as_u16()
    );
    ensure!(
        response
            .headers()
            .get("content-encoding")
            .is_none_or(|h| h == "identity"),
        "Compressed templates are not accepted"
    );
    ensure!(
        response
            .content_length()
            .is_none_or(|n| n <= MAX_BYTES as u64),
        "Template response exceeds 1 MiB"
    );
    let mut data = Vec::new();
    Read::take(&mut response, (MAX_BYTES + 1) as u64).read_to_end(&mut data)?;
    ensure!(data.len() <= MAX_BYTES, "Template exceeds 1 MiB");
    decode_manifest_json(&data)
}
#[derive(Debug, Clone)]
pub struct Registry {
    pub root: PathBuf,
}
impl Registry {
    pub fn new(root: &Path) -> Result<Self> {
        state::private_dir(root)?;
        let root = root.join("templates");
        for d in [
            root.clone(),
            root.join("manifests"),
            root.join("history"),
            root.join("deployments"),
            root.join("locks"),
            root.join("sync"),
        ] {
            state::private_dir(&d)?;
        }
        Ok(Self { root })
    }
    pub fn register(&self, value: Value) -> Result<Value> {
        let manifest = validate_manifest(value)?;
        let id = manifest["id"].as_str().unwrap();
        let digest = state::digest(&manifest);
        let _lock = state::lock(&self.root.join("locks/registry.lock"), true)?;
        let path = self.root.join("manifests").join(format!("{id}.json"));
        if path.exists() {
            let current = validate_manifest(state::read_json(&path)?)?;
            ensure!(
                current == manifest || manifest["revision"].as_u64() > current["revision"].as_u64(),
                "Changed templates need a greater revision"
            );
        }
        state::write_json(
            &self
                .root
                .join("history")
                .join(format!("{id}-{}-{digest}.json", manifest["revision"])),
            &manifest,
        )?;
        state::write_json(&path, &manifest)?;
        self.get(id)
    }
    pub fn get(&self, id: &str) -> Result<Value> {
        identifier(id, false)?;
        let mut v = validate_manifest(state::read_json(
            &self.root.join("manifests").join(format!("{id}.json")),
        )?)?;
        ensure!(v["id"] == id, "Template ID does not match filename");
        let digest = state::digest(&v);
        let mut receipts = Vec::new();
        for mut r in self.deployments(None)? {
            if r["id"] != id {
                continue;
            }
            r["in_sync"] = json!(r["manifest_digest"] == digest);
            r.as_object_mut().unwrap().remove("target");
            r.as_object_mut().unwrap().remove("host_binding");
            receipts.push(r);
        }
        v["manifest_digest"] = json!(digest);
        v["deployments"] = json!(receipts);
        Ok(v)
    }
    pub fn list(&self) -> Result<Vec<Value>> {
        state::json_files(&self.root.join("manifests"))?
            .into_iter()
            .map(|p| {
                self.get(
                    p.file_stem()
                        .unwrap()
                        .to_str()
                        .context("Invalid manifest filename")?,
                )
            })
            .collect()
    }
    pub fn deployments(&self, host: Option<&str>) -> Result<Vec<Value>> {
        let mut out = Vec::new();
        for p in state::json_files(&self.root.join("deployments"))? {
            let r = state::read_json(&p)?;
            let id = r["id"].as_str().context("Receipt has no template ID")?;
            identifier(id, false)?;
            let host_id = r["host_id"].as_str().context("Receipt has no host ID")?;
            ensure!(
                p.file_stem().unwrap().to_str() == Some(&state::digest(&json!([host_id, id]))),
                "Receipt filename mismatch"
            );
            ensure!(
                r["host_binding_digest"] == state::digest(&r["host_binding"]),
                "Receipt binding mismatch"
            );
            if host.is_none_or(|h| h == host_id) {
                out.push(r);
            }
        }
        Ok(out)
    }
    pub fn current(&self, record: &Value, host: &Host) -> bool {
        record["deployments"].as_array().is_some_and(|items| {
            items.iter().any(|r| {
                r["host_id"] == host.id
                    && r["manifest_digest"] == record["manifest_digest"]
                    && r["host_binding_digest"] == state::digest(&host.binding())
            })
        })
    }
    pub fn sync_path(&self, id: &str, host: &Host) -> PathBuf {
        self.root
            .join("sync")
            .join(format!("{}.json", state::digest(&json!([host.id, id]))))
    }
    pub fn install(&self, host: &Host, id: &str) -> Result<Value> {
        let record = self.get(id)?;
        let manifest = manifest_fields(&record);
        let binding = host.binding();
        let key = state::digest(&json!([host.id, binding, id]));
        let _lock = state::lock(
            &self.root.join("locks").join(format!("install-{key}.lock")),
            false,
        )?;
        state::write_json(
            &self.sync_path(id, host),
            &json!({"manifest_digest":record["manifest_digest"],"host_binding_digest":state::digest(&binding),"status":"syncing","updated_at":chrono::Utc::now()}),
        )?;
        let result = (|| -> Result<Value> {
            let result = crate::bootstrap::install_template(host, &manifest)?;
            let mut receipt = json!({"schema_version":1,"id":id,"label":manifest["label"],"revision":manifest["revision"],"manifest_digest":record["manifest_digest"],"minimum":manifest["minimum"],"recommended":manifest["recommended"],"host_id":host.id,"target":host.target,"project":host.project,"pool":host.pool,"guest":host.guest,"host_binding":binding,"host_binding_digest":state::digest(&binding),"installed_at":chrono::Utc::now()});
            receipt.as_object_mut().unwrap().extend(
                result
                    .as_object()
                    .context("Invalid install receipt")?
                    .clone(),
            );
            let _write = state::lock(&self.root.join("locks/registry.lock"), true)?;
            state::write_json(
                &self
                    .root
                    .join("deployments")
                    .join(format!("{}.json", state::digest(&json!([host.id, id])))),
                &receipt,
            )?;
            Ok(receipt)
        })();
        let status = match &result {
            Ok(_) => json!({"status":"ready"}),
            Err(e) => {
                json!({"status":"failed","error":e.to_string().chars().take(500).collect::<String>()})
            }
        };
        let mut status = status;
        status["manifest_digest"] = record["manifest_digest"].clone();
        status["host_binding_digest"] = json!(state::digest(&binding));
        status["updated_at"] = json!(chrono::Utc::now());
        state::write_json(&self.sync_path(id, host), &status)?;
        result
    }
}
