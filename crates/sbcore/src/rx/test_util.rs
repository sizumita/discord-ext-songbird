//! Test helpers for the receive pipeline: deterministic PRNG, real Opus packets, RTP packets as
//! songbird's Decrypt mode delivers them, and wake hubs.

use std::sync::Arc;

use songbird::driver::opus;

use crate::wake::{Doorbell, WakeHub, WakeToken};

/// xorshift64* (fixed seed ⇒ reproducible fuzzing).
pub struct XorShift(u64);

impl XorShift {
    pub fn new(seed: u64) -> Self {
        XorShift(seed.max(1))
    }

    pub fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
}

/// `frames` of interleaved 48 kHz stereo sine at `freq` Hz and peak `amp`.
pub fn sine48(freq: f32, amp: f32, frames: usize) -> Vec<f32> {
    (0..frames)
        .flat_map(|i| {
            let s = amp * (2.0 * std::f32::consts::PI * freq * i as f32 / 48_000.0).sin();
            [s, s]
        })
        .collect()
}

/// Encodes interleaved 48 kHz stereo f32 into 20 ms Opus packets (zero-padding the last one).
pub fn opus_frames(pcm: &[f32]) -> Vec<Vec<u8>> {
    opus_frames_at(pcm, 64_000)
}

/// [`opus_frames`] at `bitrate` bits per second.
pub fn opus_frames_at(pcm: &[f32], bitrate: i32) -> Vec<Vec<u8>> {
    let mut enc = opus::Encoder::new(48_000, opus::Channels::Stereo, opus::Application::Audio)
        .expect("encoder");
    enc.set_bitrate(opus::Bitrate::Bits(bitrate))
        .expect("bitrate");
    pcm.chunks(1920)
        .map(|c| {
            let mut frame = [0.0f32; 1920];
            frame[..c.len()].copy_from_slice(c);
            enc.encode_vec_float(&frame, 1275).expect("encode")
        })
        .collect()
}

/// Encodes `ms` (multiple of 20 ≤ 60, or 120) of one sine into a single Opus packet.
pub fn opus_packet_ms(ms: usize, amp: f32) -> Vec<u8> {
    let mut enc = opus::Encoder::new(48_000, opus::Channels::Stereo, opus::Application::Audio)
        .expect("encoder");
    let pcm = sine48(330.0, amp, 48 * ms);
    enc.encode_vec_float(&pcm, 4000).expect("encode")
}

/// Builds RTP packets laid out as songbird's Decrypt mode hands them to `VoiceTick`.
#[derive(Clone, Debug)]
pub struct RtpBuild {
    pub seq: u16,
    pub ssrc: u32,
    /// Words of RTP header extension data (0 = no extension).
    pub ext_words: usize,
    /// RTP padding bytes (0 = none; the last byte is the count).
    pub padding: u8,
    /// Append a DAVE leftover supplemental block with this truncated nonce.
    pub dave_nonce: Option<u64>,
}

impl RtpBuild {
    pub fn new(seq: u16, ssrc: u32) -> Self {
        RtpBuild {
            seq,
            ssrc,
            ext_words: 0,
            padding: 0,
            dave_nonce: None,
        }
    }

    /// `(packet, payload_offset, payload_end)`; `suffix` bytes of AEAD tag/nonce follow the payload.
    pub fn build(&self, opus: &[u8], suffix: usize) -> (Vec<u8>, usize, usize) {
        let mut p = Vec::with_capacity(64 + opus.len() + suffix);
        let x = self.ext_words > 0;
        let pad = self.padding > 0;
        p.push(0x80 | (u8::from(pad) << 5) | (u8::from(x) << 4));
        p.push(0x78);
        p.extend_from_slice(&self.seq.to_be_bytes());
        p.extend_from_slice(&(u32::from(self.seq).wrapping_mul(960)).to_be_bytes());
        p.extend_from_slice(&self.ssrc.to_be_bytes());
        if x {
            p.extend_from_slice(&[0xBE, 0xDE]);
            p.extend_from_slice(&u16::try_from(self.ext_words).unwrap().to_be_bytes());
            p.extend(std::iter::repeat_n(0xAB, 4 * self.ext_words));
        }
        p.extend_from_slice(opus);
        if let Some(nonce) = self.dave_nonce {
            // tag8 | ULEB128 nonce | (no unencrypted ranges) | size | FA FA (davey 0.1.5 layout)
            p.extend_from_slice(&[0x5A; 8]);
            let mut n = nonce;
            let mut leb = 0usize;
            loop {
                let byte = (n & 0x7F) as u8;
                n >>= 7;
                leb += 1;
                if n == 0 {
                    p.push(byte);
                    break;
                }
                p.push(byte | 0x80);
            }
            p.push(u8::try_from(8 + leb + 1 + 2).unwrap());
            p.extend_from_slice(&[0xFA, 0xFA]);
        }
        if pad {
            p.extend(std::iter::repeat_n(0, usize::from(self.padding) - 1));
            p.push(self.padding);
        }
        let payload_end = p.len() - 12;
        p.extend(std::iter::repeat_n(0xEE, suffix));
        (p, 0, payload_end)
    }
}

/// A wake hub whose doorbell writes into a socket the test keeps (or drops).
#[cfg(unix)]
pub fn hub() -> (Arc<WakeHub>, std::os::unix::net::UnixStream) {
    let (w, r) = std::os::unix::net::UnixStream::pair().expect("socketpair");
    r.set_nonblocking(true).expect("nonblocking");
    (
        WakeHub::new(Doorbell::from_unix_stream(w).expect("doorbell")),
        r,
    )
}

/// A wake hub whose doorbell writes into a socket the test keeps (or drops).
#[cfg(windows)]
pub fn hub() -> (Arc<WakeHub>, std::net::TcpStream) {
    use std::os::windows::io::IntoRawSocket;
    let l = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let w = std::net::TcpStream::connect(l.local_addr().expect("addr")).expect("connect");
    let (r, _) = l.accept().expect("accept");
    r.set_nonblocking(true).expect("nonblocking");
    // SAFETY: ownership of the freshly created socket moves into the doorbell.
    let bell = unsafe { Doorbell::adopt(w.into_raw_socket() as i64) }.expect("doorbell");
    (WakeHub::new(bell), r)
}

/// One token from `hub` (panics when exhausted).
pub fn token(hub: &Arc<WakeHub>) -> WakeToken {
    hub.alloc().expect("wake id")
}

/// Drains every pending wake byte and returns the ready ids.
pub fn ready(hub: &WakeHub) -> Vec<u8> {
    let mut out = Vec::new();
    hub.take_ready(&mut out);
    out
}
