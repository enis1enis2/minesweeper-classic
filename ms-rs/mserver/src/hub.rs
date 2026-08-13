//! Connection registry, per-client request queues and the FIFO admission gate
//! (ports of `ms/sim-server/hub.js` ClientHub / RequestWorkers / AdmissionGate).

use crate::config::NONCE_TTL;
use crate::crypto::timing_safe_eq;
use crate::db::{Database, now_sec};
use futures::FutureExt;
use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use tokio::io::{AsyncWriteExt, WriteHalf};
use tokio::net::TcpStream;
use tokio::sync::{mpsc, oneshot, Mutex, Notify};

pub struct Client {
    pub writer: Arc<Mutex<WriteHalf<TcpStream>>>,
    pub last: i64,
    pub seeds: u64,
    pub outcomes: u64,
    pub user: Option<String>,
    pub nonce: Option<String>,
    pub nonce_ts: i64,
    pub fails: i64,
    pub authed: bool,
    pub dead: bool,
}

pub struct ClientSnapshot {
    pub user: Option<String>,
    pub nonce: Option<String>,
}

pub struct ClientHub {
    db: Arc<Database>,
    clients: Mutex<HashMap<String, Client>>,
}

impl ClientHub {
    pub fn new(db: Arc<Database>) -> Arc<ClientHub> {
        Arc::new(ClientHub {
            db,
            clients: Mutex::new(HashMap::new()),
        })
    }

    pub async fn add(&self, addr: String, writer: Arc<Mutex<WriteHalf<TcpStream>>>) {
        let now = now_sec();
        let cl = Client {
            writer,
            last: now,
            seeds: 0,
            outcomes: 0,
            user: None,
            nonce: None,
            nonce_ts: 0,
            fails: 0,
            authed: false,
            dead: false,
        };
        self.clients.lock().await.insert(addr.clone(), cl);
        if let Err(e) = self.db.upsert_client(&addr, now, true) {
            eprintln!("  hub: upsert_client {} failed: {}", addr, e);
        }
    }

    pub async fn is_authed(&self, addr: &str) -> bool {
        self.clients.lock().await.get(addr).map(|c| c.authed).unwrap_or(false)
    }

    pub async fn get(&self, addr: &str) -> Option<ClientSnapshot> {
        let m = self.clients.lock().await;
        m.get(addr).map(|cl| ClientSnapshot {
            user: cl.user.clone(),
            nonce: cl.nonce.clone(),
        })
    }

    pub async fn auth_begin(&self, addr: &str, user: &str) -> Option<String> {
        let mut m = self.clients.lock().await;
        let cl = m.get_mut(addr)?;
        cl.user = Some(user.to_string());
        let nonce = random_hex(16);
        cl.nonce = Some(nonce.clone());
        cl.nonce_ts = now_sec();
        Some(nonce)
    }

    /// `(ok, fails)` — mirrors auth_resolve.
    pub async fn auth_resolve(&self, addr: &str, digest_hex: &str, expected_hex: &str) -> (bool, i64) {
        let mut m = self.clients.lock().await;
        let cl = match m.get_mut(addr) {
            Some(cl) => cl,
            None => return (false, 0),
        };
        let nonce_ts = cl.nonce_ts;
        if now_sec() - nonce_ts > NONCE_TTL {
            cl.fails += 1;
            return (false, cl.fails);
        }
        let ok = timing_safe_eq(&digest_hex.to_lowercase(), expected_hex);
        cl.nonce = None;
        if ok {
            cl.authed = true;
            cl.fails = 0;
        } else {
            cl.fails += 1;
        }
        (ok, cl.fails)
    }

    pub async fn remove(&self, addr: &str) {
        let existed = self.clients.lock().await.remove(addr).is_some();
        if existed {
            if let Err(e) = self.db.upsert_client(addr, now_sec(), false) {
                eprintln!("  hub: upsert_client {} failed: {}", addr, e);
            }
        }
    }

    pub async fn count(&self) -> usize {
        self.clients.lock().await.len()
    }

    pub async fn send_to(&self, addr: &str, line: &str) -> bool {
        let writer = {
            let m = self.clients.lock().await;
            match m.get(addr) {
                Some(cl) if !cl.dead => cl.writer.clone(),
                _ => return false,
            }
        };
        let res = {
            let mut w = writer.lock().await;
            w.write_all(format!("{}\n", line).as_bytes()).await
        };
        match res {
            Ok(_) => {
                let (seeds, outcomes) = {
                    let mut m = self.clients.lock().await;
                    // The client can disconnect (and be removed) between the
                    // writer clone above and this lock; that is not an error.
                    let Some(cl) = m.get_mut(addr) else {
                        return true;
                    };
                    cl.last = now_sec();
                    if line.starts_with("seed ") {
                        cl.seeds += 1;
                    } else if line.starts_with("outcome ") {
                        cl.outcomes += 1;
                    }
                    (cl.seeds, cl.outcomes)
                };
                if let Err(e) = self.db.client_touch(addr, seeds, outcomes) {
                    eprintln!("  hub: client_touch {} failed: {}", addr, e);
                }
                true
            }
            Err(_) => {
                self.mark_dead(addr).await;
                self.remove(addr).await;
                false
            }
        }
    }

    pub async fn mark_dead(&self, addr: &str) {
        let mut m = self.clients.lock().await;
        if let Some(cl) = m.get_mut(addr) {
            cl.dead = true;
        }
    }

