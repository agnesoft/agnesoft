use std::sync::Arc;
use std::sync::Condvar;
use std::sync::Mutex;
use std::sync::PoisonError;

/// [internal]
///
/// Holds the runs of a test double until it is opened, with a failure `F`
/// or without one. Clones share the gate. Waiting blocks the thread, or
/// only the task when async.
pub(crate) struct Gate<F>(Arc<GateState<F>>);

struct GateState<F> {
    opened: Mutex<Option<Option<F>>>,
    condvar: Condvar,
    #[cfg(feature = "tokio")]
    notify: tokio::sync::Notify,
}

impl<F: Clone> Gate<F> {
    pub(crate) fn new() -> Self {
        Self(Arc::new(GateState {
            opened: Mutex::new(None),
            condvar: Condvar::new(),
            #[cfg(feature = "tokio")]
            notify: tokio::sync::Notify::new(),
        }))
    }

    /// Lets the held and later runs through with `failure`. Does nothing if
    /// the gate was opened already.
    pub(crate) fn open(&self, failure: Option<F>) {
        self.state().get_or_insert(failure);
        self.0.condvar.notify_all();
        #[cfg(feature = "tokio")]
        self.0.notify.notify_waiters();
    }

    /// Blocks until the gate is opened, returning its failure.
    pub(crate) fn wait(&self) -> Option<F> {
        self.0
            .condvar
            .wait_while(self.state(), |opened| opened.is_none())
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
            .flatten()
    }

    /// Waits until the gate is opened, returning its failure.
    #[cfg(feature = "tokio")]
    pub(crate) async fn wait_async(&self) -> Option<F> {
        let mut notified = std::pin::pin!(self.0.notify.notified());
        notified.as_mut().enable();

        if self.state().is_none() {
            notified.await;
        }

        self.state().clone().flatten()
    }

    fn state(&self) -> std::sync::MutexGuard<'_, Option<Option<F>>> {
        self.0.opened.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl<F> Clone for Gate<F> {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}

/// [internal]
///
/// A [`Gate`] held by a test, opened without a failure once the test drops
/// all its clones, e.g. while unwinding from a failed assertion, so that
/// the runs it holds cannot hang the test. Clones share the gate.
pub(crate) struct HeldGate<F: Clone>(Arc<Opener<F>>);

struct Opener<F: Clone>(Gate<F>);

impl<F: Clone> HeldGate<F> {
    pub(crate) fn new() -> Self {
        Self(Arc::new(Opener(Gate::new())))
    }

    pub(crate) fn gate(&self) -> &Gate<F> {
        &self.0.0
    }
}

impl<F: Clone> Clone for HeldGate<F> {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}

impl<F: Clone> Drop for Opener<F> {
    fn drop(&mut self) {
        self.0.open(None);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    mod opener {
        use super::*;

        mod drop {
            use super::*;

            #[test]
            fn drop_should_open_gate_once_all_clones_are_dropped() {
                let held = HeldGate::<()>::new();
                let clone = held.clone();
                let gate = held.gate().clone();

                drop(held);
                let opened_by_first = gate.state().is_some();
                drop(clone);

                assert!(!opened_by_first);
                assert_eq!(gate.wait(), None);
            }
        }
    }
}
