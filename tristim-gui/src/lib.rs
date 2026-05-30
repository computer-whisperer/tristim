//! tristim-gui library surface.
//!
//! The presenter [`App`](damascene_core::event::App) lives here so it can be shared
//! by the windowed binary (`src/main.rs`) and the headless bundle-dump binary
//! (`src/bin/dump.rs`), which runs damascene's lint pass over the same tree.

pub mod app;
pub mod chart;
pub mod luminance;
pub mod plot;
pub mod setup;
pub mod space3d;
pub use app::PresenterApp;
