use crate::player::input::{PyCompose, PyInputBase};
use pyo3::{pyclass, pymethods, Bound, Py, PyAny, PyRefMut, PyResult, PyTraverseError, PyVisit, Python};
use pyo3_stub_gen::derive::{gen_stub_pyclass, gen_stub_pymethods};
use songbird::input::Input;
use songbird::tracks::{LoopState, PlayMode, Track};
use std::sync::Arc;

#[gen_stub_pyclass]
#[pyclass(name = "Track", module = "discord.ext.songbird.native.player")]
pub struct PyTrack {
    pub input: Py<PyInputBase>,
    mode: PlayMode,
    volume: f32,
    loops: LoopState,
}

#[gen_stub_pymethods]
#[pymethods]
impl PyTrack {
    #[new]
    pub fn new<'py>(input: Bound<'py, PyInputBase>) -> Self {
        Self {
            input: input.unbind(),
            mode: PlayMode::Play,
            volume: 1.0,
            loops: LoopState::Finite(0),
        }
    }

    fn play<'py>(mut slf: PyRefMut<'py, Self>) -> PyRefMut<'py, Self> {
        slf.mode = PlayMode::Play;
        slf
    }

    fn pause<'py>(mut slf: PyRefMut<'py, Self>) -> PyRefMut<'py, Self> {
        slf.mode = PlayMode::Pause;
        slf
    }

    fn stop<'py>(mut slf: PyRefMut<'py, Self>) -> PyRefMut<'py, Self> {
        slf.mode = PlayMode::Stop;
        slf
    }

    fn volume<'py>(mut slf: PyRefMut<'py, Self>, volume: f32) -> PyRefMut<'py, Self> {
        slf.volume = volume;
        slf
    }

    #[gen_stub(skip)]
    fn __traverse__(&self, visit: PyVisit<'_>) -> Result<(), PyTraverseError> {
        visit.call(&self.input)?;
        Ok(())
    }
}

impl PyTrack {
    pub fn to_track(&self, py: Python) -> PyResult<Track> {
        let mut compose = self.input.call_method0(py, "_compose")?
            .cast_bound::<PyCompose>(py)?.borrow_mut();
        let mut track = Track::new(
            Input::Lazy(compose.get_compose().unwrap())
        ).loops(self.loops)
            .volume(self.volume);
        track.playing = self.mode.clone();
        Ok(track)
    }
}
