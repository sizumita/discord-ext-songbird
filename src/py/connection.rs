//! `native.VoiceSettings`, `native.Connection` and `native.ConnectionStatus` (W2, §6, §12.6).
//!
//! `Connection` holds one field, `Arc<ConnShared>`: the only long-lived strong owner of the
//! connection (§4.3). Its `Drop` closes the connection with `"dropped"`. Every method is synchronous
//! and O(1) or O(input).

use std::collections::HashMap;
use std::num::NonZeroU64;
use std::sync::Arc;
use std::time::Duration;

use pyo3::exceptions::{PyIndexError, PyValueError};
use pyo3::prelude::*;
use pyo3_stub_gen::derive::{gen_stub_pyclass, gen_stub_pymethods};
use sbcore::conn::gateway::StatusSnapshot;
use sbcore::conn::ops::OpResult;
use sbcore::conn::settings::{
    BITRATE_BPS, DRIVER_RETRIES, MAX_DECODED_USERS, MAX_RECEIVE_MB, REJOIN_ATTEMPTS,
    SPEAKING_HANGOVER_MS, STALL_TIMEOUT_MS,
};
use sbcore::conn::{ConnParams, ConnShared, RawVoiceSettings, VoiceSettings};
use sbcore::events::KindMask;
use sbcore::limits::{
    CHUNK_MS, ENCODED_BUFFER_BYTES, EVENT_QUEUE, IDLE_MS, MAX_GAP_MS, MAX_QUEUE_MS_MAX,
    OPUS_STREAM_PACKETS, OUTPUT_LATENCY_MS, PLAYBACK_BUFFER_MS, PLAYOUT_BUFFER_MS,
    PLAYOUT_SPIKE_MS, PREBUFFER_MS, SEEK_S, VOLUME, check,
};
use sbcore::rx::{RouterSpec, RxSource, SubSpec};
use sbcore::tracks::{PlayOpts, TrackRef};
use sbcore::tx::encoded::EncodedSpec;
use sbcore::tx::opus_stream::OpusStreamSpec;
use sbcore::tx::queue::{DisconnectPolicy, PlaybackSpec};
use sbcore::tx::source::{EncodedFactory, SourceSpec};
use sbcore::{CloseReason, CoreError, clock::Clock};

use crate::convert::int_arg;
use crate::errors::{CoreResultExt, core_err, ensure_not_forked};
use crate::py::events::{Control, EventSubscription};
use crate::py::format::PyAudioFormat;
use crate::py::playback::{EncodedSink, OpusSink, Playback};
use crate::py::receive::{
    ReceiveSubscription, RouterSubscription, mix_filter, parse_overflow, user_set,
};
use crate::py::sources::source_spec;
use crate::py::tracks::{NativeTrack, TrackInfo};
use crate::py::wake::WakeBell;

/// A non-zero Discord snowflake (`ValueError` for 0).
fn snowflake(what: &str, v: u64) -> PyResult<NonZeroU64> {
    NonZeroU64::new(v).ok_or_else(|| PyValueError::new_err(format!("{what} must be non-zero")))
}

/// Validated per-connection settings (``VoiceConfig.to_native()``, §9.3).
///
/// Parameters
/// ----------
/// receive : bool, default True
///     ``False``: no receive decoding; ``listen_*`` raise ``ReceiveDisabled``.
/// playout_buffer_ms : int, default 60
///     songbird jitter buffer, ``20..=200``, a multiple of 20.
/// playout_spike_ms : int, default 60
///     ``0..=200``, a multiple of 20.
/// decode_state_timeout_s : float, default 60.0
///     ``1..=600``.
/// speaking_hangover_ms : int, default 200
///     ``20..=5000``.
/// voice_threshold_dbfs : float or None, default None
///     ``-100..=0``; ``None`` = packet activity only.
/// max_decoded_users : int, default 64
///     ``1..=256`` concurrently decoded SSRCs.
/// max_receive_mb : int, default 64
///     ``1..=1024``: the sum of all receive-queue bounds per connection.
/// stall_timeout_ms : int, default 2000
///     ``500..=30000`` receive watchdog.
/// mix_mode : {"mono", "stereo"}, default "stereo"
/// bitrate : int or None, default None
///     ``6000..=510000`` bps; ``None`` = songbird default.
/// softclip : bool, default True
/// driver_timeout_s : float, default 10.0
///     ``1..=60`` per transport attempt.
/// driver_retries : int, default 5
///     ``1..=10`` songbird transport retries (never infinite).
/// rejoin_attempts : int, default 3
///     ``0..=10`` gateway-level rejoins.
/// auto_recover : bool, default True
///     Rejoin after a ≥ 5 s receive stall.
///
/// Raises
/// ------
/// LimitExceeded
///     A value is out of range.
/// InvalidAudio
///     A duration is not a multiple of 20 ms.
/// ValueError
///     ``mix_mode`` is not ``"mono"`` or ``"stereo"``.
#[gen_stub_pyclass]
#[pyclass(
    module = "discord.ext.songbird.native",
    name = "VoiceSettings",
    frozen,
    from_py_object
)]
#[derive(Clone, Copy, Debug)]
pub struct PyVoiceSettings {
    pub(crate) settings: VoiceSettings,
}

