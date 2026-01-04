use crate::player::input::{PyCompose, PyInputBase};
use arrow::array::{AsArray, UInt8Array};
use arrow::datatypes::UInt8Type;
use async_trait::async_trait;
use pyo3::{pyclass, pymethods, PyRef, PyResult};
use pyo3_arrow::PyArray;
use pyo3_stub_gen::derive::{gen_stub_pyclass, gen_stub_pyclass_enum, gen_stub_pymethods};
use songbird::input::core::io::MediaSource;
use songbird::input::{AudioStream, AudioStreamError, Compose};
use std::io::Cursor;
use symphonia::core::probe::Hint;

#[gen_stub_pyclass_enum]
#[pyclass(module = "discord.ext.songbird.native.player")]
#[derive(Debug, Clone)]
pub enum SupportedCodec {
    PCM,
    MP3,
    WAVE,
    MKV,
    FLAC,
}

#[gen_stub_pyclass]
#[pyclass(name = "AudioInput", extends = PyInputBase, module = "discord.ext.songbird.native.player")]
pub struct PyAudioInput {
    codec: SupportedCodec,
    array: UInt8Array
}

struct ArrayCompose(UInt8Array, SupportedCodec);

#[gen_stub_pymethods]
#[pymethods]
impl PyAudioInput {
    #[gen_stub(override_return_type(type_repr = "typing.Self", imports = ("typing")))]
    #[new]
    fn new(
        #[gen_stub(override_type(type_repr = "pyarrow.UInt8Array", imports = ("pyarrow")))]
        array: PyArray,
        codec: SupportedCodec,
    ) -> PyResult<(Self, PyInputBase)> {
        println!("{:?}", array.array());
        let array = array.array().as_primitive_opt::<UInt8Type>().clone();
        if array.is_none() {
            return Err(pyo3::exceptions::PyTypeError::new_err(
                "Expected a UInt8Array",
            ));
        }
        Ok((Self {
            codec,
            array: array.unwrap().clone(),
        }, PyInputBase::new()))
    }

    fn codec(slf: PyRef<'_, Self>) {
        slf.as_super().is_lazy();
    }

    #[gen_stub(skip)]
    fn _compose(&self) -> PyResult<PyCompose> {
        let compose = ArrayCompose(self.array.clone(), self.codec.clone());
        Ok(PyCompose::new(Box::new(compose)))
    }
}

#[async_trait]
impl Compose for ArrayCompose {
    fn create(&mut self) -> Result<AudioStream<Box<dyn MediaSource>>, AudioStreamError> {
        Ok(AudioStream {
            input: Box::new(Cursor::new(self.0.values().clone())),
            hint: Some(self.1.clone().into()),
        })
    }

    async fn create_async(&mut self) -> Result<AudioStream<Box<dyn MediaSource>>, AudioStreamError> {
        unimplemented!()
    }

    fn should_create_async(&self) -> bool {
        false
    }
}

impl From<SupportedCodec> for Hint {
    fn from(value: SupportedCodec) -> Self {
        let mut hint = Hint::new();
        match value {
            SupportedCodec::PCM => hint.mime_type("audio/wav"),
            SupportedCodec::MP3 => hint.mime_type("audio/mpeg"),
            SupportedCodec::WAVE => hint.mime_type("audio/wav"),
            SupportedCodec::MKV => hint.mime_type("video/x-matroska"),
            SupportedCodec::FLAC => hint.mime_type("audio/flac")
        };
        hint
    }
}
