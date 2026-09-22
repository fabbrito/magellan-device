//! Sunrise and sunset from coordinates and date — NOAA's solar calculator.
//!
//! The installation wakes at first PV input and goes dark after the last, so
//! the sun is the schedule. Computed here, not fetched: a dark network at dawn
//! must not cost a day of capture. Accurate to about a minute below the polar
//! circles, which the capture margins dwarf.
//!
//! Formulas: NOAA Global Monitoring Laboratory solar calculator spreadsheet.
//! Degrees in, degrees out; north and east positive.

use jiff::civil::Date;
use jiff::tz::TimeZone;
use jiff::{SignedDuration, Timestamp};

/// Sun centre 50′ below the horizon: 34′ of refraction plus 16′ of radius.
const ZENITH_DEG: f64 = 90.833;
/// Furthest from the equator this formula gives a sunrise and a sunset on
/// every date of the year. Short of 66.5° because refraction pushes the
/// midnight sun about 50′ equatorward of the polar circle.
pub const LATITUDE_DEG_MAX: f64 = 65.0;
const MINUTES_PER_DEG: f64 = 4.0;

/// Where on Earth, in decimal degrees.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Site {
    pub latitude: f64,
    pub longitude: f64,
}

/// What the sun does on one date. It rises and it sets: only sites inside
/// [`LATITUDE_DEG_MAX`] reach here, and the config refuses the rest.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Day {
    pub sunrise: Timestamp,
    pub sunset: Timestamp,
}

/// The sun's day at `site` around `date`'s solar noon. Far east or west, the
/// sunrise can fall on the previous UTC date or the sunset on the next.
///
/// # Errors
///
/// If the date is at the edge of what a timestamp can hold.
pub fn day(site: Site, date: Date) -> Result<Day, jiff::Error> {
    let midnight = date.to_zoned(TimeZone::UTC)?.timestamp();
    // Evaluate at an estimate of solar noon, where declination and the
    // equation of time matter; at midnight UTC they are a day off in the Pacific.
    // jiff's `+` panics at the end of the timestamp range.
    let estimate = midnight.checked_add(minutes(720.0 - MINUTES_PER_DEG * site.longitude))?;
    let sun = Position::at(estimate);

    let noon = midnight.checked_add(minutes(
        720.0 - MINUTES_PER_DEG * site.longitude - sun.equation_of_time,
    ))?;
    let (lat, decl) = (site.latitude.to_radians(), sun.declination);
    let cos_hour_angle =
        ZENITH_DEG.to_radians().cos() / (lat.cos() * decl.cos()) - lat.tan() * decl.tan();
    // Clamped to keep the formula total; inside LATITUDE_DEG_MAX it never
    // bites.
    let half_day = minutes(MINUTES_PER_DEG * cos_hour_angle.clamp(-1.0, 1.0).acos().to_degrees());
    Ok(Day {
        sunrise: noon.checked_sub(half_day)?,
        sunset: noon.checked_add(half_day)?,
    })
}

/// Whole seconds: the formula is good to a minute, and nanoseconds in the log
/// would claim otherwise.
fn minutes(m: f64) -> SignedDuration {
    SignedDuration::from_secs((m * 60.0).round() as i64)
}

/// The two quantities sunrise needs from the sun's position.
struct Position {
    /// Radians.
    declination: f64,
    /// Minutes the sundial runs ahead of the clock.
    equation_of_time: f64,
}

impl Position {
    fn at(ts: Timestamp) -> Self {
        let julian_day = ts.as_second() as f64 / 86_400.0 + 2_440_587.5;
        let t = (julian_day - 2_451_545.0) / 36_525.0;

        let mean_long = (280.466_46 + t * (36_000.769_83 + t * 0.000_303_2)).rem_euclid(360.0);
        let mean_anomaly = 357.529_11 + t * (35_999.050_29 - 0.000_153_7 * t);
        let eccentricity = 0.016_708_634 - t * (0.000_042_037 + 0.000_000_126_7 * t);
        let m = mean_anomaly.to_radians();
        let centre = m.sin() * (1.914_602 - t * (0.004_817 + 0.000_014 * t))
            + (2.0 * m).sin() * (0.019_993 - 0.000_101 * t)
            + (3.0 * m).sin() * 0.000_289;
        let omega = (125.04 - 1_934.136 * t).to_radians();
        let apparent_long = (mean_long + centre - 0.005_69 - 0.004_78 * omega.sin()).to_radians();
        let mean_obliquity =
            23.0 + (26.0 + (21.448 - t * (46.815 + t * (0.000_59 - t * 0.001_813))) / 60.0) / 60.0;
        let obliquity = (mean_obliquity + 0.002_56 * omega.cos()).to_radians();

        let declination = (obliquity.sin() * apparent_long.sin()).asin();
        let y = (obliquity / 2.0).tan().powi(2);
        let l0 = mean_long.to_radians();
        let e = eccentricity;
        let equation_of_time = MINUTES_PER_DEG
            * (y * (2.0 * l0).sin() - 2.0 * e * m.sin() + 4.0 * e * y * m.sin() * (2.0 * l0).cos()
                - 0.5 * y * y * (4.0 * l0).sin()
                - 1.25 * e * e * (2.0 * m).sin())
            .to_degrees();
        Self {
            declination,
            equation_of_time,
        }
    }
}

