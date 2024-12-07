use std::sync::Arc;
use async_trait::async_trait;
use discortp::Packet;
use pyo3::prelude::*;
use pyo3::IntoPyObjectExt;
use pyo3_async_runtimes::tokio::future_into_py;
use songbird::{Event, EventContext, EventHandler};
use tracing::event;
use crate::sink::AudioSink;
use tokio::sync::mpsc;


pub struct VoiceEventReceiver {

}


impl VoiceEventReceiver {
    pub fn new() -> Self {
        Self {}
    }

    pub fn add_sink(&mut self) {
    }

    pub fn act(&self, ctx: &EventContext<'_>) {
        match ctx {
            EventContext::Track(_) => {}
            EventContext::SpeakingStateUpdate(_) => {}
            EventContext::VoiceTick(tick) => {
                let k = tick.silent.get(&1);
            }
            EventContext::RtpPacket(_) => {}
            EventContext::RtcpPacket(_) => {}
            EventContext::ClientDisconnect(_) => {}
            EventContext::DriverConnect(_) => {}
            EventContext::DriverReconnect(_) => {}
            EventContext::DriverDisconnect(_) => {}
            _ => {}
        }
    }
}

pub struct VoiceEventBridge {
    master: Arc<VoiceEventReceiver>,
}

impl VoiceEventBridge {
    pub fn new(master: Arc<VoiceEventReceiver>) -> Self {
        Self {
            master,
        }
    }
}


#[async_trait]
impl EventHandler for VoiceEventBridge {
    async fn act(&self, ctx: &EventContext<'_>) -> Option<Event> {
        self.master.act(ctx).await;
        None
    }
}
