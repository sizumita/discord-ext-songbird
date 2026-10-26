//! `native.NativeTrack` and `native.TrackInfo` (W2, §7.10, §12.6).
//!
//! `NativeTrack` holds the track's `TrackRef` (an `Arc<TrackFlags>` plus songbird's `TrackHandle`
//! once the track started) and a `Weak<ConnShared>` for pending queue entries.

use std::hash::{Hash, Hasher};
use std::sync::{Arc, Weak};

use pyo3::prelude::*;
use pyo3_stub_gen::derive::{gen_stub_pyclass, gen_stub_pymethods};
use sbcore::conn::ConnShared;
use sbcore::conn::ops::TrackInfoData;
use sbcore::limits::{VOLUME, check};
use sbcore::tracks::flags::set_track_loops;
use sbcore::tracks::{ControlError, PendingChange, TrackHandle, TrackRef};
use sbcore::{CloseReason, CoreError};

use crate::convert::int_arg;
use crate::errors::{CoreResultExt, core_err, ensure_not_forked};

/// songbird's `uuid` as the canonical hyphenated string.
fn format_uuid(u: u128) -> String {
    let h = format!("{u:032x}");
    format!(
        "{}-{}-{}-{}-{}",
        &h[0..8],
        &h[8..12],
        &h[12..16],
        &h[16..20],
        &h[20..32]
    )
}

/// The native half of ``TrackHandle`` (§9.8).
#[gen_stub_pyclass]
#[pyclass(module = "discord.ext.songbird.native", frozen)]
pub struct NativeTrack {
    track: TrackRef,
    conn: Weak<ConnShared>,
}

impl NativeTrack {
    pub(crate) fn new(track: TrackRef, conn: Weak<ConnShared>) -> Self {
        NativeTrack { track, conn }
    }

    pub(crate) fn track_ref(&self) -> &TrackRef {
        &self.track
    }

    fn is_pending(&self) -> bool {
        self.track.handle.get().is_none() && self.track.flags.ended().is_none()
    }

    /// Applies `op` to songbird's handle, or `change` to a pending queue entry.
    fn control(
        &self,
        py: Python<'_>,
        change: PendingChange,
        op: impl FnOnce(&TrackHandle) -> Result<(), CoreError>,
    ) -> PyResult<()> {
        ensure_not_forked(py)?;
        if let Some(h) = self.track.handle.get() {
            return op(h).py(py);
        }
        let conn = self.conn.upgrade().ok_or_else(|| {
            core_err(
                py,
                CoreError::ConnectionClosed(CloseReason::ConnectionClosed),
            )
        })?;
        if conn.queue().update_pending(&self.track, change).py(py)? {
            Ok(())
        } else {
            Err(core_err(py, CoreError::Track("track has ended".into())))
        }
    }
}

fn track_err(e: ControlError) -> CoreError {
    CoreError::Track(e.to_string().into())
}

#[gen_stub_pymethods]
#[pymethods]
impl NativeTrack {
    /// Stable per connection.
    #[getter]
    fn id(&self) -> u64 {
        self.track.flags.id
    }

    /// songbird's track uuid (``None`` until a queued entry starts).
    #[getter]
    fn uuid(&self) -> Option<String> {
        self.track.flags.uuid().map(format_uuid)
    }

