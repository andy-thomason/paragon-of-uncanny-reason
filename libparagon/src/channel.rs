//! A small unbounded channel that works with any async runtime.
//!
//! The simulation runs on its own thread and sends events; the caller receives
//! them with an async [`Receiver::recv`] or a blocking [`Receiver::recv_blocking`].
//! Dropping the receiver closes the channel, which tells the sender to stop.

use std::collections::VecDeque;
use std::future::poll_fn;
use std::sync::{Arc, Condvar, Mutex};
use std::task::{Poll, Waker};

struct State<T> {
    queue: VecDeque<T>,
    waker: Option<Waker>,
    senders: usize,
    receiver_alive: bool,
}

struct Shared<T> {
    state: Mutex<State<T>>,
    ready: Condvar,
}

pub(crate) fn channel<T>() -> (Sender<T>, Receiver<T>) {
    let shared = Arc::new(Shared {
        state: Mutex::new(State {
            queue: VecDeque::new(),
            waker: None,
            senders: 1,
            receiver_alive: true,
        }),
        ready: Condvar::new(),
    });
    (Sender(shared.clone()), Receiver(shared))
}

/// The sending half. Cloning adds another sender.
pub(crate) struct Sender<T>(Arc<Shared<T>>);

/// The receiver has been dropped.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Closed;

impl<T> Sender<T> {
    pub(crate) fn send(&self, value: T) -> Result<(), Closed> {
        let mut st = self.0.state.lock().unwrap();
        if !st.receiver_alive {
            return Err(Closed);
        }
        st.queue.push_back(value);
        let waker = st.waker.take();
        drop(st);
        self.0.ready.notify_one();
        if let Some(w) = waker {
            w.wake();
        }
        Ok(())
    }

    /// True once the receiver has been dropped.
    pub(crate) fn is_closed(&self) -> bool {
        !self.0.state.lock().unwrap().receiver_alive
    }
}

impl<T> Clone for Sender<T> {
    fn clone(&self) -> Self {
        self.0.state.lock().unwrap().senders += 1;
        Sender(self.0.clone())
    }
}

impl<T> Drop for Sender<T> {
    fn drop(&mut self) {
        let mut st = self.0.state.lock().unwrap();
        st.senders -= 1;
        let waker = if st.senders == 0 {
            st.waker.take()
        } else {
            None
        };
        drop(st);
        self.0.ready.notify_all();
        if let Some(w) = waker {
            w.wake();
        }
    }
}

/// The receiving half.
pub(crate) struct Receiver<T>(Arc<Shared<T>>);

impl<T> Receiver<T> {
    /// The next value, or `None` once every sender is gone and the queue is empty.
    pub(crate) async fn recv(&mut self) -> Option<T> {
        poll_fn(|cx| {
            let mut st = self.0.state.lock().unwrap();
            if let Some(v) = st.queue.pop_front() {
                Poll::Ready(Some(v))
            } else if st.senders == 0 {
                Poll::Ready(None)
            } else {
                st.waker = Some(cx.waker().clone());
                Poll::Pending
            }
        })
        .await
    }

    /// Like [`recv`](Self::recv), but blocks the current thread.
    pub(crate) fn recv_blocking(&mut self) -> Option<T> {
        let mut st = self.0.state.lock().unwrap();
        loop {
            if let Some(v) = st.queue.pop_front() {
                return Some(v);
            }
            if st.senders == 0 {
                return None;
            }
            st = self.0.ready.wait(st).unwrap();
        }
    }
}

impl<T> Drop for Receiver<T> {
    fn drop(&mut self) {
        let mut st = self.0.state.lock().unwrap();
        st.receiver_alive = false;
        st.queue.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::block_on;

    #[test]
    fn values_arrive_in_order_then_none() {
        let (tx, mut rx) = channel();
        let t = std::thread::spawn(move || {
            for i in 0..1000 {
                tx.send(i).unwrap();
            }
        });
        let got: Vec<i32> = std::iter::from_fn(|| block_on(rx.recv())).collect();
        t.join().unwrap();
        assert_eq!(got, (0..1000).collect::<Vec<_>>());
    }

    #[test]
    fn blocking_receive() {
        let (tx, mut rx) = channel();
        let t = std::thread::spawn(move || {
            tx.send("a").unwrap();
            tx.send("b").unwrap();
        });
        assert_eq!(rx.recv_blocking(), Some("a"));
        assert_eq!(rx.recv_blocking(), Some("b"));
        assert_eq!(rx.recv_blocking(), None);
        t.join().unwrap();
    }

    #[test]
    fn dropping_receiver_closes_sender() {
        let (tx, rx) = channel::<u8>();
        assert!(!tx.is_closed());
        drop(rx);
        assert!(tx.is_closed());
        assert_eq!(tx.send(1), Err(Closed));
    }

    #[test]
    fn channel_ends_when_all_clones_drop() {
        let (tx, mut rx) = channel::<u8>();
        let tx2 = tx.clone();
        drop(tx);
        tx2.send(7).unwrap();
        drop(tx2);
        assert_eq!(rx.recv_blocking(), Some(7));
        assert_eq!(rx.recv_blocking(), None);
    }
}
