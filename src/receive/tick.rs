use crate::model::ArrowArray;
use arrow::array::Int16Array;
use dashmap::DashMap;
use pyo3::types::PyInt;
use pyo3::{pyclass, pymethods, Bound, PyResult, Python};
use pyo3_arrow::PyArray;
use pyo3_stub_gen::derive::{gen_stub_pyclass, gen_stub_pyclass_complex_enum, gen_stub_pymethods};
use songbird::events::context_data::VoiceTick as SongbirdVoiceTick;
use std::collections::HashSet;
use std::sync::Arc;

#[gen_stub_pyclass_complex_enum]
#[pyclass(module = "discord.ext.songbird.native.receive")]
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

#[gen_stub_pyclass]
#[pyclass(module = "discord.ext.songbird.native.receive")]
/// Snapshot of received voice data for a single tick.
///
/// Examples
/// --------
/// ```python
/// key = receive.VoiceKey.User(user_id)
/// pcm = tick.get(key)
/// if pcm is None and tick.is_silent(key):
///     print(\"silent\")
/// ```
#[derive(Default, Clone)]
pub struct VoiceTick {
    pub speaking: DashMap<VoiceKey, Arc<Int16Array>>,
    pub silent: HashSet<VoiceKey>,
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
    ///
    /// Examples
    /// --------
    /// ```python
    /// pcm = tick.get(receive.VoiceKey.User(user_id))
    /// if pcm is not None:
    ///     handle_pcm(pcm)
    /// ```
    pub fn get<'py>(
        &self,
        py: Python<'py>,
        key: &VoiceKey,
    ) -> PyResult<Option<ArrowArray<'py, Int16Array>>> {
        if let Some(data) = self.speaking.get(key) {
            Ok(Some(
                PyArray::from_array_ref(data.clone()).into_arro3(py)?.into(),
            ))
        } else {
            Ok(None)
        }
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
    ///
    /// Examples
    /// --------
    /// ```python
    /// if tick.is_silent(receive.VoiceKey.User(user_id)):
    ///     ...
    /// ```
    pub fn is_silent(&self, key: &VoiceKey) -> bool {
        self.silent.contains(key)
    }

    /// Return all keys present in this tick (speaking + silent).
    ///
    /// Returns
    /// -------
    /// set[VoiceKey]
    ///
    /// Examples
    /// --------
    /// ```python
    /// for key in tick.all_keys():
    ///     ...
    /// ```
    fn all_keys(&self) -> HashSet<VoiceKey> {
        let mut all_keys: HashSet<VoiceKey> = self
            .speaking
            .iter()
            .map(|entry| entry.key().clone())
            .collect();
        all_keys.extend(self.silent.iter().cloned());
        all_keys
    }

    /// Return keys that have PCM data in this tick.
    ///
    /// Returns
    /// -------
    /// set[VoiceKey]
    ///
    /// Examples
    /// --------
    /// ```python
    /// for key in tick.speaking_keys():
    ///     ...
    /// ```
    fn speaking_keys(&self) -> HashSet<VoiceKey> {
        self.speaking
            .iter()
            .map(|entry| entry.key().clone())
            .collect()
    }

    /// Return keys marked silent in this tick.
    ///
    /// Returns
    /// -------
    /// set[VoiceKey]
    ///
    /// Examples
    /// --------
    /// ```python
    /// for key in tick.silent_keys():
    ///     ...
    /// ```
    fn silent_keys(&self) -> HashSet<VoiceKey> {
        self.silent.clone()
    }
}

impl VoiceTick {
    pub fn from_parts(tick: &SongbirdVoiceTick, ssrc_data: &DashMap<u32, u64>) -> Self {
        let mut silents = tick
            .silent
            .iter()
            .map(|ssrc| {
                if let Some(user_id) = ssrc_data.get(ssrc) {
                    VoiceKey::User(*user_id)
                } else {
                    VoiceKey::Unknown(*ssrc)
                }
            })
            .collect::<HashSet<_>>();
        let payloads = DashMap::with_capacity(tick.speaking.len());
        for (ssrc, data) in &tick.speaking {
            let key = if let Some(user_id) = ssrc_data.get(ssrc) {
                VoiceKey::User(*user_id)
            } else {
                VoiceKey::Unknown(*ssrc)
            };
            if let Some(decoded) = &data.decoded_voice {
                payloads.insert(key, Arc::new(Int16Array::from(decoded.clone())));
            } else {
                silents.insert(key);
            }
        }
        VoiceTick {
            speaking: payloads,
            silent: silents,
        }
    }
}
