
use run;

use atomicmonitor::{AtomMonitor, Ordering};

use std::mem;
use std::marker::PhantomData;
use std::sync::Arc;

use futures::Future;

/// Enter a batch of scoped operations, which allows non-static futures (which reference the stack
/// frame) to be submitted to a threadpool in a memory-safe way, by preventing the calling stack
/// frame from exiting until all futures in this scoped batch are completed.
pub fn scoped<'env, R>(operation: impl FnOnce(&Scope<'env>) -> R) -> R {
    // create scope
    let scope = Scope {
        running_count: Arc::new(AtomMonitor::new(0)),
        phantom: PhantomData,
    };

    // trigger the futures
    let result = operation(&scope);

    // wait for future completion
    scope.running_count.wait_until(|count| count == 0);

    // done
    result
}

/// A handle to a batch of scoped operations, which allow non-static futures to be wrapped into
/// static futures, which prevent this the batch stack frame from exiting until they complete.
pub struct Scope<'env> {
    running_count: Arc<AtomMonitor<usize>>,
    phantom: PhantomData<&'env ()>,
}
impl<'env> !Send for Scope<'env> {}
impl<'env> !Sync for Scope<'env> {}
impl<'env> Scope<'env> {
    /// Wrap a non-static future into a static-future (which will block this scoped batch
    /// stack frame), allowing that future to be submitted to a threadpool.
    #[must_use]
    pub fn wrap<'scope>(&'scope self, future: impl Future<Item=(), Error=()> + Send + 'env)
        -> Box<dyn Future<Item=(), Error=()> + Send + 'static> {

        self.running_count.mutate(|count| {
            count.fetch_add(1, Ordering::SeqCst);
        });

        let running_count = self.running_count.clone();
        let future = future
            .then(move |_| {
                running_count.mutate(|count| {
                    count.fetch_sub(1, Ordering::SeqCst);
                });
                Ok(())
            });

        let future: Box<dyn Future<Item=(), Error=()> + Send + 'env> =
            Box::new(future);
        let future: Box<dyn Future<Item=(), Error=()> + Send + 'static> =
            unsafe { mem::transmute(future) };

        future
    }

    /// A convenience utility for wrapping some pure, non-blocking work.
    #[must_use]
    pub fn work<'scope>(&'scope self, work: impl FnOnce() -> () + Send + 'env)
                        -> Box<dyn Future<Item=(), Error=()> + Send + 'static> {

        self.wrap(run::run(work))
    }
}