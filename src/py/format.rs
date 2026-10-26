//! `native.AudioFormat` (§9.2): the native part of the Python `AudioFormat`.

use pyo3::prelude::*;
use pyo3::types::{PyBytes, PyDict, PyType};
use pyo3_stub_gen::derive::{gen_stub_pyclass, gen_stub_pymethods};
use sbcore::dsp::b64;
use sbcore::limits::{SILENCE_MS, check};
use sbcore::{CoreError, Encoding, Format};

use crate::errors::{CoreResultExt, Exc, ensure_not_forked, msg_exc};

/// Immutable raw-audio format. Little-endian, interleaved.
///
/// Parameters
/// ----------
/// sample_rate : int, default 24000
///     One of 8000, 16000, 24000, 48000.
/// channels : int, default 1
///     1 or 2.
/// encoding : {"pcm16le", "f32le", "mulaw", "alaw"}, default "pcm16le"
///     ``"mulaw"``/``"alaw"`` (G.711) require 8000 Hz mono.
///
/// Raises
/// ------
/// InvalidAudio
///     For any other combination.
#[gen_stub_pyclass]
#[pyclass(
    module = "discord.ext.songbird.native",
    name = "AudioFormat",
    frozen,
    eq,
    hash,
    from_py_object
)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct PyAudioFormat {
    pub fmt: Format,
}

impl PyAudioFormat {
    pub fn new_from(fmt: Format) -> Self {
        PyAudioFormat { fmt }
    }

    pub fn format(&self) -> Format {
        self.fmt
    }
}

fn invalid(py: Python<'_>, msg: &str) -> PyErr {
    msg_exc(py, Exc::InvalidAudio, msg)
}

#[gen_stub_pymethods]
#[pymethods]
impl PyAudioFormat {
    #[new]
    #[pyo3(signature = (sample_rate = 24_000, channels = 1, encoding = "pcm16le"))]
    fn py_new(
        py: Python<'_>,
        sample_rate: i64,
        channels: i64,
        #[gen_stub(override_type(type_repr = "typing.Literal['pcm16le', 'f32le', 'mulaw', 'alaw']", imports = ("typing",)))]
        encoding: &str,
    ) -> PyResult<Self> {
        ensure_not_forked(py)?;
        let enc = Encoding::parse(encoding).ok_or_else(|| {
            invalid(
                py,
                &format!("unknown encoding {encoding:?}; expected pcm16le, f32le, mulaw or alaw"),
            )
        })?;
        let rate = u32::try_from(sample_rate).map_err(|_| {
            invalid(
                py,
                &format!("sample_rate must be 8000, 16000, 24000 or 48000 (got {sample_rate})"),
            )
        })?;
        let ch = u8::try_from(channels)
            .map_err(|_| invalid(py, &format!("channels must be 1 or 2 (got {channels})")))?;
        Format::new(rate, ch, enc)
            .py(py)
            .map(PyAudioFormat::new_from)
    }

    /// Parse a format specification.
    ///
    /// Parameters
    /// ----------
    /// spec : str
    ///     ``'pcm16_24k'``, ``'pcm16_48k_stereo'``, ``'f32_48k'``, ``'mulaw'``, ``'alaw'``,
    ///     ``'pcm16le/24000/1'``, a preset name such as ``'GPT_LIVE'``, or a MIME type as returned
    ///     by :attr:`mime_type`.
    ///
    /// Returns
    /// -------
    /// AudioFormat
    ///     The parsed format.
    ///
    /// Raises
    /// ------
    /// InvalidAudio
    ///     If the specification is not recognised or the combination is invalid.
    #[classmethod]
    fn parse(_cls: &Bound<'_, PyType>, py: Python<'_>, spec: &str) -> PyResult<Self> {
        ensure_not_forked(py)?;
        Format::parse(spec).py(py).map(PyAudioFormat::new_from)
    }

    /// Sample rate in Hz.
    #[getter]
    fn sample_rate(&self) -> u32 {
        self.fmt.rate.hz()
    }

    /// Number of interleaved channels (1 or 2).
    #[getter]
    fn channels(&self) -> u8 {
        self.fmt.channels
    }

