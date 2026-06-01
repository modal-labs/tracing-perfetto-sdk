use std::hash;
use std::hash::Hash as _;
use std::hash::Hasher as _;

#[cfg(feature = "tokio")]
use tokio::task;

// Seeds for consistent hashing of pid/tid/task id
const TRACK_UUID_NS: u32 = 1;
const SEQUENCE_ID_NS: u32 = 2;

const PROCESS_NS: u32 = 1;
const THREAD_NS: u32 = 2;
#[cfg(feature = "tokio")]
const TOKIO_NS: u32 = 3;
#[cfg(feature = "tokio")]
const TASK_NS: u32 = 4;
const COUNTER_NS: u32 = 5;

#[derive(Clone, Copy, Debug, Default, Eq, Hash, Ord, PartialEq, PartialOrd)]
#[repr(transparent)]
pub struct TrackUuid(u64);

#[derive(Clone, Copy, Debug, Default, Eq, Hash, Ord, PartialEq, PartialOrd)]
#[repr(transparent)]
pub struct SequenceId(u32);

impl TrackUuid {
    pub fn for_process(pid: u32) -> TrackUuid {
        let mut h = hash::DefaultHasher::new();
        (TRACK_UUID_NS, PROCESS_NS, pid).hash(&mut h);
        TrackUuid(h.finish())
    }

    pub fn for_thread(tid: usize) -> TrackUuid {
        let mut h = hash::DefaultHasher::new();
        (TRACK_UUID_NS, THREAD_NS, tid).hash(&mut h);
        TrackUuid(h.finish())
    }

    #[cfg(feature = "tokio")]
    pub fn for_task(id: task::Id) -> TrackUuid {
        let mut h = hash::DefaultHasher::new();
        (TRACK_UUID_NS, TASK_NS, id).hash(&mut h);
        TrackUuid(h.finish())
    }

    #[cfg(feature = "tokio")]
    pub fn for_tokio() -> TrackUuid {
        let mut h = hash::DefaultHasher::new();
        (TRACK_UUID_NS, TOKIO_NS).hash(&mut h);
        TrackUuid(h.finish())
    }

    pub fn for_counter(counter_name: &str) -> TrackUuid {
        let mut h = hash::DefaultHasher::new();
        (TRACK_UUID_NS, COUNTER_NS, counter_name).hash(&mut h);
        TrackUuid(h.finish())
    }

    pub fn as_raw(self) -> u64 {
        self.0
    }
}

impl SequenceId {
    pub fn for_thread(tid: usize) -> SequenceId {
        let mut h = hash::DefaultHasher::new();
        (SEQUENCE_ID_NS, THREAD_NS, tid).hash(&mut h);
        SequenceId(h.finish() as u32)
    }

    #[cfg(feature = "tokio")]
    pub fn for_task(id: task::Id) -> SequenceId {
        let mut h = hash::DefaultHasher::new();
        (SEQUENCE_ID_NS, TASK_NS, id).hash(&mut h);
        SequenceId(h.finish() as u32)
    }

    pub fn for_counter(counter_name: &str) -> SequenceId {
        let mut h = hash::DefaultHasher::new();
        (SEQUENCE_ID_NS, COUNTER_NS, counter_name).hash(&mut h);
        SequenceId(h.finish() as u32)
    }

    pub fn as_raw(self) -> u32 {
        self.0
    }
}

pub(crate) fn thread_id() -> usize {
    os_id::thread::get_raw_id() as usize
}

/// Returns the OS-level name of the current thread, if it has one.
///
/// Unlike [`std::thread::Thread::name`] this also sees threads created outside
/// of Rust (for example by C libraries such as GLib), which never carry a
/// Rust-side name. Returns `None` on platforms where the name cannot be
/// queried.
#[cfg(any(
    target_os = "linux",
    target_os = "android",
    target_os = "macos",
    target_os = "ios"
))]
pub(crate) fn os_thread_name() -> Option<String> {
    // The name is set via pthread_setname_np / prctl(PR_SET_NAME); 64 bytes is
    // larger than every platform's limit.
    let mut buf = [0 as libc::c_char; 64];
    // SAFETY: `buf` is a valid, writable buffer of `buf.len()` bytes.
    let ret =
        unsafe { libc::pthread_getname_np(libc::pthread_self(), buf.as_mut_ptr(), buf.len()) };
    if ret != 0 {
        return None;
    }
    // SAFETY: on success the buffer holds a NUL-terminated C string.
    let name = unsafe { std::ffi::CStr::from_ptr(buf.as_ptr()) };
    match name.to_str() {
        Ok(s) if !s.is_empty() => Some(s.to_owned()),
        _ => None,
    }
}

#[cfg(windows)]
pub(crate) fn os_thread_name() -> Option<String> {
    use std::ffi::c_void;

    // Minimal kernel32 declarations to avoid pulling in a Win32 binding crate
    // just for the thread name. The name is set via SetThreadDescription.
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetCurrentThread() -> *mut c_void;
        fn GetThreadDescription(thread: *mut c_void, description: *mut *mut u16) -> i32;
        fn LocalFree(mem: *mut c_void) -> *mut c_void;
    }

    let mut wide: *mut u16 = std::ptr::null_mut();
    // SAFETY: `GetCurrentThread` is a pseudo-handle that needs no closing;
    // `GetThreadDescription` writes an owned UTF-16 string pointer into `wide`.
    let hr = unsafe { GetThreadDescription(GetCurrentThread(), &mut wide) };
    // `hr` is an HRESULT; negative means failure (SUCCEEDED(hr) == hr >= 0).
    if hr < 0 || wide.is_null() {
        return None;
    }
    // SAFETY: on success `wide` points to a NUL-terminated UTF-16 string that we
    // must release with `LocalFree`.
    let mut len = 0usize;
    while unsafe { *wide.add(len) } != 0 {
        len += 1;
    }
    let name = String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(wide, len) });
    // SAFETY: `wide` was allocated by `GetThreadDescription`.
    unsafe { LocalFree(wide as *mut c_void) };
    if name.is_empty() { None } else { Some(name) }
}

#[cfg(not(any(
    target_os = "linux",
    target_os = "android",
    target_os = "macos",
    target_os = "ios",
    windows
)))]
pub(crate) fn os_thread_name() -> Option<String> {
    None
}
