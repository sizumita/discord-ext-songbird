use pyo3::{create_exception, PyErr};
use thiserror::Error;
use songbird::error::JoinError;


#[derive(Debug, Error)]
pub enum SongbirdError {
    #[error("Connection not started. Please call .start/3 before call this function")]
    ConnectionNotStarted,
    #[error("Passing Message failed")]
    JoinError(#[from] JoinError),
}


create_exception!(module, PySongbirdError, pyo3::exceptions::PyException);

create_exception!(module, PyConnectionNotInitialized, PySongbirdError);
create_exception!(module, PyJoinError, PySongbirdError);


impl From<SongbirdError> for PyErr {
    fn from(error: SongbirdError) -> Self {
        match error {
            SongbirdError::ConnectionNotStarted => PyConnectionNotInitialized::new_err(error.to_string()),
            SongbirdError::JoinError(e) => PyJoinError::new_err(e.to_string()),
        }
    }
}

pub type SongbirdResult<T> = Result<T, SongbirdError>;
