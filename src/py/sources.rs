//! `native.AudioFile`, `native.RawPcm` and `native.OpusPackets` (W4, §7.9, §9.8): reusable
//! in-memory sources. Input is copied once into Rust-owned memory and released **before** the
//! (possibly detached) sbcore call (I8).

use std::path::PathBuf;
use std::sync::Arc;

use pyo3::exceptions::{PyMemoryError, PyTypeError};
use pyo3::prelude::*;
use pyo3::types::{PyString, PyType};
use pyo3_stub_gen::derive::{gen_stub_pyclass, gen_stub_pymethods};
use sbcore::limits::{MAX_OPUS_PACKETS, MAX_SOURCE_BYTES, SOURCE_RATE_HZ};
use sbcore::tx::memory::{EncodedBytes, MemPcm, OpusFrames, read_file};
use sbcore::tx::source::SourceSpec;
use sbcore::{Encoding, Format, LimitError, LimitNum};

use crate::convert::{AudioData, extract_audio, extract_bytes, int_arg};
use crate::errors::{CoreResultExt, Exc, ensure_not_forked, limit_exc, msg_exc};
use crate::py::arrow::{SampleKind, import_binary, import_bytes, import_samples};
use crate::py::format::PyAudioFormat;

/// Inputs above this size are parsed with the thread state detached (§5.2).
const DETACH_ABOVE: usize = 1 << 20;

/// `AudioFormat | str` → `Format`.
fn format_arg(obj: &Bound<'_, PyAny>) -> PyResult<Format> {
    if let Ok(f) = obj.extract::<PyAudioFormat>() {
        return Ok(f.format());
    }
    if let Ok(s) = obj.cast::<PyString>() {
        return Format::parse(s.to_str()?).py(obj.py());
    }
    Err(PyTypeError::new_err(
        "format must be an AudioFormat or a str",
    ))
}

/// An owned copy of raw (non-base64) audio bytes.
fn owned(py: Python<'_>, data: AudioData<'_>) -> PyResult<Vec<u8>> {
    match data {
        AudioData::Owned(v) => Ok(v),
        AudioData::Bytes(b) => {
            let mut v = Vec::new();
            v.try_reserve_exact(b.len())
                .map_err(|_| PyMemoryError::new_err("allocation failed"))?;
            v.extend_from_slice(b);
            Ok(v)
        }
        AudioData::Base64(_) => Err(msg_exc(
            py,
            Exc::InvalidAudio,
            "expected a bytes-like object, not str",
        )),
    }
}

/// `AudioFile | RawPcm | OpusPackets` → `SourceSpec`.
pub(crate) fn source_spec(obj: &Bound<'_, PyAny>) -> PyResult<SourceSpec> {
    if let Ok(f) = obj.cast::<AudioFile>() {
        return Ok(SourceSpec::File(f.get().b.clone()));
    }
    if let Ok(p) = obj.cast::<RawPcm>() {
        return Ok(SourceSpec::Pcm(p.get().p.clone()));
    }
    if let Ok(o) = obj.cast::<OpusPackets>() {
        return Ok(SourceSpec::Opus(o.get().f.clone()));
    }
    Err(PyTypeError::new_err(
        "source must be an AudioFile, RawPcm or OpusPackets",
    ))
}

/// Encoded audio held in memory (copied once, probed eagerly; the container is detected from
/// content). Seekable, loopable, reusable.
///
/// Parameters
/// ----------
/// data : Buffer
///     The encoded file contents.
///
/// Raises
/// ------
/// SourceError
///     The data is not a recognised format.
#[gen_stub_pyclass]
#[pyclass(module = "discord.ext.songbird.native", frozen)]
pub struct AudioFile {
    b: Arc<EncodedBytes>,
}

#[gen_stub_pymethods]
#[pymethods]
impl AudioFile {
    #[new]
    fn py_new(
        py: Python<'_>,
        #[gen_stub(override_type(type_repr = "collections.abc.Buffer", imports = ("collections.abc",)))]
        data: &Bound<'_, PyAny>,
    ) -> PyResult<Self> {
        ensure_not_forked(py)?;
        let bytes = owned(py, extract_bytes(data)?)?;
        let b = py.detach(|| EncodedBytes::new(bytes)).py(py)?;
        Ok(AudioFile { b: Arc::new(b) })
    }

