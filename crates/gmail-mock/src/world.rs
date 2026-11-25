//! All mock state, behind one lock.

use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

use jiff::tz::TimeZone;
use jiff::{Timestamp, Zoned};

use crate::calendar::Calendars;
use crate::mail::Mailbox;
use crate::oauth::Sessions;
use crate::people::Directory;

/// How to build the mock's world.
#[derive(Debug, Clone, PartialEq)]
pub struct Config {
    /// Seed for the fixture generator. The same seed always yields the same mailbox.
    pub seed: u64,
    /// The mock's "now", which also sets the account's time zone. The clock runs from here.
    pub now: Zoned,
    /// Raw RFC 822 messages delivered to the inbox at "now", after the fixtures (to compare
    /// clients on a particular email). `/__mock/reset` delivers them again.
    pub extra_messages: Vec<Vec<u8>>,
    /// How long every API request takes before it's answered, to try the client against a
    /// real network's round trips. Zero by default.
    pub latency: Duration,
    /// Gmail quota units the account may spend per minute, enforced as Gmail does (429 with a
    /// retry time). `None` (the default) enforces none. Gmail's is
    /// [`gmail::quota::PER_USER_PER_MINUTE`].
    pub quota: Option<u32>,
    /// Where the mock is served (`http://127.0.0.1:8787/`), which the newsletters' one-click
    /// unsubscribe links point at (`/__mock/unsubscribe/…`), so no client ever unsubscribes
    /// from a real address. [`crate::serve`] and [`crate::MockServer::start`] set it. Without
    /// it, the links go to port 80 of this machine.
    pub origin: Option<url::Url>,
    /// The account has a profile photo of its own, served at `/__mock/photo/…` in whatever
    /// size Google's `=s{n}` option asks for. People's `people/me` lists it, and userinfo's
    /// `picture` points at it. Without it (the default), the account has Google's generated
    /// avatar.
    pub photo: bool,
    /// The saved contacts and the directory's colleagues have photos of their own, served at
    /// `/__mock/photo/…` like the account's. Without it (the default), they have Google's
    /// generated avatar.
    pub contact_photos: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            seed: 1,
            now: "2026-11-17T09:00:00[America/New_York]"
                .parse()
                .expect("valid default time"),
            extra_messages: Vec::new(),
            latency: Duration::ZERO,
            quota: None,
            origin: None,
            photo: false,
            contact_photos: false,
        }
    }
}

/// A zoned time (`2026-11-17T09:00:00-05:00[America/New_York]`), or a plain RFC 3339
/// instant (`2026-11-17T14:00:00Z`) shown in `tz`.
///
/// # Errors
/// When `text` is neither.
pub fn parse_time(text: &str, tz: &TimeZone) -> Result<Zoned, jiff::Error> {
    text.parse::<Zoned>()
        .or_else(|_| text.parse::<Timestamp>().map(|at| at.to_zoned(tz.clone())))
}

/// The signed-in user, shared by Gmail, People, Calendar and userinfo.
#[derive(Debug, Clone)]
pub struct Account {
    pub email: String,
    pub given_name: String,
    pub family_name: String,
    /// Numeric Google account ID.
    pub google_id: String,
    pub tz: TimeZone,
    /// Its profile photo's URL without Google's options (`=s100`), when it has one of its own.
    pub photo: Option<String>,
}

impl Account {
    pub fn name(&self) -> String {
        format!("{} {}", self.given_name, self.family_name)
    }
}

/// A clock that starts at the configured time and then runs in real time.
#[derive(Debug, Clone)]
pub struct Clock {
    anchor: Timestamp,
    started: Instant,
}

impl Clock {
    pub fn new(at: Timestamp) -> Self {
        Self {
            anchor: at,
            started: Instant::now(),
        }
    }

    pub fn now(&self) -> Timestamp {
        let elapsed = jiff::SignedDuration::try_from(self.started.elapsed()).unwrap_or_default();
        self.anchor.checked_add(elapsed).unwrap_or(self.anchor)
    }

    pub fn now_ms(&self) -> i64 {
        self.now().as_millisecond()
    }

    pub fn set(&mut self, at: Timestamp) {
        *self = Self::new(at);
    }
}

