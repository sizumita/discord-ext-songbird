//! Optional Arrow API (W3, §8.12): `AudioChunk.__arrow_c_array__`, `ChunkBatch`, and the Arrow
//! import used by `AudioFile.from_arrow`, `RawPcm.from_arrow` / `from_samples` and
//! `OpusPackets.from_arrow`.
//!
//! Every member is compiled in every build (§2.4). Without the `arrow` feature the helpers return
//! `CoreError::FeatureMissing("arrow")`, i.e. `SongbirdError("built without the 'arrow' feature")`.
//! Imports copy into Rust-owned memory and drop the imported array while still attached.
//! Chunk export is zero-copy: the Arrow buffer's owner is the chunk's `Arc<ChunkData>` (Rust-only
//! memory), so the consumer's release callback never touches Python.

use std::sync::Arc;

use pyo3::prelude::*;
use pyo3_stub_gen::derive::{gen_stub_pyclass, gen_stub_pymethods};
use sbcore::CoreError;
use sbcore::rx::{ChunkData, Delivered};

use crate::errors::CoreResultExt;

/// Element type of an imported primitive array.
#[cfg_attr(not(feature = "arrow"), allow(dead_code))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SampleKind {
    Int16,
    Float32,
    UInt8,
}

#[cfg(feature = "arrow")]
mod imp {
    use std::collections::HashMap;
    use std::ptr::NonNull;
    use std::sync::Arc;

    use arrow_array::{
        Array, ArrayRef, BinaryArray, BooleanArray, Float32Array, GenericBinaryArray, Int16Array,
        Int64Array, OffsetSizeTrait, StructArray, UInt8Array, UInt32Array, UInt64Array,
    };
    use arrow_buffer::alloc::Allocation;
    use arrow_buffer::{Buffer, OffsetBuffer, ScalarBuffer};
    use arrow_schema::{DataType, Field, Fields, Schema};
    use pyo3::prelude::*;
    use pyo3::types::PyCapsule;
    use pyo3_arrow::PyArray;
    use pyo3_arrow::ffi::{to_array_pycapsules, to_schema_pycapsule};
    use sbcore::limits::{MAX_OPUS_PACKETS, MAX_SOURCE_BYTES, check};
    use sbcore::rx::{ChunkData, Delivered};
    use sbcore::tx::memory::OpusFrames;
    use sbcore::{CoreError, Encoding};

    use super::SampleKind;

    fn invalid(msg: impl std::fmt::Display) -> CoreError {
        CoreError::InvalidAudio(msg.to_string().into())
    }

