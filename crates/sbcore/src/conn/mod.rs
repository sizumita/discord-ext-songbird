//! Connection & control (W2, §4, §6, §12.3).
//!
//! * [`settings`]: validated per-connection settings.
//! * [`gateway`]: the pure connection state machine (§4.5).
//! * [`driver`]: sole ownership of the songbird `Driver` (§4.8).
//! * [`dispatch`]: the single per-connection songbird event handler (§8.2).
//! * [`worker`]: the per-connection ConnWorker task (receive DSP, housekeeping, timers).
//! * [`ops`]: asynchronous track operations (`seek`, `info`).
//! * [`shared`]: [`ConnShared`], the synchronous facade the binding calls.
//! * [`registry`]: the process-wide registry of live connections (`_shutdown_all`).

pub mod dispatch;
pub mod driver;
pub mod gateway;
pub mod ops;
pub mod registry;
pub mod settings;
pub mod shared;
pub mod worker;

pub use registry::{registry_snapshot, shutdown_all};
pub use settings::{ConnParams, RawVoiceSettings, VoiceSettings};
pub use shared::ConnShared;
