pub mod raw;

use crate::buffer::PyBufferIO;
use pyo3::exceptions::{PyNotImplementedError, PyValueError};
use pyo3::prelude::*;
use pyo3::types::PyNotImplemented;
use songbird::input::core::io::MediaSource;
use songbird::input::{AudioStream, AudioStreamError, Compose, Input};

#[pyclass(subclass)]
pub struct AudioSource {}

#[pymethods]
impl AudioSource {
    #[new]
    fn new() -> Self {
        Self {}
    }

    fn get_source(&self) -> PyResult<Py<SourceComposed>> {
        Err(PyNotImplementedError::new_err(
            "get_input is not implemented",
        ))
    }
}

#[pyclass(frozen)]
pub struct SourceComposed(pub Box<dyn IntoSongbirdSource>);

pub trait IntoSongbirdSource: Compose + Send + Sync {
    fn input(&self) -> Input;
}

#[pymethods]
impl SourceComposed {
    #[new]
    fn new() -> PyResult<Self> {
        Err(PyValueError::new_err(
            "Cannot instantiate this class from python",
        ))
    }
}
