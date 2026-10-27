//! Raw receive inputs: what `Dispatch` sends to the ConnWorker (W3, §8.2).

use std::time::Instant;

use smallvec::SmallVec;
use songbird::events::context_data::VoiceTick;

/// One SSRC's contribution to a tick: a packet handle (refcounted `Bytes`, no copy) or a loss.
#[derive(Clone, Debug)]
pub enum RawVoice {
    Packet {
        ssrc: u32,
        packet: bytes::Bytes,
        payload_offset: usize,
        /// An **end index** (songbird's `payload_end_pad`, §8.3).
        payload_end: usize,
    },
    Lost {
        ssrc: u32,
    },
}

/// One 20 ms `VoiceTick`, detached from songbird's types.
#[derive(Clone, Debug)]
pub struct RawTick {
    pub arrived: Instant,
    pub voices: SmallVec<[RawVoice; 8]>,
    pub silent: SmallVec<[u32; 16]>,
}

impl RawTick {
    /// Copies the `(ssrc, Bytes, offsets)` handles out of a songbird tick (O(#SSRC), no packet copy).
    pub fn from_voice_tick(t: &VoiceTick, now: Instant) -> Self {
        let voices = t
            .speaking
            .iter()
            .map(|(&ssrc, vd)| match &vd.packet {
                Some(rd) => RawVoice::Packet {
                    ssrc,
                    packet: rd.packet.clone(),
                    payload_offset: rd.payload_offset,
                    payload_end: rd.payload_end_pad,
                },
                None => RawVoice::Lost { ssrc },
            })
            .collect();
        RawTick {
            arrived: now,
            voices,
            silent: t.silent.iter().copied().collect(),
        }
    }
}

/// Inputs of the receive pipeline, sent with `try_send` on a `TICK_CHANNEL_CAP` channel.
///
/// `Tick` is large by design: boxing it would allocate once per tick on the NET runtime.
#[allow(clippy::large_enum_variant)]
#[derive(Clone, Debug)]
pub enum RxInput {
    Tick(RawTick),
    Map {
        ssrc: u32,
        user_id: u64,
    },
    UserLeft {
        user_id: u64,
    },
    LinkUp {
        ssrc: Option<u32>,
    },
    LinkDown,
    /// The media session moves to another channel: a `Connect` to a channel other than the
    /// previous one is about to start. No `ClientDisconnect` arrives for the users left behind,
    /// and the old session's SSRCs mean nothing on the new one (§8.5).
    ChannelChanged,
}

/// Sender half of the tick channel.
pub type RxSender = tokio::sync::mpsc::Sender<RxInput>;
