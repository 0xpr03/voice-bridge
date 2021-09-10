use std::io::Seek;
use std::{io::Read, mem::size_of, sync::Arc, time::Duration};
use byte_slice_cast::AsByteSlice;
use serde::Deserialize;
use songbird::input::reader::MediaSource;
use tsclientlib::{ClientId, Connection, DisconnectOptions, Identity, StreamItem};
use tsproto_packets::packets::{AudioData, CodecType, OutAudio, OutPacket};
use audiopus::coder::Encoder;
use futures::prelude::*;
use slog::{debug, o, Drain, Logger};
use tokio::task;
use tokio::sync::Mutex;
use anyhow::*;

mod discord;
mod discord_audiohandler;

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
struct ConnectionId(u64);

// This trait adds the `register_songbird` and `register_songbird_with` methods
// to the client builder below, making it easy to install this voice client.
// The voice client can be retrieved in any command using `songbird::get(ctx).await`.
use songbird::{SerenityInit, Songbird};
use songbird::driver::{DecodeMode};
use songbird::Config as DriverConfig;

// Import the `Context` to handle commands.
use serenity::{prelude::{TypeMapKey}};

use serenity::{
    client::{Client},
    framework::{
        StandardFramework,
    },
};


#[derive(Debug,Deserialize)]
struct Config {
    discord_token: String,
    teamspeak_server: String,
    teamspeak_identity: String,
	teamspeak_server_password: Option<String>,
    teamspeak_channel_id: Option<u64>,
	teamspeak_channel_name: Option<String>,
	teamspeak_channel_password: Option<String>,
	teamspeak_name: Option<String>,
    /// default 0
    verbose: i32,
    /// default 1.0
    volume: f32,
}

struct ListenerHolder;

//TODO: stop shooting myself in the knee with a mutex
type AudioBufferDiscord = Arc<Mutex<discord_audiohandler::AudioHandler<u32>>>;


type TsVoiceId = (ConnectionId, ClientId);
type TsAudioHandler = tsclientlib::audio::AudioHandler<TsVoiceId>;

#[derive(Clone)]
struct TsToDiscordPipeline {
	data: Arc<std::sync::Mutex<TsAudioHandler>>,
}

impl MediaSource for TsToDiscordPipeline {
    fn is_seekable(&self) -> bool {
        false
    }

    fn len(&self) -> Option<u64> {
        None
    }
}

impl Seek for TsToDiscordPipeline {
    fn seek(&mut self, _: std::io::SeekFrom) -> std::io::Result<u64> {
        Err(std::io::Error::new(std::io::ErrorKind::Other, "source does not support seeking"))
    }
}

impl TsToDiscordPipeline {
	pub fn new(logger: Logger) -> Self {
		Self {
			data: Arc::new(std::sync::Mutex::new(TsAudioHandler::new(logger)))
		}
	}
}

impl Read for TsToDiscordPipeline {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
		let len = buf.len() / size_of::<f32>();
		let mut wtr: Vec<f32> = vec![0.0; len];
		// TODO: can't we support async read for songbird ? this is kinda bad as it requires a sync mutex
		{
			let mut lock = self.data.lock().expect("Can't lock ts voice buffer!");

			// and this is really ugly.. read only works for u8, but we get an f32 and need to convert that without changing AudioHandlers API
			// also Read for stuff that specifies to use f32 is kinda meh			
			lock.fill_buffer(wtr.as_mut_slice());
		}
		let slice = wtr.as_byte_slice();
		buf.copy_from_slice(slice);

		Ok(buf.len())
    }
}

impl TypeMapKey for ListenerHolder {
    type Value = (TsToDiscordPipeline,AudioBufferDiscord);
}

/// teamspeak audio fragment timer
/// We want to run every 20ms, but we only get ~1ms correctness
const TICK_TIME: u64 = 20;
const FRAME_SIZE_MS: usize = 20;
const SAMPLE_RATE: usize = 48000;
const STEREO_20MS: usize = SAMPLE_RATE * 2 * FRAME_SIZE_MS / 1000;
/// The maximum size of an opus frame is 1275 as from RFC6716.
const MAX_OPUS_FRAME_SIZE: usize = 1275;

