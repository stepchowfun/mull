use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

// This flag lets a caller stop a long-running computation between units of work.
#[derive(Clone, Debug, Default)]
pub struct CancellationFlag(Arc<AtomicBool>);

impl CancellationFlag {
    // Request that the computations holding this flag stop at their next opportunity.
    pub fn cancel(&self) {
        self.0.store(true, Ordering::Relaxed);
    }

    // Determine whether cancellation was requested. Relaxed ordering suffices because the flag
    // guards no other data and observing it late only costs one more unit of work.
    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Relaxed)
    }
}

// This type distinguishes a computation which ran to completion from one which stopped early.
#[derive(Debug)]
pub enum Outcome<T> {
    Completed(T),
    Cancelled,
}

impl<T> Outcome<T> {
    // Transform the value of a completed computation, preserving cancellation.
    pub fn map<U, F: FnOnce(T) -> U>(self, function: F) -> Outcome<U> {
        match self {
            Self::Completed(value) => Outcome::Completed(function(value)),
            Self::Cancelled => Outcome::Cancelled,
        }
    }

    // Retrieve the value of a computation which was given a flag that is never set.
    pub fn assume_completed(self) -> T {
        match self {
            Self::Completed(value) => value,
            Self::Cancelled => {
                unreachable!("a computation without cancellation should not be cancelled")
            }
        }
    }
}
