use crate::future::PyFuture;
use crate::receive::buffer::DefaultBuffer;
use crate::receive::ssrc::SsrcManager;
use crate::receive::system::SystemEvent;
use crate::receive::tick::VoiceTick;
use pyo3::{pyclass, pymethods, Bound, PyAny, PyErr, PyRef, PyResult, Python};
use pyo3_async_runtimes::tokio::future_into_py;
use pyo3_stub_gen::derive::{gen_stub_pyclass, gen_stub_pymethods};
use songbird::{CoreEvent, Event, EventHandler};
use std::collections::HashSet;
use std::sync::Arc;
use tokio::sync::{mpsc, watch, Mutex};

#[gen_stub_pyclass]
#[pyclass(subclass, module = "discord.ext.songbird.native.receive")]
pub struct SinkBase {
    can_multi_subscribe: bool,
    ssrc: Arc<Mutex<SsrcManager>>,
    system_rx: Option<mpsc::Receiver<SystemEvent>>,
    system_fut: Arc<Mutex<Option<tokio::task::JoinHandle<()>>>>,
    subscriber: Arc<dyn EventHandler + Send>,
    pub receive_events: HashSet<Event>,
}

impl Drop for SinkBase {
    fn drop(&mut self) {
        if let Some(handle) = self.system_fut.blocking_lock().take() {
            handle.abort();
        }
    }
}

impl SinkBase {
    fn new(
        can_multi_subscribe: bool,
        system_rx: mpsc::Receiver<SystemEvent>,
        subscriber: Arc<dyn EventHandler + Send>,
        receive_events: HashSet<Event>,
    ) -> PyResult<Self> {
        let ssrc = Arc::new(Mutex::new(SsrcManager::new()));

        let ssrc_clone = ssrc.clone();

        Ok(Self {
            can_multi_subscribe,
            ssrc: ssrc_clone,
            system_rx: Some(system_rx),
            system_fut: Arc::new(Mutex::new(None)),
            subscriber,
            receive_events,
        })
    }

    pub fn start_system_event_loop<'py>(&mut self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let mut system_rx = self.system_rx.take().ok_or_else(|| {
            PyErr::new::<pyo3::exceptions::PyException, _>("System event loop already started")
        })?;
        let system_fut = self.system_fut.clone();
        let ssrc = self.ssrc.clone();
        future_into_py(py, async move {
            let fut = tokio::spawn(async move {
                loop {
                    match system_rx.recv().await {
                        None => break,
                        Some(e) => match e {
                            SystemEvent::SpeakingStateUpdate(speaking) => {
                                let mut ssrc_lock = ssrc.lock().await;
                                ssrc_lock.insert(speaking.user_id.unwrap(), speaking.ssrc);
                            }
                            SystemEvent::ClientDisconnect(user_id) => {
                                let mut ssrc_lock = ssrc.lock().await;
                                ssrc_lock.remove_by_user(&user_id);
                            }
                        },
                    }
                }
            });
            *system_fut.lock().await = Some(fut);
            Ok(())
        })
    }

    pub fn get_subscriber(&self) -> Arc<dyn EventHandler + Send> {
        self.subscriber.clone()
    }
}

#[gen_stub_pymethods]
#[pymethods]
impl SinkBase {
    /// Resolve a user ID for the given SSRC.
    ///
    /// Parameters
    /// ----------
    /// ssrc: int
    ///     The SSRC to resolve.
    ///
    /// Returns
    /// -------
    /// int | None
    ///     The user ID if known, otherwise None.
    fn get_user_id(&self, ssrc: u32) -> PyResult<Option<u64>> {
        let ssrc_lock = self.ssrc.blocking_lock();
        if let Some(user_id) = ssrc_lock.get_user(&ssrc) {
            Ok(Some(user_id.0))
        } else {
            Ok(None)
        }
    }
}

#[gen_stub_pyclass]
#[pyclass(extends=SinkBase, subclass, module = "discord.ext.songbird.native.receive")]
pub struct DefaultSink {
    voice_rx: Arc<Mutex<watch::Receiver<VoiceTick>>>,
}

#[gen_stub_pymethods]
#[pymethods]
impl DefaultSink {
    #[gen_stub(override_return_type(type_repr = "DefaultSink"))]
    #[new]
    /// Create a default receive sink.
    ///
    /// This sink yields `VoiceTick` objects and tracks speaking state.
    ///
    /// Returns
    /// -------
    /// DefaultSink
    ///     The created sink.
    fn new() -> PyResult<(Self, SinkBase)> {
        let (voice_tx, mut voice_rx) = watch::channel(VoiceTick::default());
        let _ = voice_rx.borrow_and_update();
        let (system_tx, system_rx) = mpsc::channel::<SystemEvent>(100);
        Ok((
            Self {
                voice_rx: Arc::new(Mutex::new(voice_rx)),
            },
            SinkBase::new(
                true,
                system_rx,
                Arc::new(DefaultBuffer::new(voice_tx, system_tx)),
                vec![
                    Event::Core(CoreEvent::VoiceTick),
                    Event::Core(CoreEvent::SpeakingStateUpdate),
                    Event::Core(CoreEvent::ClientDisconnect),
                ]
                .into_iter()
                .collect(),
            )?,
        ))
    }

    /// Return this sink as an async iterator.
    ///
    /// Returns
    /// -------
    /// DefaultSink
    ///     This sink instance.
    fn __aiter__(slf: PyRef<Self>) -> PyRef<Self> {
        slf
    }

    /// |coro|
    ///
    /// Await the next voice tick.
    ///
    /// Returns
    /// -------
    /// VoiceTick
    ///     The next voice tick.
    ///
    /// Raises
    /// ------
    /// StopAsyncIteration
    ///     If the sink is closed.
    fn __anext__<'py>(&self, py: Python<'py>) -> PyResult<PyFuture<'py, VoiceTick>> {
        let voice_rx = self.voice_rx.clone();
        future_into_py(py, async move {
            let mut rx = voice_rx.lock().await;
            if rx.changed().await.is_err() {
                Err(pyo3::exceptions::PyStopAsyncIteration::new_err(()))
            } else {
                Ok(rx.borrow().clone())
            }
        })
        .map(|x| x.into())
    }
}
