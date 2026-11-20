//! Log fields for errors (ADR-0029 §3.2): a variant name and an engine code, never a message.
//!
//! A [`DriverError`] message can quote an engine's values (a duplicate key, a failing cast),
//! statement text, a host or a path, so `debug` and above log only these fields; the message
//! itself goes to the UI.

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

/// The engine error code of a query error (SQLSTATE, MySQL error number), for the `code` field;
/// only a short ASCII alphanumeric code, so a server cannot smuggle text through it.
pub(crate) fn error_code(e: &DriverError) -> Option<&str> {
    match e {
        DriverError::Query { code: Some(c), .. }
            if !c.is_empty() && c.len() <= 8 && c.bytes().all(|b| b.is_ascii_alphanumeric()) =>
        {
            Some(c)
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kind_and_code_never_carry_the_message() {
        let e = DriverError::Query {
            code: Some("23505".into()),
            message: "duplicate key value: Key (k)=(secret-value) already exists.".into(),
            position: None,
        };
        assert_eq!(error_kind(&e), "Query");
        assert_eq!(error_code(&e), Some("23505"));
        let forged = DriverError::Query {
            code: Some("k=(secret-value)".into()),
            message: String::new(),
            position: None,
        };
        assert_eq!(error_code(&forged), None);
        assert_eq!(
            error_kind(&DriverError::Connect("u:pw@h".into())),
            "Connect"
        );
        assert_eq!(error_code(&DriverError::Busy), None);
    }
}
