//! Channels with send and receive operations implemented using `select!`.
//!
//! Such `select!` invocations will often try to optimize the macro invocations by converting them
//! into direct calls to `recv()` or `send()`.

use std::ops::Deref;
use std::time::{Duration, Instant};

use channel;
use channel::{RecvError, RecvTimeoutError, TryRecvError};
use channel::{SendError, SendTimeoutError, TrySendError};

pub struct Sender<T>(pub channel::Sender<T>);

pub struct Receiver<T>(pub channel::Receiver<T>);

impl<T> Clone for Sender<T> {
    fn clone(&self) -> Sender<T> {
        Sender(self.0.clone())
    }
}

impl<T> Clone for Receiver<T> {
    fn clone(&self) -> Receiver<T> {
        Receiver(self.0.clone())
    }
}

impl<T> Deref for Receiver<T> {
    type Target = channel::Receiver<T>;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl<T> Deref for Sender<T> {
    type Target = channel::Sender<T>;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl<T> Sender<T> {
    pub fn try_send(&self, msg: T) -> Result<(), TrySendError<T>> {
        select! {
            send(self.0, msg) -> res => res.map_err(|SendError(m)| TrySendError::Disconnected(m)),
            default => Err(TrySendError::Full(msg)),
        }
    }

    pub fn send(&self, msg: T) -> Result<(), SendError<T>> {
        select! {
            send(self.0, msg) -> res => res
        }
    }

    pub fn send_timeout(&self, msg: T, timeout: Duration) -> Result<(), SendTimeoutError<T>> {
        select! {
            send(self.0, msg) -> res => {
                res.map_err(|SendError(m)| SendTimeoutError::Disconnected(m))
            }
            default(timeout) => Err(SendTimeoutError::Timeout(msg)),
        }
    }
}

impl<T> Receiver<T> {
    pub fn try_recv(&self) -> Result<T, TryRecvError> {
        select! {
            recv(self.0) -> res => res.map_err(|_| TryRecvError::Disconnected),
            default => Err(TryRecvError::Empty),
        }
    }

    pub fn recv(&self) -> Result<T, RecvError> {
        select! {
            recv(self.0) -> res => res,
        }
    }

    pub fn recv_timeout(&self, timeout: Duration) -> Result<T, RecvTimeoutError> {
        select! {
            recv(self.0) -> res => res.map_err(|_| RecvTimeoutError::Disconnected),
            default(timeout) => Err(RecvTimeoutError::Timeout),
        }
    }
}

pub fn bounded<T>(cap: usize) -> (Sender<T>, Receiver<T>) {
    let (s, r) = channel::bounded(cap);
    (Sender(s), Receiver(r))
}

pub fn unbounded<T>() -> (Sender<T>, Receiver<T>) {
    let (s, r) = channel::unbounded();
    (Sender(s), Receiver(r))
}

pub fn after(dur: Duration) -> Receiver<Instant> {
    let r = channel::after(dur);
    Receiver(r)
}

pub fn tick(dur: Duration) -> Receiver<Instant> {
    let r = channel::tick(dur);
    Receiver(r)
}
