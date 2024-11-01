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
        (TRACK_UUID_NS, TASK_NS, id).hash(&mut h);
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
