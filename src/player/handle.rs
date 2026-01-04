use crate::error::PyControlError;
use pyo3::{pyclass, pymethods, PyResult};
use pyo3_stub_gen::derive::gen_stub_pyclass;
use songbird::tracks::TrackHandle;

#[gen_stub_pyclass]
#[pyclass(name = "TrackHandle", module = "discord.ext.songbird.native.player")]
pub struct PyTrackHandle {
    inner: TrackHandle,
}

#[pymethods]
impl PyTrackHandle {
    fn play(&self) -> PyResult<()> {
        self.inner.play().map_err(|err| PyControlError::new_err(err.to_string()))?;
        Ok(())
    }

    fn pause(&self) -> PyResult<()> {
        self.inner.pause().map_err(|err| PyControlError::new_err(err.to_string()))?;
        Ok(())
    }

    fn stop(&self) -> PyResult<()> {
        self.inner.stop().map_err(|err| PyControlError::new_err(err.to_string()))?;
        Ok(())
    }
}

impl PyTrackHandle {
    pub fn new(inner: TrackHandle) -> Self {
        Self {
            inner,
        }
    }
}
