use chrono::{Datelike, Duration, NaiveDate};

pub fn month_days(year: i32, month: u32) -> Option<Vec<NaiveDate>> {
    let first = NaiveDate::from_ymd_opt(year, month, 1)?;
    let start =
        first.checked_sub_signed(Duration::days(first.weekday().num_days_from_monday() as i64))?;
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
        let days = month_days(2028, 2).unwrap();
        assert_eq!(days.len(), 42);
        assert_eq!(days[0].weekday().num_days_from_monday(), 0);
        assert!(days.contains(&NaiveDate::from_ymd_opt(2028, 2, 29).unwrap()));
        assert_eq!(shift_month(2026, 12, 1), Some((2027, 1)));
        assert_eq!(shift_month(2026, 1, -1), Some((2025, 12)));
    }
}
