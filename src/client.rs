use pyo3::prelude::PyAnyMethods;
use pyo3::{pyclass, pymethods, Bound, PyAny, PyResult};
use pyo3_stub_gen::derive::{gen_stub_pyclass, gen_stub_pymethods};
use songbird::id::ChannelId;
use std::num::NonZeroU64;

#[gen_stub_pyclass]
#[pyclass(subclass)]
pub struct SongbirdImpl {
    channel_id: ChannelId,
}

#[gen_stub_pymethods]
#[pymethods]
impl SongbirdImpl {
    #[new]
    fn new<'py>(
        #[gen_stub(override_type(type_repr="discord.Client", imports=("discord")))] _client: &Bound<
            PyAny,
        >,
        #[gen_stub(override_type(type_repr="discord.abc.Connectable", imports=("discord")))]
        connectable: &Bound<PyAny>,
    ) -> PyResult<Self> {
        let id: u64 = connectable.getattr("id")?.extract()?;
        if id == 0 {
            return Err(pyo3::exceptions::PyValueError::new_err(
                "Channel ID cannot be zero",
            ));
        }
        Ok(Self {
            channel_id: ChannelId(NonZeroU64::new(id).unwrap()),
        })
    }
}