    /// Reads a file (blocking; the thread state is detached). On the event loop prefer
    /// ``await AudioFile.load(...)``.
    ///
    /// Parameters
    /// ----------
    /// path : str or os.PathLike
    /// max_bytes : int, default 256 MiB
    ///     ``1..=1 GiB``.
    ///
    /// Raises
    /// ------
    /// OSError
    ///     The file cannot be read (``FileNotFoundError``, ``PermissionError``, …).
    /// LimitExceeded
    ///     The file is larger than ``max_bytes``.
    /// SourceError
    ///     The data is not a recognised format.
    #[classmethod]
    #[pyo3(signature = (path, *, max_bytes = 256 << 20))]
    fn from_path(
        _cls: &Bound<'_, PyType>,
        py: Python<'_>,
        path: PathBuf,
        max_bytes: i64,
    ) -> PyResult<Self> {
        ensure_not_forked(py)?;
        let max_bytes = int_arg(py, "max_bytes", max_bytes, 1..=MAX_SOURCE_BYTES)?;
        let b = py
            .detach(|| read_file(&path, max_bytes).and_then(EncodedBytes::new))
            .py(py)?;
        Ok(AudioFile { b: Arc::new(b) })
    }

    /// ``[arrow]`` A ``UInt8`` array, or a Binary/LargeBinary array holding exactly one value.
    #[classmethod]
    fn from_arrow(
        _cls: &Bound<'_, PyType>,
        py: Python<'_>,
        #[gen_stub(override_type(type_repr = "object"))] array: &Bound<'_, PyAny>,
    ) -> PyResult<Self> {
        ensure_not_forked(py)?;
        let bytes = import_bytes(py, array).py(py)?;
        let b = py.detach(|| EncodedBytes::new(bytes)).py(py)?;
        Ok(AudioFile { b: Arc::new(b) })
    }

    /// Size of the encoded data in bytes.
    #[getter]
    fn nbytes(&self) -> usize {
        self.b.len()
    }

    #[getter]
    fn codec(&self) -> String {
        self.b.info().codec.clone()
    }

    #[getter]
    fn sample_rate(&self) -> Option<u32> {
        self.b.info().sample_rate
    }

    #[getter]
    fn channels(&self) -> Option<u8> {
        self.b.info().channels
    }

    #[getter]
    fn duration_ms(&self) -> Option<u64> {
        self.b.info().duration_ms
    }
}

/// Raw PCM / G.711 in memory (whole frames; parsed once). Seekable, loopable, reusable.
///
/// Parameters
/// ----------
/// data : Buffer
///     Interleaved samples in ``format``.
/// format : AudioFormat or str
#[gen_stub_pyclass]
#[pyclass(module = "discord.ext.songbird.native", frozen)]
pub struct RawPcm {
    p: Arc<MemPcm>,
}

#[gen_stub_pymethods]
#[pymethods]
impl RawPcm {
    #[new]
    fn py_new(
        py: Python<'_>,
        #[gen_stub(override_type(type_repr = "collections.abc.Buffer", imports = ("collections.abc",)))]
        data: &Bound<'_, PyAny>,
        #[gen_stub(override_type(type_repr = "AudioFormat | str"))] format: &Bound<'_, PyAny>,
    ) -> PyResult<Self> {
        ensure_not_forked(py)?;
        let fmt = format_arg(format)?;
        let bytes = owned(py, extract_audio(data, fmt.encoding)?)?;
        let p = if bytes.len() > DETACH_ABOVE {
            py.detach(|| MemPcm::from_bytes(fmt, &bytes))
        } else {
            MemPcm::from_bytes(fmt, &bytes)
        }
        .py(py)?;
        Ok(RawPcm { p: Arc::new(p) })
    }

    /// ``[arrow]`` An Int16/Float32/UInt8 primitive array without nulls.
    #[classmethod]
    fn from_arrow(
        _cls: &Bound<'_, PyType>,
        py: Python<'_>,
        #[gen_stub(override_type(type_repr = "object"))] array: &Bound<'_, PyAny>,
        #[gen_stub(override_type(type_repr = "AudioFormat | str"))] format: &Bound<'_, PyAny>,
    ) -> PyResult<Self> {
        ensure_not_forked(py)?;
        let fmt = format_arg(format)?;
        let (_kind, bytes) = import_samples(py, array).py(py)?;
        let p = py.detach(|| MemPcm::from_bytes(fmt, &bytes)).py(py)?;
        Ok(RawPcm { p: Arc::new(p) })
    }

