extern crate pretty_env_logger;

use prelude::setup::*;

use std::sync::Arc;
use std::time::Duration as StdDuration;
use std::thread::sleep;
use std::sync::{Once, ONCE_INIT};
use std::thread;

use time::{SteadyTime, Duration};
use futures::prelude::*;
use atomicmonitor::AtomMonitor;
use atomicmonitor::atomic::{Atomic, Ordering};
use monitor::Monitor;


static INIT_LOG: Once = ONCE_INIT;
pub fn init_log() {
    INIT_LOG.call_once(|| {
        pretty_env_logger::init();
    });
}

struct OneChannelPool<C: Channel + Exec + Send + Sync + 'static> {
    thread_count: u32,
    schedule: ScheduleAlgorithm,
    channel: C,
}
impl<C: Channel + Exec + Send + Sync + 'static> OneChannelPool<C> {
}
impl<C: Channel + Exec + Send + Sync + 'static> PoolBehavior for OneChannelPool<C> {
    type ChannelKey = ();

    fn config(&mut self) -> PoolConfig<Self> {
        PoolConfig {
            threads: self.thread_count,
            schedule: self.schedule.clone(),
            levels: vec![
                vec![
                    ChannelParams {
                        key: (),
                        complete_on_close: true
                    }
                ]
            ]
        }
    }

    fn touch_channel<O>(&self, _key: <Self as PoolBehavior>::ChannelKey, mut toucher: impl ChannelToucher<O>) -> O {
        toucher.touch(&self.channel)
    }

    fn touch_channel_mut<O>(&mut self, _key: <Self as PoolBehavior>::ChannelKey, mut toucher: impl ChannelToucherMut<O>) -> O {
        toucher.touch_mut(&mut self.channel)
    }

    fn followup(&self, _from: <Self as PoolBehavior>::ChannelKey, task: RunningTask) {
        self.channel.submit(task);
    }
}

/// Tests that the threadpool can execute a series of tasks.
#[test]
fn simple_threadpool_test() {
    init_log();
    let owned = OwnedPool::new(OneChannelPool {
        thread_count: 8,
        schedule: ScheduleAlgorithm::HighestFirst,
        channel: VecDequeChannel::new()
    }).unwrap();
    let count = Arc::new(Monitor::new(0));

    for _ in 0..100 {
        let count = count.clone();
        owned.pool.channel.exec(run(move || count.with_lock(|mut guard| {
            *guard += 1;
            guard.notify_all();
        })));
    }

    let result = count.with_lock(|mut guard| {
        let end = SteadyTime::now() + Duration::seconds(15);
        while *guard < 100 && SteadyTime::now() < end {
            guard.wait_timeout(Duration::seconds(1).to_std().unwrap());
        }
        *guard
    });
    assert_eq!(result, 100);
}

#[test]
fn push_future_test() {
    init_log();

    fn pool() -> Arc<Pool<OneChannelPool<VecDequeChannel>>> {
        let owned = OwnedPool::new(OneChannelPool {
            thread_count: 8,
            schedule: ScheduleAlgorithm::HighestFirst,
            channel: VecDequeChannel::new()
        }).unwrap();
        owned.pool.clone()
    }

    let pool_1 = pool();
    let pool_2 = pool();
    let scheduler = TimeScheduler::new();

    let later = scheduler.periodically(SteadyTime::now(), Duration::milliseconds(100))
        .take(10)
        .into_future()
        .map_err(|_| ())
        .map(|_| "hello world");

    let atomic_0 = Arc::new(AtomMonitor::new(0));
    let atomic_1 = atomic_0.clone();

    let triggered = pool_1.channel.exec_push(later)
        .map(move |string| {
            if string == "hello world" {
                atomic_1.set(1);
            } else {
                atomic_1.set(-1);
            }
        });
    pool_2.channel.exec(triggered);

    match atomic_0.wait_until_timeout(|n| n != 0, Duration::seconds(2)) {
        None => panic!("timed out"),
        Some(1) => (),
        Some(-1) => panic!("failed to communicate correctly"),
        other => unreachable!("invalid {:?}", other),
    };

}

