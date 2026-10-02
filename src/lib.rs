//! ghostwriter — MCP server producing written documentation through the
//! locally served `hemmingway-1` model.
//!
//! Built on the official Rust SDK (`rmcp`) with tokio + reqwest. The six
//! tools (see [`writer::WritingServer`]) carry the whole surface; the style
//! guides in [`prompts`] are the part that keeps output out of LLM-slop
//! territory, and [`ste`] carries the deterministic ASD-STE100 checks.
//! Configuration resolves once at startup — built-in defaults, overlaid by
//! `~/.config/ghostwriter/config.toml`, then by `GHOSTWRITER_*` environment
//! variables (see [`config::load`]).

pub mod client;
pub mod config;
pub mod prompts;
pub mod ste;
pub mod writer;
