//! Discord handler

use anyhow::bail;
use serenity::builder::CreateApplicationCommand;
use serenity::model::application::command::Command;
use serenity::model::prelude::command::CommandOptionType;
use serenity::model::application::interaction::{Interaction, InteractionResponseType};
use serenity::model::id::GuildId;
use serenity::model::prelude::interaction::application_command::{CommandDataOption, CommandDataOptionValue, ApplicationCommandInteraction};
// This trait adds the `register_songbird` and `register_songbird_with` methods
// to the client builder below, making it easy to install this voice client.
// The voice client can be retrieved in any command using `songbird::get(ctx).await`.
use songbird::input::Input;

use serenity::prelude::*;

// Import the `Context` to handle commands.
use serenity::client::Context;

use serenity::{
    async_trait,
    client::{EventHandler},
    framework::{
        standard::{
            Args, CommandResult,
            macros::{command, group},
        },
    },
    model::{channel::Message, gateway::Ready},
    Result as SerenityResult,
};
use songbird::packet::PacketSize;
use songbird::packet::rtp::RtpExtensionPacket;
use songbird::{
    model::payload::{ClientDisconnect, Speaking},
    CoreEvent,
    Event,
    EventContext,
    EventHandler as VoiceEventHandler,
};

use crate::ListenerHolder;

pub(crate) struct Handler;

#[async_trait]
impl EventHandler for Handler {
    async fn interaction_create(&self, ctx: Context, interaction: Interaction) {
        if let Interaction::ApplicationCommand(command) = interaction {
            println!("Received command interaction: {:#?}", command);
            let result: Result<(), anyhow::Error> = match command.data.name.as_str() {
                "join_voice" => handle_join(&ctx,&command).await,
                _ => Err(anyhow::Error::msg("not implemented :(")),
            };

            if let Err(err) = result {
                println!("Failed to run command: {}",err);
                if let Err(why) = if command.get_interaction_response(&ctx.http).await.is_err() {
                        command
                        .create_interaction_response(&ctx.http, |response| {
                            response.kind(InteractionResponseType::ChannelMessageWithSource)
                                    .interaction_response_data(|message|message.content(err))
                        })
                        .await
                    } else {
                        command
                        .edit_original_interaction_response(&ctx.http, |response| {
                            response.content(err)
                        })
                        .await.map(|_|())
                    }
                {
                    println!("Cannot respond to slash command: {}", why);
                }
            }
        }
    }

    async fn ready(&self, ctx: Context, ready: Ready) {
        println!("{} is connected!", ready.user.name);
        Command::create_global_application_command(&ctx.http, |command| {
            register_join(command)
        })
        .await.expect("Failed creating commands");
    }
}

#[group]
#[commands(deafen, leave, mute, play, ping, undeafen, unmute)]
pub struct General;

#[command]
#[only_in(guilds)]
async fn deafen(ctx: &Context, msg: &Message) -> CommandResult {
    let guild = msg.guild(&ctx.cache).expect("No guild found!");
    let guild_id = guild.id;

    let manager = songbird::get(ctx).await
        .expect("Songbird Voice client placed in at initialisation.").clone();

    let handler_lock = match manager.get(guild_id) {
        Some(handler) => handler,
        None => {
            check_msg(msg.reply(ctx, "Not in a voice channel").await);

            return Ok(());
        },
    };

    let mut handler = handler_lock.lock().await;

    if handler.is_deaf() {
        check_msg(msg.channel_id.say(&ctx.http, "Already deafened").await);
    } else {
        if let Err(e) = handler.deafen(true).await {
            check_msg(msg.channel_id.say(&ctx.http, format!("Failed: {:?}", e)).await);
        }

        check_msg(msg.channel_id.say(&ctx.http, "Deafened").await);
    }

    Ok(())
}

fn register_join(command: &mut CreateApplicationCommand) -> &mut CreateApplicationCommand {
    command.name("join_voice").description("Join voice channel")
        .create_option(|option|
            option.name("channel").description("channel to join")
            .kind(CommandOptionType::Channel).required(true))
}

