//! Process-global runtime objects (§5.1): the NET and RX tokio runtimes, songbird's `Scheduler` and
//! `DisposalThread`, process flags, thread roles and live-instance counters.
//!
//! None of these is ever dropped: exit joins no Rust thread, and by I2 no Rust-created thread can
//! touch the interpreter during finalisation.

use std::cell::Cell;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

use parking_lot::Mutex;
use songbird::driver::{DisposalThread, Scheduler, SchedulerConfig};
use tokio::runtime::Runtime;

use crate::error::CoreError;
use crate::limits::{RUNTIME_WORKERS, check};

/// Worker counts of the two owned runtimes (`configure_runtime`, §5.1). Both must be in `1..=8`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RuntimeConfig {
    pub net_workers: usize,
    pub rx_workers: usize,
}

impl Default for RuntimeConfig {
    fn default() -> Self {
        RuntimeConfig {
            net_workers: 2,
            rx_workers: 1,
        }
    }
}

struct ConfigState {
    cfg: RuntimeConfig,
    started: bool,
}

/// Leaf lock: held only to read or replace the config and flip `started`.
static CONFIG: Mutex<ConfigState> = Mutex::new(ConfigState {
    cfg: RuntimeConfig {
        net_workers: 2,
        rx_workers: 1,
    },
    started: false,
});

static NET: OnceLock<Runtime> = OnceLock::new();
static RX: OnceLock<Runtime> = OnceLock::new();
static SCHEDULER: OnceLock<Scheduler> = OnceLock::new();
static DISPOSER: OnceLock<DisposalThread> = OnceLock::new();

/// Set by `native._shutdown_all()` at `atexit`; turns logging into a no-op (§4.10, §11.2).
pub static SHUTTING_DOWN: AtomicBool = AtomicBool::new(false);
/// Set in a forked child by `native._disable_after_fork()` (§4.10).
pub static FORKED: AtomicBool = AtomicBool::new(false);

/// Sizes the runtimes. Valid only before the first runtime touch; afterwards `RuntimeConfigured`.
pub fn configure(cfg: RuntimeConfig) -> Result<(), CoreError> {
    check("net_workers", cfg.net_workers, RUNTIME_WORKERS)?;
    check("rx_workers", cfg.rx_workers, RUNTIME_WORKERS)?;
    let mut st = CONFIG.lock();
    if st.started {
        return Err(CoreError::RuntimeConfigured);
    }
    st.cfg = cfg;
    Ok(())
}

/// Whether either runtime has been created (after which [`configure`] fails).
pub fn is_started() -> bool {
    CONFIG.lock().started
}

fn start_config() -> RuntimeConfig {
    let mut st = CONFIG.lock();
    st.started = true;
    st.cfg
}

fn try_build(
    name: &'static str,
    workers: usize,
    max_blocking: usize,
    role: Role,
) -> std::io::Result<Runtime> {
    let counter = AtomicUsize::new(0);
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(workers)
        .max_blocking_threads(max_blocking)
        .enable_all()
        .thread_name_fn(move || format!("{name}-{}", counter.fetch_add(1, Ordering::Relaxed)))
        .on_thread_start(move || set_role(role))
        .build()
}

// Building a runtime only fails when the OS refuses threads or an event queue. Nothing in this library
// can run without it, and the first touch happens on a Python thread, where pyo3 turns the panic into
// a `PanicException` instead of unwinding through foreign frames.
#[allow(clippy::panic)]
fn build_runtime(name: &'static str, workers: usize, max_blocking: usize, role: Role) -> Runtime {
    match try_build(name, workers, max_blocking, role) {
        Ok(rt) => rt,
        Err(first) => match try_build(name, 1, max_blocking, role) {
            Ok(rt) => {
                ::log::error!(
                    "building the {name} runtime with {workers} workers failed ({first}); using 1 worker"
                );
                rt
            }
            Err(second) => {
                panic!("failed to build the {name} tokio runtime: {first}; retry: {second}")
            }
        },
    }
}

/// The NET runtime: songbird's core/events/WS/UDP/Idle tasks and `Dispatch::act` (§5.2).
pub fn net() -> &'static Runtime {
    NET.get_or_init(|| build_runtime("songbird-net", start_config().net_workers, 16, Role::Net))
}

/// The RX runtime: ConnWorkers, connect futures and track ops (§5.2).
pub fn rx() -> &'static Runtime {
    RX.get_or_init(|| build_runtime("songbird-rx", start_config().rx_workers, 4, Role::Rx))
}

