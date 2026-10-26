//! Pull-based logging (§11.2): works without any connection and never attaches a Rust thread.
//!
//! Any thread logs into a bounded ring with `try_lock` only (contention or a full ring drops and
//! counts the record). A Python-created daemon thread parks in [`wait`] (detached) and drains the
//! ring with [`drain`]. The level table is lock-free for readers.

use std::collections::VecDeque;
use std::fmt::{self, Write as _};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};
use std::thread::{self, Thread};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use parking_lot::Mutex;

pub use ::log::{Level, LevelFilter};

use crate::limits::{LOG_MSG_MAX, LOG_RING_CAP};
use crate::runtime::{self, Role};

/// One formatted log record, as handed to Python by `native._log_wait`.
#[derive(Debug, Clone, PartialEq)]
pub struct LogRecord {
    pub level: Level,
    pub target: Box<str>,
    /// At most [`LOG_MSG_MAX`] bytes (truncated at a char boundary, ending in `…`).
    pub message: Box<str>,
    pub file: Option<Box<str>>,
    pub line: Option<u32>,
    pub unix_ts: f64,
}

// ── level table (lock-free reads) ────────────────────────────────────────────────────────────────────

const MAX_PREFIXES: usize = 32;
const LEVEL_UNSET: u8 = u8::MAX;
/// Crate prefixes that default to INFO; everything else (songbird, rubato, …) defaults to WARN.
const OUR_PREFIXES: [&str; 2] = ["sbcore", "discord_ext_songbird_native"];

fn filter_to_u8(f: LevelFilter) -> u8 {
    match f {
        LevelFilter::Off => 0,
        LevelFilter::Error => 1,
        LevelFilter::Warn => 2,
        LevelFilter::Info => 3,
        LevelFilter::Debug => 4,
        LevelFilter::Trace => 5,
    }
}

fn u8_to_filter(v: u8) -> LevelFilter {
    match v {
        0 => LevelFilter::Off,
        1 => LevelFilter::Error,
        2 => LevelFilter::Warn,
        3 => LevelFilter::Info,
        4 => LevelFilter::Debug,
        _ => LevelFilter::Trace,
    }
}

struct LevelSlot {
    prefix: OnceLock<Box<str>>,
    level: AtomicU8,
}

impl LevelSlot {
    const fn new() -> Self {
        LevelSlot {
            prefix: OnceLock::new(),
            level: AtomicU8::new(LEVEL_UNSET),
        }
    }
}

struct LevelTable {
    slots: [LevelSlot; MAX_PREFIXES],
    /// Level for targets that match no prefix.
    fallback: AtomicU8,
}

fn prefix_matches(target: &str, prefix: &str) -> bool {
    match target.strip_prefix(prefix) {
        Some(rest) => rest.is_empty() || rest.starts_with("::"),
        None => false,
    }
}

impl LevelTable {
    const fn new() -> Self {
        LevelTable {
            slots: [const { LevelSlot::new() }; MAX_PREFIXES],
            fallback: AtomicU8::new(2), // WARN
        }
    }

    /// Sets the level for `prefix` (`""` = the fallback). `false` if the table is full.
    fn set(&self, prefix: &str, level: LevelFilter) -> bool {
        let lvl = filter_to_u8(level);
        if prefix.is_empty() {
            self.fallback.store(lvl, Ordering::Release);
            return true;
        }
        for slot in &self.slots {
            let p = slot.prefix.get_or_init(|| prefix.into());
            if &**p == prefix {
                slot.level.store(lvl, Ordering::Release);
                return true;
            }
        }
        false
    }

    fn level_for(&self, target: &str) -> LevelFilter {
        let mut best: Option<(usize, u8)> = None;
        for slot in &self.slots {
            let Some(p) = slot.prefix.get() else { break };
            let lvl = slot.level.load(Ordering::Acquire);
            if lvl == LEVEL_UNSET || !prefix_matches(target, p) {
                continue;
            }
            if best.is_none_or(|(len, _)| p.len() > len) {
                best = Some((p.len(), lvl));
            }
        }
        u8_to_filter(best.map_or_else(|| self.fallback.load(Ordering::Acquire), |(_, l)| l))
    }

    fn enabled(&self, target: &str, level: Level) -> bool {
        level <= self.level_for(target)
    }

    fn max(&self) -> LevelFilter {
        let mut m = self.fallback.load(Ordering::Acquire);
        for slot in &self.slots {
            if slot.prefix.get().is_none() {
                break;
            }
            let l = slot.level.load(Ordering::Acquire);
            if l != LEVEL_UNSET {
                m = m.max(l);
            }
        }
        u8_to_filter(m)
    }

