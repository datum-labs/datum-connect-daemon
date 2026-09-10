//! Shared kube error classification helpers.

/// Returns true if `err` is an HTTP 401 (unauthorized).
pub fn is_unauthorized(err: &kube::Error) -> bool {
    matches!(err, kube::Error::Api(e) if e.code == 401)
}

/// Returns true if `err` is an HTTP 404 (not found).
pub fn is_not_found(err: &kube::Error) -> bool {
    matches!(err, kube::Error::Api(e) if e.code == 404)
}

/// Returns true if `err` is the operator's transient quota-check timeout
/// (a 403 whose message says "took too long to be checked against your quota").
/// Distinct from real quota exhaustion, which produces a different message.
pub fn is_quota_check_timeout(err: &kube::Error) -> bool {
    matches!(
        err,
        kube::Error::Api(e)
            if e.code == 403
                && e.message.contains("took too long to be checked against your quota")
    )
}
