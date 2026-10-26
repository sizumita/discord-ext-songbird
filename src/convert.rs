//! Python → Rust audio input conversion (§7.3, I8). The only module that touches buffers.
//!
//! * `bytes` is borrowed as `&[u8]` (zero copies): a plain borrow of an immutable object, released
//!   by a decref after the sbcore call returns. `str` is never audio (`TypeError`).
//! * Any other buffer-protocol object must be C-contiguous and its format code must match the stream
//!   encoding. Its size is checked against the caller's limit **before** anything is allocated
//!   (`LimitExceeded`), then it is copied once into an owned `Vec<u8>` (`try_reserve` →
//!   `MemoryError`), and the buffer is released **before** returning, so no `PyBuffer` ever overlaps
//!   a Rust lock.
//! * An `AudioChunk` carries its own format, which must equal the stream format (`InvalidAudio`).

use std::ffi::CStr;
use std::ops::RangeInclusive;

use pyo3::buffer::PyUntypedBuffer;
use pyo3::exceptions::{PyMemoryError, PyTypeError};
use pyo3::prelude::*;
use pyo3::types::{PyBytes, PyString};
use sbcore::limits::{LimitScalar, check};
use sbcore::{Encoding, Format, LimitError, LimitNum};

use crate::errors::{CoreResultExt, Exc, limit_exc, msg_exc};
use crate::py::chunk::AudioChunk;

/// The largest buffer a call accepts, checked before any copy (`LimitExceeded` naming `what`).
#[derive(Clone, Copy, Debug)]
pub struct MaxBytes {
    pub what: &'static str,
    pub max: u64,
}

impl MaxBytes {
    fn check(self, py: Python<'_>, len: usize) -> PyResult<()> {
        let len = len as u64;
        if len <= self.max {
            return Ok(());
        }
        Err(limit_exc(
            py,
            &LimitError {
                what: self.what,
                value: LimitNum::Int(i128::from(len)),
                min: LimitNum::Int(0),
                max: LimitNum::Int(i128::from(self.max)),
            },
        ))
    }
}

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
pub fn copy_buffer(
    py: Python<'_>,
    obj: &Bound<'_, PyAny>,
    encoding: Option<Encoding>,
    max: Option<MaxBytes>,
) -> PyResult<Vec<u8>> {
    let buf = PyUntypedBuffer::get(obj)
        .map_err(|_| PyTypeError::new_err("expected bytes or a buffer-protocol object"))?;
    let result = (|| {
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
        if let Some(max) = max {
            max.check(py, len)?;
        }
        let mut out: Vec<u8> = Vec::new();
        out.try_reserve_exact(len)
            .map_err(|_| PyMemoryError::new_err("allocation failed"))?;
        if len > 0 {
            // SAFETY: the buffer is C-contiguous, exported for the whole lifetime of `buf`, and
            // `len_bytes()` is its exact size in bytes.
            let src =
                unsafe { std::slice::from_raw_parts(buf.buf_ptr().cast::<u8>().cast_const(), len) };
            out.extend_from_slice(src);
        }
        Ok(out)
    })();
    buf.release(py);
    result
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
    copy_buffer(obj.py(), obj, Some(encoding), Some(max)).map(AudioData::Owned)
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

/// Extracts raw (encoded) bytes: `bytes` (borrowed) or a C-contiguous byte buffer (copied).
pub fn extract_bytes<'a>(obj: &'a Bound<'_, PyAny>) -> PyResult<AudioData<'a>> {
    if let Ok(b) = obj.cast::<PyBytes>() {
        return Ok(AudioData::Bytes(b.as_bytes()));
    }
    copy_buffer(obj.py(), obj, None, None).map(AudioData::Owned)
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
