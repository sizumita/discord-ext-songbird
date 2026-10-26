//! The wake protocol: Rust → Python with zero Python references (§5.4).
//!
//! Rust signals Python by writing one byte on a socket (the [`Doorbell`]); Python awaits
//! `loop.sock_recv(rsock, 256)` and then calls [`WakeHub::take_ready`] to learn which ids are ready.
//! The four hub operations on `bits`/`pending` use `SeqCst` (store-buffer pattern, §5.4);
//! `scripts/check_invariants.py` rejects any other ordering on them.

use std::io;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Weak};

use crate::limits::{LimitError, LimitNum, MAX_WAKE_IDS};
use crate::runtime;

/// A wake id: 0..=3 are reserved, 4..=255 are allocated per object.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct WakeId(pub u8);

/// Connection status changed.
pub const WAKE_CONN: WakeId = WakeId(0);
/// Op results are ready.
pub const WAKE_OPS: WakeId = WakeId(1);
/// Control slots changed.
pub const WAKE_CTRL: WakeId = WakeId(2);
/// Level-triggered track completion (§6.4, §7.10).
pub const WAKE_TRACKS: WakeId = WakeId(3);

const WORDS: usize = MAX_WAKE_IDS / 64;
const RESERVED_MASK: u64 = 0b1111;

// ── Doorbell ─────────────────────────────────────────────────────────────────────────────────────────

/// The write end of the wake socket. Owns the descriptor and closes it on drop.
#[derive(Debug)]
pub struct Doorbell {
    #[cfg(unix)]
    fd: std::os::fd::OwnedFd,
    #[cfg(windows)]
    sock: std::net::TcpStream,
}

#[cfg(all(unix, target_vendor = "apple"))]
const SEND_FLAGS: libc::c_int = libc::MSG_DONTWAIT; // SIGPIPE suppressed by SO_NOSIGPIPE
#[cfg(all(unix, not(target_vendor = "apple")))]
const SEND_FLAGS: libc::c_int = libc::MSG_DONTWAIT | libc::MSG_NOSIGNAL;

#[cfg(unix)]
impl Doorbell {
    /// Takes ownership of `raw` (a socket fd); non-blocking; `SO_NOSIGPIPE` on Apple. Closes on drop.
    ///
    /// An out-of-range or closed descriptor is rejected without being touched. Once the descriptor
    /// has been validated it is owned, so any later configuration error closes it (exactly once).
    ///
    /// # Safety
    /// `raw` must be an open descriptor whose ownership the caller transfers (e.g. `socket.detach()`);
    /// nothing else may close or use it afterwards.
    pub unsafe fn adopt(raw: i64) -> io::Result<Self> {
        use std::os::fd::{FromRawFd, OwnedFd, RawFd};
        let fd = RawFd::try_from(raw)
            .ok()
            .filter(|fd| *fd >= 0)
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidInput, "file descriptor out of range")
            })?;
        // SAFETY: F_GETFL only queries the descriptor table entry.
        if unsafe { libc::fcntl(fd, libc::F_GETFL) } < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: the caller transfers ownership of this open descriptor.
        let owned = unsafe { OwnedFd::from_raw_fd(fd) };
        let bell = Doorbell { fd: owned };
        bell.configure()?;
        Ok(bell)
    }

    /// Adopts a connected Unix stream socket (tests and harnesses).
    pub fn from_unix_stream(s: std::os::unix::net::UnixStream) -> io::Result<Self> {
        let bell = Doorbell { fd: s.into() };
        bell.configure()?;
        Ok(bell)
    }

    fn raw(&self) -> std::os::fd::RawFd {
        use std::os::fd::AsRawFd;
        self.fd.as_raw_fd()
    }

    fn configure(&self) -> io::Result<()> {
        let fd = self.raw();
        let mut ty: libc::c_int = 0;
        let mut len = libc::socklen_t::try_from(std::mem::size_of::<libc::c_int>()).unwrap_or(4);
        // SAFETY: valid fd, out-pointers to a c_int and its length.
        let r = unsafe {
            libc::getsockopt(
                fd,
                libc::SOL_SOCKET,
                libc::SO_TYPE,
                (&raw mut ty).cast::<libc::c_void>(),
                &raw mut len,
            )
        };
        if r < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: plain fcntl calls on a valid fd.
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
        if flags < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: as above.
        if unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
            return Err(io::Error::last_os_error());
        }
        #[cfg(target_vendor = "apple")]
        {
            let one: libc::c_int = 1;
            // SAFETY: valid fd, pointer to a c_int option value.
            let r = unsafe {
                libc::setsockopt(
                    fd,
                    libc::SOL_SOCKET,
                    libc::SO_NOSIGPIPE,
                    (&raw const one).cast::<libc::c_void>(),
                    libc::socklen_t::try_from(std::mem::size_of::<libc::c_int>()).unwrap_or(4),
                )
            };
            if r < 0 {
                return Err(io::Error::last_os_error());
            }
        }
        Ok(())
    }

    /// Writes one byte without blocking. `EAGAIN` (a byte is already pending) and `EPIPE` (Python is
    /// gone) are ignored, and never raise `SIGPIPE`.
    pub fn ring(&self) {
        let byte = 1u8;
        for _ in 0..3 {
            // SAFETY: valid fd; one readable byte at `byte`.
            let r = unsafe {
                libc::send(
                    self.raw(),
                    (&raw const byte).cast::<libc::c_void>(),
                    1,
                    SEND_FLAGS,
                )
            };
            if r >= 0 || io::Error::last_os_error().kind() != io::ErrorKind::Interrupted {
                return;
            }
        }
    }

    /// `shutdown(SHUT_WR)`: the reader sees the pending bytes and then EOF.
    pub fn shutdown(&self) {
        // SAFETY: valid fd; errors (e.g. ENOTCONN) are irrelevant here.
        unsafe {
            libc::shutdown(self.raw(), libc::SHUT_WR);
        }
    }
}

