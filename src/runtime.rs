// Runtime detection utilities — no longer restricted to niri-only.
// The functions are kept for optional diagnostics.

#[allow(dead_code)]
fn is_niri_session(niri_socket: Option<&std::ffi::OsStr>, xdg_current_desktop: Option<&std::ffi::OsStr>) -> bool {
    niri_socket.is_some_and(|value| !value.is_empty()) || xdg_current_desktop.is_some_and(os_str_contains_niri)
}

fn os_str_contains_niri(value: &std::ffi::OsStr) -> bool {
    value.to_string_lossy().to_ascii_lowercase().contains("niri")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsStr;

    #[test]
    fn detects_non_empty_niri_socket() {
        assert!(is_niri_session(Some(OsStr::new("/run/user/1000/niri.sock")), None));
    }

    #[test]
    fn detects_niri_desktop_case_insensitively() {
        assert!(is_niri_session(None, Some(OsStr::new("GNOME:NiRi"))));
    }

    #[test]
    fn rejects_missing_or_empty_hints() {
        assert!(!is_niri_session(None, None));
        assert!(!is_niri_session(Some(OsStr::new("")), Some(OsStr::new("sway"))));
    }
}
