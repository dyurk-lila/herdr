//! Shared wire protocol and presentation encoding code.

pub mod endpoint;
pub(crate) mod render_ansi;
pub(crate) mod surface_delta;
pub(crate) mod surface_reuse;
pub(crate) mod surface_scroll;
mod wire;

pub use wire::*;

// Generation-one input expands key repeats before enforcing this transport bound.
pub(crate) const MAX_INPUT_EVENT_BATCH: usize = 4096;
