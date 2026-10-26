//! `native.Playback`, `native.EncodedSink`, `native.OpusSink`, `native.ClearResult`,
//! `native.SegmentProgress` and `native.PlaybackStats` (W4, §7, §12.6).
//!
//! Writes are synchronous and all-or-nothing; audio is extracted (bytes borrowed, other buffers
//! copied and released) **before** any sbcore call (I8).

use std::sync::{Arc, Weak};

use pyo3::prelude::*;
use pyo3::types::PyTuple;
use pyo3_stub_gen::derive::{gen_stub_pyclass, gen_stub_pymethods};
use sbcore::conn::ConnShared;
use sbcore::limits::{HOLD_MS, REJECT_GAP_MS};
use sbcore::tracks::TrackRef;
use sbcore::tx::encoded::EncodedShared;
use sbcore::tx::opus_stream::OpusQueue;
use sbcore::tx::queue::{
    ClearOpts, ClearSummary, PcmQueue, PlaybackStatsData, SegmentProgressData, WriteMeta,
};
use sbcore::tx::{CloseMode, LiveSource, PcmProvider};

use crate::convert::{MaxBytes, check_chunk_format, extract_audio, extract_bytes, int_arg};
use crate::errors::{CoreResultExt, ensure_not_forked};
use crate::py::tracks::NativeTrack;

fn close_mode(drain: bool) -> CloseMode {
    if drain {
        CloseMode::Drain
    } else {
        CloseMode::Abort
    }
}

/// The native half of ``PlaybackStream`` (§9.7): a realtime PCM output.
///
/// Notes
/// -----
/// Starts paused (nothing is sent until the first write). Dropping the object closes it (abort),
/// unless ``close(drain=True)`` was already called (the drain then completes).
#[gen_stub_pyclass]
#[pyclass(module = "discord.ext.songbird.native", frozen, weakref)]
pub struct Playback {
    q: Arc<PcmQueue>,
    track: TrackRef,
    conn: Weak<ConnShared>,
}

impl Playback {
    pub(crate) fn new(q: Arc<PcmQueue>, track: TrackRef, conn: Weak<ConnShared>) -> Self {
        Playback { q, track, conn }
    }

    /// The queue (test hooks: `MixerHarness`).
    #[cfg(feature = "test-hooks")]
    pub(crate) fn queue(&self) -> &Arc<PcmQueue> {
        &self.q
    }
}

/// §4.10: in a forked child no lock may be taken (a vanished parent thread may hold it); the
/// handles are then simply freed. Same rule as `EventSubscription`.
fn forked() -> bool {
    sbcore::runtime::check_not_forked().is_err()
}

impl Drop for Playback {
    fn drop(&mut self) {
        if forked() {
            return;
        }
        // Abort, unless a drain close was already requested (it completes on its own).
        self.q.abandon();
    }
}

#[gen_stub_pymethods]
#[pymethods]
impl Playback {
    /// Appends raw audio in the stream format (any size; partial frames are carried).
    ///
    /// Parameters
    /// ----------
    /// data : collections.abc.Buffer
    ///     ``bytes`` or any C-contiguous buffer whose format matches the stream encoding.
    /// tag : str or None, default None
    ///     Application-defined segment tag (at most 128 bytes), used by ``segment()`` and
    ///     ``end_segment()``.
    /// at_ms : int or None, default None
    ///     Source-timeline timestamp in ms; a gap since the previous timed write is filled with
    ///     silence (up to ``max_gap_ms``).
    ///
    /// Raises
    /// ------
    /// TypeError
    ///     ``data`` is a ``str`` or not a buffer.
    /// PlaybackFull
    ///     Not enough space (nothing consumed).
    /// InvalidAudio
    ///     A typed buffer whose format does not match the encoding, an ``AudioChunk`` of another
    ///     format, or a bad tag.
    /// LimitExceeded
    ///     The write is larger than the whole buffer (checked before anything is copied).
    /// StreamClosed
    ///     The playback or connection is closed.
    #[pyo3(signature = (data, tag = None, at_ms = None))]
    fn write(
        &self,
        py: Python<'_>,
        #[gen_stub(override_type(type_repr = "collections.abc.Buffer", imports = ("collections.abc",)))]
        data: &Bound<'_, PyAny>,
        tag: Option<&str>,
        at_ms: Option<i64>,
    ) -> PyResult<()> {
        ensure_not_forked(py)?;
        let fmt = PcmProvider::format(&*self.q);
        check_chunk_format(data, fmt)?;
        let max = MaxBytes {
            what: "write_bytes",
            max: self.q.max_write_bytes(),
        };
        let audio = extract_audio(data, fmt.encoding, max)?;
        self.q
            .write(audio.bytes(), WriteMeta { tag, at_ms })
            .py(py)?;
        Ok(())
    }

