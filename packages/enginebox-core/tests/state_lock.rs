#![cfg(unix)]

#[test]
fn a_forked_child_cannot_retain_a_released_operation_lock() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("worker.lock");
    let guard = enginebox_core::state::lock(&path, false).unwrap();
    let child = unsafe { libc::fork() };
    assert!(child >= 0);
    if child == 0 {
        // Only async-signal-safe calls after fork in the multithreaded harness.
        unsafe {
            libc::sleep(1);
            libc::_exit(0);
        }
    }
    drop(guard);
    let reacquired = enginebox_core::state::lock(&path, false);
    unsafe {
        libc::waitpid(child, std::ptr::null_mut(), 0);
    }
    assert!(
        reacquired.is_ok(),
        "A child retained the completed operation's lock"
    );
}
