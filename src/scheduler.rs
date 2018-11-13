
use std::collections::BTreeMap;
use std::sync::Arc;
use std::thread;
use std::mem;

use RunningTask;
use channel::{Exec, ExecParam};

use monitor::Monitor;
use time::{Duration, SteadyTime};
use futures::{task, Future, Stream, Async};
use futures::task::Task;

/// A thread that schedules events to occur in the future.
#[derive(Clone)]
pub struct Scheduler {
    tasks: Arc<Monitor<BTreeMap<SteadyTime, ScheduledEvent>>>
}
impl Scheduler {
    pub fn new() -> Self {
        let scheduler = Scheduler {
            tasks: Arc::new(Monitor::new(BTreeMap::new()))
        };
        let tasks = scheduler.tasks.clone();
        thread::spawn(move || tasks.with_lock(|mut guard| {
            loop {
                if let Some(time) = guard.iter()
                    .next()
                    .map(|(time, _)| *time)
                    .filter(|time| *time <= SteadyTime::now()) {

                    guard.remove(&time).unwrap().run();
                } else if let Some((time, delay)) = guard.iter()
                    .next()
                    .map(|(time, _)| *time)
                    .map(|time| (time, time - SteadyTime::now()))
                    .map(|(time, delay)| match delay.to_std() {
                        Ok(delay) => (time, delay),
                        Err(_) => (time, Duration::zero().to_std().unwrap())
                    }) {

                    if guard.wait_timeout(delay).timed_out() {
                        guard.remove(&time).unwrap().run();
                    }
                } else {
                    guard.wait();
                }
            }
        }));
        scheduler
    }

    /// Insert an event to be executed. Safely handles the condition of multiple events
    /// scheduled for the same time.
    fn insert(&self, time: SteadyTime, event: ScheduledEvent) {
        self.tasks.with_lock(|mut tasks| {
            if let Some(to_insert) = match tasks.get_mut(&time) {
                None => {
                    Some(event)
                },
                Some(&mut ScheduledEvent::Multiple(ref mut vec)) => {
                    vec.push(event);
                    None
                },
                Some(a) => {
                    // turn the single scheduled event into a variant containing a vector of both
                    let mut b = ScheduledEvent::Multiple(Vec::new());
                    mem::swap( a, &mut b);
                    if let &mut ScheduledEvent::Multiple(ref mut vec) = a {
                        vec.push(b);
                    } else {
                        unreachable!()
                    }
                    None
                }
            } {
                tasks.insert(time, to_insert);
            }
            tasks.notify_all();
        })
    }

    /// A future that occurs at the given moment.
    pub fn at(&self, moment: SteadyTime) -> FutureMoment {
        FutureMoment {
            scheduler: Some(self.clone()),
            moment
        }
    }

    /// A future that occurs after the given delay.
    pub fn after(&self, delay: Duration) -> FutureMoment {
        FutureMoment {
            scheduler: Some(self.clone()),
            moment: SteadyTime::now() + delay
        }
    }

    /// A stream that produces values on a set interval.
    pub fn periodically(&self, start: SteadyTime, period: Duration) -> PeriodicMoments {
        PeriodicMoments {
            scheduler: self.clone(),
            current: start,
            period
        }
    }

    /// Run a future in an executor at a moment in the future.
    pub fn run_at(
        &self,
        moment: SteadyTime,
        future: impl Future<Item=(), Error=()> + Send + 'static,
        exec: impl Exec + Send + 'static,
    ) {
        self.insert(moment, ScheduledEvent::Submit(Box::new(SubmitExec {
            task: RunningTask::new(future),
            exec
        })));
    }

    /// Run a future in an executor with a parameter at a moment in the future.
    pub fn run_at_param<P: Send + 'static>(
        &self,
        moment: SteadyTime,
        future: impl Future<Item=(), Error=()> + Send + 'static,
        exec: impl ExecParam<Param=P> + Send + 'static,
        param: P,
    ) {
        self.insert(moment, ScheduledEvent::Submit(Box::new(SubmitExecParam {
            task: RunningTask::new(future),
            exec,
            param
        })))
    }

    /// Run a future in an executor after a delay.
    pub fn run_after(
        &self,
        delay: Duration,
        future: impl Future<Item=(), Error=()> + Send + 'static,
        exec: impl Exec + Send + 'static,
    ) {
        self.run_at(SteadyTime::now() + delay, future, exec);
    }

    /// Run a future in an executor with a parameter after a delay.
    pub fn run_after_param<P: Send + 'static>(
        &self,
        delay: Duration,
        future: impl Future<Item=(), Error=()> + Send + 'static,
        exec: impl ExecParam<Param=P> + Send + 'static,
        param: P,
    ) {
        self.run_at_param(SteadyTime::now() + delay, future, exec, param);
    }
}

/// An event that is scheduled to execute in the future.
enum ScheduledEvent {
    Notify(Task),
    Submit(Box<dyn Submit>),
    Multiple(Vec<ScheduledEvent>),
}
impl ScheduledEvent {
    fn run(self) {
        match self {
            ScheduledEvent::Notify(task) => task.notify(),
            ScheduledEvent::Submit(submit) => submit.submit(),
            ScheduledEvent::Multiple(events) => events.into_iter()
                .for_each(|event| event.run()),
        };
    }
}

/// Trait object for task to be submitted to executor.
trait Submit: Send + 'static {
    fn submit(self: Box<Self>);
}

/// Submit implementation.
struct SubmitExec<E: Exec + Send + 'static> {
    task: RunningTask,
    exec: E,
}
impl<E: Exec + Send + 'static> Submit for SubmitExec<E> {
    fn submit(self: Box<Self>) {
        let unboxed = *self;
        let SubmitExec {
            task,
            exec
        } = unboxed;
        exec.submit(task);
    }
}

/// Submit implementation with param.
struct SubmitExecParam<E: ExecParam + Send + 'static> {
    task: RunningTask,
    exec: E,
    param: E::Param,
}
impl<E: ExecParam + Send + 'static> Submit for SubmitExecParam<E>
    where E::Param: Send + 'static {

    fn submit(self: Box<Self>) {
        let unboxed = *self;
        let SubmitExecParam {
            task,
            exec,
            param
        } = unboxed;
        exec.submit(task, param);
    }
}

/// A future that is scheduled to occur at a moment in the future.
pub struct FutureMoment {
    scheduler: Option<Scheduler>,
    moment: SteadyTime,
}
impl Future for FutureMoment {
    type Item = ();
    type Error = ();

    fn poll(&mut self) -> Result<Async<<Self as Future>::Item>, <Self as Future>::Error> {
        if SteadyTime::now() >= self.moment {
            Ok(Async::Ready(()))
        } else {
            if let Some(scheduler) = self.scheduler.take() {
                scheduler.insert(self.moment, ScheduledEvent::Notify(task::current()));
            }
            Ok(Async::NotReady)
        }
    }
}

/// A stream that produces values at a continuous interval.
pub struct PeriodicMoments {
    scheduler: Scheduler,
    current: SteadyTime,
    period: Duration,
}
impl Stream for PeriodicMoments {
    type Item = ();
    type Error = ();

    fn poll(&mut self) -> Result<Async<Option<<Self as Stream>::Item>>, <Self as Stream>::Error> {
        if self.current <= SteadyTime::now() {
            self.current = self.current + self.period;
            Ok(Async::Ready(Some(())))
        } else {
            self.scheduler.insert(self.current,ScheduledEvent::Notify(task::current()));
            self.scheduler.insert(self.current,ScheduledEvent::Notify(task::current()));
            Ok(Async::NotReady)
        }
    }
}