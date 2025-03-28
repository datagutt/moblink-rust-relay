use std::sync::{Arc, Weak};

use log::info;
use tokio::sync::Mutex;

pub struct Streamer {
    me: Weak<Mutex<Self>>,
    password: String,
}

impl Streamer {
    pub fn new(password: String) -> Arc<Mutex<Self>> {
        Arc::new_cyclic(|me| {
            Mutex::new(Self {
                me: me.clone(),
                password,
            })
        })
    }

    pub async fn start(&mut self) {
        info!("Start with password {}", self.password);
    }

    pub async fn stop(&mut self) {}
}