    /// Arms a space wake-up for ``nbytes``; ``True`` if there is already enough space.
    fn arm_space(&self, nbytes: usize) -> bool {
        self.q.arm_space(nbytes)
    }

    /// Releases the prebuffer gate and arms the drain wake-up; ``True`` if already drained.
    fn arm_drain(&self) -> bool {
        self.q.arm_drain()
    }

    /// Arms the Active → Idle wake-up; ``True`` if already idle.
    fn arm_idle(&self) -> bool {
        self.q.arm_idle()
    }

    /// Arms the reader-gone wake-up; ``True`` if the track already ended.
    fn arm_end(&self) -> bool {
        self.q.arm_end()
    }

    /// Interruption: discards all queued audio now (any thread).
    ///
    /// Parameters
    /// ----------
    /// reject_late : bool, default True
    ///     Drop later writes for every open/interrupted tag.
    /// hold_ms : int, default 0
    ///     Drop **all** writes for this long (``0..=10000``).
    /// reject_until_gap_ms : int or None, default None
    ///     Drop later timed writes contiguous with the cleared audio (``0..=10000``).
    #[pyo3(signature = (*, reject_late = true, hold_ms = 0, reject_until_gap_ms = None))]
    fn clear(
        &self,
        py: Python<'_>,
        reject_late: bool,
        hold_ms: i64,
        reject_until_gap_ms: Option<i64>,
    ) -> PyResult<ClearResult> {
        ensure_not_forked(py)?;
        let opts = ClearOpts {
            reject_late,
            hold_ms: int_arg(py, "hold_ms", hold_ms, HOLD_MS)?,
            reject_until_gap_ms: match reject_until_gap_ms {
                Some(v) => Some(int_arg(py, "reject_until_gap_ms", v, REJECT_GAP_MS)?),
                None => None,
            },
        };
        let s = self.q.clear(opts).py(py)?;
        Ok(ClearResult { s: Arc::new(s) })
    }

    /// The application finished writing this tagged segment: closes the tag and releases the
    /// prebuffer gate.
    fn end_segment(&self, py: Python<'_>, tag: &str) -> PyResult<()> {
        ensure_not_forked(py)?;
        self.q.end_segment(tag);
        Ok(())
    }

    /// Progress of the tagged segment ``tag``, if it is known.
    fn segment(&self, tag: &str) -> Option<SegmentProgress> {
        self.q.segment(tag).map(SegmentProgress::new)
    }

    fn pause(&self, py: Python<'_>) -> PyResult<()> {
        ensure_not_forked(py)?;
        self.q.pause(true);
        Ok(())
    }

    fn resume(&self, py: Python<'_>) -> PyResult<()> {
        ensure_not_forked(py)?;
        self.q.pause(false);
        Ok(())
    }

    /// ``0.0..=4.0`` with a 5 ms ramp.
    #[getter]
    fn volume(&self) -> f32 {
        self.q.volume()
    }

