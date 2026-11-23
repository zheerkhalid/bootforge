//! BootForge: a careful bootable USB creator for Windows.
//!
//! The logic lives in this library so that it can be unit-tested; `main.rs`
//! only parses the command line and opens the window.
pub mod backend;
pub mod blockdev;
pub mod demo;
pub mod device;
pub mod engine;
pub mod error;
pub mod exfat;
pub mod fat32;
pub mod hash;
pub mod imaging;
pub mod iso;
pub mod logbook;
pub mod model;
pub mod part;
pub mod plan;
pub mod safety;
pub mod selftest;
pub mod settings;
pub mod sys;
pub mod ui;
pub mod unattend;
pub mod util;
