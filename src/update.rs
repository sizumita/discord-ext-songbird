use async_trait::async_trait;
use pyo3::{Py, PyAny, PyResult, Python};
use songbird::error::JoinResult;
use songbird::id::{ChannelId, GuildId};
use songbird::shards::VoiceUpdate;

pub struct VoiceUpdater(pub Py<PyAny>);

#[async_trait]
impl VoiceUpdate for VoiceUpdater {
    async fn update_voice_state(
        &self,
        _guild_id: GuildId,
        channel_id: Option<ChannelId>,
        self_deaf: bool,
        self_mute: bool,
    ) -> JoinResult<()> {
        let future = Python::attach(|py| -> PyResult<_> {
            let coroutine = self
                .0
                .call_method1(
                    py,
                    "update_hook",
                    (channel_id.map(|c| c.0), self_mute, self_deaf),
                )?
                .into_bound(py);
            pyo3_async_runtimes::tokio::into_future(coroutine)
        })
        .map_err(|_e| songbird::error::JoinError::Dropped)?;
        future
            .await
            .map_err(|_e| songbird::error::JoinError::Dropped)?;
        Ok(())
    }
}