    #[setter]
    fn set_volume(
        &self,
        #[gen_stub(override_type(type_repr = "float"))] volume: &Bound<'_, PyAny>,
    ) -> PyResult<()> {
        let py = volume.py();
        ensure_not_forked(py)?;
        let v: f32 = volume.extract()?;
        self.q.set_volume(v).py(py)
    }

    /// Real audio handed to the Discord encoder.
    #[getter]
    fn played_ms(&self) -> u64 {
        self.q.stats().played_ms
    }

    /// Estimate: ``played_ms`` minus the part of ``output_latency_ms`` not yet elapsed.
    #[getter]
    fn heard_ms(&self) -> u64 {
        self.q.stats().heard_ms
    }

    /// Playhead including silence.
    #[getter]
    fn position_ms(&self) -> u64 {
        self.q.stats().position_ms
    }

    #[getter]
    fn buffered_ms(&self) -> u64 {
        self.q.stats().buffered_ms
    }

    #[getter]
    fn written_ms(&self) -> u64 {
        self.q.stats().written_ms
    }

    #[getter]
    fn is_idle(&self) -> bool {
        self.q.stats().idle
    }

    fn stats(&self) -> PlaybackStats {
        PlaybackStats { s: self.q.stats() }
    }

    /// Closes the playback (``drain=True``: once everything queued has played). Idempotent.
    #[pyo3(signature = (drain = false))]
    fn close(&self, drain: bool) {
        // A no-op after fork() (§4.10, see Drop).
        if !forked() {
            LiveSource::close(&*self.q, close_mode(drain));
        }
    }

    #[getter]
    fn closed(&self) -> bool {
        self.q.is_closed()
    }

    #[getter]
    fn wake_id(&self) -> u8 {
        self.q.wake_id().0
    }

    #[getter]
    fn playback_id(&self) -> u32 {
        LiveSource::id(&*self.q)
    }

    #[getter]
    fn track(&self) -> NativeTrack {
        NativeTrack::new(self.track.clone(), self.conn.clone())
    }
}

/// The native half of ``EncodedStream`` (§9.7): push encoded bytes, decoded off the mixer thread.
///
/// Notes
/// -----
/// Dropping the object closes it (abort), unless ``finish()`` (or ``close(drain=True)``) was
/// already called: the decoded audio then plays out and the track ends on its own.
#[gen_stub_pyclass]
#[pyclass(module = "discord.ext.songbird.native", frozen, weakref)]
pub struct EncodedSink {
    enc: Arc<EncodedShared>,
    track: TrackRef,
    conn: Weak<ConnShared>,
}

impl EncodedSink {
    pub(crate) fn new(enc: Arc<EncodedShared>, track: TrackRef, conn: Weak<ConnShared>) -> Self {
        EncodedSink { enc, track, conn }
    }
}

impl Drop for EncodedSink {
    fn drop(&mut self) {
        if forked() {
            return;
        }
        // Abort, unless the input is complete (`finish()`): the decoded audio still plays out.
        self.enc.abandon();
    }
}

#[gen_stub_pymethods]
#[pymethods]
impl EncodedSink {
    /// Appends encoded bytes.
    ///
    /// Raises
    /// ------
    /// PlaybackFull
    ///     The buffer is full (nothing consumed).
    fn write(
        &self,
        py: Python<'_>,
        #[gen_stub(override_type(type_repr = "collections.abc.Buffer", imports = ("collections.abc",)))]
        data: &Bound<'_, PyAny>,
    ) -> PyResult<()> {
        ensure_not_forked(py)?;
        let bytes = extract_bytes(data)?;
        self.enc.write(bytes.bytes()).py(py)
    }

    /// Arms a space wake-up for ``nbytes``; ``True`` if there is already enough space.
    fn arm_space(&self, nbytes: usize) -> bool {
        self.enc.arm_space(nbytes)
    }

