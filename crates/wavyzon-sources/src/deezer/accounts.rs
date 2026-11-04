//! Deezer account pool: several ARLs with a rotation policy, port of
//! Cadence's `internal/source/deezer/accounts.go`.
//!
//! A rejected account ("arl rejected") is *benched* for [`ACCOUNT_BENCH`]
//! unless it is the last healthy one, so one expired ARL never takes the node
//! down while another one still works.

use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use tokio::sync::Mutex as AsyncMutex;

use crate::cache::Clock;
use crate::deezer::session::Session;

/// How long a rejected account is skipped.
pub const ACCOUNT_BENCH: Duration = Duration::from_secs(15 * 60);

/// `DEEZER_ARL_ROTATION` (Cadence's `ParseRotation`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rotation {
    /// Always try the first healthy account first.
    Failover,
    /// Rotate the starting account, keeping the others as fallbacks.
    RoundRobin,
    /// Start at a random account.
    Random,
}

impl Rotation {
    /// An unknown value is rejected so a typo cannot silently change
    /// behaviour (as in Cadence).
    pub fn parse(raw: &str) -> Result<Self, String> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "" => Ok(Self::Failover),
            "failover" => Ok(Self::Failover),
            "round-robin" => Ok(Self::RoundRobin),
            "random" => Ok(Self::Random),
            _ => Err(format!(
                "DEEZER_ARL_ROTATION must be failover, round-robin or random, got {raw:?}"
            )),
        }
    }
}

/// One logged-in Deezer account. Debug output never shows the ARL.
pub struct Account {
    number: usize,
    arl: String,
    session: AsyncMutex<Option<Arc<Session>>>,
    benched_until: Mutex<Option<Instant>>,
}

impl Account {
    /// 1-based position in the pool (what logs and metrics mention).
    pub fn number(&self) -> usize {
        self.number
    }

    /// The `arl=` cookie value; never log it.
    pub fn arl(&self) -> &str {
        &self.arl
    }

    /// The cached gw session slot (held across the login fetch, as in
    /// Cadence, so one login per account happens at a time).
    pub(crate) async fn session(&self) -> tokio::sync::MutexGuard<'_, Option<Arc<Session>>> {
        self.session.lock().await
    }

    /// Forget the cached session (the token was refused).
    pub(crate) async fn invalidate(&self) {
        *self.session.lock().await = None;
    }

    fn healthy(&self, now: Instant) -> bool {
        self.benched_until.lock().is_none_or(|until| now >= until)
    }
}

impl fmt::Debug for Account {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Account")
            .field("number", &self.number)
            .finish_non_exhaustive()
    }
}

impl fmt::Display for Account {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "account {}", self.number)
    }
}

type IntN = Arc<dyn Fn(usize) -> usize + Send + Sync>;

/// The ordered list of accounts a stream request walks (Cadence's
/// `accountPool`).
pub struct AccountPool {
    accounts: Vec<Arc<Account>>,
    rotation: Rotation,
    next: AtomicUsize,
    intn: IntN,
    now: Clock,
}

impl AccountPool {
    /// `arl` values, in order; blanks and duplicates are dropped.
    pub fn new(arls: Vec<String>, rotation: Rotation) -> Self {
        Self::with_hooks(
            arls,
            rotation,
            Arc::new(Instant::now),
            Arc::new(|n| rand::random_range(0..n)),
        )
    }

