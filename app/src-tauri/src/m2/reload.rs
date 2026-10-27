//! Sessions of a page that is gone.
//!
//! Tabs live in the webview only: after a reload (or a crash of the page)
//! no tab can reach the sessions the old page opened, yet they would keep
//! their connections until the app quits: Redis Pub/Sub subscriber
//! connections (which count against the subscription limits), PostgreSQL
//! LISTEN listeners, open transactions and locks. When the main webview
//! starts loading a page, a new page begins: every user session of the
//! previous page is closed (a session still being opened for it is closed
//! as soon as it is open, see `SessionManager::open_for`), and the new page
//! reads what was closed with `reload_report` to tell the user.

use tokio::sync::OwnedMutexGuard;

use crate::commands::Services;
use crate::sessions::{ReloadReport, ReloadedSession, SessionInfo};

/// The user sessions of the previous page, from [`Services::begin_page`].
pub struct UnloadedPage {
    sessions: Vec<SessionInfo>,
    /// Held until they are closed, so `reload_report` waits for them.
    _closing: Option<OwnedMutexGuard<()>>,
}

impl UnloadedPage {
    /// How many sessions the previous page had open.
    #[must_use]
    pub fn len(&self) -> usize {
        self.sessions.len()
    }

    /// True when the previous page had none (the first load).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.sessions.is_empty()
    }
}

impl Services {
    /// A page starts loading: sessions opened from now on belong to it.
    /// Returns the previous page's user sessions, to close with
    /// [`Self::close_unloaded_page`]. Called synchronously from the page
    /// load event, before the new page can open anything.
    #[must_use]
    pub fn begin_page(&self) -> UnloadedPage {
        let (page, closing) = self.sessions.begin_page();
        UnloadedPage {
            sessions: self.sessions.sessions_before_page(page),
            _closing: closing,
        }
    }

    /// Closes the previous page's sessions; returns how many were closed.
    /// Each one's open transaction (rolled back by the close) is recorded
    /// for [`Self::reload_report`].
    pub async fn close_unloaded_page(&self, page: UnloadedPage) -> usize {
        let mut closed = Vec::new();
        for info in &page.sessions {
            if self.sessions.info(&info.id).is_err() {
                continue;
            }
            let in_transaction = self.sessions.in_transaction_now(&info.id).await;
            match self.close_session(&info.id).await {
                Ok(()) => closed.push(ReloadedSession {
                    connection_id: info.connection_id.clone(),
                    in_transaction,
                }),
                Err(error) => {
                    tracing::warn!(%error, "could not close a session of the unloaded page");
                }
            }
        }
        let count = closed.len();
        self.sessions.record_reload(closed);
        drop(page);
        count
    }

    /// The profiles of user sessions with a transaction open now (one
    /// entry per session; a session running a statement is not counted).
    /// The UI asks before a reload when there are any.
    pub async fn open_transactions(&self) -> Vec<String> {
        let mut open = Vec::new();
        for info in self.sessions.list() {
            if self.sessions.in_transaction_now(&info.id).await == Some(true) {
                open.push(info.connection_id);
            }
        }
        open
    }

    /// What the last reload closed (emptied by the call), waiting while
    /// the previous page's sessions are still being closed. The new page
    /// shows it once.
    pub async fn reload_report(&self) -> ReloadReport {
        self.sessions.take_reload_report().await
    }
}
