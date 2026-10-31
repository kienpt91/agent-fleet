//! Authentication state of the dashboard: the per-start token, browser sessions with their CSRF
//! tokens, and the limiter for failed guesses.
//!
//! All time-dependent methods take the current `Instant`, so tests control the clock.
//!
//! * The **web token** is 256 random bits generated when the daemon starts, shown once on the
//!   operator's terminal and held only in memory. It is never written to the state directory, the
//!   event log or any log, and workers never receive it.
//! * A **session** is created by exchanging the token (`POST /api/login`). Its id is returned in the
//!   body and sent by the page in the `X-Fleet-Session` header, never as a cookie: cookies are not
//!   isolated by port, so a cookie would travel to every other server on the same host (a dev
//!   server a worker started, for example). The page keeps the id in `sessionStorage`, which is
//!   per origin including the port. Every session has its own CSRF token, which the page sends in
//!   a header with every state-changing request.
//! * Failed *guesses* (a wrong token, as bearer or at login) are counted per client: all local
//!   processes share one bucket, each remote address has its own. Too many lock only the token
//!   paths of that client for a while. An unknown session id is not a guess (a stale tab, a
//!   restarted daemon): it is refused without counting, and live sessions keep working during a
//!   lockout.

use crate::statedir::{generate_token, tokens_equal};
use fleet_domain::error::Result;
use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

/// The request header that carries a session id.
pub const SESSION_HEADER: &str = "x-fleet-session";

#[derive(Clone, Copy, Debug)]
pub struct AuthLimits {
    pub max_sessions: usize,
    /// A session ends this long after it was created, whatever its activity.
    pub session_lifetime: Duration,
    /// A session ends this long after its last request.
    pub session_idle: Duration,
    /// This many failed guesses within `failure_window` lock the token paths of that client.
    pub max_failures: usize,
    pub failure_window: Duration,
    pub lockout: Duration,
    /// Clients with failure records kept at once (plus the shared local bucket).
    pub max_clients: usize,
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
            max_clients: 256,
        }
    }
}

/// Who is guessing: every local process shares one key, a remote address has its own.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ClientKey {
    Local,
    Remote(IpAddr),
}

