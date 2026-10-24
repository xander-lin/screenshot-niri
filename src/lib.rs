pub mod app;
pub mod clipboard;
pub mod cli;
pub mod config;
pub mod geometry;
pub mod image;
#[macro_use]
mod trace;

pub mod wayland {
    pub mod screencopy;
    pub mod selection;
}
