use std::time::Duration;

/// Delay after a failed attempt, keyed by `attempt_count` after increment.
pub fn retry_delay(attempt_count_after_failure: u32) -> Duration {
    match attempt_count_after_failure {
        0 => Duration::from_secs(0),
        1 => Duration::from_secs(5),
        2 => Duration::from_secs(15),
        3 => Duration::from_secs(30),
        4 => Duration::from_secs(60),
        _ => Duration::from_secs(300),
    }
}

#[cfg(test)]
mod tests {
    use super::retry_delay;
    use std::time::Duration;

    #[test]
    fn retry_delay_table() {
        assert_eq!(retry_delay(1), Duration::from_secs(5));
        assert_eq!(retry_delay(2), Duration::from_secs(15));
        assert_eq!(retry_delay(3), Duration::from_secs(30));
        assert_eq!(retry_delay(4), Duration::from_secs(60));
        assert_eq!(retry_delay(5), Duration::from_secs(300));
        assert_eq!(retry_delay(99), Duration::from_secs(300));
    }
}
