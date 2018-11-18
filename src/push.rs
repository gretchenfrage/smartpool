


use futures::prelude::*;
use futures::task::Task;
use futures::task;
use std::sync::{Arc, Mutex};
use through::*;

/// The receiving end of a push future, which can be composed. The sending
/// end will push the value to it, once the sending end is executed.
pub struct PushFutureRecv<I, E> {
    interior: Arc<Mutex<PushFutureInterior<I, E>>>,
}

/// The sending end of a push future, which must be submitted to a threadpool,
/// and will push the value to the receiving end.
pub struct PushFutureSend<I, E, F: Future<Item=I, Error=E>> {
    source: F,
    interior: Arc<Mutex<PushFutureInterior<I, E>>>,
}
enum PushFutureInterior<I, E> {
    Available {
        elem: Result<I, E>,
    },
    Unavailable {
        listeners: Vec<Task>,
    },
    Taken
}

impl<I, E> Future for PushFutureRecv<I, E> {
    type Item = I;
    type Error = E;

    fn poll(&mut self) -> Poll<I, E> {
        let mut interior = self.interior.lock().unwrap();
        through_and(&mut *interior, |interior| match interior {
            PushFutureInterior::Available {
                elem
            } => {
                let result = match elem {
                    Ok(item) => Ok(Async::Ready(item)),
                    Err(error) => Err(error),
                };
                (PushFutureInterior::Taken, result)
            },
            PushFutureInterior::Unavailable {
                mut listeners
            } => {
                listeners.push(task::current());
                (PushFutureInterior::Unavailable {
                    listeners
                }, Ok(Async::NotReady))
            },
            PushFutureInterior::Taken => {
                panic!("take from a PushFutureRecv twice")
            }
        })
    }
}

impl<I, E, F: Future<Item=I, Error=E>> Future for PushFutureSend<I, E, F> {
    type Item = ();
    type Error = ();

    fn poll(&mut self) -> Poll<(), ()> {
        let found: Option<Result<I, E>> = match self.source.poll() {
            Ok(Async::Ready(elem)) => Some(Ok(elem)),
            Err(error) => Some(Err(error)),
            Ok(Async::NotReady) => None
        };
        match found {
            Some(result) => {
                let mut interior = self.interior.lock().unwrap();
                through(&mut *interior, |interior| match interior {
                    PushFutureInterior::Unavailable {
                        listeners
                    } => {
                        for listener in listeners {
                            listener.notify();
                        }
                        PushFutureInterior::Available {
                            elem: result
                        }
                    },
                    _ => panic!("PushFutureSend: PushFutureInterior in invalid state")
                });
                Ok(Async::Ready(()))
            },
            None => Ok(Async::NotReady)
        }
    }
}

/// Push futures are futures based on interior mutability and shared memory, where one
/// tasks produces a value, and sends it to another task. This is generally less
/// efficient that the normal way in which Rust's futures ecosystem occurs. The purpose
/// of this module is not to become a normal way of executing futures. Rather, this
/// module is created to allow for tasks which execute across several channels, or
/// even several pools, or with internal forking and concurrency.
///
/// Instead of using this module directly, simply use the `exec_push` method in
/// `Exec` and `ExecParam`.
pub fn push_future<I, E, F: Future<Item=I, Error=E>>(source: F) -> (PushFutureSend<I, E, F>, PushFutureRecv<I, E>) {
    let interior = PushFutureInterior::Unavailable {
        listeners: Vec::new()
    };
    let interior = Arc::new(Mutex::new(interior));
    (PushFutureSend {
        source,
        interior: interior.clone(),
    }, PushFutureRecv {
        interior
    })
}