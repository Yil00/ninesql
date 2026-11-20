//! Log fields for errors (ADR-0029 §3.2): a variant name (with SQLite's extended result code
//! where one is at hand), never a message.
//!
//! A [`DriverError`] message can quote statement text, a path or a value, so `debug` and above
//! log only these fields; the message itself goes to the UI.

use ninesql_driver_api::DriverError;

/// The variant name of `e`, for the `driver_error_kind` field.
pub(crate) fn error_kind(e: &DriverError) -> &'static str {
    match e {
        DriverError::InvalidParams(_) => "InvalidParams",
        DriverError::Connect(_) => "Connect",
        DriverError::Auth(_) => "Auth",
        DriverError::Tls(_) => "Tls",
        DriverError::ReadOnly(_) => "ReadOnly",
        DriverError::Query { .. } => "Query",
        DriverError::Timeout(_) => "Timeout",
        DriverError::Cancelled => "Cancelled",
        DriverError::Unsupported(_) => "Unsupported",
        DriverError::MultipleStatements(_) => "MultipleStatements",
        DriverError::Busy => "Busy",
        DriverError::Closed(_) => "Closed",
        DriverError::Protocol(_) => "Protocol",
        DriverError::Io(_) => "Io",
        _ => "Other",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kind_never_carries_the_message() {
        let e = DriverError::Query {
            code: Some("2067".into()),
            message: "UNIQUE constraint failed: t.k (secret-value)".into(),
            position: None,
        };
        assert_eq!(error_kind(&e), "Query");
        assert_eq!(error_kind(&DriverError::Io("/home/u/x.db".into())), "Io");
    }
}
