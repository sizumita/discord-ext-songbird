//! `native.ReceiveSubscription`, `native.RouterSubscription` and `native.StreamStats`
//! (W3, §8.9, §8.10, §12.6).

use std::collections::BTreeSet;

use pyo3::exceptions::PyValueError;
use pyo3::prelude::*;
use pyo3_stub_gen::derive::{gen_stub_pyclass, gen_stub_pymethods};
use sbcore::events::Pop;
use sbcore::rx::bus::{MixFilter, MixPolicy};
use sbcore::rx::queue::SubStats;
use sbcore::rx::{Delivered, Overflow, RouterHandle, Subscription};
use sbcore::{CloseReason, CoreError};

use crate::convert::int_arg;
use crate::errors::{CoreResultExt, core_err, ensure_not_forked};
use crate::py::arrow::ChunkBatch;
use crate::py::chunk::AudioChunk;
use crate::py::format::PyAudioFormat;

/// Largest `read_batch(max_chunks)`: every chunk of the largest queue (60 s of 10 ms chunks).
const MAX_BATCH_CHUNKS: usize = (sbcore::limits::MAX_QUEUE_MS_MAX / 10) as usize;

/// `"drop_oldest"` / `"drop_newest"` / `"error"`.
pub(crate) fn parse_overflow(s: &str) -> PyResult<Overflow> {
    match s {
        "drop_oldest" => Ok(Overflow::DropOldest),
        "drop_newest" => Ok(Overflow::DropNewest),
        "error" => Ok(Overflow::Error),
        other => Err(PyValueError::new_err(format!(
            "overflow must be 'drop_oldest', 'drop_newest' or 'error' (got {other:?})"
        ))),
    }
}

pub(crate) fn user_set(users: Vec<u64>) -> BTreeSet<u64> {
    users.into_iter().collect()
}

/// The filter of a mixed stream.
pub(crate) fn mix_filter(
    include: Option<Vec<u64>>,
    exclude: Vec<u64>,
    ignore_bots: bool,
    policy: &str,
    floor_release_ms: u32,
) -> PyResult<MixFilter> {
    let policy = match policy {
        "mix" => MixPolicy::Mix,
        "floor" => MixPolicy::Floor {
            release_ms: floor_release_ms,
        },
        other => {
            return Err(PyValueError::new_err(format!(
                "policy must be 'mix' or 'floor' (got {other:?})"
            )));
        }
    };
    Ok(MixFilter {
        include: include.map(user_set),
        exclude: user_set(exclude),
        ignore_bots,
        policy,
    })
}

/// A popped chunk, `None`, or the close error (`StreamOverflow` once after an `error` overflow,
/// otherwise `StreamClosed`).
fn pop_chunk(py: Python<'_>, p: Pop<Delivered>) -> PyResult<Option<AudioChunk>> {
    match p {
        Pop::Item(d) => Ok(Some(AudioChunk::new(d))),
        Pop::Empty => Ok(None),
        Pop::Closed(CloseReason::Overflow) => Err(core_err(py, CoreError::StreamOverflow)),
        Pop::Closed(r) => Err(core_err(py, CoreError::StreamClosed(r))),
    }
}

/// The native half of ``AudioStream`` (§9.5): a bounded, continuous receive subscription.
///
/// Notes
/// -----
/// Dropping the object closes the subscription and frees its queued chunks.
#[gen_stub_pyclass]
#[pyclass(module = "discord.ext.songbird.native", frozen, weakref)]
pub struct ReceiveSubscription {
    sub: Subscription,
}

impl ReceiveSubscription {
    pub(crate) fn new(sub: Subscription) -> Self {
        ReceiveSubscription { sub }
    }
}

#[gen_stub_pymethods]
#[pymethods]
impl ReceiveSubscription {
    /// Pops the next chunk, or arms ``wake_id`` and returns ``None`` (loop thread).
    ///
    /// Raises
    /// ------
    /// StreamOverflow
    ///     Once, after the queued chunks, under ``overflow="error"``.
    /// StreamClosed
    ///     Once closed and drained.
    fn try_recv(&self, py: Python<'_>) -> PyResult<Option<AudioChunk>> {
        ensure_not_forked(py)?;
        pop_chunk(py, self.sub.queue.try_pop_or_arm())
    }

