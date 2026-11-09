//! Sign-in state of a driver that logs in to a service with stored credentials (the cloud
//! clients), for the dashboard, `/api/cloud` and the `auth` entity.

use serde::{Deserialize, Serialize};

/// How a driver's sign-in is doing. Never holds a secret.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuthStatus {
    /// `ok`, `aanmelden nodig` (it cannot sign in by itself: set new credentials),
    /// `geblokkeerd` (stopped after repeated rejected logins), `wacht` (a login is due after a
    /// failure that was not a rejection) or `onbekend` (nothing tried yet).
    pub state: String,
    /// Where the credentials come from: `state` (captured with `energie credentials set`) or
    /// `etc` (the credentials file).
    pub credentials: String,
    /// A password login is possible with these credentials.
    pub can_login: bool,
    /// The last use of the session (token or cookies) worked.
    pub session_ok: bool,
    /// When the last password login was tried (RFC 3339).
    pub last_login: Option<String>,
    /// `ok` or why the last password login failed.
    pub last_result: Option<String>,
    /// Rejected password logins in a row.
    pub rejected_logins: u32,
    /// No more password logins until the credentials change.
    pub blocked: bool,
    /// The earliest next password login (RFC 3339), while waiting.
    pub next_login: Option<String>,
}
