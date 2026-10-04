//! Client for the Xbox 360 debug monitor (XBDM), the service Xbox 360 Neighborhood talks to.
//!
//! XBDM runs on development kits and on modified retail consoles (RGH/JTAG, BadUpdate)
//! with `xbdm.xex` loaded. It speaks a line-based text protocol on TCP port 730 and
//! answers discovery broadcasts on UDP port 730.

pub mod client;
pub mod content;
pub mod debug;
pub mod display;
pub mod discovery;
pub mod error;
pub mod memory;
pub mod path;
pub mod response;
pub mod screenshot;
pub mod shell;
pub mod title;
pub mod transfer;

pub use client::{Client, DirEntry, FreeSpace, Module};
pub use screenshot::Screenshot;
pub use memory::Pattern;
pub use debug::{Context, Notification, Notifications, Region, Section, ThreadInfo, Watch};
pub use discovery::{discover, resolve, Console};
pub use error::{Error, Result};

/// TCP and UDP port used by XBDM.
pub const XBDM_PORT: u16 = 730;
