//! A port in front of a real server, standing for what lies between Dataman and a serverless
//! database: the gateway that holds or refuses logins while the database wakes, and a network that
//! ends or silently drops the sockets already open.
#![allow(dead_code)]

use std::io::{Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

use parking_lot::Mutex;

pub enum Door {
    Open,
    /// Each new connection is taken at once and reaches the server only after this long: Aurora
    /// Serverless v2 resuming, Neon's proxy starting a compute.
    Hold(Duration),
    /// The next `left` connections are served by `with` instead (a login refused while the
    /// database wakes), then the door opens.
    Answer { left: usize, with: fn(TcpStream) },
}

struct Pipe {
    client: TcpStream,
    server: TcpStream,
    dead: Arc<AtomicBool>,
}

pub struct Gate {
    pub port: u16,
    door: Arc<Mutex<Door>>,
    pipes: Arc<Mutex<Vec<Pipe>>>,
    dials: Arc<AtomicUsize>,
}

impl Gate {
    pub fn new(target: (String, u16)) -> Gate {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let gate = Gate { port, door: Arc::new(Mutex::new(Door::Open)), pipes: Arc::default(), dials: Arc::default() };
        let (door, pipes, dials) = (gate.door.clone(), gate.pipes.clone(), gate.dials.clone());
        std::thread::spawn(move || {
            for client in listener.incoming().flatten() {
                dials.fetch_add(1, Ordering::AcqRel);
                let hold = {
                    let mut d = door.lock();
                    match &mut *d {
                        Door::Open => Duration::ZERO,
                        Door::Hold(t) => *t,
                        Door::Answer { left, with } => {
                            let with = *with;
                            *left -= 1;
                            if *left == 0 {
                                *d = Door::Open;
                            }
                            std::thread::spawn(move || with(client));
                            continue;
                        }
                    }
                };
                let (target, pipes) = (target.clone(), pipes.clone());
                std::thread::spawn(move || {
                    std::thread::sleep(hold);
                    let Ok(server) = TcpStream::connect(&target) else { return };
                    let dead = Arc::new(AtomicBool::new(false));
                    pump(client.try_clone().unwrap(), server.try_clone().unwrap(), dead.clone());
                    pump(server.try_clone().unwrap(), client.try_clone().unwrap(), dead.clone());
                    pipes.lock().push(Pipe { client, server, dead });
                });
            }
        });
        gate
    }

    pub fn set(&self, door: Door) {
        *self.door.lock() = door;
    }

    /// Connections taken so far, each a login attempt.
    pub fn dials(&self) -> usize {
        self.dials.load(Ordering::Acquire)
    }

    /// The server ends every session (a restart, a database pausing): both ends see it close.
    pub fn close_all(&self) {
        for p in self.pipes.lock().drain(..) {
            let _ = p.client.shutdown(Shutdown::Both);
            let _ = p.server.shutdown(Shutdown::Both);
        }
    }

    /// The way to the server is gone and nobody says so (a laptop that slept, a NAT that forgot
    /// the flow, a frozen host): the sockets stay open, and nothing sent on them arrives.
    pub fn drop_all(&self) {
        for p in self.pipes.lock().drain(..) {
            p.dead.store(true, Ordering::Release);
            // Kept open, as the client's end of a dropped flow is.
            std::mem::forget(p.client);
            let _ = p.server.shutdown(Shutdown::Both);
        }
    }
}

fn pump(mut from: TcpStream, mut to: TcpStream, dead: Arc<AtomicBool>) {
    std::thread::spawn(move || {
        let mut buf = [0u8; 16 * 1024];
        loop {
            let n = match from.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => n,
            };
            if dead.load(Ordering::Acquire) {
                continue;
            }
            if to.write_all(&buf[..n]).is_err() {
                break;
            }
        }
        if !dead.load(Ordering::Acquire) {
            let _ = to.shutdown(Shutdown::Write);
        }
    });
}
