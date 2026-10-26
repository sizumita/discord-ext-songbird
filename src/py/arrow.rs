//! Optional Arrow API (W3, §8.12): `AudioChunk.__arrow_c_array__`, `ChunkBatch`, and the Arrow
//! import used by `AudioFile.from_arrow`, `RawPcm.from_arrow` / `from_samples` and
//! `OpusPackets.from_arrow`.
//!
//! Every member is compiled in every build (§2.4). Without the `arrow` feature the helpers return
//! `CoreError::FeatureMissing("arrow")`, i.e. `SongbirdError("built without the 'arrow' feature")`.
//! Imports copy into Rust-owned memory and drop the imported array while still attached.

use std::sync::Arc;

use pyo3::prelude::*;
use pyo3_stub_gen::derive::{gen_stub_pyclass, gen_stub_pymethods};
use sbcore::CoreError;
use sbcore::rx::{ChunkData, Delivered};

use crate::errors::CoreResultExt;

/// `Err(FeatureMissing("arrow"))` without the feature; `Err(Internal)` until W3 implements it.
fn unavailable() -> CoreError {
    if cfg!(feature = "arrow") {
        CoreError::Internal("unimplemented")
    } else {
        CoreError::FeatureMissing("arrow")
    }
}

/// Zero-copy export of one chunk: `(schema capsule, array capsule)`.
pub(crate) fn export_chunk<'py>(
    py: Python<'py>,
    chunk: &Arc<ChunkData>,
    requested_schema: Option<Bound<'py, PyAny>>,
) -> Result<Bound<'py, PyAny>, CoreError> {
    let _ = (py, chunk, requested_schema);
    Err(unavailable())
}

/// Element type of an imported primitive array.
#[allow(dead_code)] // phase-0 skeleton: constructed by the Arrow import (W3)
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SampleKind {
    Int16,
    Float32,
    UInt8,
}

/// `UInt8` primitive array, or a Binary/LargeBinary array with exactly one non-null value → bytes.
pub(crate) fn import_bytes(py: Python<'_>, array: &Bound<'_, PyAny>) -> Result<Vec<u8>, CoreError> {
    let _ = (py, array);
    Err(unavailable())
}

/// Int16/Float32/UInt8 primitive array without nulls → its little-endian bytes.
pub(crate) fn import_samples(
    py: Python<'_>,
    array: &Bound<'_, PyAny>,
) -> Result<(SampleKind, Vec<u8>), CoreError> {
    let _ = (py, array);
    Err(unavailable())
}

/// Binary/LargeBinary array without nulls → one owned buffer per value.
pub(crate) fn import_binary(
    py: Python<'_>,
    array: &Bound<'_, PyAny>,
) -> Result<Vec<Vec<u8>>, CoreError> {
    let _ = (py, array);
    Err(unavailable())
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
        Err::<Bound<'py, PyAny>, _>(unavailable()).py(py)
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
        let _ = requested_schema;
        Err::<Bound<'py, PyAny>, _>(unavailable()).py(py)
    }

    /// ``pyarrow.RecordBatch`` (lazy ``pyarrow`` import).
    #[gen_stub(override_return_type(type_repr = "typing.Any", imports = ("typing",)))]
    fn to_pyarrow<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        Err::<Bound<'py, PyAny>, _>(unavailable()).py(py)
    }

    fn __len__(&self) -> usize {
        self.rows.len()
    }
}
