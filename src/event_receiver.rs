use discortp::Packet;
use pyo3::prelude::*;
use pyo3::IntoPyObjectExt;
use pyo3_async_runtimes::tokio::future_into_py;
use songbird::EventContext;
use tracing::event;

/// The event receiver.
#[pyclass(subclass)]
pub struct VoiceEventReceiver {}

#[pymethods]
impl VoiceEventReceiver {
    #[new]
    fn new() -> Self {
        Self {}
    }

    fn act<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        future_into_py(py, async move { Ok(1) })
    }

    fn get_method_some<'py>(self_: PyRef<'py, Self>, py: Python<'py>) -> Py<PyAny> {
        self_
            .into_py(py)
            .call_method0(py, "act")
            .unwrap()
            .into_any()
    }
}
