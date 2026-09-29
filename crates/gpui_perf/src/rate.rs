//! How fast a market feed pushes quotes into the trading workspace: named
//! tiers, from a feed that is off to one busier than the display's frame
//! rate, shared by the showcase (`--rate`, and its toolbar) and the headless
//! scenarios (`workspace-quotes-calm` and the like).
//!
//! A real feed does not tick on a clock: each quote arrives on its own, when
//! somebody trades, so the gaps between quotes are random and quotes bunch up
//! and thin out. [`Feed`] delivers them that way, as a Poisson process — gaps
//! drawn from an exponential distribution — at the tier's mean rate, from a
//! fixed seed, so two runs get the same quotes at the same times.
//!
//! | tier     | quotes a second, on average | as in                                |
//! |----------|-----------------------------|--------------------------------------|
//! | `idle`   | 0                           | a closed market                      |
//! | `calm`   | 8                           | a quiet market, a watchlist at night |
//! | `normal` | 120                         | a watchlist during trading hours     |
//! | `busy`   | 360                         | an active session                    |
//! | `burst`  | 960                         | the open, or news                    |
//!
//! `burst` is the quotes a second the showcase used to push, sixteen every
//! 16 ms, now at random times. Scenarios where the user also scrolls or
//! hovers get half the rate, as they did before.

use std::time::Duration;

/// A quote feed's mean rate.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Rate {
    Idle,
    Calm,
    Normal,
    Busy,
    Burst,
}

impl Rate {
    pub const ALL: [Rate; 5] = [
        Rate::Idle,
        Rate::Calm,
        Rate::Normal,
        Rate::Busy,
        Rate::Burst,
    ];

    /// The tier's name, as `--rate` takes it.
    pub fn name(self) -> &'static str {
        match self {
            Rate::Idle => "idle",
            Rate::Calm => "calm",
            Rate::Normal => "normal",
            Rate::Busy => "busy",
            Rate::Burst => "burst",
        }
    }

    pub fn parse(name: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|rate| rate.name().eq_ignore_ascii_case(name))
    }

    /// Quotes a second, on average, while the user does nothing.
    pub fn quotes_per_second(self) -> f64 {
        match self {
            Rate::Idle => 0.,
            Rate::Calm => 8.,
            Rate::Normal => 120.,
            Rate::Busy => 360.,
            Rate::Burst => 960.,
        }
    }

    /// How long a measurement at this rate lasts at least, for it to see
    /// about 64 quotes.
    pub fn min_duration(self) -> Duration {
        let per_second = self.quotes_per_second();
        if per_second == 0. {
            Duration::ZERO
        } else {
            Duration::from_secs_f64(64. / per_second)
        }
    }

    /// One line on the tier, for `--list` and tooltips.
    pub fn description(self) -> String {
        match self {
            Rate::Idle => "no quotes".to_string(),
            _ => format!(
                "about {} quotes a second, at random times",
                self.quotes_per_second()
            ),
        }
    }
}

/// Quotes arriving at random times, `per_second` on average: a Poisson
/// process from a fixed seed.
pub struct Feed {
    per_second: f64,
    state: u64,
    /// When the next quote arrives, in seconds since the feed started.
    next: f64,
}

impl Feed {
    pub fn new(per_second: f64) -> Self {
        let mut feed = Self {
            per_second,
            state: 0x9E37_79B9_7F4A_7C15,
            next: 0.,
        };
        feed.next = feed.gap();
        feed
    }

    /// The quotes that arrive before `time` since the feed started, each a
    /// random number to pick its symbol and move its price by.
    pub fn until(&mut self, time: Duration) -> Vec<u64> {
        let time = time.as_secs_f64();
        let mut quotes = Vec::new();
        while self.next < time {
            quotes.push(self.random());
            self.next += self.gap();
        }
        quotes
    }

    /// When the next quote arrives, since the feed started; never, for a
    /// feed that is off.
    pub fn next(&self) -> Option<Duration> {
        self.next
            .is_finite()
            .then(|| Duration::from_secs_f64(self.next))
    }

    /// The gap before the next quote: exponentially distributed, as between
    /// the events of a Poisson process.
    fn gap(&mut self) -> f64 {
        if self.per_second <= 0. {
            return f64::INFINITY;
        }
        // Uniform in (0, 1], so that its logarithm is finite.
        let uniform = ((self.random() >> 11) + 1) as f64 / (1u64 << 53) as f64;
        -uniform.ln() / self.per_second
    }

    /// SplitMix64.
    fn random(&mut self) -> u64 {
        self.state = self.state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
}
