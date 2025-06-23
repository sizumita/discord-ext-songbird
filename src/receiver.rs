use async_trait::async_trait;
use pyo3::prelude::*;
use pyo3::types::PyBytes;
use pyo3_async_runtimes::tokio::into_future;
use songbird::{
    events::{EventContext, EventHandler},
    model::payload::Speaking,
    Event,
};

#[pyclass]
#[derive(Clone)]
pub struct PyVoicePacket {
    #[pyo3(get)]
    pub ssrc: u32,
    #[pyo3(get)]
    pub sequence: Option<u16>,
    #[pyo3(get)]
    pub timestamp: Option<u32>,
    pub opus_data: Vec<u8>,
    pub rtp_data: Vec<u8>,
    pub decoded_voice: Option<Vec<i16>>,
}

#[pymethods]
impl PyVoicePacket {
    #[getter]
    fn opus_data<'py>(&self, py: Python<'py>) -> Bound<'py, PyBytes> {
        PyBytes::new(py, &self.opus_data)
    }

    #[getter]
    fn rtp_data<'py>(&self, py: Python<'py>) -> Bound<'py, PyBytes> {
        PyBytes::new(py, &self.rtp_data)
    }

    #[getter]
    fn decoded_voice<'py>(&self, py: Python<'py>) -> Option<Bound<'py, PyBytes>> {
        self.decoded_voice.as_ref().map(|voice| {
            let bytes: Vec<u8> = voice
                .iter()
                .flat_map(|&sample| sample.to_le_bytes())
                .collect();
            PyBytes::new(py, &bytes)
        })
    }
}

pub struct ReceiverAdapter {
    py_receiver: PyObject,
}

impl ReceiverAdapter {
    pub fn new(py_receiver: PyObject) -> Self {
        Self { py_receiver }
    }
}

impl Clone for ReceiverAdapter {
    fn clone(&self) -> Self {
        Python::with_gil(|py| Self {
            py_receiver: self.py_receiver.clone_ref(py),
        })
    }
}

#[async_trait]
impl EventHandler for ReceiverAdapter {
    async fn act(&self, ctx: &EventContext<'_>) -> Option<Event> {
        let py_receiver = Python::with_gil(|py| self.py_receiver.clone_ref(py));

        match ctx {
            EventContext::VoiceTick(tick) => {
                // Handle speaking users (those with voice data)
                for (ssrc, voice_data) in &tick.speaking {
                    let (sequence, timestamp, opus_data, rtp_data) =
                        if let Some(packet) = &voice_data.packet {
                            let rtp_packet = packet.rtp();
                            let payload_start = packet.payload_offset;
                            let payload_end = packet.packet.len() - packet.payload_end_pad;
                            let payload = if payload_start < payload_end {
                                packet.packet[payload_start..payload_end].to_vec()
                            } else {
                                Vec::new()
                            };
                            (
                                Some(rtp_packet.get_sequence().into()),
                                Some(rtp_packet.get_timestamp().into()),
                                payload,
                                packet.packet.to_vec(),
                            )
                        } else {
                            // Packet was lost, but we might have decoded audio
                            (None, None, Vec::new(), Vec::new())
                        };

                    let packet = PyVoicePacket {
                        ssrc: *ssrc,
                        sequence,
                        timestamp,
                        opus_data,
                        rtp_data,
                        decoded_voice: voice_data.decoded_voice.clone(),
                    };

                    let ssrc = *ssrc;

                    if let Ok(future) = Python::with_gil(|py| {
                        // Call the Python method and get the coroutine
                        py_receiver
                            .call_method1(py, "voice_packet", (ssrc, packet))
                            .and_then(|coro| {
                                let bound_coro = coro.into_bound(py);
                                into_future(bound_coro)
                            })
                    }) {
                        let _ = future.await;
                    }
                }
            }
            EventContext::SpeakingStateUpdate(Speaking {
                ssrc,
                user_id,
                speaking,
                ..
            }) => {
                // Convert SpeakingState to bool - user is speaking if any flag is set
                let is_speaking = !speaking.is_empty();
                let data = (*ssrc, user_id.map(|id| id.0), is_speaking);

                if let Ok(future) = Python::with_gil(|py| {
                    py_receiver
                        .call_method1(py, "speaking_update", data)
                        .and_then(|coro| {
                            let bound_coro = coro.into_bound(py);
                            into_future(bound_coro)
                        })
                }) {
                    let _ = future.await;
                }
            }
            EventContext::DriverConnect(_) => {
                if let Ok(future) = Python::with_gil(|py| {
                    py_receiver
                        .call_method0(py, "driver_connect")
                        .and_then(|coro| {
                            let bound_coro = coro.into_bound(py);
                            into_future(bound_coro)
                        })
                }) {
                    let _ = future.await;
                }
            }
            EventContext::DriverDisconnect(_) => {
                if let Ok(future) = Python::with_gil(|py| {
                    py_receiver
                        .call_method0(py, "driver_disconnect")
                        .and_then(|coro| {
                            let bound_coro = coro.into_bound(py);
                            into_future(bound_coro)
                        })
                }) {
                    let _ = future.await;
                }
            }
            EventContext::DriverReconnect(_) => {
                if let Ok(future) = Python::with_gil(|py| {
                    py_receiver
                        .call_method0(py, "driver_reconnect")
                        .and_then(|coro| {
                            let bound_coro = coro.into_bound(py);
                            into_future(bound_coro)
                        })
                }) {
                    let _ = future.await;
                }
            }
            _ => {}
        }

        None
    }
}
