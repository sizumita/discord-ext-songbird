use arrow::array::Int16Array;
use pyo3::{pyclass, pymethods, Bound, PyAny, PyResult, Python};
use pyo3_arrow::PyArray;
use pyo3_stub_gen::derive::{gen_stub_pyclass, gen_stub_pymethods};
use songbird::events::context_data::VoiceTick as SongbirdVoiceTick;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

#[gen_stub_pyclass]
#[pyclass(module = "discord.ext.songbird.native.receive")]
#[derive(Default, Clone)]
pub struct VoiceTick {
    pub speaking: HashMap<u32, Arc<Int16Array>>,
    pub silent: HashSet<u32>,
}

#[gen_stub_pymethods]
#[pymethods]
impl VoiceTick {
    #[gen_stub(override_return_type(type_repr = "typing.Dict[int, pyarrow.Int16Array]", imports = ("typing", "pyarrow")))]
    /// Get decoded PCM frames for speakers in this tick.
    ///
    /// Returns
    /// -------
    /// Dict[int, pyarrow.Int16Array]
    ///     Mapping of SSRC to PCM frames for users speaking this tick.
    fn get_speakings<'py>(&self, py: Python<'py>) -> PyResult<HashMap<u32, Bound<'py, PyAny>>> {
        let mut dict = HashMap::new();
        for (k, v) in self.speaking.iter() {
            let r = PyArray::from_array_ref(v.clone());
            dict.insert(*k, r.into_arro3(py)?);
        }
        Ok(dict)
    }
}

impl VoiceTick {
    pub fn from_data(value: &SongbirdVoiceTick) -> Self {
        let speaking = value
            .speaking
            .iter()
            .filter_map(|(k, v)| {
                v.decoded_voice
                    .as_ref()
                    .map(|data| (*k, Arc::new(Int16Array::from(data.clone()))))
            })
            .collect::<HashMap<_, _>>();
        Self {
            speaking,
            silent: value.silent.clone(),
        }
    }
}