#[gen_stub_pymethods]
#[pymethods]
impl PyVoiceSettings {
    #[new]
    #[pyo3(signature = (
        *,
        receive = true,
        playout_buffer_ms = 60,
        playout_spike_ms = 60,
        decode_state_timeout_s = 60.0,
        speaking_hangover_ms = 200,
        voice_threshold_dbfs = None,
        max_decoded_users = 64,
        max_receive_mb = 64,
        stall_timeout_ms = 2_000,
        mix_mode = "stereo",
        bitrate = None,
        softclip = true,
        driver_timeout_s = 10.0,
        driver_retries = 5,
        rejoin_attempts = 3,
        auto_recover = true,
    ))]
    #[allow(clippy::too_many_arguments)]
    fn py_new(
        py: Python<'_>,
        receive: bool,
        playout_buffer_ms: i64,
        playout_spike_ms: i64,
        decode_state_timeout_s: f64,
        speaking_hangover_ms: i64,
        voice_threshold_dbfs: Option<f64>,
        max_decoded_users: i64,
        max_receive_mb: i64,
        stall_timeout_ms: i64,
        #[gen_stub(override_type(type_repr = "typing.Literal['mono', 'stereo']", imports = ("typing",)))]
        mix_mode: &str,
        bitrate: Option<i64>,
        softclip: bool,
        driver_timeout_s: f64,
        driver_retries: i64,
        rejoin_attempts: i64,
        auto_recover: bool,
    ) -> PyResult<Self> {
        ensure_not_forked(py)?;
        let mix_mode_mono = match mix_mode {
            "mono" => true,
            "stereo" => false,
            other => {
                return Err(PyValueError::new_err(format!(
                    "mix_mode must be 'mono' or 'stereo' (got {other:?})"
                )));
            }
        };
        let bitrate = match bitrate {
            Some(b) => Some(check("bitrate", b, BITRATE_BPS).py(py)?),
            None => None,
        };
        let raw = RawVoiceSettings {
            receive,
            playout_buffer_ms: int_arg(
                py,
                "playout_buffer_ms",
                playout_buffer_ms,
                PLAYOUT_BUFFER_MS,
            )?,
            playout_spike_ms: int_arg(py, "playout_spike_ms", playout_spike_ms, PLAYOUT_SPIKE_MS)?,
            decode_state_timeout_s,
            speaking_hangover_ms: int_arg(
                py,
                "speaking_hangover_ms",
                speaking_hangover_ms,
                SPEAKING_HANGOVER_MS,
            )?,
            voice_threshold_dbfs: voice_threshold_dbfs.map(|v| v as f32),
            max_decoded_users: int_arg(
                py,
                "max_decoded_users",
                max_decoded_users,
                MAX_DECODED_USERS,
            )?,
            max_receive_mb: int_arg(py, "max_receive_mb", max_receive_mb, MAX_RECEIVE_MB)?,
            stall_timeout_ms: int_arg(py, "stall_timeout_ms", stall_timeout_ms, STALL_TIMEOUT_MS)?,
            mix_mode_mono,
            bitrate: bitrate.and_then(|b| i32::try_from(b).ok()),
            softclip,
            driver_timeout_s,
            driver_retries: int_arg(py, "driver_retries", driver_retries, DRIVER_RETRIES)
                .inspect_err(|e| {
                    if driver_retries == 0 {
                        // §4.8 / §13.5: the rejection of 0 explains why (a note keeps the
                        // structured `what`/`value`/`minimum`/`maximum` fields intact).
                        let _ = e.value(py).call_method1(
                            "add_note",
                            (
                                "driver_retries=0 is not allowed: songbird cannot express \"no \
                              retries\", and an unset retry limit means infinite retries. Use \
                              reconnect=False for the minimum (one retry).",
                            ),
                        );
                    }
                })?,
            rejoin_attempts: int_arg(py, "rejoin_attempts", rejoin_attempts, REJOIN_ATTEMPTS)?,
            auto_recover,
        };
        let settings = VoiceSettings::validate(raw).py(py)?;
        Ok(PyVoiceSettings { settings })
    }

    /// Whether receive decoding is enabled.
    #[getter]
    fn receive(&self) -> bool {
        self.settings.receive
    }

    fn __repr__(&self) -> String {
        format!("<VoiceSettings {:?}>", self.settings)
    }
}

