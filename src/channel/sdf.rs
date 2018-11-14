
use ::{StatusBit, RunningTask};
use super::{Channel, ExecParam, BitAssigner, NotEnoughBits};

use std::sync::Mutex;
use std::collections::BTreeMap;

use smallqueue::SmallQueue;
use time::{Duration, SteadyTime};
use futures::Future;

/// A priority channel where each task corresponds to a time::SteadyTime deadline,
/// and the task with the earliest deadline will be executed first.
pub struct ShortestDeadlineFirst {
    tree: Mutex<BTreeMap<SteadyTime, SmallQueue<RunningTask>>>,
    bit: StatusBit,
}
impl ShortestDeadlineFirst {
    pub fn new() -> Self {
        ShortestDeadlineFirst {
            tree: Mutex::new(BTreeMap::new()),
            bit: StatusBit::new(),
        }
    }
}
impl Channel for ShortestDeadlineFirst {
    fn assign_bits(&mut self, assigner: &mut BitAssigner) -> Result<(), NotEnoughBits> {
        assigner.assign(&mut self.bit)?;
        self.bit.set(!self.tree.get_mut().unwrap().is_empty());
        Ok(())
    }

    fn poll(&self) -> Option<RunningTask> {
        let mut tree = self.tree.lock().unwrap();
        let (remove, elem) = match tree.iter_mut().next() {
            Some((key, mut queue)) => {
                if SteadyTime::now() > *key {
                    warn!("task executing past deadline of {:?}", key);
                }

                let elem = queue.remove();
                let remove = if queue.is_empty() {
                    Some(*key)
                } else {
                    None
                };
                (remove, elem)
            },
            None => (None, None)
        };
        if let Some(key) = remove {
            tree.remove(&key).unwrap();
        }
        self.bit.set(!tree.is_empty());
        elem
    }
}
impl ExecParam for ShortestDeadlineFirst {
    type Param = SteadyTime;

    fn submit(&self, task: RunningTask, deadline: SteadyTime) {
        let mut tree = self.tree.lock().unwrap();
        let into_new_queue: Option<RunningTask> = match tree.get_mut(&deadline) {
            Some(mut queue) => {
                queue.add(task);
                None
            },
            None => Some(task),
        };
        if let Some(task) = into_new_queue {
            tree.insert(deadline, SmallQueue::of(task));
        }
        self.bit.set(!tree.is_empty());
    }
}
impl ShortestDeadlineFirst {
    /// Submit a future, with the deadline equal to a certain duration after the present.
    pub fn exec_after(&self, future: impl Future<Item=(), Error=()> + Send + 'static, delay: Duration) {
        self.exec(future, SteadyTime::now() + delay);
    }
}