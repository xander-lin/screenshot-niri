use std::error::Error;

const NIRI_ONLY_ERROR: &str = "this rebuild currently supports niri only; run it inside a niri session";

pub fn ensure_niri_session() -> Result<(), Box<dyn Error>> {
    if is_niri_session(
        std::env::var_os("NIRI_SOCKET").as_deref(),
        std::env::var_os("XDG_CURRENT_DESKTOP").as_deref(),
    ) {
        Ok(())
    } else {
        Err(NIRI_ONLY_ERROR.into())
    }
}

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
