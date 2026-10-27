//! Python → Rust audio input conversion (§7.3, I8). The only module that touches buffers.
//!
//! * `bytes` is borrowed as `&[u8]` (zero copies): a plain borrow of an immutable object, released
//!   by a decref after the sbcore call returns. `str` is never audio (`TypeError`).
//! * Any other buffer-protocol object must be C-contiguous and its format code must match the stream
//!   encoding. Its size is checked against the caller's limit **before** anything is allocated
//!   (`LimitExceeded`, or `InvalidAudio` where the limit is a format rule such as the 1275-byte Opus
//!   packet), then it is copied once into an owned `Vec<u8>` (`try_reserve` → `MemoryError`), and the
//!   buffer is released **before** returning, so no `PyBuffer` ever overlaps a Rust lock.
//! * Every extraction takes a [`MaxBytes`]: there is no unbounded copy of a user buffer.
//! * An `AudioChunk` carries its own format, which must equal the stream format (`InvalidAudio`).

use std::ffi::{CStr, c_char};
use std::ops::RangeInclusive;

use pyo3::exceptions::{PyMemoryError, PyTypeError};
use pyo3::ffi;
use pyo3::prelude::*;
use pyo3::sync::critical_section::{with_critical_section, with_critical_section2};
use pyo3::types::{PyBytes, PyMemoryView, PyString};
use sbcore::limits::{LimitScalar, MAX_SOURCE_BYTES, check};
use sbcore::tx::opus_stream::{MAX_OPUS_PACKET, OPUS_PACKET_SIZE_MSG};
use sbcore::{Encoding, Format, LimitError, LimitNum};

use crate::errors::{CoreResultExt, Exc, limit_exc, msg_exc};
use crate::py::chunk::AudioChunk;

