//! FIFO game worker pool (port of `ms/sim-server/worker-pool.js`). Games are
//! CPU-bound (expert budget up to 2M nodes), so they run on dedicated OS
//! threads instead of the tokio reactor; results come back over oneshot
//! channels and threads are returned to the idle queue in FIFO order.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::{mpsc, oneshot, Notify};

use crate::worker::{run_game, Task, TaskResult};

enum Msg {
    Run {
        task: Task,
        reply: oneshot::Sender<Result<TaskResult, String>>,
    },
    Shutdown,
}

pub struct WorkerPool {
    tx: Vec<mpsc::Sender<Msg>>,
    idle: Mutex<VecDeque<usize>>,
    notify: Notify,
    shutting_down: AtomicBool,
}

impl WorkerPool {
    pub fn new(size: usize) -> Arc<WorkerPool> {
        let size = size.max(1);
        let mut tx = Vec::with_capacity(size);
        let mut idle = VecDeque::with_capacity(size);
        for i in 0..size {
            let (t, mut rx) = mpsc::channel::<Msg>(64);
            tx.push(t);
            idle.push_back(i);
            std::thread::Builder::new()
                .name(format!("game-worker-{}", i))
                .spawn(move || {
                    while let Some(msg) = rx.blocking_recv() {
                        match msg {
                            Msg::Run { task, reply } => {
                                let res = run_game(&task);
                                let _ = reply.send(res);
                            }
                            Msg::Shutdown => break,
                        }
                    }
                })
                .expect("spawn game worker");
        }
        Arc::new(WorkerPool {
            tx,
            idle: Mutex::new(idle),
            notify: Notify::new(),
            shutting_down: AtomicBool::new(false),
        })
    }

    pub async fn submit(self: &Arc<Self>, task: Task) -> Result<TaskResult, String> {
        let id = loop {
            {
                let mut idle = self.idle.lock().unwrap();
                if let Some(id) = idle.pop_back() {
                    break id;
                }
            }
            if self.shutting_down.load(Ordering::SeqCst) {
                return Err("worker pool shutting down".into());
            }
            self.notify.notified().await;
        };
        let (reply_tx, reply_rx) = oneshot::channel();
        let msg = Msg::Run {
            task,
            reply: reply_tx,
        };
        if self.tx[id].send(msg).await.is_err() {
            self.idle.lock().unwrap().push_back(id);
            self.notify.notify_one();
            return Err("worker task send failed".into());
        }
        let result = reply_rx.await.map_err(|_| "worker task failed".to_string())?;
        self.idle.lock().unwrap().push_back(id);
        self.notify.notify_one();
        result
    }

    pub fn shutdown(&self) {
        self.shutting_down.store(true, Ordering::SeqCst);
        for t in &self.tx {
            let _ = t.try_send(Msg::Shutdown);
        }
    }
}
