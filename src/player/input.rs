pub(crate) mod audio;
pub(crate) mod codec;
mod data;
pub mod pcm;
pub mod stream;

use pyo3::{pyclass, pymethods, Bound, PyAny, PyResult};
use pyo3_stub_gen::derive::{gen_stub_pyclass, gen_stub_pymethods};
use songbird::input::{Compose, Input, LiveInput};

#[gen_stub_pyclass]
#[pyclass(
    name = "InputBase",
    module = "discord.ext.songbird.native.player",
    subclass
)]
pub struct PyInputBase;

#[pyclass(name = "Compose", module = "discord.ext.songbird.native.player")]
pub struct PyCompose(ComposeValue);

pub enum ComposeValue {
    Lazy {
        data: Option<Box<dyn Compose + Send + Sync + 'static>>,
    },
    Live {
        input: Option<LiveInput>,
        data: Option<Box<dyn Compose + Send + Sync + 'static>>,
    },
}

#[gen_stub_pymethods]
#[pymethods]
impl PyInputBase {
    #[gen_stub(skip)]
    fn _compose(&self, _current_loop: Bound<PyAny>) -> PyResult<PyCompose> {
        Err(pyo3::exceptions::PyNotImplementedError::new_err(()))
    }
}

impl PyInputBase {
    pub fn new() -> Self {
        Self {}
    }
}

impl PyCompose {
    pub fn new_lazy(compose: Box<dyn Compose + Send + Sync + 'static>) -> Self {
        Self(ComposeValue::Lazy {
            data: Some(compose),
        })
    }

    pub fn new_live(
        input: LiveInput,
        compose: Option<Box<dyn Compose + Send + Sync + 'static>>,
    ) -> Self {
        Self(ComposeValue::Live {
            input: Some(input),
            data: compose,
        })
    }

    pub fn get_input(&mut self) -> Option<Input> {
        match &mut self.0 {
            ComposeValue::Lazy { data } => data.take().map(|data| Input::Lazy(data)),
            ComposeValue::Live { input, data } => input.take().map(|i| {
                if let Some(d) = data.take() {
                    Input::Live(i, Some(d))
                } else {
                    Input::Live(i, None)
                }
            }),
        }
    }
}
