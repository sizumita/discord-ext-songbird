use pyo3::{Bound, IntoPyObject, PyAny, PyErr, PyResult, Python};
use pyo3_stub_gen::{PyStubType, TypeInfo};
use std::future::Future;
use std::marker::PhantomData;

pub struct PyFuture<'py, T>(Bound<'py, PyAny>, PhantomData<T>)
where
    T: PyStubType;

impl<'py, T> From<Bound<'py, PyAny>> for PyFuture<'py, T>
where
    T: PyStubType,
{
    fn from(value: Bound<'py, PyAny>) -> Self {
        Self(value, PhantomData)
    }
}

impl<T> PyStubType for PyFuture<'_, T>
where
    T: PyStubType,
{
    fn type_output() -> TypeInfo {
        let TypeInfo { name, mut import } = T::type_output();
        import.insert("typing".into());
        TypeInfo {
            name: format!("typing.Coroutine[typing.Any, typing.Any, {name}]"),
            import,
        }
    }
}

impl<'py, T> IntoPyObject<'py> for PyFuture<'py, T>
where
    T: PyStubType,
{
    type Target = PyAny;
    type Output = Bound<'py, Self::Target>;
    type Error = PyErr;

    fn into_pyobject(self, _py: Python<'py>) -> Result<Self::Output, Self::Error> {
        Ok(self.0)
    }
}

impl<'py, T> PyFuture<'py, T>
where
    T: PyStubType + for<'p> IntoPyObject<'p> + Send + 'static,
{
    pub fn try_new<F>(py: Python<'py>, fut: F) -> PyResult<Self>
    where
        F: Future<Output = PyResult<T>> + Send + 'static,
    {
        let fut = pyo3_async_runtimes::tokio::future_into_py(py, fut);
        fut.map(|fut| Self(fut, PhantomData))
    }
}