async fn handle_join(ctx: &Context ,interaction: &ApplicationCommandInteraction) -> anyhow::Result<()> {
    let guild_id = match interaction.guild_id {
        Some(id) => id,
        None => bail!("Command can't be used outside of servers!"),
    };
    let option = interaction.data.options
        .get(0)
        .expect("Expected user option")
        .resolved
        .as_ref()
        .expect("Expected user object");

    let connect_to = match option {
        CommandDataOptionValue::Channel(part_chan) => part_chan.id,
        _ => bail!("Expected channel argument!"),
    };
    // let guild = msg.guild(&ctx.cache).expect("No guild found!");
    // let guild_id = guild.id;

    // let channel_id = guild
    //     .voice_states.get(&msg.author.id)
    //     .and_then(|voice_state| voice_state.channel_id);

    // let connect_to = match channel_id {
    //     Some(channel) => channel,
    //     None => {
    //         check_msg(msg.reply(ctx, "Not in a voice channel").await);

    //         return Ok(None);
    //     }
    // };

    interaction.create_interaction_response(&ctx.http, |response: &mut serenity::builder::CreateInteractionResponse| {
        response.kind(InteractionResponseType::DeferredChannelMessageWithSource)
        .interaction_response_data(|f| f.ephemeral(true))
        
    })
    .await;

    let manager = songbird::get(ctx).await
        .expect("Songbird Voice client placed in at initialisation.").clone();
        
    let (handler_lock, conn_result) = manager.join(guild_id, connect_to).await;
    conn_result?;

    // if let Ok(_) = conn_result {
        // NOTE: this skips listening for the actual connection result.
        let channel: crate::AudioBufferDiscord;
        let ts_buffer: crate::TsToDiscordPipeline;
        {
            let data_read = ctx.data.read().await;
            let (ts_buf,chan) = data_read.get::<ListenerHolder>().expect("Expected CommandCounter in TypeMap.").clone();
            channel = chan;
            ts_buffer = ts_buf;
        }
        let mut handler = handler_lock.lock().await;
        let discord_input = Input::float_pcm(true, songbird::input::Reader::Extension(Box::new(ts_buffer.clone())));
        handler.play_only_source(discord_input);
        handler.add_global_event(
            CoreEvent::SpeakingStateUpdate.into(),
            Receiver::new(channel.clone()),
        );

        handler.add_global_event(
            CoreEvent::SpeakingUpdate.into(),
            Receiver::new(channel.clone()),
        );

        handler.add_global_event(
            CoreEvent::VoicePacket.into(),
            Receiver::new(channel.clone()),
        );

        handler.add_global_event(
            CoreEvent::RtcpPacket.into(),
            Receiver::new(channel.clone()),
        );

        handler.add_global_event(
            CoreEvent::ClientDisconnect.into(),
            Receiver::new(channel),
        );

    //     check_msg(msg.channel_id.say(&ctx.http, &format!("Joined {}", connect_to.mention())).await);
    // } else {
    //     check_msg(msg.channel_id.say(&ctx.http, "Error joining the channel").await);
    // }
    println!("joined");
    interaction.edit_original_interaction_response(&ctx.http, |response| {
        // response.kind(InteractionResponseType::ChannelMessageWithSource).content("Joined")
        response.content("Joined")  
    })
    .await?;
    // interaction.create_followup_message(&ctx.http, |response| {
    //     response.content("Joined")
    // }).await?;
    Ok(())
}

#[command]
#[only_in(guilds)]
async fn leave(ctx: &Context, msg: &Message) -> CommandResult {
    let guild = msg.guild(&ctx.cache).expect("No guild found!");
    let guild_id = guild.id;

    let manager = songbird::get(ctx).await
        .expect("Songbird Voice client placed in at initialisation.").clone();
    let has_handler = manager.get(guild_id).is_some();

    if has_handler {
        if let Err(e) = manager.remove(guild_id).await {
            check_msg(msg.channel_id.say(&ctx.http, format!("Failed: {:?}", e)).await);
        }

        check_msg(msg.channel_id.say(&ctx.http, "Left voice channel").await);
    } else {
        check_msg(msg.reply(ctx, "Not in a voice channel").await);
    }

    Ok(())
}

