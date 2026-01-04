pub(crate) mod audio;
pub mod stream;

use pyo3::{pyclass, pymethods, PyResult};
use pyo3_stub_gen::derive::{gen_stub_pyclass, gen_stub_pymethods};
use songbird::input::Compose;

#[gen_stub_pyclass]
#[pyclass(
    name = "InputBase",
    module = "discord.ext.songbird.native.player",
    subclass
)]
pub struct PyInputBase;

#[pyclass(
    name = "Compose",
    module = "discord.ext.songbird.native.player",
    subclass
)]
pub struct PyCompose(Option<Box<dyn Compose + Send + Sync + 'static>>);

#[gen_stub_pymethods]
#[pymethods]
impl PyInputBase {
    #[gen_stub(skip)]
    fn _compose(&self) -> PyResult<PyCompose> {
        Err(pyo3::exceptions::PyNotImplementedError::new_err(()))
    }
}

impl PyInputBase {
    pub fn new() -> Self {
        Self {}
    }

    pub fn is_lazy(&self) -> bool {
        false
    }
}

impl PyCompose {
    pub fn new(compose: Box<dyn Compose + Send + Sync + 'static>) -> Self {
        Self(Some(compose))
    }
    pub fn get_compose(&mut self) -> Option<Box<dyn Compose + Send + Sync + 'static>> {
        self.0.take()
    }
}
