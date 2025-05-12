#[cfg(feature = "sdk")]
use tracing_perfetto_sdk_sys::ffi;

#[allow(unused_variables)]
pub fn global_init(name: &str, enable_in_process_backend: bool, enable_system_backend: bool) {
    #[cfg(feature = "sdk")]
    ffi::perfetto_global_init(
        log_callback,
        name,
        enable_in_process_backend,
        enable_system_backend,
    );
}

#[cfg(feature = "sdk")]
fn log_callback(level: ffi::LogLev, line: i32, filename: &str, message: &str) {
    match level {
        ffi::LogLev::Debug => {
            tracing::debug!(target: "perfetto-sdk", filename, line, message);
        }
        ffi::LogLev::Info => {
            tracing::info!(target: "perfetto-sdk", filename, line, message);
        }
        ffi::LogLev::Important => {
            tracing::warn!(target: "perfetto-sdk", filename, line, message);
        }
        ffi::LogLev::Error => {
            tracing::error!(target: "perfetto-sdk", filename, line, message);
        }
        ffi::LogLev {
            repr: 4_u8..=u8::MAX,
        } => {}
    }
}
