
use futures::{Future, Async};

/// A simple future which performs a computation upon polling which cannot fail.
pub struct Run<O, F: FnOnce() -> O> {
    work: Option<F>
}
impl<O, F: FnOnce() -> O> Future for Run<O, F> {
    type Item = O;
    type Error = ();

    fn poll(&mut self) -> Result<Async<<Self as Future>::Item>, <Self as Future>::Error> {
        Ok(Async::Ready((self.work.take().unwrap())()))
    }
}

pub fn run<O, F: FnOnce() -> O>(work: F) -> Run<O, F> {
    Run {
        work: Some(work)
    }
}

/// A simple future which performs a computation upon polling which can fail.
pub struct TryRun<O, E, F: FnOnce() -> Result<O, E>> {
    work: Option<F>
}
impl<O, E, F: FnOnce() -> Result<O, E>> Future for TryRun<O, E, F> {
    type Item = O;
    type Error = E;

    fn poll(&mut self) -> Result<Async<<Self as Future>::Item>, <Self as Future>::Error> {
        (self.work.take().unwrap())().map(|item| Async::Ready(item))
    }
}

pub fn try<O, E, F: FnOnce() -> Result<O, E>>(work: F) -> TryRun<O, E, F> {
    TryRun {
        work: Some(work)
    }
}