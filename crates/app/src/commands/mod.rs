//! The Tauri commands the UI calls, by area. Each is a thin `async`
//! wrapper (Tauri runs it off the UI thread) over the backend API.

use super::*;

pub(crate) mod ai;
pub(crate) mod connections;
pub(crate) mod drivers;
pub(crate) mod explorer;
pub(crate) mod jobs;
pub(crate) mod library;
pub(crate) mod lineage;
pub(crate) mod outputs;
pub(crate) mod queries;
pub(crate) mod results;
pub(crate) mod system;
