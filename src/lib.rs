mod backend;

use pyo3::prelude::*;

/// A Python module implemented in Rust.
#[pymodule]
#[pyo3(name="backend")]
fn discord_ext_streaming_backend(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<backend::StreamingBackend>()?;
    Ok(())
}