/// songbird's mixer `Scheduler`, created under `net().enter()` because it spawns its Idle task.
pub fn scheduler() -> &'static Scheduler {
    SCHEDULER.get_or_init(|| {
        let _enter = net().enter();
        Scheduler::new(SchedulerConfig::default())
    })
}

/// The single process-wide songbird `DisposalThread`.
pub fn disposer() -> &'static DisposalThread {
    DISPOSER.get_or_init(DisposalThread::run)
}

/// `true` when this process must not touch state created by `creator_pid`: after `fork()`, or when the
/// creating pid differs from ours (§4.10). `close()` and every `Drop` then skip teardown and leak.
pub fn in_foreign_process(creator_pid: u32) -> bool {
    FORKED.load(Ordering::Acquire) || std::process::id() != creator_pid
}

/// `Err(Forked)` in a forked child (every native entry point checks this).
pub fn check_not_forked() -> Result<(), CoreError> {
    if FORKED.load(Ordering::Acquire) {
        Err(CoreError::Forked)
    } else {
        Ok(())
    }
}

// ── thread roles (debug enforcement, §5.2) ───────────────────────────────────────────────────────────

/// What kind of thread is running. Set by runtime hooks, mixer readers and decode threads.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Role {
    Unknown,
    Python,
    Net,
    Rx,
    Mixer,
    Decode,
    Harness,
}

thread_local! {
    static ROLE: Cell<Role> = const { Cell::new(Role::Unknown) };
}

/// The current thread's role (`Unknown` if never set, or during thread-local destruction).
pub fn role() -> Role {
    ROLE.try_with(Cell::get).unwrap_or(Role::Unknown)
}

/// Sets the current thread's role.
pub fn set_role(r: Role) {
    let _ = ROLE.try_with(|c| c.set(r));
}

// ── live-instance counters (§11.3) ───────────────────────────────────────────────────────────────────

/// One static counter per tracked type. Owners hold a [`LiveCounter`] for each live instance.
pub mod live {
    use std::sync::atomic::AtomicU64;

    pub static CONN_SHARED: AtomicU64 = AtomicU64::new(0);
    pub static OWNED_DRIVER: AtomicU64 = AtomicU64::new(0);
    pub static CONN_WORKER: AtomicU64 = AtomicU64::new(0);
    pub static DISPATCH: AtomicU64 = AtomicU64::new(0);
    pub static SUB_QUEUE: AtomicU64 = AtomicU64::new(0);
    pub static EVENT_QUEUE: AtomicU64 = AtomicU64::new(0);
    pub static PCM_QUEUE: AtomicU64 = AtomicU64::new(0);
    pub static LIVE_READER: AtomicU64 = AtomicU64::new(0);
    pub static DONE_GUARD: AtomicU64 = AtomicU64::new(0);
    pub static SONGBIRD_INPUT: AtomicU64 = AtomicU64::new(0);
    pub static SSRC_STATE: AtomicU64 = AtomicU64::new(0);
    pub static TRACK_FLAGS: AtomicU64 = AtomicU64::new(0);
    pub static DECODE_THREAD: AtomicU64 = AtomicU64::new(0);
}

static LIVE_TABLE: [(&str, &AtomicU64); 13] = [
    ("ConnShared", &live::CONN_SHARED),
    ("OwnedDriver", &live::OWNED_DRIVER),
    ("ConnWorker", &live::CONN_WORKER),
    ("Dispatch", &live::DISPATCH),
    ("SubQueue", &live::SUB_QUEUE),
    ("EventQueue", &live::EVENT_QUEUE),
    ("PcmQueue", &live::PCM_QUEUE),
    ("LiveReader", &live::LIVE_READER),
    ("DoneGuard", &live::DONE_GUARD),
    ("SongbirdInput", &live::SONGBIRD_INPUT),
    ("SsrcState", &live::SSRC_STATE),
    ("TrackFlags", &live::TRACK_FLAGS),
    ("DecodeThread", &live::DECODE_THREAD),
];

