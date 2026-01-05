use crate::model::future::PyFuture;
use futures::StreamExt;
use pyo3::{IntoPyObjectExt, Py, PyAny, PyRef, PyResult, Python, pyclass, pymethods};
use pyo3_async_runtimes::tokio::future_into_py;
use pyo3_stub_gen::derive::gen_stub_pymethods;
use pyo3_stub_gen::inventory::submit;
use pyo3_stub_gen::type_info::PyClassInfo;
use pyo3_stub_gen::{PyStubType, TypeInfo};
use std::any::TypeId;
use std::pin::Pin;
use std::sync::Arc;
use tokio::sync::Mutex;
use tokio_stream::Stream;

type AsyncStream = Arc<Mutex<Pin<Box<dyn Stream<Item = PyResult<Py<PyAny>>> + Send + 'static>>>>;

#[pyclass(module = "discord.ext.songbird.native.model")]
/// Async iterator wrapper used by the native module.
///
/// Notes
/// -----
/// This is an internal type and not meant to be constructed directly.
pub struct PyAsyncIterator {
    stream: AsyncStream,
}

impl PyStubType for PyAsyncIterator {
    fn type_output() -> TypeInfo {
        TypeInfo::locally_defined(
            "PyAsyncIterator",
            "discord.ext.songbird.native.model".into(),
        )
    }
}

submit! {
    PyClassInfo {
        struct_id: TypeId::of::<PyAsyncIterator>,
        pyclass_name: "PyAsyncIterator[T]",
        module: Some("discord.ext.songbird.native.model"),
        doc: "Async iterator wrapper used by the native module.\n\nNotes\n-----\nThis is an internal type and not meant to be constructed directly.",
        getters: &[],
        setters: &[],
        bases: &[],
        has_eq: false,
        has_ord: false,
        has_hash: false,
        has_str: false,
        subclass: false,
    }
}

impl PyAsyncIterator {
    pub fn new<T, S>(stream: S) -> Self
    where
        S: Stream<Item = T> + Send + 'static,
        for<'py> T: IntoPyObjectExt<'py>,
    {
        let stream = stream.map(|x| Python::attach(|py| x.into_py_any(py)));
        Self {
            stream: Arc::new(Mutex::new(Box::pin(stream))),
        }
    }

    pub fn new_in_raw<S>(stream: S) -> Self
    where
        S: Stream<Item = PyResult<Py<PyAny>>> + Send + 'static,
    {
        Self {
            stream: Arc::new(Mutex::new(Box::pin(stream))),
        }
    }
}

#[gen_stub_pymethods]
#[pymethods]
impl PyAsyncIterator {
    #[gen_stub(override_return_type(type_repr = "PyAsyncIterator[T]", imports = ("typing")))]
    /// Return self as an async iterator.
    ///
    /// Returns
    /// -------
    /// PyAsyncIterator[T]
    fn __aiter__(slf: PyRef<Self>) -> PyRef<Self> {
        slf
    }

    #[gen_stub(override_return_type(type_repr = "typing.Coroutine[typing.Any, typing.Any, T]", imports = ("typing")))]
    /// Await the next item from the iterator.
    ///
    /// Returns
    /// -------
    /// typing.Coroutine[typing.Any, typing.Any, T]
    fn __anext__<'py>(&self, py: Python<'py>) -> PyResult<PyFuture<'py, PyAny>> {
        let stream = self.stream.clone();
        future_into_py(py, async move {
            let mut locked = stream.lock().await;

            match locked.next().await {
                Some(item) => item,
                None => Err(pyo3::exceptions::PyStopAsyncIteration::new_err(())),
            }
        })
        .map(|x| x.into())
    }
}
