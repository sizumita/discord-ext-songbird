use crate::model::PyFuture;
use crate::player::input::{PyCompose, PyInputBase};
use arrow::array::{Array, ArrayRef, BinaryArray, LargeBinaryArray};
use arrow::datatypes::DataType;
use pyo3::exceptions::{PyRuntimeError, PyTypeError, PyValueError};
use pyo3::{Bound, PyAny, PyResult, Python, pyclass, pyfunction, pymethods};
use pyo3_arrow::PyArray;
use pyo3_async_runtimes::tokio::future_into_py;
use pyo3_stub_gen::derive::{gen_stub_pyclass, gen_stub_pyfunction, gen_stub_pymethods};
use songbird::input::core::audio::Layout;
use songbird::input::core::codecs::{CODEC_TYPE_OPUS, CodecParameters, Decoder, DecoderOptions};
use songbird::input::core::errors::{
    self as symph_err, Error as SymphError, Result as SymphResult, SeekErrorKind,
};
use songbird::input::core::formats::{
    Cue, FormatOptions, FormatReader, Packet, SeekMode, SeekTo, SeekedTo, Track,
};
use songbird::input::core::io::{MediaSourceStream, MediaSourceStreamOptions};
use songbird::input::core::meta::{Metadata as SymphMetadata, MetadataLog};
use songbird::input::core::probe::{
    Descriptor, Hint, Instantiate, Probe, ProbedMetadata, QueryDescriptor,
};
use songbird::input::core::sample::SampleFormat;
use songbird::input::core::units::TimeBase;
use songbird::input::{LiveInput, Parsed};
use std::io::Cursor;
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;

const OPUS_SAMPLE_RATE: u32 = 48_000;
const OPUS_FRAME_SAMPLES: u64 = 960;
const OPUS_TRACK_ID: u32 = 0;

type OpusPacket = Box<[u8]>;
type OpusPacketSender = mpsc::Sender<OpusPacket>;
type OpusPacketReceiver = mpsc::Receiver<OpusPacket>;
type SharedSender = Arc<Mutex<Option<OpusPacketSender>>>;
type SharedReceiver = Arc<Mutex<Option<OpusPacketReceiver>>>;

#[gen_stub_pyclass]
#[pyclass(
    name = "OpusPacketInput",
    extends = PyInputBase,
    module = "discord.ext.songbird.native.player",
    skip_from_py_object
)]
/// Pre-encoded Opus packet input backed by an Arrow binary array.
///
/// Notes
/// -----
/// Each array value must be one non-empty 20 ms Opus frame at 48 kHz.
/// When this input is the only active track and volume is 1.0, Songbird can
/// send these frames through without decoding and re-encoding them.
pub struct PyOpusPacketInput {
    frames: OpusFrameArray,
}

#[gen_stub_pyclass]
#[pyclass(
    name = "OpusPacketStreamInput",
    extends = PyInputBase,
    module = "discord.ext.songbird.native.player",
    skip_from_py_object
)]
/// Live pre-encoded Opus packet input.
///
/// Notes
/// -----
/// Use ``await send(packet)`` to push one 20 ms Opus frame. The bounded queue
/// provides backpressure and ``close()`` signals EOF to the player.
pub struct PyOpusPacketStreamInput {
    sender: SharedSender,
    receiver: SharedReceiver,
}

#[derive(Clone)]
enum OpusFrameArray {
    Binary(BinaryArray),
    LargeBinary(LargeBinaryArray),
}

struct OpusPacketFormatReader {
    source: OpusPacketSource,
    track: Vec<Track>,
    metas: MetadataLog,
}

enum OpusPacketSource {
    Batch {
        frames: OpusFrameArray,
        index: usize,
    },
    Stream {
        receiver: OpusPacketReceiver,
        next_ts: u64,
    },
}

struct EmptyMetadataReader {
    source: MediaSourceStream,
    metas: MetadataLog,
}

