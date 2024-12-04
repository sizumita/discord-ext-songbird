use pyo3::exceptions::PyValueError;
use pyo3::prelude::*;
use songbird::tracks::TrackHandle;

#[pyclass(frozen)]
#[derive(Debug)]
pub struct PlayerHandler {
    pub(crate) handle: TrackHandle,
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
