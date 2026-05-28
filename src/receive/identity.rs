use async_trait::async_trait;
use dashmap::DashMap;
use songbird::{Event, EventContext, EventHandler};
use std::sync::{Arc, OnceLock};

#[derive(Debug, Default)]
pub struct VoiceIdentityMap {
    ssrc_to_user: DashMap<u32, u64>,
}

#[derive(Debug, Default)]
pub struct VoiceIdentityBinding {
    map: OnceLock<Arc<VoiceIdentityMap>>,
}

pub trait VoiceIdentityResolver {
    fn user_id_for_ssrc(&self, ssrc: u32) -> Option<u64>;
}

pub struct VoiceIdentityTracker {
    map: Arc<VoiceIdentityMap>,
}

impl VoiceIdentityMap {
    pub fn insert(&self, ssrc: u32, user_id: u64) {
        self.ssrc_to_user.insert(ssrc, user_id);
    }

    pub fn remove_user(&self, user_id: u64) {
        self.ssrc_to_user.retain(|_, mapped| *mapped != user_id);
    }

    pub fn clear(&self) {
        self.ssrc_to_user.clear();
    }
}

impl VoiceIdentityResolver for VoiceIdentityMap {
    fn user_id_for_ssrc(&self, ssrc: u32) -> Option<u64> {
        self.ssrc_to_user.get(&ssrc).map(|user_id| *user_id)
    }
}

impl VoiceIdentityBinding {
    pub fn bind(&self, map: Arc<VoiceIdentityMap>) -> Result<(), VoiceIdentityBindError> {
        if let Some(existing) = self.map.get() {
            if Arc::ptr_eq(existing, &map) {
                Ok(())
            } else {
                Err(VoiceIdentityBindError::DifferentConnection)
            }
        } else {
            self.map
                .set(map)
                .map_err(|_| VoiceIdentityBindError::DifferentConnection)
        }
    }
}

impl VoiceIdentityResolver for VoiceIdentityBinding {
    fn user_id_for_ssrc(&self, ssrc: u32) -> Option<u64> {
        self.map.get()?.user_id_for_ssrc(ssrc)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VoiceIdentityBindError {
    DifferentConnection,
}

impl VoiceIdentityTracker {
    pub fn new(map: Arc<VoiceIdentityMap>) -> Self {
        Self { map }
    }
}

#[async_trait]
impl EventHandler for VoiceIdentityTracker {
    async fn act(&self, ctx: &EventContext<'_>) -> Option<Event> {
        match ctx {
            EventContext::SpeakingStateUpdate(speaking) => {
                if let Some(user_id) = speaking.user_id {
                    self.map.insert(speaking.ssrc, user_id.0);
                }
            }
            EventContext::ClientDisconnect(disconnect) => {
                self.map.remove_user(disconnect.user_id.0);
            }
            _ => {}
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use songbird::model::SpeakingState;
    use songbird::model::payload::{ClientDisconnect, Speaking};
    use songbird::{EventContext, model::id::UserId};

    #[tokio::test]
    async fn tracker_records_speaking_and_disconnect_events() {
        let map = Arc::new(VoiceIdentityMap::default());
        let tracker = VoiceIdentityTracker::new(map.clone());

        tracker
            .act(&EventContext::SpeakingStateUpdate(Speaking {
                delay: None,
                speaking: SpeakingState::MICROPHONE,
                ssrc: 42,
                user_id: Some(UserId(7)),
            }))
            .await;
        assert_eq!(map.user_id_for_ssrc(42), Some(7));

        tracker
            .act(&EventContext::ClientDisconnect(ClientDisconnect {
                user_id: UserId(7),
            }))
            .await;
        assert_eq!(map.user_id_for_ssrc(42), None);
    }

    #[test]
    fn binding_rejects_different_connection_maps() {
        let binding = VoiceIdentityBinding::default();
        let first = Arc::new(VoiceIdentityMap::default());
        let second = Arc::new(VoiceIdentityMap::default());

        assert_eq!(binding.bind(first.clone()), Ok(()));
        assert_eq!(binding.bind(first), Ok(()));
        assert_eq!(
            binding.bind(second),
            Err(VoiceIdentityBindError::DifferentConnection)
        );
    }
}