#[gen_stub_pymethods]
#[pymethods]
impl PyOpusPacketInput {
    #[gen_stub(override_return_type(type_repr = "typing.Self", imports = ("typing")))]
    #[new]
    /// Create an Opus packet input.
    ///
    /// Parameters
    /// ----------
    /// frames : pyarrow.BinaryArray | pyarrow.LargeBinaryArray
    ///     One 20 ms Opus frame per row.
    ///
    /// Returns
    /// -------
    /// OpusPacketInput
    fn new(
        #[gen_stub(override_type(
            type_repr = "pyarrow.BinaryArray | pyarrow.LargeBinaryArray",
            imports = ("pyarrow")
        ))]
        frames: PyArray,
    ) -> PyResult<(Self, PyInputBase)> {
        let frames = OpusFrameArray::try_from_array(frames.array().clone())?;
        frames.validate()?;
        Ok((Self { frames }, PyInputBase::new()))
    }

    #[gen_stub(skip)]
    fn _compose(&self, _current_loop: Bound<PyAny>) -> PyResult<PyCompose> {
        let reader = OpusPacketFormatReader::batch(self.frames.clone());
        let input = parsed_input(reader, true)?;
        Ok(PyCompose::new_live(input, None))
    }
}

#[gen_stub_pymethods]
#[pymethods]
impl PyOpusPacketStreamInput {
    #[gen_stub(override_return_type(type_repr = "typing.Self", imports = ("typing")))]
    #[new]
    #[pyo3(signature = (*, max_packets = 128))]
    /// Create a live Opus packet input.
    ///
    /// Parameters
    /// ----------
    /// max_packets : int, optional
    ///     Maximum queued packets before ``send`` applies backpressure.
    ///     Must be greater than zero.
    ///
    /// Returns
    /// -------
    /// OpusPacketStreamInput
    fn new(max_packets: usize) -> PyResult<(Self, PyInputBase)> {
        if max_packets == 0 {
            return Err(PyValueError::new_err(
                "max_packets must be greater than zero",
            ));
        }
        let (sender, receiver) = mpsc::channel(max_packets);
        Ok((
            Self {
                sender: Arc::new(Mutex::new(Some(sender))),
                receiver: Arc::new(Mutex::new(Some(receiver))),
            },
            PyInputBase::new(),
        ))
    }

    /// Send one 20 ms Opus frame.
    ///
    /// Parameters
    /// ----------
    /// packet : bytes
    ///     One non-empty Opus frame containing exactly 960 samples at 48 kHz.
    ///
    /// Returns
    /// -------
    /// None
    fn send<'py>(
        &self,
        py: Python<'py>,
        #[gen_stub(override_type(type_repr = "bytes"))] packet: Vec<u8>,
    ) -> PyResult<PyFuture<'py, ()>> {
        validate_opus_frame(&packet)?;
        let sender = self.sender()?;
        future_into_py(py, async move {
            sender
                .send(packet.into_boxed_slice())
                .await
                .map_err(|_| PyRuntimeError::new_err("OpusPacketStreamInput is closed"))?;
            Ok(())
        })
        .map(|x| x.into())
    }

    /// Close the stream and signal EOF to the player.
    ///
    /// Returns
    /// -------
    /// None
    fn close<'py>(&self, py: Python<'py>) -> PyResult<PyFuture<'py, ()>> {
        let _ = self.take_sender()?;
        future_into_py(py, async move { Ok(()) }).map(|x| x.into())
    }

    #[gen_stub(skip)]
    fn _compose(&self, _current_loop: Bound<PyAny>) -> PyResult<PyCompose> {
        let receiver = self.take_receiver()?;
        let reader = OpusPacketFormatReader::stream(receiver);
        let input = parsed_input(reader, false)?;
        Ok(PyCompose::new_live(input, None))
    }
}

