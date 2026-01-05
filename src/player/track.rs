use crate::player::input::{PyCompose, PyInputBase};
use pyo3::{
    Bound, Py, PyAny, PyRefMut, PyResult, PyTraverseError, PyVisit, Python, pyclass, pymethods,
};
use pyo3_stub_gen::derive::{gen_stub_pyclass, gen_stub_pymethods};
use songbird::tracks::{LoopState, PlayMode, Track};

#[gen_stub_pyclass]
#[pyclass(name = "Track", module = "discord.ext.songbird.native.player")]
/// Playable audio track.
///
/// Notes
/// -----
/// Tracks are created from an `InputBase` and played via the voice client.
///
/// Examples
/// --------
/// ```python
/// track = player.Track(source)
/// track = track.volume(0.8)
/// ```
pub struct PyTrack {
    pub input: Option<Py<PyInputBase>>,
    mode: PlayMode,
    volume: f32,
    loops: LoopState,
}

#[gen_stub_pymethods]
#[pymethods]
impl PyTrack {
    #[new]
    /// Create a new track from an input source.
    ///
    /// Parameters
    /// ----------
    /// input : InputBase
    ///     The audio input source.
    ///
    /// Returns
    /// -------
    /// Track
    pub fn new<'py>(input: Bound<'py, PyInputBase>) -> Self {
        Self {
            input: Some(input.unbind()),
            mode: PlayMode::Play,
            volume: 1.0,
            loops: LoopState::Finite(0),
        }
    }

    /// Mark this track as playing.
    ///
    /// Returns
    /// -------
    /// Track
    ///     This track.
    fn play<'py>(mut slf: PyRefMut<'py, Self>) -> PyRefMut<'py, Self> {
        slf.mode = PlayMode::Play;
        slf
    }

    /// Mark this track as paused.
    ///
    /// Returns
    /// -------
    /// Track
    ///     This track.
    fn pause<'py>(mut slf: PyRefMut<'py, Self>) -> PyRefMut<'py, Self> {
        slf.mode = PlayMode::Pause;
        slf
    }

    /// Mark this track as stopped.
    ///
    /// Returns
    /// -------
    /// Track
    ///     This track.
    fn stop<'py>(mut slf: PyRefMut<'py, Self>) -> PyRefMut<'py, Self> {
        slf.mode = PlayMode::Stop;
        slf
    }

    /// Set the track volume multiplier.
    ///
    /// Parameters
    /// ----------
    /// volume : float
    ///     Volume multiplier.
    ///
    /// Returns
    /// -------
    /// Track
    ///     This track.
    fn volume<'py>(mut slf: PyRefMut<'py, Self>, volume: f32) -> PyRefMut<'py, Self> {
        slf.volume = volume;
        slf
    }

    #[gen_stub(skip)]
    fn __traverse__(&self, visit: PyVisit<'_>) -> Result<(), PyTraverseError> {
        if let Some(input) = &self.input {
            visit.call(input)?;
        }
        Ok(())
    }

    #[gen_stub(skip)]
    fn __clear__(&mut self) {
        // Clear reference, this decrements ref counter.
        self.input = None;
    }
}

impl PyTrack {
    pub fn to_track(&self, py: Python, current_loop: Py<PyAny>) -> PyResult<Track> {
        let input = self.input.as_ref().ok_or_else(|| {
            pyo3::exceptions::PyRuntimeError::new_err("Track input has been cleared")
        })?;
        let mut compose = input
            .call_method1(py, "_compose", (current_loop,))?
            .cast_bound::<PyCompose>(py)?
            .borrow_mut();
        let mut track = Track::new(compose.get_input().unwrap())
            .loops(self.loops)
            .volume(self.volume);
        track.playing = self.mode.clone();
        Ok(track)
    }
}
