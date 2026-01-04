use pyo3::PyResult;
use pyo3_stub_gen::inventory::submit;
use pyo3_stub_gen::type_info::PyClassInfo;
use pyo3_stub_gen::{PyStubType, TypeInfo};
use songbird::error::JoinError;

pyo3::create_exception!(
    discord.ext.songbird.native.error,
    PySongbirdError,
    pyo3::exceptions::PyException
);

impl PyStubType for PySongbirdError {
    fn type_output() -> TypeInfo {
        TypeInfo::locally_defined(
            "PySongbirdError",
            "discord.ext.songbird.native.error".into(),
        )
    }
}
submit! {
    PyClassInfo {
        pyclass_name: "PySongbirdError",
        struct_id: std::any::TypeId::of::<PySongbirdError>,
        getters: &[],
        setters: &[],
        module: Some("discord.ext.songbird.native.error"),
        doc: "Base exception for Songbird backend errors.",
        bases: &[],
        has_eq: false,
        has_ord: false,
        has_hash: false,
        has_str: false,
        subclass: true,
    }
}

pyo3_stub_gen::create_exception!(
    discord.ext.songbird.native.error,
    PyJoinError,
    PySongbirdError,
    "Raised when a voice join fails."
);
pyo3_stub_gen::create_exception!(
    discord.ext.songbird.native.error,
    PyPlayerError,
    PySongbirdError
);
pyo3_stub_gen::create_exception!(
    discord.ext.songbird.native.error,
    PyControlError,
    PySongbirdError
);


pub trait IntoPyResult<T> {
    fn into_py(self) -> PyResult<T>;
}

impl<T> IntoPyResult<T> for Result<T, JoinError> {
    fn into_py(self) -> PyResult<T> {
        self.map_err(|err| PyJoinError::new_err(err.to_string()))
    }
}
