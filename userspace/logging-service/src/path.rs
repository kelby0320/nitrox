//! Classifying a resolved log path into `(tier, principal, source)`.
//!
//! A client obtains a log endpoint by resolving a path under the logging service's mount; the
//! path *is* the identity (see `docs/architecture/logging.md`). The suffix below the mount,
//! e.g. `system/heartbeat/worker`, parses as:
//!
//! - component 0 = **tier** (`system` / `app`; `kernel` rings are not opened this way)
//! - component 1 = **principal** (required, non-empty)
//! - the remainder = an optional **source** sub-label
//!
//! Authority is *not* checked here: a caller can only resolve paths its namespace binding
//! permits. This is pure syntax.

/// The tiers and their names, from `librsproto`. `TIER_KERNEL` is never produced by [`classify`].
pub use librsproto::log::{TIER_APP, TIER_KERNEL, TIER_SYSTEM, tier_name};

/// A classified log path.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Classified<'a> {
    /// `TIER_SYSTEM` or `TIER_APP`.
    pub tier: u8,
    /// The required, non-empty second path component.
    pub principal: &'a str,
    /// Optional self-declared sub-label under `principal`.
    pub source: Option<&'a str>,
}

/// Parse a resolve suffix into `(tier, principal, source)`, or `None` if the tier is
/// unknown / not resolvable or the principal is missing.
pub fn classify(suffix: &str) -> Option<Classified<'_>> {
    let s = suffix.trim_start_matches('/');
    let mut it = s.splitn(3, '/');
    let tier = match it.next()? {
        "system" => TIER_SYSTEM,
        "app" => TIER_APP,
        _ => return None, // unknown tier, or `kernel` (not resolvable)
    };
    let principal = it.next().filter(|p| !p.is_empty())?;
    let source = it.next().filter(|s| !s.is_empty());
    Some(Classified { tier, principal, source })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn system_principal() {
        assert_eq!(
            classify("system/heartbeat"),
            Some(Classified { tier: TIER_SYSTEM, principal: "heartbeat", source: None })
        );
    }

    #[test]
    fn system_principal_with_source() {
        assert_eq!(
            classify("system/heartbeat/worker"),
            Some(Classified { tier: TIER_SYSTEM, principal: "heartbeat", source: Some("worker") })
        );
    }

    #[test]
    fn app_tier() {
        let c = classify("app/session1/editor").unwrap();
        assert_eq!(c.tier, TIER_APP);
        assert_eq!(c.principal, "session1");
        assert_eq!(c.source, Some("editor"));
    }

    #[test]
    fn leading_slash_tolerated() {
        assert_eq!(classify("/system/foo").unwrap().principal, "foo");
    }

    #[test]
    fn rejects_missing_principal() {
        assert!(classify("system").is_none());
        assert!(classify("system/").is_none());
    }

    #[test]
    fn rejects_unknown_and_kernel_tier() {
        assert!(classify("bogus/x").is_none());
        assert!(classify("kernel/x").is_none()); // kernel rings aren't resolved
        assert!(classify("").is_none());
    }
}