impl ClientKey {
    pub fn of(ip: IpAddr) -> ClientKey {
        if ip.to_canonical().is_loopback() {
            ClientKey::Local
        } else {
            ClientKey::Remote(ip.to_canonical())
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
struct Failures {
    at: Vec<Instant>,
    locked_until: Option<Instant>,
}

#[derive(Default)]
struct State {
    sessions: Vec<Session>,
    clients: HashMap<ClientKey, Failures>,
}

/// What a request presented as credentials.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Presented<'a> {
    Nothing,
    Bearer(&'a str),
    Session(&'a str),
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
    /// Wrong token, or an unknown or expired session id.
    Rejected,
    /// A token was presented while this client is locked out; nothing was checked.
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

    /// Checks `presented` credentials. A wrong token counts as a failed guess; an unknown session
    /// id and presenting nothing do not.
    pub fn check(&self, presented: Presented<'_>, client: ClientKey, now: Instant) -> Verdict {
        let mut st = lock(&self.state);
        match presented {
            Presented::Nothing => Verdict::Missing,
            Presented::Bearer(t) => {
                if let Some(retry) = self.locked(&mut st, client, now) {
                    return Verdict::Locked {
                        retry_after_secs: retry,
                    };
                }
                if tokens_equal(t, &self.token) {
                    Verdict::Bearer
                } else {
                    self.fail(&mut st, client, now)
                }
            }
            Presented::Session(id) => {
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
                    None => Verdict::Rejected,
                }
            }
        }
    }

    /// True while the session exists and has not expired. Does not count as activity: an event
    /// stream uses it to notice that its session was ended, logged out or timed out.
    pub fn session_alive(&self, id: &str, now: Instant) -> bool {
        let st = lock(&self.state);
        st.sessions.iter().any(|s| {
            tokens_equal(id, &s.id)
                && now.saturating_duration_since(s.created) < self.limits.session_lifetime
                && now.saturating_duration_since(s.last_seen) < self.limits.session_idle
        })
    }

    /// Exchanges the token for a new session: its id and its CSRF token. Returns `Err(verdict)`
    /// with `Rejected` or `Locked` otherwise.
    pub fn login(
        &self,
        token: &str,
        client: ClientKey,
        now: Instant,
    ) -> std::result::Result<(String, String), Verdict> {
        let mut st = lock(&self.state);
        if let Some(retry) = self.locked(&mut st, client, now) {
            return Err(Verdict::Locked {
                retry_after_secs: retry,
            });
        }
        if !tokens_equal(token, &self.token) {
            return Err(self.fail(&mut st, client, now));
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

    fn expire(&self, st: &mut State, now: Instant) {
        let (life, idle) = (self.limits.session_lifetime, self.limits.session_idle);
        st.sessions.retain(|s| {
            now.saturating_duration_since(s.created) < life
                && now.saturating_duration_since(s.last_seen) < idle
        });
    }

    /// Seconds left of a lockout of `client`, if one is active (clearing it when it has passed).
    fn locked(&self, st: &mut State, client: ClientKey, now: Instant) -> Option<u64> {
        let rec = st.clients.get_mut(&client)?;
        match rec.locked_until {
            Some(until) if now < until => Some(
                until
                    .saturating_duration_since(now)
                    .as_secs()
                    .saturating_add(1),
            ),
            Some(_) => {
                st.clients.remove(&client);
                None
            }
            None => None,
        }
    }

    fn fail(&self, st: &mut State, client: ClientKey, now: Instant) -> Verdict {
        let window = self.limits.failure_window;
        if !st.clients.contains_key(&client) && st.clients.len() >= self.limits.max_clients.max(1) {
            // Forget clients whose records are over, then, if the table is still full, count this
            // failure against the shared local bucket rather than growing without bound.
            st.clients.retain(|_, f| {
                f.locked_until.is_some_and(|u| now < u)
                    || f.at
                        .last()
                        .is_some_and(|t| now.saturating_duration_since(*t) < window)
            });
        }
        let key = if st.clients.contains_key(&client)
            || st.clients.len() < self.limits.max_clients.max(1)
        {
            client
        } else {
            ClientKey::Local
        };
        let rec = st.clients.entry(key).or_default();
        rec.at
            .retain(|t| now.saturating_duration_since(*t) < window);
        rec.at.push(now);
        if rec.at.len() >= self.limits.max_failures {
            rec.locked_until = Some(now + self.limits.lockout);
        }
        Verdict::Rejected
    }

    /// Clients with a failure record (for tests).
    pub fn tracked_clients(&self) -> usize {
        lock(&self.state).clients.len()
    }
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

/// A session id from its header: bounded and printable, anything else counts as no session.
pub fn session_id(header: &str) -> Option<&str> {
    (!header.is_empty() && header.len() <= 128).then_some(header)
}

#[cfg(test)]
mod tests {
    use super::*;

    const LOCAL: ClientKey = ClientKey::Local;

    fn auth(limits: AuthLimits) -> WebAuth {
        WebAuth::with_token("t".repeat(64), limits)
    }

    fn remote(last: u8) -> ClientKey {
        ClientKey::Remote(IpAddr::from([203, 0, 113, last]))
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
        assert_eq!(
            a.check(Presented::Bearer(&tok), LOCAL, now),
            Verdict::Bearer
        );
        assert_eq!(a.check(Presented::Nothing, LOCAL, now), Verdict::Missing);
        assert_eq!(
            a.check(Presented::Bearer("nope"), LOCAL, now),
            Verdict::Rejected
        );
        let (id, csrf) = a.login(&tok, LOCAL, now).unwrap();
        assert_ne!(id, csrf);
        assert_eq!(
            a.check(Presented::Session(&id), LOCAL, now),
            Verdict::Session { csrf: csrf.clone() }
        );
        assert_eq!(
            a.check(Presented::Session("x"), LOCAL, now),
            Verdict::Rejected
        );
        assert!(a.session_alive(&id, now));
        a.logout(&id);
        assert_eq!(
            a.check(Presented::Session(&id), LOCAL, now),
            Verdict::Rejected
        );
        assert!(!a.session_alive(&id, now));
        assert_eq!(a.session_count(), 0);
    }

    #[test]
    fn a_wrong_token_does_not_log_in() {
        let a = auth(AuthLimits::default());
        assert_eq!(
            a.login(&"u".repeat(64), LOCAL, Instant::now()).unwrap_err(),
            Verdict::Rejected
        );
        assert_eq!(
            a.login("", LOCAL, Instant::now()).unwrap_err(),
            Verdict::Rejected
        );
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
        let (id, _) = a.login(&tok, LOCAL, t0).unwrap();
        // activity keeps it alive past the idle limit...
        for secs in [20, 40, 60, 85] {
            assert!(
                matches!(
                    a.check(
                        Presented::Session(&id),
                        LOCAL,
                        t0 + Duration::from_secs(secs)
                    ),
                    Verdict::Session { .. }
                ),
                "{secs}"
            );
        }
        // ...but not past the absolute lifetime
        assert!(!a.session_alive(&id, t0 + Duration::from_secs(101)));
        assert_eq!(
            a.check(
                Presented::Session(&id),
                LOCAL,
                t0 + Duration::from_secs(101)
            ),
            Verdict::Rejected
        );
        let (id2, _) = a.login(&tok, LOCAL, t0 + Duration::from_secs(200)).unwrap();
        assert!(
            !a.session_alive(&id2, t0 + Duration::from_secs(300)),
            "idle"
        );
        assert_eq!(
            a.check(
                Presented::Session(&id2),
                LOCAL,
                t0 + Duration::from_secs(300)
            ),
            Verdict::Rejected,
            "idle"
        );
    }

    #[test]
    fn peeking_at_a_session_is_not_activity() {
        let limits = AuthLimits {
            session_idle: Duration::from_secs(30),
            ..AuthLimits::default()
        };
        let a = auth(limits);
        let t0 = Instant::now();
        let (id, _) = a.login(&"t".repeat(64), LOCAL, t0).unwrap();
        assert!(a.session_alive(&id, t0 + Duration::from_secs(20)));
        assert!(
            !a.session_alive(&id, t0 + Duration::from_secs(40)),
            "a stream that only peeks does not keep an idle session alive"
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
        let ids: Vec<String> = (0..5)
            .map(|_| a.login(&tok, LOCAL, now).unwrap().0)
            .collect();
        assert_eq!(a.session_count(), 3);
        assert_eq!(
            a.check(Presented::Session(&ids[0]), LOCAL, now),
            Verdict::Rejected
        );
        assert!(matches!(
            a.check(Presented::Session(&ids[4]), LOCAL, now),
            Verdict::Session { .. }
        ));
    }

    fn lock_limits() -> AuthLimits {
        AuthLimits {
            max_failures: 3,
            failure_window: Duration::from_secs(60),
            lockout: Duration::from_secs(30),
            ..AuthLimits::default()
        }
    }

    #[test]
    fn repeated_wrong_tokens_lock_the_token_paths_and_the_lock_ends() {
        let a = auth(lock_limits());
        let t0 = Instant::now();
        let tok = "t".repeat(64);
        for _ in 0..3 {
            assert_eq!(
                a.check(Presented::Bearer("bad"), LOCAL, t0),
                Verdict::Rejected
            );
        }
        // locked: even the right token is not looked at, as bearer or at login
        match a.check(Presented::Bearer(&tok), LOCAL, t0 + Duration::from_secs(1)) {
            Verdict::Locked { retry_after_secs } => assert!((29..=31).contains(&retry_after_secs)),
            other => panic!("{other:?}"),
        }
        assert!(matches!(
            a.login(&tok, LOCAL, t0 + Duration::from_secs(2)),
            Err(Verdict::Locked { .. })
        ));
        // no credentials at all is not an attempt, so static pages keep working
        assert_eq!(
            a.check(Presented::Nothing, LOCAL, t0 + Duration::from_secs(3)),
            Verdict::Missing
        );
        // after the lock the right token works again and the counter starts over
        assert_eq!(
            a.check(Presented::Bearer(&tok), LOCAL, t0 + Duration::from_secs(31)),
            Verdict::Bearer
        );
        assert_eq!(
            a.check(
                Presented::Bearer("bad"),
                LOCAL,
                t0 + Duration::from_secs(32)
            ),
            Verdict::Rejected
        );
    }

    #[test]
    fn a_lockout_leaves_live_sessions_alone_and_unknown_sessions_are_not_guesses() {
        let a = auth(lock_limits());
        let t0 = Instant::now();
        let (id, csrf) = a.login(&"t".repeat(64), LOCAL, t0).unwrap();
        for _ in 0..20 {
            assert_eq!(
                a.check(Presented::Session("stale"), LOCAL, t0),
                Verdict::Rejected
            );
        }
        assert_eq!(a.tracked_clients(), 0, "unknown sessions leave no record");
        for _ in 0..3 {
            a.check(Presented::Bearer("bad"), LOCAL, t0);
        }
        assert!(matches!(
            a.check(Presented::Bearer("bad"), LOCAL, t0),
            Verdict::Locked { .. }
        ));
        assert_eq!(
            a.check(Presented::Session(&id), LOCAL, t0),
            Verdict::Session { csrf }
        );
        assert_eq!(
            a.check(Presented::Session("stale"), LOCAL, t0),
            Verdict::Rejected,
            "not Locked: nothing was guessed"
        );
    }

    #[test]
    fn a_remote_address_locks_only_itself() {
        let a = auth(lock_limits());
        let t0 = Instant::now();
        let tok = "t".repeat(64);
        for _ in 0..3 {
            a.check(Presented::Bearer("bad"), remote(9), t0);
        }
        assert!(matches!(
            a.check(Presented::Bearer(&tok), remote(9), t0),
            Verdict::Locked { .. }
        ));
        assert_eq!(
            a.check(Presented::Bearer(&tok), remote(10), t0),
            Verdict::Bearer
        );
        assert_eq!(a.check(Presented::Bearer(&tok), LOCAL, t0), Verdict::Bearer);
        assert!(a.login(&tok, LOCAL, t0).is_ok());
        assert!(a.login(&tok, remote(10), t0).is_ok());
        assert!(matches!(
            a.login(&tok, remote(9), t0),
            Err(Verdict::Locked { .. })
        ));
    }

    #[test]
    fn the_failure_table_is_bounded() {
        let limits = AuthLimits {
            max_clients: 4,
            ..lock_limits()
        };
        let a = auth(limits);
        let t0 = Instant::now();
        for i in 0..50u8 {
            a.check(Presented::Bearer("bad"), remote(i), t0);
        }
        // at most `max_clients` addresses plus the shared local bucket that overflow falls into
        assert!(a.tracked_clients() <= 5, "{}", a.tracked_clients());
        // records of clients that went quiet are dropped to make room
        let later = t0 + Duration::from_secs(120);
        a.check(Presented::Bearer("bad"), remote(200), later);
        assert_eq!(a.tracked_clients(), 1, "only the new client is left");
    }

    #[test]
    fn old_failures_age_out_of_the_window() {
        let limits = AuthLimits {
            failure_window: Duration::from_secs(10),
            ..lock_limits()
        };
        let a = auth(limits);
        let t0 = Instant::now();
        for i in 0..6u64 {
            assert_eq!(
                a.check(
                    Presented::Bearer("bad"),
                    LOCAL,
                    t0 + Duration::from_secs(i * 11)
                ),
                Verdict::Rejected,
                "failure {i} is alone in its window"
            );
        }
    }

    #[test]
    fn client_keys() {
        assert_eq!(
            ClientKey::of(IpAddr::from([127, 0, 0, 1])),
            ClientKey::Local
        );
        assert_eq!(ClientKey::of("::1".parse().unwrap()), ClientKey::Local);
        assert_eq!(
            ClientKey::of("::ffff:127.0.0.1".parse().unwrap()),
            ClientKey::Local
        );
        assert_eq!(
            ClientKey::of("::ffff:203.0.113.5".parse().unwrap()),
            remote(5)
        );
    }

    #[test]
    fn authorization_and_session_header_parsing() {
        assert_eq!(bearer_token("Bearer abc"), Some("abc"));
        assert_eq!(bearer_token("bearer abc"), Some("abc"));
        assert_eq!(bearer_token("Basic abc"), None);
        assert_eq!(bearer_token("Bearer"), None);
        assert_eq!(bearer_token("Bearer a b"), None);
        assert_eq!(session_id("abc"), Some("abc"));
        assert_eq!(session_id(""), None);
        assert_eq!(session_id(&"a".repeat(129)), None);
    }
}
