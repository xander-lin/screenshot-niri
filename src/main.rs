mod app;
mod cli;
mod clipboard;
mod geometry;
mod image;
mod runtime;
#[cfg(test)]
mod stitch;
mod wayland;

fn main() {
    if let Err(err) = app::run() {
        eprintln!("screenshot: {err}");
        std::process::exit(1);
    }
}
