use chrono::{DateTime, SecondsFormat, Utc};

pub fn now_rfc3339() -> String {
    Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true)
}

pub fn datetime_rfc3339(value: DateTime<Utc>) -> String {
    value.to_rfc3339_opts(SecondsFormat::Millis, true)
}

pub fn parse_rfc3339(value: &str) -> crate::error::Result<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(value)
        .map(|dt| dt.with_timezone(&Utc))
        .map_err(|error| {
            crate::error::AppError::other(format!("invalid RFC3339 timestamp '{value}': {error}"))
        })
}
