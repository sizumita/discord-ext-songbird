use crate::player::input::data::AnyVoiceDataArray;
use crate::player::input::{PyCompose, PyInputBase};
use arrow::array::{Array, ArrayRef};
use async_trait::async_trait;
use pyo3::{Bound, PyAny, PyResult, pyclass, pymethods};
use pyo3_arrow::PyArray;
use pyo3_stub_gen::derive::{gen_stub_pyclass, gen_stub_pymethods};
use songbird::input::core::io::MediaSource;
use songbird::input::{AudioStream, AudioStreamError, Compose};

#[gen_stub_pyclass]
#[pyclass(
    name = "AudioInput",
    extends = PyInputBase,
    module = "discord.ext.songbird.native.player",
    skip_from_py_object
)]
/// Encoded audio input backed by a pyarrow array.
///
/// Notes
/// -----
/// The payload format is detected by Songbird/Symphonia.
pub struct PyAudioInput {
    array: ArrayRef,
}

struct ArrayCompose(AnyVoiceDataArray);

#[gen_stub_pymethods]
#[pymethods]
impl PyAudioInput {
    #[gen_stub(override_return_type(type_repr = "typing.Self", imports = ("typing")))]
    #[new]
    /// Create an encoded audio input.
    ///
    /// Parameters
    /// ----------
    /// array : pyarrow.Array
    ///     Encoded audio payload.
    ///
    /// Returns
    /// -------
    /// AudioInput
    fn new(
        #[gen_stub(override_type(type_repr = "pyarrow.Array", imports = ("pyarrow")))]
        array: PyArray,
    ) -> PyResult<(Self, PyInputBase)> {
        let array = array.array();
        if !array.data_type().is_primitive() {
            return Err(pyo3::exceptions::PyTypeError::new_err(
                "Expected a primitive array",
            ));
        }
        Ok((
            Self {
                array: array.clone(),
            },
            PyInputBase::new(),
        ))
    }

    #[gen_stub(skip)]
    fn _compose(&self, _current_loop: Bound<PyAny>) -> PyResult<PyCompose> {
        let compose = ArrayCompose(self.array.clone().try_into()?);
        Ok(PyCompose::new_lazy(Box::new(compose)))
    }
}

#[async_trait]
impl Compose for ArrayCompose {
    fn create(&mut self) -> Result<AudioStream<Box<dyn MediaSource>>, AudioStreamError> {
        Ok(AudioStream {
            input: self.0.clone().try_into_media_source()?,
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
