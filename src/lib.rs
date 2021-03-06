
///
/// The Phoenix Kahlo design for an intelligently prioritized threadpool system, particularly
/// for video games, based on the atomic monitor, with future awareness.
///

#[macro_use]
extern crate log;
extern crate atomicmonitor;
extern crate futures;
extern crate atom;
extern crate monitor;
extern crate time;
extern crate smallqueue;
extern crate atomic;
extern crate stopwatch;
extern crate through;

pub mod channel;
pub mod pool;
pub mod prelude;
pub mod run;
pub mod scoped;
pub mod timescheduler;
#[cfg(test)]
pub mod test;
pub mod push;

use channel::Channel;

use std::sync::Arc;
use std::fmt::{Debug, Formatter};
use std::fmt;

use atomicmonitor::AtomMonitor;
use atomicmonitor::atomic::{Atomic, Ordering};

use futures::Future;
use futures::executor::{spawn, Spawn};

use time::Duration;

/// The form a future exists in while it is being executed
pub struct RunningTask {
    pub spawn: Spawn<Box<dyn Future<Item=(), Error=()> + Send + 'static>>,
    pub close_counted: Atomic<bool>,
}
impl RunningTask {
    fn new(future: impl Future<Item=(), Error=()> + Send + 'static) -> Self {
        let future: Box<dyn Future<Item=(), Error=()> + Send + 'static> = Box::new(future);

        RunningTask {
            spawn: spawn(future),
            close_counted: Atomic::new(false),
        }

    }
}
impl Debug for RunningTask {
    fn fmt(&self, f: &mut Formatter) -> Result<(), fmt::Error> {
        f.write_str("RunningTask")
    }
}

/// A way for a channel to access the status bit of the pool, which it must maintain
pub struct StatusBit {
    inner: Option<StatusBitInner>
}

struct StatusBitInner {
    monitor: Arc<AtomMonitor<u64>>,
    mask: u64
}

pub struct StatusBitIndexTooBig(pub usize);

impl StatusBit {
    pub fn new() -> Self {
        StatusBit {
            inner: None
        }
    }

    pub fn activate(&mut self, monitor: Arc<AtomMonitor<u64>>, index: usize) -> Result<(), StatusBitIndexTooBig> {
        if index < 64 {
            self.inner = Some(StatusBitInner {
                monitor,
                mask: 0x1 << index as u64
            });
            Ok(())
        } else {
            Err(StatusBitIndexTooBig(index))
        }
    }

    pub fn set(&self, value: bool) {
        if let Some(ref inner) = self.inner {
            if value {
                inner.monitor.mutate(|field| {
                    field.fetch_or(inner.mask, Ordering::SeqCst)
                })
            } else {
                inner.monitor.mutate(|field| {
                    field.fetch_and(!inner.mask, Ordering::SeqCst)
                })
            };
        }
    }
}

/// A statically dispatched type which serves to configure the behavior of a threadpool.
pub trait PoolBehavior: Sized + Send + Sync + 'static {
    /// The identifier for a channel for this pool.
    type ChannelKey: Copy + Clone + Send + Sync + 'static;

    fn config(&mut self) -> PoolConfig<Self>;

    fn touch_channel<O>(&self, key: Self::ChannelKey, toucher: impl ChannelToucher<O>) -> O;

    fn touch_channel_mut<O>(&mut self, key: Self::ChannelKey, toucher: impl ChannelToucherMut<O>) -> O;

    fn followup(&self, from: Self::ChannelKey, task: RunningTask);
}

/// A system for the pool to index the behavior's channels with static dispatch.
pub trait ChannelToucher<O>: Sized {
    fn touch(&mut self, channel: & impl Channel) -> O;
}

/// A system for the pool to index the behavior's channels with static dispatch. Mutably.
pub trait ChannelToucherMut<O>: Sized {
    fn touch_mut(&mut self, channel: &mut impl Channel) -> O;
}

/// Config data produced by the pool behavior to configure the pool.
pub struct PoolConfig<Behavior: PoolBehavior> {
    /// Worker thread count
    pub threads: u32,
    /// Schedule algorithm
    pub schedule: ScheduleAlgorithm,
    /// Priority levels, from highest to lowest, containing vecs of all the channels in that level
    pub levels: Vec<Vec<ChannelParams<Behavior>>>,
}

/// Different schedules by which the thread pool can be configured to select between levels.
#[derive(Clone, Debug)]
pub enum ScheduleAlgorithm {
    /// Always run the highest level at which there is a task.
    HighestFirst,
    /// Rotate between the schedule levels, round-robin, giving each level a certain
    /// duration of time slice.
    RoundRobin(Vec<Duration>)
}

/// Part of the pool config data which configured a certain pool-facing channel.
pub struct ChannelParams<Behavior: PoolBehavior> {
    /// The channel index in the pool behavior
    pub key: Behavior::ChannelKey,
    /// Whether to run this channel to completion before closing the pool
    pub complete_on_close: bool,
}