#[command]
#[only_in(guilds)]
async fn mute(ctx: &Context, msg: &Message) -> CommandResult {
    let guild = msg.guild(&ctx.cache).expect("No guild found!");
    let guild_id = guild.id;

    let manager = songbird::get(ctx).await
        .expect("Songbird Voice client placed in at initialisation.").clone();

    let handler_lock = match manager.get(guild_id) {
        Some(handler) => handler,
        None => {
            check_msg(msg.reply(ctx, "Not in a voice channel").await);

            return Ok(());
        },
    };

    let mut handler = handler_lock.lock().await;

    if handler.is_mute() {
        check_msg(msg.channel_id.say(&ctx.http, "Already muted").await);
    } else {
        if let Err(e) = handler.mute(true).await {
            check_msg(msg.channel_id.say(&ctx.http, format!("Failed: {:?}", e)).await);
        }

        check_msg(msg.channel_id.say(&ctx.http, "Now muted").await);
    }

    Ok(())
}

#[command]
async fn ping(context: &Context, msg: &Message) -> CommandResult {
    check_msg(msg.channel_id.say(&context.http, "Pong!").await);

    Ok(())
}

#[command]
#[only_in(guilds)]
async fn play(ctx: &Context, msg: &Message, mut args: Args) -> CommandResult {
    let url = match args.single::<String>() {
        Ok(url) => url,
        Err(_) => {
            check_msg(msg.channel_id.say(&ctx.http, "Must provide a URL to a video or audio").await);

            return Ok(());
        },
    };

    if !url.starts_with("http") {
        check_msg(msg.channel_id.say(&ctx.http, "Must provide a valid URL").await);

        return Ok(());
    }

    let guild = msg.guild(&ctx.cache).expect("No guild found!");
    let guild_id = guild.id;

    let manager = songbird::get(ctx).await
        .expect("Songbird Voice client placed in at initialisation.").clone();

    if let Some(handler_lock) = manager.get(guild_id) {
        let mut handler = handler_lock.lock().await;

        let source = match songbird::ytdl(&url).await {
            Ok(source) => source,
            Err(why) => {
                println!("Err starting source: {:?}", why);

                check_msg(msg.channel_id.say(&ctx.http, "Error sourcing ffmpeg").await);

                return Ok(());
            },
        };

        handler.play_source(source);

        check_msg(msg.channel_id.say(&ctx.http, "Playing song").await);
    } else {
        check_msg(msg.channel_id.say(&ctx.http, "Not in a voice channel to play in").await);
    }

    Ok(())
}

#[command]
#[only_in(guilds)]
async fn undeafen(ctx: &Context, msg: &Message) -> CommandResult {
    let guild = msg.guild(&ctx.cache).expect("No guild found!");
    let guild_id = guild.id;

    let manager = songbird::get(ctx).await
        .expect("Songbird Voice client placed in at initialisation.").clone();

    if let Some(handler_lock) = manager.get(guild_id) {
        let mut handler = handler_lock.lock().await;
        if let Err(e) = handler.deafen(false).await {
            check_msg(msg.channel_id.say(&ctx.http, format!("Failed: {:?}", e)).await);
        }

        check_msg(msg.channel_id.say(&ctx.http, "Undeafened").await);
    } else {
        check_msg(msg.channel_id.say(&ctx.http, "Not in a voice channel to undeafen in").await);
    }

    Ok(())
}

#[command]
#[only_in(guilds)]
async fn unmute(ctx: &Context, msg: &Message) -> CommandResult {
    let guild = msg.guild(&ctx.cache).expect("No guild found!");
    let guild_id = guild.id;
    
    let manager = songbird::get(ctx).await
        .expect("Songbird Voice client placed in at initialisation.").clone();

    if let Some(handler_lock) = manager.get(guild_id) {
        let mut handler = handler_lock.lock().await;
        if let Err(e) = handler.mute(false).await {
            check_msg(msg.channel_id.say(&ctx.http, format!("Failed: {:?}", e)).await);
        }

        check_msg(msg.channel_id.say(&ctx.http, "Unmuted").await);
    } else {
        check_msg(msg.channel_id.say(&ctx.http, "Not in a voice channel to unmute in").await);
    }

    Ok(())
}

