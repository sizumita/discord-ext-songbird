use crate::receive::system::SystemEvent;
use crate::receive::tick::VoiceTick;
use async_trait::async_trait;
use songbird::{Event, EventContext, EventHandler};
use std::sync::Arc;
use tokio::sync::{mpsc, watch};

pub struct BufferWrapper(pub Arc<dyn EventHandler + Sync>);

#[async_trait]
impl EventHandler for BufferWrapper {
    async fn act(&self, ctx: &EventContext<'_>) -> Option<Event> {
        self.0.act(ctx).await
    }
}

pub struct DefaultBuffer {
    pub voice_tx: watch::Sender<VoiceTick>,
    pub system_tx: mpsc::Sender<SystemEvent>,
}

impl DefaultBuffer {
    pub fn new(voice_tx: watch::Sender<VoiceTick>, system_tx: mpsc::Sender<SystemEvent>) -> Self {
        Self {
            voice_tx,
            system_tx,
        }
    }
}

#[async_trait]
impl EventHandler for DefaultBuffer {
    async fn act(&self, ctx: &EventContext<'_>) -> Option<Event> {
        match ctx {
            EventContext::VoiceTick(tick) => {
                self.voice_tx.send(VoiceTick::from_data(tick)).unwrap();
            }
            EventContext::SpeakingStateUpdate(speaking) => {
                self.system_tx
                    .send(SystemEvent::SpeakingStateUpdate(speaking.clone()))
                    .await
                    .unwrap();
            }
            EventContext::ClientDisconnect(disconnect) => {
                self.system_tx
                    .send(SystemEvent::ClientDisconnect(disconnect.user_id.clone()))
                    .await
                    .unwrap();
            }
            _ => {}
        }
        None
    }
}