/// Tests that the threadpool can execute a series of tasks with the round robin scheduler.
#[test]
fn simple_threadpool_test_round_robin() {
    init_log();
    let owned = OwnedPool::new(OneChannelPool {
        thread_count: 8,
        schedule: ScheduleAlgorithm::RoundRobin(vec![Duration::milliseconds(50)]),
        channel: VecDequeChannel::new()
    }).unwrap();
    let count = Arc::new(Monitor::new(0));

    for _ in 0..100 {
        let count = count.clone();
        owned.pool.channel.exec(run(move || count.with_lock(|mut guard| {
            *guard += 1;
            guard.notify_all();
        })));
    }

    let result = count.with_lock(|mut guard| {
        let end = SteadyTime::now() + Duration::seconds(15);
        while *guard < 100 && SteadyTime::now() < end {
            guard.wait_timeout(Duration::seconds(1).to_std().unwrap());
        }
        *guard
    });
    assert_eq!(result, 100);
}

struct CompleteOnCloseTestPool {
    do_complete: VecDequeChannel,
    do_not_complete: VecDequeChannel,
}
impl PoolBehavior for CompleteOnCloseTestPool {
    type ChannelKey = bool;

    fn config(&mut self) -> PoolConfig<Self> {
        PoolConfig {
            threads: 4,
            schedule: ScheduleAlgorithm::HighestFirst,
            levels: vec![
                vec![
                    ChannelParams {
                        key: true,
                        complete_on_close: true
                    },
                    ChannelParams {
                        key: false,
                        complete_on_close: false
                    }
                ]
            ]
        }
    }

    fn touch_channel<O>(&self, key: <Self as PoolBehavior>::ChannelKey, mut toucher: impl ChannelToucher<O>) -> O {
        match key {
            true => toucher.touch(&self.do_complete),
            false => toucher.touch(&self.do_not_complete),
        }
    }

    fn touch_channel_mut<O>(&mut self, key: <Self as PoolBehavior>::ChannelKey, mut toucher: impl ChannelToucherMut<O>) -> O {
        match key {
            true => toucher.touch_mut(&mut self.do_complete),
            false => toucher.touch_mut(&mut self.do_not_complete),
        }
    }

    fn followup(&self, from: <Self as PoolBehavior>::ChannelKey, task: RunningTask) {
        match from {
            true => self.do_complete.submit(task),
            false => self.do_not_complete.submit(task)
        }
    }
}

/// Test that the pool correctly pulls from multiple channels, and that close channels and
/// open channels are handled properly.
#[test]
fn close_test() {
    init_log();
    let owned = OwnedPool::new(CompleteOnCloseTestPool {
        do_complete: VecDequeChannel::new(),
        do_not_complete: VecDequeChannel::new()
    }).unwrap();

    // there are 4 threads

    // submit 10 tasks to the do_not_complete channels, blocking all threads
    let monitor_1 = Arc::new(Monitor::new(false));
    let counter_1 = Arc::new(Atomic::new(0usize));
    for _ in 0..10 {
        let monitor_1 = monitor_1.clone();
        let counter_1 = counter_1.clone();
        owned.pool.do_not_complete.exec(run(move || monitor_1.with_lock(|mut guard| {
            trace!("enter guard 1");
            let end = SteadyTime::now() + Duration::seconds(10);
            while !*guard && SteadyTime::now() < end {
                guard.wait_timeout(StdDuration::from_secs(1));
            }
            counter_1.fetch_add(1, Ordering::SeqCst);
            trace!("exit guard 1");
        })))
    }

    sleep(StdDuration::from_millis(500));
    trace!("slept");

    // submit another 10 tasks to the do_complete channel
    let counter_2 = Arc::new(Monitor::new(0usize));
    for _ in 0..10 {
        let counter_2 = counter_2.clone();
        owned.pool.do_complete.exec(run(move || {
            counter_2.with_lock(|mut guard| {
                *guard += 1;
                guard.notify_all();
            })
        }))
    }

    sleep(StdDuration::from_millis(500));

    // close the pool
    let _ = owned.close();

    // unblock the first 10 tasks
    monitor_1.with_lock(|mut guard| {
        *guard = true;
        guard.notify_all();
    });

    sleep(StdDuration::from_millis(500));

    // wait for the second batch of tasks to run to completion
    let second_batch_result = counter_2.with_lock(|mut guard| {
        let end = SteadyTime::now() + Duration::seconds(10);
        while *guard < 10 && SteadyTime::now() < end {
            guard.wait_timeout(StdDuration::from_secs(1));
        }
        *guard
    });

    // the second batch, being on the do_complete channel, should run to completion
    assert_eq!(second_batch_result, 10);

    // the first batch, on the other hand, should only have 4 completed, corresponding to the
    // 4 threads, since the threads began the tasks before the pool was placed into closing state
    assert_eq!(counter_1.load(Ordering::Acquire), 4);
}

