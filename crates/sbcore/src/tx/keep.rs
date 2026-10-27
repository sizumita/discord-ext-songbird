//! Deferred release of in-memory sources (§5.2, §7.9): a mixer thread is never the last owner of an
//! `AudioFile` / `RawPcm` / `OpusPackets` buffer.
//!
//! songbird drops a track's input inline when the whole Driver is stopped or poisoned
//! (`SetTrack(None)` → `tracks.clear()`, T3 and `stop_all`), on the live mixer thread or the NET
//! Idle task; only natural ends and per-handle stops go through its DisposalThread. A memory buffer
//! may be up to `MAX_SOURCE_BYTES`, and its final free (an `munmap` on glibc) takes milliseconds,
//! enough to overrun the 20 ms tick of every call on that mixer.
//!
//! So each memory input reaches its buffer through a **per-track** `Arc` (the `MemPcmCursor`, a
//! per-track `ArcBytes` or `OpusFrames`), and [`hold`] keeps a second strong reference to it. The
//! input's drop, wherever it runs, then only decrements. A reaper task on the RX runtime polls the
//! held references and releases the ones nobody else holds any more (the input is gone; the
//! per-track `Arc` is private, so no one can clone it again) on a blocking RX thread, where the
//! buffer is freed unless Python still holds the source.
//!
//! `HELD` is a registry lock (L10, leaf), taken by threads starting a track and by the reaper,
//! never by a mixer thread. The reaper runs only while something is held.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use parking_lot::Mutex;

use crate::runtime;
use crate::sync::lock_checked;

/// A per-track reference shared with a songbird input.
pub(crate) type Held = Arc<dyn Send + Sync>;

/// L10 (leaf).
static HELD: Mutex<Vec<Held>> = Mutex::new(Vec::new());
/// Whether a reaper task is running (set and cleared under `HELD`).
static REAPER: AtomicBool = AtomicBool::new(false);
/// How often the reaper polls (release latency only; correctness never depends on it).
const REAP_EVERY: Duration = Duration::from_millis(50);

/// Keeps `per_track` alive until the songbird input that shares it is gone, then releases it off
/// the mixer. Called once per started memory input, on the thread building it (never a mixer).
pub(crate) fn hold(per_track: Held) {
    let spawn = {
        let mut g = lock_checked(&HELD); // L10
        g.push(per_track);
        !REAPER.swap(true, Ordering::AcqRel)
    };
    if spawn {
        runtime::rx().spawn(reaper());
    }
}

/// The references whose input is gone, removed from `HELD`; and whether `HELD` is now empty (the
/// reaper then stops, under the lock, so a concurrent [`hold`] starts a new one).
fn take_released() -> (Vec<Held>, bool) {
    let mut g = lock_checked(&HELD); // L10
    let mut released = Vec::new();
    let mut i = 0;
    while i < g.len() {
        // Only `HELD` refers to it: the input dropped its clone, and nobody can clone it again.
        if g.get(i).is_some_and(|h| Arc::strong_count(h) == 1) {
            released.push(g.swap_remove(i));
        } else {
            i += 1;
        }
    }
    let empty = g.is_empty();
    if empty {
        REAPER.store(false, Ordering::Release);
    }
    (released, empty)
}

async fn reaper() {
    loop {
        tokio::time::sleep(REAP_EVERY).await;
        let (released, empty) = take_released();
        if !released.is_empty() {
            // The final free of a large buffer blocks: keep it off the RX workers too.
            drop(tokio::task::spawn_blocking(move || drop(released)));
        }
        if empty {
            return;
        }
    }
}

/// Held references (tests and diagnostics).
#[cfg(test)]
pub(crate) fn held() -> usize {
    HELD.lock().len()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::format::PCM16_48K_STEREO;
    use crate::runtime::{Role, set_role};
    use crate::tx::decoder::tests::guard;
    use crate::tx::memory::tests::wav;
    use crate::tx::memory::{EncodedBytes, MemPcm, OpusFrames};
    use crate::tx::source::SourceSpec;
    use crate::wake::{Doorbell, WakeHub};
    use std::os::unix::net::UnixStream;
    use std::sync::Weak;
    use std::time::Instant;

    fn wait_gone(w: &Weak<dyn Send + Sync>) -> bool {
        let t = Instant::now();
        while t.elapsed() < Duration::from_secs(10) {
            if w.upgrade().is_none() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        false
    }

    /// An input dropped on a mixer thread (as `SetTrack(None)` does) never frees the shared
    /// buffer there, even when the Python object is already gone: the reaper frees it later.
    #[test]
    fn a_memory_input_dropped_on_the_mixer_never_frees_its_buffer_there() {
        set_role(Role::Harness);
        let (a, _b) = UnixStream::pair().unwrap();
        let hub = WakeHub::new(Doorbell::from_unix_stream(a).unwrap());
        let samples: Vec<i16> = (0..48_000 * 2).map(|i| (i % 1000) as i16).collect();
        let pcm = Arc::new(MemPcm::from_i16(PCM16_48K_STEREO, samples.clone()).unwrap());
        let file = Arc::new(EncodedBytes::new(wav(48_000, 2, &samples)).unwrap());
        let opus = Arc::new(OpusFrames::from_packets([&[0xF8u8, 0xFF, 0xFE][..]; 50]).unwrap());
        // The big buffers themselves (the Python-facing objects are freed with their last
        // Python reference, whoever holds the buffers).
        let weak: Vec<Weak<dyn Send + Sync>> = vec![
            Arc::downgrade(&(pcm.clone() as Arc<dyn Send + Sync>)),
            file.buffer_weak(),
            opus.buffer_weak(),
        ];
        let specs = [
            SourceSpec::Pcm(pcm),
            SourceSpec::File(file),
            SourceSpec::Opus(opus),
        ];
        let inputs: Vec<_> = specs
            .iter()
            .map(|s| s.to_input(guard().0, &hub).unwrap().0)
            .collect();
        // Python lets go of the sources: the inputs (inside songbird) are their only users.
        drop(specs);
        assert!(held() >= 3);
        let probe = weak.clone();
        let survived = std::thread::spawn(move || {
            set_role(Role::Mixer);
            drop(inputs);
            probe
                .iter()
                .map(|w| w.upgrade().is_some())
                .collect::<Vec<_>>()
        })
        .join()
        .unwrap();
        assert_eq!(survived, vec![true; 3], "a buffer was freed on the mixer");
        for w in &weak {
            assert!(wait_gone(w), "the reaper never released a buffer");
        }
    }
}