const RUST_LOG: &'static str = "RUST_LOG";
#[tokio::main]
async fn main() -> Result<()> {
	if std::env::var(RUST_LOG).is_err() {
        std::env::set_var(
            RUST_LOG,
            #[cfg(debug_assertions)]
            "info,voice_bridge=debug",
            #[cfg(not(debug_assertions))]
            "warn,voice_bridge=info",
        );
    }
    tracing_subscriber::fmt::init();
	// init logging stuff used by tsclientlib
    let config: Config = toml::from_str(&std::fs::read_to_string(".credentials.toml").unwrap()).unwrap();
    let logger = {
		let decorator = slog_term::TermDecorator::new().build();
		let drain = slog_term::CompactFormat::new(decorator).build().fuse();
		let drain = slog_envlogger::new(drain).fuse();
		let drain = slog_async::Async::new(drain).build().fuse();

		Logger::root(drain, o!())
	};
    // init discord framework
    let framework = StandardFramework::new()
        .configure(|c| c
                   .prefix("~"))
        .group(&discord::GENERAL_GROUP);

	// Here, we need to configure Songbird to decode all incoming voice packets.
    // If you want, you can do this on a per-call basis---here, we need it to
    // read the audio data that other people are sending us!
    let songbird = Songbird::serenity();
    songbird.set_config(
        DriverConfig::default()
            .decode_mode(DecodeMode::Decrypt)
    );

	// init discord client
    let mut client = Client::builder(&config.discord_token)
        .event_handler(discord::Handler)
        .framework(framework)
        .register_songbird_with(songbird.into())
        .await
        .expect("Err creating client");

	// init teamspeak -> discord pipeline
	let ts_voice_logger = logger.new(o!("pipeline" => "voice-ts"));
	let teamspeak_voice_handler = TsToDiscordPipeline::new(ts_voice_logger);

	// init discord -> teamspeak pipeline
	let discord_voice_logger = logger.new(o!("pipeline" => "voice-discord"));
	let discord_voice_buffer: AudioBufferDiscord = Arc::new(Mutex::new(discord_audiohandler::AudioHandler::new(discord_voice_logger)));
	// stuff discord -> teamspeak pipeline into discord context for retrieval inside the client
	{
		// Open the data lock in write mode, so keys can be inserted to it.
		let mut data = client.data.write().await;

		// The CommandCounter Value has the following type:
		// Arc<RwLock<HashMap<String, u64>>>
		// So, we have to insert the same type to it.
		data.insert::<ListenerHolder>((teamspeak_voice_handler.clone(),discord_voice_buffer.clone()));
	}

	// spawn client runner
    tokio::spawn(async move {
        let _ = client.start().await.map_err(|why| println!("Client ended: {:?}", why));
    });

    let con_id = ConnectionId(0);

	// configure teamspeak client
	let mut con_config = Connection::build(config.teamspeak_server)
		.log_commands(config.verbose >= 1)
		.log_packets(config.verbose >= 2)
		.log_udp_packets(config.verbose >= 3);

	if let Some(name) = config.teamspeak_name {
		con_config = con_config.name(name);
	}
	if let Some(channel) = config.teamspeak_channel_id {
		con_config = con_config.channel_id(tsclientlib::ChannelId(channel));
	}
	if let Some(channel) = config.teamspeak_channel_name {
		con_config = con_config.channel(channel);
	}
	if let Some(password) = config.teamspeak_server_password {
		con_config = con_config.password(password);
	}
	if let Some(password) = config.teamspeak_channel_password {
		con_config = con_config.channel_password(password);
	}

	// teamspeak: Optionally set the key of this client, otherwise a new key is generated.
	let id = Identity::new_from_str(&config.teamspeak_identity).expect("Can't load identity!");
	let con_config = con_config.identity(id);

	// Connect teamspeak client
	let mut con = con_config.connect()?;

	// todo: something something discord connection events?
	let r = con
		.events()
		.try_filter(|e| future::ready(matches!(e, StreamItem::BookEvents(_))))
		.next()
		.await;
	if let Some(r) = r {
		r?;
	}

	// init discord -> teamspeak opus encoder
	let encoder = audiopus::coder::Encoder::new(
		audiopus::SampleRate::Hz48000,
		audiopus::Channels::Stereo,
		audiopus::Application::Voip)
		.expect("Can't construct encoder!");
	// we have to stuff this inside an arc-mutex to avoid lifetime shenanigans
	let encoder = Arc::new(Mutex::new(encoder));

	// teamspeak playback timer
	let mut interval = tokio::time::interval(Duration::from_millis(TICK_TIME));
	
	loop {
		// handle teamspeak events
		let events = con.events().try_for_each(|e| async {
			// handle teamspeak audio packets
			if let StreamItem::Audio(packet) = e {
				let from = ClientId(match packet.data().data() {
					AudioData::S2C { from, .. } => *from,
					AudioData::S2CWhisper { from, .. } => *from,
					_ => panic!("Can only handle S2C packets but got a C2S packet"),
				});
				
				let mut ts_voice: std::sync::MutexGuard<TsAudioHandler> = teamspeak_voice_handler.data.lock().expect("Can't lock ts audio buffer!");
				// feed mixer+jitter buffer, consumed by discord
				if let Err(e) = ts_voice.handle_packet((con_id, from), packet) {
					debug!(logger, "Failed to handle TS_Voice packet"; "error" => %e);
				}
			}
			Ok(())
		});
		// Wait for ctrl + c and run everything else, end on who ever stops first
		tokio::select! {
			_send = interval.tick() => {
				let start = std::time::Instant::now();
				// send audio frame to teamspeak
				if let Some(processed) = process_discord_audio(&discord_voice_buffer,&encoder).await {
					con.send_audio(processed)?;
					let dur = start.elapsed();
					if dur >= Duration::from_millis(1) {
						eprintln!("Audio pipeline took {}ms",dur.as_millis());
					}
				}
			}
			_ = tokio::signal::ctrl_c() => { break; }
			r = events => {
				r?;
				bail!("Disconnected");
			}
		};
	}
	println!("Disconnecting");
	// Disconnect
	con.disconnect(DisconnectOptions::new())?;
	con.events().for_each(|_| future::ready(())).await;
	println!("Disconnected");
    Ok(())
}


