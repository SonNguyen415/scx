pub const VERSION: &str = env!("CARGO_PKG_VERSION");

mod alloc;
pub use alloc::ALLOCATOR;

mod rustland_builder;
pub use rustland_builder::RustLandBuilder;

use libc::{c_char};
const TASK_COMM_LEN: usize = 16;



// Task queued for scheduling from the BPF component (see bpf_intf::queued_task_ctx).
#[derive(Copy, Debug, PartialEq, Eq, PartialOrd, Clone)]
pub struct QueuedTask {
    pub pid: i32,             // pid that uniquely identifies a task
    pub cpu: i32,             // CPU previously used by the task
    pub nr_cpus_allowed: u64, // Number of CPUs that the task can use
    pub flags: u64,           // task's enqueue flags
    pub start_ts: u64,        // Timestamp since last time the task ran on a CPU (in ns)
    pub stop_ts: u64,         // Timestamp since last time the task released a CPU (in ns)
    pub exec_runtime: u64,    // Total cpu time since last sleep (in ns)
    pub weight: u64,          // Task priority in the range [1..10000] (default is 100)
    pub vtime: u64,           // Current task vruntime / deadline (set by the scheduler)
    pub enq_cnt: u64,
    pub comm: [c_char; TASK_COMM_LEN], // Task's executable name
}


impl QueuedTask {
    /// Convert the task's comm field (C char array) into a Rust String.
    #[allow(dead_code)]
    pub fn comm_str(&self) -> String {
        let bytes: &[u8] =
            unsafe { std::slice::from_raw_parts(self.comm.as_ptr() as *const u8, self.comm.len()) };

        // Find the first NUL byte, or take the whole array.
        let nul_pos = bytes.iter().position(|&c| c == 0).unwrap_or(bytes.len());

        // Convert to String (handle invalid UTF-8 gracefully).
        String::from_utf8_lossy(&bytes[..nul_pos]).into_owned()
    }
}

// Task queued for dispatching to the BPF component (see bpf_intf::dispatched_task_ctx).
#[derive(Copy, Debug, PartialEq, Eq, PartialOrd, Clone)]
pub struct DispatchedTask {
    pub pid: i32,      // pid that uniquely identifies a task
    pub cpu: i32, // target CPU selected by the scheduler (RL_CPU_ANY = dispatch on the first CPU available)
    pub flags: u64, // task's enqueue flags
    pub slice_ns: u64, // time slice in nanoseconds assigned to the task (0 = use default time slice)
    pub vtime: u64, // this value can be used to send the task's vruntime or deadline directly to the underlying BPF dispatcher
    pub enq_cnt: u64,
}

impl DispatchedTask {
    // Create a DispatchedTask from a QueuedTask.
    //
    // A dispatched task should be always originated from a QueuedTask (there is no reason to
    // dispatch a task if it wasn't queued to the scheduler earlier).
    pub fn new(task: &QueuedTask) -> Self {
        DispatchedTask {
            pid: task.pid,
            cpu: task.cpu,
            flags: task.flags,
            slice_ns: 0, // use default time slice
            vtime: 0,
            enq_cnt: task.enq_cnt,
        }
    }
}




// Orchestra API
use std::sync::atomic::{AtomicU32, Ordering};
use shared_memory::{Shmem, ShmemConf};

pub const SOCKET_PATH: &str = "/tmp/orchestra.sock";
pub const SHM_BASE:    &str = "/tmp/orchestra";
pub const QUEUE_CAP:   usize = 1024;

// #[repr(C)]
// #[derive(Copy, Clone, Debug, Default)]
// pub struct OrchestraTask { pub pid: u32 }


#[repr(C)]
#[derive(Copy, Clone, Debug, Default)]
pub struct Notification {
    pub kind:   NotifKind,
    pub cpu_id: u32,
}

#[repr(C)]
#[derive(Copy, Clone, Debug, Default)]
pub enum NotifKind {
    #[default]
    DomainExpansion,
    DomainContraction,
}

#[derive(Copy, Clone, Debug)]
pub enum ResourceKind { Cpu }

#[repr(C)]
struct QueueInner<T: Copy> {
    head:  AtomicU32,
    tail:  AtomicU32,
    slots: [T; QUEUE_CAP],
}

pub struct ShmQueue<T: Copy> {
    shm: Shmem,
    _t:  std::marker::PhantomData<T>,
}

unsafe impl<T: Copy> Send for ShmQueue<T> {}

impl<T: Copy> ShmQueue<T> {
    pub fn create(path: &str) -> Self {
        let _ = std::fs::remove_file(path);
        let shm = ShmemConf::new()
            .flink(path)
            .size(std::mem::size_of::<QueueInner<T>>())
            .create().unwrap();
        unsafe { (shm.as_ptr() as *mut QueueInner<T>).write(std::mem::zeroed()); }
        ShmQueue { shm, _t: std::marker::PhantomData }
    }

    pub fn open(path: &str) -> Self {
        let shm = loop {
            match ShmemConf::new().flink(path).open() {
                Ok(s) => break s,
                Err(_) => std::thread::sleep(std::time::Duration::from_millis(10)),
            }
        };
        ShmQueue { shm, _t: std::marker::PhantomData }
    }

    fn inner(&self) -> &QueueInner<T> {
        unsafe { &*(self.shm.as_ptr() as *const QueueInner<T>) }
    }

    pub fn enqueue(&self, item: T) -> bool {
        let q = self.inner();
        let tail = q.tail.load(Ordering::Relaxed);
        let head = q.head.load(Ordering::Acquire);
        if tail.wrapping_sub(head) as usize >= QUEUE_CAP { return false; }
        let slot = (tail as usize) & (QUEUE_CAP - 1);
        unsafe { (&q.slots[slot] as *const T as *mut T).write(item) };
        q.tail.store(tail.wrapping_add(1), Ordering::Release);
        true
    }

    pub fn dequeue(&self) -> Option<T> {
        let q = self.inner();
        let head = q.head.load(Ordering::Relaxed);
        let tail = q.tail.load(Ordering::Acquire);
        if head == tail { return None; }
        let slot = (head as usize) & (QUEUE_CAP - 1);
        let item = unsafe { (&q.slots[slot] as *const T).read() };
        q.head.store(head.wrapping_add(1), Ordering::Release);
        Some(item)
    }
}

pub struct Domain {
    pub name:           String,
    pub cpu_ids:        Vec<u32>,
    pub client_count:   u32,
    pub dispatch_queue: ShmQueue<DispatchedTask>,
    pub task_queue:     ShmQueue<QueuedTask>,
    pub notif_queue:    ShmQueue<Notification>,
}

impl Domain {
    pub fn new(name: &str) -> Self {
        Domain {
            name:           name.to_string(),
            cpu_ids:        vec![],
            client_count:   0,
            dispatch_queue: ShmQueue::create(&format!("{}-{}-dispatch", SHM_BASE, name)),
            task_queue:     ShmQueue::create(&format!("{}-{}-task",     SHM_BASE, name)),
            notif_queue:    ShmQueue::create(&format!("{}-{}-notif",    SHM_BASE, name)),
        }
    }
}

#[tarpc::service]
pub trait Orchestra {
    async fn register(domain: String)                  -> Result<(), String>;
    async fn unregister(domain: String)                -> Result<(), String>;
    async fn request_cpu(domain: String, cpu_id: u32) -> Result<(), String>;
    async fn release_cpu(domain: String, cpu_id: u32) -> Result<(), String>;
}