    /// `requested_schema` as a capsule (`None` passes through).
    fn requested<'py>(s: Option<Bound<'py, PyAny>>) -> PyResult<Option<Bound<'py, PyCapsule>>> {
        match s {
            Some(s) if !s.is_none() => Ok(Some(s.cast_into::<PyCapsule>()?)),
            _ => Ok(None),
        }
    }

    // ── export ───────────────────────────────────────────────────────────────────────────────────

    /// `pcm` field with `{"sample_rate", "channels", "encoding"}` metadata.
    fn chunk_field(c: &ChunkData) -> Field {
        let fmt = c.meta.format;
        let dt = match fmt.encoding {
            Encoding::Pcm16Le => DataType::Int16,
            Encoding::F32Le => DataType::Float32,
            Encoding::MuLaw | Encoding::ALaw => DataType::UInt8,
        };
        Field::new("pcm", dt, false).with_metadata(HashMap::from([
            ("sample_rate".to_owned(), fmt.rate.hz().to_string()),
            ("channels".to_owned(), fmt.channels.to_string()),
            ("encoding".to_owned(), fmt.encoding.as_str().to_owned()),
        ]))
    }

    /// A primitive array over the chunk's own buffer (no copy); the buffer owns an `Arc`.
    fn chunk_array(c: &Arc<ChunkData>) -> Result<ArrayRef, CoreError> {
        let len = c.data.len();
        let ptr =
            NonNull::new(c.data.as_ptr().cast_mut()).ok_or(CoreError::Internal("arrow export"))?;
        let owner: Arc<dyn Allocation> = c.clone();
        // SAFETY: `ptr` is the start of `len` initialised, immutable bytes of the chunk's `PcmBuf`
        // (4-byte aligned). The buffer's owner is an `Arc` of that same chunk, so the memory lives
        // at least as long as the buffer and is never written again.
        let buf = unsafe { Buffer::from_custom_allocation(ptr, len, owner) };
        Ok(match c.meta.format.encoding {
            Encoding::Pcm16Le => {
                Arc::new(Int16Array::new(ScalarBuffer::new(buf, 0, len / 2), None))
            }
            Encoding::F32Le => {
                Arc::new(Float32Array::new(ScalarBuffer::new(buf, 0, len / 4), None))
            }
            Encoding::MuLaw | Encoding::ALaw => {
                Arc::new(UInt8Array::new(ScalarBuffer::new(buf, 0, len), None))
            }
        })
    }

    pub(crate) fn export_chunk<'py>(
        py: Python<'py>,
        chunk: &Arc<ChunkData>,
        requested_schema: Option<Bound<'py, PyAny>>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let array = chunk_array(chunk).map_err(|e| crate::errors::core_err(py, e))?;
        let field = Arc::new(chunk_field(chunk));
        let req = requested(requested_schema)?;
        Ok(to_array_pycapsules(py, field, &array, req)?.into_any())
    }

    fn batch_fields() -> Fields {
        Fields::from(vec![
            Field::new("user_id", DataType::UInt64, true),
            Field::new("seq", DataType::UInt64, false),
            Field::new("start_ms", DataType::Int64, false),
            Field::new("timeline_ms", DataType::Int64, false),
            Field::new("duration_ms", DataType::UInt32, false),
            Field::new("speaking", DataType::Boolean, false),
            Field::new("dropped_ms", DataType::UInt32, false),
            Field::new("pcm", DataType::Binary, false),
        ])
    }

    fn i64_of(v: u64) -> i64 {
        i64::try_from(v).unwrap_or(i64::MAX)
    }

    fn u32_of(v: u64) -> u32 {
        u32::try_from(v).unwrap_or(u32::MAX)
    }

    /// The batch as a `StructArray`; `pcm` is built from owned, concatenated bytes in one copy.
    fn batch_array(rows: &[Delivered]) -> Result<StructArray, CoreError> {
        let total: usize = rows.iter().map(|d| d.chunk.data.len()).sum();
        if i32::try_from(total).is_err() {
            return Err(invalid("batch too large for a binary column (2 GiB)"));
        }
        let mut values: Vec<u8> = Vec::new();
        values.try_reserve_exact(total)?;
        let mut offsets: Vec<i32> = Vec::new();
        offsets.try_reserve_exact(rows.len() + 1)?;
        offsets.push(0);
        for d in rows {
            values.extend_from_slice(d.chunk.data.as_bytes());
            offsets.push(i32::try_from(values.len()).unwrap_or(i32::MAX));
        }
        let pcm = BinaryArray::try_new(
            OffsetBuffer::new(ScalarBuffer::from(offsets)),
            Buffer::from_vec(values),
            None,
        )
        .map_err(invalid)?;
        let user_id: UInt64Array = rows.iter().map(|d| d.chunk.meta.user_id).collect();
        let seq = UInt64Array::from_iter_values(rows.iter().map(|d| d.seq));
        let start = Int64Array::from_iter_values(rows.iter().map(|d| i64_of(d.start_ms)));
        let timeline =
            Int64Array::from_iter_values(rows.iter().map(|d| i64_of(d.chunk.meta.timeline_ms)));
        let duration =
            UInt32Array::from_iter_values(rows.iter().map(|d| u32_of(d.chunk.duration_ms())));
        let speaking: BooleanArray = rows.iter().map(|d| Some(d.chunk.meta.speaking)).collect();
        let dropped = UInt32Array::from_iter_values(rows.iter().map(|d| u32_of(d.dropped_ms)));
        let columns: Vec<ArrayRef> = vec![
            Arc::new(user_id),
            Arc::new(seq),
            Arc::new(start),
            Arc::new(timeline),
            Arc::new(duration),
            Arc::new(speaking),
            Arc::new(dropped),
            Arc::new(pcm),
        ];
        StructArray::try_new(batch_fields(), columns, None).map_err(invalid)
    }

    pub(crate) fn batch_schema(py: Python<'_>) -> PyResult<Bound<'_, PyAny>> {
        Ok(to_schema_pycapsule(py, Schema::new(batch_fields()))?.into_any())
    }

    pub(crate) fn batch_export<'py>(
        py: Python<'py>,
        rows: &[Delivered],
        requested_schema: Option<Bound<'py, PyAny>>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let array = batch_array(rows).map_err(|e| crate::errors::core_err(py, e))?;
        let field = Arc::new(Field::new("", DataType::Struct(batch_fields()), false));
        let req = requested(requested_schema)?;
        Ok(to_array_pycapsules(py, field, &array, req)?.into_any())
    }

    // ── import ───────────────────────────────────────────────────────────────────────────────────

    fn import(array: &Bound<'_, PyAny>) -> Result<ArrayRef, CoreError> {
        let a: PyArray = array
            .extract()
            .map_err(|e| invalid(format!("expected an Arrow array: {e}")))?;
        Ok(a.into_inner().0)
    }

    fn no_nulls(a: &dyn Array) -> Result<(), CoreError> {
        if a.null_count() > 0 {
            return Err(invalid("the Arrow array must not contain nulls"));
        }
        Ok(())
    }

    /// An owned copy of `src` (`try_reserve`: `MemoryError` instead of an abort).
    fn owned(src: &[u8]) -> Result<Vec<u8>, CoreError> {
        check("bytes", src.len(), 0..=MAX_SOURCE_BYTES)?;
        let mut v = Vec::new();
        v.try_reserve_exact(src.len())?;
        v.extend_from_slice(src);
        Ok(v)
    }

    /// Little-endian bytes of a primitive array's values (respecting its offset).
    fn primitive_bytes<T: arrow_array::types::ArrowPrimitiveType>(
        a: &ArrayRef,
    ) -> Result<Vec<u8>, CoreError> {
        let p = a
            .as_any()
            .downcast_ref::<arrow_array::PrimitiveArray<T>>()
            .ok_or_else(|| invalid("unexpected Arrow array type"))?;
        no_nulls(p)?;
        owned(p.values().inner().as_slice())
    }

    fn one_binary<O: OffsetSizeTrait>(a: &ArrayRef) -> Result<Vec<u8>, CoreError> {
        let b = a
            .as_any()
            .downcast_ref::<GenericBinaryArray<O>>()
            .ok_or_else(|| invalid("unexpected Arrow array type"))?;
        if b.len() - b.null_count() != 1 {
            return Err(invalid(
                "a Binary/LargeBinary array must hold exactly one non-null value",
            ));
        }
        let i = (0..b.len())
            .find(|i| b.is_valid(*i))
            .ok_or_else(|| invalid("no value"))?;
        owned(b.value(i))
    }

    /// Every value, validated and copied once, straight from the Arrow buffer into the frames.
    fn all_binary<O: OffsetSizeTrait>(a: &ArrayRef) -> Result<OpusFrames, CoreError> {
        let b = a
            .as_any()
            .downcast_ref::<GenericBinaryArray<O>>()
            .ok_or_else(|| invalid("unexpected Arrow array type"))?;
        no_nulls(b)?;
        check("packets", b.len(), 0..=MAX_OPUS_PACKETS)?;
        let total: usize = (0..b.len()).map(|i| b.value(i).len()).sum();
        check("bytes", total, 0..=MAX_SOURCE_BYTES)?;
        OpusFrames::from_packets((0..b.len()).map(|i| b.value(i)))
    }

    pub(crate) fn import_bytes(
        py: Python<'_>,
        array: &Bound<'_, PyAny>,
    ) -> Result<Vec<u8>, CoreError> {
        let _ = py;
        let a = import(array)?;
        let out = match a.data_type() {
            DataType::UInt8 => primitive_bytes::<arrow_array::types::UInt8Type>(&a),
            DataType::Binary => one_binary::<i32>(&a),
            DataType::LargeBinary => one_binary::<i64>(&a),
            other => Err(invalid(format!(
                "expected a UInt8 or Binary/LargeBinary array, got {other}"
            ))),
        };
        drop(a); // released while attached
        out
    }

    pub(crate) fn import_samples(
        py: Python<'_>,
        array: &Bound<'_, PyAny>,
    ) -> Result<(SampleKind, Vec<u8>), CoreError> {
        let _ = py;
        let a = import(array)?;
        let out =
            match a.data_type() {
                DataType::Int16 => primitive_bytes::<arrow_array::types::Int16Type>(&a)
                    .map(|b| (SampleKind::Int16, b)),
                DataType::Float32 => primitive_bytes::<arrow_array::types::Float32Type>(&a)
                    .map(|b| (SampleKind::Float32, b)),
                DataType::UInt8 => primitive_bytes::<arrow_array::types::UInt8Type>(&a)
                    .map(|b| (SampleKind::UInt8, b)),
                other => Err(invalid(format!(
                    "expected an Int16, Float32 or UInt8 array, got {other}"
                ))),
            };
        drop(a);
        out
    }

    pub(crate) fn import_opus_frames(
        py: Python<'_>,
        array: &Bound<'_, PyAny>,
    ) -> Result<OpusFrames, CoreError> {
        let _ = py;
        let a = import(array)?;
        let out = match a.data_type() {
            DataType::Binary => all_binary::<i32>(&a),
            DataType::LargeBinary => all_binary::<i64>(&a),
            other => Err(invalid(format!(
                "expected a Binary/LargeBinary array, got {other}"
            ))),
        };
        drop(a);
        out
    }
}