/// Create an audio frame for consumption by teamspeak.
/// Merges all streams and converts them to opus
async fn process_discord_audio(voice_buffer: &AudioBufferDiscord, encoder: &Arc<Mutex<Encoder>>) -> Option<OutPacket> {
	// let mut buffer_map;
	// {
	// 	let mut lock = voice_buffer.lock().await;
	// 	buffer_map = std::mem::replace(&mut *lock, HashMap::new());
	// }

	let mut data = [0.0; STEREO_20MS];
	{
		let mut lock = voice_buffer.lock().await;
		lock.fill_buffer(&mut data);
	}
	let mut encoded = [0; MAX_OPUS_FRAME_SIZE];
	let encoder_c = encoder.clone();
	// don't block the async runtime
	let res = task::spawn_blocking(move || {
		let start = std::time::Instant::now();		
		// encode back to opus
		// this should never block, thus we don't fail gracefully for it
		let lock = encoder_c.try_lock().expect("Can't reach encoder!");
		let length = match lock.encode_float(&data, &mut encoded) {
			Err(e) => {eprintln!("Failed to encode voice: {}",e); return None;},
			Ok(size) => size,
		};
		//println!("Data size: {}/{} enc-length: {}",data.len(),STEREO_20MS,length);
		//println!("length size: {}",length);
		// warn on high encoding times
		let duration = start.elapsed().as_millis();
		if duration > 2 {
			eprintln!("Took too {}ms for processing audio!",duration);
		}
		// package into teamspeak audio structure
		Some(OutAudio::new(&AudioData::C2S { id: 0, codec: CodecType::OpusMusic, data: &encoded[..length] }))
	}).await.expect("Join error for audio processing thread!");
	res
}