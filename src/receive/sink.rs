mod buffer;

use pyo3::{pyclass, PyResult};
use pyo3_stub_gen::derive::gen_stub_pyclass;
use songbird::{Event, EventHandler};
use std::collections::HashSet;
use std::sync::Arc;

pub use buffer::BufferSink;

#[gen_stub_pyclass]
#[pyclass(subclass, module = "discord.ext.songbird.native.receive")]
/// Base class for receive sinks.
///
/// Notes
/// -----
/// This is an internal type exposed to Python for sink registration.
/// Custom sinks are not currently supported from Python.
pub struct SinkBase {
    subscriber: Arc<dyn EventHandler + Send>,
    pub receive_events: HashSet<Event>,
}

impl SinkBase {
    fn new(
        subscriber: Arc<dyn EventHandler + Send>,
        receive_events: HashSet<Event>,
    ) -> PyResult<Self> {
        Ok(Self {
            subscriber,
            receive_events,
        })
    }

    pub fn get_subscriber(&self) -> Arc<dyn EventHandler + Send> {
        self.subscriber.clone()
    }
}
