use std::num::NonZeroU64;
use std::sync::Arc;
use std::sync::mpsc::channel;
use pyo3::{pyclass, pymethods, Bound, Py, PyAny, PyObject, PyResult, Python};
use pyo3::types::PyDict;
use pyo3_async_runtimes::tokio::future_into_py;
use songbird::id::{ChannelId, UserId, GuildId};
use tokio::sync::Mutex;
use crate::connection::{DpyVoiceUpdate, VoiceConnection};
use crate::error::{SongbirdError, SongbirdResult};

#[pyclass]
pub struct SongbirdClient {
    connection: Arc<VoiceConnection>,
}

#[pymethods]
impl SongbirdClient {
    #[new]
    fn new<'py>(py: Python<'py>, channel_id: u64) -> PyResult<Self> {
        use std::panic;

        panic::set_hook(Box::new(|panic_info| {
            println!("{}", panic_info);
        }));
        let connection = Arc::new(VoiceConnection::new(
            NonZeroU64::new(channel_id).unwrap(),
        ));
        Ok(Self {
            connection
        })
    }

    fn start<'py>(&self, py: Python<'py>, shard_hook: Py<PyAny>, client_id: u64, guild_id: u64) -> PyResult<Bound<'py, PyAny>> {
        let conn = self.connection.clone();
        future_into_py(py, async move {
            conn.start(DpyVoiceUpdate::new(conn.clone(), shard_hook), NonZeroU64::new(client_id).unwrap(), NonZeroU64::new(guild_id).unwrap()).await;
            Ok(())
        })
    }

    fn on_server_update<'py>(&self, py: Python<'py>, endpoint: String, token: String) -> PyResult<Bound<'py, PyAny>> {
        let conn = self.connection.clone();
        future_into_py(py, async move {
            Ok(conn.update_server(endpoint, token).await?)
        })
    }

    fn on_voice_state_update<'py>(&self, py: Python<'py>, session_id: String, channel_id: Option<u64>) -> PyResult<Bound<'py, PyAny>> {
        let conn = self.connection.clone();
        future_into_py(py, async move {
            conn.update_state(session_id, channel_id.map(|x| NonZeroU64::new(x).unwrap())).await?;
            Ok(())
        })
    }

    fn connect<'py>(&self, py: Python<'py>, timeout: f32, self_deaf: bool, self_mute: bool) -> PyResult<Bound<'py, PyAny>> {
        let conn = self.connection.clone();
        future_into_py(py, async move {
            Ok(conn.connect(timeout, self_deaf, self_mute).await?)
        })
    }

    fn leave<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let conn = self.connection.clone();
        future_into_py(py, async move {
            Ok(conn.leave().await?)
        })
    }
}