#[gen_stub_pyfunction(module = "discord.ext.songbird.native.player")]
#[pyfunction]
/// Return codec and format identifiers enabled in this native build.
///
/// Returns
/// -------
/// list[str]
pub fn supported_codecs() -> Vec<&'static str> {
    let mut values = vec!["opus", "dca", "raw-pcm"];

    if cfg!(any(feature = "codec-full", feature = "codec-aac")) {
        values.push("aac");
    }
    if cfg!(any(feature = "codec-full", feature = "codec-adpcm")) {
        values.push("adpcm");
    }
    if cfg!(any(feature = "codec-full", feature = "codec-alac")) {
        values.push("alac");
    }
    if cfg!(any(feature = "codec-full", feature = "codec-flac")) {
        values.push("flac");
    }
    if cfg!(any(feature = "codec-full", feature = "codec-mp1")) {
        values.push("mp1");
    }
    if cfg!(any(feature = "codec-full", feature = "codec-mp2")) {
        values.push("mp2");
    }
    if cfg!(any(feature = "codec-full", feature = "codec-mp3")) {
        values.push("mp3");
    }
    if cfg!(any(
        feature = "codec-full",
        feature = "codec-minimal",
        feature = "codec-pcm"
    )) {
        values.push("pcm");
    }
    if cfg!(any(feature = "codec-full", feature = "codec-vorbis")) {
        values.push("vorbis");
    }
    if cfg!(any(feature = "codec-full", feature = "format-aiff")) {
        values.push("format:aiff");
    }
    if cfg!(any(feature = "codec-full", feature = "format-caf")) {
        values.push("format:caf");
    }
    if cfg!(any(feature = "codec-full", feature = "format-isomp4")) {
        values.push("format:isomp4");
    }
    if cfg!(any(feature = "codec-full", feature = "format-mkv")) {
        values.push("format:mkv");
    }
    if cfg!(any(feature = "codec-full", feature = "format-ogg")) {
        values.push("format:ogg");
    }
    if cfg!(any(
        feature = "codec-full",
        feature = "codec-minimal",
        feature = "format-wav"
    )) {
        values.push("format:wav");
    }

    values
}

impl PyOpusPacketStreamInput {
    fn sender(&self) -> PyResult<OpusPacketSender> {
        self.sender
            .lock()
            .map_err(|_| PyRuntimeError::new_err("OpusPacketStreamInput lock is poisoned"))?
            .as_ref()
            .cloned()
            .ok_or_else(|| PyRuntimeError::new_err("OpusPacketStreamInput is closed"))
    }

    fn take_sender(&self) -> PyResult<Option<OpusPacketSender>> {
        Ok(self
            .sender
            .lock()
            .map_err(|_| PyRuntimeError::new_err("OpusPacketStreamInput lock is poisoned"))?
            .take())
    }

    fn take_receiver(&self) -> PyResult<OpusPacketReceiver> {
        self.receiver
            .lock()
            .map_err(|_| PyRuntimeError::new_err("OpusPacketStreamInput lock is poisoned"))?
            .take()
            .ok_or_else(|| {
                PyRuntimeError::new_err("OpusPacketStreamInput has already been composed")
            })
    }
}

impl OpusFrameArray {
    fn try_from_array(array: ArrayRef) -> PyResult<Self> {
        match array.data_type() {
            DataType::Binary => Ok(Self::Binary(
                array
                    .as_any()
                    .downcast_ref::<BinaryArray>()
                    .expect("DataType::Binary must downcast to BinaryArray")
                    .clone(),
            )),
            DataType::LargeBinary => Ok(Self::LargeBinary(
                array
                    .as_any()
                    .downcast_ref::<LargeBinaryArray>()
                    .expect("DataType::LargeBinary must downcast to LargeBinaryArray")
                    .clone(),
            )),
            other => Err(PyTypeError::new_err(format!(
                "Expected a BinaryArray or LargeBinaryArray, got {other}"
            ))),
        }
    }

    fn validate(&self) -> PyResult<()> {
        if self.null_count() != 0 {
            return Err(PyValueError::new_err(
                "Opus packet arrays must not contain nulls",
            ));
        }
        for index in 0..self.len() {
            validate_opus_frame(self.value(index))?;
        }
        Ok(())
    }

    fn len(&self) -> usize {
        match self {
            Self::Binary(array) => array.len(),
            Self::LargeBinary(array) => array.len(),
        }
    }

    fn null_count(&self) -> usize {
        match self {
            Self::Binary(array) => array.null_count(),
            Self::LargeBinary(array) => array.null_count(),
        }
    }

    fn value(&self, index: usize) -> &[u8] {
        match self {
            Self::Binary(array) => array.value(index),
            Self::LargeBinary(array) => array.value(index),
        }
    }

    #[cfg(test)]
    fn value_buffer_ptr(&self) -> *const u8 {
        match self {
            Self::Binary(array) => array.values().as_ptr(),
            Self::LargeBinary(array) => array.values().as_ptr(),
        }
    }
}

impl OpusPacketFormatReader {
    fn batch(frames: OpusFrameArray) -> Self {
        Self::new(OpusPacketSource::Batch { frames, index: 0 })
    }

    fn stream(receiver: OpusPacketReceiver) -> Self {
        Self::new(OpusPacketSource::Stream {
            receiver,
            next_ts: 0,
        })
    }

