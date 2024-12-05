use crate::error::{SongbirdError, SongbirdResult};
use async_trait::async_trait;
use pyo3::{Py, PyAny, Python};
use songbird::error::{JoinError, JoinResult};
use songbird::id::{ChannelId, GuildId};
use songbird::shards::{Shard, VoiceUpdate};
use songbird::tracks::{Track, TrackHandle};
use songbird::{Call, Config};
use std::fmt::Debug;
use std::num::NonZeroU64;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;

pub struct VoiceConnection {
    channel_id: NonZeroU64,
    call: Arc<Mutex<Option<Call>>>,
}

impl VoiceConnection {
    pub fn new(channel_id: NonZeroU64) -> Self {
        Self {
            call: Arc::new(Mutex::new(None)),
            channel_id,
        }
    }

    pub async fn start(
        &self,
        updater: DpyVoiceUpdate,
        client_id: NonZeroU64,
        guild_id: NonZeroU64,
    ) {
        let shard = Shard::Generic(Arc::new(updater));
        let config = Config::default();

        let call = Call::from_config(guild_id, shard, client_id, config);
        {
            let mut handler = self.call.lock().await;
            *handler = Some(call);
        }
    }

    pub async fn update_server(&self, endpoint: String, token: String) -> SongbirdResult<()> {
        let Some(handler) = &mut *self.call.lock().await else {
            return Err(SongbirdError::ConnectionNotStarted);
        };
        handler.update_server(endpoint, token);
        Ok(())
    }

    pub async fn update_state<C>(
        &self,
        session_id: String,
        channel_id: Option<C>,
    ) -> SongbirdResult<()>
    where
        C: Into<ChannelId> + Debug,
    {
        let Some(handler) = &mut *self.call.lock().await else {
            return Err(SongbirdError::ConnectionNotStarted);
        };
        handler.update_state(session_id, channel_id);
        Ok(())
    }

    pub async fn connect(
        &self,
        timeout: f32,
        self_deaf: bool,
        self_mute: bool,
    ) -> SongbirdResult<()> {
        let joined = {
            let Some(handler) = &mut *self.call.lock().await else {
                return Err(SongbirdError::ConnectionNotStarted);
            };
            let config = handler
                .config()
                .clone()
                .gateway_timeout(Some(Duration::from_secs_f32(timeout)));
            handler.set_config(config);
            handler.mute(self_mute).await?;
            handler.deafen(self_deaf).await?;
            handler.join(self.channel_id).await
        }?;

        Ok(joined.await?)
    }

    pub async fn leave(&self) -> SongbirdResult<()> {
        let Some(handler) = &mut *self.call.lock().await else {
            return Err(SongbirdError::ConnectionNotStarted);
        };
        Ok(handler.leave().await?)
    }

    pub async fn mute(&self, mute: bool) -> SongbirdResult<()> {
        if let Some(handler) = &mut *self.call.lock().await {
            Ok(handler.mute(mute).await?)
        } else {
            Err(SongbirdError::ConnectionNotStarted)
        }
    }

    pub async fn deafen(&self, deaf: bool) -> SongbirdResult<()> {
        if let Some(handler) = &mut *self.call.lock().await {
            Ok(handler.deafen(deaf).await?)
        } else {
            Err(SongbirdError::ConnectionNotStarted)
        }
    }

    pub fn is_deaf(&self) -> SongbirdResult<bool> {
        if let Some(handler) = &mut *self.call.blocking_lock() {
            Ok(handler.is_deaf())
        } else {
            Err(SongbirdError::ConnectionNotStarted)
        }
    }

    pub fn is_mute(&self) -> SongbirdResult<bool> {
        if let Some(handler) = &mut *self.call.blocking_lock() {
            Ok(handler.is_mute())
        } else {
            Err(SongbirdError::ConnectionNotStarted)
        }
    }

    pub async fn move_to<C>(&self, channel_id: C) -> SongbirdResult<()>
    where
        C: Into<ChannelId> + Debug,
    {
        if let Some(handler) = &mut *self.call.lock().await {
            let r = handler.join(channel_id).await?;
            Ok(r.await?)
        } else {
            Err(SongbirdError::ConnectionNotStarted)
        }
    }

    pub async fn enqueue(&self, track: Track) -> SongbirdResult<TrackHandle> {
        if let Some(handler) = &mut *self.call.lock().await {
            Ok(handler.enqueue(track).await)
        } else {
            Err(SongbirdError::ConnectionNotStarted)
        }
    }

    pub fn skip_queue(&self) -> SongbirdResult<()> {
        if let Some(handler) = &mut *self.call.blocking_lock() {
            Ok(handler.queue().skip()?)
        } else {
            Err(SongbirdError::ConnectionNotStarted)
        }
    }

    pub fn stop_queue(&self) -> SongbirdResult<()> {
        if let Some(handler) = &mut *self.call.blocking_lock() {
            handler.queue().stop();
            Ok(())
        } else {
            Err(SongbirdError::ConnectionNotStarted)
        }
    }

    pub fn resume_queue(&self) -> SongbirdResult<()> {
        if let Some(handler) = &mut *self.call.blocking_lock() {
            Ok(handler.queue().resume()?)
        } else {
            Err(SongbirdError::ConnectionNotStarted)
        }
    }

    pub fn get_current_player(&self) -> SongbirdResult<Option<TrackHandle>> {
        if let Some(handler) = &mut *self.call.blocking_lock() {
            Ok(handler.queue().current())
        } else {
            Err(SongbirdError::ConnectionNotStarted)
        }
    }

    pub fn dequeue(&self, index: usize) -> SongbirdResult<Option<TrackHandle>> {
        if let Some(handler) = &mut *self.call.blocking_lock() {
            Ok(handler.queue().dequeue(index).map(|x| x.handle()))
        } else {
            Err(SongbirdError::ConnectionNotStarted)
        }
    }
}

pub struct DpyVoiceUpdate {
    update_hook: Py<PyAny>,
}

impl DpyVoiceUpdate {
    pub fn new(hook: Py<PyAny>) -> Self {
        Self { update_hook: hook }
    }
}

#[async_trait]
impl VoiceUpdate for DpyVoiceUpdate {
    async fn update_voice_state(
        &self,
        _guild_id: GuildId,
        channel_id: Option<ChannelId>,
        self_deaf: bool,
        self_mute: bool,
    ) -> JoinResult<()> {
        let hook_awaited = Python::with_gil(|py| {
            let channel_id = channel_id.map(|x| x.0.get());
            // .map(|x| x.0.into_pyobject(py))
            // .unwrap_or_else(|| (-1i32).into_pyobject(py))?;
            pyo3_async_runtimes::tokio::into_future(
                self.update_hook
                    .call1(py, (channel_id, self_deaf, self_mute))
                    .unwrap()
                    .into_bound(py),
            )
        })
        .map_err(|_| JoinError::Dropped)?;

        hook_awaited.await.map_err(|_| JoinError::Dropped)?;
        Ok(())
    }
}