#[cfg(not(feature = "arrow"))]
mod imp {
    use std::sync::Arc;

    use pyo3::prelude::*;
    use sbcore::CoreError;
    use sbcore::rx::{ChunkData, Delivered};

    use super::SampleKind;

    fn missing<'py>(py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        Err(crate::errors::core_err(
            py,
            CoreError::FeatureMissing("arrow"),
        ))
    }

    pub(crate) fn export_chunk<'py>(
        py: Python<'py>,
        _chunk: &Arc<ChunkData>,
        _requested_schema: Option<Bound<'py, PyAny>>,
    ) -> PyResult<Bound<'py, PyAny>> {
        missing(py)
    }

    pub(crate) fn batch_schema(py: Python<'_>) -> PyResult<Bound<'_, PyAny>> {
        missing(py)
    }

    pub(crate) fn batch_export<'py>(
        py: Python<'py>,
        _rows: &[Delivered],
        _requested_schema: Option<Bound<'py, PyAny>>,
    ) -> PyResult<Bound<'py, PyAny>> {
        missing(py)
    }

    pub(crate) fn import_bytes(
        _py: Python<'_>,
        _a: &Bound<'_, PyAny>,
    ) -> Result<Vec<u8>, CoreError> {
        Err(CoreError::FeatureMissing("arrow"))
    }

    pub(crate) fn import_samples(
        _py: Python<'_>,
        _a: &Bound<'_, PyAny>,
    ) -> Result<(SampleKind, Vec<u8>), CoreError> {
        Err(CoreError::FeatureMissing("arrow"))
    }

    pub(crate) fn import_opus_frames(
        _py: Python<'_>,
        _a: &Bound<'_, PyAny>,
    ) -> Result<sbcore::tx::memory::OpusFrames, CoreError> {
        Err(CoreError::FeatureMissing("arrow"))
    }
}