/// A copy of the connection status (§9.3).
#[gen_stub_pyclass]
#[pyclass(module = "discord.ext.songbird.native", frozen, skip_from_py_object)]
#[derive(Clone, Copy, Debug)]
pub struct ConnectionStatus {
    snap: StatusSnapshot,
    timeline_ms: u64,
}

#[gen_stub_pymethods]
#[pymethods]
impl ConnectionStatus {
    /// ``"idle"``, ``"joining"``, ``"connecting"``, ``"connected"``, ``"reconnecting"`` or
    /// ``"closed"``.
    #[getter]
    #[gen_stub(override_return_type(type_repr = "typing.Literal['idle', 'joining', 'connecting', 'connected', 'reconnecting', 'closed']", imports = ("typing",)))]
    fn state(&self) -> &'static str {
        self.snap.state
    }

    /// Channel of the current media connection.
    #[getter]
    fn channel_id(&self) -> Option<u64> {
        self.snap.channel_id
    }

    /// Channel being joined or moved to.
    #[getter]
    fn target_channel_id(&self) -> Option<u64> {
        self.snap.target_channel_id
    }

    #[getter]
    fn ssrc(&self) -> Option<u32> {
        self.snap.ssrc
    }

    #[getter]
    fn self_mute(&self) -> bool {
        self.snap.self_mute
    }

    #[getter]
    fn self_deaf(&self) -> bool {
        self.snap.self_deaf
    }

    #[getter]
    fn rejoin_attempt(&self) -> u8 {
        self.snap.rejoin_attempt
    }

    /// Increments on every new media connection.
    #[getter]
    fn generation(&self) -> u64 {
        self.snap.generation
    }

    #[getter]
    fn close_reason(&self) -> Option<&'static str> {
        self.snap.close_reason.map(|r| r.as_str())
    }

    #[getter]
    fn close_code(&self) -> Option<u16> {
        self.snap.close_code
    }

    /// Receive timeline position (20 ms ticks since the connection was created).
    #[getter]
    fn timeline_ms(&self) -> u64 {
        self.timeline_ms
    }

    fn __repr__(&self) -> String {
        format!(
            "<ConnectionStatus state={:?} channel_id={:?} generation={}>",
            self.snap.state, self.snap.channel_id, self.snap.generation
        )
    }
}

/// The native half of one voice connection (§6).
///
/// Parameters
/// ----------
/// bell : WakeBell
///     The doorbell; moved out of ``bell`` (``bell.taken`` becomes ``True``).
/// guild_id : int
/// user_id : int
///     ``client.user.id``.
/// settings : VoiceSettings
/// reconnect : bool
///     Transport retries and gateway-level rejoins (§6.8).
///
/// Raises
/// ------
/// ValueError
///     A zero id, or a doorbell that was already taken.
///
/// Notes
/// -----
/// Dropping the object closes the connection with reason ``"dropped"``.
#[gen_stub_pyclass]
#[pyclass(module = "discord.ext.songbird.native", frozen, weakref)]
pub struct Connection {
    shared: Arc<ConnShared>,
}

impl Drop for Connection {
    fn drop(&mut self) {
        self.shared.close(CloseReason::Dropped);
    }
}

impl Connection {
    /// Wraps an existing connection (test hooks: detached connections).
    #[cfg(feature = "test-hooks")]
    pub(crate) fn from_shared(shared: Arc<ConnShared>) -> Self {
        Connection { shared }
    }

    #[cfg(feature = "test-hooks")]
    pub(crate) fn shared(&self) -> &Arc<ConnShared> {
        &self.shared
    }

    fn weak(&self) -> std::sync::Weak<ConnShared> {
        Arc::downgrade(&self.shared)
    }

    fn native_track(&self, track: TrackRef) -> NativeTrack {
        NativeTrack::new(track, self.weak())
    }

    /// `TrackQueue::enqueue`, reporting the connection's own close reason (like every other
    /// connection-level method) instead of the queue's generic one.
    fn enqueue_spec(&self, spec: SourceSpec, opts: PlayOpts) -> Result<TrackRef, CoreError> {
        self.shared
            .queue()
            .enqueue(spec, opts, self.shared.tracks())
            .map_err(|e| match e {
                CoreError::ConnectionClosed(_) => CoreError::ConnectionClosed(
                    self.shared
                        .status()
                        .close_reason
                        .unwrap_or(CloseReason::ConnectionClosed),
                ),
                e => e,
            })
    }
}

/// `(volume, loops, paused)` → `PlayOpts`.
fn play_opts(py: Python<'_>, volume: f32, loops: Option<i64>, paused: bool) -> PyResult<PlayOpts> {
    let volume = check("volume", volume, VOLUME).py(py)?;
    let loops = match loops {
        Some(n) => Some(int_arg(py, "loops", n, 0..=u32::MAX - 1)?),
        None => None,
    };
    Ok(PlayOpts {
        volume,
        loops,
        paused,
    })
}