struct MultiLevelPool {
    alpha_1: VecDequeChannel,
    alpha_2: VecDequeChannel,
    beta: VecDequeChannel,
    gamma_1: VecDequeChannel,
    gamma_2: VecDequeChannel,
}
impl MultiLevelPool {
    fn new() -> Self {
        MultiLevelPool {
            alpha_1: VecDequeChannel::new(),
            alpha_2: VecDequeChannel::new(),
            beta: VecDequeChannel::new(),
            gamma_1: VecDequeChannel::new(),
            gamma_2: VecDequeChannel::new(),
        }
    }
}
#[derive(Copy, Clone)]
enum MultiLevelPoolChannel {
    Alpha1,
    Alpha2,
    Beta,
    Gamma1,
    Gamma2
}
impl PoolBehavior for MultiLevelPool {
    type ChannelKey = MultiLevelPoolChannel;

    fn config(&mut self) -> PoolConfig<Self> {
        PoolConfig {
            threads: 4,
            schedule: ScheduleAlgorithm::HighestFirst,
            levels: vec![
                vec![
                    ChannelParams {
                        key: MultiLevelPoolChannel::Alpha1,
                        complete_on_close: false
                    },
                    ChannelParams {
                        key: MultiLevelPoolChannel::Alpha2,
                        complete_on_close: false
                    }
                ],
                vec![
                    ChannelParams {
                        key: MultiLevelPoolChannel::Beta,
                        complete_on_close: false
                    }
                ],
                vec![
                    ChannelParams {
                        key: MultiLevelPoolChannel::Gamma1,
                        complete_on_close: false
                    },
                    ChannelParams {
                        key: MultiLevelPoolChannel::Gamma2,
                        complete_on_close: false
                    }
                ]
            ]
        }
    }

    fn touch_channel<O>(&self, key: <Self as PoolBehavior>::ChannelKey, mut toucher: impl ChannelToucher<O>) -> O {
        match key {
            MultiLevelPoolChannel::Alpha1 => toucher.touch(&self.alpha_1),
            MultiLevelPoolChannel::Alpha2 => toucher.touch(&self.alpha_2),
            MultiLevelPoolChannel::Beta => toucher.touch(&self.beta),
            MultiLevelPoolChannel::Gamma1 => toucher.touch(&self.gamma_1),
            MultiLevelPoolChannel::Gamma2 => toucher.touch(&self.gamma_2),
        }
    }

    fn touch_channel_mut<O>(&mut self, key: <Self as PoolBehavior>::ChannelKey, mut toucher: impl ChannelToucherMut<O>) -> O {
        match key {
            MultiLevelPoolChannel::Alpha1 => toucher.touch_mut(&mut self.alpha_1),
            MultiLevelPoolChannel::Alpha2 => toucher.touch_mut(&mut self.alpha_2),
            MultiLevelPoolChannel::Beta => toucher.touch_mut(&mut self.beta),
            MultiLevelPoolChannel::Gamma1 => toucher.touch_mut(&mut self.gamma_1),
            MultiLevelPoolChannel::Gamma2 => toucher.touch_mut(&mut self.gamma_2),
        }
    }

    fn followup(&self, from: <Self as PoolBehavior>::ChannelKey, task: RunningTask) {
        match from {
            MultiLevelPoolChannel::Alpha1 => self.alpha_1.submit(task),
            MultiLevelPoolChannel::Alpha2 => self.alpha_2.submit(task),
            MultiLevelPoolChannel::Beta => self.beta.submit(task),
            MultiLevelPoolChannel::Gamma1 => self.gamma_1.submit(task),
            MultiLevelPoolChannel::Gamma2 => self.gamma_2.submit(task),
        };
    }
}

