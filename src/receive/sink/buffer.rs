use crate::model::{Generic, PyAsyncIterator};
use crate::receive::sink::SinkBase;
use crate::receive::tick::{VoiceKey, VoiceTick};
use arrow::array::Int16Array;
use async_stream::stream;
use async_trait::async_trait;
use dashmap::DashMap;
use pyo3::{pyclass, pymethods, IntoPyObjectExt, PyRef, PyResult, Python};
use pyo3_stub_gen::derive::{gen_stub_pyclass, gen_stub_pymethods};
use songbird::{CoreEvent, Event, EventContext, EventHandler};
use std::collections::{HashSet, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::sync::Mutex;

pub struct BufferSinkHandler {
    is_stopped: Arc<AtomicBool>,
    ssrc_map: DashMap<u32, u64>,
    ticks: Arc<Mutex<VecDeque<VoiceTick>>>,
    max_ticks: Option<usize>,
}

#[gen_stub_pyclass]
#[pyclass(extends=SinkBase, module = "discord.ext.songbird.native.receive")]
pub struct BufferSink {
    is_stopped: Arc<AtomicBool>,
    ticks: Arc<Mutex<VecDeque<VoiceTick>>>,
}

impl BufferSinkHandler {
    pub fn new(
        ticks: Arc<Mutex<VecDeque<VoiceTick>>>,
        is_stopped: Arc<AtomicBool>,
        max_ticks: Option<usize>,
    ) -> Self {
        Self {
            is_stopped,
            ssrc_map: DashMap::new(),
            ticks,
            max_ticks,
        }
    }
}

#[async_trait]
impl EventHandler for BufferSinkHandler {
    async fn act(&self, ctx: &EventContext<'_>) -> Option<Event> {
        if self.is_stopped.load(Ordering::Relaxed) {
            return None;
        }
        match ctx {
            EventContext::SpeakingStateUpdate(speaking) => {
                if let Some(user_id) = speaking.user_id {
                    self.ssrc_map.insert(speaking.ssrc, user_id.0);
                }
            }
            EventContext::VoiceTick(tick) => {
                let mut silents = tick
                    .silent
                    .iter()
                    .map(|ssrc| {
                        if let Some(user_id) = self.ssrc_map.get(ssrc) {
                            VoiceKey::User(*user_id)
                        } else {
                            VoiceKey::Unknown(*ssrc)
                        }
                    })
                    .collect::<HashSet<_>>();
                let payloads = DashMap::with_capacity(tick.speaking.len());
                for (ssrc, data) in &tick.speaking {
                    let key = if let Some(user_id) = self.ssrc_map.get(ssrc) {
                        VoiceKey::User(*user_id)
                    } else {
                        VoiceKey::Unknown(*ssrc)
                    };
                    if let Some(decoded) = &data.decoded_voice {
                        payloads.insert(key, Arc::new(Int16Array::from(decoded.clone())));
                    } else {
                        silents.insert(key);
                    }
                }
                let tick = VoiceTick {
                    speaking: payloads,
                    silent: silents,
                };
                let mut guard = self.ticks.lock().await;
                if let Some(max_in_seconds) = self.max_ticks {
                    while guard.len() >= max_in_seconds {
                        guard.pop_front();
                    }
                }
                guard.push_back(tick);
            }
            _ => {}
        }
        None
    }
}

#[gen_stub_pymethods]
#[pymethods]
impl BufferSink {
    #[gen_stub(override_return_type(type_repr = "typing.Self", imports = ("typing")))]
    #[new]
    #[pyo3(signature = (max_in_seconds = None))]
    fn new(max_in_seconds: Option<usize>) -> PyResult<(Self, SinkBase)> {
        let is_stopped = Arc::new(AtomicBool::new(false));
        let max_ticks = max_in_seconds.map(|secs| secs * 50);
        let ticks = Arc::new(Mutex::new(if let Some(max) = max_ticks {
            VecDeque::with_capacity(max)
        } else {
            VecDeque::new()
        }));
        let handler = BufferSinkHandler::new(ticks.clone(), is_stopped.clone(), max_in_seconds);
        Ok((
            Self { is_stopped, ticks },
            SinkBase::new(
                Arc::new(handler),
                vec![
                    Event::Core(CoreEvent::VoiceTick),
                    Event::Core(CoreEvent::SpeakingStateUpdate),
                ]
                .into_iter()
                .collect(),
            )?,
        ))
    }

    fn stop(&self) {
        self.is_stopped.store(true, Ordering::Relaxed);
    }

    #[gen_stub(override_return_type(
        type_repr = "PyAsyncIterator[typing.Optional[typing.Tuple[VoiceState.Speaking, pyarrow.Int16Array] | typing.Tuple[VoiceState.Silent, None]]]",
        imports = ("typing", "pyarrow")
    ))]
    fn __getitem__(&self, key: VoiceKey) -> PyResult<PyAsyncIterator> {
        let ticks = self.ticks.clone();
        let s = stream! {
            loop {
                let tick = {
                    let mut guard = ticks.lock().await;
                    if let Some(tick) = guard.pop_front() {
                        Python::attach(|py| {
                            let k = tick.get(py, &key);
                            k.and_then(|x| x.into_py_any(py))
                        })
                    } else {
                        drop(guard);
                        break;
                    }
                };
                yield tick;
            }
        };
        Ok(PyAsyncIterator::new(s))
    }

    fn __aiter__<'py>(slf: PyRef<'py, Self>) -> Generic<'py, PyAsyncIterator, VoiceTick> {
        let ticks = slf.ticks.clone();
        let s = stream! {
            loop {
                let tick = {
                    let mut guard = ticks.lock().await;
                    if let Some(tick) = guard.pop_front() {
                        Python::attach(|py| tick.into_py_any(py))
                    } else {
                        drop(guard);
                        break;
                    }
                };
                yield tick;
            }
        };
        Generic::new(PyAsyncIterator::new(s))
    }
}
