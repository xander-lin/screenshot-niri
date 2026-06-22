use std::error::Error;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Normal,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClipboardMode {
    ImagePng,
    FileUri,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    Screenshot(Args),
    ClipboardProvider(ClipboardProviderArgs),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Args {
    pub help: bool,
    pub mode: Mode,
    pub clipboard_mode: ClipboardMode,
    pub output_path: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClipboardProviderArgs {
    pub path: PathBuf,
    pub mode: ClipboardMode,
}

pub const HELP: &str = "Usage: screenshot [OPTIONS] [PATH]\n\nOptions:\n  -h, --help           Show this help text\n      --file [PATH]    Save to PATH, or to the Pictures/Screenshots directory when omitted\n  -o, --output PATH    Save to PATH instead of the default temporary path\n      --name NAME      Use NAME as the output filename, appending .png when needed\n      --url            Put a file URI on the clipboard instead of image/png\n";

impl Command {
    pub fn parse() -> Result<Self, Box<dyn Error>> {
        Self::parse_from(std::env::args_os().skip(1).collect())
    }

    fn parse_from(raw: Vec<OsString>) -> Result<Self, Box<dyn Error>> {
        if raw.first().is_some_and(|arg| arg == "--clipboard-provider") {
            parse_clipboard_provider(raw)
        } else {
            Args::parse_from(raw).map(Self::Screenshot)
        }
    }
}

impl Args {
    fn parse_from(raw: Vec<OsString>) -> Result<Self, Box<dyn Error>> {
        let mut help = false;
        let mut clipboard_mode = ClipboardMode::ImagePng;
        let mut output_path = None;
        let mut file_requested = false;
        let mut file_path = None;
        let mut generated_name = None;

        let mut index = 0;
        while index < raw.len() {
            let arg = &raw[index];
            if arg == "-h" || arg == "--help" {
                help = true;
                index += 1;
            } else if arg == "--url" {
                clipboard_mode = ClipboardMode::FileUri;
                index += 1;
            } else if arg == "--name" {
                index += 1;
                let value = raw.get(index).ok_or("--name requires a filename")?;
                generated_name = Some(parse_output_name(value)?);
                index += 1;
            } else if let Some(value) = strip_os_prefix(arg, "--name=") {
                generated_name = Some(parse_output_name(value)?);
                index += 1;
            } else if arg == "--file" {
                file_requested = true;
                if let Some(next) = raw.get(index + 1) {
                    if !is_option(next) {
                        index += 1;
                        file_path = Some(PathBuf::from(&raw[index]));
                    }
                }
                index += 1;
            } else if let Some(value) = strip_os_prefix(arg, "--file=") {
                file_requested = true;
                file_path = Some(PathBuf::from(value));
                index += 1;
            } else if arg == "-o" || arg == "--output" {
                index += 1;
                let value = raw.get(index).ok_or("--output requires a path")?;
                output_path = Some(PathBuf::from(value));
                index += 1;
            } else if let Some(value) = strip_os_prefix(arg, "--output=") {
                output_path = Some(PathBuf::from(value));
                index += 1;
            } else if is_option(arg) {
                return Err(format!("unexpected argument: {}", arg.to_string_lossy()).into());
            } else {
                if output_path.is_some() || file_path.is_some() {
                    return Err(format!("unexpected argument: {}", arg.to_string_lossy()).into());
                }
                file_requested = true;
                file_path = Some(PathBuf::from(arg));
                index += 1;
            }
        }

        if output_path.is_some() && file_requested {
            return Err("--output and --file cannot be used together".into());
        }

        let output_path = if let Some(path) = output_path {
            resolve_explicit_output_path(path, generated_name.as_deref())
        } else if file_requested {
            resolve_file_output_path(file_path, generated_name.as_deref())
        } else {
            resolve_temp_output_path(generated_name.as_deref())?
        };

        Ok(Self {
            help,
            mode: Mode::Normal,
            clipboard_mode,
            output_path,
        })
    }
}

fn parse_clipboard_provider(raw: Vec<OsString>) -> Result<Command, Box<dyn Error>> {
    let mut path = None;
    let mut mode = None;
    let mut index = 1;
    while index < raw.len() {
        let arg = &raw[index];
        if arg == "--clipboard-path" {
            index += 1;
            path = Some(PathBuf::from(raw.get(index).ok_or("--clipboard-path requires a path")?));
            index += 1;
        } else if arg == "--clipboard-mode" {
            index += 1;
            mode = Some(parse_clipboard_mode(raw.get(index).ok_or("--clipboard-mode requires a value")?)?);
            index += 1;
        } else {
            return Err(format!("unexpected clipboard argument: {}", arg.to_string_lossy()).into());
        }
    }
    Ok(Command::ClipboardProvider(ClipboardProviderArgs {
        path: path.ok_or("--clipboard-path is required")?,
        mode: mode.ok_or("--clipboard-mode is required")?,
    }))
}

fn parse_clipboard_mode(value: &OsStr) -> Result<ClipboardMode, Box<dyn Error>> {
    match value.to_str() {
        Some("image") => Ok(ClipboardMode::ImagePng),
        Some("url") => Ok(ClipboardMode::FileUri),
        _ => Err("--clipboard-mode must be image or url".into()),
    }
}

fn strip_os_prefix<'a>(value: &'a OsStr, prefix: &str) -> Option<&'a OsStr> {
    value
        .as_bytes()
        .strip_prefix(prefix.as_bytes())
        .map(OsStr::from_bytes)
}

