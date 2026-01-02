mod buffer;

use pyo3::{pyclass, PyResult};
use pyo3_stub_gen::derive::gen_stub_pyclass;
use songbird::{Event, EventHandler};
use std::collections::HashSet;
use std::sync::Arc;

pub use buffer::BufferSink;

#[gen_stub_pyclass]
#[pyclass(subclass, module = "discord.ext.songbird.native.receive")]
pub struct SinkBase {
    can_multi_subscribe: bool,
    subscriber: Arc<dyn EventHandler + Send>,
    pub receive_events: HashSet<Event>,
}

impl SinkBase {
    fn new(
        can_multi_subscribe: bool,
        subscriber: Arc<dyn EventHandler + Send>,
        receive_events: HashSet<Event>,
    ) -> PyResult<Self> {
        Ok(Self {
            can_multi_subscribe,
            subscriber,
            receive_events,
        })
    }

    pub fn get_subscriber(&self) -> Arc<dyn EventHandler + Send> {
        self.subscriber.clone()
    }
}
