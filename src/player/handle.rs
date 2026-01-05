use crate::error::PyControlError;
use crate::model::PyFuture;
use pyo3::{PyResult, Python, pyclass, pymethods};
use pyo3_async_runtimes::tokio::future_into_py;
use pyo3_stub_gen::derive::{gen_stub_pyclass, gen_stub_pymethods};
use songbird::tracks::TrackHandle;
use std::time::Duration;

#[gen_stub_pyclass]
#[pyclass(name = "TrackHandle", module = "discord.ext.songbird.native.player")]
/// Handle for controlling a playing track.
///
/// Notes
/// -----
/// Returned by `SongbirdImpl.play`.
pub struct PyTrackHandle {
    inner: TrackHandle,
}

#[gen_stub_pymethods]
#[pymethods]
impl PyTrackHandle {
    fn seek<'py>(&self, py: Python<'py>, position: Duration) -> PyResult<PyFuture<'py, Duration>> {
        let inner = self.inner.clone();
        future_into_py(py, async move {
            let d = inner
                .seek_async(position)
                .await
                .map_err(|err| PyControlError::new_err(err.to_string()))?;
            Ok(d)
        })
        .map(|x| x.into())
    }
    /// Resume playback.
    ///
    /// Returns
    /// -------
    /// None
    fn play(&self) -> PyResult<()> {
        self.inner
            .play()
            .map_err(|err| PyControlError::new_err(err.to_string()))?;
        Ok(())
    }

    /// Pause playback.
    ///
    /// Returns
    /// -------
    /// None
    fn pause(&self) -> PyResult<()> {
        self.inner
            .pause()
            .map_err(|err| PyControlError::new_err(err.to_string()))?;
        Ok(())
    }

    /// Stop playback.
    ///
    /// Returns
    /// -------
    /// None
    fn stop(&self) -> PyResult<()> {
        self.inner
            .stop()
            .map_err(|err| PyControlError::new_err(err.to_string()))?;
        Ok(())
    }

    fn enable_loop(&self) -> PyResult<()> {
        self.inner
            .enable_loop()
            .map_err(|err| PyControlError::new_err(err.to_string()))?;
        Ok(())
    }

    fn disable_loop(&self) -> PyResult<()> {
        self.inner
            .disable_loop()
            .map_err(|err| PyControlError::new_err(err.to_string()))?;
        Ok(())
    }

    fn loop_for(&self, times: usize) -> PyResult<()> {
        self.inner
            .loop_for(times)
            .map_err(|err| PyControlError::new_err(err.to_string()))?;
        Ok(())
    }
}

impl PyTrackHandle {
    pub fn new(inner: TrackHandle) -> Self {
        Self { inner }
    }
}
