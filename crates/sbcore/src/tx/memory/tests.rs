//! In-memory source tests (§13.1 `tx::memory`/`source`).

use songbird::input::core::audio::{AudioBufferRef, Signal};
use songbird::input::core::formats::{SeekMode, SeekTo};
use songbird::input::core::io::ReadBytes;

use super::*;
use crate::format::{F32_48K, PCM16_24K, PCM16_48K};
use crate::runtime::{Role, set_role};
use crate::tracks::flags::DROPPED;
use crate::tx::decoder::tests::{guard, parsed};
use crate::tx::opus_stream::tests::opus_packets;

/// A canonical PCM16 WAV file.
pub(crate) fn wav(rate: u32, ch: u16, samples: &[i16]) -> Vec<u8> {
    let data_len = samples.len() * 2;
    let mut v = Vec::new();
    v.extend_from_slice(b"RIFF");
    v.extend_from_slice(&((36 + data_len) as u32).to_le_bytes());
    v.extend_from_slice(b"WAVE");
    v.extend_from_slice(b"fmt ");
    v.extend_from_slice(&16u32.to_le_bytes());
    v.extend_from_slice(&1u16.to_le_bytes());
    v.extend_from_slice(&ch.to_le_bytes());
    v.extend_from_slice(&rate.to_le_bytes());
    v.extend_from_slice(&(rate * u32::from(ch) * 2).to_le_bytes());
    v.extend_from_slice(&(ch * 2).to_le_bytes());
    v.extend_from_slice(&16u16.to_le_bytes());
    v.extend_from_slice(b"data");
    v.extend_from_slice(&(data_len as u32).to_le_bytes());
    for s in samples {
        v.extend_from_slice(&s.to_le_bytes());
    }
    v
}

/// `secs` of a `hz` sine at `amp` (full scale 1.0), interleaved, `ch` identical channels.
pub(crate) fn tone(rate: u32, ch: usize, hz: f64, amp: f64, secs: f64) -> Vec<f32> {
    let frames = (f64::from(rate) * secs) as usize;
    (0..frames)
        .flat_map(|i| {
            let s =
                (amp * (2.0 * std::f64::consts::PI * hz * i as f64 / f64::from(rate)).sin()) as f32;
            std::iter::repeat_n(s, ch)
        })
        .collect()
}

fn rms(x: &[f32]) -> f64 {
    (x.iter().map(|s| f64::from(*s).powi(2)).sum::<f64>() / x.len() as f64).sqrt()
}

#[test]
fn mem_pcm_validates_frames() {
    assert!(matches!(
        MemPcm::from_bytes(PCM16_24K, &[1, 2, 3]),
        Err(CoreError::InvalidAudio(_))
    ));
    let m = MemPcm::from_bytes(PCM16_24K, &[1, 0, 2, 0]).unwrap();
    assert_eq!(m.frames(), 2);
    let st = Format::new(48_000, 2, Encoding::Pcm16Le).unwrap();
    assert!(MemPcm::from_i16(st, vec![1, 2, 3]).is_err());
    let m = MemPcm::from_i16(st, vec![0; 96_000]).unwrap();
    assert_eq!(m.duration_ms(), 1_000);
}

#[test]
fn from_any_rate_resamples_to_48k_with_the_expected_length_and_level() {
    for (rate, ch) in [(44_100u32, 2usize), (22_050, 1), (192_000, 2), (11_025, 1)] {
        let x = tone(rate, ch, 1_000.0, 0.5, 1.0);
        let bytes: Vec<u8> = x.iter().flat_map(|s| s.to_le_bytes()).collect();
        let m = MemPcm::from_any_rate(rate, ch as u8, Encoding::F32Le, &bytes).unwrap();
        assert_eq!(m.format().rate, SampleRate::Hz48000);
        let frames = (x.len() / ch) as u64;
        let expect = (frames * 48_000 + u64::from(rate) / 2) / u64::from(rate);
        assert_eq!(m.frames(), expect, "{rate}");
        let y: Vec<f32> = m.samples[4_800 * ch..m.samples.len() - 4_800 * ch]
            .iter()
            .map(|s| f32::from(*s) / 32_768.0)
            .collect();
        let db = 20.0 * (rms(&y) / (0.5 / 2f64.sqrt())).log10();
        assert!(db.abs() < 0.2, "{rate}: {db} dB");
    }
    // Stream rates are stored as-is (pcm16le).
    let m = MemPcm::from_any_rate(24_000, 1, Encoding::Pcm16Le, &[1, 0, 2, 0]).unwrap();
    assert_eq!(m.format(), PCM16_24K);
    assert_eq!(m.frames(), 2);
    for bad in [7_999u32, 192_001] {
        assert!(matches!(
            MemPcm::from_any_rate(bad, 2, Encoding::F32Le, &[]),
            Err(CoreError::Limit(_))
        ));
    }
    assert!(MemPcm::from_any_rate(44_100, 3, Encoding::F32Le, &[]).is_err());
    assert!(MemPcm::from_any_rate(44_100, 1, Encoding::MuLaw, &[]).is_err());
    assert!(MemPcm::from_any_rate(44_100, 1, Encoding::Pcm16Le, &[0]).is_err());
}

