use crate::player::input::{PyCompose, PyInputBase};
use arrow::array::{AsArray, Float32Array};
use arrow::datatypes::Float32Type;
use async_trait::async_trait;
use bytemuck::cast_slice;
use pyo3::{pyclass, pymethods, Bound, PyAny, PyResult};
use pyo3_arrow::PyArray;
use pyo3_stub_gen::derive::{gen_stub_pyclass, gen_stub_pymethods};
use songbird::input::core::io::MediaSource;
use songbird::input::{AudioStream, AudioStreamError, Compose, RawAdapter};
use std::io::Cursor;
use symphonia::core::probe::Hint;

#[gen_stub_pyclass]
#[pyclass(name = "RawPCMInput", extends = PyInputBase, module = "discord.ext.songbird.native.player.input")]
/// Raw PCM input backed by a float32 array.
///
/// Notes
/// -----
/// Samples are expected to be interleaved float32 PCM.
pub struct PyRawPcmInput {
    array: Float32Array,
    sample_rate: u32,
    channels: u32,
}

struct PcmCompose(Float32Array, u32, u32);
struct PcmArray(Float32Array);

#[gen_stub_pymethods]
#[pymethods]
impl PyRawPcmInput {
    #[gen_stub(override_return_type(type_repr = "typing.Self", imports = ("typing")))]
    #[new]
    #[pyo3(signature = (array, *, sample_rate = 48000, channels = 2))]
    /// Create a raw PCM input.
    ///
    /// Parameters
    /// ----------
    /// array : pyarrow.Float32Array
    ///     Interleaved PCM samples.
    /// sample_rate : int, optional
    ///     Sample rate in Hz.
    /// channels : int, optional
    ///     Channel count.
    ///
    /// Returns
    /// -------
    /// RawPCMInput
    fn new(
        #[gen_stub(override_type(type_repr = "pyarrow.Float32Array", imports = ("pyarrow")))]
        array: PyArray,
        sample_rate: u32,
        channels: u32,
    ) -> PyResult<(Self, PyInputBase)> {
        let array = array.array();
        let Some(array) = array.as_primitive_opt::<Float32Type>() else {
            return Err(pyo3::exceptions::PyTypeError::new_err(
                "Expected a Float32 array",
            ));
        };
        Ok((
            Self {
                array: array.clone(),
                sample_rate,
                channels,
            },
            PyInputBase::new(),
        ))
    }

    #[gen_stub(skip)]
    fn _compose(&self, _current_loop: Bound<PyAny>) -> PyResult<PyCompose> {
        let compose = PcmCompose(self.array.clone(), self.sample_rate, self.channels);
        Ok(PyCompose::new_lazy(Box::new(compose)))
    }
}
#[async_trait]
impl Compose for PcmCompose {
    fn create(&mut self) -> Result<AudioStream<Box<dyn MediaSource>>, AudioStreamError> {
        Ok(AudioStream {
            input: Box::new(RawAdapter::new(
                Cursor::new(PcmArray(self.0.clone())),
                self.1,
                self.2,
            )),
            hint: Some({
                let mut hint = Hint::new();
                hint.with_extension("rawf32");
                hint
            }),
        })
    }

    async fn create_async(
        &mut self,
    ) -> Result<AudioStream<Box<dyn MediaSource>>, AudioStreamError> {
        todo!()
    }

    fn should_create_async(&self) -> bool {
        false
    }
}

impl AsRef<[u8]> for PcmArray {
    fn as_ref(&self) -> &[u8] {
        cast_slice(self.0.values().as_ref())
    }
}
