use std::sync::{Arc, Mutex};

use anyhow::Result;
use slog::Logger;
use tokio::task::LocalSet;


use ts_to_audio::TsToAudio;


pub mod ts_to_audio;

/// The usual frame size.
///
/// Use 48 kHz, 20 ms frames (50 per second) and mono data (1 channel).
/// This means 1920 samples and 7.5 kiB.
const USUAL_FRAME_SIZE: usize = 48000 / 50;



#[derive(Clone)]
pub struct AudioData {
	pub ts2a: Arc<Mutex<TsToAudio>>,
}

pub fn start(logger: Logger, local_set: &LocalSet) -> Result<AudioData> {
	let sdl_context = sdl2::init().unwrap();

	let audio_subsystem = sdl_context.audio().unwrap();
	// SDL automatically disables the screensaver, enable it again
	if let Ok(video_subsystem) = sdl_context.video() {
		video_subsystem.enable_screen_saver();
	}

	let ts2a = TsToAudio::new(logger.clone(), audio_subsystem.clone(), local_set)?;
	

	Ok(AudioData { ts2a })
}