#[test]
fn opus_frames_validate_and_seek_by_index() {
    set_role(Role::Harness);
    let pk = opus_packets(10, 960);
    let f = Arc::new(OpusFrames::from_packets(pk.iter().map(Vec::as_slice)).unwrap());
    assert_eq!(f.len(), 10);
    assert_eq!(f.duration_ms(), 200);
    assert_eq!(f.packet(3).unwrap(), &pk[3][..]);
    let bad = opus_packets(1, 480);
    assert!(matches!(
        OpusFrames::from_packets([pk[0].as_slice(), bad[0].as_slice()]),
        Err(CoreError::InvalidAudio(_))
    ));
    let (done, flags) = guard();
    let mut p = parsed(f.to_input(done).unwrap());
    assert_eq!(p.format.next_packet().unwrap().buf(), &pk[0][..]);
    let s = p
        .format
        .seek(
            SeekMode::Accurate,
            SeekTo::TimeStamp {
                ts: 7 * 960 + 100,
                track_id: 0,
            },
        )
        .unwrap();
    assert_eq!(s.actual_ts, 7 * 960);
    let pkt = p.format.next_packet().unwrap();
    assert_eq!(pkt.buf(), &pk[7][..]);
    assert!(p.decoder.decode(&pkt).is_ok());
    p.format.next_packet().unwrap();
    p.format.next_packet().unwrap();
    assert!(p.format.next_packet().is_err());
    assert_ne!(flags.hints() & EOF_SEEN, 0);
    assert!(p.supports_backseek);
}

#[test]
fn encoded_bytes_probe_and_errors() {
    let samples: Vec<i16> = (0..44_100).map(|i| ((i % 100) * 100) as i16).collect();
    let b = EncodedBytes::new(wav(44_100, 1, &samples)).unwrap();
    let info = b.info();
    assert_eq!(info.sample_rate, Some(44_100));
    assert_eq!(info.channels, Some(1));
    assert_eq!(info.duration_ms, Some(1_000));
    assert!(info.codec.contains("pcm"), "{}", info.codec);
    assert!(matches!(
        EncodedBytes::new(vec![0x42; 1_000]),
        Err(CoreError::Source(_))
    ));
    assert!(matches!(
        EncodedBytes::new(Vec::new()),
        Err(CoreError::Source(_))
    ));
}

#[test]
fn read_file_errors_are_io_or_limit() {
    let missing = std::env::temp_dir().join("sbcore-definitely-missing-file.wav");
    assert!(matches!(
        read_file(&missing, 1 << 20),
        Err(CoreError::Io(e)) if e.kind() == ErrorKind::NotFound
    ));
    let path = std::env::temp_dir().join(format!("sbcore-read-file-{}.bin", std::process::id()));
    std::fs::write(&path, vec![7u8; 1_000]).unwrap();
    assert_eq!(read_file(&path, 1_000).unwrap().len(), 1_000);
    assert!(matches!(read_file(&path, 999), Err(CoreError::Limit(_))));
    let _ = std::fs::remove_file(&path);
}

#[test]
fn audio_file_input_plays_through_songbird_promotion() {
    set_role(Role::Harness);
    let samples: Vec<i16> = (0..4_800).map(|i| ((i % 50) * 300) as i16).collect();
    let b = Arc::new(EncodedBytes::new(wav(48_000, 1, &samples)).unwrap());
    let (done, flags) = guard();
    let input = b.to_input(done);
    let Input::Live(live, None) = input else {
        panic!("live input expected")
    };
    let promoted = live.promote(get_codec_registry(), get_probe()).unwrap();
    let LiveInput::Parsed(mut p) = promoted else {
        panic!("parsed expected")
    };
    let mut frames = 0;
    while let Ok(pkt) = p.format.next_packet() {
        let out = p.decoder.decode(&pkt).unwrap();
        frames += out.frames();
        if let AudioBufferRef::S16(b) = &out {
            assert_eq!(b.chan(0).len(), out.frames());
        }
    }
    assert_eq!(frames, 4_800);
    drop(p);
    assert_ne!(flags.hints() & DROPPED, 0);
    // Reusable: every play gets a fresh cursor.
    let (done, _) = guard();
    assert!(matches!(
        b.to_input(done),
        Input::Live(LiveInput::Raw(_), None)
    ));
}

#[test]
fn source_spec_inputs() {
    set_role(Role::Harness);
    let (hub, _bell) = crate::tx::queue::tests::test_hub();
    let pcm = Arc::new(MemPcm::from_i16(PCM16_48K, vec![100; 9_600]).unwrap());
    let spec = super::super::source::SourceSpec::Pcm(pcm);
    assert!(spec.seekable());
    let (done, _) = guard();
    let (input, enc) = spec.to_input(done, &hub).unwrap();
    assert!(enc.is_none());
    let mut p = parsed(input);
    assert!(p.supports_backseek);
    let pkt = p.format.next_packet().unwrap();
    assert_eq!(p.decoder.decode(&pkt).unwrap().frames(), 960);
    let mss = p.format.into_inner();
    assert_eq!(mss.pos(), 16);
    let _ = F32_48K;
}

#[test]
fn done_source_records_eof_and_drop() {
    let (done, flags) = guard();
    let mut src = DoneSource::new(Cursor::new(ArcBytes(Arc::new(vec![1u8; 10]))), done);
    assert!(!src.is_seekable() || src.byte_len() == Some(10));
    let mut buf = [0u8; 8];
    assert_eq!(src.read(&mut buf).unwrap(), 8);
    assert_eq!(flags.hints() & EOF_SEEN, 0);
    assert_eq!(src.read(&mut buf).unwrap(), 2);
    assert_eq!(src.read(&mut buf).unwrap(), 0);
    assert_ne!(flags.hints() & EOF_SEEN, 0);
    assert_eq!(src.seek(SeekFrom::Start(3)).unwrap(), 3);
    drop(src);
    assert_ne!(flags.hints() & DROPPED, 0);
}
