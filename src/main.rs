use std::{collections::HashMap, env, sync::Arc};
use serde::Deserialize;
use tsclientlib::{ClientId, Connection, DisconnectOptions, Identity, StreamItem};
use tsproto_packets::packets::{AudioData, CodecType, OutAudio, OutPacket};
use audiopus::coder::Encoder;
use futures::prelude::*;
use sdl2::audio::{AudioCallback, AudioDevice, AudioSpec, AudioSpecDesired, AudioStatus};
use sdl2::AudioSubsystem;
use slog::{debug, info, o, Drain, Logger};
use tokio::sync::mpsc;
use tokio::task::LocalSet;
use anyhow::*;
mod ts_voice;
mod discord;
use tsproto_packets::packets::{Direction, InAudioBuf};

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
struct ConnectionId(u64);

// This trait adds the `register_songbird` and `register_songbird_with` methods
// to the client builder below, making it easy to install this voice client.
// The voice client can be retrieved in any command using `songbird::get(ctx).await`.
use songbird::{SerenityInit, Songbird};
use songbird::driver::{Config as DriverConfig, DecodeMode};

// Import the `Context` to handle commands.
use serenity::{client::Context, prelude::{RwLock, TypeMapKey}};

use serenity::{
    async_trait,
    client::{Client, EventHandler},
    framework::{
        StandardFramework,
        standard::{
            Args, CommandResult,
            macros::{command, group},
        },
    },
    model::{channel::Message, gateway::Ready},
    Result as SerenityResult,
};


#[derive(Debug,Deserialize)]
struct Config {
    discord_token: String,
    teamspeak_server: String,
    teamspeak_identity: String,
    teamspeak_channel: i32,
    /// default 0
    verbose: i32,
    /// default 1.0
    volume: f32,
}

struct ListenerHolder;

impl TypeMapKey for ListenerHolder {
    type Value = Arc<mpsc::Sender<OutPacket>>;
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();

    let config: Config = toml::from_str(&std::fs::read_to_string(".credentials.toml").unwrap()).unwrap();
    let logger = {
		let decorator = slog_term::TermDecorator::new().build();
		let drain = slog_term::CompactFormat::new(decorator).build().fuse();
		let drain = slog_envlogger::new(drain).fuse();
		let drain = slog_async::Async::new(drain).build().fuse();

		Logger::root(drain, o!())
	};
    
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
            .decode_mode(DecodeMode::Decode)
    );


    let mut client = Client::builder(&config.discord_token)
        .event_handler(discord::Handler)
        .framework(framework)
        .register_songbird_with(songbird.into())
        .await
        .expect("Err creating client");

	let (tx,mut rx) = mpsc::channel(50);
	let voice_pipes: Arc<mpsc::Sender<OutPacket>> = Arc::new(tx);
	{
		// Open the data lock in write mode, so keys can be inserted to it.
		let mut data = client.data.write().await;

		// The CommandCounter Value has the following type:
		// Arc<RwLock<HashMap<String, u64>>>
		// So, we have to insert the same type to it.
		data.insert::<ListenerHolder>(voice_pipes);
	}

    tokio::spawn(async move {
        let _ = client.start().await.map_err(|why| println!("Client ended: {:?}", why));
    });

    let con_id = ConnectionId(0);
	let local_set = LocalSet::new();
	let audiodata = ts_voice::start(logger.clone(), &local_set)?;

	let con_config = Connection::build(config.teamspeak_server)
		.log_commands(config.verbose >= 1)
		.log_packets(config.verbose >= 2)
		.log_udp_packets(config.verbose >= 3);

	// Optionally set the key of this client, otherwise a new key is generated.
	let id = Identity::new_from_str(&config.teamspeak_identity).expect("Can't load identity!");
	let con_config = con_config.identity(id);

	// Connect
	let mut con = con_config.connect()?;

	let r = con
		.events()
		.try_filter(|e| future::ready(matches!(e, StreamItem::BookEvents(_))))
		.next()
		.await;
	if let Some(r) = r {
		r?;
	}

	// let (send, mut recv) = mpsc::channel(5);
	// {
	// 	let mut a2t = audiodata.a2ts.lock().unwrap();
	// 	a2t.set_listener(send);
	// 	a2t.set_volume(config.volume);
	// 	a2t.set_playing(true);
	// }

	loop {
		let t2a = audiodata.ts2a.clone();
		let events = con.events().try_for_each(|e| async {
			if let StreamItem::Audio(packet) = e {
				let from = ClientId(match packet.data().data() {
					AudioData::S2C { from, .. } => *from,
					AudioData::S2CWhisper { from, .. } => *from,
					_ => panic!("Can only handle S2C packets but got a C2S packet"),
				});
				let mut t2a = t2a.lock().unwrap();
				if let Err(e) = t2a.play_packet((con_id, from), packet) {
					debug!(logger, "Failed to play packet"; "error" => %e);
				}
			}
			Ok(())
		});

		// Wait for ctrl + c
		tokio::select! {
			// send_audio = recv.recv() => {
			// 	if let Some(packet) = send_audio {
			// 		con.send_audio(packet)?;
			// 	} else {
			// 		info!(logger, "Audio sending stream was canceled");
			// 		break;
			// 	}
			// }
			send_audio = rx.recv() => {
				if let Some(packet) = send_audio {
					con.send_audio(packet)?;
				} else {
					info!(logger, "Audio sending stream was canceled");
					break;
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