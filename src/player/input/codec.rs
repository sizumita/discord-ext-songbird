use pyo3::pyclass;
use pyo3_stub_gen::derive::gen_stub_pyclass_enum;
use songbird::input::core::probe::Hint;

#[gen_stub_pyclass_enum]
#[pyclass(module = "discord.ext.songbird.native.player")]
#[derive(Debug, Clone, Eq, PartialEq)]
pub enum SupportedCodec {
    /// Raw f32 Stereo PCM
    PCM,
    MP3,
    WAVE,
    MKV,
    FLAC,
    AAC,
}

impl From<SupportedCodec> for Hint {
    fn from(value: SupportedCodec) -> Self {
        let mut hint = Hint::new();
        match value {
            SupportedCodec::PCM => hint.with_extension("rawf32"),
            SupportedCodec::MP3 => hint.mime_type("audio/mpeg"),
            SupportedCodec::WAVE => hint.mime_type("audio/wav"),
            SupportedCodec::MKV => hint.mime_type("video/x-matroska"),
            SupportedCodec::FLAC => hint.mime_type("audio/flac"),
            SupportedCodec::AAC => hint.mime_type("audio/aac"),
        };
        hint
    }
}
