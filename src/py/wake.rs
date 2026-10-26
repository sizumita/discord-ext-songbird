//! `native.WakeBell` (§5.4): adopts the write end of the wake socket pair.

use std::sync::{Mutex, PoisonError};

use pyo3::prelude::*;
use pyo3_stub_gen::derive::{gen_stub_pyclass, gen_stub_pymethods};
use sbcore::wake::Doorbell;

use crate::errors::ensure_not_forked;

/// Owner of the doorbell socket (the write end of ``socket.socketpair()``).
///
/// Parameters
/// ----------
/// fd : int
///     A socket file descriptor (``wsock.detach()``; a ``SOCKET`` on Windows). Ownership moves to
///     Rust immediately: the descriptor is made non-blocking (and ``SO_NOSIGPIPE`` on Apple
///     platforms) and is closed exactly once, by this object or by the ``Connection`` that takes it.
///
/// Raises
/// ------
/// OSError
///     If the descriptor is invalid, closed or not a socket (a validated descriptor is closed).
///
/// Notes
/// -----
/// ``Connection(bell, …)`` moves the doorbell out; :attr:`taken` then becomes ``True``.
#[gen_stub_pyclass]
#[pyclass(module = "discord.ext.songbird.native", frozen)]
pub struct WakeBell {
    bell: Mutex<Option<Doorbell>>,
}

impl WakeBell {
    /// Moves the doorbell out (once). `None` if it was already taken.
    pub fn take(&self) -> Option<Doorbell> {
        self.bell
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
    }
}

#[gen_stub_pymethods]
#[pymethods]
impl WakeBell {
    #[new]
    fn py_new(py: Python<'_>, fd: i64) -> PyResult<Self> {
        ensure_not_forked(py)?;
        // SAFETY: the Python side hands over ownership (`socket.detach()`); single integer argument,
        // so extraction cannot fail after the descriptor has left Python.
        let bell = unsafe { Doorbell::adopt(fd) }?;
        Ok(WakeBell {
            bell: Mutex::new(Some(bell)),
        })
    }

    /// Whether a ``Connection`` has taken the doorbell.
    #[getter]
    fn taken(&self) -> bool {
        self.bell
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .is_none()
    }
}