    fn new(source: OpusPacketSource) -> Self {
        Self {
            source,
            track: vec![Track::new(OPUS_TRACK_ID, opus_codec_params())],
            metas: MetadataLog::default(),
        }
    }
}

impl FormatReader for OpusPacketFormatReader {
    fn try_new(_source: MediaSourceStream, _options: &FormatOptions) -> SymphResult<Self>
    where
        Self: Sized,
    {
        symph_err::unsupported_error("opus packet inputs are constructed directly")
    }

    fn cues(&self) -> &[Cue] {
        &[]
    }

    fn metadata(&mut self) -> SymphMetadata<'_> {
        self.metas.metadata()
    }

    fn seek(&mut self, _mode: SeekMode, to: SeekTo) -> SymphResult<SeekedTo> {
        let ts = seek_timestamp(&self.track[0], to)?;
        match &mut self.source {
            OpusPacketSource::Batch { frames, index } => {
                let max_ts = (frames.len() as u64) * OPUS_FRAME_SAMPLES;
                if ts >= max_ts {
                    return symph_err::seek_error(SeekErrorKind::OutOfRange);
                }
                let packet_index = (ts / OPUS_FRAME_SAMPLES) as usize;
                *index = packet_index;
                Ok(SeekedTo {
                    track_id: OPUS_TRACK_ID,
                    required_ts: ts,
                    actual_ts: (packet_index as u64) * OPUS_FRAME_SAMPLES,
                })
            }
            OpusPacketSource::Stream { .. } => symph_err::seek_error(SeekErrorKind::Unseekable),
        }
    }

    fn tracks(&self) -> &[Track] {
        &self.track
    }

    fn default_track(&self) -> Option<&Track> {
        self.track.first()
    }

    fn next_packet(&mut self) -> SymphResult<Packet> {
        match &mut self.source {
            OpusPacketSource::Batch { frames, index } => {
                if *index >= frames.len() {
                    return symph_err::end_of_stream_error();
                }
                let ts = (*index as u64) * OPUS_FRAME_SAMPLES;
                let frame = frames.value(*index);
                *index += 1;
                Ok(Packet::new_from_slice(
                    OPUS_TRACK_ID,
                    ts,
                    OPUS_FRAME_SAMPLES,
                    frame,
                ))
            }
            OpusPacketSource::Stream { receiver, next_ts } => {
                let Some(frame) = receiver.blocking_recv() else {
                    return symph_err::end_of_stream_error();
                };
                let ts = *next_ts;
                *next_ts += OPUS_FRAME_SAMPLES;
                Ok(Packet::new_from_boxed_slice(
                    OPUS_TRACK_ID,
                    ts,
                    OPUS_FRAME_SAMPLES,
                    frame,
                ))
            }
        }
    }

    fn into_inner(self: Box<Self>) -> MediaSourceStream {
        empty_media_source_stream()
    }
}

impl QueryDescriptor for EmptyMetadataReader {
    fn query() -> &'static [Descriptor] {
        static DESCRIPTORS: &[Descriptor] = &[Descriptor {
            short_name: "discord-ext-songbird-empty",
            long_name: "discord-ext-songbird empty metadata probe",
            extensions: &[],
            mime_types: &[],
            markers: &[b"DESB"],
            score: empty_metadata_score,
            inst: Instantiate::Format(empty_metadata_instantiate),
        }];
        DESCRIPTORS
    }

    fn score(context: &[u8]) -> u8 {
        empty_metadata_score(context)
    }
}

impl FormatReader for EmptyMetadataReader {
    fn try_new(source: MediaSourceStream, _options: &FormatOptions) -> SymphResult<Self>
    where
        Self: Sized,
    {
        Ok(Self {
            source,
            metas: MetadataLog::default(),
        })
    }

    fn cues(&self) -> &[Cue] {
        &[]
    }

    fn metadata(&mut self) -> SymphMetadata<'_> {
        self.metas.metadata()
    }

    fn seek(&mut self, _mode: SeekMode, _to: SeekTo) -> SymphResult<SeekedTo> {
        symph_err::seek_error(SeekErrorKind::Unseekable)
    }

    fn tracks(&self) -> &[Track] {
        &[]
    }

    fn next_packet(&mut self) -> SymphResult<Packet> {
        symph_err::end_of_stream_error()
    }

    fn into_inner(self: Box<Self>) -> MediaSourceStream {
        self.source
    }
}