#[cfg(windows)]
impl Doorbell {
    /// Takes ownership of `raw` (a SOCKET); non-blocking. Closes on drop.
    ///
    /// # Safety
    /// `raw` must be an open socket whose ownership the caller transfers (e.g. `socket.detach()`).
    pub unsafe fn adopt(raw: i64) -> io::Result<Self> {
        use std::os::windows::io::{FromRawSocket, OwnedSocket, RawSocket};
        let s = RawSocket::try_from(raw)
            .ok()
            .filter(|s| *s != RawSocket::MAX)
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidInput, "socket handle out of range")
            })?;
        // SAFETY: the caller transfers ownership of this open socket.
        let owned = unsafe { OwnedSocket::from_raw_socket(s) };
        let sock = std::net::TcpStream::from(owned);
        sock.set_nonblocking(true)?;
        Ok(Doorbell { sock })
    }

    /// Writes one byte without blocking; errors are ignored.
    pub fn ring(&self) {
        use std::io::Write;
        let _ = (&self.sock).write(&[1u8]);
    }

    /// Shuts down the write half: the reader sees the pending bytes and then EOF.
    pub fn shutdown(&self) {
        let _ = self.sock.shutdown(std::net::Shutdown::Write);
    }
}

// ── WakeHub ──────────────────────────────────────────────────────────────────────────────────────────

/// Per-connection wake state: 256 level-triggered ready bits, an id allocator and the doorbell.
#[derive(Debug)]
pub struct WakeHub {
    bits: [AtomicU64; WORDS],
    used: [AtomicU64; WORDS],
    pending: AtomicBool,
    closed: AtomicBool,
    bell: Doorbell,
    /// The creating process; teardown is skipped in any other process (§4.10).
    pid: u32,
}

impl WakeHub {
    /// Ids 0..=3 are pre-reserved; records `getpid()`.
    pub fn new(bell: Doorbell) -> Arc<Self> {
        Self::with_pid(bell, std::process::id())
    }

    pub(crate) fn with_pid(bell: Doorbell, pid: u32) -> Arc<Self> {
        Arc::new(WakeHub {
            bits: [const { AtomicU64::new(0) }; WORDS],
            used: [
                AtomicU64::new(RESERVED_MASK),
                AtomicU64::new(0),
                AtomicU64::new(0),
                AtomicU64::new(0),
            ],
            pending: AtomicBool::new(false),
            closed: AtomicBool::new(false),
            bell,
            pid,
        })
    }

