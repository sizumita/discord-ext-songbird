use pyo3::prelude::*;

#[pyclass]
pub struct StreamingBackend {
}

#[pymethods]
impl StreamingBackend {
    #[new]
    fn new() -> Self {
        Self {

        }
    }
}