fn validate_opus_frame(frame: &[u8]) -> PyResult<()> {
    if frame.is_empty() {
        return Err(PyValueError::new_err("Opus frames must not be empty"));
    }
    if frame.len() > i32::MAX as usize {
        return Err(PyValueError::new_err("Opus frame is too large"));
    }

    let sample_count = opus2::packet::get_nb_samples(frame, OPUS_SAMPLE_RATE)
        .map_err(|_| PyValueError::new_err("Opus frame sample count could not be read"))?;
    if sample_count as u64 != OPUS_FRAME_SAMPLES {
        return Err(PyValueError::new_err(format!(
            "Opus frames must contain exactly {OPUS_FRAME_SAMPLES} samples, got {sample_count}"
        )));
    }
    Ok(())
}

fn parsed_input(reader: OpusPacketFormatReader, supports_backseek: bool) -> PyResult<LiveInput> {
    let codec_params = reader
        .default_track()
        .expect("OpusPacketFormatReader always has one track")
        .codec_params
        .clone();
    let decoder =
        songbird::input::codecs::OpusDecoder::try_new(&codec_params, &DecoderOptions::default())
            .map_err(to_py_runtime_error)?;
    let parsed = Parsed {
        format: Box::new(reader),
        decoder: Box::new(decoder),
        track_id: OPUS_TRACK_ID,
        meta: empty_probed_metadata()?,
        supports_backseek,
    };
    Ok(LiveInput::Parsed(parsed))
}

fn opus_codec_params() -> CodecParameters {
    let mut codec_params = CodecParameters::new();
    codec_params
        .for_codec(CODEC_TYPE_OPUS)
        .with_max_frames_per_packet(1)
        .with_sample_rate(OPUS_SAMPLE_RATE)
        .with_time_base(TimeBase::new(1, OPUS_SAMPLE_RATE))
        .with_sample_format(SampleFormat::F32)
        .with_channel_layout(Layout::Stereo);
    codec_params
}

fn seek_timestamp(track: &Track, to: SeekTo) -> SymphResult<u64> {
    match to {
        SeekTo::Time { time, track_id } => {
            if let Some(track_id) = track_id
                && track_id != track.id
            {
                return symph_err::seek_error(SeekErrorKind::InvalidTrack);
            }
            let Some(rate) = track.codec_params.sample_rate else {
                return symph_err::seek_error(SeekErrorKind::Unseekable);
            };
            Ok(TimeBase::new(1, rate).calc_timestamp(time))
        }
        SeekTo::TimeStamp { ts, track_id } => {
            if track_id != track.id {
                return symph_err::seek_error(SeekErrorKind::InvalidTrack);
            }
            Ok(ts)
        }
    }
}

fn empty_probed_metadata() -> PyResult<ProbedMetadata> {
    let mut probe = Probe::default();
    probe.register_all::<EmptyMetadataReader>();
    let source = MediaSourceStream::new(
        Box::new(Cursor::new(b"DESB".to_vec())),
        MediaSourceStreamOptions::default(),
    );
    probe
        .format(
            &Hint::new(),
            source,
            &FormatOptions::default(),
            &songbird::input::core::meta::MetadataOptions::default(),
        )
        .map(|result| result.metadata)
        .map_err(to_py_runtime_error)
}

fn empty_media_source_stream() -> MediaSourceStream {
    MediaSourceStream::new(
        Box::new(Cursor::new(Vec::<u8>::new())),
        MediaSourceStreamOptions::default(),
    )
}

fn empty_metadata_score(_context: &[u8]) -> u8 {
    255
}

fn empty_metadata_instantiate(
    source: MediaSourceStream,
    options: &FormatOptions,
) -> SymphResult<Box<dyn FormatReader>> {
    EmptyMetadataReader::try_new(source, options).map(|reader| Box::new(reader) as _)
}

