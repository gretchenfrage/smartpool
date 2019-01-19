# Smartpool

Smartpool is a future-aware threadpool designed to grant great control over the behavior 
of the pool. It requires a little bit of coding to get up and running, but can remain reactive
in many high-backpressure situations, or situations where the pool has to shut down in a specific 
way. If there's some specific priority scheme missing from the pool, this library can be easily 
extended to achieve the desired behavior.

### Features

* High-performance concurrency
* Future awareness
* Multiple priority levels
* Customizable task prioritization behavior
* Partitioning task channels by their behavior when the pool closes
* Scoped operations
* Scheduled operations
* Spatial prioritization, with the `smartpool-spatial` crate
* Use on stable channel
* Round-robin scheduler
* Shortest deadline first
* Extensible
* Data-driven
* BPA-free
* 20% more gucci than our competitors, and with 70% more buzzwords

### Concepts

The smartpool architecture has a few foundational concepts:

- Channels
- Levels
- Schedulers
- Pool

#### Channels

A channel is some source that tasks can be polled from. Usually, a channel is some sort of
task queue, which the programmer can insert tasks into.
These channels can have any exotic method of prioritization, such as _soonest deadline 
first_ or **spatial prioritization.**

From an abstract standpoint, a channel does not actually have to be a queue which 
can be inserted into; it only has to be pollable. For example, a channel could be made 
to mine bitcoin, simulate protein folding, or asynchronously download tasks from a 
network.

#### Levels

A level is a group of channels that have the same _category of priority_.
This sounds vague because the exact meaning of a level is dependent on the scheduler.

#### Schedulers

Schedulers determine how the pool prioritizes different levels. While channels
are completely extensible, schedulers unfortunately are not; they currently require 
modification to the core logic of the pool. As of the initial release of this project,
two schedulers exist:

- Highest Level First
    - The pool will always poll for a task from the highest level available.
- Round Robin
    - The pool will alternate between different levels in a loop, allocating a specific
    duration of real world time to each. 

#### Pool

A pool is the actual collection of threads, and other data used to manage it.
The pool is divided into two parts:

- An `OwnedPool`, which denotes ownership of the pool, and can be used to close the 
pool and wait for it to properly close
- An `Arc<Pool>`, a shareable handle which can be used to access its channels.

#### Followup

Smartpool is a future-aware threadpool. This means that it efficiently and idiomatically 
handles coroutines which can yield. In the event that a task yields, it must be 
re-inserted into some channel. After a task is taken from a channel, and it yields, it
is re-inserted into some _followup_ channel. The exact decision of which 
channel becomes the followup channel is configured by the user.

It can improve _average time to completion_ for a channel's followup channel to be of a 
higher priority level than the original channel. Essentially, this is because that causes
the threadpool is "commit" it completing a task after it has started that task, and not become 
"distracted" with other tasks.

#### Closing

An `OwnedPool` can be used to properly close its corresponding threadpool. The close
method produces a future, which can either be discarded or awaited. 

Each channel can configure whether or not it is `complete_on_close`. When a threadpool
closes, if it still contains tasks, all task in channels that are `complete_on_close` will
be run to completion, whereas tasks in other channels will be completely ignored (and 
eventually, dropped). 

Even if a coroutine from a `complete_on_close` channel is currently blocking on some 
external condition, the threadpool will wait for the coroutine to wake up, then 
drive it to completion, before closing.