#[test]
fn multi_level_test() {
    init_log();
    let owned = OwnedPool::new(MultiLevelPool::new()).unwrap();

    // submit 10 tasks to the alpha level, which are blocked
    let alpha_blocker = Arc::new(Monitor::new(false));
    let alpha_started = Arc::new(Atomic::new(0usize));
    let alpha_run = Arc::new(Atomic::new(0usize));
    for i in 0..10 {
        let alpha_started = alpha_started.clone();
        let alpha_blocker = alpha_blocker.clone();
        let alpha_run = alpha_run.clone();
        let task = run(move || {
            alpha_started.fetch_add(1, Ordering::SeqCst);
            alpha_blocker.with_lock(|mut guard| {
                let end = SteadyTime::now() + Duration::seconds(10);
                while !*guard && SteadyTime::now() < end {
                    guard.wait_timeout(StdDuration::from_secs(1));
                }
            });
            alpha_run.fetch_add(1, Ordering::SeqCst);
        });
        if i % 2 == 0 {
            owned.pool.alpha_1.exec(task);
        } else {
            owned.pool.alpha_2.exec(task);
        }
    }

    sleep(StdDuration::from_millis(500));

    // then to gamma
    let gamma_blocker = Arc::new(Monitor::new(false));
    let gamma_started = Arc::new(Atomic::new(0usize));
    let gamma_run = Arc::new(Atomic::new(0usize));
    for i in 0..10 {
        let gamma_blocker = gamma_blocker.clone();
        let gamma_started = gamma_started.clone();
        let gamma_run = gamma_run.clone();
        let task = run(move || {
            gamma_started.fetch_add(1, Ordering::SeqCst);
            gamma_blocker.with_lock(|mut guard| {
                let end = SteadyTime::now() + Duration::seconds(10);
                while !*guard && SteadyTime::now() < end {
                    guard.wait_timeout(StdDuration::from_secs(1));
                }
            });
            gamma_run.fetch_add(1, Ordering::SeqCst);
        });
        if i % 2 == 0 {
            owned.pool.gamma_1.exec(task);
        } else {
            owned.pool.gamma_2.exec(task);
        }
    }

    sleep(StdDuration::from_millis(500));

    // then to beta
    let beta_blocker = Arc::new(Monitor::new(false));
    let beta_started = Arc::new(Atomic::new(0usize));
    let beta_run = Arc::new(Atomic::new(0usize));
    for _ in 0..10 {
        let beta_blocker = beta_blocker.clone();
        let beta_started = beta_started.clone();
        let beta_run = beta_run.clone();
        let task = run(move || {
            beta_started.fetch_add(1, Ordering::SeqCst);
            beta_blocker.with_lock(|mut guard| {
                let end = SteadyTime::now() + Duration::seconds(10);
                while !*guard && SteadyTime::now() < end {
                    guard.wait_timeout(StdDuration::from_secs(1));
                }
            });
            beta_run.fetch_add(1, Ordering::SeqCst);
        });
        owned.pool.beta.exec(task);
    }

    sleep(StdDuration::from_millis(500));

    // assert initial conditions
    assert_eq!(alpha_started.load(Ordering::Acquire), 4);
    assert_eq!(alpha_run.load(Ordering::Acquire), 0);
    assert_eq!(beta_started.load(Ordering::Acquire), 0);
    assert_eq!(beta_run.load(Ordering::Acquire), 0);
    assert_eq!(gamma_started.load(Ordering::Acquire), 0);
    assert_eq!(gamma_run.load(Ordering::Acquire), 0);

    // unblock alpha
    alpha_blocker.with_lock(|mut guard| {
        *guard = true;
        guard.notify_all();
    });

    sleep(StdDuration::from_millis(500));

    // assert new conditions
    assert_eq!(alpha_started.load(Ordering::Acquire), 10);
    assert_eq!(alpha_run.load(Ordering::Acquire), 10);
    assert_eq!(beta_started.load(Ordering::Acquire), 4);
    assert_eq!(beta_run.load(Ordering::Acquire), 0);
    assert_eq!(gamma_started.load(Ordering::Acquire), 0);
    assert_eq!(gamma_run.load(Ordering::Acquire), 0);

    // unblock beta
    beta_blocker.with_lock(|mut guard| {
        *guard = true;
        guard.notify_all();
    });

    sleep(StdDuration::from_millis(500));

    // assert new conditions
    assert_eq!(alpha_started.load(Ordering::Acquire), 10);
    assert_eq!(alpha_run.load(Ordering::Acquire), 10);
    assert_eq!(beta_started.load(Ordering::Acquire), 10);
    assert_eq!(beta_run.load(Ordering::Acquire), 10);
    assert_eq!(gamma_started.load(Ordering::Acquire), 4);
    assert_eq!(gamma_run.load(Ordering::Acquire), 0);

    // unblock gamma
    gamma_blocker.with_lock(|mut guard| {
        *guard = true;
        guard.notify_all();
    });

    sleep(StdDuration::from_millis(500));

    // assert new conditions
    assert_eq!(alpha_started.load(Ordering::Acquire), 10);
    assert_eq!(alpha_run.load(Ordering::Acquire), 10);
    assert_eq!(beta_started.load(Ordering::Acquire), 10);
    assert_eq!(beta_run.load(Ordering::Acquire), 10);
    assert_eq!(gamma_started.load(Ordering::Acquire), 10);
    assert_eq!(gamma_run.load(Ordering::Acquire), 10);

    // close the pool
    let _ = owned.close();
}

