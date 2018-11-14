
use super::{PoolBehavior, PriorityLevel, ChannelToucher,
            ChannelToucherMut, RunningTask, ScheduleAlgorithm};
use channel::{Channel, BitAssigner, NotEnoughBits};

use std::sync::Arc;
use std::convert::Into;
use std::thread::JoinHandle;
use std::thread;
use std::ops::Deref;
use std::mem;
use std::any::Any;
use std::time::Duration as StdDuration;

use atomicmonitor::AtomMonitor;
use atomicmonitor::atomic::{Atomic, Ordering};

use atom::Atom;
use futures::{task, Async};
use futures::future::Future;
use futures::executor::{Notify, NotifyHandle};
use monitor::Monitor;
use time::Duration;
use stopwatch::Stopwatch;

/// The shared pool data.
pub struct Pool<Behavior: PoolBehavior> {
    /// The pool behavior
    behavior: Behavior,
    /// What point in the lifecycle of the threadpool it is in.
    lifecycle_state: Atomic<LifecycleState>,
    /// The atomic monitor bitfield for tracking the present status of each channel
    present_field: Arc<AtomMonitor<u64>>,
    /// The levels for the normal operation of the threadpool
    levels: Vec<Level<Behavior>>,
    /// The levels pertaining to what is necessary to run at shutdown
    levels_shutdown: Vec<Level<Behavior>>,
    /// The present bitfield mask for all channels which must be completed by shutdown
    complete_shutdown_mask: u64,
    /// The counter of externally blocked tasks which are critical for closing
    close_counter: AtomMonitor<u64>,
}

/// The shared pool data, the shared join handle to the pool, and the ability to tell the pool to close.
pub struct OwnedPool<Behavior: PoolBehavior> {
    pub pool: Arc<Pool<Behavior>>,
    pub join: PoolJoinHandle,
    workers: Vec<JoinHandle<()>>,
}

/// A handle for joining a pool's threads.
#[derive(Clone)]
pub struct PoolJoinHandle {
    completion: Arc<Monitor<bool>>
}
impl PoolJoinHandle {
    pub fn join(&self) {
        self.completion.with_lock(|mut guard| {
            while !*guard {
                guard.wait();
            }
        })
    }
}

/// A discrete priority level of a threadpool
struct Level<Behavior: PoolBehavior> {
    /// The present bitfield mask for all the channels on this level
    mask: u64,
    /// The roundrobin atomic index for alternating between channels in this level
    channel_index: Atomic<usize>,
    /// All the channels in this level
    channels: Vec<ChannelIdentifier<Behavior::ChannelKey>>
}

/// The identifier for a channel within a level, consisting of a channel key, and its bit mask.
#[derive(Copy, Clone)]
struct ChannelIdentifier<Key> {
    key: Key,
    mask: u64
}

/// What point in the lifecycle of the threadpool this service is in.
#[derive(Copy, Clone, Eq, PartialEq, Debug)]
enum LifecycleState {
    Running,
    Closed
}

/// Pool can be dereferenced to its behavior, for the purpose of submitting tasks to channels.
impl<Behavior: PoolBehavior> Deref for Pool<Behavior> {
    type Target = Behavior;

    fn deref(&self) -> &<Self as Deref>::Target {
        &self.behavior
    }
}

#[derive(Debug)]
pub enum NewPoolError {
    Over64Channels,
    InvalidChannelIndex,
    WrongNumberTimeslices {
        num_levels: usize,
        num_timeslices: usize,
    },
    NegativeTimeSlice {
        index: usize,
        duration: Duration,
    }
}