    /// Allocates a free id in 4..=255. Freed again when the token drops (any thread).
    pub fn alloc(self: &Arc<Self>) -> Result<WakeToken, LimitError> {
        for (w, used_word) in self.used.iter().enumerate() {
            let mut cur = used_word.load(Ordering::Acquire);
            while cur != u64::MAX {
                let bit = (!cur).trailing_zeros();
                match used_word.compare_exchange_weak(
                    cur,
                    cur | (1u64 << bit),
                    Ordering::AcqRel,
                    Ordering::Acquire,
                ) {
                    Ok(_) => {
                        let id = u8::try_from(w * 64 + bit as usize).unwrap_or(u8::MAX);
                        return Ok(WakeToken {
                            hub: Arc::downgrade(self),
                            id: WakeId(id),
                        });
                    }
                    Err(actual) => cur = actual,
                }
            }
        }
        Err(LimitError {
            what: "wake_ids",
            value: LimitNum::Int(MAX_WAKE_IDS as i128 + 1),
            min: LimitNum::Int(0),
            max: LimitNum::Int(MAX_WAKE_IDS as i128),
        })
    }

    /// Marks `id` ready and rings at most once per burst. Wait-free apart from at most one
    /// non-blocking `send`. Never called from mixer threads.
    pub fn notify(&self, id: WakeId) {
        debug_assert_ne!(
            runtime::role(),
            runtime::Role::Mixer,
            "WakeHub::notify on a mixer thread"
        );
        let (w, b) = (usize::from(id.0) / 64, u32::from(id.0) % 64);
        if let Some(ready_word) = self.bits.get(w) {
            ready_word.fetch_or(1u64 << b, Ordering::SeqCst);
        }
        if !self.pending.swap(true, Ordering::SeqCst) && !self.closed.load(Ordering::Acquire) {
            self.bell.ring();
        }
    }

    /// Collects every ready id into `out` (Python only, after `sock_recv` returned).
    pub fn take_ready(&self, out: &mut Vec<u8>) {
        self.pending.store(false, Ordering::SeqCst);
        for (w, ready_word) in self.bits.iter().enumerate() {
            let mut v = ready_word.swap(0, Ordering::SeqCst);
            while v != 0 {
                let b = v.trailing_zeros() as usize;
                v &= v - 1;
                out.push(u8::try_from(w * 64 + b).unwrap_or(u8::MAX));
            }
        }
    }

    /// Shuts the doorbell down (Python reads the pending bytes, then EOF). Idempotent. In a forked
    /// child, or any process other than the creator, it never calls `shutdown()` (§4.10).
    pub fn close(&self) {
        if self.closed.swap(true, Ordering::AcqRel) {
            return;
        }
        if runtime::in_foreign_process(self.pid) {
            return;
        }
        self.bell.shutdown();
    }

    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Acquire)
    }

    /// The pid of the process that created this hub.
    pub fn creator_pid(&self) -> u32 {
        self.pid
    }

    fn free(&self, id: WakeId) {
        let (w, b) = (usize::from(id.0) / 64, u32::from(id.0) % 64);
        if usize::from(id.0) < 4 {
            return;
        }
        // Clear the ready bit first, so a re-allocation can never lose a fresh notification.
        if let Some(ready_word) = self.bits.get(w) {
            ready_word.fetch_and(!(1u64 << b), Ordering::SeqCst);
        }
        if let Some(used_word) = self.used.get(w) {
            used_word.fetch_and(!(1u64 << b), Ordering::AcqRel);
        }
    }
}

/// An allocated wake id. Dropping it frees the id (atomics only; any thread).
#[derive(Debug)]
pub struct WakeToken {
    hub: Weak<WakeHub>,
    id: WakeId,
}

impl WakeToken {
    pub fn id(&self) -> WakeId {
        self.id
    }

    /// Notifies this id if the hub is still alive.
    pub fn notify(&self) {
        if let Some(hub) = self.hub.upgrade() {
            hub.notify(self.id);
        }
    }

    /// The hub this token belongs to, if still alive.
    pub fn hub(&self) -> Option<Arc<WakeHub>> {
        self.hub.upgrade()
    }
}