    /// Like [`AccountPool::new`] with an injectable clock and random source.
    pub(crate) fn with_hooks(arls: Vec<String>, rotation: Rotation, now: Clock, intn: IntN) -> Self {
        let mut accounts = Vec::new();
        let mut seen = std::collections::HashSet::new();
        for arl in arls {
            let arl = arl.trim().to_string();
            if arl.is_empty() || !seen.insert(arl.clone()) {
                continue;
            }
            accounts.push(Arc::new(Account {
                number: accounts.len() + 1,
                arl,
                session: AsyncMutex::new(None),
                benched_until: Mutex::new(None),
            }));
        }
        Self {
            accounts,
            rotation,
            next: AtomicUsize::new(0),
            intn,
            now,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.accounts.is_empty()
    }

    /// Number of configured accounts (logs).
    pub fn len(&self) -> usize {
        self.accounts.len()
    }

    /// The healthy accounts in try order; the starting account depends on the
    /// rotation, the rest stay as fallbacks.
    pub fn order(&self) -> Vec<Arc<Account>> {
        let now = (self.now)();
        let healthy: Vec<Arc<Account>> = self.accounts.iter().filter(|a| a.healthy(now)).cloned().collect();
        if healthy.len() < 2 {
            return healthy;
        }
        let start = match self.rotation {
            Rotation::Failover => 0,
            Rotation::RoundRobin => self.next.fetch_add(1, Ordering::Relaxed) % healthy.len(),
            Rotation::Random => (self.intn)(healthy.len()),
        };
        let mut out = healthy[start..].to_vec();
        out.extend_from_slice(&healthy[..start]);
        out
    }

    /// Bench `a` for [`ACCOUNT_BENCH`]. `true` when the bench is new (log it
    /// once); the last healthy account is never benched.
    pub fn bench(&self, a: &Account) -> bool {
        let now = (self.now)();
        let fresh = a.healthy(now);
        if fresh && !self.other_healthy(a, now) {
            return false;
        }
        *a.benched_until.lock() = Some(now + ACCOUNT_BENCH);
        fresh
    }

    fn other_healthy(&self, a: &Account, now: Instant) -> bool {
        self.accounts.iter().any(|o| o.number != a.number && o.healthy(now))
    }

    #[cfg(test)]
    pub(crate) fn accounts(&self) -> &[Arc<Account>] {
        &self.accounts
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn numbers(accounts: &[Arc<Account>]) -> Vec<usize> {
        accounts.iter().map(|a| a.number()).collect()
    }

    #[test]
    fn parse_rotation() {
        for (raw, want) in [
            ("", Rotation::Failover),
            ("failover", Rotation::Failover),
            (" Round-Robin", Rotation::RoundRobin),
            ("RANDOM", Rotation::Random),
        ] {
            assert_eq!(Rotation::parse(raw).unwrap(), want, "{raw}");
        }
        assert!(
            Rotation::parse("sticky").is_err(),
            "an unknown rotation must be rejected so a typo cannot silently change behaviour"
        );
    }

    #[test]
    fn pool_drops_blanks_and_duplicates() {
        let p = AccountPool::new(
            vec![" a ".into(), "".into(), "b".into(), "a".into(), "c".into()],
            Rotation::Failover,
        );
        assert_eq!(numbers(&p.order()), [1, 2, 3]);
        assert!(
            AccountPool::new(vec![" ".into(), "".into()], Rotation::Failover).is_empty(),
            "blank entries must not count as accounts"
        );
    }

    #[test]
    fn failover_always_starts_with_the_first_healthy_account() {
        let p = AccountPool::new(vec!["a".into(), "b".into(), "c".into()], Rotation::Failover);
        for _ in 0..3 {
            assert_eq!(numbers(&p.order()), [1, 2, 3]);
        }
    }

    #[test]
    fn round_robin_rotates_the_start_and_keeps_the_rest_as_fallbacks() {
        let p = AccountPool::new(vec!["a".into(), "b".into(), "c".into()], Rotation::RoundRobin);
        for want in [[1, 2, 3], [2, 3, 1], [3, 1, 2], [1, 2, 3]] {
            assert_eq!(numbers(&p.order()), want);
        }
    }

    #[test]
    fn random_starts_where_the_generator_says_and_keeps_every_account() {
        let p = AccountPool::with_hooks(
            vec!["a".into(), "b".into(), "c".into()],
            Rotation::Random,
            Arc::new(Instant::now),
            Arc::new(|_| 2),
        );
        assert_eq!(numbers(&p.order()), [3, 1, 2]);
    }

    #[test]
    fn benched_accounts_sit_out_until_the_bench_expires() {
        let base = Instant::now();
        let offset = Arc::new(Mutex::new(Duration::ZERO));
        let now: Clock = {
            let offset = offset.clone();
            Arc::new(move || base + *offset.lock())
        };
        let p = AccountPool::with_hooks(vec!["a".into(), "b".into()], Rotation::Failover, now, Arc::new(|n| n));
        let first = p.accounts()[0].clone();
        assert!(
            p.bench(&first),
            "the first bench must report itself as fresh so it is logged once"
        );
        assert!(
            !p.bench(&first),
            "benching an already benched account must not report it as fresh"
        );
        assert_eq!(numbers(&p.order()), [2], "only the healthy account");
        *offset.lock() = ACCOUNT_BENCH;
        assert_eq!(numbers(&p.order()), [1, 2], "the account is back after its bench");
    }

    #[test]
    fn an_account_never_prints_its_arl() {
        let p = AccountPool::new(vec!["secret-arl-value".into()], Rotation::Failover);
        let a = &p.accounts()[0];
        for s in [format!("{a:?}"), format!("{a}"), format!("{a:#?}")] {
            assert!(!s.contains("secret"), "formatted account leaked its ARL: {s}");
        }
    }
}
