//! How fast a market feed pushes quotes into the trading workspace: named
//! tiers, from a feed that is off to one that ticks every frame, shared by the
//! showcase (`--rate`, and its toolbar) and the headless scenarios
//! (`workspace-quotes-calm` and the like).
//!
//! A feed delivers quotes in batches: the slower it ticks, the more quotes
//! pile up between ticks, so the batches of the slower tiers are larger, but
//! the quotes per second still fall from tier to tier.
//!
//! | tier     | ticks            | quotes a tick | quotes a second |
//! |----------|------------------|---------------|-----------------|
//! | `idle`   | never            | 0             | 0               |
//! | `calm`   | 1 s (1 Hz)       | 8             | 8               |
//! | `normal` | 250 ms (4 Hz)    | 32            | 128             |
//! | `busy`   | 66 ms (~15 Hz)   | 24            | ~360            |
//! | `burst`  | 16 ms (60 Hz)    | 16            | ~1000           |
//!
//! - `calm`: a quiet market, or a watchlist left open at night.
//! - `normal`: a feed batched every 250 ms during trading hours, as most
//!   brokers' quote pushes are.
//! - `busy`: an active session, the feed coalesced to about a tick per four
//!   frames.
//! - `burst`: every frame gets a batch, as at the open or on news. This was
//!   the showcase's only rate before the tiers, so its numbers compare with
//!   older runs.
//!
//! Scenarios where the user also scrolls or hovers push half as many quotes a
//! tick, as they did before.

use std::time::Duration;

/// A quote feed's update rate.
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

    /// How often the feed ticks, or `None` if it doesn't.
    pub fn every(self) -> Option<Duration> {
        match self {
            Rate::Idle => None,
            Rate::Calm => Some(Duration::from_millis(1000)),
            Rate::Normal => Some(Duration::from_millis(250)),
            Rate::Busy => Some(Duration::from_millis(66)),
            Rate::Burst => Some(Duration::from_millis(16)),
        }
    }

    /// How often the feed ticks in frames of a 60 Hz display, for the
    /// headless scenarios, which have no clock.
    pub fn every_frames(self) -> Option<usize> {
        match self {
            Rate::Idle => None,
            Rate::Calm => Some(60),
            Rate::Normal => Some(15),
            Rate::Busy => Some(4),
            Rate::Burst => Some(1),
        }
    }

    /// Quotes each tick delivers while the user does nothing.
    pub fn quotes_per_tick(self) -> usize {
        match self {
            Rate::Idle => 0,
            Rate::Calm => 8,
            Rate::Normal => 32,
            Rate::Busy => 24,
            Rate::Burst => 16,
        }
    }

    /// The tick frame `frame` of a 60 Hz display delivers, counting from 0,
    /// if the feed ticks on it.
    pub fn tick_at(self, frame: usize) -> Option<usize> {
        let every = self.every_frames()?;
        frame.is_multiple_of(every).then(|| frame / every)
    }

    /// One line on the tier, for `--list` and tooltips.
    pub fn description(self) -> String {
        match self.every() {
            None => "no quotes".to_string(),
            Some(every) => format!(
                "{} quotes every {} ms",
                self.quotes_per_tick(),
                every.as_millis()
            ),
        }
    }
}