    pub async fn broadcast(&self, line: &str) -> usize {
        let targets: Vec<(String, Arc<Mutex<WriteHalf<TcpStream>>>)> = {
            let m = self.clients.lock().await;
            m.iter()
                .filter(|(_, cl)| !cl.dead)
                .map(|(a, cl)| (a.clone(), cl.writer.clone()))
                .collect()
        };
        let mut sent = 0;
        let mut dead: Vec<String> = Vec::new();
        let mut touched: Vec<(String, u64, u64)> = Vec::new();
        for (addr, writer) in targets {
            let res = {
                let mut w = writer.lock().await;
                w.write_all(format!("{}\n", line).as_bytes()).await
            };
            match res {
                Ok(_) => {
                    let (seeds, outcomes) = {
                        let mut m = self.clients.lock().await;
                        // Client may have disconnected since `targets` was
                        // snapshotted; skip bookkeeping rather than panic.
                        let Some(cl) = m.get_mut(&addr) else {
                            continue;
                        };
                        cl.last = now_sec();
                        if line.starts_with("seed ") {
                            cl.seeds += 1;
                        } else if line.starts_with("outcome ") {
                            cl.outcomes += 1;
                        }
                        (cl.seeds, cl.outcomes)
                    };
                    touched.push((addr.clone(), seeds, outcomes));
                    sent += 1;
                }
                Err(_) => dead.push(addr),
            }
        }
        for a in &dead {
            self.mark_dead(a).await;
            self.remove(a).await;
        }
        if let Err(e) = self.db.client_touch_many(&touched) {
            eprintln!("  hub: client_touch_many failed: {}", e);
        }
        sent
    }
}

fn random_hex(bytes: usize) -> String {
    use rand::RngCore;
    let mut buf = vec![0u8; bytes];
    rand::rngs::OsRng.fill_bytes(&mut buf);
    buf.iter().map(|b| format!("{:02x}", b)).collect()
}

/// Per-address FIFO request queue. Each address gets its own drain task that
/// runs `handleRequest` sequentially; the queue is dropped when the client
/// disconnects (`drop`), exactly like hub.js RequestWorkers.
pub struct RequestWorkers {
    states: Arc<Mutex<HashMap<String, mpsc::UnboundedSender<String>>>>,
}

impl RequestWorkers {
    pub fn new() -> Arc<RequestWorkers> {
        Arc::new(RequestWorkers {
            states: Arc::new(Mutex::new(HashMap::new())),
        })
    }

    pub async fn enqueue<F, Fut>(&self, addr: String, line: String, handler: F)
    where
        F: Fn(String, String) -> Fut + Send + Sync + 'static,
        Fut: std::future::Future<Output = ()> + Send + 'static,
    {
        let states = Arc::clone(&self.states);
        let sender = {
            let mut m = self.states.lock().await;
            if let Some(tx) = m.get(&addr) {
                tx.clone()
            } else {
                let (tx, mut rx) = mpsc::unbounded_channel::<String>();
                m.insert(addr.clone(), tx.clone());
                let addr_for_task = addr.clone();
                tokio::spawn(async move {
                    while let Some(line) = rx.recv().await {
                        // One panicking request must not kill this client's
                        // FIFO queue (and leak the channel entry in `states`).
                        let result = std::panic::AssertUnwindSafe(handler(addr_for_task.clone(), line))
                            .catch_unwind()
                            .await;
                        if let Err(panic) = result {
                            eprintln!(
                                "conn {}: panic in request handler: {:?}",
                                addr_for_task, panic
                            );
                        }
                    }
                    // Channel closed (drop() called) -> reap the entry.
                    let mut m = states.lock().await;
                    if let Some(tx) = m.get(&addr_for_task) {
                        if tx.is_closed() {
                            m.remove(&addr_for_task);
                        }
                    }
                });
                tx
            }
        };
        let _ = sender.send(line);
    }

    pub async fn drop_addr(&self, addr: &str) {
        let mut m = self.states.lock().await;
        if let Some(tx) = m.remove(addr) {
            drop(tx);
        }
    }
}

/// FIFO concurrency limiter, faithful to hub.js AdmissionGate: tickets are
/// handed out strictly in order, so heavy games from one client can not jump
/// the queue.
pub struct AdmissionGate {
    state: Mutex<GateState>,
    notify: Notify,
}

struct GateState {
    free: usize,
    head: u64,
    next_ticket: u64,
    waiters: VecDeque<(u64, oneshot::Sender<()>)>,
}

impl AdmissionGate {
    pub fn new(max_concurrent: usize) -> Arc<AdmissionGate> {
        let max = max_concurrent.max(1);
        Arc::new(AdmissionGate {
            state: Mutex::new(GateState {
                free: max,
                head: 0,
                next_ticket: 0,
                waiters: VecDeque::new(),
            }),
            notify: Notify::new(),
        })
    }

    pub async fn acquire(self: &Arc<Self>) {
        let ticket = {
            let mut s = self.state.lock().await;
            let t = s.next_ticket;
            s.next_ticket += 1;
            t
        };
        loop {
            let mut s = self.state.lock().await;
            if s.free > 0 && ticket == s.head {
                s.free -= 1;
                s.head += 1;
                return;
            }
            let (tx, rx) = oneshot::channel();
            s.waiters.push_back((ticket, tx));
            drop(s);
            let _ = rx.await;
            return;
        }
    }

    pub async fn release(self: &Arc<Self>) {
        let mut s = self.state.lock().await;
        s.free += 1;
        while s.free > 0 && !s.waiters.is_empty() && s.waiters.front().unwrap().0 == s.head {
            let (_, tx) = s.waiters.pop_front().unwrap();
            s.free -= 1;
            s.head += 1;
            let _ = tx.send(());
        }
        drop(s);
        self.notify.notify_one();
    }
}