impl<Behavior: PoolBehavior> OwnedPool<Behavior> {
    /// Create and start a new pool
    pub fn new(mut behavior: Behavior) -> Result<Self, NewPoolError> {
        let config = behavior.config();

        let mut levels = Vec::new();
        let mut levels_shutdown = Vec::new();
        let mut complete_shutdown_mask = 0;

        let present_field = Arc::new(AtomMonitor::new(0));
        let mut current_bit = 0;

        {
            let mut bit_assigner = BitAssigner::new(&present_field, &mut current_bit);
            // build each level, both normal and shutdown simultaneously
            for &PriorityLevel(ref channel_param_vec) in &config.levels {
                let mut level = Level {
                    mask: 0,
                    channel_index: Atomic::new(0),
                    channels: Vec::new(),
                };
                let mut level_shutdown = Level {
                    mask: 0,
                    channel_index: Atomic::new(0),
                    channels: Vec::new(),
                };

                // loop through each channel in the level
                for channel_params in channel_param_vec {
                    // assign status bits
                    let bit_from = bit_assigner.current_index();
                    behavior.touch_channel_mut(channel_params.key, AssignChannelBits(&mut bit_assigner))
                        .map_err(|_| NewPoolError::Over64Channels)?;
                    let bit_until = bit_assigner.current_index();

                    // add to the level mask
                    let mut channel_mask: u64 = 0;
                    for bit in bit_from..bit_until {
                        channel_mask |= 0x1 << bit;
                    }
                    level.mask |= channel_mask;

                    // add to the channel vec
                    level.channels.push(ChannelIdentifier {
                        key: channel_params.key,
                        mask: channel_mask
                    });

                    // possibly repeat for shutdown
                    if channel_params.complete_on_close {
                        level_shutdown.mask |= channel_mask;
                        level_shutdown.channels.push(ChannelIdentifier {
                            key: channel_params.key,
                            mask: channel_mask,
                        });

                        // also add to the complete shutdown mask
                        complete_shutdown_mask |= channel_mask;
                    }
                }

                // the new level is built
                levels.push(level);
                levels_shutdown.push(level_shutdown);
            }
            // release the borrow of present field
        }

        // verify the scheduler algorithm's validity
        match config.schedule {
            ScheduleAlgorithm::HighestFirst => (),
            ScheduleAlgorithm::RoundRobin(ref time_slices) => {
                if time_slices.len() != levels.len() {
                    return Err(NewPoolError::WrongNumberTimeslices {
                        num_levels: levels.len(),
                        num_timeslices: time_slices.len(),
                    });
                }
            }
        };

        // create the shared pool struct
        let pool = Arc::new(Pool {
            behavior,
            lifecycle_state: Atomic::new(LifecycleState::Running),
            present_field,
            levels,
            levels_shutdown,
            complete_shutdown_mask,
            close_counter: AtomMonitor::new(0)
        });

        // spawn the workers
        /*
        let mut workers = Vec::new();
        for _ in 0..config.threads {
            let pool = pool.clone();
            let worker = match config.schedule {
                ScheduleAlgorithm::HighestFirst =>
                    thread::spawn(move || work_highest_first(pool)),
                ScheduleAlgorithm::RoundRobin(ref time_slices) => {
                    let time_slices = time_slices.clone();
                    thread::spawn(move || work_round_robin(pool, time_slices))
                },
            };
            workers.push(worker);
        }
        */

        let mut workers = Vec::new();
        match config.schedule {
            ScheduleAlgorithm::HighestFirst => {
                for _ in 0..config.threads {
                    let pool = pool.clone();
                    let worker = thread::spawn(move || work_highest_first(pool));
                    workers.push(worker);
                }
            },
            ScheduleAlgorithm::RoundRobin(time_slices) => {
                // convert time slices into std duration
                let mut std_time_slices = Vec::new();
                for (i, duration) in time_slices.into_iter().enumerate() {
                    match duration.to_std() {
                        Ok(std_duration) => {
                            std_time_slices.push(std_duration);
                        },
                        Err(_) => {
                            return Err(NewPoolError::NegativeTimeSlice {
                                index: i,
                                duration,
                            });
                        }
                    };
                }

                let pool = pool.clone();
                let worker = thread::spawn(move || work_round_robin(pool, std_time_slices));
                workers.push(worker);
            }
        };

        Ok(OwnedPool {
            pool,
            join: PoolJoinHandle {
                completion: Arc::new(Monitor::new(false))
            },
            workers,
        })
    }