/// A request matcher that fails the next `remaining` matching requests with `status`.
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Fault {
    /// HTTP method to match, or any when absent.
    pub method: Option<String>,
    /// Path prefix to match, e.g. `/gmail/v1/users/me/threads/`.
    pub path: String,
    /// Query parameters the request must carry, as `name=value` pairs joined by `&`, or none
    /// when absent. `format=full` fails a thread's full `threads.get` but not its
    /// `format=minimal` one, as Gmail's `responseTooLarge` does.
    pub query: Option<String>,
    /// Status to answer with.
    pub status: u16,
    /// Overrides the canned message for the status.
    pub message: Option<String>,
    /// Overrides the canned reason for the status.
    pub reason: Option<String>,
    /// How many requests to fail, or 1 when absent.
    #[serde(default = "one")]
    pub times: u32,
    /// The request takes effect and only its answer is lost, as when Google did the work but
    /// the response never arrived (a dropped connection, a 5xx from the front end).
    #[serde(default)]
    pub after: bool,
}

fn one() -> u32 {
    1
}

/// Everything the mock knows.
pub struct World {
    /// What the world was built from, for `/__mock/reset`.
    pub config: Config,
    pub account: Account,
    pub clock: Clock,
    pub mail: Mailbox,
    /// The calendars of the mailbox's owner.
    pub calendar: Calendars,
    /// The calendars of the other addresses tokens sign in ([`crate::oauth::Identity`]), made
    /// the first time each is asked for.
    pub others: std::collections::BTreeMap<String, Calendars>,
    pub people: Directory,
    pub sessions: Sessions,
    pub faults: Vec<Fault>,
    pub quota: crate::http::quota::Meter,
    /// The one-click unsubscribes `/__mock/unsubscribe/…` received, oldest first.
    pub unsubscribes: Vec<crate::admin::Unsubscribe>,
}

/// The world locked for one Calendar request of `identity`'s. Until the guard drops, another
/// address sees its own calendars as [`World::calendar`] and itself as [`World::account`]'s
/// address. Dropping the guard puts back those of the mailbox's owner.
pub struct CalendarWorld<'a> {
    world: MutexGuard<'a, World>,
    /// Another address's key in [`World::others`], and the address of the mailbox's owner
    /// meanwhile.
    swapped: Option<(String, String)>,
}

impl std::ops::Deref for CalendarWorld<'_> {
    type Target = World;

    fn deref(&self) -> &World {
        &self.world
    }
}

impl std::ops::DerefMut for CalendarWorld<'_> {
    fn deref_mut(&mut self) -> &mut World {
        &mut self.world
    }
}

impl Drop for CalendarWorld<'_> {
    fn drop(&mut self) {
        if let Some((key, owner)) = self.swapped.take() {
            let world = &mut *self.world;
            let theirs = world.others.get_mut(&key).expect("swapped in at lock");
            std::mem::swap(&mut world.calendar, theirs);
            world.account.email = owner;
        }
    }
}

/// Shared server state.
pub struct App {
    world: Mutex<World>,
}

impl App {
    pub fn new(config: &Config) -> Self {
        Self::from_world(crate::fixtures::world(config))
    }

    pub fn from_world(world: World) -> Self {
        Self {
            world: Mutex::new(world),
        }
    }

    /// Locks the world. A handler that panicked mid-mutation leaves it usable, because the mock
    /// favors staying up over strict consistency.
    pub fn lock(&self) -> MutexGuard<'_, World> {
        self.world
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Locks the world for a Calendar request signed in as `identity` ([`CalendarWorld`]).
    /// Another address's calendars are made the first time it asks.
    pub fn lock_calendar(&self, identity: &crate::oauth::Identity) -> CalendarWorld<'_> {
        let mut world = self.lock();
        let other = identity
            .0
            .as_ref()
            .filter(|email| !email.eq_ignore_ascii_case(&world.account.email));
        let swapped = other.map(|email| {
            let key = email.to_lowercase();
            let world = &mut *world;
            let now = &world.config.now;
            let theirs = world
                .others
                .entry(key.clone())
                .or_insert_with(|| crate::fixtures::personal_calendars(&key, now));
            std::mem::swap(&mut world.calendar, theirs);
            (
                key,
                std::mem::replace(&mut world.account.email, email.clone()),
            )
        });
        CalendarWorld { world, swapped }
    }
}
