// Interval for cleaning up old equivalent sessions
pub(super) const CLEANUP_EQUIV_SESSIONS_INTERVAL_SECS: u64 = 16;

// Maximum age for equivalent session entries
pub(super) const EQUIV_SESSION_MAX_AGE_SECS: u64 = 4;

// Rasonably small limit to prevent DoS via equiv session entries
// Note that the normal number of equiv sessions should be very small.. 0 or 1 :)
pub(super) const MAX_EQUIV_ENTRIES: usize = 32;
