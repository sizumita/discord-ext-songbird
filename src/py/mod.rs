//! pyclasses of `discord.ext.songbird.native` (§12.6). Fields hold only `Arc<sbcore::…>`, songbird
//! handles or `Copy` data (I1); every class is `frozen`.

pub mod format;
pub mod wake;

pub mod arrow;
pub mod chunk;
pub mod connection;
pub mod events;
pub mod playback;
pub mod receive;
pub mod sources;
pub mod tracks;

#[cfg(feature = "test-hooks")]
pub mod testing;