#[gen_stub_pymethods]
#[pymethods]
impl Connection {
    #[new]
    fn py_new(
        py: Python<'_>,
        bell: PyRef<'_, WakeBell>,
        guild_id: u64,
        user_id: u64,
        settings: PyVoiceSettings,
        reconnect: bool,
    ) -> PyResult<Self> {
        ensure_not_forked(py)?;
        let params = ConnParams {
            guild_id: snowflake("guild_id", guild_id)?,
            user_id: snowflake("user_id", user_id)?,
            reconnect,
            settings: settings.settings,
            clock: Clock::Real,
        };
        let doorbell = bell
            .take()
            .ok_or_else(|| PyValueError::new_err("this WakeBell was already taken"))?;
        let shared = ConnShared::new(params, doorbell).py(py)?;
        Ok(Connection { shared })
    }

    // ── gateway ──────────────────────────────────────────────────────────────────────────────────

    /// Idle → joining (before the join op 4 is sent).
    fn request_join(
        &self,
        py: Python<'_>,
        channel_id: u64,
        self_mute: bool,
        self_deaf: bool,
    ) -> PyResult<()> {
        ensure_not_forked(py)?;
        self.shared
            .request_join(snowflake("channel_id", channel_id)?, self_mute, self_deaf);
        Ok(())
    }

    /// ``move_to``: joining with the new target.
    fn request_move(&self, py: Python<'_>, channel_id: u64) -> PyResult<()> {
        ensure_not_forked(py)?;
        self.shared
            .request_move(snowflake("channel_id", channel_id)?);
        Ok(())
    }

    /// ``reconnect()``: enters rejoining, so the ``channel=None`` echo is ignored.
    fn begin_rejoin(&self, py: Python<'_>) -> PyResult<()> {
        ensure_not_forked(py)?;
        self.shared.begin_rejoin();
        Ok(())
    }

    /// ``disconnect()``: the following ``VoiceState(None)`` closes with ``"requested"``.
    fn request_leave(&self, py: Python<'_>) -> PyResult<()> {
        ensure_not_forked(py)?;
        self.shared.request_leave();
        Ok(())
    }

    /// Forwards ``on_voice_state_update`` for our own user.
    fn on_voice_state(
        &self,
        py: Python<'_>,
        session_id: &str,
        channel_id: Option<u64>,
    ) -> PyResult<()> {
        ensure_not_forked(py)?;
        self.shared.on_voice_state(session_id, channel_id);
        Ok(())
    }

    /// Forwards ``on_voice_server_update`` (``endpoint=None``: the server is being reallocated).
    fn on_voice_server(&self, py: Python<'_>, token: &str, endpoint: Option<&str>) -> PyResult<()> {
        ensure_not_forked(py)?;
        self.shared.on_voice_server(token, endpoint);
        Ok(())
    }

    /// ``set_self_mute`` / ``set_self_deaf`` (mirrors the send-mute flag into live sources).
    fn set_self_flags(&self, py: Python<'_>, mute: bool, deaf: bool) -> PyResult<()> {
        ensure_not_forked(py)?;
        self.shared.set_self_flags(mute, deaf);
        Ok(())
    }

    /// A snapshot of the connection status.
    fn status(&self, py: Python<'_>) -> PyResult<ConnectionStatus> {
        ensure_not_forked(py)?;
        Ok(ConnectionStatus {
            snap: self.shared.status(),
            timeline_ms: self.shared.timeline_ms(),
        })
    }

    /// Receive timeline position in ms.
    #[getter]
    fn timeline_ms(&self) -> u64 {
        self.shared.timeline_ms()
    }

    // ── signalling ───────────────────────────────────────────────────────────────────────────────

    /// Ready wake ids (call after the doorbell socket became readable).
    fn take_ready(&self, py: Python<'_>) -> PyResult<Vec<u16>> {
        ensure_not_forked(py)?;
        let mut out = Vec::new();
        self.shared.take_ready(&mut out);
        // `list[int]` (§12.6): pyo3 would turn a `Vec<u8>` into `bytes`.
        Ok(out.into_iter().map(u16::from).collect())
    }

    /// Pending control slots, in the order of §6.4.
    fn take_controls(&self, py: Python<'_>) -> PyResult<Vec<Control>> {
        ensure_not_forked(py)?;
        let mut out = Vec::new();
        self.shared.take_controls(usize::MAX, &mut out);
        Ok(out.into_iter().map(Control::from).collect())
    }

