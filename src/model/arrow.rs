use arrow::array::Int16Array;
use pyo3::{Bound, IntoPyObject, PyAny, Python};
use pyo3_stub_gen::{PyStubType, TypeInfo};
use std::collections::HashSet;
use std::marker::PhantomData;

trait ArrayElementType {
    const ARROW_TYPE_NAME: &'static str;
}

pub struct ArrowArray<'py, T>(Bound<'py, PyAny>, PhantomData<T>)
where
    T: ArrayElementType;

impl<'py, T> PyStubType for ArrowArray<'py, T>
where
    T: ArrayElementType,
{
    fn type_output() -> TypeInfo {
        let mut imports = HashSet::new();
        imports.insert("pyarrow".into());
        TypeInfo {
            name: format!("pyarrow.{}", T::ARROW_TYPE_NAME),
            import: imports,
        }
    }
}

impl<'py, T> IntoPyObject<'py> for ArrowArray<'py, T>
where
    T: ArrayElementType,
{
    type Target = PyAny;
    type Output = Bound<'py, Self::Target>;
    type Error = pyo3::PyErr;
    fn into_pyobject(self, _py: Python<'py>) -> Result<Self::Output, Self::Error> {
        Ok(self.0)
    }
}

macro_rules! define_element {
    ($name:ident, $arrow_name:expr) => {
        impl ArrayElementType for $name {
            const ARROW_TYPE_NAME: &'static str = $arrow_name;
        }
    };
    ($name:ident) => {
        impl ArrayElementType for $name {
            const ARROW_TYPE_NAME: &'static str = stringify!($name);
        }
    };
}

define_element!(Int16Array);
