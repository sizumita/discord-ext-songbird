use crate::model::{ArrowArray, ArrowRecordBatch, Generic, PyAsyncIterator, PyFuture};
use crate::receive::identity::VoiceIdentityBinding;
use crate::receive::sink::SinkBase;
use crate::receive::tick::{VoiceKey, VoiceTickBatch};
use arrow::array::Int16Array;
use async_trait::async_trait;
use futures::StreamExt;
use pyo3::exceptions::PyValueError;
use pyo3::{Bound, IntoPyObjectExt, PyAny, PyRef, PyRefMut, PyResult, Python, pyclass, pymethods};
use pyo3_async_runtimes::tokio::future_into_py;
use pyo3_stub_gen::derive::{gen_stub_pyclass, gen_stub_pymethods};
use songbird::{CoreEvent, Event, EventContext, EventHandler};
use std::sync::Arc;
use tokio::sync::{OwnedSemaphorePermit, Semaphore, broadcast};
use tokio_stream::wrappers::BroadcastStream;

#[gen_stub_pyclass]
#[pyclass(
    extends = SinkBase,
    module = "discord.ext.songbird.native.receive",
    skip_from_py_object
)]
/// Streaming sink for received voice data.
///
/// Unlike `BufferSink`, this sink exposes a stream interface backed by a
/// broadcast channel and supports concurrent consumers via permits. Stream
/// iteration yields Arrow record batches.
///
/// Examples
/// --------
/// ```python
/// from discord.ext import songbird
/// from discord.ext.songbird import receive
///
/// vc = await channel.connect(cls=songbird.SongbirdClient)
/// sink = receive.StreamSink()
/// vc.listen(sink)
///
/// async with sink.stream() as stream:
///     async for batch in stream:
///         ...
/// ```
pub struct StreamSink {
    _rx: broadcast::Receiver<Arc<VoiceTickBatch>>,
    weak_tx: broadcast::WeakSender<Arc<VoiceTickBatch>>,
    sem: Arc<Semaphore>,
}

#[gen_stub_pyclass]
#[pyclass(
    name = "Stream",
    module = "discord.ext.songbird.native.receive",
    skip_from_py_object
)]
/// Async stream handle returned by `StreamSink.stream()`.
///
/// This object is an async context manager that acquires a stream permit.
pub struct PyStream {
    acquire: Option<OwnedSemaphorePermit>,
    sem: Arc<Semaphore>,
    weak_tx: broadcast::WeakSender<Arc<VoiceTickBatch>>,
}

pub struct StreamSinkHandler {
    tx: broadcast::Sender<Arc<VoiceTickBatch>>,
    max_concurrent: usize,
    retain: bool,
    sem: Arc<Semaphore>,
    identity: Arc<VoiceIdentityBinding>,
}

