use pyo3::prelude::*;
use songbird::driver::CryptoMode;

/// See [`songbird::driver::CryptoMode`]
#[pyclass(name = "CryptoMode", frozen, eq, eq_int)]
#[derive(PartialEq)]
pub enum PyCryptoMode {
    /// See [`songbird::driver::CryptoMode::Aes256Gcm`]
    Aes256Gcm,
    /// See [`songbird::driver::CryptoMode::XChaCha20Poly1305`]
    XChaCha20Poly1305,
}

impl From<&'_ PyCryptoMode> for CryptoMode {
    fn from(value: &PyCryptoMode) -> Self {
        match value {
            PyCryptoMode::Aes256Gcm => Self::Aes256Gcm,
            PyCryptoMode::XChaCha20Poly1305 => Self::XChaCha20Poly1305,
        }
    }
}
