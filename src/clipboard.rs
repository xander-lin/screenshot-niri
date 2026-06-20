use std::error::Error;
use std::fs::File;
use std::io::{Read, Write};
use std::os::fd::OwnedFd;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use wayland_client::protocol::{
    wl_registry::{self, WlRegistry},
    wl_seat::WlSeat,
};
use wayland_client::{delegate_noop, event_created_child, Connection, Dispatch, QueueHandle};
use wayland_protocols_wlr::data_control::v1::client::{
    zwlr_data_control_device_v1::{self, ZwlrDataControlDeviceV1},
    zwlr_data_control_manager_v1::ZwlrDataControlManagerV1,
    zwlr_data_control_offer_v1::{self, ZwlrDataControlOfferV1},
    zwlr_data_control_source_v1::{self, ZwlrDataControlSourceV1},
};

use crate::cli::ClipboardMode;

struct ClipboardState {
    seat: Option<WlSeat>,
    manager: Option<ZwlrDataControlManagerV1>,
    running: bool,
    path: PathBuf,
    mode: ClipboardMode,
}

pub fn serve_path_detached(path: &Path, mode: ClipboardMode) -> Result<(), Box<dyn Error>> {
    let mode_arg = match mode {
        ClipboardMode::ImagePng => "image",
        ClipboardMode::FileUri => "url",
    };
    let mut child = Command::new(std::env::current_exe()?)
        .arg("--clipboard-provider")
        .arg("--clipboard-path")
        .arg(path)
        .arg("--clipboard-mode")
        .arg(mode_arg)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()?;
    let mut stdout = child.stdout.take().ok_or("clipboard provider stdout unavailable")?;
    let mut ready = [0u8; 6];
    stdout.read_exact(&mut ready)?;
    if &ready != b"ready\n" {
        return Err("clipboard provider did not become ready".into());
    }
    Ok(())
}

pub fn serve_path(path: &Path, mode: ClipboardMode) -> Result<(), Box<dyn Error>> {
    let conn = Connection::connect_to_env()?;
    let mut event_queue = conn.new_event_queue::<ClipboardState>();
    let qh = event_queue.handle();
    let mut state = ClipboardState {
        seat: None,
        manager: None,
        running: true,
        path: path.to_path_buf(),
        mode,
    };

    conn.display().get_registry(&qh, ());
    event_queue.roundtrip(&mut state)?;
    let manager = state.manager.as_ref().ok_or("compositor does not expose zwlr_data_control_manager_v1")?;
    let seat = state.seat.as_ref().ok_or("compositor does not expose wl_seat required for clipboard")?;
    let source = manager.create_data_source(&qh, ());
    match mode {
        ClipboardMode::ImagePng => source.offer("image/png".to_owned()),
        ClipboardMode::FileUri => {
            source.offer("text/uri-list".to_owned());
            source.offer("x-special/gnome-copied-files".to_owned());
            source.offer("application/x-kde-cutselection".to_owned());
        }
    }
    manager.get_data_device(seat, &qh, ()).set_selection(Some(&source));
    conn.flush()?;
    println!("ready");
    std::io::stdout().flush()?;

    while state.running {
        event_queue.blocking_dispatch(&mut state)?;
    }
    Ok(())
}

impl Dispatch<WlRegistry, ()> for ClipboardState {
    fn event(
        state: &mut Self,
        registry: &WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        let wl_registry::Event::Global { name, interface, version } = event else {
            return;
        };
        match interface.as_str() {
            "wl_seat" => state.seat = Some(registry.bind::<WlSeat, _, _>(name, version.min(7), qh, ())),
            "zwlr_data_control_manager_v1" => {
                state.manager = Some(registry.bind::<ZwlrDataControlManagerV1, _, _>(name, version.min(2), qh, ()))
            }
            _ => {}
        }
    }
}

impl Dispatch<ZwlrDataControlSourceV1, ()> for ClipboardState {
    fn event(
        state: &mut Self,
        _: &ZwlrDataControlSourceV1,
        event: zwlr_data_control_source_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            zwlr_data_control_source_v1::Event::Send { mime_type, fd } => {
                if let Err(err) = send_payload(&state.path, state.mode, &mime_type, fd) {
                    eprintln!("screenshot: clipboard send failed: {err}");
                }
            }
            zwlr_data_control_source_v1::Event::Cancelled => state.running = false,
            _ => {}
        }
    }
}

impl Dispatch<ZwlrDataControlDeviceV1, ()> for ClipboardState {
    event_created_child!(ClipboardState, ZwlrDataControlDeviceV1, [
        0 => (ZwlrDataControlOfferV1, ())
    ]);

    fn event(
        state: &mut Self,
        _: &ZwlrDataControlDeviceV1,
        event: zwlr_data_control_device_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if matches!(event, zwlr_data_control_device_v1::Event::Finished) {
            state.running = false;
        }
    }
}

impl Dispatch<ZwlrDataControlOfferV1, ()> for ClipboardState {
    fn event(
        _: &mut Self,
        _: &ZwlrDataControlOfferV1,
        _: zwlr_data_control_offer_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ZwlrDataControlManagerV1, ()> for ClipboardState {
    fn event(
        _: &mut Self,
        _: &ZwlrDataControlManagerV1,
        _: <ZwlrDataControlManagerV1 as wayland_client::Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

fn send_payload(path: &Path, mode: ClipboardMode, mime_type: &str, fd: OwnedFd) -> Result<(), Box<dyn Error>> {
    let mut out = File::from(fd);
    match (mode, mime_type) {
        (ClipboardMode::ImagePng, "image/png") => {
            let mut input = File::open(path)?;
            std::io::copy(&mut input, &mut out)?;
        }
        (ClipboardMode::FileUri, "text/uri-list") => writeln!(out, "{}", file_uri_for_path(path)?)?,
        (ClipboardMode::FileUri, "x-special/gnome-copied-files") => {
            writeln!(out, "copy")?;
            writeln!(out, "{}", file_uri_for_path(path)?)?;
        }
        (ClipboardMode::FileUri, "application/x-kde-cutselection") => out.write_all(b"0")?,
        _ => {}
    }
    Ok(())
}

fn file_uri_for_path(path: &Path) -> Result<String, Box<dyn Error>> {
    let absolute = if path.is_absolute() { path.to_path_buf() } else { std::env::current_dir()?.join(path) };
    let mut uri = String::from("file://");
    for &byte in absolute.as_os_str().as_bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~' | b'/') {
            uri.push(byte as char);
        } else {
            uri.push_str(&format!("%{byte:02X}"));
        }
    }
    Ok(uri)
}

delegate_noop!(ClipboardState: ignore WlSeat);
