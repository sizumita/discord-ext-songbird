use crate::error::IntoPyResult;
use crate::future::PyFuture;
use crate::update::VoiceUpdater;
use pyo3::prelude::PyAnyMethods;
use pyo3::types::PyTuple;
use pyo3::{pyclass, pymethods, Bound, IntoPyObjectExt, PyAny, PyRef, PyResult, Python};
use pyo3_async_runtimes::tokio::future_into_py;
use pyo3_stub_gen::derive::{gen_stub_pyclass, gen_stub_pymethods};
use songbird::id::{ChannelId, GuildId, UserId};
use songbird::shards::Shard;
use songbird::{Call, Config};
use std::num::NonZeroU64;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;

struct CallWrapper(Option<Call>);

impl CallWrapper {
    fn new() -> Self {
        CallWrapper(None)
    }

    fn set(&mut self, call: Call) {
        self.0 = Some(call);
    }

    fn get(&self) -> PyResult<&Call> {
        self.0
            .as_ref()
            .ok_or_else(|| pyo3::exceptions::PyValueError::new_err("Connection not started"))
    }

    fn get_mut(&mut self) -> PyResult<&mut Call> {
        self.0
            .as_mut()
            .ok_or_else(|| pyo3::exceptions::PyValueError::new_err("Connection not started"))
    }
}

#[gen_stub_pyclass]
#[pyclass(subclass)]
pub struct SongbirdImpl {
    channel_id: ChannelId,
    guild_id: GuildId,
    application_id: UserId,
    call: Arc<Mutex<CallWrapper>>,
}

#[gen_stub_pymethods]
#[pymethods]
impl SongbirdImpl {
    #[new]
    fn new(
        #[gen_stub(override_type(type_repr="discord.Client", imports=("discord")))] client: &Bound<
            PyAny,
        >,
        #[gen_stub(override_type(type_repr="discord.abc.Connectable", imports=("discord")))]
        connectable: &Bound<PyAny>,
    ) -> PyResult<Self> {
        let id = connectable.getattr("id")?.extract::<NonZeroU64>()?;

        let channel_id = ChannelId(id);
        let (guild_id, key_type) = {
            let keys = connectable.call_method0("_get_voice_client_key")?;
            let casted_key = keys.cast::<PyTuple>()?;
            (
                casted_key.get_item(0)?.extract::<NonZeroU64>()?,
                casted_key.get_item(1)?.extract::<String>()?,
            )
        };
        if &key_type != "guild_id" {
            log::debug!("Unsupported voice client key type: {}", key_type);
            return Err(pyo3::exceptions::PyValueError::new_err(
                "Only guild voice connections are supported",
            ));
        }
        let application_id = client.getattr("application_id")?.extract::<NonZeroU64>()?;

        log::debug!(
            "Initialized SongbirdImpl (guild {}, channel {}, application {})",
            guild_id,
            channel_id,
            application_id
        );

        Ok(Self {
            channel_id,
            guild_id: guild_id.into(),
            application_id: application_id.into(),
            call: Arc::new(Mutex::new(CallWrapper::new())),
        })
    }

