use pyo3::pyclass;
use pyo3_stub_gen::derive::gen_stub_pyclass;

#[gen_stub_pyclass]
#[pyclass(name = "StreamInput", module = "discord.ext.songbird.native.player")]
pub struct PyStreamInput {}
