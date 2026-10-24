#[cfg(feature = "trace-logs")]
#[macro_export]
macro_rules! trace_log {
    ($($arg:tt)*) => {
        {
            eprintln!("[screenshot trace] {}", format_args!($($arg)*));
        }
    };
}

#[cfg(feature = "trace-logs")]
pub fn trace_verbose_enabled() -> bool {
    std::env::var_os("SCREENSHOT_TRACE_VERBOSE").is_some()
}

#[cfg(feature = "trace-logs")]
pub fn trace_profile_enabled() -> bool {
    std::env::var_os("SCREENSHOT_TRACE_PROFILE").is_some()
}

#[cfg(feature = "trace-logs")]
pub fn trace_deep_profile_enabled() -> bool {
    std::env::var_os("SCREENSHOT_TRACE_DEEP_PROFILE").is_some()
}

#[cfg(feature = "trace-logs")]
pub fn trace_fast_motion_enabled() -> bool {
    std::env::var_os("SCREENSHOT_TRACE_FAST_MOTION").is_some()
}

#[cfg(feature = "trace-logs")]
pub fn edge_gate_trace_enabled() -> bool {
    std::env::var_os("SCREENSHOT_TRACE_EDGE_GATE").is_some()
}

/// Whether to accept placements from the fast-motion scan.
///
/// Only the legacy pipeline consults it, so the stub is marked rather than
/// reported as dead code in a build that has no legacy pipeline.
#[cfg_attr(not(feature = "stitch-legacy"), allow(dead_code))]
pub fn fast_motion_accept_enabled() -> bool {
    std::env::var_os("SCREENSHOT_FAST_MOTION_ACCEPT").is_some()
}

#[cfg(not(feature = "trace-logs"))]
#[macro_export]
macro_rules! trace_log {
    ($($arg:tt)*) => {
        ()
    };
}

#[cfg(not(feature = "trace-logs"))]
pub fn trace_verbose_enabled() -> bool {
    false
}

#[cfg(not(feature = "trace-logs"))]
pub fn trace_profile_enabled() -> bool {
    false
}

#[cfg(not(feature = "trace-logs"))]
pub fn trace_deep_profile_enabled() -> bool {
    false
}

#[cfg(not(feature = "trace-logs"))]
#[cfg_attr(not(feature = "stitch-legacy"), allow(dead_code))]
pub fn trace_fast_motion_enabled() -> bool {
    false
}

#[cfg(not(feature = "trace-logs"))]
pub fn edge_gate_trace_enabled() -> bool {
    false
}
