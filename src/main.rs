#[macro_use]
extern crate anyhow;

#[macro_use]
extern crate log;

use std::{
    env,
    fs::{self, File},
    io::Write,
    iter,
    path::{Path, PathBuf},
    sync::Arc,
};

use anyhow::{Context as _, Error, Result};
use replay_queue::ReplayQueue;
use rosu_v2::Osu;
use serenity::{framework::standard::macros::hook, model::prelude::*, prelude::*};

mod checks;
mod commands;
mod logging;
mod process_replays;
mod replay_queue;
mod server_settings;
mod util;

use commands::*;
use process_replays::*;

#[derive(Debug)]
pub struct Data {
    settings: std::sync::Mutex<server_settings::Root>,
}

type PoiseContext<'a> = poise::Context<'a, Data, Error>;

const DEFAULT_PREFIX: &str = "!!";

struct ReplayHandler;
impl TypeMapKey for ReplayHandler {
    type Value = Arc<ReplayQueue>;
}

struct ServerSettings;
impl TypeMapKey for ServerSettings {
    type Value = server_settings::Root;
}

#[hook]
async fn event_listener(
    ctx: &Context,
    event: &poise::Event<'_>,
    _: poise::FrameworkContext<'_, Data, Error>,
    _: &Data,
) -> Result<(), Error> {
    match event {
        poise::Event::Ready { data_about_bot } => {
            info!("{} is connected!", data_about_bot.user.name);
            ctx.set_activity(Activity::playing(format!(
                "in {} servers | !!help",
                ctx.cache.guilds().len()
            )))
            .await;
        }
        poise::Event::Message { new_message: msg } => {
            if msg.content.contains("start") || msg.content.contains("end") {
                return Ok(());
            }

            match parse_attachment_replay(&msg, &ctx.data, None).await {
                Ok(AttachmentParseSuccess::NothingToDo) => {}
                Ok(AttachmentParseSuccess::BeingProcessed) => {
                    let reaction = ReactionType::Unicode("✅".to_string());
                    if let Err(why) = msg.react(&ctx, reaction).await {
                        let err = Error::new(why)
                            .context("failed to react after attachment parse success");
                        warn!("{:?}", err);
                    }
                }
                Err(AttachmentParseError::IncorrectMode(_)) => {
                    if let Err(why) = msg
                        .reply(&ctx, "danser only accepts osu!standard plays, sorry :(")
                        .await
                    {
                        let err =
                            Error::new(why).context("failed to reply after attachment parse error");
                        warn!("{:?}", err);
                    }
                }
                Err(why) => {
                    let err = Error::new(why).context("failed to parse attachment");
                    warn!("{:?}", err);

                    if let Err(why) = msg.reply(&ctx, "something went wrong, blame mezo").await {
                        let err =
                            Error::new(why).context("failed to reply after attachment parse error");
                        warn!("{:?}", err);
                    }
                }
            }
        }
        poise::Event::GuildCreate { is_new, .. } => {
            if *is_new {
                ctx.set_activity(Activity::playing(format!(
                    "in {} servers | !!help",
                    ctx.cache.guilds().len()
                )))
                .await;
            }
        }
        poise::Event::GuildDelete { .. } => {
            ctx.set_activity(Activity::playing(format!(
                "in {} servers | !!help",
                ctx.cache.guilds().len()
            )))
            .await;
        }
        _ => {}
    }

    Ok(())
}

#[poise::command(prefix_command)]
async fn register(ctx: PoiseContext<'_>) -> Result<(), Error> {
    poise::builtins::register_application_commands_buttons(ctx).await?;
    Ok(())
}

