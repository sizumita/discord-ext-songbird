pub mod crypto_mode;
pub mod decode_mode;

use crate::config::crypto_mode::PyCryptoMode;
use crate::config::decode_mode::{PyChannels, PyDecodeMode};
use pyo3::prelude::*;
use pyo3::types::PyType;
use pyo3::IntoPyObjectExt;
use songbird::driver::DecodeMode;
use songbird::Config;

#[pyclass]
#[derive(Clone)]
pub struct ConfigBuilder {
    pub(crate) config: Config,
}

#[pymethods]
impl ConfigBuilder {
    #[new]
    fn new() -> Self {
        Self {
            config: Config::default(),
        }
    }

    #[classmethod]
    fn send_only<'py>(cls: &Bound<'py, PyType>, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let mut slf = cls.call0()?.extract::<Self>()?;
        slf.config.decode_mode = DecodeMode::Pass;
        slf.into_bound_py_any(py)
    }

    fn crypto_mode<'py>(
        mut self_: PyRefMut<'py, Self>,
        mode: &Bound<'py, PyCryptoMode>,
    ) -> PyRefMut<'py, Self> {
        self_.config.crypto_mode = mode.get().into();
        self_
    }

    fn decode_mode<'py>(
        mut self_: PyRefMut<'py, Self>,
        mode: &Bound<'py, PyDecodeMode>,
    ) -> PyRefMut<'py, Self> {
        self_.config.decode_mode = mode.get().into();
        self_
    }

    fn decode_channels<'py>(
        mut self_: PyRefMut<'py, Self>,
        channel: &Bound<'py, PyChannels>,
    ) -> PyRefMut<'py, Self> {
        self_.config.decode_channels = channel.get().into();
        self_
    }
}