    /// End of input: the track ends after the decoded audio has played.
    fn finish(&self, py: Python<'_>) -> PyResult<()> {
        ensure_not_forked(py)?;
        self.enc.finish();
        Ok(())
    }

    /// Idempotent (``drain=True``: once the decoded audio has played).
    #[pyo3(signature = (drain = false))]
    fn close(&self, drain: bool) {
        // A no-op after fork() (§4.10, see Drop).
        if !forked() {
            LiveSource::close(&*self.enc, close_mode(drain));
        }
    }

    #[getter]
    fn wake_id(&self) -> u8 {
        self.enc.wake_id().0
    }

    #[getter]
    fn track(&self) -> NativeTrack {
        NativeTrack::new(self.track.clone(), self.conn.clone())
    }
}

/// The native half of ``OpusPacketStream`` (§9.7): push 20 ms / 48 kHz Opus packets.
///
/// Notes
/// -----
/// Dropping the object closes it (abort), unless ``close(drain=True)`` was already called (the
/// drain then completes).
#[gen_stub_pyclass]
#[pyclass(module = "discord.ext.songbird.native", frozen, weakref)]
pub struct OpusSink {
    q: Arc<OpusQueue>,
    track: TrackRef,
    conn: Weak<ConnShared>,
}

impl OpusSink {
    pub(crate) fn new(q: Arc<OpusQueue>, track: TrackRef, conn: Weak<ConnShared>) -> Self {
        OpusSink { q, track, conn }
    }
}

impl Drop for OpusSink {
    fn drop(&mut self) {
        if forked() {
            return;
        }
        // Abort, unless a drain close was already requested (it completes on its own).
        self.q.abandon();
    }
}

#[gen_stub_pymethods]
#[pymethods]
impl OpusSink {
    /// Queues one packet.
    ///
    /// Raises
    /// ------
    /// PlaybackFull
    ///     The queue is full (nothing consumed).
    /// InvalidAudio
    ///     Not 1..=1275 bytes or not exactly 960 samples at 48 kHz.
    fn send(
        &self,
        py: Python<'_>,
        #[gen_stub(override_type(type_repr = "collections.abc.Buffer", imports = ("collections.abc",)))]
        packet: &Bound<'_, PyAny>,
    ) -> PyResult<()> {
        ensure_not_forked(py)?;
        let bytes = extract_bytes(packet)?;
        self.q.send(bytes.bytes()).py(py)
    }

    /// Arms a space wake-up; ``True`` if there is already space.
    fn arm_space(&self) -> bool {
        self.q.arm_space()
    }

    /// Discards queued packets; returns the milliseconds discarded.
    fn clear(&self) -> u64 {
        self.q.clear()
    }

    #[getter]
    fn buffered_ms(&self) -> u64 {
        self.q.buffered_ms()
    }

    /// Idempotent (``drain=True``: once the queued packets have played).
    #[pyo3(signature = (drain = false))]
    fn close(&self, drain: bool) {
        // A no-op after fork() (§4.10, see Drop).
        if !forked() {
            LiveSource::close(&*self.q, close_mode(drain));
        }
    }

    #[getter]
    fn wake_id(&self) -> u8 {
        self.q.wake_id().0
    }

    #[getter]
    fn track(&self) -> NativeTrack {
        NativeTrack::new(self.track.clone(), self.conn.clone())
    }
}

/// Result of ``PlaybackStream.clear()`` (§9.7).
#[gen_stub_pyclass]
#[pyclass(module = "discord.ext.songbird.native", frozen)]
pub struct ClearResult {
    s: Arc<ClearSummary>,
}

#[gen_stub_pymethods]
#[pymethods]
impl ClearResult {
    #[getter]
    fn discarded_ms(&self) -> u64 {
        self.s.discarded_ms
    }

    #[getter]
    fn played_ms(&self) -> u64 {
        self.s.played_ms
    }

