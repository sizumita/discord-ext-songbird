use async_trait::async_trait;
use pyo3::prelude::*;
use pyo3::types::PyBytes;
use songbird::{
    events::{EventContext, EventHandler},
    model::payload::Speaking,
    Event,
};
use std::sync::Arc;

#[pyclass]
#[derive(Clone)]
pub struct VoiceTick {
    #[pyo3(get)]
    pub speaking: Vec<(u32, VoiceData)>,
    pub silent: std::collections::HashSet<u32>,
}

#[pymethods]
impl VoiceTick {
    #[getter]
    fn silent(&self, py: Python<'_>) -> pyo3::PyResult<pyo3::PyObject> {
        let set = pyo3::types::PySet::new(py, &self.silent)?;
        Ok(set.into())
    }
}

#[pyclass]
#[derive(Clone)]
pub struct VoiceData {
    #[pyo3(get)]
    pub packet: Option<RtpData>,
    pub decoded_voice: Option<Vec<i16>>,
}

#[pymethods]
impl VoiceData {
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

#[pyclass]
#[derive(Clone)]
pub struct RtpData {
    #[pyo3(get)]
    pub sequence: u16,
    #[pyo3(get)]
    pub timestamp: u32,
    pub payload: Vec<u8>,
    pub packet: Vec<u8>,
}

#[pymethods]
impl RtpData {
    #[getter]
    fn payload<'py>(&self, py: Python<'py>) -> Bound<'py, PyBytes> {
        PyBytes::new(py, &self.payload)
    }

    #[getter]
    fn packet<'py>(&self, py: Python<'py>) -> Bound<'py, PyBytes> {
        PyBytes::new(py, &self.packet)
    }
}

#[pyclass(subclass)]
pub struct VoiceReceiver();

#[pymethods]
impl VoiceReceiver {
    #[new]
    fn new() -> Self {
        Self()
    }

    fn voice_tick<'py>(_slf: PyRef<'py, Self>, _tick: PyRef<'py, VoiceTick>) -> PyResult<()> {
        Ok(())
    }

    fn speaking_update(_slf: PyRef<'_, Self>, _ssrc: i32, _speaking: bool) -> PyResult<()> {
        Ok(())
    }

    fn driver_connect(_slf: PyRef<'_, Self>) -> PyResult<()> {
        Ok(())
    }

    fn driver_disconnect(_slf: PyRef<'_, Self>) -> PyResult<()> {
        Ok(())
    }

    fn driver_reconnect(_slf: PyRef<'_, Self>) -> PyResult<()> {
        Ok(())
    }
}

pub struct ReceiverAdapter(Arc<Py<VoiceReceiver>>);

impl ReceiverAdapter {
    pub fn new(receiver: Arc<Py<VoiceReceiver>>) -> Self {
        Self(receiver)
    }
}

#[async_trait]
impl EventHandler for ReceiverAdapter {
    async fn act(&self, ctx: &EventContext<'_>) -> Option<Event> {
        match ctx {
            EventContext::VoiceTick(tick) => {
                let mut speaking_data = Vec::new();
                let silent_data: std::collections::HashSet<u32> =
                    tick.silent.iter().cloned().collect();

                // Convert speaking users data
                for (ssrc, voice_data) in &tick.speaking {
                    let rtp_packet = if let Some(packet) = &voice_data.packet {
                        let rtp_packet = packet.rtp();
                        let payload_start = packet.payload_offset;
                        let payload_end = packet.packet.len() - packet.payload_end_pad;
                        let payload = if payload_start < payload_end {
                            packet.packet[payload_start..payload_end].to_vec()
                        } else {
                            Vec::new()
                        };

                        Some(RtpData {
                            sequence: rtp_packet.get_sequence().into(),
                            timestamp: rtp_packet.get_timestamp().into(),
                            payload,
                            packet: packet.packet.to_vec(),
                        })
                    } else {
                        None
                    };

                    let py_voice_data = VoiceData {
                        packet: rtp_packet,
                        decoded_voice: voice_data.decoded_voice.clone(),
                    };

                    speaking_data.push((*ssrc, py_voice_data));
                }

                let py_voice_tick = VoiceTick {
                    speaking: speaking_data,
                    silent: silent_data,
                };

                Python::with_gil(|py| {
                    let _ = self.0.bind(py).call_method1("voice_tick", (py_voice_tick,));
                });
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

                Python::with_gil(|py| {
                    let _ = self.0.bind(py).call_method1("speaking_update", data);
                });
            }
            EventContext::DriverConnect(_) => {
                Python::with_gil(|py| {
                    let _ = self.0.bind(py).call_method0("driver_connect");
                });
            }
            EventContext::DriverDisconnect(_) => {
                Python::with_gil(|py| {
                    let _ = self.0.bind(py).call_method0("driver_disconnect");
                });
            }
            EventContext::DriverReconnect(_) => {
                Python::with_gil(|py| {
                    let _ = self.0.bind(py).call_method0("driver_reconnect");
                });
            }
            _ => {}
        }

        None
    }
}