### Example Code

    extern crate smartpool;
    extern crate atomic;
    extern crate num_cpus;
    extern crate futures;
    
    use futures::prelude::*;
    use smartpool::prelude::*;
    use std::sync::Arc;
    
    // we will use a submodule to define our pool behavior
    mod my_pool {
        // the prelude::setup module is a special prelude for defining pool behavior
        use smartpool::prelude::setup::*;
        use num_cpus;
    
        // enum for a channel in the pool
        #[derive(Copy, Clone, Eq, PartialEq, Debug)]
        pub enum MyPoolChannel {
            Realtime,
            Responsive,
            Backlog,
            BacklogDeadline,
        }
        // the pool behavior
        pub struct MyPool {
            // we can only submit time-critical tasks here
            // the multi channel creates an array of the inner channel type, and alternates
            // between them round-robin using an atomic integer.
            // this reduces thread contention.
            pub realtime: MultiChannel<VecDequeChannel>,
            // we can submit tasks here which should be executed soonish, but try to keep backpressure low
            pub responsive: MultiChannel<VecDequeChannel>,
            // the backlog can be a lot of extra stuff that needs to get run
            pub backlog: MultiChannel<VecDequeChannel>,
            // another backlog channel with a SDF scheduler
            pub backlog_deadline: MultiChannel<ShortestDeadlineFirst>,
        }
        impl MyPool {
            // constructor is useful
            pub fn new() -> Self {
                MyPool {
                    realtime: MultiChannel::new(4, VecDequeChannel::new),
                    responsive: MultiChannel::new(4, VecDequeChannel::new),
                    backlog: MultiChannel::new(4, VecDequeChannel::new),
                    backlog_deadline: MultiChannel::new(4, ShortestDeadlineFirst::new),
                }
            }
        }
        // implementing pool behavior allows it to initialize a threadpool
        impl PoolBehavior for MyPool {
            type ChannelKey = MyPoolChannel;
    
            // configuration data, queried once by the pool
            fn config(&mut self) -> PoolConfig<Self> {
                PoolConfig {
                    // as many threads as we have CPU
                    threads: num_cpus::get() as u32,
                    // use the highest first scheduler
                    schedule: ScheduleAlgorithm::HighestFirst,
                    // levels, highest to lowest
                    levels: vec![
                        // level 0 is just the realtime channel
                        // it will complete on close
                        vec![ChannelParams {
                            key: MyPoolChannel::Realtime,
                            complete_on_close: true,
                        }],
                        // level 1 is just the responsive channel
                        // it will complete on close
                        vec![ChannelParams {
                            key: MyPoolChannel::Responsive,
                            complete_on_close: true,
                        }],
                        // level 2 is both backlog channels, together
                        // they will not complete on close
                        vec![
                            ChannelParams {
                                key: MyPoolChannel::Backlog,
                                complete_on_close: false,
                            },
                            ChannelParams {
                                key: MyPoolChannel::BacklogDeadline,
                                complete_on_close: false,
                            }
                        ]
                    ]
                }
            }
    
            // boilerplate for the engine to access the channels with no dynamic dispatch
            fn touch_channel<O>(&self, key: MyPoolChannel, mut toucher: impl ChannelToucher<O>) -> O {
                match key {
                    MyPoolChannel::Realtime => toucher.touch(&self.realtime),
                    MyPoolChannel::Responsive => toucher.touch(&self.responsive),
                    MyPoolChannel::Backlog => toucher.touch(&self.backlog),
                    MyPoolChannel::BacklogDeadline => toucher.touch(&self.backlog_deadline),
                }
            }
    
            // boilerplate for the engine to access the channels with no dynamic dispatch
            fn touch_channel_mut<O>(&mut self, key: MyPoolChannel, mut toucher: impl ChannelToucherMut<O>) -> O {
                match key {
                    MyPoolChannel::Realtime => toucher.touch_mut(&mut self.realtime),
                    MyPoolChannel::Responsive => toucher.touch_mut(&mut self.responsive),
                    MyPoolChannel::Backlog => toucher.touch_mut(&mut self.backlog),
                    MyPoolChannel::BacklogDeadline => toucher.touch_mut(&mut self.backlog_deadline),
                }
            }
    
            // given a running task which has been produced by a particular channel, then yielded,
            // then become available once again, re-insert it in some channel
            fn followup(&self, from: MyPoolChannel, task: RunningTask) {
                match from {
                    MyPoolChannel::Realtime => self.realtime.submit(task),
                    MyPoolChannel::Responsive => self.responsive.submit(task),
                    MyPoolChannel::Backlog => self.responsive.submit(task),
                    MyPoolChannel::BacklogDeadline => self.responsive.submit(task),
                }
            }
        }
    }
    
    use self::my_pool::MyPool;
    
    fn main() {
        // create the threadpool
        let owned: OwnedPool<MyPool> = OwnedPool::new(MyPool::new()).unwrap();
        let pool: Arc<Pool<MyPool>> = owned.pool.clone();
    
        // spawn a task (use the run function to create a task of pure work)
        pool.realtime.exec(run(|| {
            let mut a: u128 = 0;
            let mut b: u128 = 1;
            for _ in 0..100 {
                let sum = a + b;
                a = b;
                b = sum;
            }
            println!("{}", a);
        }));
    
        // close the pool, and wait for it to finish closing
        owned.close().wait().unwrap();
    }

### Scoped Operations

The `scoped` module allows for the execution of batches of operations which 
can reference the local stack frame, in a safe way. This can help avoid
the performance and elegance penalties that come with sharing data across multithreaded
tasks.

### Scheduled Operations

The `timescheduler` module allows for the creation of futures which become available
at some moment in the future, or which stream data at a periodic interval. This allows 
for operations which are scheduled to occur in the future.

