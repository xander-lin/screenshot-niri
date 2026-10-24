//! The screenshot binary.
//!
//! Everything it runs lives in the library (`screenshot_rs_rebuild`), including
//! `app`, `cli` and `clipboard`. The binary used to declare the modules itself,
//! which made every module's `#[cfg(test)]` block compile and run twice — once
//! for `cargo test --lib` and once for the bin target. Linking the library
//! keeps one copy of the code and one copy of the tests.

fn main() {
    if let Err(err) = screenshot_rs_rebuild::app::run() {
        eprintln!("screenshot: {err}");
        std::process::exit(1);
    }
}