/// Sites and a helper the sun's and the window's tests share.
#[cfg(test)]
pub mod testing {
    use super::{Day, Site, day};
    use jiff::Timestamp;

    pub const SAO_PAULO: Site = Site {
        latitude: -23.55,
        longitude: -46.63,
    };
    pub const FORTALEZA: Site = Site {
        latitude: -3.73,
        longitude: -38.53,
    };
    pub const SYDNEY: Site = Site {
        latitude: -33.8688,
        longitude: 151.2093,
    };
    pub const HONOLULU: Site = Site {
        latitude: 21.3069,
        longitude: -157.8583,
    };
    pub const TROMSO: Site = Site {
        latitude: 69.6492,
        longitude: 18.9553,
    };

    /// Sunrise and sunset on `date`.
    ///
    /// # Panics
    ///
    /// If `date` is not a date, or falls where a timestamp cannot reach. Test input only.
    #[must_use]
    pub fn rises(site: Site, date: &str) -> (Timestamp, Timestamp) {
        let Day { sunrise, sunset } = day(site, date.parse().unwrap()).unwrap();
        (sunrise, sunset)
    }
}

#[cfg(test)]
mod tests {
    use super::testing::{FORTALEZA, SAO_PAULO, SYDNEY, rises};
    use super::*;

    fn at(date: &str, hms: &str) -> Timestamp {
        format!("{date}T{hms}Z").parse().unwrap()
    }

    fn assert_near(got: Timestamp, want: Timestamp, within_s: i64, what: &str) {
        let off = got.duration_since(want).as_secs().abs();
        assert!(
            off <= within_s,
            "{what}: got {got}, want {want} ({off}s off)"
        );
    }

    /// NOAA's solar calculator tables, 2026, local UTC−3 converted to UTC.
    /// Sunrise and sunset are published to the minute, so ±30 s of rounding;
    /// the formula, evaluated once at solar noon, adds under 15 s. A radians
    /// slip or a flipped sign lands hours off.
    const ROUNDED_S: i64 = 60;

    #[test]
    fn solstices_match_noaa() {
        // Between the solstices São Paulo's day changes length by almost 3 h;
        // Fortaleza's, near the equator, by under half an hour, so its times
        // rest on longitude and the equation of time more than on latitude.
        let table = [
            (SAO_PAULO, "2026-06-21", "09:48:00", "20:29:00"),
            (SAO_PAULO, "2026-12-21", "08:17:00", "21:52:00"),
            (FORTALEZA, "2026-06-21", "08:39:00", "20:33:00"),
            (FORTALEZA, "2026-12-21", "08:22:00", "20:42:00"),
        ];
        for (site, date, sunrise, sunset) in table {
            let (up, down) = rises(site, date);
            let what = format!("{site:?} {date}");
            assert_near(up, at(date, sunrise), ROUNDED_S, &format!("{what} sunrise"));
            assert_near(down, at(date, sunset), ROUNDED_S, &format!("{what} sunset"));
        }
    }

    #[test]
    fn solar_noon_follows_the_equation_of_time() {
        // NOAA publishes solar noon to the second. Near its extremes the
        // sundial leads the clock by over 16 min (3 Nov) and lags by over 14
        // (11 Feb); at the solstices it is under 2, too small for the sunrise
        // test to notice it missing.
        let table = [
            (SAO_PAULO, "2026-11-03", "14:50:02"),
            (SAO_PAULO, "2026-02-11", "15:20:45"),
            (FORTALEZA, "2026-11-03", "14:17:38"),
            (FORTALEZA, "2026-02-11", "14:48:21"),
        ];
        for (site, date, noon) in table {
            let (up, down) = rises(site, date);
            let mid = up + up.duration_until(down) / 2;
            assert_near(
                mid,
                at(date, noon),
                20,
                &format!("{site:?} {date} solar noon"),
            );
        }
    }

    #[test]
    fn far_east_sunrise_falls_on_the_previous_utc_date() {
        // Sydney, winter solstice: 07:00 and 16:54 local, UTC+10. A longitude
        // sign flipped, or a day computed from UTC midnight, misses by hours.
        let (up, down) = rises(SYDNEY, "2026-06-21");
        assert_near(up, at("2026-06-20", "21:00:00"), 120, "sunrise");
        assert_near(down, at("2026-06-21", "06:54:00"), 120, "sunset");
    }

    #[test]
    fn the_sun_rises_and_sets_every_day_up_to_the_latitude_bound() {
        // What the config's refusal rests on. A year of dates covers both
        // solstices, which is where the bound is tight.
        let mut date: Date = "2026-01-01".parse().unwrap();
        for _ in 0..365 {
            for latitude in [LATITUDE_DEG_MAX, -LATITUDE_DEG_MAX] {
                let site = Site {
                    latitude,
                    longitude: 0.0,
                };
                // A clamped hour angle gives a day of no length or of a full
                // 24 h, so both ways past the bound are caught here.
                let Day { sunrise, sunset } = day(site, date).unwrap();
                let span = sunrise.duration_until(sunset);
                assert!(
                    span > SignedDuration::ZERO && span < SignedDuration::from_hours(24),
                    "{latitude} on {date}: {span:?}"
                );
            }
            date = date.tomorrow().unwrap();
        }
    }
}
