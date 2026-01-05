#![doc = include_str!("../README.md")]

mod client;
mod error;
mod model;
mod player;
mod receive;
mod update;

use crate::client::SongbirdImpl;
use pyo3::prelude::*;
use pyo3_stub_gen::define_stub_info_gatherer;

/// A Python module implemented in Rust.
#[pymodule]
mod native {
    use pyo3::prelude::*;

    pyo3_stub_gen::module_doc!(
        "discord.ext.songbird.native",
        r#"
Native backend for discord-ext-songbird.

This module contains the Rust-backed implementation used by
`discord.ext.songbird.SongbirdClient`. Most users should import from
`discord.ext.songbird` instead of this module directly.

Submodules
----------
receive
    Voice receive types and sinks (BufferSink, StreamSink).
error
    Backend exceptions.
model
    Internal iterator helpers.

Attributes
----------
VERSION : str
    Package version string.
"#
    );

    pyo3_stub_gen::module_variable!(
        "discord.ext.songbird.native",
        "VERSION",
        &str,
        env!("CARGO_PKG_VERSION")
    );

    #[pymodule_export]
    const VERSION: &str = env!("CARGO_PKG_VERSION");

    #[pymodule_export]
    use super::SongbirdImpl;

    #[pymodule]
    mod model {
        #[pymodule_export]
        use crate::model::PyAsyncIterator;
    }

    #[pymodule]
    mod player {
        use pyo3::prelude::*;

        #[pymodule_export]
        use crate::player::handle::PyTrackHandle;
        #[pymodule_export]
        use crate::player::queue::PyQueue;
        #[pymodule_export]
        use crate::player::track::PyTrack;

        #[pymodule]
        mod input {
            #[pymodule_export]
            use crate::player::input::PyInputBase;
            #[pymodule_export]
            use crate::player::input::audio::PyAudioInput;
            #[pymodule_export]
            use crate::player::input::codec::SupportedCodec;
            #[pymodule_export]
            use crate::player::input::pcm::PyRawPcmInput;
            #[pymodule_export]
            use crate::player::input::stream::PyStreamInput;
        }
    }

    #[pymodule]
    mod receive {
        #[pymodule_export]
        use crate::receive::sink::BufferSink;
        #[pymodule_export]
        use crate::receive::sink::PyStream;
        #[pymodule_export]
        use crate::receive::sink::SinkBase;
        #[pymodule_export]
        use crate::receive::sink::StreamSink;
        #[pymodule_export]
        use crate::receive::tick::VoiceKey;
        #[pymodule_export]
        use crate::receive::tick::VoiceTick;
    }

    #[pymodule]
    mod error {
        #[pymodule_export]
        use crate::error::PyJoinError;
        #[pymodule_export]
        use crate::error::PySongbirdError;
    }

    #[pymodule_init]
    fn init(_: &Bound<'_, PyModule>) -> PyResult<()> {
        pyo3_log::init();
        Ok(())
    }
}

define_stub_info_gatherer!(stub_info);
