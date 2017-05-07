// Copyright 2017 PingCAP, Inc.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// See the License for the specific language governing permissions and
// limitations under the License.


// TODO: remove following line once all changes are merged into master
#![allow(dead_code)]

mod promise;
mod lock;

use std::fmt::{self, Debug, Formatter};
use std::sync::Arc;

use futures::{Async, Future, Poll};
use futures::task::{self, Task};

use call::BatchContext;
use cq::CompletionQueue;
use error::{Error, Result};
use self::lock::SpinLock;
use self::promise::{Batch as BatchPromise, Shutdown as ShutdownPromise};

pub use self::promise::BatchType;

/// A handle that is used to notify future that the task finishes.
pub struct NotifyHandle<T> {
    result: Option<Result<T>>,
    park: Option<Task>,
    stale: bool,
}

impl<T> NotifyHandle<T> {
    fn new() -> NotifyHandle<T> {
        NotifyHandle {
            result: None,
            park: None,
            stale: false,
        }
    }

    /// Set the result and notify future if necessary.
    fn set_result(&mut self, res: Result<T>) {
        self.result = Some(res);

        if let Some(ref t) = self.park {
            t.unpark();
        }
    }
}

type Inner<T> = SpinLock<NotifyHandle<T>>;

fn new_inner<T>() -> Arc<Inner<T>> {
    Arc::new(SpinLock::new(NotifyHandle::new()))
}

/// A future object for task that is scheduled to `CompletionQueue`.
pub struct CqFuture<T> {
    inner: Arc<Inner<T>>,
}

impl<T> CqFuture<T> {
    fn new(inner: Arc<Inner<T>>) -> CqFuture<T> {
        CqFuture { inner: inner }
    }
}

impl<T> Future for CqFuture<T> {
    type Item = T;
    type Error = Error;

    fn poll(&mut self) -> Poll<T, Error> {
        let mut guard = self.inner.lock();
        if guard.stale {
            panic!("Resolved future is not supposed to be polled again.");
        }

        if let Some(res) = guard.result.take() {
            guard.stale = true;
            let r = try!(res);
            return Ok(Async::Ready(r));
        }

        // So the task has not been finished yet, add notification hook.
        if guard.park.is_none() {
            guard.park = Some(task::park());
        }

        Ok(Async::NotReady)
    }
}

pub type BatchMessage = Option<Vec<u8>>;
/// Future object for batch jobs.
pub type BatchFuture = CqFuture<BatchMessage>;

/// A result holder for asynchronous execution.
// This enum is going to be passed to FFI, so don't use trait or generic here.
pub enum Promise {
    Batch(BatchPromise),
    Shutdown(ShutdownPromise),
}

impl Promise {
    /// Generate a future/promise pair for batch jobs.
    pub fn batch_pair(ty: BatchType) -> (BatchFuture, Promise) {
        let inner = new_inner();
        let batch = BatchPromise::new(ty, inner.clone());
        (CqFuture::new(inner), Promise::Batch(batch))
    }

    /// Generate a future/promise pair for shutdown call.
    pub fn shutdown_pair() -> (CqFuture<()>, Promise) {
        let inner = new_inner();
        let shutdown = ShutdownPromise::new(inner.clone());
        (CqFuture::new(inner), Promise::Shutdown(shutdown))
    }

    /// Get the batch context from result holder.
    pub fn batch_ctx(&self) -> Option<&BatchContext> {
        match *self {
            Promise::Batch(ref prom) => Some(prom.context()),
            _ => None,
        }
    }

    /// Resolve the promise with given status.
    pub fn resolve(self, _: &CompletionQueue, success: bool) {
        match self {
            Promise::Batch(prom) => prom.resolve(success),
            Promise::Shutdown(prom) => prom.resolve(success),
        }
    }
}

impl Debug for Promise {
    fn fmt(&self, f: &mut Formatter) -> fmt::Result {
        match *self {
            Promise::Batch(_) => write!(f, "Context::Batch(..)"),
            Promise::Shutdown(_) => write!(f, "Context::Shutdown"),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::thread;
    use std::sync::*;
    use std::sync::mpsc::*;

    use super::*;
    use env::Environment;

    #[test]
    fn test_resolve() {
        let env = Environment::new(1);

        let (cq_f1, prom1) = Promise::shutdown_pair();
        let (cq_f2, prom2) = Promise::shutdown_pair();
        let (tx, rx) = mpsc::channel();

        let handler = thread::spawn(move || {
                                        tx.send(cq_f1.wait()).unwrap();
                                        tx.send(cq_f2.wait()).unwrap();
                                    });

        assert_eq!(rx.try_recv().unwrap_err(), TryRecvError::Empty);
        prom1.resolve(&env.pick_cq(), true);
        assert!(rx.recv().unwrap().is_ok());

        assert_eq!(rx.try_recv().unwrap_err(), TryRecvError::Empty);
        prom2.resolve(&env.pick_cq(), false);
        match rx.recv() {
            Ok(Err(Error::ShutdownFailed)) => {}
            res => panic!("expect shutdown failed, but got {:?}", res),
        }

        handler.join().unwrap();
    }
}