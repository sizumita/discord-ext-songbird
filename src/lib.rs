mod connection;
mod client;
mod error;

use pyo3::prelude::*;


/// A Python module implemented in Rust.
#[pymodule]
#[pyo3(name="backend")]
fn discord_ext_songbird_backend(py: Python<'_>, m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<client::SongbirdClient>()?;
    m.add("SongbirdError", py.get_type::<error::PySongbirdError>())?;
    m.add("JoinError", py.get_type::<error::PyJoinError>())?;
    m.add("ConnectionNotInitialized", py.get_type::<error::PyConnectionNotInitialized>())?;
    Ok(())
}