    /// Wire encoding.
    #[getter]
    #[gen_stub(override_return_type(type_repr = "typing.Literal['pcm16le', 'f32le', 'mulaw', 'alaw']", imports = ("typing",)))]
    fn encoding(&self) -> &'static str {
        self.fmt.encoding.as_str()
    }

    /// Bytes per sample: 2 (pcm16le), 4 (f32le), 1 (G.711).
    #[getter]
    fn sample_width(&self) -> usize {
        self.fmt.sample_width()
    }

    /// ``sample_width * channels``.
    #[getter]
    fn frame_bytes(&self) -> usize {
        self.fmt.frame_bytes()
    }

    /// Exact bytes per millisecond (48 for PCM16 24 kHz mono).
    #[getter]
    fn bytes_per_ms(&self) -> usize {
        self.fmt.bytes_per_ms()
    }

    /// MIME type: ``'audio/pcm;rate=24000'``, ``'audio/pcmu'``, ``'audio/pcma'`` or
    /// ``'audio/x-f32le;rate=48000'`` (stereo formats append ``';channels=2'``).
    #[getter]
    fn mime_type(&self) -> String {
        self.fmt.mime_type().into_owned()
    }

    /// OpenAI session format object.
    ///
    /// Returns
    /// -------
    /// dict[str, object]
    ///     ``{'type': 'audio/pcm', 'rate': R}`` (mono PCM16 at 16/24 kHz), ``{'type': 'audio/pcmu'}``
    ///     or ``{'type': 'audio/pcma'}``.
    ///
    /// Raises
    /// ------
    /// InvalidAudio
    ///     For any other format.
    #[getter]
    #[gen_stub(override_return_type(type_repr = "dict[str, object]"))]
    fn openai_format<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyDict>> {
        let (ty, rate) = self.fmt.openai_format().ok_or_else(|| {
            invalid(
                py,
                "OpenAI formats are mono pcm16le at 16000/24000 Hz, mulaw or alaw",
            )
        })?;
        let d = PyDict::new(py);
        d.set_item("type", ty)?;
        if let Some(r) = rate {
            d.set_item("rate", r)?;
        }
        Ok(d)
    }

    /// Exact byte length of ``ms`` milliseconds.
    ///
    /// Parameters
    /// ----------
    /// ms : int
    ///     Duration in milliseconds (``0 <= ms <= 2**32 - 1``).
    ///
    /// Returns
    /// -------
    /// int
    ///     ``ms * bytes_per_ms``.
    fn bytes_for(&self, py: Python<'_>, ms: i64) -> PyResult<usize> {
        let ms = check("ms", ms, 0..=i64::from(u32::MAX)).py(py)?;
        let ms = u32::try_from(ms).unwrap_or(u32::MAX);
        self.fmt.bytes_for_ms(ms).py(py)
    }

    /// Duration of ``nbytes`` bytes.
    ///
    /// Parameters
    /// ----------
    /// nbytes : int
    ///     Byte count (``>= 0``).
    ///
    /// Returns
    /// -------
    /// float
    ///     Milliseconds.
    fn ms_for(&self, py: Python<'_>, nbytes: i64) -> PyResult<f64> {
        let n = check("nbytes", nbytes, 0..=i64::MAX).py(py)?;
        Ok(n as f64 / self.fmt.bytes_per_ms() as f64)
    }

    /// ``ms`` of digital silence (0x00; µ-law 0xFF; A-law 0xD5).
    ///
    /// Parameters
    /// ----------
    /// ms : int
    ///     ``0 <= ms <= 600_000``, checked before any arithmetic.
    ///
    /// Returns
    /// -------
    /// bytes
    ///     ``bytes_for(ms)`` bytes of silence.
    ///
    /// Raises
    /// ------
    /// LimitExceeded
    ///     If ``ms`` is out of range.
    /// MemoryError
    ///     If the allocation fails.
    fn silence<'py>(&self, py: Python<'py>, ms: i64) -> PyResult<Bound<'py, PyBytes>> {
        let n = silence_len(self.fmt, ms).py(py)?;
        let byte = self.fmt.silence_byte();
        PyBytes::new_with(py, n, |buf| {
            buf.fill(byte);
            Ok(())
        })
    }

    /// Base64 of :meth:`silence`, encoded in Rust. Same limit.
    ///
    /// Parameters
    /// ----------
    /// ms : int
    ///     ``0 <= ms <= 600_000``.
    ///
    /// Returns
    /// -------
    /// str
    ///     Standard padded base64.
    fn silence_base64(&self, py: Python<'_>, ms: i64) -> PyResult<String> {
        let n = silence_len(self.fmt, ms).py(py)?;
        silence_base64(self.fmt.silence_byte(), n).py(py)
    }

    fn __repr__(&self) -> String {
        format!(
            "AudioFormat(sample_rate={}, channels={}, encoding='{}')",
            self.fmt.rate.hz(),
            self.fmt.channels,
            self.fmt.encoding.as_str()
        )
    }
}

/// Byte length of `ms` of silence, validated against `SILENCE_MS` before any arithmetic.
fn silence_len(fmt: Format, ms: i64) -> Result<usize, CoreError> {
    let max = i64::try_from(*SILENCE_MS.end()).unwrap_or(i64::MAX);
    let min = i64::try_from(*SILENCE_MS.start()).unwrap_or(0);
    let ms = check("ms", ms, min..=max)?;
    let n = sbcore::limits::mul_checked(
        "ms",
        u64::try_from(ms).unwrap_or(0),
        fmt.bytes_per_ms() as u64,
    )?;
    usize::try_from(n).map_err(|_| CoreError::Alloc)
}

/// Base64 of `n` copies of `byte` without materialising the raw bytes: every 3 identical input bytes
/// encode to the same 4 characters.
fn silence_base64(byte: u8, n: usize) -> Result<String, CoreError> {
    let len = b64::encoded_len(n).ok_or(CoreError::Alloc)?;
    let mut out = String::new();
    out.try_reserve_exact(len)?;
    let mut group = String::new();
    b64::encode_into(&[byte; 3], &mut group);
    for _ in 0..n / 3 {
        out.push_str(&group);
    }
    b64::encode_into(&[byte, byte][..n % 3], &mut out);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn silence_base64_matches_direct_encoding() {
        for byte in [0x00u8, 0xFF, 0xD5] {
            for n in 0..40usize {
                let mut direct = String::new();
                b64::encode_into(&vec![byte; n], &mut direct);
                assert_eq!(silence_base64(byte, n).unwrap(), direct, "{byte:#x} {n}");
            }
        }
    }

    #[test]
    fn silence_len_limits() {
        let f = sbcore::format::PCM16_48K;
        assert_eq!(silence_len(f, 0).unwrap(), 0);
        assert_eq!(silence_len(f, 600_000).unwrap(), 600_000 * 96);
        assert!(matches!(silence_len(f, 600_001), Err(CoreError::Limit(_))));
        assert!(matches!(silence_len(f, -1), Err(CoreError::Limit(_))));
        assert!(matches!(
            silence_len(f, 1_000_000_000_000),
            Err(CoreError::Limit(_))
        ));
    }
}
