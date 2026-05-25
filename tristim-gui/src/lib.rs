//! tristim-gui library surface.
//!
//! The presenter [`App`](aetna_core::event::App) lives here so it can be shared
//! by the windowed binary (`src/main.rs`) and the headless bundle-dump binary
//! (`src/bin/dump.rs`), which runs aetna's lint pass over the same tree.

pub mod app;
pub mod chart;
pub mod luminance;
pub mod plot;
pub mod setup;
pub use app::PresenterApp;
