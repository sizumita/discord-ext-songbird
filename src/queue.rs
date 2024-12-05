use crate::connection::VoiceConnection;
use crate::player::PlayerHandler;
use crate::track::IntoTrack;
use pyo3::exceptions::PyValueError;
use pyo3::prelude::*;
use pyo3_async_runtimes::tokio::future_into_py;
use std::sync::Arc;

#[pyclass]
pub struct QueueHandler {
    connection: Arc<VoiceConnection>,
}

#[pymethods]
impl QueueHandler {
    #[new]
    fn __new__() -> PyResult<Self> {
        Err(PyValueError::new_err(
            "Queue handler cannot initialize from python",
        ))
    }

    fn enqueue<'py>(
        &self,
        py: Python<'py>,
        track: Bound<'py, PyAny>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let conn = self.connection.clone();
        let builder = track.call_method0("into_songbird_track")?;
        let into_track = builder.downcast_exact::<IntoTrack>().unwrap();
        let track = into_track.get().build()?;
        future_into_py(py, async move {
            let handle = conn.enqueue(track).await?;
            PlayerHandler::from_handle(handle, conn.clone())
        })
    }

    fn skip(&self) -> PyResult<()> {
        Ok(self.connection.skip_queue()?)
    }

    fn stop(&self) -> PyResult<()> {
        Ok(self.connection.stop_queue()?)
    }

    fn resume(&self) -> PyResult<()> {
        Ok(self.connection.resume_queue()?)
    }

    fn dequeue<'py>(&self, py: Python<'py>, index: usize) -> PyResult<Bound<'py, PyAny>> {
        if let Some(handle) = self.connection.dequeue(index)? {
            let handler = PlayerHandler::from_handle(handle, self.connection.clone())?
                .into_pyobject(py)?
                .into_any();
            Ok(handler)
        } else {
            Ok(py.None().into_bound(py))
        }
    }
}

impl QueueHandler {
    pub fn new(connection: Arc<VoiceConnection>) -> Self {
        Self { connection }
    }
}
