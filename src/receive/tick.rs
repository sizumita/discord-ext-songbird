use crate::model::{ArrowArray, ArrowRecordBatch};
use crate::receive::identity::VoiceIdentityResolver;
use arrow::array::{
    ArrayRef, BooleanArray, Int16Array, Int16Builder, ListArray, ListBuilder, RecordBatch,
    UInt8Array, UInt64Array,
};
use arrow::datatypes::{DataType, Field, Schema, SchemaRef};
use pyo3::types::PyInt;
use pyo3::{Bound, PyResult, Python, pyclass, pymethods};
use pyo3_arrow::{PyArray, PyRecordBatch};
use pyo3_stub_gen::derive::{gen_stub_pyclass, gen_stub_pyclass_complex_enum, gen_stub_pymethods};
use songbird::events::context_data::VoiceTick as SongbirdVoiceTick;
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, OnceLock};

const KEY_KIND_USER: u8 = 0;
const KEY_KIND_UNKNOWN_SSRC: u8 = 1;

#[gen_stub_pyclass_complex_enum]
#[pyclass(module = "discord.ext.songbird.native.receive", frozen, from_py_object)]
/// Identifier for a voice source.
///
/// This is either a Discord user ID or an unknown SSRC.
///
/// Examples
/// --------
/// ```python
/// key = receive.VoiceKey.User(1234)
/// key.id()
/// key.is_user()
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum VoiceKey {
    User(u64),
    Unknown(u32),
}

#[derive(Clone)]
pub struct VoiceTickBatch {
    batch: RecordBatch,
    key_kind: UInt8Array,
    key_id: UInt64Array,
    speaking: BooleanArray,
    pcm: ListArray,
}

struct VoiceTickRow<'a> {
    key: VoiceKey,
    pcm: Option<&'a [i16]>,
}

#[gen_stub_pyclass]
#[pyclass(
    module = "discord.ext.songbird.native.receive",
    frozen,
    skip_from_py_object
)]
/// Compatibility wrapper for a received voice tick.
///
/// Notes
/// -----
/// New receive iterators yield `pyarrow.RecordBatch` objects. This class remains
/// available for Rust-side compatibility helpers.
#[derive(Clone)]
pub struct VoiceTick {
    inner: Arc<VoiceTickBatch>,
}

#[gen_stub_pymethods]
#[pymethods]
impl VoiceKey {
    /// Return the underlying integer identifier.
    ///
    /// For user keys this is the user ID, otherwise the SSRC value.
    ///
    /// Returns
    /// -------
    /// int
    ///
    /// Examples
    /// --------
    /// ```python
    /// key = receive.VoiceKey.User(1234)
    /// key.id()
    /// ```
    fn id<'py>(&self, py: Python<'py>) -> Bound<'py, PyInt> {
        match self {
            VoiceKey::User(user_id) => PyInt::new(py, *user_id),
            VoiceKey::Unknown(ssrc) => PyInt::new(py, *ssrc),
        }
    }

    /// Check whether this key represents a user ID.
    ///
    /// Returns
    /// -------
    /// bool
    ///
    /// Examples
    /// --------
    /// ```python
    /// receive.VoiceKey.User(1).is_user()
    /// ```
    fn is_user(&self) -> bool {
        matches!(self, VoiceKey::User(_))
    }

    /// Check whether this key represents an unknown SSRC.
    ///
    /// Returns
    /// -------
    /// bool
    ///
    /// Examples
    /// --------
    /// ```python
    /// receive.VoiceKey.Unknown(42).is_unknown()
    /// ```
    fn is_unknown(&self) -> bool {
        matches!(self, VoiceKey::Unknown(_))
    }

    /// Return a debug representation.
    ///
    /// Returns
    /// -------
    /// str
    fn __repr__(&self) -> String {
        match self {
            VoiceKey::User(user_id) => format!("VoiceKey.User({})", user_id),
            VoiceKey::Unknown(ssrc) => format!("VoiceKey.Unknown({})", ssrc),
        }
    }
}

