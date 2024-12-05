use crate::buffer::PyBufferIO;
use crate::source::{AudioSource, IntoSongbirdSource, SourceComposed};
use async_trait::async_trait;
use pyo3::prelude::*;
use pyo3::{PyTraverseError, PyVisit};
use songbird::input::core::io::MediaSource;
use songbird::input::core::probe::Hint;
use songbird::input::{AudioStream, AudioStreamError, Compose, Input};
use std::sync::Arc;

/// Creates an AudioSource from raw data source.
/// The source must be a Stream of either pcm, wav, mp3, or ogg opus format.
#[pyclass(extends=AudioSource)]
pub struct RawBufferSource {
    source: Py<PyAny>,
}

#[pymethods]
impl RawBufferSource {
    #[new]
    fn new(source: Py<PyAny>) -> (Self, AudioSource) {
        (Self { source }, AudioSource::new())
    }

    fn __traverse__(&self, visit: PyVisit<'_>) -> Result<(), PyTraverseError> {
        visit.call(&self.source)?;
        Ok(())
    }

    fn get_source(&self) -> PyResult<Py<SourceComposed>> {
        Python::with_gil(|py| {
            Py::new(
                py,
                SourceComposed(Box::new(RawSourceInner(Arc::new(
                    self.source.clone_ref(py),
                )))),
            )
        })
    }
}

#[derive(Clone)]
struct RawSourceInner(Arc<Py<PyAny>>);

impl IntoSongbirdSource for RawSourceInner {
    fn input(&self) -> Input {
        Input::Lazy(Box::new(self.clone()))
    }
}

#[async_trait]
impl Compose for RawSourceInner {
    fn create(&mut self) -> Result<AudioStream<Box<dyn MediaSource>>, AudioStreamError> {
        let mut hint = Hint::new();
        hint.with_extension("wav");
        Ok(AudioStream {
            input: Box::new(PyBufferIO(Python::with_gil(|py| self.0.clone_ref(py)))),
            hint: Some(hint),
        })
    }

    async fn create_async(
        &mut self,
    ) -> Result<AudioStream<Box<dyn MediaSource>>, AudioStreamError> {
        Err(AudioStreamError::Unsupported)
    }

    fn should_create_async(&self) -> bool {
        false
    }
}
