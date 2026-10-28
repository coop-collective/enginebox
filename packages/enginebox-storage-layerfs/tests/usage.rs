#[test]
fn bounded_storage_usage_collector() {
    enginebox_core::transport::run_process(
        &[
            "node".into(),
            "--test".into(),
            concat!(env!("CARGO_MANIFEST_DIR"), "/tests/usage.mjs").into(),
        ],
        None,
        std::time::Duration::from_secs(30),
    )
    .expect("storage metadata collector tests");
}
