use crate::player::input::codec::SupportedCodec;
use arrow::array::{
    ArrayRef, AsArray, Float32Array, Float64Array, Int16Array, Int32Array, Int64Array, UInt16Array,
    UInt32Array, UInt8Array,
};
use arrow::datatypes::{
    Float32Type, Float64Type, Int16Type, Int32Type, Int64Type, UInt16Type, UInt32Type, UInt8Type,
};
use bytemuck::cast_slice;
use pyo3::PyErr;
use songbird::input::core::io::MediaSource;
use songbird::input::{AudioStreamError, RawAdapter};
use std::io::{Cursor, ErrorKind};

#[derive(Clone)]
pub enum AnyVoiceDataArray {
    UInt8(UInt8Array),
    UInt16(UInt16Array),
    UInt32(UInt32Array),
    Int16(Int16Array),
    Int32(Int32Array),
    Int64(Int64Array),
    Float32(Float32Array),
    Float64(Float64Array),
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
