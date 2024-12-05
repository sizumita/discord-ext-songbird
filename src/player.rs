use crate::connection::VoiceConnection;
use crate::queue::QueueHandler;
use pyo3::exceptions::PyValueError;
use pyo3::prelude::*;
use songbird::tracks::TrackHandle;
use std::sync::Arc;
use crate::error::SongbirdError;

#[pyclass(frozen)]
pub struct PlayerHandler {
    pub(crate) handle: TrackHandle,
    #[pyo3(get)]
    queue: Py<QueueHandler>,
}

#[pymethods]
impl PlayerHandler {
    #[new]
    fn new() -> PyResult<Self> {
        Err(PyValueError::new_err(
            "Cannot create PlayerHandler from python",
        ))
    }

    fn play(&self) -> PyResult<()> {
        Ok(self.handle.play().map_err(SongbirdError::from)?)
    }

    fn stop(&self) -> PyResult<()> {
        Ok(self.handle.stop().map_err(SongbirdError::from)?)
    }

    fn pause(&self) -> PyResult<()> {
        Ok(self.handle.pause().map_err(SongbirdError::from)?)
    }

    fn set_volume(&self, volume: f32) -> PyResult<()> {
        Ok(self.handle.set_volume(volume).map_err(SongbirdError::from)?)
    }
}

impl PlayerHandler {
    pub fn from_handle(handle: TrackHandle, conn: Arc<VoiceConnection>) -> PyResult<Self> {
        Ok(Self {
            handle,
            queue: Python::with_gil(|py| Py::new(py, QueueHandler::new(conn)))?,
        })
    }
}
