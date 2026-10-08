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
