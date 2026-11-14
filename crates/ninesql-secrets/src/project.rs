//! Project scope of stored secrets (ADR-0017 §4).
//!
//! Every persistent store scopes its items by the project's stable UUID (the `id` of
//! `project.json`), never by the project name: a renamed project keeps its secrets, and two
//! projects with different ids never read or overwrite each other's secrets.

/// Error returned when a store is not given a project UUID.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("invalid project identifier: a hyphenated UUID (from project.json) is expected")]
pub struct InvalidProjectId;

/// Validates a hyphenated UUID (`8-4-4-4-12` hex digits) and returns it lower-cased.
///
/// # Errors
/// [`InvalidProjectId`] for anything else, so a project name cannot be passed by mistake.
pub(crate) fn normalize_project_id(project_id: &str) -> Result<String, InvalidProjectId> {
    let groups: Vec<&str> = project_id.split('-').collect();
    let valid = groups.len() == 5
        && groups
            .iter()
            .zip([8, 4, 4, 4, 12])
            .all(|(g, len)| g.len() == len && g.bytes().all(|b| b.is_ascii_hexdigit()));
    if valid {
        Ok(project_id.to_ascii_lowercase())
    } else {
        Err(InvalidProjectId)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)] // Tests may panic (AGENTS.md).
mod tests {
    use super::*;

    #[test]
    fn uuid_only() {
        assert_eq!(
            normalize_project_id("3F2A6C1E-0B7D-4C51-9E2A-6D8F0A1B2C3D").unwrap(),
            "3f2a6c1e-0b7d-4c51-9e2a-6d8f0a1b2c3d"
        );
        for bad in [
            "General",
            "",
            "3f2a6c1e0b7d4c519e2a6d8f0a1b2c3d",
            "3f2a6c1e-0b7d-4c51-9e2a-6d8f0a1b2c3",
            "3f2a6c1e-0b7d-4c51-9e2a-6d8f0a1b2c3g",
            "3f2a6c1e-0b7d-4c51-9e2a-6d8f0a1b2c3d-",
        ] {
            assert!(normalize_project_id(bad).is_err(), "{bad:?}");
        }
    }
}
