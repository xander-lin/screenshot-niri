pub mod app;
pub mod clipboard;
pub mod cli;
pub mod corpus;
pub mod geometry;
pub mod image;
pub mod config;
pub mod stitch;
#[macro_use]
mod trace;

pub mod wayland {
    pub mod screencopy;
    pub mod selection;
}