/// The largest buffer a call accepts, checked before any copy.
#[derive(Clone, Copy, Debug)]
pub enum MaxBytes {
    /// A size limit: a larger buffer raises `LimitExceeded` naming `what`.
    Limit { what: &'static str, max: u64 },
    /// A format rule (e.g. the 1275-byte Opus packet): a larger buffer raises `InvalidAudio(msg)`,
    /// the same error sbcore raises for a borrowed `bytes` of that size.
    Invalid { msg: &'static str, max: u64 },
}

impl MaxBytes {
    /// `LimitExceeded(what)` above `max` bytes.
    pub const fn limit(what: &'static str, max: usize) -> Self {
        MaxBytes::Limit {
            what,
            max: max as u64,
        }
    }

    /// `InvalidAudio(msg)` above `max` bytes.
    pub const fn invalid(msg: &'static str, max: usize) -> Self {
        MaxBytes::Invalid {
            msg,
            max: max as u64,
        }
    }

    fn check(self, py: Python<'_>, len: usize) -> PyResult<()> {
        let len = len as u64;
        match self {
            MaxBytes::Limit { max, .. } | MaxBytes::Invalid { max, .. } if len <= max => Ok(()),
            MaxBytes::Limit { what, max } => Err(limit_exc(
                py,
                &LimitError {
                    what,
                    value: LimitNum::Int(i128::from(len)),
                    min: LimitNum::Int(0),
                    max: LimitNum::Int(i128::from(max)),
                },
            )),
            MaxBytes::Invalid { msg, .. } => Err(msg_exc(py, Exc::InvalidAudio, msg)),
        }
    }
}

/// One Opus packet (`OpusSink.send`, each `OpusPackets` item): over 1275 bytes is `InvalidAudio`.
pub const OPUS_PACKET_MAX: MaxBytes = MaxBytes::invalid(OPUS_PACKET_SIZE_MSG, MAX_OPUS_PACKET);

/// An in-memory source (`AudioFile`, `RawPcm`): over 1 GiB is `LimitExceeded("source_bytes")`.
pub const SOURCE_MAX: MaxBytes = MaxBytes::limit("source_bytes", MAX_SOURCE_BYTES);

/// Audio handed to sbcore: borrowed bytes or an owned copy of a buffer.
#[derive(Debug)]
pub enum AudioData<'a> {
    Bytes(&'a [u8]),
    Owned(Vec<u8>),
}

impl AudioData<'_> {
    /// The raw bytes.
    pub fn bytes(&self) -> &[u8] {
        match self {
            AudioData::Bytes(b) => b,
            AudioData::Owned(v) => v,
        }
    }
}

/// Whether a buffer format code may be written to a stream with `encoding`: `B`/`b`/`c` (raw bytes)
/// for every encoding, `h` only for pcm16le, `f` only for f32le; byte-order prefixes `<` (and `=`/`@`
/// on little-endian targets) are accepted, `>`/`!` are not.
pub fn format_allowed(format: &str, encoding: Option<Encoding>) -> bool {
    let native_le = cfg!(target_endian = "little");
    let code = match format.as_bytes().first() {
        Some(b'<') => format.get(1..),
        Some(b'=' | b'@') if native_le => format.get(1..),
        Some(b'>' | b'!' | b'=' | b'@') => None,
        _ => Some(format),
    };
    matches!(
        (code, encoding),
        (Some("B" | "b" | "c"), _)
            | (Some("h"), Some(Encoding::Pcm16Le))
            | (Some("f"), Some(Encoding::F32Le))
    )
}

fn format_str(f: &CStr) -> String {
    f.to_string_lossy().into_owned()
}

/// Copies a C-contiguous buffer into an owned `Vec<u8>` after checking its format against
/// `encoding` and its size against `max` (both before allocating), releasing the buffer while still
/// attached.
///
/// The exporter's memory may be written by another thread while it is copied (a `bytearray` on the
/// free-threaded build, or any buffer written without the GIL), so Rust never forms a slice over it:
/// CPython's `PyBuffer_ToContiguous` copies it into our allocation, inside the critical section of
/// the exporter (and, for a `memoryview`, of the object it views). On the free-threaded build the
/// copy is therefore one whole state with respect to that object's own mutators (`bytearray` slice
/// assignment, …). Writes that take no lock (e.g. NumPy) may still leave a mix of old and new
/// bytes, exactly as `bytes(buffer)` would, but never undefined behaviour.
fn copy_buffer(
    py: Python<'_>,
    obj: &Bound<'_, PyAny>,
    encoding: Option<Encoding>,
    max: MaxBytes,
) -> PyResult<Vec<u8>> {
    let buf = Export::get(obj)
        .ok_or_else(|| PyTypeError::new_err("expected bytes or a buffer-protocol object"))?;
    let fmt = format_str(buf.format());
    if !format_allowed(&fmt, encoding) {
        let enc = encoding.map_or("raw bytes", Encoding::as_str);
        return Err(msg_exc(
            py,
            Exc::InvalidAudio,
            &format!("buffer format {fmt:?} does not match the stream encoding ({enc})"),
        ));
    }
    if !buf.is_c_contiguous() {
        return Err(msg_exc(
            py,
            Exc::InvalidAudio,
            "buffer must be C-contiguous",
        ));
    }
    let len = buf.len_bytes();
    max.check(py, len)?;
    let mut out: Vec<u8> = Vec::new();
    out.try_reserve_exact(len)
        .map_err(|_| PyMemoryError::new_err("allocation failed"))?;
    if len > 0 {
        let base = viewed_object(obj);
        let copy = || {
            // SAFETY: `out` has room for `len` bytes, and `buf` is a live C-contiguous export of
            // exactly `len` bytes; CPython reads the exporter's memory, Rust only writes `out`.
            unsafe {
                ffi::PyBuffer_ToContiguous(
                    out.as_mut_ptr().cast(),
                    buf.raw(),
                    buf.raw().len,
                    b'C' as c_char,
                )
            }
        };
        let rc = match &base {
            Some(base) => with_critical_section2(obj, base, copy),
            None => with_critical_section(obj, copy),
        };
        if rc != 0 {
            return Err(PyErr::fetch(py));
        }
        // SAFETY: `PyBuffer_ToContiguous` succeeded, so it wrote all `len` bytes.
        unsafe { out.set_len(len) };
    }
    Ok(out)
}

/// The object a `memoryview` views (its `obj`), whose own mutators lock that object, not the view.
fn viewed_object<'py>(obj: &Bound<'py, PyAny>) -> Option<Bound<'py, PyAny>> {
    let view = obj.cast::<PyMemoryView>().ok()?;
    let base = view.getattr("obj").ok()?;
    (!base.is_none() && !base.is(obj)).then_some(base)
}

/// A `Py_buffer` export, released on drop. It holds `Python<'py>`, so it cannot outlive the
/// attachment `PyBuffer_Release` needs, and it is boxed because exporters may point the struct's
/// `shape`/`strides` into itself (`PyBuffer_FillInfo`), so it must never move.
struct Export<'py> {
    view: Box<ffi::Py_buffer>,
    _py: Python<'py>,
}

impl<'py> Export<'py> {
    /// `None` (the Python error cleared) if `obj` does not export a buffer.
    fn get(obj: &Bound<'py, PyAny>) -> Option<Self> {
        let mut view = Box::<ffi::Py_buffer>::new_uninit();
        // SAFETY: `view` is writable memory for one `Py_buffer`, filled when the call returns 0.
        let rc =
            unsafe { ffi::PyObject_GetBuffer(obj.as_ptr(), view.as_mut_ptr(), ffi::PyBUF_FULL_RO) };
        if rc != 0 {
            drop(PyErr::take(obj.py()));
            return None;
        }
        Some(Self {
            // SAFETY: initialised by the successful `PyObject_GetBuffer` above.
            view: unsafe { view.assume_init() },
            _py: obj.py(),
        })
    }

    fn raw(&self) -> &ffi::Py_buffer {
        &self.view
    }

    fn format(&self) -> &CStr {
        if self.view.format.is_null() {
            c"B"
        } else {
            // SAFETY: a non-null `format` is a NUL-terminated string owned by the live export.
            unsafe { CStr::from_ptr(self.view.format) }
        }
    }