    fn install_defaults(&self) {
        for p in OUR_PREFIXES {
            if !self
                .slots
                .iter()
                .any(|s| s.prefix.get().is_some_and(|x| &**x == p))
            {
                let _ = self.set(p, LevelFilter::Info);
            }
        }
    }
}

// ── token bucket (50 records/s, burst 200; atomics only) ─────────────────────────────────────────────

const BUCKET_BURST: u64 = 200;
const BUCKET_MS_PER_TOKEN: u64 = 20;
const TOKEN_BITS: u32 = 20;
const TOKEN_MASK: u64 = (1 << TOKEN_BITS) - 1;

struct TokenBucket {
    /// `(last_refill_ms << TOKEN_BITS) | tokens`
    state: AtomicU64,
}

impl TokenBucket {
    const fn new() -> Self {
        TokenBucket {
            state: AtomicU64::new(BUCKET_BURST),
        }
    }

    fn take(&self, now_ms: u64) -> bool {
        let mut cur = self.state.load(Ordering::Acquire);
        loop {
            let (mut last, mut tokens) = (cur >> TOKEN_BITS, cur & TOKEN_MASK);
            let add = now_ms.saturating_sub(last) / BUCKET_MS_PER_TOKEN;
            if add > 0 {
                tokens = tokens.saturating_add(add);
                if tokens >= BUCKET_BURST {
                    tokens = BUCKET_BURST;
                    last = now_ms;
                } else {
                    last = last.saturating_add(add * BUCKET_MS_PER_TOKEN);
                }
            }
            if tokens == 0 {
                return false;
            }
            let new = (last << TOKEN_BITS) | (tokens - 1);
            match self
                .state
                .compare_exchange_weak(cur, new, Ordering::AcqRel, Ordering::Acquire)
            {
                Ok(_) => return true,
                Err(actual) => cur = actual,
            }
        }
    }
}

// ── the ring ─────────────────────────────────────────────────────────────────────────────────────────

/// Writes at most `cap` bytes and then stops formatting (so huge messages are never fully built).
struct Bounded {
    buf: String,
    cap: usize,
    truncated: bool,
}

impl fmt::Write for Bounded {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        let room = self.cap.saturating_sub(self.buf.len());
        if s.len() <= room {
            self.buf.push_str(s);
            return Ok(());
        }
        let mut cut = room;
        while cut > 0 && !s.is_char_boundary(cut) {
            cut -= 1;
        }
        self.buf.push_str(s.get(..cut).unwrap_or(""));
        self.truncated = true;
        Err(fmt::Error)
    }
}

fn format_message(args: &fmt::Arguments<'_>) -> Box<str> {
    const ELLIPSIS: &str = "…";
    let mut w = Bounded {
        buf: String::new(),
        cap: LOG_MSG_MAX,
        truncated: false,
    };
    let _ = w.write_fmt(*args);
    if w.truncated {
        let mut cut = LOG_MSG_MAX - ELLIPSIS.len();
        while cut > 0 && !w.buf.is_char_boundary(cut) {
            cut -= 1;
        }
        w.buf.truncate(cut);
        w.buf.push_str(ELLIPSIS);
    }
    w.buf.into_boxed_str()
}

/// The logger state. One global instance backs [`install`]; tests build private ones.
pub(crate) struct LogCore {
    ring: Mutex<VecDeque<LogRecord>>, // L9
    waiter: Mutex<Option<Thread>>,    // L11
    dropped: AtomicU64,
    bucket: TokenBucket,
    rate_limited: bool,
    levels: LevelTable,
    stopping: AtomicBool,
    /// The global instance also honours `runtime::SHUTTING_DOWN`.
    global: bool,
    base: OnceLock<Instant>,
}

impl LogCore {
    const fn new_global() -> Self {
        LogCore {
            ring: Mutex::new(VecDeque::new()),
            waiter: Mutex::new(None),
            dropped: AtomicU64::new(0),
            bucket: TokenBucket::new(),
            rate_limited: true,
            levels: LevelTable::new(),
            stopping: AtomicBool::new(false),
            global: true,
            base: OnceLock::new(),
        }
    }

    #[cfg(test)]
    fn new_private(rate_limited: bool) -> Self {
        let core = LogCore {
            global: false,
            rate_limited,
            ..LogCore::new_global()
        };
        core.levels.install_defaults();
        core
    }

    fn stopped(&self) -> bool {
        self.stopping.load(Ordering::Acquire)
            || (self.global && runtime::SHUTTING_DOWN.load(Ordering::Acquire))
    }