    /// Close the pool. This will cause the threads to enter closing mode, but other joined threads
    /// will not be notified until this future is driven to completion.
    #[must_use]
    pub fn close(self) -> PoolClose {
        // raise the closing flag
        self.pool.lifecycle_state.compare_exchange(
            LifecycleState::Running,
            LifecycleState::Closed,
            Ordering::SeqCst, Ordering::SeqCst
        ).expect("Illegal lifecycle state");

        // interrupt blocking threads
        self.pool.present_field.notify_all();

        // future
        PoolClose {
            handles: Some(self.workers),
            completed: Arc::new(Atom::empty()),
            join: self.join.clone(),
        }
    }
}

/// Future for the pool closing.
/// If this future is not executed, other threads which have joined the pool will not wake up.
pub struct PoolClose {
    handles: Option<Vec<JoinHandle<()>>>,
    completed: Arc<Atom<Box<Result<(), Box<dyn Any + Send + 'static>>>>>,
    join: PoolJoinHandle,
}
impl Future for PoolClose {
    type Item = ();
    type Error = Box<dyn Any + Send + 'static>;

    fn poll(&mut self) -> Result<Async<<Self as Future>::Item>, <Self as Future>::Error> {
        if let Some(result) = self.completed.take() {
            self.join.completion.with_lock(|mut guard| {
                // wake up joined threads
                *guard = true;
                guard.notify_all();
            });
            result.map(|()| Async::Ready(()))
        } else if self.handles.is_some() {
            // spawn a thread to block on the join handles
            let handles = self.handles.take().unwrap();
            let completed = self.completed.clone();
            let task = task::current();

            thread::spawn(move || {
                for handle in handles {
                    match handle.join() {
                        Ok(()) => (),
                        Err(e) => {
                            completed.set_if_none(Box::new(Err(e)));
                            task.notify();
                            return;
                        }
                    };
                    completed.set_if_none(Box::new(Ok(())));
                    task.notify();
                }
            });

            Ok(Async::NotReady)
        } else {
            Ok(Async::NotReady)
        }
    }
}

fn work_highest_first<Behavior: PoolBehavior>(pool: Arc<Pool<Behavior>>) {
    // the main work loop
    'work: while pool.lifecycle_state.load(Ordering::Acquire) == LifecycleState::Running {

        // block until there is a task to run or the pool is closing
        pool.present_field.wait_until(|field| {
            field != 0x0 ||
                pool.lifecycle_state.load(Ordering::Acquire) != LifecycleState::Running
        });

        // find the highest level with a task
        let present_field_capture = pool.present_field.get();
        if let Some(level) = pool.levels.iter()
            .find(|level| (level.mask & present_field_capture) != 0x0) {

            // attempt to extract a task from the level, or jump to some other code point
            let (task, from) = 'find_task: loop {
                // get a channel index
                let level_channel_index = level.channel_index.fetch_add(1, Ordering::SeqCst)
                    % level.channels.len();
                let channel_identifier = level.channels[level_channel_index];

                // attempt to extract a task from that channel, and break the loop with it
                if let Some(task) = pool.behavior.touch_channel(channel_identifier.key, PollChannel) {
                    break 'find_task (task, channel_identifier);
                }

                // else, if the whole level is empty, skip this work pass, to avoid getting stuck
                // in the find task loop
                if (pool.present_field.get() & level.mask) == 0x0 {
                    continue 'work;
                }

                // else, if the pool is closing, break out of the work loop, to avoid getting stuck
                // in the find task loop
                if pool.lifecycle_state.load(Ordering::Acquire) != LifecycleState::Running {
                    break 'work;
                }

                // else, do another find task pass
            };

            // now that a task was successfully acquired, run it, then repeat
            run::run(&pool, task, from);
        }
    }

    // now that the pool is closing, properly run the close procedure
    close(pool);
}

