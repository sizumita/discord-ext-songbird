use crate::connection::{DpyVoiceUpdate, VoiceConnection};
use crate::error::{SongbirdError, SongbirdResult};
use crate::queue::QueueHandler;
use pyo3::{pyclass, pymethods, Bound, Py, PyAny, PyResult, Python};
use pyo3_async_runtimes::tokio::future_into_py;
use std::num::NonZeroU64;
use std::sync::Arc;

#[pyclass]
pub struct SongbirdBackend {
    connection: Arc<VoiceConnection>,
    #[pyo3(get)]
    queue: Py<QueueHandler>,
}

#[pymethods]
impl SongbirdBackend {
    #[new]
    pub fn new(py: Python<'_>, channel_id: u64) -> PyResult<Self> {
        let connection = Arc::new(VoiceConnection::new(non_zero_u64(channel_id)?));
        let handler = QueueHandler::new(connection.clone());
        Ok(Self {
            connection,
            queue: Py::new(py, handler)?,
        })
    }

    pub fn start<'py>(
        &self,
        py: Python<'py>,
        shard_hook: Py<PyAny>,
        client_id: u64,
        guild_id: u64,
    ) -> PyResult<Bound<'py, PyAny>> {
        let conn = self.connection.clone();
        future_into_py(py, async move {
            conn.start(
                DpyVoiceUpdate::new(shard_hook),
                non_zero_u64(client_id)?,
                non_zero_u64(guild_id)?,
            )
            .await;
            Ok(())
        })
    }

    pub fn on_server_update<'py>(
        &self,
        py: Python<'py>,
        endpoint: String,
        token: String,
    ) -> PyResult<Bound<'py, PyAny>> {
        let conn = self.connection.clone();
        future_into_py(
            py,
            async move { Ok(conn.update_server(endpoint, token).await?) },
        )
    }

    #[pyo3(signature = (session_id, channel_id=None))]
    pub fn on_voice_state_update<'py>(
        &self,
        py: Python<'py>,
        session_id: String,
        channel_id: Option<u64>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let conn = self.connection.clone();
        future_into_py(py, async move {
            conn.update_state(session_id, channel_id.and_then(NonZeroU64::new))
                .await?;
            Ok(())
        })
    }

    pub fn connect<'py>(
        &self,
        py: Python<'py>,
        timeout: f32,
        self_deaf: bool,
        self_mute: bool,
    ) -> PyResult<Bound<'py, PyAny>> {
        let conn = self.connection.clone();
        future_into_py(py, async move {
            Ok(conn.connect(timeout, self_deaf, self_mute).await?)
        })
    }

    pub fn leave<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let conn = self.connection.clone();
        future_into_py(py, async move { Ok(conn.leave().await?) })
    }

    pub fn mute<'py>(&self, py: Python<'py>, mute: bool) -> PyResult<Bound<'py, PyAny>> {
        let conn = self.connection.clone();
        future_into_py(py, async move { Ok(conn.mute(mute).await?) })
    }

    pub fn deafen<'py>(&self, py: Python<'py>, deaf: bool) -> PyResult<Bound<'py, PyAny>> {
        let conn = self.connection.clone();
        future_into_py(py, async move { Ok(conn.deafen(deaf).await?) })
    }

    pub fn is_deaf(&self) -> PyResult<bool> {
        Ok(self.connection.is_deaf()?)
    }

    pub fn is_mute(&self) -> PyResult<bool> {
        Ok(self.connection.is_mute()?)
    }

    pub fn move_to<'py>(&self, py: Python<'py>, channel_id: u64) -> PyResult<Bound<'py, PyAny>> {
        let conn = self.connection.clone();
        future_into_py(py, async move {
            Ok(conn.move_to(non_zero_u64(channel_id)?).await?)
        })
    }
}

#[inline]
fn non_zero_u64(val: u64) -> SongbirdResult<NonZeroU64> {
    NonZeroU64::new(val)
        .map(Ok)
        .unwrap_or_else(|| Err(SongbirdError::InvalidId))
}
