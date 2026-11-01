use chrono::{Datelike, Duration, NaiveDate, Weekday};

/// The first day of the week chosen in Windows' region settings.
#[cfg(windows)]
pub fn first_day_of_week() -> Weekday {
    use windows_sys::Win32::Globalization::{
        GetLocaleInfoEx, LOCALE_IFIRSTDAYOFWEEK, LOCALE_RETURN_NUMBER,
    };
    let mut value = [0u16; 2];
    let written = unsafe {
        GetLocaleInfoEx(
            std::ptr::null(),
            LOCALE_IFIRSTDAYOFWEEK | LOCALE_RETURN_NUMBER,
            value.as_mut_ptr(),
            2,
        )
    };
    // Windows numbers from Monday as 0.
    match (written, value[0]) {
        (0, _) => Weekday::Mon,
        (_, 0) => Weekday::Mon,
        (_, 1) => Weekday::Tue,
        (_, 2) => Weekday::Wed,
        (_, 3) => Weekday::Thu,
        (_, 4) => Weekday::Fri,
        (_, 5) => Weekday::Sat,
        (_, 6) => Weekday::Sun,
        _ => Weekday::Mon,
    }
}

#[cfg(not(windows))]
pub fn first_day_of_week() -> Weekday {
    Weekday::Mon
}

pub fn weekday_letters(first: Weekday) -> Vec<&'static str> {
    let letters = ["M", "T", "W", "T", "F", "S", "S"];
    let offset = first.num_days_from_monday() as usize;
    (0..7).map(|day| letters[(day + offset) % 7]).collect()
}

pub fn month_days(year: i32, month: u32, first_day: Weekday) -> Option<Vec<NaiveDate>> {
    let first = NaiveDate::from_ymd_opt(year, month, 1)?;
    let back = first.weekday().days_since(first_day);
    let start = first.checked_sub_signed(Duration::days(i64::from(back)))?;
    (0..42)
        .map(|day| start.checked_add_signed(Duration::days(day)))
        .collect()
}

pub fn shift_month(year: i32, month: u32, delta: i32) -> Option<(i32, u32)> {
    let serial = year
        .checked_mul(12)?
        .checked_add(month as i32 - 1)?
        .checked_add(delta)?;
    let year = serial.div_euclid(12);
    let month = serial.rem_euclid(12) as u32 + 1;
    NaiveDate::from_ymd_opt(year, month, 1).map(|_| (year, month))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn calendar_has_six_monday_first_weeks_and_leap_day() {
        let days = month_days(2028, 2, Weekday::Mon).unwrap();
        assert_eq!(days.len(), 42);
        assert_eq!(days[0].weekday().num_days_from_monday(), 0);
        assert!(days.contains(&NaiveDate::from_ymd_opt(2028, 2, 29).unwrap()));
        assert_eq!(shift_month(2026, 12, 1), Some((2027, 1)));
        assert_eq!(shift_month(2026, 1, -1), Some((2025, 12)));
    }

    #[test]
    fn weeks_can_start_on_any_day() {
        let days = month_days(2026, 11, Weekday::Sun).unwrap();
        // November 2026 begins on a Sunday, so nothing from October is shown.
        assert_eq!(days[0], NaiveDate::from_ymd_opt(2026, 11, 1).unwrap());
        let days = month_days(2026, 10, Weekday::Sun).unwrap();
        assert_eq!(days[0].weekday(), Weekday::Sun);
        assert!(days[0] <= NaiveDate::from_ymd_opt(2026, 10, 1).unwrap());
        assert_eq!(
            weekday_letters(Weekday::Sun),
            ["S", "M", "T", "W", "T", "F", "S"]
        );
    }
}