#[gen_stub_pymethods]
#[pymethods]
impl VoiceTick {
    /// Get PCM audio for a key if it is speaking in this tick.
    ///
    /// Parameters
    /// ----------
    /// key : VoiceKey
    ///     The user/ssrc key to query.
    ///
    /// Returns
    /// -------
    /// pyarrow.Int16Array | None
    ///     PCM when speaking, otherwise None.
    pub fn get<'py>(
        &self,
        py: Python<'py>,
        key: &VoiceKey,
    ) -> PyResult<Option<ArrowArray<'py, Int16Array>>> {
        self.inner.get(py, key)
    }

    /// Check whether a key is marked silent in this tick.
    ///
    /// Parameters
    /// ----------
    /// key : VoiceKey
    ///     The user/ssrc key to query.
    ///
    /// Returns
    /// -------
    /// bool
    pub fn is_silent(&self, key: &VoiceKey) -> bool {
        self.inner.is_silent(key)
    }

    /// Return all keys present in this tick (speaking + silent).
    ///
    /// Returns
    /// -------
    /// set[VoiceKey]
    fn all_keys(&self) -> HashSet<VoiceKey> {
        self.inner.all_keys()
    }

    /// Return keys that have PCM data in this tick.
    ///
    /// Returns
    /// -------
    /// set[VoiceKey]
    fn speaking_keys(&self) -> HashSet<VoiceKey> {
        self.inner.speaking_keys()
    }

    /// Return keys marked silent in this tick.
    ///
    /// Returns
    /// -------
    /// set[VoiceKey]
    fn silent_keys(&self) -> HashSet<VoiceKey> {
        self.inner.silent_keys()
    }

    /// Return this tick as a columnar Arrow record batch.
    ///
    /// Returns
    /// -------
    /// pyarrow.RecordBatch
    fn record_batch<'py>(&self, py: Python<'py>) -> PyResult<ArrowRecordBatch<'py>> {
        self.inner.to_record_batch(py)
    }
}

impl VoiceTick {
    pub fn from_batch(inner: Arc<VoiceTickBatch>) -> Self {
        Self { inner }
    }
}

impl VoiceTickBatch {
    pub fn from_parts(
        tick: &SongbirdVoiceTick,
        identities: &impl VoiceIdentityResolver,
    ) -> Arc<Self> {
        Self::from_ssrc_rows(
            tick.speaking
                .iter()
                .map(|(ssrc, data)| (*ssrc, data.decoded_voice.as_deref())),
            tick.silent.iter().copied(),
            identities,
        )
    }

    fn from_ssrc_rows<'a, S, I>(
        speaking: S,
        silent: I,
        identities: &impl VoiceIdentityResolver,
    ) -> Arc<Self>
    where
        S: IntoIterator<Item = (u32, Option<&'a [i16]>)>,
        I: IntoIterator<Item = u32>,
    {
        let speaking = speaking.into_iter();
        let silent = silent.into_iter();
        let speaking_capacity = speaking.size_hint().0;
        let silent_capacity = silent.size_hint().0;
        let mut rows_by_key = HashMap::with_capacity(speaking_capacity + silent_capacity);

        for (ssrc, decoded) in speaking {
            let key = key_for_ssrc(ssrc, identities);
            if let Some(decoded) = decoded {
                let entry = rows_by_key.entry(key).or_insert(None);
                if entry.is_none() {
                    *entry = Some(decoded);
                }
            } else {
                rows_by_key.entry(key).or_insert(None);
            }
        }

        for ssrc in silent {
            let key = key_for_ssrc(ssrc, identities);
            rows_by_key.entry(key).or_insert(None);
        }

        let rows = rows_by_key
            .into_iter()
            .map(|(key, pcm)| VoiceTickRow { key, pcm })
            .collect();

        Arc::new(Self::from_rows(rows))
    }

    fn from_rows(mut rows: Vec<VoiceTickRow<'_>>) -> Self {
        rows.sort_by_key(|row| (key_kind(&row.key), key_id(&row.key)));

        let total_samples = rows
            .iter()
            .filter_map(|row| row.pcm)
            .map(<[i16]>::len)
            .sum();

        let key_kind = UInt8Array::from(
            rows.iter()
                .map(|row| key_kind(&row.key))
                .collect::<Vec<_>>(),
        );
        let key_id = UInt64Array::from(rows.iter().map(|row| key_id(&row.key)).collect::<Vec<_>>());
        let speaking =
            BooleanArray::from(rows.iter().map(|row| row.pcm.is_some()).collect::<Vec<_>>());

        let values = Int16Builder::with_capacity(total_samples);
        let item_field = Arc::new(Field::new_list_field(DataType::Int16, false));
        let mut pcm_builder = ListBuilder::with_capacity(values, rows.len()).with_field(item_field);
        for row in &rows {
            if let Some(pcm) = row.pcm {
                pcm_builder.values().append_slice(pcm);
            }
            pcm_builder.append(true);
        }
        let pcm = pcm_builder.finish();

        let batch = RecordBatch::try_new(
            voice_tick_schema(),
            vec![
                Arc::new(key_kind.clone()) as ArrayRef,
                Arc::new(key_id.clone()) as ArrayRef,
                Arc::new(speaking.clone()) as ArrayRef,
                Arc::new(pcm.clone()) as ArrayRef,
            ],
        )
        .expect("VoiceTickBatch columns must match the fixed schema");

        Self {
            batch,
            key_kind,
            key_id,
            speaking,
            pcm,
        }
    }

    pub fn record_batch(&self) -> RecordBatch {
        self.batch.clone()
    }

    pub fn to_record_batch<'py>(&self, py: Python<'py>) -> PyResult<ArrowRecordBatch<'py>> {
        PyRecordBatch::new(self.record_batch())
            .into_arro3(py)
            .map(Into::into)
    }

    pub fn get<'py>(
        &self,
        py: Python<'py>,
        key: &VoiceKey,
    ) -> PyResult<Option<ArrowArray<'py, Int16Array>>> {
        if let Some(data) = self.get_array_ref(key) {
            Ok(Some(PyArray::from_array_ref(data).into_arro3(py)?.into()))
        } else {
            Ok(None)
        }
    }

    pub fn get_array_ref(&self, key: &VoiceKey) -> Option<ArrayRef> {
        self.find_row(key)
            .filter(|row| self.speaking.value(*row))
            .map(|row| self.pcm.value(row))
    }

    pub fn is_silent(&self, key: &VoiceKey) -> bool {
        self.find_row(key)
            .is_some_and(|row| !self.speaking.value(row))
    }

    fn all_keys(&self) -> HashSet<VoiceKey> {
        (0..self.batch.num_rows())
            .map(|row| self.key_at(row))
            .collect()
    }

    fn speaking_keys(&self) -> HashSet<VoiceKey> {
        (0..self.batch.num_rows())
            .filter(|row| self.speaking.value(*row))
            .map(|row| self.key_at(row))
            .collect()
    }

    fn silent_keys(&self) -> HashSet<VoiceKey> {
        (0..self.batch.num_rows())
            .filter(|row| !self.speaking.value(*row))
            .map(|row| self.key_at(row))
            .collect()
    }

    fn find_row(&self, key: &VoiceKey) -> Option<usize> {
        let kind = key_kind(key);
        let id = key_id(key);
        (0..self.batch.num_rows())
            .find(|row| self.key_kind.value(*row) == kind && self.key_id.value(*row) == id)
    }

    fn key_at(&self, row: usize) -> VoiceKey {
        match self.key_kind.value(row) {
            KEY_KIND_USER => VoiceKey::User(self.key_id.value(row)),
            KEY_KIND_UNKNOWN_SSRC => VoiceKey::Unknown(
                self.key_id
                    .value(row)
                    .try_into()
                    .expect("unknown SSRC keys are stored as u32-compatible values"),
            ),
            _ => unreachable!("VoiceTickBatch only stores fixed key kinds"),
        }
    }
}