    #[getter]
    #[gen_stub(override_return_type(type_repr = "typing.Literal['file', 'pcm', 'opus', 'playback', 'encoded_stream', 'opus_stream']", imports = ("typing",)))]
    fn kind(&self) -> &'static str {
        self.track.flags.kind.as_str()
    }

    #[getter]
    fn ended(&self) -> bool {
        self.track.flags.ended().is_some()
    }

    /// ``"finished"``, ``"stopped"``, ``"error"``, ``"disconnected"`` or ``None``.
    #[getter]
    #[gen_stub(override_return_type(type_repr = "typing.Literal['finished', 'stopped', 'error', 'disconnected'] | None", imports = ("typing",)))]
    fn end_reason(&self) -> Option<&'static str> {
        self.track.flags.ended().map(|r| r.as_str())
    }

    /// The error message of an ``"error"`` end.
    #[getter]
    fn end_error(&self) -> Option<String> {
        self.track.flags.error().map(str::to_owned)
    }

    /// Whether this is a queue entry that has not started yet.
    #[getter]
    fn pending(&self) -> bool {
        self.is_pending()
    }

    fn pause(&self, py: Python<'_>) -> PyResult<()> {
        self.control(py, PendingChange::Paused(true), |h| {
            h.pause().map_err(track_err)
        })
    }

    fn resume(&self, py: Python<'_>) -> PyResult<()> {
        self.control(py, PendingChange::Paused(false), |h| {
            h.play().map_err(track_err)
        })
    }

    /// Stops the track; a pending entry is removed with reason ``"stopped"``.
    fn stop(&self, py: Python<'_>) -> PyResult<()> {
        ensure_not_forked(py)?;
        if let Some(h) = self.track.handle.get() {
            return h.stop().map_err(track_err).py(py);
        }
        if let Some(conn) = self.conn.upgrade() {
            conn.queue().remove(&self.track);
        }
        Ok(())
    }

    /// ``0.0..=4.0``. File tracks: applied by songbird's live mixer.
    fn set_volume(&self, py: Python<'_>, volume: f32) -> PyResult<()> {
        let volume = check("volume", volume, VOLUME).py(py)?;
        self.control(py, PendingChange::Volume(volume), |h| {
            h.set_volume(volume).map_err(track_err)
        })
    }

    /// ``None`` loops forever. ``TrackError`` for live sources.
    fn set_loops(&self, py: Python<'_>, count: Option<i64>) -> PyResult<()> {
        if self.track.flags.kind.is_live() {
            return Err(core_err(
                py,
                CoreError::Track("live sources cannot loop".into()),
            ));
        }
        let loops = match count {
            Some(n) => Some(int_arg(py, "loops", n, 0..=u32::MAX - 1)?),
            None => None,
        };
        self.control(py, PendingChange::Loops(loops), |h| {
            set_track_loops(h, loops)
        })
    }

    fn __eq__(&self, other: &Bound<'_, PyAny>) -> bool {
        other
            .cast::<NativeTrack>()
            .is_ok_and(|o| Arc::ptr_eq(&o.get().track.flags, &self.track.flags))
    }

    fn __hash__(&self) -> u64 {
        let mut h = std::collections::hash_map::DefaultHasher::new();
        (Arc::as_ptr(&self.track.flags) as usize).hash(&mut h);
        h.finish()
    }

    fn __repr__(&self) -> String {
        format!(
            "<NativeTrack id={} kind={} ended={:?}>",
            self.track.flags.id,
            self.track.flags.kind.as_str(),
            self.track.flags.ended().map(|r| r.as_str())
        )
    }
}

/// A track's state as reported by songbird's live mixer (§9.8).
#[gen_stub_pyclass]
#[pyclass(module = "discord.ext.songbird.native", frozen, skip_from_py_object)]
#[derive(Clone, Copy, Debug)]
pub struct TrackInfo {
    info: TrackInfoData,
}

impl TrackInfo {
    pub(crate) fn new(info: TrackInfoData) -> Self {
        TrackInfo { info }
    }
}

#[gen_stub_pymethods]
#[pymethods]
impl TrackInfo {
    #[getter]
    #[gen_stub(override_return_type(type_repr = "typing.Literal['play', 'pause', 'stop', 'end', 'errored']", imports = ("typing",)))]
    fn playing(&self) -> &'static str {
        self.info.playing
    }

    #[getter]
    fn volume(&self) -> f32 {
        self.info.volume
    }

    #[getter]
    fn position_ms(&self) -> u64 {
        self.info.position_ms
    }

    #[getter]
    fn play_time_ms(&self) -> u64 {
        self.info.play_time_ms
    }

    /// ``None`` = infinite.
    #[getter]
    fn loops(&self) -> Option<u32> {
        self.info.loops
    }

    fn __repr__(&self) -> String {
        format!("<TrackInfo {:?}>", self.info)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uuid_format() {
        assert_eq!(
            format_uuid(0x0123_4567_89ab_cdef_0123_4567_89ab_cdef),
            "01234567-89ab-cdef-0123-456789abcdef"
        );
    }
}
