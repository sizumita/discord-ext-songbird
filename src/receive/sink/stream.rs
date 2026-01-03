use crate::model::{ArrowArray, Generic, PyAsyncIterator, PyFuture};
use crate::receive::sink::SinkBase;
use crate::receive::tick::{VoiceKey, VoiceTick};
use arrow::array::Int16Array;
use async_trait::async_trait;
use dashmap::DashMap;
use futures::StreamExt;
use pyo3::{pyclass, pymethods, Bound, IntoPyObjectExt, PyAny, PyRef, PyRefMut, PyResult, Python};
use pyo3_async_runtimes::tokio::future_into_py;
use pyo3_stub_gen::derive::{gen_stub_pyclass, gen_stub_pymethods};
use songbird::{CoreEvent, Event, EventContext, EventHandler};
use std::sync::Arc;
use tokio::sync::{broadcast, OwnedSemaphorePermit, Semaphore};
use tokio_stream::wrappers::BroadcastStream;

#[gen_stub_pyclass]
#[pyclass(extends = SinkBase, module = "discord.ext.songbird.native.receive")]
pub struct StreamSink {
    rx: broadcast::Receiver<Option<VoiceTick>>,
    weak_tx: broadcast::WeakSender<Option<VoiceTick>>,
    sem: Arc<Semaphore>,
}

#[gen_stub_pyclass]
#[pyclass(name = "Stream", module = "discord.ext.songbird.native.receive")]
pub struct PyStream {
    acquire: Option<OwnedSemaphorePermit>,
    sem: Arc<Semaphore>,
    weak_tx: broadcast::WeakSender<Option<VoiceTick>>,
}

pub struct StreamSinkHandler {
    tx: broadcast::Sender<Option<VoiceTick>>,
    max_concurrent: usize,
    retain: bool,
    sem: Arc<Semaphore>,
    ssrc_map: DashMap<u32, u64>,
}

#[async_trait]
impl EventHandler for StreamSinkHandler {
    async fn act(&self, ctx: &EventContext<'_>) -> Option<Event> {
        match ctx {
            EventContext::SpeakingStateUpdate(speaking) => {
                if let Some(user_id) = speaking.user_id {
                    self.ssrc_map.insert(speaking.ssrc, user_id.0);
                }
            }
            EventContext::VoiceTick(tick) => {
                if self.sem.available_permits() < self.max_concurrent || self.retain {
                    let tick = VoiceTick::from_parts(tick, &self.ssrc_map);
                    drop(self.tx.send(Some(tick)))
                } else {
                    drop(self.tx.send(None))
                }
            }
            EventContext::ClientDisconnect(disconnect) => {
                self.ssrc_map.retain(|_, v| !disconnect.user_id.0.eq(v));
            }
            _ => {}
        }
        None
    }
}

#[gen_stub_pymethods]
#[pymethods]
impl StreamSink {
    #[gen_stub(override_return_type(type_repr = "typing.Self", imports = ("typing")))]
    #[new]
    #[pyo3(signature = (*, retain = false, retain_secs = 15, max_concurrent = 50))]
    fn new(retain: bool, retain_secs: usize, max_concurrent: usize) -> (StreamSink, SinkBase) {
        let (tx, rx) = broadcast::channel(retain_secs * 50);
        let sem = Arc::new(Semaphore::new(max_concurrent));
        (
            StreamSink {
                rx,
                sem: sem.clone(),
                weak_tx: tx.downgrade(),
            },
            SinkBase {
                subscriber: Arc::new(StreamSinkHandler {
                    tx,
                    max_concurrent,
                    retain,
                    sem,
                    ssrc_map: Default::default(),
                }),
                receive_events: vec![
                    Event::Core(CoreEvent::VoiceTick),
                    Event::Core(CoreEvent::SpeakingStateUpdate),
                    Event::Core(CoreEvent::ClientDisconnect),
                ]
                .into_iter()
                .collect(),
            },
        )
    }

    fn stream(&self) -> PyResult<PyStream> {
        Ok(PyStream {
            acquire: None,
            sem: self.sem.clone(),
            weak_tx: self.weak_tx.clone(),
        })
    }
}

#[gen_stub_pymethods]
#[pymethods]
impl PyStream {
    fn close<'py>(&mut self, py: Python<'py>) -> PyResult<PyFuture<'py, ()>> {
        if let Some(acq) = self.acquire.take() {
            drop(acq);
        }
        future_into_py(py, async move { Ok(()) }).map(|x| x.into())
    }

    fn __aenter__<'py>(mut slf: PyRefMut<Self>, py: Python<'py>) -> PyResult<PyFuture<'py, Self>> {
        if slf.acquire.is_some() {
            return Err(pyo3::exceptions::PyRuntimeError::new_err(
                "StreamSink has already been entered",
            ));
        }
        let acquire = slf.sem.clone().try_acquire_owned().map_err(|_| {
            pyo3::exceptions::PyRuntimeError::new_err("Failed to acquire stream permit")
        })?;
        let _ = slf.acquire.insert(acquire);

        let self_any = slf.into_py_any(py);
        let fut = future_into_py(py, async move { self_any });
        fut.map(|x| x.into())
    }

    fn __aiter__<'py>(slf: PyRef<'py, Self>) -> PyResult<Generic<'py, PyAsyncIterator, VoiceTick>> {
        let tx = slf.try_tx()?;
        let rx = tx.subscribe();
        let stream = BroadcastStream::new(rx).filter_map(|r| async move { r.ok().flatten() });

        Ok(Generic::new(PyAsyncIterator::new(stream)))
    }

    fn __aexit__<'py>(
        &mut self,
        py: Python<'py>,
        _exc_type: Bound<PyAny>,
        _exc_val: Bound<PyAny>,
        _exc_tb: Bound<PyAny>,
    ) -> PyResult<PyFuture<'py, ()>> {
        self.close(py)
    }

    fn __getitem__(
        &self,
        key: VoiceKey,
    ) -> PyResult<Generic<'_, PyAsyncIterator, Option<ArrowArray<'_, Int16Array>>>> {
        let tx = self.try_tx()?;
        let rx = tx.subscribe();
        let stream = BroadcastStream::new(rx)
            .filter_map(|r| async move { r.ok().flatten() })
            .map(move |r| {
                Python::attach(|py| {
                    let k = r.get(py, &key);
                    k.and_then(|x| x.into_py_any(py))
                })
            });

        Ok(Generic::new(PyAsyncIterator::new_in_raw(stream)))
    }
}

impl PyStream {
    fn try_tx(&self) -> PyResult<broadcast::Sender<Option<VoiceTick>>> {
        if self.acquire.is_none() {
            return Err(pyo3::exceptions::PyRuntimeError::new_err(
                "StreamSink has been closed",
            ));
        }
        let Some(tx) = self.weak_tx.upgrade() else {
            return Err(pyo3::exceptions::PyRuntimeError::new_err(
                "StreamSink has been closed",
            ));
        };
        Ok(tx)
    }
}

impl Drop for PyStream {
    fn drop(&mut self) {
        if let Some(acq) = self.acquire.take() {
            drop(acq);
        }
    }
}
