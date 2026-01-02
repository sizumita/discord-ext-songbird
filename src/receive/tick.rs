use arrow::array::Int16Array;
use dashmap::DashMap;
use pyo3::{pyclass, pymethods, Bound, PyAny, PyResult, Python};
use pyo3_arrow::PyArray;
use pyo3_stub_gen::derive::{gen_stub_pyclass, gen_stub_pyclass_complex_enum, gen_stub_pymethods};
use std::collections::HashSet;
use std::sync::Arc;

#[gen_stub_pyclass_complex_enum]
#[pyclass(module = "discord.ext.songbird.native.receive")]
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum VoiceKey {
    User(u64),
    Unknown(u32),
}

#[gen_stub_pyclass_complex_enum]
#[pyclass(module = "discord.ext.songbird.native.receive")]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VoiceState {
    Speaking,
    Silent,
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
    #[gen_stub(override_return_type(
        type_repr = "typing.Optional[typing.Tuple[VoiceState.Speaking, pyarrow.Int16Array] | typing.Tuple[VoiceState.Silent, None]]",
        imports = ("typing", "pyarrow")
    ))]
    pub fn get<'py>(
        &self,
        py: Python<'py>,
        key: &VoiceKey,
    ) -> PyResult<Option<(VoiceState, Option<Bound<'py, PyAny>>)>> {
        if let Some(data) = self.speaking.get(key) {
            Ok(Some((
                VoiceState::Speaking,
                Some(PyArray::from_array_ref(data.clone()).into_arro3(py)?),
            )))
        } else if self.silent.contains(key) {
            Ok(Some((VoiceState::Silent, None)))
        } else {
            Ok(None)
        }
    }
}