    #[getter]
    fn heard_ms(&self) -> u64 {
        self.s.heard_ms
    }

    #[getter]
    fn position_ms(&self) -> u64 {
        self.s.position_ms
    }

    #[getter]
    fn generation(&self) -> u64 {
        self.s.generation
    }

    /// Every open tag plus every tag pulled within the last ``output_latency_ms``.
    #[getter]
    #[gen_stub(override_return_type(type_repr = "tuple[SegmentProgress, ...]"))]
    fn segments<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyTuple>> {
        PyTuple::new(
            py,
            self.s
                .segments
                .iter()
                .map(|d| SegmentProgress::new(d.clone())),
        )
    }

    /// The progress of ``tag``, if it is in ``segments``.
    fn segment(&self, tag: &str) -> Option<SegmentProgress> {
        self.s
            .segments
            .iter()
            .find(|d| d.tag.as_ref() == tag)
            .map(|d| SegmentProgress::new(d.clone()))
    }
}

/// Playback progress of one tagged segment (§7.7).
#[gen_stub_pyclass]
#[pyclass(module = "discord.ext.songbird.native", frozen)]
pub struct SegmentProgress {
    d: Arc<SegmentProgressData>,
}

impl SegmentProgress {
    fn new(d: SegmentProgressData) -> Self {
        SegmentProgress { d: Arc::new(d) }
    }
}

#[gen_stub_pymethods]
#[pymethods]
impl SegmentProgress {
    #[getter]
    fn tag(&self) -> &str {
        &self.d.tag
    }

    #[getter]
    fn written_ms(&self) -> u64 {
        self.d.written_ms
    }

    /// Clamped to ``written_ms``.
    #[getter]
    fn played_ms(&self) -> u64 {
        self.d.played_ms
    }

    #[getter]
    fn heard_ms(&self) -> u64 {
        self.d.heard_ms
    }

    /// ``end_segment(tag)`` was called and everything written has been played.
    #[getter]
    fn complete(&self) -> bool {
        self.d.complete
    }

    fn __repr__(&self) -> String {
        format!(
            "<SegmentProgress tag={:?} written_ms={} played_ms={} heard_ms={} complete={}>",
            self.d.tag, self.d.written_ms, self.d.played_ms, self.d.heard_ms, self.d.complete
        )
    }
}

/// Counters of one playback (§9.7).
#[gen_stub_pyclass]
#[pyclass(module = "discord.ext.songbird.native", frozen, skip_from_py_object)]
#[derive(Clone, Copy, Debug)]
pub struct PlaybackStats {
    s: PlaybackStatsData,
}

#[gen_stub_pymethods]
#[pymethods]
impl PlaybackStats {
    #[getter]
    fn written_ms(&self) -> u64 {
        self.s.written_ms
    }

    #[getter]
    fn played_ms(&self) -> u64 {
        self.s.played_ms
    }

    #[getter]
    fn heard_ms(&self) -> u64 {
        self.s.heard_ms
    }

    #[getter]
    fn buffered_ms(&self) -> u64 {
        self.s.buffered_ms
    }

    #[getter]
    fn underrun_ms(&self) -> u64 {
        self.s.underrun_ms
    }

    #[getter]
    fn gap_filled_ms(&self) -> u64 {
        self.s.gap_filled_ms
    }

    #[getter]
    fn rejected_ms(&self) -> u64 {
        self.s.rejected_ms
    }

    #[getter]
    fn discarded_ms(&self) -> u64 {
        self.s.discarded_ms
    }

    #[getter]
    fn contended_ticks(&self) -> u64 {
        self.s.contended_ticks
    }

    #[getter]
    fn clears(&self) -> u64 {
        self.s.clears
    }

    #[getter]
    fn tags_evicted(&self) -> u64 {
        self.s.tags_evicted
    }

    fn __repr__(&self) -> String {
        format!("<PlaybackStats {:?}>", self.s)
    }
}
