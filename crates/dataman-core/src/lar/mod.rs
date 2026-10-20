pub mod live;

use std::sync::Arc;

use crate::conn::Connection;
use crate::server;

#[derive(Clone)]
pub struct Lar {
    conn: Arc<Connection>,
}

impl Connection {
    pub fn lar(self: &Arc<Self>) -> Option<Lar> {
        (self.info.has(server::LAR) && self.caps().notifications).then(|| Lar { conn: self.clone() })
    }
}

impl Lar {
    pub fn connection(&self) -> &Arc<Connection> {
        &self.conn
    }

}
