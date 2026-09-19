// Modified for Distill by Samuel Fajreldines, 2026.
//! `FEATURES` is the source of truth and the operator tables are hand-maintained mirrors with no compile-time check of their own.
//! This test is that check.

use distill_shell::agent::config::FEATURES;

const ENTERPRISE: &str = include_str!("../docs/internal/25-enterprise.md");
const ENV_VARS: &str = include_str!("../docs/internal/22-environment-variables.md");

#[test]
fn every_registered_feature_reaches_the_operator() {
    for spec in FEATURES {
        assert!(
            ENTERPRISE.contains(&format!("`{}`", spec.key)),
            "{} has no row in the 25-enterprise.md pinning table",
            spec.key,
        );
        assert!(
            ENV_VARS.contains(&format!("`{}`", spec.env)),
            "{} is undocumented in 22-environment-variables.md",
            spec.env,
        );
    }
}
