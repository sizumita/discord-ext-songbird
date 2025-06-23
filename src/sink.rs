use pyo3::prelude::*;
use songbird::events::context_data::VoiceTick;
use songbird::EventContext;

#[pyclass(subclass)]
pub struct AudioSink {}

#[pymethods]
impl AudioSink {
    #[new]
    fn new() -> Self {
        Self {}
    }
}

impl AudioSink {
    pub fn receive_tick(&mut self, tick: &VoiceTick) {}
}