    /// Interleaved PCM at any rate ``8000..=192000`` Hz (a buffer, or an ``[arrow]`` Int16/Float32
    /// array); resampled once to 48 kHz unless the rate is 8/16/24/48 kHz.
    #[classmethod]
    #[pyo3(signature = (data, sample_rate, channels = 2, encoding = "f32le"))]
    fn from_samples(
        _cls: &Bound<'_, PyType>,
        py: Python<'_>,
        #[gen_stub(override_type(type_repr = "collections.abc.Buffer | object", imports = ("collections.abc",)))]
        data: &Bound<'_, PyAny>,
        sample_rate: i64,
        channels: i64,
        #[gen_stub(override_type(type_repr = "typing.Literal['pcm16le', 'f32le']", imports = ("typing",)))]
        encoding: &str,
    ) -> PyResult<Self> {
        ensure_not_forked(py)?;
        let sample_rate = int_arg(py, "sample_rate", sample_rate, SOURCE_RATE_HZ)?;
        let channels = int_arg(py, "channels", channels, 1..=2u8)?;
        let enc = match Encoding::parse(encoding) {
            Some(e @ (Encoding::Pcm16Le | Encoding::F32Le)) => e,
            _ => {
                return Err(msg_exc(
                    py,
                    Exc::InvalidAudio,
                    "from_samples encoding must be 'pcm16le' or 'f32le'",
                ));
            }
        };
        let is_buffer = data.cast::<pyo3::types::PyBytes>().is_ok()
            || data.hasattr("__buffer__").unwrap_or(false);
        let bytes = if !is_buffer && data.hasattr("__arrow_c_array__").unwrap_or(false) {
            let (kind, bytes) = import_samples(py, data).py(py)?;
            let ok = matches!(
                (kind, enc),
                (SampleKind::Int16, Encoding::Pcm16Le) | (SampleKind::Float32, Encoding::F32Le)
            );
            if !ok {
                return Err(msg_exc(
                    py,
                    Exc::InvalidAudio,
                    "the Arrow array type does not match the encoding",
                ));
            }
            bytes
        } else {
            owned(py, extract_audio(data, enc)?)?
        };
        let p = py
            .detach(|| MemPcm::from_any_rate(sample_rate, channels, enc, &bytes))
            .py(py)?;
        Ok(RawPcm { p: Arc::new(p) })
    }

    #[getter]
    fn format(&self) -> PyAudioFormat {
        PyAudioFormat::new_from(self.p.format())
    }

    #[getter]
    fn duration_ms(&self) -> u64 {
        self.p.duration_ms()
    }
}

/// Pre-encoded 20 ms / 48 kHz Opus packets; passthrough-capable; seekable by packet index.
///
/// Parameters
/// ----------
/// packets : Iterable[Buffer]
///
/// Raises
/// ------
/// InvalidAudio
///     A packet is not exactly 960 samples at 48 kHz or is larger than 1275 bytes.
#[gen_stub_pyclass]
#[pyclass(module = "discord.ext.songbird.native", frozen)]
pub struct OpusPackets {
    f: Arc<OpusFrames>,
}

fn opus_frames(py: Python<'_>, packets: Vec<Vec<u8>>) -> PyResult<OpusFrames> {
    OpusFrames::from_packets(packets.iter().map(Vec::as_slice)).py(py)
}

#[gen_stub_pymethods]
#[pymethods]
impl OpusPackets {
    #[new]
    fn py_new(
        py: Python<'_>,
        #[gen_stub(override_type(type_repr = "collections.abc.Iterable[collections.abc.Buffer]", imports = ("collections.abc",)))]
        packets: &Bound<'_, PyAny>,
    ) -> PyResult<Self> {
        ensure_not_forked(py)?;
        let mut out: Vec<Vec<u8>> = Vec::new();
        let mut total = 0usize;
        for item in packets.try_iter()? {
            let item = item?;
            let p = owned(py, extract_bytes(&item)?)?;
            total = total.saturating_add(p.len());
            if out.len() >= MAX_OPUS_PACKETS || total > MAX_SOURCE_BYTES {
                let (what, value, max) = if out.len() >= MAX_OPUS_PACKETS {
                    ("opus_packets", out.len() + 1, MAX_OPUS_PACKETS)
                } else {
                    ("source_bytes", total, MAX_SOURCE_BYTES)
                };
                return Err(limit_exc(
                    py,
                    &LimitError {
                        what,
                        value: LimitNum::Int(value as i128),
                        min: LimitNum::Int(0),
                        max: LimitNum::Int(max as i128),
                    },
                ));
            }
            out.try_reserve(1)
                .map_err(|_| PyMemoryError::new_err("allocation failed"))?;
            out.push(p);
        }
        let f = opus_frames(py, out)?;
        Ok(OpusPackets { f: Arc::new(f) })
    }

    /// ``[arrow]`` A Binary/LargeBinary array without nulls.
    #[classmethod]
    fn from_arrow(
        _cls: &Bound<'_, PyType>,
        py: Python<'_>,
        #[gen_stub(override_type(type_repr = "object"))] array: &Bound<'_, PyAny>,
    ) -> PyResult<Self> {
        ensure_not_forked(py)?;
        let packets = import_binary(py, array).py(py)?;
        let f = opus_frames(py, packets)?;
        Ok(OpusPackets { f: Arc::new(f) })
    }

    fn __len__(&self) -> usize {
        self.f.len()
    }

    #[getter]
    fn duration_ms(&self) -> u64 {
        self.f.duration_ms()
    }
}