pub(crate) use imp::{import_bytes, import_opus_frames, import_samples};

/// Zero-copy export of one chunk: `(schema capsule, array capsule)`.
pub(crate) fn export_chunk<'py>(
    py: Python<'py>,
    chunk: &Arc<ChunkData>,
    requested_schema: Option<Bound<'py, PyAny>>,
) -> PyResult<Bound<'py, PyAny>> {
    imp::export_chunk(py, chunk, requested_schema)
}

/// ``[arrow]`` Up to ``max_chunks`` drained chunks as one record batch (§8.12).
///
/// Schema: ``user_id: uint64 (nullable)``, ``seq: uint64``, ``start_ms: int64``,
/// ``timeline_ms: int64``, ``duration_ms: uint32``, ``speaking: bool``, ``dropped_ms: uint32``,
/// ``pcm: binary``.
#[gen_stub_pyclass]
#[pyclass(module = "discord.ext.songbird.native", frozen)]
pub struct ChunkBatch {
    rows: Arc<Vec<Delivered>>,
}

impl ChunkBatch {
    pub(crate) fn new(rows: Vec<Delivered>) -> Self {
        ChunkBatch {
            rows: Arc::new(rows),
        }
    }
}

#[gen_stub_pymethods]
#[pymethods]
impl ChunkBatch {
    #[getter]
    fn num_rows(&self) -> usize {
        self.rows.len()
    }

    /// The batch schema as an Arrow PyCapsule.
    ///
    /// Raises
    /// ------
    /// SongbirdError
    ///     Built without the ``arrow`` feature.
    #[gen_stub(override_return_type(type_repr = "object"))]
    fn __arrow_c_schema__<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        imp::batch_schema(py)
    }

    /// The batch as a ``StructArray``: ``(schema capsule, array capsule)``.
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
        imp::batch_export(py, &self.rows, requested_schema)
    }

    /// ``pyarrow.RecordBatch`` (lazy ``pyarrow`` import).
    ///
    /// Raises
    /// ------
    /// SongbirdError
    ///     Built without the ``arrow`` feature.
    /// ImportError
    ///     ``pyarrow`` is not installed.
    #[gen_stub(override_return_type(type_repr = "typing.Any", imports = ("typing",)))]
    fn to_pyarrow<'py>(slf: &Bound<'py, Self>) -> PyResult<Bound<'py, PyAny>> {
        let py = slf.py();
        if !cfg!(feature = "arrow") {
            return Err::<Bound<'py, PyAny>, _>(CoreError::FeatureMissing("arrow")).py(py);
        }
        py.import("pyarrow")?.call_method1("record_batch", (slf,))
    }

    fn __len__(&self) -> usize {
        self.rows.len()
    }

    fn __repr__(&self) -> String {
        format!("<ChunkBatch num_rows={}>", self.rows.len())
    }
}
