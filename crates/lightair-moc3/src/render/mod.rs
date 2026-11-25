//! Renderer-facing helpers.
//!
//! The [`crate::render::common`] module is backend-neutral and can be used with
//! any graphics API. Enable the `wgpu` feature to use the built-in `wgpu`
//! backend.

pub mod common;

#[cfg(feature = "wgpu")]
pub mod wgpu;
