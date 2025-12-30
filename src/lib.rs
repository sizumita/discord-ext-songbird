#![doc = include_str!("../README.md")]

mod client;

use crate::client::SongbirdImpl;
use pyo3::prelude::*;
use pyo3_stub_gen::define_stub_info_gatherer;

/// A Python module implemented in Rust.
#[pymodule]
mod native {
    #[pymodule_export]
    use super::SongbirdImpl;
}

define_stub_info_gatherer!(stub_info);
