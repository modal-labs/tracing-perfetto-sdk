use tracing_perfetto_sdk_sys::ffi;

// This is in its own test file to get a new process for this test
#[test]
fn happy_path() {
    ffi::perfetto_global_init(|_, _, _, _| {}, "rust_tracing", false, false);
    let result = ffi::new_tracing_session(&[], -1);
    assert!(result.is_ok());
}
