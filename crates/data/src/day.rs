//! Calendar days in UTC, the unit the event store is partitioned by.

use std::fmt;
use std::str::FromStr;

/// Seconds in a day.
pub const DAY_SECONDS: u64 = 86_400;

/// A UTC calendar day, counted in days since 1970-01-01.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Day(pub u32);

impl Day {
    /// The day a Unix time falls on.
    pub fn of_time(unix_seconds: u64) -> Self {
        Day((unix_seconds / DAY_SECONDS) as u32)
    }

    /// Unix time of the day's first second.
    pub fn start(self) -> u64 {
        u64::from(self.0) * DAY_SECONDS
    }

    /// Unix time of the next day's first second.
    pub fn end(self) -> u64 {
        self.start() + DAY_SECONDS
    }

    pub fn next(self) -> Self {
        Day(self.0 + 1)
    }

    /// The day `days` earlier, stopping at 1970-01-01.
    pub fn minus(self, days: u32) -> Self {
        Day(self.0.saturating_sub(days))
    }

    /// Year, month and day of the month.
    pub fn ymd(self) -> (i32, u32, u32) {
        // Days-to-civil conversion for the proleptic Gregorian calendar.
        let z = i64::from(self.0) + 719_468;
        let era = z.div_euclid(146_097);
        let day_of_era = z.rem_euclid(146_097);
        let year_of_era =
            (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
        let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
        let month_index = (5 * day_of_year + 2) / 153;
        let day = day_of_year - (153 * month_index + 2) / 5 + 1;
        let month = if month_index < 10 {
            month_index + 3
        } else {
            month_index - 9
        };
        let year = year_of_era + era * 400 + i64::from(month <= 2);
        (year as i32, month as u32, day as u32)
    }

    /// The day with this date, if the date exists and is not before 1970.
    pub fn from_ymd(year: i32, month: u32, day: u32) -> Option<Self> {
        if !(1..=12).contains(&month) || day == 0 || year < 1970 {
            return None;
        }
        let y = i64::from(year) - i64::from(month <= 2);
        let era = y.div_euclid(400);
        let year_of_era = y.rem_euclid(400);
        let month_index = (i64::from(month) + 9) % 12;
        let day_of_year = (153 * month_index + 2) / 5 + i64::from(day) - 1;
        let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
        let days = era * 146_097 + day_of_era - 719_468;
        let candidate = Day(u32::try_from(days).ok()?);
        // Rejects dates such as 30 February, which the formula rolls over.
        (candidate.ymd() == (year, month, day)).then_some(candidate)
    }
}

impl fmt::Display for Day {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (year, month, day) = self.ymd();
        write!(f, "{year:04}-{month:02}-{day:02}")
    }
}

impl FromStr for Day {
    type Err = String;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        let bad = || format!("`{text}` is not a date; write it as YYYY-MM-DD");
        let parts: Vec<&str> = text.split('-').collect();
        let [year, month, day] = parts.as_slice() else {
            return Err(bad());
        };
        if year.len() != 4 || month.len() != 2 || day.len() != 2 {
            return Err(bad());
        }
        Day::from_ymd(
            year.parse().map_err(|_| bad())?,
            month.parse().map_err(|_| bad())?,
            day.parse().map_err(|_| bad())?,
        )
        .ok_or_else(bad)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_dates_convert_both_ways() {
        for (days, text) in [
            (0, "1970-01-01"),
            (59, "1970-03-01"),
            (11_016, "2000-02-29"),
            (11_017, "2000-03-01"),
            (18_628, "2021-01-01"),
            (19_723, "2024-01-01"),
            (19_782, "2024-02-29"),
            (20_454, "2026-01-01"),
            (20_769, "2026-11-12"),
        ] {
            assert_eq!(Day(days).to_string(), text);
            assert_eq!(text.parse::<Day>(), Ok(Day(days)));
        }
    }

    #[test]
    fn every_day_of_a_century_survives_a_round_trip() {
        let mut previous = Day(0).ymd();
        for days in 1..36_600 {
            let day = Day(days);
            let (y, m, d) = day.ymd();
            assert_eq!(Day::from_ymd(y, m, d), Some(day));
            // Consecutive days are consecutive dates.
            let stepped = (y, m, d) == (previous.0, previous.1, previous.2 + 1)
                || (d == 1 && (m == previous.1 + 1 || (m == 1 && y == previous.0 + 1)));
            assert!(stepped, "{previous:?} -> {:?}", (y, m, d));
            previous = (y, m, d);
        }
    }

    #[test]
    fn dates_that_do_not_exist_are_refused() {
        for text in [
            "2026-02-30",
            "2025-02-29",
            "2026-13-01",
            "2026-00-10",
            "2026-01-00",
            "1969-12-31",
            "2026-1-1",
            "20260101",
            "yesterday",
            "",
        ] {
            assert!(text.parse::<Day>().is_err(), "{text}");
        }
        assert!("2026-02-30"
            .parse::<Day>()
            .unwrap_err()
            .contains("YYYY-MM-DD"));
    }

    #[test]
    fn a_time_belongs_to_the_day_it_falls_in() {
        let day: Day = "2026-11-12".parse().unwrap();
        assert_eq!(Day::of_time(day.start()), day);
        assert_eq!(Day::of_time(day.end() - 1), day);
        assert_eq!(Day::of_time(day.end()), day.next());
        assert_eq!(day.minus(11).to_string(), "2026-11-01");
        assert_eq!(Day(3).minus(10), Day(0));
    }
}
