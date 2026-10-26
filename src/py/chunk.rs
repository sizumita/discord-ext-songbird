//! `native.AudioChunk` (W3, §8.10): one received chunk, exposing its PCM through the buffer protocol
//! without copying (`view.obj = self`), plus `to_bytes()` and Arrow export.

use std::ffi::{CStr, c_int, c_void};
use std::ptr;
use std::sync::Arc;
use std::time::Instant;

use pyo3::exceptions::PyBufferError;
use pyo3::ffi;
use pyo3::prelude::*;
use pyo3::sync::PyOnceLock;
use pyo3::types::{PyBytes, PyFrozenSet};
use pyo3_stub_gen::derive::{gen_stub_pyclass, gen_stub_pymethods};
use pyo3_stub_gen::inventory::submit;
use sbcore::Encoding;
use sbcore::rx::{ChunkData, Delivered};

use crate::py::format::PyAudioFormat;

#[cfg(target_endian = "big")]
compile_error!("AudioChunk exports native-endian buffer formats, which must equal little-endian");

/// `(Instant, time.monotonic())` captured together on first use; maps Rust instants to the Python
/// monotonic clock (§8.10). Plain data, no Python object (I1).
static MONO_ANCHOR: PyOnceLock<(Instant, f64)> = PyOnceLock::new();

/// `t` as a `time.monotonic()` value.
pub(crate) fn monotonic(py: Python<'_>, t: Instant) -> f64 {
    let &(base, base_mono) = MONO_ANCHOR.get_or_init(py, || {
        let now = Instant::now();
        let mono = py
            .import("time")
            .and_then(|m| m.call_method0("monotonic"))
            .and_then(|v| v.extract::<f64>())
            .unwrap_or(0.0);
        (now, mono)
    });
    match t.checked_duration_since(base) {
        Some(d) => base_mono + d.as_secs_f64(),
        None => base_mono - base.duration_since(t).as_secs_f64(),
    }
}

/// One received chunk on a continuous stream clock (§9.5).
///
/// Notes
/// -----
/// ``memoryview(chunk)``, ``bytes(chunk)``, ``np.frombuffer(chunk, '<i2')`` and
/// ``wave.writeframes(chunk)`` read the payload without an intermediate copy. The buffer is
/// read-only and 1-D with format ``'h'`` (pcm16le), ``'f'`` (f32le) or ``'B'`` (G.711).
#[gen_stub_pyclass]
#[pyclass(module = "discord.ext.songbird.native", frozen, weakref)]
pub struct AudioChunk {
    chunk: Arc<ChunkData>,
    seq: u64,
    start_ms: u64,
    dropped_ms: u64,
}

impl AudioChunk {
    pub(crate) fn new(d: Delivered) -> Self {
        AudioChunk {
            chunk: d.chunk,
            seq: d.seq,
            start_ms: d.start_ms,
            dropped_ms: d.dropped_ms,
        }
    }

    /// Buffer format code and item size of the payload.
    fn item(&self) -> (&'static CStr, isize) {
        match self.chunk.meta.format.encoding {
            Encoding::Pcm16Le => (c"h", 2),
            Encoding::F32Le => (c"f", 4),
            Encoding::MuLaw | Encoding::ALaw => (c"B", 1),
        }
    }
}

submit! {
    pyo3_stub_gen::derive::gen_methods_from_python! {
        r#"
        class AudioChunk:
            def __buffer__(self, flags: int, /) -> memoryview:
                """Read-only, zero-copy view of the payload ('h' | 'f' | 'B')."""
        "#
    }
}

#[gen_stub_pymethods]
#[pymethods]
impl AudioChunk {
    #[getter]
    fn format(&self) -> PyAudioFormat {
        PyAudioFormat::new_from(self.chunk.meta.format)
    }

    /// ``None`` for mixed streams.
    #[getter]
    fn user_id(&self) -> Option<u64> {
        self.chunk.meta.user_id
    }

    /// ``start_ms // chunk_ms`` (jumps over dropped chunks).
    #[getter]
    fn seq(&self) -> u64 {
        self.seq
    }

    /// Stream clock (0 = first chunk of this stream).
    #[getter]
    fn start_ms(&self) -> u64 {
        self.start_ms
    }

    #[getter]
    fn end_ms(&self) -> u64 {
        self.start_ms.saturating_add(self.chunk.duration_ms())
    }

    /// ``== chunk_ms``.
    #[getter]
    fn duration_ms(&self) -> u64 {
        self.chunk.duration_ms()
    }

    #[getter]
    fn frames(&self) -> u32 {
        self.chunk.meta.frames
    }

    /// ``start_ms * sample_rate // 1000``.
    #[getter]
    fn start_sample(&self) -> u64 {
        self.start_ms
            .saturating_mul(u64::from(self.chunk.meta.format.rate.hz()))
            / 1000
    }

    /// Connection receive timeline (same clock as ``VoiceEvent`` ``timeline_ms``).
    #[getter]
    fn timeline_ms(&self) -> u64 {
        self.chunk.meta.timeline_ms
    }

    /// ``time.monotonic()`` of the tick that produced the first sample.
    #[getter]
    fn captured_at(&self, py: Python<'_>) -> f64 {
        monotonic(py, self.chunk.meta.captured_at)
    }

