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
pub fn trace_fast_motion_enabled() -> bool {
    false
}
