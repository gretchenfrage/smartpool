
extern crate rand;
extern crate stopwatch;
extern crate pretty_env_logger;

use ::prelude::*;
use self::rand::prelude::*;
use self::rand::XorShiftRng;
use self::stopwatch::Stopwatch;
use atomic::{Atomic, Ordering};
use time::{Duration, SteadyTime};

mod sdf_pool {
    use ::prelude::setup::*;
    use ::channel::ShortestDeadlineFirst;

    pub struct SdfPool {
        followup: MultiChannel<VecDequeChannel>,
        pub deadline: MultiChannel<ShortestDeadlineFirst>,
    }
    impl Default for SdfPool {
        fn default() -> Self {
            SdfPool {
                followup: MultiChannel::new(16, VecDequeChannel::new),
                deadline: MultiChannel::new(16, ShortestDeadlineFirst::new),
            }
        }
    }
    impl PoolBehavior for SdfPool {
        type ChannelKey = ();

        fn config(&mut self) -> PoolConfig<Self> {
            PoolConfig {
                threads: 1,
                levels: vec![PriorityLevel(vec![ChannelParams {
                    key: (),
                    complete_on_close: false,
                }])]
            }
        }

        fn touch_channel<O>(&self, _: (), mut toucher: impl ChannelToucher<O>) -> O {
            toucher.touch(&self.deadline)
        }

        fn touch_channel_mut<O>(&mut self, _: (), mut toucher: impl ChannelToucherMut<O>) -> O {
            toucher.touch_mut(&mut self.deadline)
        }

        fn followup(&self, _: (), task: RunningTask) {
            self.followup.submit(task)
        }
    }
}

const BATCHES: usize = 100;
const BATCH_SIZE: usize = 1000;

#[test]
fn sdf_test() {
    ::test::init_log();

    use self::sdf_pool::SdfPool;

    let seed = [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15];
    let mut rng = XorShiftRng::from_seed(seed);
    let pool = OwnedPool::new(SdfPool::default()).unwrap();
    let atomic = Atomic::new(0usize);
    let timer = Stopwatch::start_new();
    for batch in 0..BATCHES {
        scoped(|scope| {
            for _ in 0..BATCH_SIZE {
                let op = scope.work(|| {
                    atomic.fetch_add(1, Ordering::SeqCst);
                });
                let delay = Duration::milliseconds((rng.gen::<u64>() % 5000 + 5000) as i64);
                let moment = SteadyTime::now() + delay;
                pool.pool.deadline.exec(op, moment);
            }
        });
        info!("did batch {}", batch);
    }
    let elapsed = timer.elapsed();
    let elapsed = elapsed.subsec_nanos() as u128 + elapsed.as_secs() as u128 * 1000000000;
    //let elapsed = timer.elapsed().subsec_nanos() as u128 + timer;
    let average = elapsed as f64 / (BATCHES * BATCH_SIZE) as f64;
    assert_eq!(atomic.load(Ordering::Acquire), BATCHES * BATCH_SIZE);
    info!("average operation took {} ns", average);
}