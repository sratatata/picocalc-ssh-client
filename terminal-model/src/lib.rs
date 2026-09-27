#![cfg_attr(not(test), no_std)]

extern crate alloc;

pub mod audio_ring;
pub mod dma_ring;
pub mod glyphs;
pub mod i2s_program;
pub mod key_dispatch;
pub mod keyfile;
pub mod mic_config;
pub mod pcm_extract;
pub mod ptt_frame;
pub mod screen_model;
