use enginebox_core::{Fleet, Host, preparation, warm};
use serde_json::json;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

#[test]
fn required_registration_fences_all_new_fleet_admission_before_host_commands() {
    let root = tempfile::tempdir().unwrap();
    let host = Host {
        id: "server".into(),
        target: "must-not-contact.invalid".into(),
        managed_registration: true,
        ..Default::default()
    };
    let fleet = Fleet::from_hosts(vec![host.clone()], &root.path().join("state")).unwrap();
    assert!(fleet.assert_admission("server").is_err());
    for operation in [
        "create",
        "allocate",
        "resume",
        "restore",
        "install_template",
    ] {
        let error = fleet
            .operate("server", operation, &json!({}))
            .unwrap_err()
            .to_string();
        assert!(error.contains("registration"), "{operation}: {error}");
    }
    assert!(
        preparation::prepare(&fleet, "server", "game", &json!({}))
            .unwrap_err()
            .to_string()
            .contains("registration")
    );
    assert!(
        warm::allocate(&fleet, &host, "ebox-test", "native", None, None)
            .unwrap_err()
            .to_string()
            .contains("registration")
    );
    assert!(
        warm::reconcile_once(&fleet, &host)
            .unwrap_err()
            .to_string()
            .contains("registration")
    );
    let reconcile = fleet.reconcile().unwrap();
    assert_eq!(reconcile["changed"], json!([]));
    assert!(
        reconcile["failures"][0]["error"]
            .as_str()
            .unwrap()
            .contains("registration")
    );
    // Inspection still resolves the host; no VM lifecycle is changed on revoke.
    assert_eq!(fleet.host("server").unwrap().target, host.target);
    let live = Arc::new(AtomicBool::new(true));
    let lease = live.clone();
    fleet.set_admission_guard(Arc::new(move |_| {
        anyhow::ensure!(lease.load(Ordering::SeqCst), "Node revoked");
        Ok(())
    }));
    assert!(fleet.assert_admission("server").is_ok());
    live.store(false, Ordering::SeqCst);
    assert!(fleet.assert_admission("server").is_err());
}

#[test]
fn opt_in_is_a_strict_configuration_gate_without_changing_template_binding() {
    let host = Host {
        id: "server".into(),
        target: "local".into(),
        ..Default::default()
    };
    let original = serde_json::to_value(&host).unwrap();
    assert!(original.get("managed_registration").is_none());
    let mut managed = host.clone();
    managed.managed_registration = true;
    let required = serde_json::to_value(&managed).unwrap();
    assert_eq!(required["managed_registration"], true);
    assert_eq!(host.binding(), managed.binding());
    // New config cannot silently turn the required policy off by misspelling it.
    let mut invalid = required;
    invalid["managed_registration_typo"] = json!(true);
    assert!(serde_json::from_value::<Host>(invalid).is_err());
    let root = tempfile::tempdir().unwrap();
    let fleet = Fleet::from_hosts(vec![host], &root.path().join("state")).unwrap();
    assert!(fleet.assert_admission("server").is_ok());
}