/// RAII live-instance counter: increments on creation, decrements on drop (any thread, atomics only).
#[derive(Debug)]
pub struct LiveCounter(&'static AtomicU64);

impl LiveCounter {
    pub fn new(counter: &'static AtomicU64) -> Self {
        counter.fetch_add(1, Ordering::AcqRel);
        LiveCounter(counter)
    }
}

impl Clone for LiveCounter {
    fn clone(&self) -> Self {
        LiveCounter::new(self.0)
    }
}

impl Drop for LiveCounter {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

/// Tasks alive on `rt` (spawned and not yet finished; 0 before the runtime exists).
fn alive_tasks(rt: &OnceLock<Runtime>) -> u64 {
    rt.get().map_or(0, |rt| {
        u64::try_from(rt.metrics().num_alive_tasks()).unwrap_or(u64::MAX)
    })
}

/// Tasks alive on the NET runtime (0 before it exists): what a never-Poisoned Driver leaks.
pub fn net_alive_tasks() -> u64 {
    alive_tasks(&NET)
}

/// Every counter, `songbird_live_tasks` (= `SCHEDULER.live_tasks()`, 0 before the scheduler
/// exists), and the tasks alive on each runtime: `net_alive_tasks` (songbird's Idle task and the
/// core, events, idle-mixer, WS and UDP tasks of every Driver) and `rx_alive_tasks` (ConnWorkers,
/// connect futures, track ops). Only the task counts see a Driver that was never
/// Poisoned: our own counters drop when `OwnedDriver::teardown` returns, `Dispatch` clones go with
/// `RemoveGlobalEvents`, and a parked mixer is not a live one (§11.3). `total_tasks()` is
/// deliberately not reported. Never starts a runtime.
pub fn live_counts() -> Vec<(&'static str, u64)> {
    let mut out: Vec<(&'static str, u64)> = LIVE_TABLE
        .iter()
        .map(|(name, c)| (*name, c.load(Ordering::Acquire)))
        .collect();
    out.push((
        "songbird_live_tasks",
        SCHEDULER.get().map_or(0, Scheduler::live_tasks),
    ));
    out.push(("net_alive_tasks", alive_tasks(&NET)));
    out.push(("rx_alive_tasks", alive_tasks(&RX)));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roles_are_thread_local() {
        set_role(Role::Harness);
        assert_eq!(role(), Role::Harness);
        std::thread::spawn(|| assert_eq!(role(), Role::Unknown))
            .join()
            .unwrap();
        set_role(Role::Unknown);
    }

    #[test]
    fn live_counter_inc_dec() {
        static C: AtomicU64 = AtomicU64::new(0);
        let a = LiveCounter::new(&C);
        let b = a.clone();
        assert_eq!(C.load(Ordering::Acquire), 2);
        drop(a);
        std::thread::spawn(move || drop(b)).join().unwrap();
        assert_eq!(C.load(Ordering::Acquire), 0);
    }

    #[test]
    fn live_counts_lists_every_type() {
        let counts = live_counts();
        let names: Vec<_> = counts.iter().map(|(n, _)| *n).collect();
        for n in [
            "ConnShared",
            "OwnedDriver",
            "ConnWorker",
            "Dispatch",
            "SubQueue",
            "EventQueue",
            "PcmQueue",
            "LiveReader",
            "DoneGuard",
            "SongbirdInput",
            "SsrcState",
            "TrackFlags",
            "DecodeThread",
            "songbird_live_tasks",
            "net_alive_tasks",
            "rx_alive_tasks",
        ] {
            assert!(names.contains(&n), "missing {n}");
        }
        assert!(!names.contains(&"songbird_total_tasks"));
    }

    #[test]
    fn configure_validates_ranges() {
        let e = configure(RuntimeConfig {
            net_workers: 0,
            rx_workers: 1,
        })
        .unwrap_err();
        assert!(matches!(e, CoreError::Limit(ref l) if l.what == "net_workers"));
        let e = configure(RuntimeConfig {
            net_workers: 2,
            rx_workers: 9,
        })
        .unwrap_err();
        assert!(matches!(e, CoreError::Limit(ref l) if l.what == "rx_workers"));
    }

    #[test]
    fn runtimes_scheduler_and_lockout() {
        // Touching a runtime locks the configuration.
        let rt = net();
        let role_on_worker = rt.block_on(async { tokio::spawn(async { role() }).await.unwrap() });
        assert_eq!(role_on_worker, Role::Net);
        let rx_role = rx().block_on(async { tokio::spawn(async { role() }).await.unwrap() });
        assert_eq!(rx_role, Role::Rx);
        assert!(is_started());
        assert!(matches!(
            configure(RuntimeConfig::default()),
            Err(CoreError::RuntimeConfigured)
        ));
        let _ = scheduler();
        let _ = disposer();
        let live = live_counts()
            .into_iter()
            .find(|(n, _)| *n == "songbird_live_tasks")
            .unwrap();
        assert_eq!(live.1, 0);
    }

    #[test]
    fn foreign_process_detection() {
        assert!(!in_foreign_process(std::process::id()));
        assert!(in_foreign_process(std::process::id().wrapping_add(1)));
        assert!(check_not_forked().is_ok());
    }
}