    /// Pops without arming (any thread); ``None`` if empty.
    ///
    /// Raises
    /// ------
    /// StreamOverflow
    ///     Once, after the queued chunks, under ``overflow="error"``.
    /// StreamClosed
    ///     Once closed and drained.
    fn read_nowait(&self, py: Python<'_>) -> PyResult<Option<AudioChunk>> {
        ensure_not_forked(py)?;
        pop_chunk(py, self.sub.queue.try_pop())
    }

    /// The wake id rung when a chunk arrives after arming.
    #[getter]
    fn wake_id(&self) -> u8 {
        self.sub.queue.wake_id().0
    }

    /// Drops everything queued; returns the milliseconds dropped.
    ///
    /// Parameters
    /// ----------
    /// reset_clock : bool, default True
    ///     The next chunk has ``start_ms == 0`` and ``seq == 0``.
    #[pyo3(signature = (reset_clock = true))]
    fn skip_to_live(&self, py: Python<'_>, reset_clock: bool) -> PyResult<u64> {
        ensure_not_forked(py)?;
        Ok(self.sub.queue.skip_to_live(reset_clock))
    }

    /// ``timeline_ms`` of the stream clock origin; ``None`` until the first chunk after a reset.
    #[getter]
    fn origin_timeline_ms(&self) -> Option<u64> {
        self.sub.queue.origin_timeline_ms()
    }

    /// Mixed streams only: applied at the next chunk boundary, with no gap or duplicate.
    ///
    /// Raises
    /// ------
    /// ValueError
    ///     On a per-user stream.
    #[pyo3(signature = (include, exclude, ignore_bots = true, policy = "mix", floor_release_ms = 600))]
    fn set_filter(
        &self,
        py: Python<'_>,
        include: Option<Vec<u64>>,
        exclude: Vec<u64>,
        ignore_bots: bool,
        #[gen_stub(override_type(type_repr = "typing.Literal['mix', 'floor']", imports = ("typing",)))]
        policy: &str,
        floor_release_ms: i64,
    ) -> PyResult<()> {
        ensure_not_forked(py)?;
        let floor_release_ms = int_arg(py, "floor_release_ms", floor_release_ms, 0..=u32::MAX)?;
        if self.sub.queue.user_id().is_some() {
            return Err(PyValueError::new_err(
                "set_include/set_exclude apply to mixed streams only",
            ));
        }
        let f = mix_filter(include, exclude, ignore_bots, policy, floor_release_ms)?;
        self.sub.set_filter(f).py(py)
    }

    fn stats(&self) -> StreamStats {
        StreamStats {
            s: self.sub.queue.stats(),
        }
    }

    /// Idempotent, any thread; unregisters at the next tick and frees queued chunks.
    fn close(&self) {
        self.sub.close();
    }

    #[getter]
    fn closed(&self) -> bool {
        self.sub.queue.is_closed()
    }

