
mod sdf;
#[cfg(test)]
mod test;

pub use self::sdf::ShortestDeadlineFirst;

use super::{StatusBit, RunningTask};
use super::push::push_future;
use super::push::PushFutureRecv;

use std::sync::{Mutex, RwLock, Arc};
use std::collections::VecDeque;

use atomicmonitor::AtomMonitor;
use atomicmonitor::atomic::{Atomic, Ordering};

use futures::Future;

/// A channel by which futures becomes available to the thread pool.
pub trait Channel {
    fn assign_bits(&mut self, assigner: &mut BitAssigner) -> Result<(), NotEnoughBits>;

    fn poll(&self) -> Option<RunningTask>;

    /// Wrap self in a reference counted read/write lock, which is still a channel,
    /// and can have shared ownership, allowing it to be used in a scheduler.
    fn into_shared(self) -> Arc<RwLock<Self>> where Self: Sized {
        Arc::new(RwLock::new(self))
    }
}

/// A struct used by pools to dispatch up to 64 present bitfield bits to channels.
pub struct BitAssigner<'a, 'b> {
    monitor: &'a Arc<AtomMonitor<u64>>,
    index: &'b mut usize
}
impl<'a, 'b> BitAssigner<'a, 'b> {
    pub fn new(monitor: &'a Arc<AtomMonitor<u64>>, index: &'b mut usize) -> Self {
        BitAssigner {
            monitor,
            index
        }
    }

    pub fn current_index(&self) -> usize {
        *self.index
    }

    pub fn assign(&mut self, bit: &mut StatusBit) -> Result<(), NotEnoughBits> {
        let curr_index = *self.index;
        if curr_index < 64 {
            *self.index += 1;
            bit.activate(self.monitor.clone(), curr_index).ok().unwrap();
            Ok(())
        } else {
            Err(NotEnoughBits)
        }
    }
}

#[derive(Debug)]
pub struct NotEnoughBits;

/// Trait for channels for which a task can be submitted.
pub trait Exec {
    /// Execute a future on this channel.
    fn exec(&self, future: impl Future<Item=(), Error=()> + Send + 'static) {
        self.submit(RunningTask::new(future));
    }

    /// Execute a future on this channel, and push the result to a push future.
    fn exec_push<I: Send + 'static, E: Send + 'static>(&self,
                                                       future: impl Future<Item=I, Error=E> + Send + 'static)
        -> PushFutureRecv<I, E> {
        let (send, recv) = push_future(future);
        self.submit(RunningTask::new(send));
        recv
    }

    /// Submit a raw running task.
    fn submit(&self, task: RunningTask);
}

/// Trait for channels for which a task can be submitted along side an additional parameter.
pub trait ExecParam {
    type Param;

    /// Execute a future on this channel.
    fn exec(&self, future: impl Future<Item=(), Error=()> + Send + 'static, param: Self::Param) {
        self.submit(RunningTask::new(future), param);
    }

    /// Execute a future on this channel, and push the result to a push future.
    fn exec_push<I: Send + 'static, E: Send + 'static>(&self,
                                                       future: impl Future<Item=I, Error=E> + Send + 'static,
                                                       param: Self::Param)
        -> PushFutureRecv<I, E> {
        let (send, recv) = push_future(future);
        self.submit(RunningTask::new(send), param);
        recv
    }

    /// Submit a raw running task.
    fn submit(&self, task: RunningTask, param: Self::Param);
}

/// Shared channel implementation.
impl<C: Channel> Channel for Arc<RwLock<C>> {
    fn assign_bits(&mut self, assigner: &mut BitAssigner) -> Result<(), NotEnoughBits> {
        let mut guard = self.write().unwrap();
        guard.assign_bits(assigner)
    }

    fn poll(&self) -> Option<RunningTask> {
        let guard = self.read().unwrap();
        guard.poll()
    }
}

/// Shared exec implementation.
impl<E: Exec> Exec for Arc<RwLock<E>> {
    fn submit(&self, task: RunningTask) {
        let guard = self.read().unwrap();
        guard.submit(task);
    }
}

/// Shared exec param implementation.
impl<E: ExecParam> ExecParam for Arc<RwLock<E>> {
    type Param = E::Param;

    fn submit(&self, task: RunningTask, param: E::Param) {
        let guard = self.read().unwrap();
        guard.submit(task, param);
    }
}

/// A simple FIFO channel.
pub struct VecDequeChannel {
    queue: Mutex<VecDeque<RunningTask>>,
    bit: StatusBit
}
impl VecDequeChannel {
    pub fn new() -> Self {
        VecDequeChannel {
            queue: Mutex::new(VecDeque::new()),
            bit: StatusBit::new()
        }
    }
}
impl Exec for VecDequeChannel {
    fn submit(&self, task: RunningTask) {
        let mut guard = self.queue.lock().unwrap();
        guard.push_front(task);
        self.bit.set(true);
    }
}
impl Channel for VecDequeChannel {
    fn assign_bits(&mut self, assigner: &mut BitAssigner) -> Result<(), NotEnoughBits> {
        assigner.assign(&mut self.bit)?;
        self.bit.set(!self.queue.get_mut().unwrap().is_empty());
        Ok(())
    }

    fn poll(&self) -> Option<RunningTask> {
        let mut guard = self.queue.lock().unwrap();
        let future = guard.pop_back();
        self.bit.set(!guard.is_empty());
        future
    }
}

/// A round-robin multi channel wrapper.
pub struct MultiChannel<Inner: Channel> {
    inner: Vec<Inner>,
    index: Atomic<usize>,
}
impl<Inner: Channel> MultiChannel<Inner> {
    pub fn from_vec(inner: Vec<Inner>) -> Self {
        MultiChannel {
            inner,
            index: Atomic::new(0)
        }
    }

    pub fn new(count: usize, factory: impl Fn() -> Inner) -> Self {
        let mut vec = Vec::new();
        for _ in 0..count {
            vec.push(factory());
        }
        Self::from_vec(vec)
    }

    fn index(&self) -> usize {
        self.index.fetch_add(1, Ordering::SeqCst) % self.inner.len()
    }
}
impl<Inner: Channel + Exec> Exec for MultiChannel<Inner> {
    fn submit(&self, task: RunningTask) {
        self.inner[self.index()].submit(task);
    }
}
impl<Inner: Channel + ExecParam> ExecParam for MultiChannel<Inner> {
    type Param = Inner::Param;

    fn submit(&self, task: RunningTask, param: <Self as ExecParam>::Param) {
        self.inner[self.index()].submit(task, param);
    }
}
impl<Inner: Channel> Channel for MultiChannel<Inner> {
    fn assign_bits(&mut self, assigner: &mut BitAssigner) -> Result<(), NotEnoughBits> {
        for inner_channel in &mut self.inner {
            inner_channel.assign_bits(assigner)?;
        }
        Ok(())
    }

    fn poll(&self) -> Option<RunningTask> {
        // Poll from an index as many times as we have inner channels, returning the first one found.
        // (This iterator combination is expected to short circuit).
        (0..self.inner.len())
            .filter_map(|_| self.inner[self.index()].poll())
            .next()
    }
}