    fn now_ms(&self) -> u64 {
        let base = *self.base.get_or_init(Instant::now);
        u64::try_from(base.elapsed().as_millis()).unwrap_or(u64::MAX >> TOKEN_BITS)
    }

    fn log(&self, record: &::log::Record<'_>) {
        if self.stopped() {
            return;
        }
        // The mixer never logs (G4); a stray record there is dropped before any formatting.
        if runtime::role() == Role::Mixer {
            return;
        }
        let target = record.target();
        if target.starts_with("tracing::span") || !self.levels.enabled(target, record.level()) {
            return;
        }
        if self.rate_limited && !self.bucket.take(self.now_ms()) {
            self.dropped.fetch_add(1, Ordering::Relaxed);
            return;
        }
        let rec = LogRecord {
            level: record.level(),
            target: target.into(),
            message: format_message(record.args()),
            file: record.file().map(Into::into),
            line: record.line(),
            unix_ts: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0.0, |d| d.as_secs_f64()),
        };
        let (rejected, was_empty) = match self.ring.try_lock() {
            Some(mut ring) if ring.len() < LOG_RING_CAP => {
                let was_empty = ring.is_empty();
                ring.push_back(rec);
                (None, was_empty)
            }
            _ => (Some(rec), false),
        };
        if rejected.is_some() {
            self.dropped.fetch_add(1, Ordering::Relaxed);
            drop(rejected); // freed outside the ring lock
            return;
        }
        if was_empty {
            let waiter = self.waiter.try_lock().and_then(|w| w.clone());
            if let Some(t) = waiter {
                t.unpark();
            }
        }
    }

    fn wait(&self, timeout: Duration) {
        *self.waiter.lock() = Some(thread::current());
        if self.stopped() || !self.ring.lock().is_empty() {
            return;
        }
        thread::park_timeout(timeout);
    }

    fn drain(&self, max: usize, out: &mut Vec<LogRecord>) {
        let mut ring = self.ring.lock();
        for _ in 0..max {
            match ring.pop_front() {
                Some(r) => out.push(r),
                None => break,
            }
        }
    }

    fn take_dropped(&self) -> u64 {
        self.dropped.swap(0, Ordering::AcqRel)
    }

    fn shutdown(&self) {
        self.stopping.store(true, Ordering::Release);
        let waiter = self.waiter.lock().clone();
        if let Some(t) = waiter {
            t.unpark();
        }
    }
}

static CORE: LogCore = LogCore::new_global();
static DEFAULTS: std::sync::Once = std::sync::Once::new();

fn ensure_defaults() {
    DEFAULTS.call_once(|| CORE.levels.install_defaults());
}

struct RingLogger;
static LOGGER: RingLogger = RingLogger;

impl ::log::Log for RingLogger {
    fn enabled(&self, metadata: &::log::Metadata<'_>) -> bool {
        CORE.levels.enabled(metadata.target(), metadata.level())
    }

    fn log(&self, record: &::log::Record<'_>) {
        CORE.log(record);
    }

    fn flush(&self) {}
}

/// Installs the ring logger as the `log` facade's logger. `false` if another logger is installed.
pub fn install() -> bool {
    ensure_defaults();
    match ::log::set_logger(&LOGGER) {
        Ok(()) => {
            ::log::set_max_level(CORE.levels.max());
            true
        }
        Err(_) => false,
    }
}

/// Sets the level for a Rust target prefix (module-path aware: `"songbird"` matches
/// `"songbird::driver"` but not `"songbirds"`; the longest prefix wins; `""` = every other target).
/// Defaults: ours INFO, everything else (songbird, rubato, …) WARN. `false` if the table is full.
pub fn set_level(target_prefix: &str, level: LevelFilter) -> bool {
    ensure_defaults();
    let ok = CORE.levels.set(target_prefix, level);
    ::log::set_max_level(CORE.levels.max());
    ok
}

/// The effective level for `target`.
pub fn level_for(target: &str) -> LevelFilter {
    ensure_defaults();
    CORE.levels.level_for(target)
}

/// Registers the caller in `LOG_WAITER` and parks for up to `timeout` (returns at once if records
/// are queued or logging is shut down). Call it detached from the interpreter.
pub fn wait(timeout: Duration) {
    CORE.wait(timeout);
}

/// Moves up to `max` queued records into `out`.
pub fn drain(max: usize, out: &mut Vec<LogRecord>) {
    CORE.drain(max, out);
}