#[async_trait]
impl EventHandler for StreamSinkHandler {
    async fn act(&self, ctx: &EventContext<'_>) -> Option<Event> {
        match ctx {
            EventContext::VoiceTick(tick)
                if self.sem.available_permits() < self.max_concurrent || self.retain =>
            {
                let tick = VoiceTickBatch::from_parts(tick, &*self.identity);
                drop(self.tx.send(tick))
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
    /// Create a new StreamSink.
    ///
    /// Parameters
    /// ----------
    /// retain : bool, optional
    ///     If True, ticks are retained even when no streams are active.
    /// retain_secs : int, optional
    ///     Retention window in seconds for the broadcast buffer.
    ///     Internally converted to a tick count based on 20 ms per tick (50 ticks/sec).
    ///     Must be greater than zero.
    /// max_concurrent : int, optional
    ///     Maximum number of concurrent streams.
    ///     Must be greater than zero.
    ///
    /// Returns
    /// -------
    /// StreamSink
    fn new(
        retain: bool,
        retain_secs: usize,
        max_concurrent: usize,
    ) -> PyResult<(StreamSink, SinkBase)> {
        if retain_secs == 0 {
            return Err(PyValueError::new_err(
                "retain_secs must be greater than zero",
            ));
        }
        if max_concurrent == 0 {
            return Err(PyValueError::new_err(
                "max_concurrent must be greater than zero",
            ));
        }
        let retain_ticks = retain_secs
            .checked_mul(50)
            .ok_or_else(|| PyValueError::new_err("retain_secs is too large"))?;
        let (tx, rx) = broadcast::channel(retain_ticks);
        let sem = Arc::new(Semaphore::new(max_concurrent));
        let identity = Arc::new(VoiceIdentityBinding::default());
        Ok((
            StreamSink {
                _rx: rx,
                sem: sem.clone(),
                weak_tx: tx.downgrade(),
            },
            SinkBase {
                subscriber: Arc::new(StreamSinkHandler {
                    tx,
                    max_concurrent,
                    retain,
                    sem,
                    identity: identity.clone(),
                }),
                identity,
                receive_events: vec![Event::Core(CoreEvent::VoiceTick)]
                    .into_iter()
                    .collect(),
            },
        ))
    }

    /// Create an async stream handle.
    ///
    /// Use this with `async with` to acquire a stream permit.
    ///
    /// Returns
    /// -------
    /// Stream
    ///
    /// Examples
    /// --------
    /// ```python
    /// async with sink.stream() as stream:
    ///     async for batch in stream:
    ///         ...
    /// ```
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
    /// Close the stream and release its permit.
    ///
    /// Returns
    /// -------
    /// None
    fn close<'py>(&mut self, py: Python<'py>) -> PyResult<PyFuture<'py, ()>> {
        if let Some(acq) = self.acquire.take() {
            drop(acq);
        }
        future_into_py(py, async move { Ok(()) }).map(|x| x.into())
    }

    /// Enter the async context and acquire a stream permit.
    ///
    /// Returns
    /// -------
    /// Stream
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

    /// Return an async iterator over Arrow record batches.
    ///
    /// Returns
    /// -------
    /// PyAsyncIterator[pyarrow.RecordBatch]
    fn __aiter__<'py>(
        slf: PyRef<'py, Self>,
    ) -> PyResult<Generic<'py, PyAsyncIterator, ArrowRecordBatch<'py>>> {
        let tx = slf.try_tx()?;
        let rx = tx.subscribe();
        let stream = BroadcastStream::new(rx)
            .filter_map(|r| async move { r.ok() })
            .map(|r| Python::attach(|py| r.to_record_batch(py).and_then(|x| x.into_py_any(py))));

        Ok(Generic::new(PyAsyncIterator::new_in_raw(stream)))
    }

    /// Exit the async context and release the stream permit.
    ///
    /// Returns
    /// -------
    /// None
    fn __aexit__<'py>(
        &mut self,
        py: Python<'py>,
        _exc_type: Bound<PyAny>,
        _exc_val: Bound<PyAny>,
        _exc_tb: Bound<PyAny>,
    ) -> PyResult<PyFuture<'py, ()>> {
        self.close(py)
    }

    /// Return an async iterator over PCM for a specific key.
    ///
    /// Parameters
    /// ----------
    /// key : VoiceKey
    ///     The user/ssrc key to filter.
    ///
    /// Returns
    /// -------
    /// PyAsyncIterator[pyarrow.Int16Array | None]
    ///
    /// Examples
    /// --------
    /// ```python
    /// async with sink.stream() as stream:
    ///     async for pcm in stream[receive.VoiceKey.User(user_id)]:
    ///         if pcm is not None:
    ///             handle_pcm(pcm)
    /// ```
    fn __getitem__(
        &self,
        key: VoiceKey,
    ) -> PyResult<Generic<'_, PyAsyncIterator, Option<ArrowArray<'_, Int16Array>>>> {
        let tx = self.try_tx()?;
        let rx = tx.subscribe();
        let stream = BroadcastStream::new(rx)
            .filter_map(|r| async move { r.ok() })
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
    fn try_tx(&self) -> PyResult<broadcast::Sender<Arc<VoiceTickBatch>>> {
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