fn work_round_robin<Behavior: PoolBehavior>(pool: Arc<Pool<Behavior>>, time_slices: Vec<StdDuration>) {
    // the main work loop
    'work: while pool.lifecycle_state.load(Ordering::Acquire) == LifecycleState::Running {

        // block until there is a task to run or the pool is closing
        pool.present_field.wait_until(|field| {
            field != 0x0 ||
                pool.lifecycle_state.load(Ordering::Acquire) != LifecycleState::Running
        });

        // iterate through the levels
        'levels: for (level_index, level) in pool.levels.iter().enumerate() {
            // start a timer for this level
            let timer = Stopwatch::start_new();

            // until either:
            while
                !({
                    // the level is empty
                    (pool.present_field.get() & level.mask) == 0x0
                } || {
                    // the stopwatch runs out
                    timer.elapsed() >= time_slices[level_index]
                } || {
                    // or the pool is closing
                    pool.lifecycle_state.load(Ordering::Acquire) != LifecycleState::Running
                }) {

                // wait until **any** channel has contents, or the stopwatch expires
                let remaining_time = Duration::from_std(time_slices[level_index]).unwrap() -
                    Duration::from_std(timer.elapsed()).unwrap();
                pool.present_field.wait_until_timeout(
                    |mask| mask != 0x0,
                    remaining_time
                );

                // if the pool is closing, break out of the work loop, to avoid getting stuck
                // in the find task loop
                if pool.lifecycle_state.load(Ordering::Acquire) != LifecycleState::Running {
                    break 'work;
                }

                // else, if the timer has expired, or this level is empty (but another level isn't
                // empty), continue to the next level
                if timer.elapsed() >= time_slices[level_index] ||
                    (pool.present_field.get() & level.mask) == 0x0 {
                    continue 'levels;
                }

                // otherwise, attempt to extract a task from the level, or jump to some other code point
                let (task, from) = 'find_task: loop {
                    // get a channel index
                    let level_channel_index = level.channel_index.fetch_add(1, Ordering::SeqCst)
                        % level.channels.len();
                    let channel_identifier = level.channels[level_channel_index];

                    // attempt to extract a task from that channel, and break the loop with it
                    if let Some(task) = pool.behavior.touch_channel(channel_identifier.key, PollChannel) {
                        break 'find_task (task, channel_identifier);
                    }

                    // else, if the whole level is empty, skip this work pass, to avoid getting stuck
                    // in the find task loop
                    if (pool.present_field.get() & level.mask) == 0x0 {
                        continue 'levels;
                    }

                    // else, if the pool is closing, break out of the work loop, to avoid getting stuck
                    // in the find task loop
                    if pool.lifecycle_state.load(Ordering::Acquire) != LifecycleState::Running {
                        break 'work;
                    }

                    // else, do another find task pass
                };

                // now that a task was successfully acquired, run it, then repeat
                run::run(&pool, task, from);
            }
        }
    }

    // now that the pool is closing, properly run the close procedure
    close(pool);
}