/// Records dropped (rate limit, contention, full ring) since the last call.
pub fn take_dropped() -> u64 {
    CORE.take_dropped()
}

/// Stops logging (`SHUTTING_DOWN`) and wakes the waiter; queued records stay drainable.
pub fn shutdown() {
    runtime::SHUTTING_DOWN.store(true, Ordering::Release);
    CORE.shutdown();
}

#[cfg(test)]
mod tests {
    use super::*;
    use ::log::Record;
    use std::sync::Arc;

    fn emit(core: &LogCore, level: Level, target: &str, msg: &str) {
        core.log(
            &Record::builder()
                .args(format_args!("{msg}"))
                .level(level)
                .target(target)
                .build(),
        );
    }

    fn count(core: &LogCore) -> usize {
        core.ring.lock().len()
    }

    #[test]
    fn default_levels_and_prefix_rules() {
        let core = LogCore::new_private(false);
        let l = &core.levels;
        assert!(l.enabled("sbcore::wake", Level::Info));
        assert!(!l.enabled("sbcore::wake", Level::Debug));
        assert!(l.enabled("discord_ext_songbird_native::py", Level::Info));
        assert!(l.enabled("songbird::driver", Level::Warn));
        assert!(!l.enabled("songbird::driver", Level::Info));
        assert!(!l.enabled("rubato", Level::Info));
        assert!(
            !l.enabled("sbcorex", Level::Info),
            "prefix match is module-aware"
        );
        assert!(l.set("songbird", LevelFilter::Debug));
        assert!(l.set("songbird::driver::tasks", LevelFilter::Error));
        assert!(l.enabled("songbird::input", Level::Debug));
        assert!(
            !l.enabled("songbird::driver::tasks::mixer", Level::Warn),
            "longest prefix wins"
        );
        assert!(l.enabled("songbird::driver::tasks::mixer", Level::Error));
        assert_eq!(l.max(), LevelFilter::Debug);
        assert!(l.set("", LevelFilter::Off));
        assert!(!l.enabled("tokio", Level::Error));
        assert!(
            l.set("songbird", LevelFilter::Info),
            "re-setting an existing prefix reuses its slot"
        );
        assert!(!l.enabled("songbird::input", Level::Debug));
    }

    #[test]
    fn level_table_full() {
        let core = LogCore::new_private(false);
        let mut ok = 0;
        for i in 0..40 {
            if core.levels.set(&format!("crate{i}"), LevelFilter::Info) {
                ok += 1;
            }
        }
        assert_eq!(ok, MAX_PREFIXES - OUR_PREFIXES.len());
    }

    #[test]
    fn records_are_filtered_formatted_and_drained() {
        let core = LogCore::new_private(false);
        emit(&core, Level::Info, "sbcore::x", "hello");
        emit(&core, Level::Debug, "sbcore::x", "filtered");
        emit(&core, Level::Warn, "tracing::span", "span");
        emit(&core, Level::Warn, "songbird::driver", "warned");
        let mut out = Vec::new();
        core.drain(1, &mut out);
        core.drain(10, &mut out);
        assert_eq!(out.len(), 2);
        assert_eq!(&*out[0].message, "hello");
        assert_eq!(&*out[0].target, "sbcore::x");
        assert_eq!(out[0].level, Level::Info);
        assert!(out[0].unix_ts > 1.0e9);
        assert_eq!(&*out[1].message, "warned");
        assert_eq!(core.take_dropped(), 0);
    }

    #[test]
    fn long_messages_are_truncated_at_char_boundary() {
        let core = LogCore::new_private(false);
        let long = "é".repeat(1000);
        emit(&core, Level::Error, "sbcore", &long);
        let mut out = Vec::new();
        core.drain(1, &mut out);
        let m = &out[0].message;
        assert!(m.len() <= LOG_MSG_MAX, "{}", m.len());
        assert!(m.ends_with('…'));
        assert!(m.starts_with("éé"));
    }

    #[test]
    fn full_ring_drops_and_counts() {
        let core = LogCore::new_private(false);
        for i in 0..LOG_RING_CAP + 76 {
            emit(&core, Level::Error, "sbcore", &format!("{i}"));
        }
        assert_eq!(count(&core), LOG_RING_CAP);
        assert_eq!(core.take_dropped(), 76);
        assert_eq!(core.take_dropped(), 0);
    }

