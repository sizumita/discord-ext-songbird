use songbird::model::id::UserId;
use std::collections::HashMap;

pub struct SsrcManager {
    to_ssrc: HashMap<UserId, u32>,
    to_user: HashMap<u32, UserId>,
}

impl SsrcManager {
    pub fn new() -> Self {
        Self {
            to_ssrc: HashMap::new(),
            to_user: HashMap::new(),
        }
    }

    pub fn insert(&mut self, user: UserId, ssrc: u32) {
        self.to_ssrc.insert(user, ssrc);
        self.to_user.insert(ssrc, user);
    }

    pub fn remove_by_user(&mut self, user: &UserId) {
        if let Some(ssrc) = self.to_ssrc.remove(user) {
            self.to_user.remove(&ssrc);
        }
    }

    pub fn get_ssrc(&self, user: &UserId) -> Option<&u32> {
        self.to_ssrc.get(user)
    }

    pub fn get_user(&self, ssrc: &u32) -> Option<&UserId> {
        self.to_user.get(ssrc)
    }
}
