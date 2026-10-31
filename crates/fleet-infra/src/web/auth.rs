//! Authentication state of the dashboard: the per-start token, browser sessions with their CSRF
//! tokens, and the limiter for failed attempts.
//!
//! All time-dependent methods take the current `Instant`, so tests control the clock.
//!
//! * The **web token** is 256 random bits generated when the daemon starts, shown once on the
//!   operator's terminal and held only in memory. It is never written to the state directory, the
//!   event log or any log, and workers never receive it.
//! * A **session** is created by exchanging the token (`POST /api/login`); the browser keeps only a
//!   random session id in an `HttpOnly; SameSite=Strict` cookie. Every session has its own CSRF
//!   token, which the page must send in a header with every state-changing request.
//! * Failed credentials are counted globally (every client is a process on this machine, so there
//!   is no useful per-address key): too many failures lock authentication for a while.

use crate::statedir::{generate_token, tokens_equal};
use fleet_domain::error::Result;
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

pub const COOKIE_NAME: &str = "fleet_session";

#[derive(Clone, Copy, Debug)]
pub struct AuthLimits {
    pub max_sessions: usize,
    /// A session ends this long after it was created, whatever its activity.
    pub session_lifetime: Duration,
    /// A session ends this long after its last request.
    pub session_idle: Duration,
    /// This many failed credentials within `failure_window` lock authentication.
    pub max_failures: usize,
    pub failure_window: Duration,
    pub lockout: Duration,
}

impl Default for AuthLimits {
    fn default() -> AuthLimits {
        AuthLimits {
            max_sessions: 16,
            session_lifetime: Duration::from_secs(12 * 3600),
            session_idle: Duration::from_secs(2 * 3600),
            max_failures: 5,
            failure_window: Duration::from_secs(60),
            lockout: Duration::from_secs(60),
        }
    }
}

struct Session {
    id: String,
    csrf: String,
    created: Instant,
    last_seen: Instant,
}

#[derive(Default)]
struct State {
    sessions: Vec<Session>,
    failures: Vec<Instant>,
    locked_until: Option<Instant>,
}

/// What a request presented as credentials.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Presented<'a> {
    Nothing,
    Bearer(&'a str),
    Cookie(&'a str),
}

/// The outcome of checking credentials.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Verdict {
    /// The bearer token was right: no ambient credentials, so no CSRF token is needed.
    Bearer,
    /// A live session; the caller must still compare the CSRF token for state-changing requests.
    Session { csrf: String },
    /// No credentials were presented. Not a failure.
    Missing,
    /// Wrong or expired credentials.
    Rejected,
    /// Locked out; retry after this many seconds. Nothing was checked.
    Locked { retry_after_secs: u64 },
}

pub struct WebAuth {
    token: String,
    limits: AuthLimits,
    state: Mutex<State>,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

impl WebAuth {
    /// A new authenticator with a fresh random token.
    pub fn new(limits: AuthLimits) -> Result<WebAuth> {
        Ok(WebAuth::with_token(generate_token()?, limits))
    }

    pub fn with_token(token: String, limits: AuthLimits) -> WebAuth {
        WebAuth {
            token,
            limits,
            state: Mutex::new(State::default()),
        }
    }

    /// The secret. Only the daemon's start-up code prints it, once.
    pub fn token(&self) -> &str {
        &self.token
    }

    /// Checks `presented` credentials. A wrong credential counts as a failure; presenting nothing
    /// does not.
    pub fn check(&self, presented: Presented<'_>, now: Instant) -> Verdict {
        let mut st = lock(&self.state);
        if let Some(retry) = self.locked(&mut st, now) {
            if presented != Presented::Nothing {
                return Verdict::Locked {
                    retry_after_secs: retry,
                };
            }
        }
        match presented {
            Presented::Nothing => Verdict::Missing,
            Presented::Bearer(t) => {
                if tokens_equal(t, &self.token) {
                    Verdict::Bearer
                } else {
                    self.fail(&mut st, now)
                }
            }
            Presented::Cookie(id) => {
                self.expire(&mut st, now);
                // Compared against every session in constant time per comparison, so the lookup
                // does not reveal how much of an id matches.
                let mut found: Option<usize> = None;
                for (i, s) in st.sessions.iter().enumerate() {
                    if tokens_equal(id, &s.id) {
                        found = Some(i);
                    }
                }
                match found.and_then(|i| st.sessions.get_mut(i)) {
                    Some(s) => {
                        s.last_seen = now;
                        Verdict::Session {
                            csrf: s.csrf.clone(),
                        }
                    }
                    None => self.fail(&mut st, now),
                }
            }
        }
    }

