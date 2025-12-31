#![doc = include_str!("../README.md")]

mod client;
mod error;
mod future;
mod update;

use crate::client::SongbirdImpl;
use pyo3::prelude::*;
use pyo3_stub_gen::define_stub_info_gatherer;

/// A Python module implemented in Rust.
#[pymodule]
mod native {
    use pyo3::prelude::*;

    #[pymodule_export]
    use super::SongbirdImpl;

    #[pymodule]
    mod error {
        #[pymodule_export]
        use crate::error::PyJoinError;
        #[pymodule_export]
        use crate::error::PySongbirdError;
    }
}

define_stub_info_gatherer!(stub_info);
