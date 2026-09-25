//! Command timing

use crate::error;

struct StopwatchTime {
    now: std::time::SystemTime,
    self_user: std::time::Duration,
    self_system: std::time::Duration,
    children_user: std::time::Duration,
    children_system: std::time::Duration,
}

impl StopwatchTime {
    #[allow(clippy::unchecked_time_subtraction)]
    fn minus(&self, other: &Self) -> Result<StopwatchTiming, error::Error> {
        let user = (self.self_user - other.self_user) + (self.children_user - other.children_user);
        let system =
            (self.self_system - other.self_system) + (self.children_system - other.children_system);

        Ok(StopwatchTiming {
            wall: self.now.duration_since(other.now)?,
            user,
            system,
        })
    }
}

pub(crate) struct Stopwatch {
    start: StopwatchTime,
}

impl Stopwatch {
    pub fn stop(&self) -> Result<StopwatchTiming, error::Error> {
        let end = get_current_stopwatch_time()?;
        end.minus(&self.start)
    }
}
pub(crate) struct StopwatchTiming {
    pub wall: std::time::Duration,
    pub user: std::time::Duration,
    pub system: std::time::Duration,
}

pub(crate) fn start_timing() -> Result<Stopwatch, error::Error> {
    Ok(Stopwatch {
        start: get_current_stopwatch_time()?,
    })
}

fn get_current_stopwatch_time() -> Result<StopwatchTime, error::Error> {
    let now = std::time::SystemTime::now();
    let (self_user, self_system) = crate::sys::resource::get_self_user_and_system_time()?;
    let (children_user, children_system) =
        crate::sys::resource::get_children_user_and_system_time()?;

    Ok(StopwatchTime {
        now,
        self_user,
        self_system,
        children_user,
        children_system,
    })
}

/// Bash's default `TIMEFORMAT`.
pub(crate) const BASH_TIMEFORMAT: &str = "\nreal\t%3lR\nuser\t%3lU\nsys\t%3lS";

/// The format `time -p` uses, whatever `TIMEFORMAT` holds.
pub(crate) const POSIX_TIMEFORMAT: &str = "real %2R\nuser %2U\nsys %2S";

/// Formats `timing` as bash's `print_formatted_time` does: `%%` is a percent sign, `%[p][l]R`,
/// `U` and `S` the real, user and system times with `p` decimal places (default 3, at most 6) and,
/// with `l`, minutes, and `%P` the CPU percentage. Returns the error message for an invalid
/// format character.
pub(crate) fn format_timing(format: &str, timing: &StopwatchTiming) -> Result<String, String> {
    use std::fmt::Write as _;

    let mut result = String::new();
    let mut chars = format.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '%' || chars.peek().is_none() {
            result.push(c);
            continue;
        }
        if chars.next_if_eq(&'%').is_some() {
            result.push('%');
            continue;
        }
        if chars.next_if_eq(&'P').is_some() {
            let cpu = (timing.user + timing.system)
                .as_micros()
                .saturating_mul(10000)
                .checked_div(timing.wall.as_micros())
                .unwrap_or(0);
            let _ = write!(result, "{}.{:02}", cpu / 100, cpu % 100);
            continue;
        }
        let precision = match chars.peek().and_then(|c| c.to_digit(10)) {
            Some(digit) => {
                chars.next();
                digit.min(6)
            }
            None => 3,
        };
        let long = chars.next_if_eq(&'l').is_some();
        let duration = match chars.next() {
            Some('R' | 'E') => timing.wall,
            Some('U') => timing.user,
            Some('S') => timing.system,
            other => {
                return Err(format!(
                    "TIMEFORMAT: `{}': invalid format character",
                    other.unwrap_or_default()
                ));
            }
        };
        let mut seconds = duration.as_secs();
        if long {
            let _ = write!(result, "{}m", seconds / 60);
            seconds %= 60;
        }
        let _ = write!(result, "{seconds}");
        if precision > 0 {
            let micros = format!("{:06}", duration.subsec_micros());
            result.push('.');
            result.extend(micros.chars().take(precision as usize));
        }
        if long {
            result.push('s');
        }
    }
    Ok(result)
}

/// Format the given duration in a non-POSIX-y way.
///
/// # Arguments
///
/// * `duration` - The duration to format.
pub fn format_duration_non_posixly(duration: &std::time::Duration) -> String {
    let minutes = duration.as_secs() / 60;
    let seconds = duration.as_secs() % 60;
    let millis = duration.subsec_millis();
    format!("{minutes}m{seconds}.{millis:03}s")
}

/// Format the given duration in a POSIX-y way.
///
/// # Arguments
///
/// * `duration` - The duration to format.
pub fn format_duration_posixly(duration: &std::time::Duration) -> String {
    let seconds = duration.as_secs();
    let ten_millis = duration.subsec_millis() / 10;
    format!("{seconds}.{ten_millis:02}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn test_format_time() {
        assert_eq!(
            format_duration_non_posixly(&Duration::from_millis(0)),
            "0m0.000s"
        );
        assert_eq!(
            format_duration_non_posixly(&Duration::from_millis(1)),
            "0m0.001s"
        );
        assert_eq!(
            format_duration_non_posixly(&Duration::from_millis(123)),
            "0m0.123s"
        );
        assert_eq!(
            format_duration_non_posixly(&Duration::from_millis(1234)),
            "0m1.234s"
        );
        assert_eq!(
            format_duration_non_posixly(&Duration::from_millis(12345)),
            "0m12.345s"
        );
        assert_eq!(
            format_duration_non_posixly(&Duration::from_millis(123_456)),
            "2m3.456s"
        );
        assert_eq!(
            format_duration_non_posixly(&Duration::from_millis(1_234_567)),
            "20m34.567s"
        );

        assert_eq!(
            format_duration_non_posixly(&Duration::from_micros(1)),
            "0m0.000s"
        );
        assert_eq!(
            format_duration_non_posixly(&Duration::from_micros(999)),
            "0m0.000s"
        );
        assert_eq!(
            format_duration_non_posixly(&Duration::from_micros(1001)),
            "0m0.001s"
        );
    }
}