fn voice_tick_schema() -> SchemaRef {
    static SCHEMA: OnceLock<SchemaRef> = OnceLock::new();
    SCHEMA
        .get_or_init(|| {
            Arc::new(Schema::new(vec![
                Field::new("key_kind", DataType::UInt8, false),
                Field::new("key_id", DataType::UInt64, false),
                Field::new("speaking", DataType::Boolean, false),
                Field::new_list("pcm", Field::new_list_field(DataType::Int16, false), false),
            ]))
        })
        .clone()
}

fn key_for_ssrc(ssrc: u32, identities: &impl VoiceIdentityResolver) -> VoiceKey {
    if let Some(user_id) = identities.user_id_for_ssrc(ssrc) {
        VoiceKey::User(user_id)
    } else {
        VoiceKey::Unknown(ssrc)
    }
}

fn key_kind(key: &VoiceKey) -> u8 {
    match key {
        VoiceKey::User(_) => KEY_KIND_USER,
        VoiceKey::Unknown(_) => KEY_KIND_UNKNOWN_SSRC,
    }
}

fn key_id(key: &VoiceKey) -> u64 {
    match key {
        VoiceKey::User(user_id) => *user_id,
        VoiceKey::Unknown(ssrc) => u64::from(*ssrc),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use arrow::array::AsArray;
    use arrow::datatypes::Int16Type;

    fn int16_values(array: &ArrayRef) -> Vec<i16> {
        array
            .as_primitive::<Int16Type>()
            .values()
            .iter()
            .copied()
            .collect()
    }

    #[test]
    fn builds_expected_columnar_batch() {
        let user_pcm = [1, 2, 3, 4];
        let unknown_pcm = [9, 8];
        let batch = VoiceTickBatch::from_rows(vec![
            VoiceTickRow {
                key: VoiceKey::Unknown(42),
                pcm: Some(&unknown_pcm),
            },
            VoiceTickRow {
                key: VoiceKey::User(7),
                pcm: Some(&user_pcm),
            },
            VoiceTickRow {
                key: VoiceKey::User(9),
                pcm: None,
            },
        ]);

        assert_eq!(
            batch
                .record_batch()
                .schema()
                .fields()
                .iter()
                .map(|field| field.name().as_str())
                .collect::<Vec<_>>(),
            ["key_kind", "key_id", "speaking", "pcm"]
        );
        assert_eq!(
            batch.key_kind.values(),
            &[KEY_KIND_USER, KEY_KIND_USER, KEY_KIND_UNKNOWN_SSRC]
        );
        assert_eq!(batch.key_id.values(), &[7, 9, 42]);
        assert!(batch.speaking.value(0));
        assert!(!batch.speaking.value(1));
        assert!(batch.speaking.value(2));
        assert_eq!(batch.pcm.value_offsets(), &[0, 4, 4, 6]);
        assert_eq!(int16_values(batch.pcm.values()), vec![1, 2, 3, 4, 9, 8]);
    }

    #[test]
    fn per_key_helper_returns_zero_copy_pcm_slice() {
        let user_pcm = [1, 2, 3, 4];
        let unknown_pcm = [9, 8];
        let batch = VoiceTickBatch::from_rows(vec![
            VoiceTickRow {
                key: VoiceKey::User(7),
                pcm: Some(&user_pcm),
            },
            VoiceTickRow {
                key: VoiceKey::Unknown(42),
                pcm: Some(&unknown_pcm),
            },
        ]);

        let base = batch.pcm.values().as_primitive::<Int16Type>();
        let base_ptr = base.values().inner().as_ptr();
        let user = batch
            .get_array_ref(&VoiceKey::User(7))
            .expect("user PCM should be present");
        let user = user.as_primitive::<Int16Type>();
        let unknown = batch
            .get_array_ref(&VoiceKey::Unknown(42))
            .expect("unknown PCM should be present");
        let unknown = unknown.as_primitive::<Int16Type>();

        assert_eq!(user.values(), &[1, 2, 3, 4]);
        assert_eq!(unknown.values(), &[9, 8]);
        assert_eq!(user.values().inner().as_ptr(), base_ptr);
        assert_eq!(
            unknown.values().inner().as_ptr(),
            base_ptr.wrapping_add(user_pcm.len() * std::mem::size_of::<i16>())
        );
    }

    #[test]
    fn clone_keeps_payload_buffer_shared() {
        let pcm = [1, 2, 3, 4];
        let batch = VoiceTickBatch::from_rows(vec![VoiceTickRow {
            key: VoiceKey::User(7),
            pcm: Some(&pcm),
        }]);
        let cloned = batch.clone();

        let original_values = batch.pcm.values().as_primitive::<Int16Type>();
        let cloned_values = cloned.pcm.values().as_primitive::<Int16Type>();
        assert_eq!(
            original_values.values().inner().as_ptr(),
            cloned_values.values().inner().as_ptr()
        );
    }

    #[test]
    fn ssrc_rows_resolve_user_ids_from_shared_identity_map() {
        let identities = crate::receive::identity::VoiceIdentityMap::default();
        identities.insert(42, 7);
        let pcm = [1, 2, 3, 4];
        let batch = VoiceTickBatch::from_ssrc_rows([(42, Some(pcm.as_slice()))], [99], &identities);

        assert_eq!(batch.speaking_keys(), HashSet::from([VoiceKey::User(7)]));
        assert_eq!(batch.silent_keys(), HashSet::from([VoiceKey::Unknown(99)]));
    }

    #[test]
    fn ssrc_rows_merge_duplicate_user_keys_and_prefer_pcm() {
        let identities = crate::receive::identity::VoiceIdentityMap::default();
        identities.insert(42, 7);
        identities.insert(43, 7);
        identities.insert(44, 7);
        let first_pcm = [1, 2, 3, 4];
        let second_pcm = [9, 8];
        let batch = VoiceTickBatch::from_ssrc_rows(
            [
                (42, None),
                (43, Some(first_pcm.as_slice())),
                (44, Some(second_pcm.as_slice())),
            ],
            [42],
            &identities,
        );

        assert_eq!(batch.record_batch().num_rows(), 1);
        assert_eq!(batch.speaking_keys(), HashSet::from([VoiceKey::User(7)]));
        assert_eq!(batch.silent_keys(), HashSet::new());
        let pcm = batch
            .get_array_ref(&VoiceKey::User(7))
            .expect("merged user PCM should be present");
        assert_eq!(int16_values(&pcm), first_pcm);
    }

    #[test]
    fn ssrc_rows_merge_duplicate_silent_user_keys() {
        let identities = crate::receive::identity::VoiceIdentityMap::default();
        identities.insert(42, 7);
        identities.insert(43, 7);
        let batch = VoiceTickBatch::from_ssrc_rows([(42, None)], [42, 43], &identities);

        assert_eq!(batch.record_batch().num_rows(), 1);
        assert_eq!(batch.all_keys(), HashSet::from([VoiceKey::User(7)]));
        assert_eq!(batch.speaking_keys(), HashSet::new());
        assert_eq!(batch.silent_keys(), HashSet::from([VoiceKey::User(7)]));
    }
}