fn is_option(value: &OsStr) -> bool {
    value.as_bytes().starts_with(b"-") && value != "-"
}

fn parse_output_name(value: &OsStr) -> Result<String, Box<dyn Error>> {
    let name = value.to_str().ok_or("--name must be valid UTF-8")?.to_owned();
    if name.is_empty() || name.contains('/') {
        return Err("--name must be a filename without slashes".into());
    }
    Ok(name)
}

fn output_filename(name: Option<&str>) -> String {
    match name {
        Some(name) if name.ends_with(".png") => name.to_owned(),
        Some(name) => format!("{name}.png"),
        None => make_timestamped_filename(),
    }
}

fn make_timestamped_filename() -> String {
    let duration = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default();
    let time = duration.as_secs() as libc::time_t;
    let mut tm = unsafe { std::mem::zeroed::<libc::tm>() };
    unsafe {
        libc::localtime_r(&time, &mut tm);
    }
    format!(
        "screenshot-{:04}{:02}{:02}-{:02}{:02}{:02}-{:09}.png",
        tm.tm_year + 1900,
        tm.tm_mon + 1,
        tm.tm_mday,
        tm.tm_hour,
        tm.tm_min,
        tm.tm_sec,
        duration.subsec_nanos()
    )
}

fn resolve_temp_output_path(name: Option<&str>) -> Result<PathBuf, Box<dyn Error>> {
    let filename = output_filename(name);
    if let Some(runtime_dir) = non_empty_env_path("XDG_RUNTIME_DIR") {
        return Ok(runtime_dir.join(filename));
    }

    let dir = PathBuf::from("/tmp").join(format!("screenshot-rust-{}", unsafe { libc::getuid() }));
    ensure_private_dir(&dir)?;
    Ok(dir.join(filename))
}

fn resolve_file_output_path(path: Option<PathBuf>, name: Option<&str>) -> PathBuf {
    match path {
        Some(path) => resolve_explicit_output_path(path, name),
        None => default_screenshot_dir().join(output_filename(name)),
    }
}

fn resolve_explicit_output_path(path: PathBuf, name: Option<&str>) -> PathBuf {
    if path_has_trailing_slash(&path) || path.is_dir() {
        path.join(output_filename(name))
    } else {
        path
    }
}

fn path_has_trailing_slash(path: &Path) -> bool {
    path.as_os_str().as_bytes().ends_with(b"/")
}

fn default_screenshot_dir() -> PathBuf {
    if let Some(path) = non_empty_env_path("XDG_PICTURES_DIR") {
        return path.join("Screenshots");
    }
    if let Some(home) = non_empty_env_path("HOME") {
        return home.join("Pictures").join("Screenshots");
    }
    PathBuf::from("Pictures").join("Screenshots")
}

fn non_empty_env_path(name: &str) -> Option<PathBuf> {
    std::env::var_os(name).filter(|value| !value.is_empty()).map(PathBuf::from)
}

fn ensure_private_dir(path: &Path) -> Result<(), Box<dyn Error>> {
    match fs::create_dir(path) {
        Ok(()) => {
            fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
            return Ok(());
        }
        Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(err) => return Err(err.into()),
    }

    let metadata = fs::symlink_metadata(path)?;
    if !metadata.is_dir() || metadata.uid() != unsafe { libc::getuid() } {
        return Err(format!("refusing unsafe temporary output directory {}", path.display()).into());
    }
    if metadata.permissions().mode() & 0o077 != 0 {
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn name_appends_png_and_rejects_slashes() {
        assert_eq!(parse_output_name(OsStr::new("capture")).unwrap(), "capture");
        assert_eq!(output_filename(Some("capture")), "capture.png");
        assert_eq!(output_filename(Some("capture.png")), "capture.png");
        assert!(parse_output_name(OsStr::new("nested/capture")).is_err());
    }

    #[test]
    fn file_without_path_uses_pictures_screenshots() {
        let args = Args::parse_from(vec![OsString::from("--file"), OsString::from("--name=test")]).unwrap();
        assert!(args.output_path.ends_with(Path::new("Pictures/Screenshots/test.png")));
    }

    #[test]
    fn output_and_file_are_exclusive() {
        let err = Args::parse_from(vec![OsString::from("--file"), OsString::from("--output"), OsString::from("out.png")]).unwrap_err();
        assert!(err.to_string().contains("cannot be used together"));
    }


}