    /// Users awaiting bot classification (``ClassifyUsers``).
    fn take_unclassified_users(&self, py: Python<'_>) -> PyResult<Vec<u64>> {
        ensure_not_forked(py)?;
        Ok(self.shared.take_unclassified_users())
    }

    /// ``[(user_id, is_bot), …]`` from the member cache.
    fn set_users_bot(&self, py: Python<'_>, users: Vec<(u64, bool)>) -> PyResult<()> {
        ensure_not_forked(py)?;
        self.shared.set_users_bot(&users);
        Ok(())
    }

    // ── receive ──────────────────────────────────────────────────────────────────────────────────

    /// A continuous per-user stream.
    #[pyo3(signature = (uid, fmt, chunk_ms = 20, max_queue_ms = 2_000, overflow = "drop_oldest"))]
    fn subscribe_user(
        &self,
        py: Python<'_>,
        uid: u64,
        fmt: PyAudioFormat,
        chunk_ms: i64,
        max_queue_ms: i64,
        #[gen_stub(override_type(type_repr = "typing.Literal['drop_oldest', 'drop_newest', 'error']", imports = ("typing",)))]
        overflow: &str,
    ) -> PyResult<ReceiveSubscription> {
        ensure_not_forked(py)?;
        let spec = SubSpec {
            source: RxSource::User(snowflake("user_id", uid)?.get()),
            format: fmt.format(),
            chunk_ms: int_arg(py, "chunk_ms", chunk_ms, CHUNK_MS)?,
            max_queue_ms: int_arg(py, "max_queue_ms", max_queue_ms, 0..=MAX_QUEUE_MS_MAX)?,
            overflow: parse_overflow(overflow)?,
        };
        let sub = self.shared.subscribe(spec).py(py)?;
        Ok(ReceiveSubscription::new(sub))
    }

