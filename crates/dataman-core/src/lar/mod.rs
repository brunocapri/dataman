//! LAR-only APIs, behind the [`LarExtension`](crate::server::LarExtension): live queries
//! (`LISTEN … AS`, `SUBSCRIBE`).
//!
//! [`Connection::lar`] hands out a [`Lar`] only when the extension recognized the server. The
//! pure parts (parsing payloads, applying deltas) are free functions in the submodule, usable
//! and tested without a server.

pub mod live;

use std::sync::Arc;

use crate::conn::Connection;
use crate::server;

/// LAR operations on an open connection.
#[derive(Clone)]
pub struct Lar {
    conn: Arc<Connection>,
}

impl Connection {
    /// The LAR operations, when this server is LAR.
    pub fn lar(self: &Arc<Self>) -> Option<Lar> {
        (self.info.has(server::LAR) && self.caps().notifications).then(|| Lar { conn: self.clone() })
    }
}

impl Lar {
    pub fn connection(&self) -> &Arc<Connection> {
        &self.conn
    }

    /// For tests and tools: LAR operations without the detection (the server must be LAR).
    pub fn new_unchecked(conn: Arc<Connection>) -> Lar {
        Lar { conn }
    }
}