struct TooManyBits(MultiChannel<VecDequeChannel>);
impl TooManyBits {
    fn new() -> Self {
        // a multi channel with a count of 100, will consume 100 bits, which is too many
        // the maximum is 64, so that a single atomic bitfield can be used
        TooManyBits(MultiChannel::new(100, VecDequeChannel::new))
    }
}
impl PoolBehavior for TooManyBits {
    type ChannelKey = ();

    fn config(&mut self) -> PoolConfig<Self> {
        PoolConfig {
            threads: 1,
            schedule: ScheduleAlgorithm::HighestFirst,
            levels: vec![
                vec![
                    ChannelParams {
                        key: (),
                        complete_on_close: false
                    }
                ]
            ]
        }
    }

    fn touch_channel<O>(&self, _key: <Self as PoolBehavior>::ChannelKey, mut toucher: impl ChannelToucher<O>) -> O {
        toucher.touch(&self.0)
    }

    fn touch_channel_mut<O>(&mut self, _key: <Self as PoolBehavior>::ChannelKey, mut toucher: impl ChannelToucherMut<O>) -> O {
        toucher.touch_mut(&mut self.0)
    }

    fn followup(&self, _from: <Self as PoolBehavior>::ChannelKey, task: RunningTask) {
        self.0.submit(task);
    }
}

#[test]
fn fail_on_too_many_bits() {
    init_log();
    assert!(OwnedPool::new(TooManyBits::new()).is_err());
}

/// Tests that the timers work, and that the pool properly handles yielding futures.
/// Also tests that the shared channel mechanic works.
#[test]
fn timer_and_yield_test() {
    init_log();
    let owned =
        OwnedPool::new(OneChannelPool {
            thread_count: 4,
            schedule: ScheduleAlgorithm::HighestFirst,
            channel: VecDequeChannel::new().into_shared()
        }).unwrap();
    let scheduler = TimeScheduler::new();

    // create 1000 sleep routines
    for _ in 0..1000 {
        // which sleep for 1 seconds, then sleeps for 1 second 3 times, and execute it after a 1
        // second delay, for a total of 5 seconds
        let scheduler_2 = scheduler.clone();
        let routine = scheduler.after(Duration::seconds(3))
            .and_then(move |()| scheduler_2
                .periodically(SteadyTime::now(), Duration::seconds(1))
                .take(3)
                .for_each(|()| Ok(())));
        scheduler.run_after(Duration::seconds(1), routine, owned.pool.channel.clone());
        //owned.pool.channel.exec(routine);
    }

    // then wait for the futures to complete, by joining the pool
    // time this operation
    let start = SteadyTime::now();
    owned.close().wait().unwrap();
    let end = SteadyTime::now();

    // assert that the time this took was less than 10 seconds
    // allowing a 5 second overhead for only 1000 tasks is pretty generous
    // this is really just to assert that the pool is not blocking on yielding tasks
    assert!((end - start) < Duration::seconds(10));

}

/// Tests that scoped operations work correctly, including when an operation is yielding.
#[test]
fn scoped_op_test() {
    init_log();
    let owned = OwnedPool::new(OneChannelPool {
        thread_count: 4,
        schedule: ScheduleAlgorithm::HighestFirst,
        channel: VecDequeChannel::new()
    }).unwrap();
    let scheduler = TimeScheduler::new();

    let atom = Atomic::new(0usize);
    scoped(|s| {
        for _ in 0..1000 {
            let future = scheduler
                .after(Duration::seconds(1))
                .map(|()| {
                    atom.fetch_add(1, Ordering::SeqCst);
                });
            owned.pool.channel.exec(s.wrap(future));
        }
    });
    let value = atom.load(Ordering::Acquire);

    assert_eq!(value, 1000);
}

mod rrp {
    use ::channel::{BitAssigner, NotEnoughBits};
    use ::StatusBit;
    use ::prelude::setup::*;
    use time::Duration;
    use atomic::*;
    use std::sync::Arc;