    /// Any voiced audio in this chunk.
    #[getter]
    fn speaking(&self) -> bool {
        self.chunk.meta.speaking
    }

    /// Voiced users contributing.
    #[getter]
    #[gen_stub(override_return_type(type_repr = "frozenset[int]"))]
    fn speakers<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyFrozenSet>> {
        PyFrozenSet::new(py, self.chunk.meta.speakers.iter())
    }

    #[getter]
    fn voiced_ms(&self) -> u32 {
        self.chunk.meta.voiced_ms
    }

    #[getter]
    fn concealed_ms(&self) -> u32 {
        self.chunk.meta.concealed_ms
    }

    /// Synthetic silence (no Discord audio flow: link down, reconnect, stall).
    #[getter]
    fn gap_ms(&self) -> u32 {
        self.chunk.meta.gap_ms
    }

    /// Audio dropped by overflow immediately before this chunk.
    #[getter]
    fn dropped_ms(&self) -> u64 {
        self.dropped_ms
    }

    #[getter]
    fn rms_dbfs(&self) -> f32 {
        self.chunk.meta.level.rms_dbfs
    }

    #[getter]
    fn peak(&self) -> f32 {
        self.chunk.meta.level.peak
    }

    /// Payload length in bytes.
    fn __len__(&self) -> usize {
        self.chunk.data.len()
    }

    /// One copy of the payload.
    fn to_bytes<'py>(&self, py: Python<'py>) -> Bound<'py, PyBytes> {
        PyBytes::new(py, self.chunk.data.as_bytes())
    }

    /// ``[arrow]`` Zero-copy Arrow export (``Int16Array`` / ``Float32Array`` / ``UInt8Array``).
    ///
    /// Raises
    /// ------
    /// SongbirdError
    ///     Built without the ``arrow`` feature.
    #[pyo3(signature = (requested_schema = None))]
    #[gen_stub(override_return_type(type_repr = "tuple[object, object]"))]
    fn __arrow_c_array__<'py>(
        &self,
        py: Python<'py>,
        #[gen_stub(override_type(type_repr = "object | None"))] requested_schema: Option<
            Bound<'py, PyAny>,
        >,
    ) -> PyResult<Bound<'py, PyAny>> {
        crate::py::arrow::export_chunk(py, &self.chunk, requested_schema)
    }

    #[gen_stub(skip)]
    unsafe fn __getbuffer__(
        slf: Bound<'_, Self>,
        view: *mut ffi::Py_buffer,
        flags: c_int,
    ) -> PyResult<()> {
        if view.is_null() {
            return Err(PyBufferError::new_err("view is null"));
        }
        if (flags & ffi::PyBUF_WRITABLE) == ffi::PyBUF_WRITABLE {
            return Err(PyBufferError::new_err("AudioChunk is read-only"));
        }
        let this = slf.get();
        let bytes = this.chunk.data.as_bytes();
        let len = isize::try_from(bytes.len())
            .map_err(|_| PyBufferError::new_err("payload too large"))?;
        // Typed view ('h'/'f'/'B') when the consumer asks for the format; plain bytes otherwise.
        let (format, itemsize) = if (flags & ffi::PyBUF_FORMAT) == ffi::PyBUF_FORMAT {
            let (f, n) = this.item();
            (f.as_ptr().cast_mut(), n)
        } else {
            (ptr::null_mut(), 1)
        };
        // shape[0], strides[0]; freed in __releasebuffer__.
        let dims = Box::into_raw(Box::new([len / itemsize, itemsize])).cast::<isize>();
        // SAFETY: `view` is non-null and owned by the caller for the duration of the export; the
        // payload is immutable and kept alive by `view.obj = self` (a new strong reference).
        unsafe {
            (*view).obj = slf.clone().into_any().into_ptr();
            (*view).buf = bytes.as_ptr().cast_mut().cast::<c_void>();
            (*view).len = len;
            (*view).readonly = 1;
            (*view).itemsize = itemsize;
            (*view).format = format;
            (*view).ndim = 1;
            (*view).shape = if (flags & ffi::PyBUF_ND) == ffi::PyBUF_ND {
                dims
            } else {
                ptr::null_mut()
            };
            (*view).strides = if (flags & ffi::PyBUF_STRIDES) == ffi::PyBUF_STRIDES {
                dims.add(1)
            } else {
                ptr::null_mut()
            };
            (*view).suboffsets = ptr::null_mut();
            (*view).internal = dims.cast::<c_void>();
        }
        Ok(())
    }

    #[gen_stub(skip)]
    unsafe fn __releasebuffer__(&self, view: *mut ffi::Py_buffer) {
        if view.is_null() {
            return;
        }
        // SAFETY: `internal` was set by `__getbuffer__` to a leaked `Box<[isize; 2]>`.
        unsafe {
            let dims = (*view).internal.cast::<[isize; 2]>();
            if !dims.is_null() {
                drop(Box::from_raw(dims));
                (*view).internal = ptr::null_mut();
            }
        }
    }

    fn __repr__(&self) -> String {
        format!(
            "<AudioChunk user_id={:?} seq={} start_ms={} frames={} bytes={}>",
            self.chunk.meta.user_id,
            self.seq,
            self.start_ms,
            self.chunk.meta.frames,
            self.chunk.data.len()
        )
    }
}
