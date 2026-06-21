mod app;
mod cli;
mod clipboard;
mod geometry;
mod image;
mod runtime;
mod stitch;
#[macro_use]
mod trace;
mod wayland;

fn main() {
    if let Err(err) = app::run() {
        eprintln!("screenshot: {err}");
        std::process::exit(1);
    }
}
