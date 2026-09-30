//! When a source is worth polling: the installation's day, with margins, for a source that goes
//! dark at night.
//!
//! One seam, [`Daylight`]: a date in, the polling window out. The sun answers it, computed rather
//! than fetched — a dark network at dawn must not cost a day of readings.

use std::time::Duration;

use jiff::civil::Date;
use jiff::tz::TimeZone;
use jiff::{SignedDuration, Timestamp};

use crate::sun::{self, Day, Site};

/// A span to poll in, `[start, stop)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Window {
    pub start: Timestamp,
    pub stop: Timestamp,
}

/// Whoever decides the day's window.
pub trait Daylight {
    /// The window around `date`'s solar noon.
    ///
    /// # Errors
    ///
    /// If the date cannot be placed in time.
    fn window(&self, date: Date) -> Result<Window, jiff::Error>;
}

/// Sunrise minus a margin to sunset plus one.
#[derive(Debug, Clone, Copy)]
pub struct Sun {
    pub site: Site,
    pub before_sunrise: SignedDuration,
    pub after_sunset: SignedDuration,
    /// Longest a closed window is slept on before looking again. The board has no clock of its
    /// own until the network steps it, so a sleep computed until sunrise at boot can land hours
    /// out; looking again settles it.
    pub recheck: Duration,
}

impl Daylight for Sun {
    fn window(&self, date: Date) -> Result<Window, jiff::Error> {
        let Day { sunrise, sunset } = sun::day(self.site, date)?;
        // Margins are the caller's; jiff's operators panic past the range.
        Ok(Window {
            start: sunrise.checked_sub(self.before_sunrise)?,
            stop: sunset.checked_add(self.after_sunset)?,
        })
    }
}

/// Where `at` falls.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Now {
    Open { until: Timestamp },
    Closed { opens: Timestamp },
}

impl Now {
    #[must_use]
    pub const fn is_open(&self) -> bool {
        matches!(self, Self::Open { .. })
    }
}

/// Where `at` falls among the windows of the dates around it.
///
/// A solar day at the far east or west of the map straddles UTC midnight, so
/// the UTC date of `at` alone can miss the window it is in.
///
/// # Errors
///
/// If the dates cannot be placed in time.
pub fn now(daylight: &dyn Daylight, at: Timestamp) -> Result<Now, jiff::Error> {
    let today = at.to_zoned(TimeZone::UTC).date();
    let windows @ [_, _, tomorrow] = [
        daylight.window(today.yesterday()?)?,
        daylight.window(today)?,
        daylight.window(today.tomorrow()?)?,
    ];
    if let Some(open) = windows.iter().find(|w| w.start <= at && at < w.stop) {
        return Ok(Now::Open { until: open.stop });
    }
    // Latitude is bounded and longitude is not, so far enough east a short
    // winter day can leave every opening behind `at`. The fallback keeps the
    // expression total; the next recheck settles it.
    Ok(Now::Closed {
        opens: windows
            .iter()
            .map(|w| w.start)
            .filter(|&s| s > at)
            .min()
            .unwrap_or(tomorrow.start),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sun::testing::{HONOLULU, SAO_PAULO, SYDNEY, rises};

    fn ts(s: &str) -> Timestamp {
        s.parse().unwrap()
    }

    /// Sunrise near 09:00 UTC and sunset near 21:00 in September.
    fn sun(margin_min: i64) -> Sun {
        Sun {
            site: SAO_PAULO,
            before_sunrise: SignedDuration::from_mins(margin_min),
            after_sunset: SignedDuration::from_mins(margin_min),
            recheck: Duration::from_mins(15),
        }
    }

    #[test]
    fn midday_is_open_until_sunset_plus_the_margin() {
        let daylight = sun(30);
        let Now::Open { until } = now(&daylight, ts("2026-09-14T15:00:00Z")).unwrap() else {
            panic!("midday must be open");
        };
        let (_, sunset) = rises(daylight.site, "2026-09-14");
        assert_eq!(until, sunset + SignedDuration::from_mins(30));
    }

    #[test]
    fn night_is_closed_until_tomorrows_sunrise_minus_the_margin() {
        let daylight = sun(30);
        let Now::Closed { opens } = now(&daylight, ts("2026-09-14T23:30:00Z")).unwrap() else {
            panic!("night must be closed with a next opening");
        };
        let (sunrise, _) = rises(daylight.site, "2026-09-15");
        assert_eq!(opens, sunrise - SignedDuration::from_mins(30));
    }

    #[test]
    fn the_margin_is_what_opens_the_window_early() {
        // Twenty minutes before sunrise: closed without a margin, open with one.
        let (sunrise, _) = rises(SAO_PAULO, "2026-09-14");
        let before = sunrise - SignedDuration::from_mins(20);
        assert!(!now(&sun(0), before).unwrap().is_open());
        assert!(now(&sun(30), before).unwrap().is_open());
    }

    #[test]
    fn a_window_straddling_utc_midnight_is_found_from_either_side() {
        // Sydney's day runs roughly 21:00 to 07:00 UTC in June.
        let sydney = Sun {
            site: SYDNEY,
            before_sunrise: SignedDuration::ZERO,
            after_sunset: SignedDuration::ZERO,
            recheck: Duration::from_mins(15),
        };
        for at in ["2026-06-20T23:00:00Z", "2026-06-21T03:00:00Z"] {
            assert!(
                now(&sydney, ts(at)).unwrap().is_open(),
                "{at} is Sydney daytime"
            );
        }
        assert!(matches!(
            now(&sydney, ts("2026-06-21T12:00:00Z")).unwrap(),
            Now::Closed { .. }
        ));
    }

    #[test]
    fn a_far_west_evening_belongs_to_the_previous_utc_date() {
        // Honolulu's 21 June runs roughly 15:50 to 05:20 UTC the next day, so
        // at 04:00 on the 22nd only yesterday's window holds the answer.
        let honolulu = Sun {
            site: HONOLULU,
            before_sunrise: SignedDuration::ZERO,
            after_sunset: SignedDuration::ZERO,
            recheck: Duration::from_mins(15),
        };
        assert!(
            now(&honolulu, ts("2026-06-22T04:00:00Z"))
                .unwrap()
                .is_open()
        );
    }
}