/// Highest-first close pool routine.
/// To manage code complexity, this is the close behavior for all pools, regardless of its
/// schedule algorithm while it's running.
fn close<Behavior: PoolBehavior>(pool: Arc<Pool<Behavior>>) {
    // to determine whether we should break the close loop, all shutdown channels must be empty
    // but also, we must wait for externally blocked close-critical tasks to complete
    let should_break_close = || {
        if (pool.present_field.get() & pool.complete_shutdown_mask) == 0x0 {
            // block until something changes
            pool.present_field.wait_until(|field| {
                // if the external count drops to zero, we obviously want to unblock, to check if there
                // are any remaining tasks. however, if a task does appear, we want to unblock, otherwise
                // we will cause deadlock, in which the worker thread will be waiting for the task to
                // complete, and the task will be waiting for the worker thread to execute it
                pool.close_counter.get() == 0 || (field & pool.complete_shutdown_mask) != 0x0
            });
            // now, if there is nothing to execute, that means we're done
            (pool.present_field.get() & pool.complete_shutdown_mask) == 0x0
        } else {
            false
        }
    };

    // the close loop, similar to the main loop, which runs close-critical tasks to completion
    'close: loop {
        // load the present field for this pass
        let present_field_capture = pool.present_field.get();

        // break if all shutdown channels are empty
        if should_break_close() {
            break 'close;
        }

        // otherwise, find the higheset shutdown level with a shutdown task to be run
        if let Some(level) = pool.levels_shutdown.iter()
            .find(|level| (level.mask & present_field_capture) != 0x0) {

            // attempt to extract a task from the level, or jump to some other code point
            let (task, from) = 'find_task: loop {
                // get a channel index
                let level_channel_index = level.channel_index.fetch_add(1, Ordering::SeqCst)
                    % level.channels.len();
                let channel_identifier = level.channels[level_channel_index];

                // attempt to extract a task from that channel, and break the loop with it
                if let Some(task) = pool.behavior.touch_channel(channel_identifier.key, PollChannel) {
                    break 'find_task (task, channel_identifier);
                }

                if should_break_close() {
                    // if all shutdown channels are empty, exit the close loop
                    break 'close;
                } else if (pool.present_field.get() & pool.complete_shutdown_mask & level.mask) == 0x0 {
                    // if all channels in the level are empty, skip this close pass, to avoid getting stuck
                    // in the find task loop
                    continue 'close;
                }

                // else, do another find task pass
            };

            // now that a task was successfully acquired, run it, then repeat
            run::run(&pool, task, from);
        }
    };
}

/// The algorithm for running tasks with no mutex use, based on atomic responsibility transfer of
/// heap-allocated data.
mod run {
    use super::*;

    pub (super) fn run<Behavior: PoolBehavior>(
        pool: &Arc<Pool<Behavior>>,
        task: RunningTask,
        from: ChannelIdentifier<Behavior::ChannelKey>
    ) {
        unsafe {
            let task: *mut RunningTask = Box::into_raw(Box::new(task));
            run_helper(pool, task, from)
        };
    }

    unsafe fn run_helper<Behavior: PoolBehavior>(
        pool: &Arc<Pool<Behavior>>,
        task: *mut RunningTask,
        from: ChannelIdentifier<Behavior::ChannelKey>,
    ) {
        let status: Arc<Atomic<RunStatus>> = Arc::new(Atomic::new(RunStatus::NotRequestedAndWillBeTakenCareOf));
        match (&mut*task).spawn.poll_future_notify(&IntoAtomicFollowup {
            pool,
            from,
            status: &status,
            task
        }, 0) {
            Ok(Async::NotReady) => {
                match status.compare_exchange(
                    RunStatus::NotRequestedAndWillBeTakenCareOf,
                    RunStatus::NotRequestedAndWillNotBeTakenCareOf,
                    Ordering::SeqCst, Ordering::SeqCst
                ) {
                    Ok(_) => {
                        // the future will be re-inserted asynchronously by an external actor
                        if (from.mask & pool.complete_shutdown_mask) != 0x0 {
                            if let Ok(_) = (&*task).close_counted.compare_exchange(
                                false, true, Ordering::SeqCst, Ordering::SeqCst) {

                                pool.close_counter.mutate(|counter| {
                                    counter.fetch_add(1, Ordering::SeqCst);
                                });
                            }
                        }
                    },
                    Err(RunStatus::RequestedAndWillBeTakenCareOf) => {
                        // recurse
                        run_helper(pool, task, from);
                    },
                    Err(other) => panic!("Illegal run status: {:?}", other)
                };
            },
            _ => {
                // complete, so drop it
                if (&*task).close_counted.load(Ordering::Acquire) {
                    pool.close_counter.mutate(|counter| {
                        counter.fetch_sub(1, Ordering::SeqCst);
                    });
                    // if we're closing, notify the present field, so that the worker can unblock
                    // if it's waiting to close for externally satisfied conditions
                    if pool.lifecycle_state.load(Ordering::Acquire) == LifecycleState::Closed {
                        pool.present_field.notify_all();
                    }
                }
                mem::drop(Box::from_raw(task));
            }
        };
    }

