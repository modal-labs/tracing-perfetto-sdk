use tracing_perfetto_sdk_sys::ffi;

// This is in its own test file to get a new process for this test
#[test]
fn invalid_config() {
    ffi::perfetto_global_init(|_, _, _, _| {}, "rust_tracing", false, false);
    let result = ffi::new_tracing_session(&[0x01], -1);
    if let Err(e) = result {
        let expected =
            "trace_config_bytes didn't contain a valid perfetto::TraceConfig proto message";
        let actual = e.to_string();
        assert_eq!(expected, actual);
    } else {
        panic!("expected new_tracing_session to fail");
    }
}
