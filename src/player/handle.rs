use crate::error::PyControlError;
use pyo3::{pyclass, pymethods, PyResult};
use pyo3_stub_gen::derive::gen_stub_pyclass;
use songbird::tracks::TrackHandle;

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

#[pymethods]
impl PyTrackHandle {
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
}

impl PyTrackHandle {
    pub fn new(inner: TrackHandle) -> Self {
        Self { inner }
    }
}