    /// ``'closed'``, ``'connection_closed'``, ``'user_left'``, ``'overflow'`` or
    /// ``'internal_error'``.
    #[getter]
    fn close_reason(&self) -> Option<&'static str> {
        self.sub.queue.close_reason().map(|r| r.as_str())
    }

    /// ``[arrow]`` Drains up to ``max_chunks`` chunks into one batch; ``None`` if empty.
    ///
    /// Parameters
    /// ----------
    /// max_chunks : int, default 50
    ///     ``1..=6000`` (the largest queue bound: 60 s of 10 ms chunks).
    ///
    /// Raises
    /// ------
    /// SongbirdError
    ///     Built without the ``arrow`` feature.
    /// StreamOverflow
    ///     Once, after the queued chunks, under ``overflow="error"``.
    /// StreamClosed
    ///     Once closed and drained.
    #[pyo3(signature = (max_chunks = 50))]
    fn read_batch(&self, py: Python<'_>, max_chunks: i64) -> PyResult<Option<ChunkBatch>> {
        ensure_not_forked(py)?;
        if !cfg!(feature = "arrow") {
            return Err(core_err(py, CoreError::FeatureMissing("arrow")));
        }
        let max_chunks = int_arg(py, "max_chunks", max_chunks, 1..=MAX_BATCH_CHUNKS)?;
        let mut rows = Vec::new();
        let closed = self.sub.queue.drain(max_chunks, &mut rows);
        match (rows.is_empty(), closed) {
            (false, _) => Ok(Some(ChunkBatch::new(rows))),
            (true, Some(CloseReason::Overflow)) => Err(core_err(py, CoreError::StreamOverflow)),
            (true, Some(r)) => Err(core_err(py, CoreError::StreamClosed(r))),
            (true, None) => Ok(None),
        }
    }

    /// Stream format.
    #[getter]
    fn format(&self) -> PyAudioFormat {
        PyAudioFormat::new_from(self.sub.queue.format())
    }

    /// The user of a per-user stream; ``None`` for mixed streams.
    #[getter]
    fn user_id(&self) -> Option<u64> {
        self.sub.queue.user_id()
    }

    #[getter]
    fn chunk_ms(&self) -> u32 {
        self.sub.queue.chunk_ms()
    }
}

/// The native half of ``UserStreams`` (§9.5): yields one subscription per user as they first speak.
///
/// Notes
/// -----
/// Dropping the object closes the router and every stream it created.
#[gen_stub_pyclass]
#[pyclass(module = "discord.ext.songbird.native", frozen)]
pub struct RouterSubscription {
    router: RouterHandle,
}

impl RouterSubscription {
    pub(crate) fn new(router: RouterHandle) -> Self {
        RouterSubscription { router }
    }
}

#[gen_stub_pymethods]
#[pymethods]
impl RouterSubscription {
    /// The next per-user stream, or ``None`` after arming ``wake_id``.
    ///
    /// Raises
    /// ------
    /// StreamClosed
    ///     Once the router is closed.
    fn try_next(&self, py: Python<'_>) -> PyResult<Option<ReceiveSubscription>> {
        ensure_not_forked(py)?;
        match self.router.queue.try_pop_or_arm() {
            Pop::Item(sub) => Ok(Some(ReceiveSubscription::new(sub))),
            Pop::Empty => Ok(None),
            Pop::Closed(r) => Err(core_err(py, CoreError::StreamClosed(r))),
        }
    }

    #[getter]
    fn wake_id(&self) -> u8 {
        self.router.queue.wake_id().0
    }

    /// Streams discarded because the router queue (64) overflowed.
    #[getter]
    fn dropped(&self) -> u64 {
        self.router.queue.dropped()
    }

    /// Closes the router; ``close_streams`` also closes every stream it created.
    #[pyo3(signature = (close_streams = true))]
    fn close(&self, close_streams: bool) {
        self.router.queue.close(close_streams);
    }
}

/// Counters of one receive stream (§9.5).
#[gen_stub_pyclass]
#[pyclass(module = "discord.ext.songbird.native", frozen, skip_from_py_object)]
#[derive(Clone, Copy, Debug)]
pub struct StreamStats {
    s: SubStats,
}

#[gen_stub_pymethods]
#[pymethods]
impl StreamStats {
    #[getter]
    fn queued_chunks(&self) -> u64 {
        self.s.queued_chunks
    }

    #[getter]
    fn queued_ms(&self) -> u64 {
        self.s.queued_ms
    }

    #[getter]
    fn delivered_chunks(&self) -> u64 {
        self.s.delivered
    }

    #[getter]
    fn dropped_chunks(&self) -> u64 {
        self.s.dropped_chunks
    }

    #[getter]
    fn dropped_ms(&self) -> u64 {
        self.s.dropped_ms
    }

    #[getter]
    fn overflowed(&self) -> bool {
        self.s.overflowed
    }

    fn __repr__(&self) -> String {
        format!("<StreamStats {:?}>", self.s)
    }
}