    /// Exchanges the token for a new session: its id (the cookie value) and its CSRF token.
    /// Returns `Err(verdict)` with `Rejected` or `Locked` otherwise.
    pub fn login(
        &self,
        token: &str,
        now: Instant,
    ) -> std::result::Result<(String, String), Verdict> {
        let mut st = lock(&self.state);
        if let Some(retry) = self.locked(&mut st, now) {
            return Err(Verdict::Locked {
                retry_after_secs: retry,
            });
        }
        if !tokens_equal(token, &self.token) {
            return Err(self.fail(&mut st, now));
        }
        self.expire(&mut st, now);
        let (Ok(id), Ok(csrf)) = (generate_token(), generate_token()) else {
            return Err(Verdict::Rejected);
        };
        while st.sessions.len() >= self.limits.max_sessions.max(1) {
            // The oldest session makes room.
            st.sessions.remove(0);
        }
        st.sessions.push(Session {
            id: id.clone(),
            csrf: csrf.clone(),
            created: now,
            last_seen: now,
        });
        Ok((id, csrf))
    }

    pub fn logout(&self, id: &str) {
        let mut st = lock(&self.state);
        st.sessions.retain(|s| !tokens_equal(id, &s.id));
    }

    pub fn session_count(&self) -> usize {
        lock(&self.state).sessions.len()
    }

    /// Seconds the cookie lives (the browser forgets it then).
    pub fn cookie_max_age(&self) -> u64 {
        self.limits.session_lifetime.as_secs()
    }

    fn expire(&self, st: &mut State, now: Instant) {
        let (life, idle) = (self.limits.session_lifetime, self.limits.session_idle);
        st.sessions.retain(|s| {
            now.saturating_duration_since(s.created) < life
                && now.saturating_duration_since(s.last_seen) < idle
        });
    }

    /// Seconds left of a lockout, if one is active (clearing it when it has passed).
    fn locked(&self, st: &mut State, now: Instant) -> Option<u64> {
        match st.locked_until {
            Some(until) if now < until => Some(
                until
                    .saturating_duration_since(now)
                    .as_secs()
                    .saturating_add(1),
            ),
            Some(_) => {
                st.locked_until = None;
                st.failures.clear();
                None
            }
            None => None,
        }
    }

