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
        guild_id: GuildId,
        channel_id: Option<ChannelId>,
        self_deaf: bool,
        self_mute: bool,
    ) -> JoinResult<()> {
        let channel_id_value = channel_id.map(|c| c.0);
        log::trace!(
            "Voice state update (guild {}, channel_id={:?}, self_mute={}, self_deaf={})",
            guild_id,
            channel_id_value,
            self_mute,
            self_deaf
        );
        let future = Python::attach(|py| -> PyResult<_> {
            let coroutine = self
                .0
                .call_method1(py, "update_hook", (channel_id_value, self_mute, self_deaf))?
                .into_bound(py);
            pyo3_async_runtimes::tokio::into_future(coroutine)
        })
        .map_err(|err| {
            log::warn!(
                "Failed to schedule update_hook (guild {}): {}",
                guild_id,
                err
            );
            songbird::error::JoinError::Dropped
        })?;
        future.await.map_err(|err| {
            log::warn!("update_hook failed (guild {}): {}", guild_id, err);
            songbird::error::JoinError::Dropped
        })?;
        Ok(())
    }
}
