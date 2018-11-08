
use ::{StatusBit, RunningTask};
use super::{Channel, ExecParam, BitAssigner, NotEnoughBits};

use std::sync::{Mutex, Arc};

use manhattan_tree::collection::MTreeQueue;
use manhattan_tree::space::{CoordSpace, ZeroCoord};
use atomicmonitor::atomic::{Atomic, Ordering};

pub struct MTreeChannel<S: CoordSpace> where S::Coord: Copy {
    pub focus: Arc<Atomic<S::Coord>>,
    tree: Mutex<MTreeQueue<RunningTask, S>>,
    bit: StatusBit,
}
impl<S: CoordSpace> MTreeChannel<S> where S::Coord: Copy  {
    pub fn new(focus: Arc<Atomic<S::Coord>>, space: S) -> Self {
        MTreeChannel {
            focus,
            tree: Mutex::new(MTreeQueue::new(space)),
            bit: StatusBit::new(),
        }
    }
}
impl<S: CoordSpace + Default> Default for MTreeChannel<S> where S::Coord: Copy + ZeroCoord {
    fn default() -> Self {
        Self::new(Arc::new(Atomic::new(S::Coord::zero_coord())), S::default())
    }
}
impl<S: CoordSpace> Channel for MTreeChannel<S> where S::Coord: Copy {
    fn assign_bits(&mut self, assigner: &mut BitAssigner) -> Result<(), NotEnoughBits> {
        assigner.assign(&mut self.bit)?;
        self.bit.set(!self.tree.get_mut().unwrap().is_empty());
        Ok(())
    }

    fn poll(&self) -> Option<RunningTask> {
        let mut tree = self.tree.lock().unwrap();
        let future = tree.remove(self.focus.load(Ordering::Relaxed));
        self.bit.set(!tree.is_empty());
        future
    }
}
impl<S: CoordSpace> ExecParam for MTreeChannel<S> where S::Coord: Copy {
    type Param = S::Coord;

    fn submit(&self, task: RunningTask, param: S::Coord) {
        let mut tree = self.tree.lock().unwrap();
        tree.insert(param, task);
        self.bit.set(true);
    }
}