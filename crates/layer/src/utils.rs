extern crate libc;

pub(crate) fn get_os_thread_id() -> usize {
    unsafe { libc::gettid() as usize }
}
