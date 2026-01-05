use crate::error::IntoPyResult;
use crate::player::handle::PyTrackHandle;
use pyo3::{PyResult, pyclass, pymethods};
use pyo3_stub_gen::derive::{gen_stub_pyclass, gen_stub_pymethods};
use songbird::tracks::TrackQueue;

#[gen_stub_pyclass]
#[pyclass(name = "Queue", module = "discord.ext.songbird.native.player")]
/// Track queue controller.
///
/// Notes
/// -----
/// Exposes queue operations for the active voice call.
pub struct PyQueue {
    handle: TrackQueue,
}

impl PyQueue {
    pub fn new(handle: TrackQueue) -> Self {
        Self { handle }
    }
}

#[gen_stub_pymethods]
#[pymethods]
impl PyQueue {
    /// Return the currently playing track handle, if any.
    ///
    /// Returns
    /// -------
    /// TrackHandle | None
    fn current(&self) -> PyResult<Option<PyTrackHandle>> {
        Ok(self
            .handle
            .current()
            .map(|handle| PyTrackHandle::new(handle)))
    }

    /// Remove and return a queued track handle by index.
    ///
    /// Parameters
    /// ----------
    /// index : int
    ///     Zero-based index in the queue.
    ///
    /// Returns
    /// -------
    /// TrackHandle | None
    fn dequeue(&self, index: usize) -> PyResult<Option<PyTrackHandle>> {
        Ok(self
            .handle
            .dequeue(index)
            .map(|handle| PyTrackHandle::new(handle.handle())))
    }

    /// Check whether the queue is empty.
    ///
    /// Returns
    /// -------
    /// bool
    fn is_empty(&self) -> PyResult<bool> {
        Ok(self.handle.is_empty())
    }

    /// Pause all tracks in the queue.
    ///
    /// Returns
    /// -------
    /// None
    fn pause(&self) -> PyResult<()> {
        self.handle.pause().into_pyerr()
    }

    /// Resume all tracks in the queue.
    ///
    /// Returns
    /// -------
    /// None
    fn resume(&self) -> PyResult<()> {
        self.handle.resume().into_pyerr()
    }

    /// Stop playback and clear the queue.
    ///
    /// Returns
    /// -------
    /// None
    fn stop(&self) -> PyResult<()> {
        self.handle.stop();
        Ok(())
    }

    /// Skip the current track.
    ///
    /// Returns
    /// -------
    /// None
    fn skip(&self) -> PyResult<()> {
        self.handle.skip().into_pyerr()
    }

    /// Return handles for all queued tracks.
    ///
    /// Returns
    /// -------
    /// list[TrackHandle]
    fn tracks(&self) -> PyResult<Vec<PyTrackHandle>> {
        Ok(self
            .handle
            .current_queue()
            .iter()
            .map(|handle| PyTrackHandle::new(handle.clone()))
            .collect())
    }

    /// Return the number of queued tracks.
    ///
    /// Returns
    /// -------
    /// int
    fn __len__(&self) -> PyResult<usize> {
        Ok(self.handle.len())
    }

    /// Return a queued track handle by index.
    ///
    /// Parameters
    /// ----------
    /// item : int
    ///     Zero-based index in the queue.
    ///
    /// Returns
    /// -------
    /// TrackHandle | None
    fn __getitem__(&self, item: usize) -> PyResult<Option<PyTrackHandle>> {
        Ok(self
            .handle
            .current_queue()
            .get(item)
            .map(|x| PyTrackHandle::new(x.clone())))
    }
}