    fn fail(&self, st: &mut State, now: Instant) -> Verdict {
        let window = self.limits.failure_window;
        st.failures
            .retain(|t| now.saturating_duration_since(*t) < window);
        st.failures.push(now);
        if st.failures.len() >= self.limits.max_failures {
            st.locked_until = Some(now + self.limits.lockout);
        }
        Verdict::Rejected
    }
}

/// Extracts the session id from a `Cookie` header. Anything unusual (a repeated cookie of ours,
/// an empty value) is "no cookie".
pub fn session_cookie(header: &str) -> Option<&str> {
    let mut found: Option<&str> = None;
    for pair in header.split(';') {
        let pair = pair.trim();
        if let Some(value) = pair
            .strip_prefix(COOKIE_NAME)
            .and_then(|r| r.strip_prefix('='))
        {
            if found.is_some() {
                return None;
            }
            found = Some(value);
        }
    }
    found.filter(|v| !v.is_empty() && v.len() <= 128)
}

/// The value of the `Set-Cookie` header that starts a session.
pub fn set_cookie(id: &str, max_age_secs: u64) -> String {
    format!("{COOKIE_NAME}={id}; Path=/; HttpOnly; SameSite=Strict; Max-Age={max_age_secs}")
}

/// The `Set-Cookie` header value that removes the cookie.
pub fn clear_cookie() -> String {
    format!("{COOKIE_NAME}=; Path=/; HttpOnly; SameSite=Strict; Max-Age=0")
}

/// Splits an `Authorization` header into its bearer token (scheme compared without case).
pub fn bearer_token(header: &str) -> Option<&str> {
    let (scheme, rest) = header.split_once(' ')?;
    if !scheme.eq_ignore_ascii_case("bearer") {
        return None;
    }
    let rest = rest.trim();
    (!rest.is_empty() && !rest.contains(' ')).then_some(rest)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn auth(limits: AuthLimits) -> WebAuth {
        WebAuth::with_token("t".repeat(64), limits)
    }

    #[test]
    fn the_generated_token_is_long_random_hex() {
        let a = WebAuth::new(AuthLimits::default()).unwrap();
        let b = WebAuth::new(AuthLimits::default()).unwrap();
        assert_eq!(a.token().len(), 64);
        assert!(a.token().bytes().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(a.token(), b.token());
    }

    #[test]
    fn bearer_and_session_checks() {
        let a = auth(AuthLimits::default());
        let now = Instant::now();
        let tok = "t".repeat(64);
        assert_eq!(a.check(Presented::Bearer(&tok), now), Verdict::Bearer);
        assert_eq!(a.check(Presented::Nothing, now), Verdict::Missing);
        assert_eq!(a.check(Presented::Bearer("nope"), now), Verdict::Rejected);
        let (id, csrf) = a.login(&tok, now).unwrap();
        assert_ne!(id, csrf);
        assert_eq!(
            a.check(Presented::Cookie(&id), now),
            Verdict::Session { csrf: csrf.clone() }
        );
        assert_eq!(a.check(Presented::Cookie("x"), now), Verdict::Rejected);
        a.logout(&id);
        assert_eq!(a.check(Presented::Cookie(&id), now), Verdict::Rejected);
        assert_eq!(a.session_count(), 0);
    }

    #[test]
    fn a_wrong_token_does_not_log_in() {
        let a = auth(AuthLimits::default());
        assert_eq!(
            a.login(&"u".repeat(64), Instant::now()).unwrap_err(),
            Verdict::Rejected
        );
        assert_eq!(a.login("", Instant::now()).unwrap_err(), Verdict::Rejected);
        assert_eq!(a.session_count(), 0);
    }

    #[test]
    fn sessions_expire_by_age_and_by_idleness() {
        let limits = AuthLimits {
            session_lifetime: Duration::from_secs(100),
            session_idle: Duration::from_secs(30),
            ..AuthLimits::default()
        };
        let a = auth(limits);
        let t0 = Instant::now();
        let tok = "t".repeat(64);
        let (id, _) = a.login(&tok, t0).unwrap();
        // activity keeps it alive past the idle limit...
        assert!(matches!(
            a.check(Presented::Cookie(&id), t0 + Duration::from_secs(20)),
            Verdict::Session { .. }
        ));
        assert!(matches!(
            a.check(Presented::Cookie(&id), t0 + Duration::from_secs(40)),
            Verdict::Session { .. }
        ));
        assert!(matches!(
            a.check(Presented::Cookie(&id), t0 + Duration::from_secs(60)),
            Verdict::Session { .. }
        ));
        assert!(matches!(
            a.check(Presented::Cookie(&id), t0 + Duration::from_secs(85)),
            Verdict::Session { .. }
        ));
        // ...but not past the absolute lifetime
        assert_eq!(
            a.check(Presented::Cookie(&id), t0 + Duration::from_secs(101)),
            Verdict::Rejected
        );
        let (id2, _) = a.login(&tok, t0 + Duration::from_secs(200)).unwrap();
        assert_eq!(
            a.check(Presented::Cookie(&id2), t0 + Duration::from_secs(300)),
            Verdict::Rejected,
            "idle"
        );
    }

    #[test]
    fn the_session_table_is_bounded() {
        let limits = AuthLimits {
            max_sessions: 3,
            ..AuthLimits::default()
        };
        let a = auth(limits);
        let now = Instant::now();
        let tok = "t".repeat(64);
        let ids: Vec<String> = (0..5).map(|_| a.login(&tok, now).unwrap().0).collect();
        assert_eq!(a.session_count(), 3);
        assert_eq!(a.check(Presented::Cookie(&ids[0]), now), Verdict::Rejected);
        assert!(matches!(
            a.check(Presented::Cookie(&ids[4]), now),
            Verdict::Session { .. }
        ));
    }

    #[test]
    fn repeated_failures_lock_authentication_and_the_lock_ends() {
        let limits = AuthLimits {
            max_failures: 3,
            failure_window: Duration::from_secs(60),
            lockout: Duration::from_secs(30),
            ..AuthLimits::default()
        };
        let a = auth(limits);
        let t0 = Instant::now();
        let tok = "t".repeat(64);
        for _ in 0..3 {
            assert_eq!(a.check(Presented::Bearer("bad"), t0), Verdict::Rejected);
        }
        // locked: even the right token is not looked at
        match a.check(Presented::Bearer(&tok), t0 + Duration::from_secs(1)) {
            Verdict::Locked { retry_after_secs } => assert!((29..=31).contains(&retry_after_secs)),
            other => panic!("{other:?}"),
        }
        assert!(matches!(
            a.login(&tok, t0 + Duration::from_secs(2)),
            Err(Verdict::Locked { .. })
        ));
        // no credentials at all is not an attempt, so static pages keep working
        assert_eq!(
            a.check(Presented::Nothing, t0 + Duration::from_secs(3)),
            Verdict::Missing
        );
        // after the lock the right token works again and the counter starts over
        assert_eq!(
            a.check(Presented::Bearer(&tok), t0 + Duration::from_secs(31)),
            Verdict::Bearer
        );
        assert_eq!(
            a.check(Presented::Bearer("bad"), t0 + Duration::from_secs(32)),
            Verdict::Rejected
        );
    }

    #[test]
    fn old_failures_age_out_of_the_window() {
        let limits = AuthLimits {
            max_failures: 3,
            failure_window: Duration::from_secs(10),
            ..AuthLimits::default()
        };
        let a = auth(limits);
        let t0 = Instant::now();
        for i in 0..6u64 {
            assert_eq!(
                a.check(Presented::Bearer("bad"), t0 + Duration::from_secs(i * 11)),
                Verdict::Rejected,
                "failure {i} is alone in its window"
            );
        }
    }

    #[test]
    fn cookie_and_authorization_parsing() {
        assert_eq!(session_cookie("a=b; fleet_session=abc; c=d"), Some("abc"));
        assert_eq!(session_cookie("fleet_session=abc"), Some("abc"));
        assert_eq!(session_cookie("fleet_session="), None);
        assert_eq!(session_cookie("fleet_session=a; fleet_session=b"), None);
        assert_eq!(session_cookie("xfleet_session=a"), None);
        assert_eq!(session_cookie("other=1"), None);
        assert_eq!(bearer_token("Bearer abc"), Some("abc"));
        assert_eq!(bearer_token("bearer abc"), Some("abc"));
        assert_eq!(bearer_token("Basic abc"), None);
        assert_eq!(bearer_token("Bearer"), None);
        assert_eq!(bearer_token("Bearer a b"), None);
    }

    #[test]
    fn the_cookie_is_httponly_samesite_strict() {
        let c = set_cookie("id", 60);
        for part in [
            "fleet_session=id",
            "HttpOnly",
            "SameSite=Strict",
            "Path=/",
            "Max-Age=60",
        ] {
            assert!(c.contains(part), "{c}");
        }
        assert!(clear_cookie().contains("Max-Age=0"));
    }
}