fn to_py_runtime_error(err: SymphError) -> pyo3::PyErr {
    PyRuntimeError::new_err(err.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use opus2::{Application, Channels, Encoder};

    fn opus_frame_with_samples(samples_per_channel: usize) -> Vec<u8> {
        let mut encoder =
            Encoder::new(OPUS_SAMPLE_RATE, Channels::Stereo, Application::Audio).unwrap();
        let samples = vec![0.0_f32; samples_per_channel * 2];
        encoder.encode_vec_float(&samples, 4_000).unwrap()
    }

    fn opus_frame() -> Vec<u8> {
        opus_frame_with_samples(OPUS_FRAME_SAMPLES as usize)
    }

    #[test]
    fn batch_reader_returns_opus_packets_without_payload_clone_on_array_clone() {
        let first = opus_frame();
        let second = opus_frame();
        let array = BinaryArray::from_vec(vec![first.as_slice(), second.as_slice()]);
        let frames = OpusFrameArray::try_from_array(Arc::new(array)).unwrap();
        frames.validate().unwrap();

        let clone = frames.clone();
        assert_eq!(frames.value_buffer_ptr(), clone.value_buffer_ptr());

        let mut reader = OpusPacketFormatReader::batch(clone);
        assert_eq!(reader.tracks()[0].codec_params.codec, CODEC_TYPE_OPUS);

        let packet = reader.next_packet().unwrap();
        assert_eq!(packet.track_id(), OPUS_TRACK_ID);
        assert_eq!(packet.ts(), 0);
        assert_eq!(packet.dur(), OPUS_FRAME_SAMPLES);
        assert_eq!(packet.buf(), first.as_slice());

        let packet = reader.next_packet().unwrap();
        assert_eq!(packet.ts(), OPUS_FRAME_SAMPLES);
        assert_eq!(packet.dur(), OPUS_FRAME_SAMPLES);
        assert_eq!(packet.buf(), second.as_slice());
    }

    #[test]
    fn batch_reader_seeks_by_timestamp() {
        let first = opus_frame();
        let second = opus_frame();
        let array = LargeBinaryArray::from_vec(vec![first.as_slice(), second.as_slice()]);
        let frames = OpusFrameArray::try_from_array(Arc::new(array)).unwrap();
        let mut reader = OpusPacketFormatReader::batch(frames);

        let seeked = reader
            .seek(
                SeekMode::Accurate,
                SeekTo::TimeStamp {
                    ts: OPUS_FRAME_SAMPLES,
                    track_id: OPUS_TRACK_ID,
                },
            )
            .unwrap();
        assert_eq!(seeked.actual_ts, OPUS_FRAME_SAMPLES);

        let packet = reader.next_packet().unwrap();
        assert_eq!(packet.ts(), OPUS_FRAME_SAMPLES);
        assert_eq!(packet.buf(), second.as_slice());
    }

    #[test]
    fn invalid_packet_arrays_are_rejected() {
        let frame = opus_frame();
        let null_array = BinaryArray::from_opt_vec(vec![Some(frame.as_slice()), None]);
        let frames = OpusFrameArray::try_from_array(Arc::new(null_array)).unwrap();
        assert!(frames.validate().is_err());

        let empty_array = BinaryArray::from_vec(vec![b"".as_slice()]);
        let frames = OpusFrameArray::try_from_array(Arc::new(empty_array)).unwrap();
        assert!(frames.validate().is_err());

        let short_frame = opus_frame_with_samples(480);
        let short_array = BinaryArray::from_vec(vec![short_frame.as_slice()]);
        let frames = OpusFrameArray::try_from_array(Arc::new(short_array)).unwrap();
        assert!(frames.validate().is_err());
    }

    #[test]
    fn stream_reader_blocks_until_packet_then_eofs() {
        let frame = opus_frame();
        let (sender, receiver) = mpsc::channel(1);
        sender.try_send(frame.clone().into_boxed_slice()).unwrap();
        drop(sender);

        let mut reader = OpusPacketFormatReader::stream(receiver);
        let packet = reader.next_packet().unwrap();
        assert_eq!(packet.ts(), 0);
        assert_eq!(packet.dur(), OPUS_FRAME_SAMPLES);
        assert_eq!(packet.buf(), frame.as_slice());
        assert!(reader.next_packet().is_err());
    }

    #[test]
    fn stream_input_rejects_double_compose() {
        let (input, _) = PyOpusPacketStreamInput::new(1).unwrap();
        let _ = input.take_receiver().unwrap();
        assert!(input.take_receiver().is_err());
    }

    #[test]
    fn supported_codecs_reports_default_full_set() {
        let values = supported_codecs();
        assert!(values.contains(&"opus"));
        if cfg!(feature = "codec-full") {
            assert!(values.contains(&"aac"));
            assert!(values.contains(&"format:ogg"));
        }
    }
}
