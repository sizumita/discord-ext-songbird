mod buffer;
mod stream;

use super::identity::{VoiceIdentityBindError, VoiceIdentityBinding, VoiceIdentityMap};
use pyo3::exceptions::PyRuntimeError;
use pyo3::{PyResult, pyclass};
use pyo3_stub_gen::derive::gen_stub_pyclass;
use songbird::{Event, EventHandler};
use std::collections::HashSet;
use std::sync::Arc;

pub use buffer::BufferSink;
pub use stream::{PyStream, StreamSink};

#[gen_stub_pyclass]
#[pyclass(
    subclass,
    module = "discord.ext.songbird.native.receive",
    skip_from_py_object
)]
/// Base class for receive sinks.
///
/// Notes
/// -----
/// This is an internal type exposed to Python for sink registration.
/// Custom sinks are not currently supported from Python.
pub struct SinkBase {
    subscriber: Arc<dyn EventHandler + Send + Sync>,
    identity: Arc<VoiceIdentityBinding>,
    pub receive_events: HashSet<Event>,
}

impl SinkBase {
    fn new(
        subscriber: Arc<dyn EventHandler + Send + Sync>,
        identity: Arc<VoiceIdentityBinding>,
        receive_events: HashSet<Event>,
    ) -> PyResult<Self> {
        Ok(Self {
            subscriber,
            identity,
            receive_events,
        })
    }

    pub fn get_subscriber(&self) -> Arc<dyn EventHandler + Send + Sync> {
        self.subscriber.clone()
    }

    pub fn bind_identity(&self, map: Arc<VoiceIdentityMap>) -> PyResult<()> {
        self.identity.bind(map).map_err(|err| match err {
            VoiceIdentityBindError::DifferentConnection => {
                PyRuntimeError::new_err("receive sinks cannot be reused across voice connections")
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use songbird::{CoreEvent, EventContext};

    struct NoopHandler;

    #[async_trait]
    impl EventHandler for NoopHandler {
        async fn act(&self, _ctx: &EventContext<'_>) -> Option<Event> {
            None
        }
    }

    #[test]
    fn sink_rejects_binding_to_different_voice_connection() {
        let identity = Arc::new(VoiceIdentityBinding::default());
        let sink = SinkBase::new(
            Arc::new(NoopHandler),
            identity,
            vec![Event::Core(CoreEvent::VoiceTick)]
                .into_iter()
                .collect(),
        )
        .expect("sink construction should succeed");
        let first = Arc::new(VoiceIdentityMap::default());
        let second = Arc::new(VoiceIdentityMap::default());

        assert!(sink.bind_identity(first.clone()).is_ok());
        assert!(sink.bind_identity(first).is_ok());
        assert!(sink.bind_identity(second).is_err());
    }
}
