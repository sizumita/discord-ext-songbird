use crate::connection::VoiceConnection;
use crate::queue::QueueHandler;
use pyo3::exceptions::PyValueError;
use pyo3::prelude::*;
use songbird::tracks::TrackHandle;
use std::sync::Arc;

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

    fn play(&self) {
        println!("play");
        self.handle.play().unwrap()
    }

    fn stop(&self) {
        self.handle.stop().unwrap()
    }

    fn set_volume(&self, volume: f32) {
        self.handle.set_volume(volume).unwrap()
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