/// Checks that a message successfully sent; if not, then logs why to stdout.
fn check_msg(result: SerenityResult<Message>) {
    if let Err(why) = result {
        println!("Error sending message: {:?}", why);
    }
}

struct Receiver{
    sink: crate::AudioBufferDiscord,
}

impl Receiver {
    pub fn new(voice_receiver: crate::AudioBufferDiscord) -> Self {
        // You can manage state here, such as a buffer of audio packet bytes so
        // you can later store them in intervals.
        Self {
            sink: voice_receiver,
        }
    }
}

#[async_trait]
impl VoiceEventHandler for Receiver {
    #[allow(unused_variables)]
    async fn act(&self, ctx: &EventContext<'_>) -> Option<Event> {
        use EventContext as Ctx;
        match ctx {
            Ctx::SpeakingStateUpdate(
                Speaking {speaking, ssrc, user_id, ..}
            ) => {
                // Discord voice calls use RTP, where every sender uses a randomly allocated
                // *Synchronisation Source* (SSRC) to allow receivers to tell which audio
                // stream a received packet belongs to. As this number is not derived from
                // the sender's user_id, only Discord Voice Gateway messages like this one
                // inform us about which random SSRC a user has been allocated. Future voice
                // packets will contain *only* the SSRC.
                //
                // You can implement logic here so that you can differentiate users'
                // SSRCs and map the SSRC to the User ID and maintain this state.
                // Using this map, you can map the `ssrc` in `voice_packet`
                // to the user ID and handle their audio packets separately.
                //println!(
                //     "Speaking state update: user {:?} has SSRC {:?}, using {:?}",
                //     user_id,
                //     ssrc,
                //     speaking,
                // );
            },
            Ctx::SpeakingUpdate(_) => {
                // You can implement logic here which reacts to a user starting
                // or stopping speaking.
                //println!(
                //     "Source {} has {} speaking.",
                //     ssrc,
                //     if *speaking {"started"} else {"stopped"},
                // );
            },
            Ctx::VoicePacket(data) => {
                // {audio, packet, payload_offset, payload_end_pad}
                // An event which fires for every received audio packet,

                // get raw opus package, we don't decode here and leave that to the AudioHandler
                let packet = data.packet;
                let last_bytes = packet.payload.len() - data.payload_end_pad;
                let data = &packet.payload[data.payload_offset..last_bytes];
                let start = if packet.extension != 0 {
                    match RtpExtensionPacket::new(data) {
                        Some(v) => v.packet_size(),
                        None => {
                            eprintln!("Extension packet indicated, but insufficient space.");
                            return None;
                        }
                    }
                } else {
                    0
                };
                let opus_slice = &data[start..];
                let dur;
                {
                    let time = std::time::Instant::now();
                    let mut lock = self.sink.lock().await;
                    dur = time.elapsed();
                    if let Err(e) = lock.handle_packet(packet.ssrc, packet.sequence.0.0, opus_slice.to_vec()) {
                        eprintln!("Failed to handle Discord voice packet: {}",e);
                    }
                    if dur.as_millis() > 1 {
                        eprintln!("Acquiring lock took {}ms",dur.as_millis());
                    }
                }
                
            },
            Ctx::RtcpPacket(_) => {
                // An event which fires for every received rtcp packet,
                // containing the call statistics and reporting information.
                //println!("RTCP packet received: {:?}", packet);
            },
            Ctx::ClientDisconnect(
                ClientDisconnect {user_id, ..}
            ) => {
                // You can implement your own logic here to handle a user who has left the
                // voice channel e.g., finalise processing of statistics etc.
                // You will typically need to map the User ID to their SSRC; observed when
                // speaking or connecting.

                println!("Client disconnected: user {:?}", user_id);
            },
            _ => {
                // We won't be registering this struct for any more event classes.
                unimplemented!()
            }
        }

        None
    }
}