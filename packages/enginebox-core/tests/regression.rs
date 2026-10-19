use enginebox_core::{
    Fleet, GIB, GuestConfig, Host, Resources, bootstrap, capabilities,
    capacity::{capacity, cpu_count, pool_metadata, resource_bytes, resource_fits},
    guest,
    model::{load_hosts, preset},
    state,
    templates::{Registry, public_ip, validate_manifest},
    warm,
};
use serde_json::{Value, json};
fn fixture() -> Value {
    serde_json::from_str(include_str!("fixtures/v0_2_compatibility.json")).unwrap()
}
fn host() -> Host {
    Host {
        id: "primary".into(),
        label: "Test host".into(),
        target: "test-server".into(),
        project: "enginebox".into(),
        pool: "pool".into(),
        reserve_cpu: 4,
        reserve_memory_gib: 8,
        reserve_disk_gib: 10,
        disk_gib: 30,
        max_instances: 8,
        ..Default::default()
    }
}
fn facts() -> Value {
    json!({"cpu_total":24,"memory_total_bytes":64*GIB,"memory_available_bytes":50*GIB,"disk_free_bytes":400*GIB,"pool_total_bytes":320*GIB,"pool_used_bytes":20*GIB,"instances":[{"name":"ebox-base","project":"enginebox","status":"Stopped","cpu":4,"memory_bytes":4*GIB,"disk_bytes":30*GIB,"state_disk_bytes":8*GIB,"pool":"pool","managed":false}]})
}
fn manifest() -> Value {
    fixture()["manifest"].clone()
}
#[test]
fn released_registry_digests_are_identical() {
    let f = fixture();
    let m = validate_manifest(manifest()).unwrap();
    assert_eq!(state::digest(&m), f["manifest_digest"]);
    assert_eq!(state::digest(&host().binding()), f["host_binding_digest"]);
    assert_eq!(
        state::digest(&json!(["primary", "native"])),
        f["receipt_key"]
    );
}
#[test]
fn released_unicode_warm_generation_is_identical() {
    let f = fixture();
    let r = Resources::from_value(&f["resources"], None).unwrap();
    assert_eq!(
        warm::generation(&host(), &f["template"], r, &f["snapshot"]),
        f["warm_generation"]
    );
}
#[test]
fn released_bootstrap_recipe_is_identical() {
    let f = fixture();
    let c = bootstrap::normalize(f["bootstrap"].clone()).unwrap();
    assert_eq!(bootstrap::fingerprint(&c), f["bootstrap_fingerprint"]);
}
#[test]
fn unknown_and_frozen_states_keep_cpu_and_ram() {
    for status in ["Frozen", "Migrating", "Starting", "Stopping", "Error"] {
        let mut f = facts();
        f["instances"][0]["status"] = json!(status);
        let c = capacity(&host(), &f).unwrap();
        assert_eq!(c["allocated_cpu"], 4);
        assert_eq!(c["allocated_memory_bytes"], 4 * GIB);
    }
}
#[test]
fn stopped_releases_cpu_but_keeps_disk() {
    let c = capacity(&host(), &facts()).unwrap();
    assert_eq!(c["allocated_cpu"], 0);
    assert_eq!(c["disk_committed_bytes"], 38 * GIB);
}
#[test]
fn allocations_across_projects_reduce_admission() {
    let mut f = facts();
    f["instances"].as_array_mut().unwrap().push(json!({"name":"other","project":"other","status":"Frozen","cpu":16,"memory_bytes":40*GIB,"disk_bytes":30*GIB,"state_disk_bytes":42*GIB,"pool":"pool","managed":false}));
    let c = capacity(&host(), &f).unwrap();
    assert_eq!(c["allocated_cpu"], 16);
    assert_eq!(c["max_additional"]["standard"], 1);
}
#[test]
fn sparse_disk_commitments_prevent_overcommit() {
    let mut f = facts();
    f["pool_total_bytes"] = json!(80 * GIB);
    assert_eq!(
        capacity(&host(), &f).unwrap()["max_additional"]["standard"],
        0
    );
}
#[test]
fn actual_free_ram_limits_budget() {
    let mut f = facts();
    f["memory_available_bytes"] = json!(9 * GIB);
    assert_eq!(capacity(&host(), &f).unwrap()["max_additional"]["small"], 0);
}
#[test]
fn contribution_caps_include_owned_templates_across_projects() {
    let mut h = host();
    h.contribution_cpu = Some(6);
    h.contribution_memory_gib = Some(8);
    let mut f = facts();
    f["instances"][0]["status"] = json!("Frozen");
    f["instances"][0]["project"] = json!("other");
    f["instances"][0]["enginebox_owned"] = json!(true);
    let c = capacity(&h, &f).unwrap();
    assert_eq!(c["cpu_available"], 2);
    assert_eq!(c["memory_budget_available_bytes"], 4 * GIB);
    assert!(
        !resource_fits(
            &h,
            &f,
            Resources {
                cpu: 4,
                memory_gib: 2,
                disk_gib: 30
            }
        )
        .unwrap()
    );
}
#[test]
fn contribution_limits_cannot_relax_physical_reserves() {
    let mut h = host();
    h.contribution_cpu = Some(100);
    h.contribution_memory_gib = Some(100);
    let c = capacity(&h, &facts()).unwrap();
    assert_eq!(c["cpu_available"], 20);
    assert_eq!(c["memory_budget_available_bytes"], 42 * GIB);
}
#[test]
fn malformed_resource_allocations_are_conservative() {
    assert_eq!(cpu_count(&json!("0"), 16), 16);
    assert_eq!(cpu_count(&json!("-1"), 16), 16);
    assert_eq!(cpu_count(&json!("1-4,3-6"), 16), 6);
    assert_eq!(cpu_count(&json!("0-999999999"), 16), 16);
    assert_eq!(resource_bytes(&json!("50%"), 64 * GIB), 64 * GIB);
    assert_eq!(resource_bytes(&json!("4GiB"), 0), 4 * GIB);
}
#[test]
fn custom_resources_respect_all_minima() {
    let m = manifest();
    let min = Resources::from_value(&m["minimum"], None).unwrap();
    for r in [
        Resources { cpu: 1, ..min },
        Resources {
            memory_gib: 1,
            ..min
        },
        Resources {
            disk_gib: 29,
            ..min
        },
    ] {
        assert!(r.validate(Some(min)).is_err());
    }
    assert!(preset(&host(), &m, Some("small"), Some(&json!(min))).is_err());
}
#[test]
fn host_config_rejects_missing_and_duplicate_targets() {
    let dir = private_temp();
    let path = dir.path().join("fleet.toml");
    for content in [
        "schema_version=1\n[[hosts]]\nid='one'\n",
        "schema_version=1\n[[hosts]]\nid='one'\ntarget='same'\n[[hosts]]\nid='two'\ntarget='same'\n",
        "schema_version=1\n[[hosts]]\nid='one'\ntarget='same'\n[hosts.guest]\nuid=true\n",
    ] {
        std::fs::write(&path, content).unwrap();
        assert!(load_hosts(&path).is_err());
    }
}
#[test]
fn host_guest_defaults_never_inherit_operator_settings() {
    let dir = private_temp();
    let path = dir.path().join("fleet.toml");
    std::fs::write(
        &path,
        "schema_version=1\n[[hosts]]\nid='one'\ntarget='server'\n",
    )
    .unwrap();
    assert_eq!(load_hosts(&path).unwrap()[0].guest, GuestConfig::default());
}
#[test]
fn guest_configuration_rejects_root_and_traversal() {
    let mut g = GuestConfig {
        uid: 0,
        ..Default::default()
    };
    assert!(g.validate().is_err());
    g.uid = 1001;
    g.home = "/home/agent/../../root".into();
    assert!(g.validate().is_err());
    g.home = "/home/agent".into();
    g.display = "evil:0".into();
    assert!(g.validate().is_err());
}
#[test]
fn durable_pool_claim_overrides_stale_ready_config() {
    let cfg = json!({"user.enginebox.pool.state":"ready"});
    let vm = json!({"description":"enginebox-pool:v1:{\"state\":\"claimed\",\"workspace_name\":\"ebox-private\"}"});
    let p = pool_metadata(&vm, &cfg);
    assert_eq!(p["warm_pool_state"], "claimed");
    assert_eq!(p["workspace_name"], "ebox-private");
}
#[test]
fn malformed_reservation_is_quarantined() {
    for description in [
        "enginebox-pool:v1:garbage",
        "enginebox-pool:v1:{\"state\":\"ready\"}",
        "enginebox-pool:v1:{\"state\":\"claimed\",\"workspace_name\":\"../x\"}",
    ] {
        assert_eq!(
            pool_metadata(
                &json!({"description":description}),
                &json!({"user.enginebox.pool.state":"ready"})
            )["warm_pool_state"],
            "quarantined"
        );
    }
}
#[test]
fn workspace_marker_alone_permanently_claims_pool() {
    let p = pool_metadata(
        &json!({}),
        &json!({"user.enginebox.pool.state":"ready","user.enginebox.workspace":"ebox-private"}),
    );
    assert_eq!(p["warm_pool_state"], "claimed");
}
#[test]
fn warm_generation_excludes_observation_fields() {
    let f = fixture();
    let r = Resources::from_value(&f["resources"], None).unwrap();
    let a = warm::generation(&host(), &f["template"], r, &f["snapshot"]);
    let mut s = f["snapshot"].clone();
    s["last_used_at"] = json!("2099-01-01");
    s["size"] = json!(123);
    s["expires_at"] = json!("2099");
    assert_eq!(a, warm::generation(&host(), &f["template"], r, &s));
    s["config"]["limits.cpu"] = json!("8");
    assert_ne!(a, warm::generation(&host(), &f["template"], r, &s));
}
#[test]
fn warm_eligibility_requires_unclaimed_frozen_exact_budget() {
    let h = host();
    let r = Resources {
        cpu: 2,
        memory_gib: 2,
        disk_gib: 30,
    };
    let vm = json!({"project":h.project,"pool":h.pool,"managed":true,"warm_pool_state":"ready","pool_claim":null,"status":"Frozen","template":"native","pool_generation":"digest","cpu":2,"memory_bytes":2*GIB,"disk_bytes":30*GIB,"state_disk_bytes":4*GIB});
    assert!(warm::eligible(&vm, &h, "native", r, "digest"));
    for (k, v) in [
        ("status", json!("Running")),
        ("pool_claim", json!({"state":"claimed"})),
        ("state_disk_bytes", json!(3 * GIB)),
        ("cpu", json!(1)),
        ("pool", json!("foreign")),
    ] {
        let mut changed = vm.clone();
        changed[k] = v;
        assert!(!warm::eligible(&changed, &h, "native", r, "digest"));
    }
}
#[test]
fn private_workspace_guard_excludes_bases_and_reserves() {
    let h = host();
    let mut f = facts();
    assert!(enginebox_core::fleet::private_workspace(&h, "ebox-base", &f).is_err());
    f["instances"][0]["managed"] = json!(true);
    f["instances"][0]["warm_pool_state"] = json!("ready");
    assert!(enginebox_core::fleet::private_workspace(&h, "ebox-base", &f).is_err());
    f["instances"][0]["warm_pool_state"] = json!("claimed");
    assert!(enginebox_core::fleet::private_workspace(&h, "ebox-base", &f).is_ok());
}
#[test]
fn registry_changed_manifest_requires_strictly_new_revision() {
    let dir = private_temp();
    let r = Registry::new(dir.path()).unwrap();
    r.register(manifest()).unwrap();
    r.register(manifest()).unwrap();
    let mut changed = manifest();
    changed["label"] = json!("Changed");
    assert!(r.register(changed.clone()).is_err());
    changed["revision"] = json!(4);
    assert!(r.register(changed).is_ok());
}
#[test]
fn registry_rejects_unsigned_path_fields_and_arbitrary_recipes() {
    for (k, v) in [
        ("id", json!("../escape")),
        ("recipe", json!("curl-and-execute")),
        ("source_image", json!("images:other")),
        ("native_clients", json!(["codex", "codex"])),
    ] {
        let mut m = manifest();
        m[k] = v;
        assert!(validate_manifest(m).is_err());
    }
    let mut m = manifest();
    m["url"] = json!("https://example.com/script");
    assert!(validate_manifest(m).is_err());
}
#[cfg(unix)]
#[test]
fn registry_refuses_symlinks_and_unprotected_state() {
    use std::os::unix::fs::{PermissionsExt, symlink};
    let dir = private_temp();
    let outside = private_temp();
    symlink(outside.path(), dir.path().join("templates")).unwrap();
    assert!(Registry::new(dir.path()).is_err());
    let dir = private_temp();
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
    assert!(Registry::new(dir.path()).is_err());
}
#[test]
fn registry_receipt_binding_cannot_silently_change_hosts() {
    let dir = private_temp();
    let f = Fleet::from_hosts(vec![host()], dir.path()).unwrap();
    let registered = f.register_template(manifest()).unwrap();
    let h = host();
    let mut receipt = json!({"id":"native","host_id":"primary","host_binding":h.binding(),"host_binding_digest":state::digest(&h.binding()),"manifest_digest":registered["manifest_digest"],"source":"ebox-base","snapshot":"prepared","label":"Native","minimum":manifest()["minimum"],"recommended":manifest()["recommended"]});
    let path = f.registry.root.join("deployments").join(format!(
        "{}.json",
        state::digest(&json!(["primary", "native"]))
    ));
    state::write_json(&path, &receipt).unwrap();
    assert_eq!(f.templates_for(&h).unwrap().len(), 1);
    let mut moved = h.clone();
    moved.target = "other-server".into();
    assert_eq!(f.templates_for(&moved).unwrap().len(), 0);
    receipt["host_binding"]["pool"] = json!("forged");
    state::write_json(&path, &receipt).unwrap();
    assert!(f.registry.deployments(None).is_err());
}
#[test]
fn template_import_refuses_nonpublic_address_classes() {
    for ip in [
        "127.0.0.1",
        "10.0.0.1",
        "172.16.0.1",
        "192.168.1.2",
        "169.254.169.254",
        "100.64.0.1",
        "192.0.2.1",
        "198.18.0.1",
        "224.0.0.1",
        "240.0.0.1",
        "::1",
        "fc00::1",
        "fe80::1",
        "::ffff:127.0.0.1",
        "2001:db8::1",
        "2002:c000:0201::",
    ] {
        assert!(!public_ip(ip.parse().unwrap()), "{ip}");
    }
    for ip in ["1.1.1.1", "8.8.8.8", "2606:4700:4700::1111"] {
        assert!(public_ip(ip.parse().unwrap()));
    }
}
fn observed(c: &Value) -> Value {
    json!({"system":"Linux","architecture":"x86_64","uid":0,"kvm":true,"os":{"ID":"ubuntu","VERSION_ID":"24.04"},"cpu_count":16,"memory_available":32*GIB,"host_disk_free":500*GIB,"incus_disk_free":500*GIB,"incus_present":true,"server":{"environment":{"server_version":"6.0.5","driver":"qemu | lxc"},"api_extensions":["migration_stateful"]},"qemu_version":"QEMU emulator version 10.2.1","projects":[],"profiles":[],"instances":[],"storage":[{"name":c["storage"]["pool"],"driver":"btrfs","config":{}}],"pool_resources":{"space":{"total":200*GIB,"used":10*GIB}},"networks":[{"name":c["network"]["name"],"type":"bridge","managed":true,"config":{"ipv4.address":"10.9.0.1/24"}}],"routes":[],"errors":[]})
}
fn add_owned(c: &Value, o: &mut Value) {
    o["projects"] = json!([{"name":c["incus"]["project"],"config":{"user.enginebox.owner":c["incus"]["project"]}}]);
    let mut profile = bootstrap::profile_definition(c);
    profile["name"] = c["incus"]["profile"].clone();
    o["profiles"] = json!([profile]);
    o["instances"] = json!([{"name":c["workspace"]["name"],"status":"Running","type":"virtual-machine","profiles":[c["incus"]["profile"]],"config":{"user.enginebox.owner":c["incus"]["project"],"user.enginebox.recipe":bootstrap::fingerprint(c),"user.enginebox.prepared":bootstrap::fingerprint(c)}}]);
}
#[test]
fn bootstrap_plan_only_creates_owned_named_resources() {
    let c = fixture()["bootstrap"].clone();
    let p = bootstrap::build_plan(&c, &observed(&c)).unwrap();
    assert_eq!(p["ok"], true, "{p}");
    assert_eq!(
        p["operations"],
        json!([{"action":"create_project"},{"action":"create_profile"},{"action":"create_workspace"},{"action":"prepare_workspace"}])
    );
}
#[test]
fn bootstrap_second_apply_is_noop_even_when_ram_is_used() {
    let c = fixture()["bootstrap"].clone();
    let mut o = observed(&c);
    add_owned(&c, &mut o);
    o["memory_available"] = json!(GIB);
    let p = bootstrap::build_plan(&c, &o).unwrap();
    assert_eq!(p["ok"], true, "{p}");
    assert_eq!(p["operations"], json!([]));
}
#[test]
fn bootstrap_rejects_foreign_profile_and_recipe_drift() {
    let c = fixture()["bootstrap"].clone();
    let mut o = observed(&c);
    add_owned(&c, &mut o);
    o["profiles"][0]["config"]["user.enginebox.owner"] = json!("foreign");
    assert_eq!(bootstrap::build_plan(&c, &o).unwrap()["ok"], false);
    add_owned(&c, &mut o);
    o["instances"][0]["config"]["user.enginebox.recipe"] = json!("drift");
    assert_eq!(bootstrap::build_plan(&c, &o).unwrap()["ok"], false);
}
#[test]
fn bootstrap_rejects_route_overlap_without_modifying_firewall() {
    let mut c = fixture()["bootstrap"].clone();
    c["network"]["mode"] = json!("managed");
    let mut o = observed(&c);
    o["networks"] = json!([]);
    o["routes"] = json!([{"dst":"10.203.0.0/16"}]);
    assert_eq!(bootstrap::build_plan(&c, &o).unwrap()["ok"], false);
}
#[test]
fn bootstrap_guards_platform_state_disk_and_configuration_types() {
    let c = fixture()["bootstrap"].clone();
    for (section, key, value) in [
        ("host", "target", json!("bad;rm -rf /")),
        ("workspace", "cpu", json!(true)),
        ("workspace", "state_disk", json!("4GiB")),
        ("workspace", "path", json!("/tmp/../root")),
        ("network", "subnet", json!("8.8.8.1/24")),
    ] {
        let mut m = c.clone();
        m[section][key] = value;
        assert!(bootstrap::normalize(m).is_err());
    }
    let mut o = observed(&c);
    o["system"] = json!("Darwin");
    assert_eq!(bootstrap::build_plan(&c, &o).unwrap()["ok"], false);
}
#[test]
fn bootstrap_admission_counts_prepared_templates_in_contribution() {
    let mut c = fixture()["bootstrap"].clone();
    c["host"]["admission"] = json!({"reserve_cpu":2,"reserve_memory_gib":4,"reserve_disk_gib":10,"max_instances":16,"contribution_cpu":4,"contribution_memory_gib":8});
    let mut o = observed(&c);
    let mut f = facts();
    f["instances"][0]["status"] = json!("Frozen");
    f["instances"][0]["enginebox_owned"] = json!(true);
    o["fleet_facts"] = f;
    assert!(!bootstrap::admission_errors(&c, &o).unwrap().is_empty());
}
#[test]
fn portable_allocation_retains_personal_machine_reserves() {
    let report =
        capabilities::allocation(Some(16), Some(32 * GIB), Some(100 * GIB), None, None, None);
    assert_eq!(report["maximum"]["cpus"], 8);
    assert_eq!(report["maximum"]["memory_bytes"], 16 * GIB);
    assert_eq!(report["enforced"], false);
    assert_eq!(
        capabilities::allocation(
            Some(16),
            Some(32 * GIB),
            Some(100 * GIB),
            Some(12),
            None,
            None
        )["status"],
        "rejected"
    );
}
#[test]
fn clean_base_check_blocks_credentials_but_not_empty_native_config() {
    let dir = private_temp();
    std::fs::create_dir(dir.path().join(".claude")).unwrap();
    std::fs::write(
        dir.path().join(".claude.json"),
        r#"{"mcpServers":{},"theme":"dark"}"#,
    )
    .unwrap();
    assert!(guest::clean_base_check(dir.path()).is_ok());
    std::fs::write(
        dir.path().join(".claude.json"),
        r#"{"nested":{"refresh_token":"private"}}"#,
    )
    .unwrap();
    assert!(guest::clean_base_check(dir.path()).is_err());
}
#[test]
fn guest_bundle_validation_rejects_macos_and_wrong_arch() {
    let d = private_temp();
    let p = d.path().join("bin");
    std::fs::write(&p, [0u8; 20]).unwrap();
    assert!(bootstrap::validate_guest_binary(&p).is_err());
    let mut b = [0u8; 20];
    b[..4].copy_from_slice(b"\x7fELF");
    b[4] = 2;
    b[5] = 1;
    b[18] = 183;
    std::fs::write(&p, b).unwrap();
    assert!(bootstrap::validate_guest_binary(&p).is_err());
    b[18] = 62;
    std::fs::write(&p, b).unwrap();
    assert!(bootstrap::validate_guest_binary(&p).is_ok());
}
#[cfg(unix)]
#[test]
fn command_timeout_reaps_descendants() {
    use enginebox_core::transport::run_process;
    let start = std::time::Instant::now();
    let r = run_process(
        &["sh".into(), "-c".into(), "sleep 60 & wait".into()],
        None,
        std::time::Duration::from_millis(80),
    );
    assert!(r.is_err());
    assert!(start.elapsed() < std::time::Duration::from_secs(3));
}