    #[pyo3(signature = (*, timeout, reconnect, self_deaf = false, self_mute = false))]
    fn connect<'py>(
        slf: PyRef<'py, Self>,
        py: Python<'py>,
        timeout: f32,
        reconnect: bool,
        self_deaf: bool,
        self_mute: bool,
    ) -> PyResult<PyFuture<'py, ()>> {
        let config = Config::default().gateway_timeout(Some(Duration::from_secs_f32(timeout)));
        let self_call = slf.call.clone();
        let guild_id = slf.guild_id;
        let channel_id = slf.channel_id;
        let application_id = slf.application_id;

        log::debug!(
            "Connecting voice (guild {}, channel {}, timeout={}s, reconnect={}, self_deaf={}, self_mute={})",
            guild_id,
            channel_id,
            timeout,
            reconnect,
            self_deaf,
            self_mute
        );

        let shard = Shard::Generic(Arc::new(VoiceUpdater(slf.into_py_any(py)?.into_any())));

        future_into_py(py, async move {
            let call = Call::from_config(guild_id, shard, application_id, config);
            {
                let mut guard = self_call.lock().await;
                guard.set(call);
            }
            log::trace!(
                "Joining voice channel {} for guild {}",
                channel_id,
                guild_id
            );
            let joined = {
                let mut guard = self_call.lock().await;
                let call = guard.get_mut()?;
                call.deafen(self_deaf).await.into_py()?;
                call.mute(self_mute).await.into_py()?;
                call.join(channel_id).await.into_py()?
            };
            joined.await.into_py()?;
            log::debug!(
                "Connected to voice channel {} for guild {}",
                channel_id,
                guild_id
            );
            Ok(())
        })
        .map(|x| x.into())
    }

    #[pyo3(signature = (*, force))]
    async fn disconnect(&self, force: bool) -> PyResult<()> {
        log::debug!(
            "Disconnecting voice for guild {} (force={})",
            self.guild_id,
            force
        );
        let mut guard = self.call.lock().await;
        let call = guard.get_mut()?;
        call.leave().await.into_py()
    }

    async fn update_server(&self, endpoint: String, token: String) -> PyResult<()> {
        log::trace!(
            "Received voice server update for guild {} (endpoint={}, token_len={})",
            self.guild_id,
            endpoint,
            token.len()
        );
        let mut guard = self.call.lock().await;
        let call = guard.get_mut()?;
        call.update_server(endpoint, token);
        Ok(())
    }

    #[pyo3(signature = (session_id, channel_id=None))]
    async fn update_state(
        &self,
        session_id: String,
        #[gen_stub(override_type(type_repr = "int | None"))] channel_id: Option<NonZeroU64>,
    ) -> PyResult<()> {
        log::trace!(
            "Received voice state update for guild {} (session_id_len={}, channel_id={:?})",
            self.guild_id,
            session_id.len(),
            channel_id
        );
        let mut guard = self.call.lock().await;
        let call = guard.get_mut()?;
        call.update_state(session_id, channel_id);
        Ok(())
    }

    #[allow(unused)]
    async fn update_hook(
        &self,
        channel_id: Option<u64>,
        self_mute: bool,
        self_deaf: bool,
    ) -> PyResult<()> {
        Err(pyo3::exceptions::PyNotImplementedError::new_err(
            "update_hook must be implemented in a subclass",
        ))
    }

    /// |coro|
    ///
    /// Deafen or undeafen this account.
    ///
    /// Parameters
    /// ----------
    /// self_deaf: bool
    ///     Whether to deafen or undeafen this account.
    ///
    /// Returns
    /// -------
    /// None
    async fn deafen(&self, self_deaf: bool) -> PyResult<()> {
        log::trace!(
            "Setting self_deaf={} for guild {}",
            self_deaf,
            self.guild_id
        );
        let mut guard = self.call.lock().await;
        let call = guard.get_mut()?;
        call.deafen(self_deaf).await.into_py()
    }

    /// |coro|
    ///
    /// Mute or unmute this account.
    ///
    /// Parameters
    /// ----------
    /// self_mute: bool
    ///     Whether to mute or unmute this account.
    ///
    /// Returns
    /// -------
    /// None
    async fn mute(&self, self_mute: bool) -> PyResult<()> {
        log::trace!(
            "Setting self_mute={} for guild {}",
            self_mute,
            self.guild_id
        );
        let mut guard = self.call.lock().await;
        let call = guard.get_mut()?;
        call.mute(self_mute).await.into_py()
    }

    /// Check if this account is muted.
    ///
    /// Returns
    /// -------
    /// bool
    ///     Whether this account is muted.
    fn is_mute(&self) -> PyResult<bool> {
        let guard = self.call.blocking_lock();
        let call = guard.get()?;
        Ok(call.is_mute())
    }

    /// Check if this account is deafened.
    ///
    /// Returns
    /// -------
    /// bool
    ///     Whether this account is deafened.
    fn is_deaf(&self) -> PyResult<bool> {
        let guard = self.call.blocking_lock();
        let call = guard.get()?;
        Ok(call.is_deaf())
    }

    /// |coro|
    ///
    /// Move this account to another voice channel.
    ///
    /// Parameters
    /// ----------
    /// channel: discord.abc.Snowflake | None
    ///     The channel to move to.
    ///     If None, disconnects from voice.
    fn move_to<'py>(
        &self,
        py: Python<'py>,
        #[gen_stub(override_type(type_repr="discord.abc.Snowflake", imports=("discord")))]
               channel: Option<Bound<'py, PyAny>>,
    ) -> PyResult<PyFuture<'py, ()>> {
        let call = self.call.clone();
        if let Some(channel) = channel {
            let id = channel.getattr("id")?.extract::<NonZeroU64>()?;
            log::debug!(
                "Moving voice connection for guild {} to channel {}",
                self.guild_id,
                id
            );
            future_into_py(py, async move {
                let mut guard = call.lock().await;
                let call = guard.get_mut()?;
                call.join(id).await.into_py()?;
                Ok(())
            })
            .map(|x| x.into())
        } else {
            log::debug!(
                "Leaving voice channel for guild {} via move_to(None)",
                self.guild_id
            );
            future_into_py(py, async move {
                let mut guard = call.lock().await;
                let call = guard.get_mut()?;
                call.leave().await.into_py()?;
                Ok(())
            })
            .map(|x| x.into())
        }
    }
}