#[tokio::main]
async fn main() {
    dotenv::dotenv().expect("Failed to read .env file");
    logging::initialize().expect("Failed to initialize logging");

    match create_missing_folders_and_files().await {
        Ok(_) => info!("created folders and files"),
        Err(why) => panic!("{:?}", why),
    }

    let token = env::var("DISCORD_TOKEN").expect("Expected a token from the env");

    let client_id: u64 = env::var("CLIENT_ID")
        .expect("Expected client id from the env")
        .parse()
        .expect("Expected client id to be an integer");

    let client_secret: String =
        env::var("CLIENT_SECRET").expect("Expected client secret from the env");

    let framework = poise::Framework::builder()
        .token(token)
        .options(poise::FrameworkOptions {
            prefix_options: poise::PrefixFrameworkOptions {
                // stripped_dynamic_prefix: Some(|a, b, c| Box::pin(dynamic_prefix(a, b, c))),
                stripped_dynamic_prefix: Some(dynamic_prefix),
                ..Default::default()
            },
            pre_command: log_command,
            post_command: finished_command,
            on_error: dispatch_error,
            listener: event_listener,
            commands: vec![setup(), register()],
            ..Default::default()
        })
        .intents(GatewayIntents::all())
        .user_data_setup(|_, _, _| {
            Box::pin(async move {
                let settings_content = tokio::fs::read_to_string("src/server_settings.json")
                    .await
                    .context("failed to read `src/server_settings.json`")?;
                Ok(Data {
                    settings: serde_json::from_str(&settings_content)
                        .context("failed to deserialize server settings")?,
                })
            })
        })
        .build()
        .await
        .unwrap();

    let osu: Osu = match Osu::new(client_id, client_secret).await {
        Ok(client) => client,
        Err(why) => panic!(
            "{:?}",
            Error::new(why).context("failed to create osu! client")
        ),
    };

    let reqwest_client = match reqwest::Client::builder().build() {
        Ok(client) => client,
        Err(why) => panic!(
            "{:?}",
            Error::new(why).context("failed to create reqwest client"),
        ),
    };

    let http = Arc::clone(&framework.client().cache_and_http.http);
    let queue = Arc::new(ReplayQueue::new());
    tokio::spawn(process_replay(
        osu,
        http,
        reqwest_client,
        Arc::clone(&queue),
    ));

    if let Err(why) = framework.start().await {
        error!("{:?}", Error::new(why).context("critical client error"));
    }

    info!("Shutting down");
}

async fn create_missing_folders_and_files() -> Result<()> {
    use anyhow::Context;

    fs::create_dir_all("../Songs").context("failed to create `../Songs`")?;
    fs::create_dir_all("../Skins").context("failed to create `../Skins`")?;
    fs::create_dir_all("../Replays").context("failed to create `../Replays`")?;
    fs::create_dir_all("../Downloads").context("failed to create `../Downloads`")?;
    fs::create_dir_all("../danser").context("failed to create `../danser`")?;

    if PathBuf::from("../danser").read_dir()?.next().is_none() {
        info!("danser not found! please download from https://github.com/Wieku/danser-go/releases/")
    }

    if !Path::new("src/server_settings.json").exists() {
        let mut file = File::create("src/server_settings.json")
            .context("failed to create file `src/server_settings.json`")?;
        file.write_all(b"{\"Servers\":[]}")
            .context("failed writing to `src/server_settings.json`")?;
    }

    Ok(())
}

#[hook]
async fn log_command(ctx: PoiseContext<'fut>) {
    info!(
        "Got command '{}' by user '{}'",
        ctx.command().name,
        ctx.author().name
    );
}

#[hook]
async fn finished_command(ctx: PoiseContext<'fut>) {
    info!("Processed command '{}'", ctx.command().name);
}

#[hook]
async fn dynamic_prefix(
    ctx: &'fut Context,
    msg: &'fut Message,
    _: &'fut Data,
) -> Result<Option<(&'fut str, &'fut str)>, Error> {
    let prefix = if let Some(ref guild_id) = msg.guild_id {
        let data = ctx.data.read().await;
        let settings = data.get::<ServerSettings>().unwrap();

        let prefix = settings
            .servers
            .get(guild_id)
            .and_then(|server| {
                server
                    .prefixes
                    .iter()
                    .map(String::as_str)
                    .chain(iter::once(DEFAULT_PREFIX))
                    .fold(None, |longest, prefix| {
                        if !msg.content.starts_with(prefix)
                            || longest
                                .map(|longest: &str| prefix.len() <= longest.len())
                                .is_some()
                        {
                            longest
                        } else {
                            Some(prefix)
                        }
                    })
            })
            .unwrap_or(DEFAULT_PREFIX);

        prefix.to_owned()
    } else {
        DEFAULT_PREFIX.to_owned()
    };

    Ok(msg
        .content
        .strip_prefix(&prefix)
        .map(|rest| (&msg.content[..prefix.len()], rest)))
}

#[hook]
async fn dispatch_error(error: poise::FrameworkError<'fut, Data, Error>) {
    match error {
        poise::FrameworkError::CommandCheckFailed { error, .. } => {
            if let Some(error) = error {
                info!("Check failed: {error}");
            }
        }
        poise::FrameworkError::Command { ctx, error } => {
            warn!("Command '{}' returned error: {}", ctx.command().name, error);
            let mut e = &*error as &dyn std::error::Error;

            while let Some(src) = e.source() {
                warn!("  - caused by: {}", src);
                e = src;
            }
        }
        _ => info!("Other: {error:?}"),
    }
}
