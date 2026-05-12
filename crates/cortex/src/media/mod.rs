//! Multimodal I/O: image/audio parsing, compression, AI image generation, TTS,
//! and LLM-based media understanding.

pub mod core;
pub mod image_gen;
pub mod tts;
pub mod understanding;

pub use core::*;
