//! Binding crate for `discord.ext.songbird.native` (§2.3, §12.6): pyclasses and conversions only.
//!
//! All logic lives in the pyo3-free `sbcore` crate. Pyclass fields hold only `Arc<sbcore::…>`,
//! songbird handles or `Copy` data (I1); no Rust-created thread ever attaches (I2); every native
//! method is synchronous.
#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used))]

pub mod convert;
pub mod errors;
pub mod logging;
pub mod py;

use std::collections::HashMap;
use std::sync::atomic::Ordering;

use pyo3::prelude::*;
use pyo3_stub_gen::define_stub_info_gatherer;
use pyo3_stub_gen::derive::gen_stub_pyfunction;
use sbcore::limits::RUNTIME_WORKERS;
use sbcore::runtime::{self, RuntimeConfig};

use crate::convert::int_arg;
use crate::errors::{CoreResultExt, ensure_not_forked};

/// Codec and container names compiled into this build (for ``AudioFile`` / ``EncodedStream``).
///
/// Returns
/// -------
/// list[str]
///     For example ``'opus'``, ``'mp3'``, ``'format:ogg'``.
#[gen_stub_pyfunction]
#[pyfunction]
pub fn supported_codecs() -> Vec<&'static str> {
    sbcore::tx::source::supported_codecs()
}

/// Size the two internal tokio runtimes.
///
/// Parameters
/// ----------
/// net_workers : int, default 2
///     Workers of the NET runtime (songbird's tasks), ``1..=8``.
/// rx_workers : int, default 1
///     Workers of the RX runtime (receive DSP, housekeeping), ``1..=8``.
///
/// Raises
/// ------
/// RuntimeError
///     If a runtime has already started (call this before the first connection).
/// LimitExceeded
///     If a count is out of range.
#[gen_stub_pyfunction]
#[pyfunction]
#[pyo3(signature = (net_workers = 2, rx_workers = 1))]
pub fn configure_runtime(py: Python<'_>, net_workers: i64, rx_workers: i64) -> PyResult<()> {
    ensure_not_forked(py)?;
    // Checked before narrowing, so a negative count is `LimitExceeded`, not `OverflowError`.
    let net_workers = int_arg(py, "net_workers", net_workers, RUNTIME_WORKERS)?;
    let rx_workers = int_arg(py, "rx_workers", rx_workers, RUNTIME_WORKERS)?;
    runtime::configure(RuntimeConfig {
        net_workers,
        rx_workers,
    })
    .py(py)
}

/// Process-exit hook (``atexit``): stops log forwarding after a final drain and closes every live
/// connection with reason ``"process_exit"``. Never joins a Rust thread.
#[gen_stub_pyfunction]
#[pyfunction]
#[pyo3(name = "_shutdown_all")]
pub fn shutdown_all() {
    // §4.10: in a forked child (an inherited `atexit` hook) take no process-global lock. A thread
    // that held the registry or log-waiter lock at `fork()` does not exist here, so the child would
    // park forever; inherited connections are leaked, not closed.
    if runtime::FORKED.load(Ordering::Acquire) {
        return;
    }
    runtime::SHUTTING_DOWN.store(true, Ordering::Release);
    sbcore::log::shutdown();
    // Snapshot REGISTRY, then close() each connection outside the registry lock (§4.10).
    sbcore::conn::shutdown_all(sbcore::CloseReason::ProcessExit);
}

/// ``os.register_at_fork(after_in_child=…)`` hook: every native entry point in the child then
/// raises ``SongbirdError``, and inherited connections are leaked instead of torn down (§4.10).
#[gen_stub_pyfunction]
#[pyfunction]
#[pyo3(name = "_disable_after_fork")]
pub fn disable_after_fork() {
    runtime::FORKED.store(true, Ordering::Release);
}

