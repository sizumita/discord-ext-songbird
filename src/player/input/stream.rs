use crate::player::input::codec::SupportedCodec;
use crate::player::input::data::AnyVoiceDataArray;
use crate::player::input::{PyCompose, PyInputBase};
use async_trait::async_trait;
use pin_project_lite::pin_project;
use pyo3::{pyclass, pymethods, Bound, BoundObject, Py, PyAny, PyRef, PyResult, Python};
use pyo3_arrow::PyArray;
use pyo3_async_runtimes::tokio::into_future;
use pyo3_async_runtimes::{into_future_with_locals, TaskLocals};
use pyo3_stub_gen::derive::{gen_stub_pyclass, gen_stub_pymethods};
use songbird::input::core::io::{MediaSource, MediaSourceStream, MediaSourceStreamOptions};
use songbird::input::{
    AsyncAdapterStream, AsyncReadOnlySource, AudioStream, AudioStreamError, Compose, LiveInput,
};
use std::future::Future;
use std::io;
use std::io::ErrorKind;
use std::pin::Pin;
use std::task::{Context, Poll};
use tokio::io::{AsyncRead, ReadBuf};

#[gen_stub_pyclass]
#[pyclass(name = "StreamInput", extends = PyInputBase, module = "discord.ext.songbird.native.player")]
pub struct PyStreamInput(Py<PyAny>, SupportedCodec);

struct InputStreamIterator {
    codec: SupportedCodec,
    stream: Option<Py<PyAny>>,
    current_loop: Option<Py<PyAny>>,
}

pin_project! {
    struct AsyncStream {
        stream: Py<PyAny>,
        current_loop: Py<PyAny>,
        pending: Option<Pin<Box<dyn Future<Output = PyResult<Py<PyAny>>> + Send + Sync + 'static>>>,
    }
}

#[gen_stub_pymethods]
#[pymethods]
impl PyStreamInput {
    #[gen_stub(override_return_type(type_repr = "typing.Self", imports = ("typing")))]
    #[new]
    fn new(
        #[gen_stub(override_type(type_repr = "asyncio.StreamReader", imports = ("asyncio")))]
        stream_reader: Bound<PyAny>,
        codec: SupportedCodec,
    ) -> (Self, PyInputBase) {
        (Self(stream_reader.unbind(), codec), PyInputBase::new())
    }

    #[gen_stub(skip)]
    fn _compose<'py>(
        slf: PyRef<Self>,
        py: Python<'py>,
        current_loop: Bound<'py, PyAny>,
    ) -> PyResult<PyCompose> {
        let stream = slf.0.clone_ref(py);
        let codec = slf.1.clone();
        let source = AsyncReadOnlySource::new(AsyncStream {
            stream,
            current_loop: current_loop.unbind(),
            pending: None,
        });
        Ok(PyCompose::new_live(
            LiveInput::Wrapped(AudioStream {
                input: MediaSourceStream::new(
                    Box::new(AsyncAdapterStream::new(Box::new(source), 64 * 1024)),
                    MediaSourceStreamOptions::default(),
                ),
                hint: Some(codec.into()),
            }),
            None,
        ))
    }
}

#[async_trait]
impl Compose for InputStreamIterator {
    fn create(&mut self) -> Result<AudioStream<Box<dyn MediaSource>>, AudioStreamError> {
        unimplemented!()
    }

    async fn create_async(
        &mut self,
    ) -> Result<AudioStream<Box<dyn MediaSource>>, AudioStreamError> {
        let Some(stream) = self.stream.take() else {
            return Err(AudioStreamError::Fail("Stream already consumed".into()));
        };
        let source = AsyncReadOnlySource::new(AsyncStream {
            stream,
            current_loop: self.current_loop.take().unwrap(),
            pending: None,
        });
        Ok(AudioStream {
            input: Box::new(AsyncAdapterStream::new(Box::new(source), 1024)),
            hint: Some(self.codec.clone().into()),
        })
    }

    fn should_create_async(&self) -> bool {
        true
    }
}

impl AsyncRead for AsyncStream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let this = self.get_mut();
        let len = buf.remaining();
        if len == 0 {
            return Poll::Ready(Ok(()));
        }

        if this.pending.is_none() {
            let awaitable = match Python::attach(|py| {
                let awaitable = this.stream.call_method1(py, "read", (len,))?;
                let locals = TaskLocals::new(this.current_loop.bind(py).clone());
                into_future_with_locals(&locals, awaitable.bind(py).clone())
            }) {
                Ok(awaitable) => awaitable,
                Err(err) => return Poll::Ready(Err(io::Error::new(ErrorKind::InvalidInput, err))),
            };
            this.pending = Some(Box::pin(awaitable));
        }

        let poll = {
            let awaitable = this.pending.as_mut().expect("pending must be set");
            awaitable.as_mut().poll(cx)
        };

        match poll {
            Poll::Ready(result) => {
                this.pending = None;
                match result {
                    Ok(array) => {
                        let array =
                            Python::attach(|py| array.extract::<Vec<u8>>(py)).map_err(|_| {
                                io::Error::new(ErrorKind::InvalidInput, "input is not bytes")
                            })?;

                        buf.put_slice(array.as_slice());
                        Poll::Ready(Ok(()))
                    }
                    Err(err) => Poll::Ready(Err(io::Error::new(ErrorKind::InvalidInput, err))),
                }
            }
            Poll::Pending => Poll::Pending,
        }
    }
}
