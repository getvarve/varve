//! Cancelling an `EXPLAIN` as a whole.
//!
//! One [`super::explain`] takes several driver calls (the check, the run,
//! a savepoint and its rollback). The session's cancel handle stops only
//! the statement that is running when it arrives: a cancel between two
//! calls reaches nothing, and the next call (the run itself) would start
//! uncancelled. An [`ExplainCancel`] is sticky instead: once cancelled,
//! every later step of the explain it was given to is refused before it is
//! sent, and a step that is running is cancelled through the session's
//! cancel handle, again every [`RESEND_INTERVAL`] until it ends (a cancel
//! that reaches the server before the statement does is lost). The steps
//! that undo what the explain opened (a rollback) still run.

use std::any::Any;
use std::future::{Future, poll_fn};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::task::{Poll, Waker};
use std::time::Duration;

use varve_driver::{
    CancelHandle, DriverError, ErrorKind, ExecOptions, ExecOutput, RanStatement, ServerInfo,
    Session,
};

/// How often a running step is cancelled again while it has not ended.
pub const RESEND_INTERVAL: Duration = Duration::from_millis(100);

/// Cancels one [`super::explain_with_cancel`] call as a whole, from any
/// task: clone it, hand one clone to the call and call
/// [`ExplainCancel::cancel`] on another. Cancelling before the call starts
/// stops it before it sends anything.
#[derive(Debug, Clone, Default)]
pub struct ExplainCancel {
    shared: Arc<Shared>,
}

#[derive(Debug, Default)]
struct Shared {
    cancelled: AtomicBool,
    /// The task of the step that is running, woken by a cancel.
    waker: Mutex<Option<Waker>>,
}

impl ExplainCancel {
    /// A handle that is not cancelled.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Cancels the explain: the running step is cancelled and no later
    /// step starts.
    pub fn cancel(&self) {
        self.shared.cancelled.store(true, Ordering::SeqCst);
        let waker = self
            .shared
            .waker
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        if let Some(waker) = waker {
            waker.wake();
        }
    }

    /// True once [`ExplainCancel::cancel`] was called.
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.shared.cancelled.load(Ordering::SeqCst)
    }

    /// Registers `waker` to be woken by a cancel; true when already
    /// cancelled.
    fn watch(&self, waker: &Waker) -> bool {
        let mut slot = self
            .shared
            .waker
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if self.is_cancelled() {
            return true;
        }
        match slot.as_ref() {
            Some(current) if current.will_wake(waker) => {}
            _ => *slot = Some(waker.clone()),
        }
        false
    }
}

/// The error of a step refused because the explain was cancelled.
fn cancelled() -> DriverError {
    DriverError::new(
        ErrorKind::Cancelled,
        "the EXPLAIN was cancelled; its next step was not run",
    )
}

type Sending = Pin<Box<dyn Future<Output = Result<(), DriverError>> + Send>>;

/// A session whose every call honours an [`ExplainCancel`]: refused once
/// it is cancelled, and cancelled (repeatedly) while it runs.
pub(super) struct Guarded<'a> {
    inner: &'a dyn Session,
    cancel: &'a ExplainCancel,
}

impl<'a> Guarded<'a> {
    pub(super) fn new(inner: &'a dyn Session, cancel: &'a ExplainCancel) -> Self {
        Self { inner, cancel }
    }
}

#[async_trait::async_trait]
impl Session for Guarded<'_> {
    fn server_info(&self) -> &ServerInfo {
        self.inner.server_info()
    }

    fn cancel_handle(&self) -> Arc<dyn CancelHandle> {
        self.inner.cancel_handle()
    }

    async fn execute(&self, sql: &str, options: &ExecOptions) -> Result<ExecOutput, DriverError> {
        if self.cancel.is_cancelled() {
            return Err(cancelled());
        }
        let handle = self.inner.cancel_handle();
        let mut call = self.inner.execute(sql, options);
        let mut sending: Option<Sending> = None;
        // The next resend, once cancelled.
        let mut resend: Option<Pin<Box<tokio::time::Sleep>>> = None;
        poll_fn(|cx| {
            if let Poll::Ready(result) = call.as_mut().poll(cx) {
                return Poll::Ready(result);
            }
            if !self.cancel.watch(cx.waker()) {
                return Poll::Pending;
            }
            // Cancelled while this step runs: cancel it now and again every
            // RESEND_INTERVAL until it ends.
            let send = match resend.as_mut() {
                None => {
                    resend = Some(Box::pin(tokio::time::sleep(RESEND_INTERVAL)));
                    true
                }
                Some(timer) => {
                    if timer.as_mut().poll(cx).is_ready() {
                        timer
                            .as_mut()
                            .reset(tokio::time::Instant::now() + RESEND_INTERVAL);
                        sending.is_none()
                    } else {
                        false
                    }
                }
            };
            if let Some(timer) = resend.as_mut() {
                // Registers the task with the timer (a no-op when due).
                let _ = timer.as_mut().poll(cx);
            }
            if send {
                let handle = Arc::clone(&handle);
                sending = Some(Box::pin(async move { handle.cancel().await }));
            }
            if let Some(future) = sending.as_mut()
                && future.as_mut().poll(cx).is_ready()
            {
                sending = None;
            }
            Poll::Pending
        })
        .await
    }

    fn last_statements(&self) -> Option<Vec<RanStatement>> {
        self.inner.last_statements()
    }

    fn in_transaction(&self) -> Option<bool> {
        self.inner.in_transaction()
    }

    async fn ping(&self) -> Result<(), DriverError> {
        self.inner.ping().await
    }

    async fn close(self: Box<Self>) -> Result<(), DriverError> {
        // The session belongs to the caller of the explain.
        Ok(())
    }

    fn as_any(&self) -> &dyn Any {
        // Engine-specific features downcast to the driver's own session.
        self.inner.as_any()
    }
}