/// Live instance counts of every tracked native type, plus ``songbird_live_tasks`` and the tasks
/// alive on each runtime (``net_alive_tasks``, ``rx_alive_tasks``).
///
/// Returns
/// -------
/// dict[str, int]
///     Used by the leak tests; never starts a runtime. Only the task counts see a songbird Driver
///     that was never Poisoned (its core, events and parked-mixer tasks).
#[gen_stub_pyfunction]
#[pyfunction]
#[pyo3(name = "_debug_live_counts")]
pub fn debug_live_counts() -> HashMap<&'static str, u64> {
    runtime::live_counts().into_iter().collect()
}

pyo3_stub_gen::module_doc!(
    "discord.ext.songbird.native",
    r#"Native backend of discord-ext-songbird (internal).

The public API is ``discord.ext.songbird``; this module's classes are re-exported there where
documented. Every native method is synchronous.

Attributes
----------
VERSION : str
    Package version string.
WAKE_CONN, WAKE_OPS, WAKE_CTRL, WAKE_TRACKS : int
    Reserved wake ids (connection status, op results, control slots, track completion)."#
);
pyo3_stub_gen::module_variable!("discord.ext.songbird.native", "VERSION", &str);
pyo3_stub_gen::module_variable!("discord.ext.songbird.native", "WAKE_CONN", u8);
pyo3_stub_gen::module_variable!("discord.ext.songbird.native", "WAKE_OPS", u8);
pyo3_stub_gen::module_variable!("discord.ext.songbird.native", "WAKE_CTRL", u8);
pyo3_stub_gen::module_variable!("discord.ext.songbird.native", "WAKE_TRACKS", u8);

/// The `discord.ext.songbird.native` extension module.
#[pymodule(gil_used = false)]
mod native {
    use pyo3::prelude::*;

    #[pymodule_export]
    const VERSION: &str = env!("CARGO_PKG_VERSION");
    #[pymodule_export]
    const WAKE_CONN: u8 = sbcore::wake::WAKE_CONN.0;
    #[pymodule_export]
    const WAKE_OPS: u8 = sbcore::wake::WAKE_OPS.0;
    #[pymodule_export]
    const WAKE_CTRL: u8 = sbcore::wake::WAKE_CTRL.0;
    #[pymodule_export]
    const WAKE_TRACKS: u8 = sbcore::wake::WAKE_TRACKS.0;

    #[pymodule_export]
    use crate::py::format::PyAudioFormat;
    #[pymodule_export]
    use crate::py::wake::WakeBell;

    // W2: connection, control, events, tracks.
    #[pymodule_export]
    use crate::py::connection::{Connection, ConnectionStatus, PyVoiceSettings};
    #[pymodule_export]
    use crate::py::events::{Control, EventSubscription, VoiceEvent};
    #[pymodule_export]
    use crate::py::tracks::{NativeTrack, TrackInfo};

    // W3: receive.
    #[pymodule_export]
    use crate::py::arrow::ChunkBatch;
    #[pymodule_export]
    use crate::py::chunk::AudioChunk;
    #[pymodule_export]
    use crate::py::receive::{ReceiveSubscription, RouterSubscription, StreamStats};

    // W4: send and sources.
    #[pymodule_export]
    use crate::py::playback::{
        ClearResult, EncodedSink, OpusSink, Playback, PlaybackStats, SegmentProgress,
    };
    #[pymodule_export]
    use crate::py::sources::{AudioFile, OpusPackets, RawPcm};

    // W5: test hooks (`native._testing`, never in release wheels).
    #[cfg(feature = "test-hooks")]
    #[pymodule_export]
    use crate::py::testing::testing_module;

    #[pymodule_export]
    use crate::{
        configure_runtime, debug_live_counts, disable_after_fork, shutdown_all, supported_codecs,
    };

    #[pymodule_export]
    use crate::logging::{log_dropped, log_wait, set_log_level};

    #[pymodule_init]
    fn init(_m: &Bound<'_, PyModule>) -> PyResult<()> {
        // false only if another `log` logger is already installed in this process.
        let _ = sbcore::log::install();
        Ok(())
    }
}

define_stub_info_gatherer!(stub_info);
