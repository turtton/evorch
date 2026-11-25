//! Local calendar days as day numbers, so ranges and weeks need no time zone database.
//!
//! The ledger stores each request's local day (computed by SQLite), and these
//! helpers only do calendar arithmetic on those labels.

/// A calendar day, counted from 1970-01-01.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct LocalDay(i64);

impl LocalDay {
    /// Parses `YYYY-MM-DD`.
    pub fn parse(value: &str) -> Option<Self> {
        let mut parts = value.splitn(3, '-');
        let year = parts.next()?.parse::<i64>().ok()?;
        let month = parts.next()?.parse::<u32>().ok()?;
        let day = parts.next()?.parse::<u32>().ok()?;
        ((1..=12).contains(&month) && (1..=days_in_month(year, month)).contains(&day))
            .then(|| Self(days_from_civil(year, month, day)))
    }

    pub fn from_ymd(year: i64, month: u32, day: u32) -> Self {
        Self(days_from_civil(year, month, day))
    }

    pub fn ymd(self) -> (i64, u32, u32) {
        civil_from_days(self.0)
    }

    pub fn add_days(self, days: i64) -> Self {
        Self(self.0 + days)
    }

    pub fn days_since(self, earlier: Self) -> i64 {
        self.0 - earlier.0
    }

    /// Monday of this day's ISO week.
    pub fn week_start(self) -> Self {
        // 1970-01-01 was a Thursday, three days after a Monday.
        Self(self.0 - (self.0 + 3).rem_euclid(7))
    }

    pub fn month_start(self) -> Self {
        let (year, month, _) = self.ymd();
        Self::from_ymd(year, month, 1)
    }

    /// `YYYY-MM`.
    pub fn month_label(self) -> String {
        let (year, month, _) = self.ymd();
        format!("{year:04}-{month:02}")
    }
}

impl std::fmt::Display for LocalDay {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let (year, month, day) = self.ymd();
        write!(formatter, "{year:04}-{month:02}-{day:02}")
    }
}

fn days_in_month(year: i64, month: u32) -> u32 {
    match month {
        2 if year % 4 == 0 && (year % 100 != 0 || year % 400 == 0) => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

// Howard Hinnant's proleptic Gregorian conversions.
fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let year_of_era = year - era * 400;
    let month_index = i64::from((month + 9) % 12);
    let day_of_year = (153 * month_index + 2) / 5 + i64::from(day) - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let days = days + 719_468;
    let era = days.div_euclid(146_097);
    let day_of_era = days - era * 146_097;
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
    // Both values are bounded by the calendar arithmetic above.
    (year, month as u32, day as u32)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn labels_round_trip_across_leap_and_century_boundaries() {
        for label in [
            "1970-01-01",
            "1999-12-31",
            "2000-02-29",
            "2024-02-29",
            "2026-10-04",
            "2100-03-01",
        ] {
            assert_eq!(LocalDay::parse(label).unwrap().to_string(), label);
        }
        assert_eq!(LocalDay::parse("2026-02-29"), None);
        assert_eq!(LocalDay::parse("2026-13-01"), None);
    }

    #[test]
    fn weeks_start_on_monday_and_months_on_the_first() {
        // 2026-10-04 is a Sunday.
        let sunday = LocalDay::parse("2026-10-04").unwrap();
        assert_eq!(sunday.week_start().to_string(), "2026-09-28");
        assert_eq!(sunday.add_days(1).week_start().to_string(), "2026-10-05");
        assert_eq!(sunday.month_start().to_string(), "2026-10-01");
        assert_eq!(sunday.month_label(), "2026-10");
        assert_eq!(sunday.days_since(sunday.add_days(-6)), 6);
    }
}
