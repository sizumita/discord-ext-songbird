mod buffer;
mod client;
mod connection;
mod error;
mod player;
mod source;

use pyo3::prelude::*;
use tracing_subscriber::fmt;

/// A Python module implemented in Rust.
#[pymodule]
#[pyo3(name = "backend")]
fn discord_ext_songbird_backend(py: Python<'_>, m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<client::SongbirdBackend>()?;
    m.add_class::<source::AudioSource>()?;
    m.add_class::<source::raw::RawBufferSource>()?;
    m.add_class::<source::SourceComposed>()?;
    m.add_class::<player::PlayerHandler>()?;
    m.add("SongbirdError", py.get_type::<error::PySongbirdError>())?;
    m.add("JoinError", py.get_type::<error::PyJoinError>())?;
    m.add(
        "ConnectionNotInitialized",
        py.get_type::<error::PyConnectionNotInitialized>(),
    )?;
    Ok(())
}
