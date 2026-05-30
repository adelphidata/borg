//! Multimodal I/O: image/audio parsing, compression, AI image generation, TTS,
//! and LLM-based media understanding.

pub mod core;
pub mod image_gen;
pub mod playback;
pub(crate) mod shell_template;
pub mod tts;
pub mod understanding;
pub mod voice_capture;
pub mod voice_loop;

pub use core::*;