impl Drop for WakeToken {
    fn drop(&mut self) {
        if let Some(hub) = self.hub.upgrade() {
            hub.free(self.id);
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::io::{ErrorKind, Read};
    use std::os::unix::net::UnixStream;
    use std::sync::atomic::AtomicUsize;
    use std::time::Duration;

    fn pair() -> (Arc<WakeHub>, UnixStream) {
        let (r, w) = UnixStream::pair().unwrap();
        (WakeHub::new(Doorbell::from_unix_stream(w).unwrap()), r)
    }

    fn read_available(r: &mut UnixStream) -> Option<usize> {
        r.set_nonblocking(true).unwrap();
        let mut buf = [0u8; 256];
        match r.read(&mut buf) {
            Ok(n) => Some(n),
            Err(e) if e.kind() == ErrorKind::WouldBlock => None,
            Err(e) => panic!("{e}"),
        }
    }

    #[test]
    fn reserved_ids_and_level_semantics() {
        let (hub, mut r) = pair();
        hub.notify(WAKE_CTRL);
        hub.notify(WAKE_CONN);
        hub.notify(WAKE_CTRL);
        assert_eq!(read_available(&mut r), Some(1), "one byte per burst");
        let mut out = Vec::new();
        hub.take_ready(&mut out);
        out.sort_unstable();
        assert_eq!(out, vec![0, 2]);
        out.clear();
        hub.take_ready(&mut out);
        assert!(out.is_empty());
        assert_eq!(read_available(&mut r), None);
    }

    #[test]
    fn alloc_free_256_ids() {
        let (hub, _r) = pair();
        let mut toks: Vec<WakeToken> = (4..256).map(|_| hub.alloc().unwrap()).collect();
        let mut ids: Vec<u8> = toks.iter().map(|t| t.id().0).collect();
        ids.sort_unstable();
        assert_eq!(ids, (4..=255).collect::<Vec<u8>>());
        let e = hub.alloc().unwrap_err();
        assert_eq!(e.what, "wake_ids");
        assert_eq!(e.max, LimitNum::Int(256));
        // Free one on another thread; it becomes allocatable again with a clean ready bit.
        let t = toks.remove(100);
        let freed = t.id();
        t.notify();
        std::thread::spawn(move || drop(t)).join().unwrap();
        let again = hub.alloc().unwrap();
        assert_eq!(again.id(), freed);
        let mut out = Vec::new();
        hub.take_ready(&mut out);
        assert!(
            !out.contains(&freed.0),
            "a freed id's stale ready bit is cleared"
        );
        drop(toks);
        let _a = hub.alloc().unwrap();
    }

    #[test]
    fn token_notify_after_hub_dropped_is_noop() {
        let (hub, _r) = pair();
        let t = hub.alloc().unwrap();
        drop(hub);
        t.notify();
        assert!(t.hub().is_none());
    }

    #[test]
    fn doorbell_eof_on_close() {
        let (hub, mut r) = pair();
        r.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        hub.notify(WAKE_CONN);
        hub.close();
        assert!(hub.is_closed());
        let mut buf = Vec::new();
        r.read_to_end(&mut buf).unwrap();
        assert_eq!(buf, vec![1u8]);
        hub.close(); // idempotent
    }

    #[test]
    fn ring_after_close_is_noop() {
        let (hub, mut r) = pair();
        r.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        hub.close();
        for _ in 0..10 {
            hub.notify(WAKE_OPS);
            let mut out = Vec::new();
            hub.take_ready(&mut out);
            assert_eq!(out, vec![1]);
        }
        let mut buf = Vec::new();
        r.read_to_end(&mut buf).unwrap();
        assert!(buf.is_empty(), "no byte after close");
    }

    #[test]
    fn close_in_forked_child_skips_shutdown() {
        let (r, w) = UnixStream::pair().unwrap();
        let hub = WakeHub::with_pid(
            Doorbell::from_unix_stream(w).unwrap(),
            std::process::id().wrapping_add(1),
        );
        let mut r = r;
        hub.close();
        assert!(hub.is_closed());
        // No shutdown(): the reader sees neither EOF nor data.
        assert_eq!(read_available(&mut r), None);
    }

    #[test]
    fn epipe_does_not_signal() {
        let (hub, r) = pair();
        drop(r);
        // SAFETY: restore the default SIGPIPE disposition so a missing MSG_NOSIGNAL/SO_NOSIGPIPE kills
        // the process; Rust's runtime ignores SIGPIPE by default.
        let prev = unsafe { libc::signal(libc::SIGPIPE, libc::SIG_DFL) };
        for _ in 0..1000 {
            hub.notify(WAKE_CONN);
            let mut out = Vec::new();
            hub.take_ready(&mut out);
        }
        // SAFETY: restore the previous disposition.
        unsafe { libc::signal(libc::SIGPIPE, prev) };
    }

    #[test]
    fn adopt_rejects_bad_descriptors() {
        // SAFETY: negative / out-of-range values are rejected before any ownership is taken.
        assert!(unsafe { Doorbell::adopt(-1) }.is_err());
        assert!(unsafe { Doorbell::adopt(i64::MAX) }.is_err());
        // A descriptor number that is very unlikely to be open.
        assert!(unsafe { Doorbell::adopt(1_000_000) }.is_err());
    }

    #[test]
    fn adopt_takes_ownership_of_socket() {
        use std::os::fd::IntoRawFd;
        let (mut r, w) = UnixStream::pair().unwrap();
        // (macOS rejects SO_RCVTIMEO with EINVAL once the peer is closed, so set it first.)
        r.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let raw = w.into_raw_fd();
        // SAFETY: we own `raw` and hand it over.
        let bell = unsafe { Doorbell::adopt(i64::from(raw)) }.unwrap();
        bell.ring();
        drop(bell); // closes the fd → EOF
        let mut buf = Vec::new();
        r.read_to_end(&mut buf).unwrap();
        assert_eq!(buf, vec![1u8]);
    }

    #[test]
    fn adopt_non_socket_is_rejected_and_closed() {
        use std::os::fd::IntoRawFd;
        let f = std::fs::File::open("/dev/null").unwrap();
        let raw = f.into_raw_fd();
        // SAFETY: we own `raw`; adopt closes it on the configuration error. (Whether it is closed is
        // not re-checked here: parallel tests may reuse the descriptor number at once.)
        assert!(unsafe { Doorbell::adopt(i64::from(raw)) }.is_err());
    }

    /// 4 producers × 10⁶ notifies against one consumer: every producer's last notification is
    /// observed, and bytes written ≤ wake-ups + 1 (§5.4, §13.1).
    #[test]
    fn notify_take_no_lost_wakeup() {
        const PRODUCERS: usize = 4;
        const N: usize = 1_000_000;
        let (r, w) = UnixStream::pair().unwrap();
        let hub = WakeHub::new(Doorbell::from_unix_stream(w).unwrap());
        let tokens: Vec<WakeToken> = (0..PRODUCERS).map(|_| hub.alloc().unwrap()).collect();
        let ids: Vec<u8> = tokens.iter().map(|t| t.id().0).collect();
        let progress: Arc<Vec<AtomicUsize>> =
            Arc::new((0..PRODUCERS).map(|_| AtomicUsize::new(0)).collect());
        let done = Arc::new(AtomicUsize::new(0));

        let consumer = {
            let hub = hub.clone();
            let progress = progress.clone();
            let done = done.clone();
            let ids = ids.clone();
            std::thread::spawn(move || {
                let mut r = r;
                r.set_read_timeout(Some(Duration::from_millis(500)))
                    .unwrap();
                let mut seen = vec![0usize; PRODUCERS];
                let (mut bytes, mut wakeups) = (0usize, 0usize);
                let mut buf = [0u8; 256];
                let mut out = Vec::new();
                loop {
                    match r.read(&mut buf) {
                        Ok(0) => break,
                        Ok(n) => {
                            bytes += n;
                            wakeups += 1;
                            out.clear();
                            hub.take_ready(&mut out);
                            for id in &out {
                                if let Some(p) = ids.iter().position(|x| x == id) {
                                    seen[p] = progress[p].load(Ordering::Acquire);
                                }
                            }
                        }
                        Err(e)
                            if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) =>
                        {
                            // Quiet for 500 ms: stop once every producer has finished. Deliberately no
                            // extra take_ready() here, which would mask a lost wake-up.
                            if done.load(Ordering::Acquire) == PRODUCERS {
                                break;
                            }
                        }
                        Err(e) => panic!("{e}"),
                    }
                }
                (seen, bytes, wakeups)
            })
        };

        let producers: Vec<_> = tokens
            .into_iter()
            .enumerate()
            .map(|(p, tok)| {
                let progress = progress.clone();
                let done = done.clone();
                std::thread::spawn(move || {
                    for k in 1..=N {
                        progress[p].store(k, Ordering::Release);
                        tok.notify();
                    }
                    done.fetch_add(1, Ordering::AcqRel);
                    tok
                })
            })
            .collect();
        let tokens: Vec<WakeToken> = producers.into_iter().map(|h| h.join().unwrap()).collect();
        let (seen, bytes, wakeups) = consumer.join().unwrap();
        for (p, s) in seen.iter().enumerate() {
            assert_eq!(*s, N, "producer {p}'s final notification was lost");
        }
        assert!(
            bytes <= wakeups + 1,
            "bytes {bytes} > wakeups {wakeups} + 1"
        );
        drop(tokens);
    }
}
