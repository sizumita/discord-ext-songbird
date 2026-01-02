use crate::model::ArrowArray;
use arrow::array::Int16Array;
use dashmap::DashMap;
use pyo3::{pyclass, pymethods, Bound, PyAny, PyResult, Python};
use pyo3_arrow::PyArray;
use pyo3_stub_gen::derive::{
    gen_stub_pyclass, gen_stub_pyclass_complex_enum, gen_stub_pyclass_enum, gen_stub_pymethods,
};
use std::collections::HashSet;
use std::sync::Arc;

#[gen_stub_pyclass_complex_enum]
#[pyclass(module = "discord.ext.songbird.native.receive")]
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum VoiceKey {
    User(u64),
    Unknown(u32),
}

#[gen_stub_pyclass]
#[pyclass(module = "discord.ext.songbird.native.receive")]
#[derive(Default, Clone)]
pub struct VoiceTick {
    pub speaking: DashMap<VoiceKey, Arc<Int16Array>>,
    pub silent: HashSet<VoiceKey>,
}

#[gen_stub_pymethods]
#[pymethods]
impl VoiceTick {
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

    pub fn is_silent(&self, key: &VoiceKey) -> bool {
        self.silent.contains(key)
    }

    fn all_keys(&self) -> HashSet<VoiceKey> {
        let mut all_keys: HashSet<VoiceKey> = self
            .speaking
            .iter()
            .map(|entry| entry.key().clone())
            .collect();
        all_keys.extend(self.silent.iter().cloned());
        all_keys
    }

    fn speaking_keys(&self) -> HashSet<VoiceKey> {
        self.speaking
            .iter()
            .map(|entry| entry.key().clone())
            .collect()
    }

    fn silent_keys(&self) -> HashSet<VoiceKey> {
        self.silent.clone()
    }
}