    #[test]
    fn token_bucket_limits_bursts() {
        let core = LogCore::new_private(true);
        for _ in 0..300 {
            emit(&core, Level::Error, "sbcore", "burst");
        }
        let kept = count(&core);
        assert!((200..=215).contains(&kept), "kept {kept}");
        assert_eq!(core.take_dropped() as usize, 300 - kept);
        let b = TokenBucket::new();
        for _ in 0..200 {
            assert!(b.take(0));
        }
        assert!(!b.take(0));
        assert!(b.take(20), "one token per 20 ms");
        assert!(!b.take(20));
        assert!(b.take(100_000));
    }

    #[test]
    fn producer_try_lock_never_blocks() {
        let core = Arc::new(LogCore::new_private(false));
        let guard = core.ring.lock();
        let handles: Vec<_> = (0..8)
            .map(|t| {
                let core = core.clone();
                std::thread::spawn(move || {
                    let mut times = Vec::with_capacity(100);
                    for i in 0..100 {
                        let start = Instant::now();
                        emit(&core, Level::Error, "sbcore", &format!("t{t} {i}"));
                        times.push(start.elapsed());
                    }
                    times
                })
            })
            .collect();
        let mut all: Vec<Duration> = Vec::new();
        for h in handles {
            all.extend(h.join().unwrap());
        }
        // Keep the lock held well past the producers: a blocking lock() would have waited for it.
        drop(guard);
        all.sort_unstable();
        let median = all[all.len() / 2];
        let max = *all.last().unwrap();
        assert!(median < Duration::from_micros(50), "median {median:?}");
        assert!(
            max < Duration::from_millis(50),
            "max {max:?}: a producer blocked"
        );
        assert_eq!(core.take_dropped(), 800);
        assert_eq!(count(&core), 0);
    }

    #[test]
    fn wait_returns_on_push() {
        let core = Arc::new(LogCore::new_private(false));
        let c2 = core.clone();
        let waiter = std::thread::spawn(move || {
            let start = Instant::now();
            c2.wait(Duration::from_secs(10));
            start.elapsed()
        });
        // Wait until the waiter registered (or give up and log anyway).
        for _ in 0..200 {
            if core.waiter.lock().is_some() {
                break;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        std::thread::sleep(Duration::from_millis(20));
        emit(&core, Level::Error, "sbcore", "wake up");
        let took = waiter.join().unwrap();
        assert!(took < Duration::from_secs(5), "{took:?}");
        // Records queued: wait returns immediately.
        let start = Instant::now();
        core.wait(Duration::from_secs(10));
        assert!(start.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn wait_returns_on_shutdown_and_shutdown_short_circuits() {
        let core = Arc::new(LogCore::new_private(false));
        let c2 = core.clone();
        let waiter = std::thread::spawn(move || {
            let start = Instant::now();
            c2.wait(Duration::from_secs(10));
            start.elapsed()
        });
        for _ in 0..200 {
            if core.waiter.lock().is_some() {
                break;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        std::thread::sleep(Duration::from_millis(20));
        core.shutdown();
        assert!(waiter.join().unwrap() < Duration::from_secs(5));
        emit(&core, Level::Error, "sbcore", "after shutdown");
        assert_eq!(count(&core), 0);
        assert_eq!(core.take_dropped(), 0, "short-circuited, not dropped");
        let start = Instant::now();
        core.wait(Duration::from_secs(10));
        assert!(start.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn wait_times_out() {
        let core = LogCore::new_private(false);
        let start = Instant::now();
        core.wait(Duration::from_millis(30));
        assert!(start.elapsed() >= Duration::from_millis(20));
    }

    #[test]
    fn mixer_threads_never_log() {
        let core = Arc::new(LogCore::new_private(false));
        let c2 = core.clone();
        std::thread::spawn(move || {
            runtime::set_role(Role::Mixer);
            emit(&c2, Level::Error, "sbcore", "from the mixer");
        })
        .join()
        .unwrap();
        assert_eq!(count(&core), 0);
    }

    #[test]
    fn global_install_and_drain() {
        assert!(install());
        assert!(!install(), "a second install reports the existing logger");
        assert!(set_level("sbcore::log::tests", LevelFilter::Debug));
        assert_eq!(level_for("sbcore::log::tests::x"), LevelFilter::Debug);
        assert!(::log::max_level() >= LevelFilter::Debug);
        ::log::debug!(target: "sbcore::log::tests", "global-marker-{}", 42);
        let mut out = Vec::new();
        let mut found = false;
        for _ in 0..50 {
            drain(256, &mut out);
            if out.iter().any(|r| &*r.message == "global-marker-42") {
                found = true;
                break;
            }
            wait(Duration::from_millis(10));
        }
        assert!(found);
        let _ = take_dropped();
    }
}
