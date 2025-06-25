use pyo3::prelude::*;
use songbird::driver::{Channels, DecodeMode};

/// See [`songbird::driver::DecodeMode`]
#[pyclass(name = "DecodeMode", frozen, eq, eq_int)]
#[derive(PartialEq)]
pub enum PyDecodeMode {
    /// See [`songbird::driver::DecodeMode::Pass`]
    Pass,
    /// See [`songbird::driver::DecodeMode::Decrypt`]
    Decrypt,
    /// See [`songbird::driver::DecodeMode::Decode`]
    Decode,
}

impl From<&'_ PyDecodeMode> for DecodeMode {
    fn from(value: &'_ PyDecodeMode) -> Self {
        match value {
            PyDecodeMode::Pass => Self::Pass,
            PyDecodeMode::Decrypt => Self::Decrypt,
            PyDecodeMode::Decode => Self::Decode,
        }
    }
}

/// See [`songbird::driver::Channels`]
#[pyclass(name = "Channels", frozen, eq, eq_int)]
#[derive(PartialEq)]
pub enum PyChannels {
    /// See [`songbird::driver::Channels::Mono`]
    Mono,
    /// See [`songbird::driver::Channels::Stereo`]
    Stereo,
}

impl From<&'_ PyChannels> for Channels {
    fn from(value: &'_ PyChannels) -> Self {
        match value {
            PyChannels::Mono => Self::Mono,
            PyChannels::Stereo => Self::Stereo,
        }
    }
}
