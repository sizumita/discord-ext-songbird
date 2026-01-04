use pyo3::pyclass;
use pyo3_stub_gen::derive::gen_stub_pyclass_enum;
use songbird::input::core::probe::Hint;

#[gen_stub_pyclass_enum]
#[pyclass(
    module = "discord.ext.songbird.native.player.input",
    rename_all = "UPPERCASE"
)]
#[derive(Debug, Clone, Eq, PartialEq)]
pub enum SupportedCodec {
    Mp3,
    Wave,
    Mkv,
    Flac,
    Aac,
}

impl From<SupportedCodec> for Hint {
    fn from(value: SupportedCodec) -> Self {
        let mut hint = Hint::new();
        match value {
            SupportedCodec::Mp3 => hint.mime_type("audio/mpeg"),
            SupportedCodec::Wave => hint.mime_type("audio/wav"),
            SupportedCodec::Mkv => hint.mime_type("video/x-matroska"),
            SupportedCodec::Flac => hint.mime_type("audio/flac"),
            SupportedCodec::Aac => hint.mime_type("audio/aac"),
        };
        hint
    }
}
