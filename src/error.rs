use pyo3::PyResult;
use pyo3_stub_gen::create_exception;
use songbird::error::JoinError;

create_exception!(
    discord.ext.songbird.native.error,
    PySongbirdError,
    pyo3::exceptions::PyException
);
create_exception!(
    discord.ext.songbird.native.error,
    PyJoinError,
    PySongbirdError
);

pub trait IntoPyResult<T> {
    fn into_py(self) -> PyResult<T>;
}

impl<T> IntoPyResult<T> for Result<T, JoinError> {
    fn into_py(self) -> PyResult<T> {
        self.map_err(|err| PyJoinError::new_err(err.to_string()))
    }
}
