use crate::player::input::{PyCompose, PyInputBase};
use arrow::array::{
    Array, ArrayRef, AsArray, Float32Array, Float64Array, Int16Array, Int32Array,
    Int64Array, UInt16Array, UInt32Array, UInt8Array,
};
use arrow::datatypes::{
    Float32Type, Float64Type, Int16Type, Int32Type, Int64Type, UInt16Type, UInt32Type,
    UInt8Type,
};
use async_trait::async_trait;
use bytemuck::cast_slice;
use pyo3::{pyclass, pymethods, PyErr, PyRef, PyResult};
use pyo3_arrow::PyArray;
use pyo3_stub_gen::derive::{gen_stub_pyclass, gen_stub_pyclass_enum, gen_stub_pymethods};
use songbird::input::core::io::MediaSource;
use songbird::input::{AudioStream, AudioStreamError, Compose, RawAdapter};
use std::io::{Cursor, ErrorKind};
use symphonia::core::probe::Hint;

#[derive(Clone)]
enum AnyVoiceDataArray {
    UInt8(UInt8Array),
    UInt16(UInt16Array),
    UInt32(UInt32Array),
    Int16(Int16Array),
    Int32(Int32Array),
    Int64(Int64Array),
    Float32(Float32Array),
    Float64(Float64Array),
}

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
}

#[gen_stub_pyclass]
#[pyclass(name = "AudioInput", extends = PyInputBase, module = "discord.ext.songbird.native.player")]
pub struct PyAudioInput {
    codec: SupportedCodec,
    array: ArrayRef,
}

struct ArrayCompose(AnyVoiceDataArray, SupportedCodec);

#[gen_stub_pymethods]
#[pymethods]
impl PyAudioInput {
    #[gen_stub(override_return_type(type_repr = "typing.Self", imports = ("typing")))]
    #[new]
    fn new(
        #[gen_stub(override_type(type_repr = "pyarrow.Array", imports = ("pyarrow")))]
        array: PyArray,
        codec: SupportedCodec,
    ) -> PyResult<(Self, PyInputBase)> {
        let array = array.array();
        if !array.data_type().is_primitive() {
            return Err(pyo3::exceptions::PyTypeError::new_err(
                "Expected a primitive array",
            ));
        }
        Ok((
            Self {
                codec,
                array: array.clone(),
            },
            PyInputBase::new(),
        ))
    }

    fn codec(slf: PyRef<'_, Self>) {
        slf.as_super().is_lazy();
    }

    #[gen_stub(skip)]
    fn _compose(&self) -> PyResult<PyCompose> {
        let compose = ArrayCompose(self.array.clone().try_into()?, self.codec.clone());
        Ok(PyCompose::new(Box::new(compose)))
    }
}

#[async_trait]
impl Compose for ArrayCompose {
    fn create(&mut self) -> Result<AudioStream<Box<dyn MediaSource>>, AudioStreamError> {
        Ok(AudioStream {
            input: self.0.clone().try_into_media_source(self.1.clone())?,
            hint: Some(self.1.clone().into()),
        })
    }

    async fn create_async(
        &mut self,
    ) -> Result<AudioStream<Box<dyn MediaSource>>, AudioStreamError> {
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
            SupportedCodec::PCM => hint.with_extension("rawf32"),
            SupportedCodec::MP3 => hint.mime_type("audio/mpeg"),
            SupportedCodec::WAVE => hint.mime_type("audio/wav"),
            SupportedCodec::MKV => hint.mime_type("video/x-matroska"),
            SupportedCodec::FLAC => hint.mime_type("audio/flac"),
        };
        hint
    }
}

impl AsRef<[u8]> for AnyVoiceDataArray {
    fn as_ref(&self) -> &[u8] {
        match self {
            AnyVoiceDataArray::UInt8(value) => value.values().as_ref(),
            AnyVoiceDataArray::UInt16(value) => cast_slice(value.values().as_ref()),
            AnyVoiceDataArray::UInt32(value) => cast_slice(value.values().as_ref()),
            AnyVoiceDataArray::Int16(value) => cast_slice(value.values().as_ref()),
            AnyVoiceDataArray::Int32(value) => cast_slice(value.values().as_ref()),
            AnyVoiceDataArray::Int64(value) => cast_slice(value.values().as_ref()),
            AnyVoiceDataArray::Float32(value) => cast_slice(value.values().as_ref()),
            AnyVoiceDataArray::Float64(value) => cast_slice(value.values().as_ref()),
        }
    }
}

impl TryFrom<ArrayRef> for AnyVoiceDataArray {
    type Error = PyErr;

    fn try_from(value: ArrayRef) -> Result<Self, Self::Error> {
        if let Some(value) = value.as_primitive_opt::<UInt8Type>() {
            Ok(AnyVoiceDataArray::UInt8(value.clone()))
        } else if let Some(value) = value.as_primitive_opt::<UInt16Type>() {
            Ok(AnyVoiceDataArray::UInt16(value.clone()))
        } else if let Some(value) = value.as_primitive_opt::<UInt32Type>() {
            Ok(AnyVoiceDataArray::UInt32(value.clone()))
        } else if let Some(value) = value.as_primitive_opt::<Int16Type>() {
            Ok(AnyVoiceDataArray::Int16(value.clone()))
        } else if let Some(value) = value.as_primitive_opt::<Int32Type>() {
            Ok(AnyVoiceDataArray::Int32(value.clone()))
        } else if let Some(value) = value.as_primitive_opt::<Int64Type>() {
            Ok(AnyVoiceDataArray::Int64(value.clone()))
        } else if let Some(value) = value.as_primitive_opt::<Float32Type>() {
            Ok(AnyVoiceDataArray::Float32(value.clone()))
        } else if let Some(value) = value.as_primitive_opt::<Float64Type>() {
            Ok(AnyVoiceDataArray::Float64(value.clone()))
        } else {
            Err(pyo3::exceptions::PyTypeError::new_err(format!(
                "Unsupported array data type: {}",
                value.data_type()
            )))
        }
    }
}

impl AnyVoiceDataArray {
    pub fn try_into_media_source(
        self,
        codec: SupportedCodec,
    ) -> Result<Box<dyn MediaSource>, AudioStreamError> {
        if codec == SupportedCodec::PCM {
            if !matches!(self, AnyVoiceDataArray::Float32(_)) {
                return Err(AudioStreamError::Fail(Box::new(std::io::Error::new(
                    ErrorKind::InvalidData,
                    "PCM codec requires Float32 array",
                ))));
            }
            Ok(Box::new(RawAdapter::new(Cursor::new(self), 48000, 2)))
        } else {
            Ok(Box::new(Cursor::new(self)))
        }
    }
}