### Extensibility

The behavior of the pool can be easily extended by third party libraries, simply 
by implementing the `Channel` interface, as well as optionally `Exec` or
`ExecParam`.

### The Algorithm

The internal algorithm is relatively simple, after being abstracted into several 
levels and separated into modules. No thread cooperates with any other thread,
and each thread has identical behavior. Each thread simply attempts to find the 
highest priority level with an available task, and poll a task from some channel
in that level, then run it. When there are multiple channels in a level, it simply 
alternates between them, round-robin, with an atomic integer. Each channel 
internally uses a data structure, wrapped with a mutex, and is pushed to and polled 
from by use of internal mutability.

However, this algorithm still presents some troublesome issues to be resolved:

- How does the pool keep track of which channels and which levels have tasks in them?
- How does the pool properly sleep when there are no tasks, and then wake up when 
tasks become available somewhere, without introducing global contention?
- How can the concept of coroutines, which can yield and then be notified asynchronously,
be integrated in to the pool in a minimally expensive way?

#### Status field

The first two issues are addressed with the `atomicmonitor` abstraction, which lives 
in its own crate. To summarize, the atomicmonitor behaves like a monitor over atomic 
data, which is mutated atomically, and can often be less contentious than a traditional
monitor.

Each mutex-guarded data structure that may correspond to the availability of a 
task in a particular channel (which is usually 1 per channel) corresponds to a bit in a 
shared `AtomicMonitor<u64>`. This atomic `u64` is a bitfield, where each bit represents
whether there are any tasks in a particular mutex-guarded structure. The channel
must simply maintain the validity of this bit when adding or removing elements to its
queue data structure. When the value of a particular bit changes, the atomic monitor
is updated with atomic bitwise AND and OR instructions.

When the threadpool is checking for whether there are any tasks in a particular level,
it can simply load the bitfield, AND it with the mask for all the bits corresponding to
channels in that level, and check for equality to `0x0`. 

More significantly, however, is that for the threadpool to properly handle sleeping
in a minimal-cost way, it simply has to block on the atomicmonitor on the condition that
the bitfield is not equal to `0x0`.

#### Atomic responsibility transfer

The third issue, that of efficiently handling coroutines with regard to rust's ownership 
schemes, is handled by another algorithm which was invented for this threadpool:
atomic responsibility transfer of pointers to owned heap data.

When the smartpool runs `poll_future_notify` on a coroutine, it attempts to drive the 
coroutine to completion. At this point, the coroutine can return two possible results

- Done (success or failure)
- Blocking

In the event that the future returns *blocking*, at some point in the future, the coroutine
will become available, and a callback which we passed into `poll_future_notify` will be
activated. 

This would seem to suggest a simple approach to handling notification: for the notification 
handler callback to simply re-submit the task to the followup queue. However, there is one 
architecture detail that complicates this:

_For the pool to properly shutdown, including waiting on yielded coroutines on channels 
which are `complete_on_close` before eventually driving them to completion, an atomicmonitor 
must be maintained which counts all of the externally blocked coroutines which come from
channels which are `complete_on_close`._ This atomic integer must be incremented, and then
decremented, **in that order**, during the time when the pool is blocked from closing 
by an external task. This detail makes handling ownership with regard to running tasks 
surprisingly complicated.

As a solution to this issue, we introduce two atomic data, which exist for every task:

- `Atomic<bool>` spawn_counted
    - tied into the `RunningTask`
- `Atomic<RunStatus>` status
    - shared between the running task and the notify callback
    - 3 possible states
        - `NotRequestedAndWillBeTakenCareOf`
        - `RequestedAndWillBeTakenCareOf`
        - `NotRequestedAndWillNotBeTakenCareOf`
        
When a `RunningTask` begins to run, it is moved onto the heap, and turned into a raw pointer.
Our algorithms take manual responsibility for handling its borrowing and 
lifetime semantics.

The exact ways in which these atomic data are used are best expressed through code, and the implications 
regarding why this code is necessary is best expressed through tinkering with the code 
and attempting to run the automated tests. This code can be found in the `run` submodule within 
the `pool` module file.

### Performance

The unit tests for this crate contain performance tests of various pool setups,
the results of which can be seen by setting the rust log level to `INFO` or above while running 
the tests.

One such very unscientific test performs 1,000 batches of 1,000 atomic scoped operations
(to eliminate reference counting costs). This test, running in release mode, on a windows 10 
ultrabook laptop, yields an average operation time of:

**824 nanoseconds**