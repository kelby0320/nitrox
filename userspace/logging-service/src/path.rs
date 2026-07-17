//! Classifying a resolved log path into `(tier, principal, source)`.
//!
//! A client obtains a log endpoint by resolving a path under the logging service's
//! mount — the path *is* the identity (see `docs/architecture/logging.md`). The
//! forwarded resolve delivers the suffix below the mount, e.g. `system/heartbeat` or
//! `system/heartbeat/worker`. This parses it:
//!
//! - component 0 = **tier** (`system` / `app`; `kernel` is not resolvable — the kernel
//!   rings are not opened this way)
//! - component 1 = **principal** (required, non-empty)
//! - the remainder = an optional **source** sub-label
//!
//! Authority is *not* checked here — a caller can only resolve paths its namespace
//! binding permits, so reaching `system/*` at all already implies the right to. This is
//! pure syntax.

/// Kernel tier — the kernel `klog`/audit rings. Never produced by [`classify`] (kernel
/// rings are not resolved); defined for the record's `tier` field.
pub const TIER_KERNEL: u8 = 0;
/// System tier — supervised services (service-mgr / init mint these).
pub const TIER_SYSTEM: u8 = 1;
/// Application tier — user apps (session-mgr, later).
pub const TIER_APP: u8 = 2;

/// A classified log path.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Classified<'a> {
    pub tier: u8,
    pub principal: &'a str,
    /// Optional self-declared sub-label under `principal`.
    pub source: Option<&'a str>,
}

/// Human-readable tier name (for the serial sink / logging).
pub fn tier_name(tier: u8) -> &'static str {
    match tier {
        TIER_KERNEL => "kernel",
        TIER_SYSTEM => "system",
        TIER_APP => "app",
        _ => "?",
    }
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
            Some(Classified {
                tier: TIER_SYSTEM,
                principal: "heartbeat",
                source: Some("worker"),
            })
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
