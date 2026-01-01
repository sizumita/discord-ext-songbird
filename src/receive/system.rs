use songbird::model::id::UserId;
use songbird::model::payload::Speaking;

#[derive(Debug, Clone)]
pub enum SystemEvent {
    SpeakingStateUpdate(Speaking),
    ClientDisconnect(UserId),
}