    /// A continuous, silence-filled mixed stream.
    #[pyo3(signature = (
        fmt,
        chunk_ms = 20,
        max_queue_ms = 2_000,
        overflow = "drop_oldest",
        include = None,
        exclude = Vec::new(),
        ignore_bots = true,
        policy = "mix",
        floor_release_ms = 600,
    ))]
    #[allow(clippy::too_many_arguments)]
    fn subscribe_mixed(
        &self,
        py: Python<'_>,
        fmt: PyAudioFormat,
        chunk_ms: i64,
        max_queue_ms: i64,
        #[gen_stub(override_type(type_repr = "typing.Literal['drop_oldest', 'drop_newest', 'error']", imports = ("typing",)))]
        overflow: &str,
        include: Option<Vec<u64>>,
        exclude: Vec<u64>,
        ignore_bots: bool,
        #[gen_stub(override_type(type_repr = "typing.Literal['mix', 'floor']", imports = ("typing",)))]
        policy: &str,
        floor_release_ms: i64,
    ) -> PyResult<ReceiveSubscription> {
        ensure_not_forked(py)?;
        let floor_release_ms = int_arg(py, "floor_release_ms", floor_release_ms, 0..=u32::MAX)?;
        let spec = SubSpec {
            source: RxSource::Mixed(mix_filter(
                include,
                exclude,
                ignore_bots,
                policy,
                floor_release_ms,
            )?),
            format: fmt.format(),
            chunk_ms: int_arg(py, "chunk_ms", chunk_ms, CHUNK_MS)?,
            max_queue_ms: int_arg(py, "max_queue_ms", max_queue_ms, 0..=MAX_QUEUE_MS_MAX)?,
            overflow: parse_overflow(overflow)?,
        };
        let sub = self.shared.subscribe(spec).py(py)?;
        Ok(ReceiveSubscription::new(sub))
    }

    /// A ``listen_users`` router: one stream per user when they first speak.
    #[pyo3(signature = (
        fmt,
        chunk_ms = 20,
        max_queue_ms = 2_000,
        overflow = "drop_oldest",
        include = None,
        ignore_bots = true,
        close_on_leave = true,
    ))]
    #[allow(clippy::too_many_arguments)]
    fn subscribe_router(
        &self,
        py: Python<'_>,
        fmt: PyAudioFormat,
        chunk_ms: i64,
        max_queue_ms: i64,
        #[gen_stub(override_type(type_repr = "typing.Literal['drop_oldest', 'drop_newest', 'error']", imports = ("typing",)))]
        overflow: &str,
        include: Option<Vec<u64>>,
        ignore_bots: bool,
        close_on_leave: bool,
    ) -> PyResult<RouterSubscription> {
        ensure_not_forked(py)?;
        let spec = RouterSpec {
            format: fmt.format(),
            chunk_ms: int_arg(py, "chunk_ms", chunk_ms, CHUNK_MS)?,
            max_queue_ms: int_arg(py, "max_queue_ms", max_queue_ms, 0..=MAX_QUEUE_MS_MAX)?,
            overflow: parse_overflow(overflow)?,
            include: include.map(user_set),
            ignore_bots,
            close_on_leave,
        };
        let router = self.shared.subscribe_router(spec).py(py)?;
        Ok(RouterSubscription::new(router))
    }

    /// An event stream; ``kind_mask`` bit ``i`` selects the ``i``-th ``VoiceEvent`` variant
    /// (declaration order, ``Connected`` = bit 0 … ``Lagged`` = bit 12).
    #[pyo3(signature = (max_queue = 256, kind_mask = i64::from(KindMask::ALL.0)))]
    fn subscribe_events(
        &self,
        py: Python<'_>,
        max_queue: i64,
        kind_mask: i64,
    ) -> PyResult<EventSubscription> {
        ensure_not_forked(py)?;
        let cap = int_arg(py, "max_queue", max_queue, EVENT_QUEUE)?;
        let kind_mask = int_arg(py, "kind_mask", kind_mask, 0..=KindMask::ALL.0)?;
        let queue = self
            .shared
            .subscribe_events(cap, KindMask(kind_mask))
            .py(py)?;
        Ok(EventSubscription::new(queue))
    }

    // ── send ─────────────────────────────────────────────────────────────────────────────────────

    /// A realtime PCM playback (starts paused until the first write).
    #[pyo3(signature = (
        fmt,
        max_buffer_ms = 60_000,
        prebuffer_ms = 0,
        idle_ms = 250,
        auto_pause = true,
        max_gap_ms = 2_000,
        output_latency_ms = 40,
        volume = 1.0,
        when_disconnected = "buffer",
    ))]
    #[allow(clippy::too_many_arguments)]
    fn open_playback(
        &self,
        py: Python<'_>,
        fmt: PyAudioFormat,
        max_buffer_ms: i64,
        prebuffer_ms: i64,
        idle_ms: i64,
        auto_pause: bool,
        max_gap_ms: i64,
        output_latency_ms: i64,
        volume: f32,
        #[gen_stub(override_type(type_repr = "typing.Literal['buffer', 'discard']", imports = ("typing",)))]
        when_disconnected: &str,
    ) -> PyResult<Playback> {
        ensure_not_forked(py)?;
        let when_disconnected = match when_disconnected {
            "buffer" => DisconnectPolicy::Buffer,
            "discard" => DisconnectPolicy::Discard,
            other => {
                return Err(PyValueError::new_err(format!(
                    "when_disconnected must be 'buffer' or 'discard' (got {other:?})"
                )));
            }
        };
        let spec = PlaybackSpec {
            format: fmt.format(),
            max_buffer_ms: int_arg(py, "max_buffer_ms", max_buffer_ms, PLAYBACK_BUFFER_MS)?,
            prebuffer_ms: int_arg(py, "prebuffer_ms", prebuffer_ms, PREBUFFER_MS)?,
            idle_ms: int_arg(py, "idle_ms", idle_ms, IDLE_MS)?,
            auto_pause,
            max_gap_ms: int_arg(py, "max_gap_ms", max_gap_ms, MAX_GAP_MS)?,
            output_latency_ms: int_arg(
                py,
                "output_latency_ms",
                output_latency_ms,
                OUTPUT_LATENCY_MS,
            )?,
            volume: check("volume", volume, VOLUME).py(py)?,
            when_disconnected,
        };
        let (q, track) = self.shared.open_playback(spec).py(py)?;
        Ok(Playback::new(q, track, self.weak()))
    }

    /// A push-based encoded stream (mp3/ogg/…).
    #[pyo3(signature = (max_buffer_bytes = 1 << 20, volume = 1.0))]
    fn open_encoded(
        &self,
        py: Python<'_>,
        max_buffer_bytes: i64,
        volume: f32,
    ) -> PyResult<EncodedSink> {
        ensure_not_forked(py)?;
        let spec = EncodedSpec {
            max_buffer_bytes: int_arg(
                py,
                "max_buffer_bytes",
                max_buffer_bytes,
                ENCODED_BUFFER_BYTES,
            )?,
            volume: check("volume", volume, VOLUME).py(py)?,
        };
        let (enc, track) = self.shared.open_encoded(spec).py(py)?;
        Ok(EncodedSink::new(enc, track, self.weak()))
    }

    /// A push-based Opus packet stream.
    #[pyo3(signature = (max_packets = 250, volume = 1.0))]
    fn open_opus(&self, py: Python<'_>, max_packets: i64, volume: f32) -> PyResult<OpusSink> {
        ensure_not_forked(py)?;
        let spec = OpusStreamSpec {
            max_packets: int_arg(py, "max_packets", max_packets, OPUS_STREAM_PACKETS)?,
            volume: check("volume", volume, VOLUME).py(py)?,
        };
        let (q, track) = self.shared.open_opus_stream(spec).py(py)?;
        Ok(OpusSink::new(q, track, self.weak()))
    }

    /// Plays a memory source now (``loops=None`` loops forever).
    #[pyo3(signature = (source, volume = 1.0, loops = Some(0), paused = false))]
    fn play(
        &self,
        py: Python<'_>,
        #[gen_stub(override_type(type_repr = "AudioFile | RawPcm | OpusPackets"))] source: &Bound<
            '_,
            PyAny,
        >,
        volume: f32,
        loops: Option<i64>,
        paused: bool,
    ) -> PyResult<NativeTrack> {
        ensure_not_forked(py)?;
        let spec = source_spec(source)?;
        let opts = play_opts(py, volume, loops, paused)?;
        let track = self.shared.play(&spec, opts).py(py)?;
        Ok(self.native_track(track))
    }

    /// Adds a pending queue entry.
    #[pyo3(signature = (source, volume = 1.0, loops = Some(0), paused = false))]
    fn enqueue(
        &self,
        py: Python<'_>,
        #[gen_stub(override_type(type_repr = "AudioFile | RawPcm | OpusPackets"))] source: &Bound<
            '_,
            PyAny,
        >,
        volume: f32,
        loops: Option<i64>,
        paused: bool,
    ) -> PyResult<NativeTrack> {
        ensure_not_forked(py)?;
        let spec = source_spec(source)?;
        let opts = play_opts(py, volume, loops, paused)?;
        let track = self.enqueue_spec(spec, opts).py(py)?;
        Ok(self.native_track(track))
    }

    /// Adds a lazy encoded-stream entry; its buffer and decode thread open when it becomes current
    /// (``queue_take_stream``).
    #[pyo3(signature = (volume = 1.0, loops = Some(0), paused = false, max_buffer_bytes = 1 << 20))]
    fn enqueue_stream(
        &self,
        py: Python<'_>,
        volume: f32,
        loops: Option<i64>,
        paused: bool,
        max_buffer_bytes: i64,
    ) -> PyResult<NativeTrack> {
        ensure_not_forked(py)?;
        let opts = play_opts(py, volume, loops, paused)?;
        let spec = SourceSpec::Encoded(EncodedFactory {
            spec: EncodedSpec {
                max_buffer_bytes: int_arg(
                    py,
                    "max_buffer_bytes",
                    max_buffer_bytes,
                    ENCODED_BUFFER_BYTES,
                )?,
                volume: opts.volume,
            },
        });
        let track = self.enqueue_spec(spec, opts).py(py)?;
        Ok(self.native_track(track))
    }

    /// The encoded sink of a lazy entry that became current, once.
    fn queue_take_stream(
        &self,
        py: Python<'_>,
        track: PyRef<'_, NativeTrack>,
    ) -> PyResult<Option<EncodedSink>> {
        ensure_not_forked(py)?;
        let t = track.track_ref();
        Ok(self
            .shared
            .queue_take_stream(t)
            .map(|enc| EncodedSink::new(enc, t.clone(), self.weak())))
    }

    /// The playing queue entry.
    fn queue_current(&self, py: Python<'_>) -> PyResult<Option<NativeTrack>> {
        ensure_not_forked(py)?;
        Ok(self.shared.queue().current().map(|t| self.native_track(t)))
    }

    /// Pending entries (index 0 = next up).
    fn queue_snapshot(&self, py: Python<'_>) -> PyResult<Vec<NativeTrack>> {
        ensure_not_forked(py)?;
        Ok(self
            .shared
            .queue()
            .snapshot()
            .into_iter()
            .map(|t| self.native_track(t))
            .collect())
    }

    fn queue_skip(&self, py: Python<'_>) -> PyResult<()> {
        ensure_not_forked(py)?;
        self.shared.queue().skip();
        Ok(())
    }

    fn queue_stop(&self, py: Python<'_>) -> PyResult<()> {
        ensure_not_forked(py)?;
        self.shared.queue().stop();
        Ok(())
    }

    fn queue_pause(&self, py: Python<'_>) -> PyResult<()> {
        ensure_not_forked(py)?;
        self.shared.queue().pause();
        Ok(())
    }

    fn queue_resume(&self, py: Python<'_>) -> PyResult<()> {
        ensure_not_forked(py)?;
        self.shared.queue().resume();
        Ok(())
    }

    fn queue_clear(&self, py: Python<'_>) -> PyResult<()> {
        ensure_not_forked(py)?;
        self.shared.queue().clear();
        Ok(())
    }

    /// Removes pending entry ``i`` (``IndexError`` if out of range, including negative ``i``).
    fn queue_dequeue(&self, py: Python<'_>, i: i64) -> PyResult<NativeTrack> {
        ensure_not_forked(py)?;
        let i =
            usize::try_from(i).map_err(|_| PyIndexError::new_err("queue index out of range"))?;
        self.shared
            .queue()
            .dequeue(i)
            .map(|t| self.native_track(t))
            .ok_or_else(|| PyIndexError::new_err("queue index out of range"))
    }

    /// Number of pending entries.
    fn queue_len(&self, py: Python<'_>) -> PyResult<usize> {
        ensure_not_forked(py)?;
        Ok(self.shared.queue().len())
    }

    /// Stops every track, queue entry and live stream; the connection stays up.
    fn stop_all(&self, py: Python<'_>) -> PyResult<()> {
        ensure_not_forked(py)?;
        self.shared.stop_all();
        Ok(())
    }

    /// ``6000..=510000`` bps, or ``None`` for songbird's default.
    fn set_bitrate(&self, py: Python<'_>, bps: Option<i64>) -> PyResult<()> {
        ensure_not_forked(py)?;
        let bps = match bps {
            Some(b) => Some(check("bitrate", b, BITRATE_BPS).py(py)?),
            None => None,
        };
        self.shared
            .set_bitrate(bps.and_then(|b| i32::try_from(b).ok()))
            .py(py)
    }

    // ── track ops ────────────────────────────────────────────────────────────────────────────────

    /// Starts an ``info()`` op; the result arrives through ``take_op_results`` (``WAKE_OPS``).
    fn start_track_info(
        &self,
        py: Python<'_>,
        track: PyRef<'_, NativeTrack>,
        timeout_ms: i64,
    ) -> PyResult<u32> {
        ensure_not_forked(py)?;
        let timeout_ms = int_arg(py, "timeout_ms", timeout_ms, 0..=u64::MAX)?;
        self.shared
            .start_track_info(track.track_ref(), Duration::from_millis(timeout_ms))
            .py(py)
    }

    /// Starts a ``seek()`` op (``0 <= seconds <= 86400``, finite).
    fn start_track_seek(
        &self,
        py: Python<'_>,
        track: PyRef<'_, NativeTrack>,
        seconds: f64,
        timeout_ms: i64,
    ) -> PyResult<u32> {
        ensure_not_forked(py)?;
        let seconds = check("seconds", seconds, SEEK_S).py(py)?;
        let timeout_ms = int_arg(py, "timeout_ms", timeout_ms, 0..=u64::MAX)?;
        self.shared
            .start_track_seek(
                track.track_ref(),
                Duration::from_secs_f64(seconds),
                Duration::from_millis(timeout_ms),
            )
            .py(py)
    }

    /// Finished ops: ``(op_id, TrackInfo | float | BaseException)``.
    #[gen_stub(override_return_type(
        type_repr = "list[tuple[int, TrackInfo | float | BaseException]]"
    ))]
    fn take_op_results(&self, py: Python<'_>) -> PyResult<Vec<(u32, Py<PyAny>)>> {
        ensure_not_forked(py)?;
        let mut done = Vec::new();
        self.shared.take_op_results(&mut done);
        let mut out = Vec::with_capacity(done.len());
        for (id, r) in done {
            let value: Py<PyAny> = match r {
                OpResult::Info(info) => Py::new(py, TrackInfo::new(info))?.into_any(),
                OpResult::Seeked(d) => d.as_secs_f64().into_pyobject(py)?.into_any().unbind(),
                OpResult::Err(e) => core_err(py, e).into_value(py).into_any(),
                OpResult::TimedOut => {
                    pyo3::exceptions::PyTimeoutError::new_err("track op timed out")
                        .into_value(py)
                        .into_any()
                }
            };
            out.push((id, value));
        }
        Ok(out)
    }

    // ── lifecycle ────────────────────────────────────────────────────────────────────────────────

    /// Complete, idempotent, non-blocking teardown (§4.7).
    ///
    /// Parameters
    /// ----------
    /// reason : str
    ///     ``"requested"``, ``"cleanup"``, ``"dropped"``, ``"loop_closed"``, ``"connect_aborted"``, …
    #[pyo3(signature = (reason = "closed"))]
    fn close(&self, reason: &str) -> PyResult<()> {
        let r = CloseReason::from_name(reason)
            .ok_or_else(|| PyValueError::new_err(format!("unknown close reason {reason:?}")))?;
        self.shared.close(r);
        Ok(())
    }

    #[getter]
    fn closed(&self) -> bool {
        self.shared.is_closed()
    }

    /// Receive, control and health counters (§11.3).
    fn stats(&self, py: Python<'_>) -> PyResult<HashMap<&'static str, u64>> {
        ensure_not_forked(py)?;
        Ok(self.shared.stats().into_iter().collect())
    }

    fn __repr__(&self) -> String {
        format!("<Connection state={:?}>", self.shared.status().state)
    }
}
