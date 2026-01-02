use pyo3::{IntoPyObject, Python};
use pyo3_stub_gen::{PyStubType, TypeInfo};
use std::marker::PhantomData;

pub struct Generic<'py, T, G>(T, PhantomData<&'py G>)
where
    T: IntoPyObject<'py> + PyStubType,
    G: PyStubType;

impl<'py, T, G> Generic<'py, T, G>
where
    T: IntoPyObject<'py> + PyStubType,
    G: PyStubType,
{
    pub fn new(value: T) -> Self {
        Self(value, PhantomData)
    }
}

impl<'py, T, G> IntoPyObject<'py> for Generic<'py, T, G>
where
    T: IntoPyObject<'py> + PyStubType,
    G: PyStubType,
{
    type Target = T::Target;
    type Output = T::Output;
    type Error = T::Error;

    fn into_pyobject(self, py: Python<'py>) -> Result<Self::Output, Self::Error> {
        self.0.into_pyobject(py)
    }
}

impl<'py, T, G> PyStubType for Generic<'py, T, G>
where
    T: IntoPyObject<'py> + PyStubType,
    G: PyStubType,
{
    fn type_output() -> TypeInfo {
        let TypeInfo {
            name: t_name,
            import: mut t_import,
        } = T::type_output();
        let TypeInfo {
            name: g_name,
            import: g_import,
        } = G::type_output();
        t_import.extend(g_import);
        TypeInfo {
            name: format!("{t_name}[{g_name}]"),
            import: t_import,
        }
    }
}