    fn len_bytes(&self) -> usize {
        usize::try_from(self.view.len).unwrap_or(0)
    }

    fn is_c_contiguous(&self) -> bool {
        // SAFETY: `view` is a live export.
        unsafe { ffi::PyBuffer_IsContiguous(self.raw(), b'C' as c_char) != 0 }
    }
}

impl Drop for Export<'_> {
    fn drop(&mut self) {
        // SAFETY: the export is live, released exactly once, and the thread is attached (`'py`).
        unsafe { ffi::PyBuffer_Release(&mut *self.view) };
    }
}

/// Extracts audio for a stream with `encoding`: `bytes` (borrowed) or any other C-contiguous
/// buffer whose format matches (copied). Either is size-checked against `max` first; `str` raises
/// `TypeError`.
pub fn extract_audio<'a>(
    obj: &'a Bound<'_, PyAny>,
    encoding: Encoding,
    max: MaxBytes,
) -> PyResult<AudioData<'a>> {
    if let Ok(b) = obj.cast::<PyBytes>() {
        let b = b.as_bytes();
        max.check(obj.py(), b.len())?;
        return Ok(AudioData::Bytes(b));
    }
    if obj.is_instance_of::<PyString>() {
        return Err(PyTypeError::new_err(
            "audio must be a bytes-like object, not str",
        ));
    }
    copy_buffer(obj.py(), obj, Some(encoding), max).map(AudioData::Owned)
}

/// `InvalidAudio` if `obj` is an `AudioChunk` whose format is not `fmt` (its buffer format code
/// alone cannot tell, e.g., a 24 kHz mono chunk from a 48 kHz stereo one). Other objects pass.
pub fn check_chunk_format(obj: &Bound<'_, PyAny>, fmt: Format) -> PyResult<()> {
    let Ok(chunk) = obj.cast::<AudioChunk>() else {
        return Ok(());
    };
    let got = chunk.get().chunk_format();
    if got == fmt {
        return Ok(());
    }
    let show = |f: Format| format!("{}/{}/{}", f.encoding.as_str(), f.rate.hz(), f.channels);
    Err(msg_exc(
        obj.py(),
        Exc::InvalidAudio,
        &format!(
            "AudioChunk format {} does not match the stream format {}",
            show(got),
            show(fmt)
        ),
    ))
}

/// Extracts raw (encoded) bytes: `bytes` (borrowed) or a C-contiguous byte buffer (copied). Either
/// is size-checked against `max` first, so an oversized buffer is never copied.
pub fn extract_bytes<'a>(obj: &'a Bound<'_, PyAny>, max: MaxBytes) -> PyResult<AudioData<'a>> {
    if let Ok(b) = obj.cast::<PyBytes>() {
        let b = b.as_bytes();
        max.check(obj.py(), b.len())?;
        return Ok(AudioData::Bytes(b));
    }
    copy_buffer(obj.py(), obj, None, max).map(AudioData::Owned)
}

/// A Python integer argument checked against `range` **before** narrowing to `T`, so an out-of-range
/// value of any size raises `LimitExceeded` (a `ValueError`), never `OverflowError` (§10).
pub fn int_arg<T>(
    py: Python<'_>,
    what: &'static str,
    v: i64,
    range: RangeInclusive<T>,
) -> PyResult<T>
where
    T: LimitScalar + TryFrom<i64>,
{
    match T::try_from(v) {
        Ok(t) => check(what, t, range).py(py),
        Err(_) => Err(limit_exc(
            py,
            &LimitError {
                what,
                value: LimitNum::Int(i128::from(v)),
                min: range.start().num(),
                max: range.end().num(),
            },
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_codes() {
        for enc in [
            Encoding::Pcm16Le,
            Encoding::F32Le,
            Encoding::MuLaw,
            Encoding::ALaw,
        ] {
            assert!(format_allowed("B", Some(enc)));
            assert!(format_allowed("<b", Some(enc)));
            assert!(format_allowed("c", Some(enc)));
            assert!(!format_allowed(">h", Some(enc)));
            assert!(!format_allowed("i", Some(enc)));
            assert!(!format_allowed("d", Some(enc)));
        }
        assert!(format_allowed("h", Some(Encoding::Pcm16Le)));
        assert!(format_allowed("<h", Some(Encoding::Pcm16Le)));
        assert!(!format_allowed("h", Some(Encoding::F32Le)));
        assert!(format_allowed("f", Some(Encoding::F32Le)));
        assert!(!format_allowed("<f", Some(Encoding::Pcm16Le)));
        assert!(!format_allowed("!h", Some(Encoding::Pcm16Le)));
        assert!(!format_allowed("h", None));
        assert!(format_allowed("B", None));
        if cfg!(target_endian = "little") {
            assert!(format_allowed("=h", Some(Encoding::Pcm16Le)));
            assert!(format_allowed("@f", Some(Encoding::F32Le)));
        }
    }
}
