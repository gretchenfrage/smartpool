

use run;

use atomicmonitor::{AtomMonitor, Ordering};

use std::mem;
use std::marker::PhantomData;
use std::sync::Arc;
use std::sync::atomic::fence;

use futures::Future;

/// Enter a batch of scoped operations, which allows non-static futures (which reference the stack
/// frame) to be submitted to a threadpool in a memory-safe way, by preventing the calling stack
/// frame from exiting until all futures in this scoped batch are completed.
pub unsafe fn scoped<'env, R>(operation: impl FnOnce(&Scope<'env>) -> R) -> R {
    // create scope
    let scope = Scope {
        running_count: Arc::new(AtomMonitor::new(0)),
        not_static: PhantomData,
        not_sendsync: PhantomData,
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
    not_static: PhantomData<&'env ()>,
    not_sendsync: PhantomData<*const ()>,
}
impl<'env> Scope<'env> {
    /// Wrap a non-static future into a static-future (which will block this scoped batch
    /// stack frame), allowing that future to be submitted to a threadpool.
    #[must_use]
    pub fn wrap<'scope, F: Future<Item=(), Error=()> + Send + 'env>(
        &'scope self, future_factory: impl FnOnce() -> F)
        -> Box<dyn Future<Item=(), Error=()> + Send + 'static> {
        // if we don't fence, then when optimizations are enabled, strange things will happen
        // with regard to caching captured local variables which are copy
        fence(Ordering::SeqCst);

        // now that we've fenced, we can create the future
        let future = future_factory();

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
    pub fn work<'scope>(&'scope self, work: impl FnOnce() + Send + 'env)
                        -> Box<dyn Future<Item=(), Error=()> + Send + 'static> {
        self.wrap(move || run::run(work))
    }
}