    #[derive(Copy, Clone)]
    pub enum RoundRobinPoolKey {
        A,
        B,
        C,
    }

    /// this channel just produces the task of incrementing an atomic integer
    pub struct AtomicIncrChannel {
        pub int: Arc<Atomic<u128>>,
    }
    impl AtomicIncrChannel {
        pub fn new() -> Self {
            AtomicIncrChannel {
                int: Arc::new(Atomic::new(0)),
            }
        }
    }
    impl Channel for AtomicIncrChannel {
        fn assign_bits(&mut self, assigner: &mut BitAssigner) -> Result<(), NotEnoughBits> {
            let mut bit = StatusBit::new();
            assigner.assign(&mut bit)?;
            bit.set(true);
            Ok(())
        }

        fn poll(&self) -> Option<RunningTask> {
            let int = self.int.clone();
            Some(RunningTask::new(run(move || {
                int.fetch_add(1, Ordering::SeqCst);
            })))
        }
    }

    pub struct RoundRobinPool {
        pub a: AtomicIncrChannel,
        pub b: AtomicIncrChannel,
        pub c: AtomicIncrChannel,
    }
    impl RoundRobinPool {
        pub fn new() -> Self {
            RoundRobinPool {
                a: AtomicIncrChannel::new(),
                b: AtomicIncrChannel::new(),
                c: AtomicIncrChannel::new(),
            }
        }
    }
    impl PoolBehavior for RoundRobinPool {
        type ChannelKey = RoundRobinPoolKey;

        fn config(&mut self) -> PoolConfig<Self> {
            PoolConfig {
                threads: 4,
                schedule: ScheduleAlgorithm::RoundRobin(vec![
                    Duration::milliseconds(20),
                    Duration::milliseconds(40),
                    Duration::milliseconds(60),
                ]),
                levels: vec![
                    vec![ChannelParams {
                        key: RoundRobinPoolKey::A,
                        complete_on_close: false,
                    }],
                    vec![ChannelParams {
                        key: RoundRobinPoolKey::B,
                        complete_on_close: false,
                    }],
                    vec![ChannelParams {
                        key: RoundRobinPoolKey::C,
                        complete_on_close: false,
                    }]
                ]
            }
        }

        fn touch_channel<O>(&self, key: RoundRobinPoolKey, mut toucher: impl ChannelToucher<O>) -> O {
            match key {
                RoundRobinPoolKey::A => toucher.touch(&self.a),
                RoundRobinPoolKey::B => toucher.touch(&self.b),
                RoundRobinPoolKey::C => toucher.touch(&self.c),
            }
        }

        fn touch_channel_mut<O>(&mut self, key: RoundRobinPoolKey, mut toucher: impl ChannelToucherMut<O>) -> O {
            match key {
                RoundRobinPoolKey::A => toucher.touch_mut(&mut self.a),
                RoundRobinPoolKey::B => toucher.touch_mut(&mut self.b),
                RoundRobinPoolKey::C => toucher.touch_mut(&mut self.c),
            }
        }

        fn followup(&self, _: RoundRobinPoolKey, _: RunningTask) {
            unreachable!()
        }
    }
}



/// Tests that the round-robin time slicing is more or less correct.
#[test]
fn test_slicing() {
    init_log();
    use self::rrp::*;

    // create the round-robin pool
    let owned = OwnedPool::new(RoundRobinPool::new()).unwrap();

    let pool = owned.pool.clone();

    // wait a bit, then close the pool, which should cancel further tasks
    thread::sleep(StdDuration::from_secs(2));
    owned.close().wait().unwrap();

    // then, check if the time slices are more or less accurate
    let a = pool.a.int.load(Ordering::Acquire);
    let b = pool.b.int.load(Ordering::Acquire);
    let c = pool.c.int.load(Ordering::Acquire);
    let total = a + b + c;
    info!("total tasks completed: {}, {}, {}", a, b, c);

    let a_closeness = (a as f32 / total as f32) / (1.0 / 6.0);
    let b_closeness = (b as f32 / total as f32) / (2.0 / 6.0);
    let c_closeness = (c as f32 / total as f32) / (3.0 / 6.0);

    let check = |n: f32| {
        assert!(n > 0.9, "{} !> 0.9", n);
        assert!(n < 1.1, "{} !< 1.1", n);
    };
    check(a_closeness);
    check(b_closeness);
    check(c_closeness);
}