fn private_temp() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    dir
}

#[test]
fn manifest_decoder_rejects_duplicate_keys_at_any_depth() {
    use enginebox_core::templates::decode_json;
    for data in [
        r#"{"revision":1,"revision":2}"#,
        r#"{"config":{"browser":true,"browser":false}}"#,
        r#"{"x":1,"\u0078":2}"#,
    ] {
        assert!(decode_json(data.as_bytes()).is_err());
    }
    assert_eq!(
        decode_json(br#"{"items":[1,true,null,"ok"]}"#).unwrap(),
        json!({"items":[1,true,null,"ok"]})
    );
}

#[test]
fn helper_migration_requires_owned_running_workspace_or_recipe() {
    let private = json!({"name":"ebox-private","type":"virtual-machine","status":"Running","config":{"user.enginebox.owner":"enginebox","user.enginebox.fleet":"true"}});
    assert!(guest::validate_helper_target(&private, "enginebox").is_ok());
    for (k, v) in [("type", json!("container")), ("status", json!("Frozen"))] {
        let mut vm = private.clone();
        vm[k] = v;
        assert!(guest::validate_helper_target(&vm, "enginebox").is_err());
    }
    let mut foreign = private.clone();
    foreign["config"]["user.enginebox.owner"] = json!("another");
    assert!(guest::validate_helper_target(&foreign, "enginebox").is_err());
    let mut quarantined = private.clone();
    quarantined["description"] = json!("enginebox-pool:v1:{\"state\":\"quarantined\"}");
    assert!(guest::validate_helper_target(&quarantined, "enginebox").is_err());
    let mut base = private;
    base["config"]
        .as_object_mut()
        .unwrap()
        .remove("user.enginebox.fleet");
    assert!(guest::validate_helper_target(&base, "enginebox").is_err());
    base["config"]["user.enginebox.recipe"] = json!("a".repeat(64));
    assert!(guest::validate_helper_target(&base, "enginebox").is_ok());
}