    #[repr(u8)]
    #[derive(Copy, Clone, Eq, PartialEq, Debug)]
    enum RunStatus {
        NotRequestedAndWillBeTakenCareOf,
        RequestedAndWillBeTakenCareOf,
        NotRequestedAndWillNotBeTakenCareOf
    }

    pub struct AtomicFollowup<Behavior: PoolBehavior> {
        pool: Arc<Pool<Behavior>>,
        from: ChannelIdentifier<Behavior::ChannelKey>,
        status: Arc<Atomic<RunStatus>>,
        task: *mut RunningTask
    }
    impl<Behavior: PoolBehavior> Notify for AtomicFollowup<Behavior> {
        fn notify(&self, _: usize) {
            match self.status.compare_exchange(
                RunStatus::NotRequestedAndWillBeTakenCareOf,
                RunStatus::RequestedAndWillBeTakenCareOf,
                Ordering::SeqCst, Ordering::SeqCst
            ) {
                Err(RunStatus::NotRequestedAndWillNotBeTakenCareOf) => unsafe {
                    let task: RunningTask = *Box::from_raw(self.task);
                    self.pool.behavior.followup(self.from.key, task);
                },
                Ok(RunStatus::RequestedAndWillBeTakenCareOf) => (),
                invalid => panic!("Invalid atomic followup CAS result: {:#?}", invalid)
            };
        }
    }
    unsafe impl<Behavior: PoolBehavior> Send for AtomicFollowup<Behavior> {}
    unsafe impl<Behavior: PoolBehavior> Sync for AtomicFollowup<Behavior> {}

    pub struct IntoAtomicFollowup<'a, 'b, Behavior: PoolBehavior> {
        pool: &'a Arc<Pool<Behavior>>,
        from: ChannelIdentifier<Behavior::ChannelKey>,
        status: &'b Arc<Atomic<RunStatus>>,
        task: *mut RunningTask
    }
    impl<'a, 'b, Behavior: PoolBehavior> Into<NotifyHandle> for IntoAtomicFollowup<'a, 'b, Behavior> {
        fn into(self) -> NotifyHandle {
            NotifyHandle::from(Arc::new(AtomicFollowup {
                pool: self.pool.clone(),
                from: self.from,
                status: self.status.clone(),
                task: self.task
            }))
        }
    }
    impl<'a, 'b, Behavior: PoolBehavior> Clone for IntoAtomicFollowup<'a, 'b, Behavior> {
        fn clone(&self) -> Self {
            IntoAtomicFollowup {
                ..*self
            }
        }
    }
}

struct AssignChannelBits<'a, 'b: 'a, 'c: 'a>(&'a mut BitAssigner<'b, 'c>);
impl<'a, 'b: 'a, 'c: 'a> ChannelToucherMut<Result<(), NotEnoughBits>> for AssignChannelBits<'a, 'b, 'c> {
    fn touch_mut(&mut self, channel: &mut impl Channel) -> Result<(), NotEnoughBits> {
        let &mut AssignChannelBits(ref mut assigner) = self;
        channel.assign_bits(assigner)
    }
}

struct PollChannel;
impl ChannelToucher<Option<RunningTask>> for PollChannel {
    fn touch(&mut self, channel: & impl Channel) -> Option<RunningTask> {
        channel.poll()
    }
}