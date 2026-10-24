#[cfg(feature = "trace-logs")]
#[macro_export]
macro_rules! trace_log {
    ($($arg:tt)*) => {
        {
            eprintln!("[screenshot trace] {}", format_args!($($arg)*));
        }
    };
}

#[cfg(not(feature = "trace-logs"))]
#[macro_export]
macro_rules! trace_log {
    ($($arg:tt)*) => {
        ()
    };
}
