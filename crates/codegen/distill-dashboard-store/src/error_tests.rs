use super::*;

fn sqlite_failure(code: rusqlite::ErrorCode, message: Option<&str>) -> rusqlite::Error {
    rusqlite::Error::SqliteFailure(
        rusqlite::ffi::Error {
            code,
            extended_code: 0,
        },
        message.map(str::to_owned),
    )
}

#[test]
fn unusable_classifier_catches_codes_and_specific_messages() {
    for error in [
        sqlite_failure(rusqlite::ErrorCode::DatabaseCorrupt, None),
        sqlite_failure(rusqlite::ErrorCode::NotADatabase, None),
        sqlite_failure(
            rusqlite::ErrorCode::Unknown,
            Some("database disk image is malformed"),
        ),
        sqlite_failure(
            rusqlite::ErrorCode::Unknown,
            Some("malformed database schema (members)"),
        ),
        sqlite_failure(
            rusqlite::ErrorCode::Unknown,
            Some("file is encrypted or is not a database"),
        ),
    ] {
        assert!(matches!(
            classify_unusable(error),
            StoreError::Unusable { .. }
        ));
    }

    for message in ["malformed MATCH expression", "database is corrupt"] {
        let error = sqlite_failure(rusqlite::ErrorCode::Unknown, Some(message));
        assert!(matches!(classify_unusable(error), StoreError::Sqlite(_)));
    }
}
