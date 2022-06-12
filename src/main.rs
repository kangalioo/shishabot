#[macro_use]
extern crate anyhow;

#[macro_use]
extern crate lazy_static;

#[macro_use]
extern crate log;

use std::{
    env,
    fs::{self, File},
    future::Future,
    io::Write,
    path::Path,
    pin::Pin,
    sync::Arc,
};

use anyhow::{Error, Result};
use rosu_v2::Osu;
use serenity::{
    async_trait,
    framework::standard::{
        macros::{group, hook},
        CommandResult, StandardFramework,
    },
    model::prelude::*,
    prelude::*,
};
use tokio::sync::mpsc;

mod commands;
use commands::*;

mod process_replays;
use process_replays::*;

mod logging;

const DEFAULT_PREFIX: &str = "!!";

struct ReplayHandler;
impl TypeMapKey for ReplayHandler {
    type Value = mpsc::UnboundedSender<Data>;
}

struct ServerSettings;
impl TypeMapKey for ServerSettings {
    type Value = commands::server_settings_struct::Root;
}

struct Handler;
#[async_trait]
impl EventHandler for Handler {
    async fn ready(&self, ctx: Context, ready: Ready) {
        info!("{} is connected!", ready.user.name);
        ctx.set_activity(Activity::watching("!!help - Waiting for replay"))
            .await;
    }

    async fn message(&self, ctx: Context, msg: Message) {
        let data = ctx.data.read().await;
        let sender = data.get::<ReplayHandler>().unwrap();
        let shard = ctx.shard.clone();

        match parse_attachment_replay(&msg, sender, shard, &ctx.data).await {
            Ok(AttachmentParseSuccess::NothingToDo) => {}
            Ok(AttachmentParseSuccess::BeingProcessed) => {
                let reaction = ReactionType::Unicode("✅".to_string());
                if let Err(why) = msg.react(&ctx, reaction).await {
                    let err =
                        Error::new(why).context("failed to react after attachment parse success");
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
}

#[group]
#[commands(ping, prefix)]
struct General;

#[group]
#[commands(settings, skinlist, setup)]
struct Danser;

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

    let framework = StandardFramework::new()
        .configure(|c| {
            c.with_whitespace(true)
                .prefix("")
                .dynamic_prefix(dynamic_prefix)
        })
        .before(log_command)
        .after(finished_command)
        .group(&GENERAL_GROUP)
        .group(&DANSER_GROUP)
        .help(&HELP);

    let client_fut = Client::builder(&token)
        .event_handler(Handler)
        .framework(framework);

    let mut client = match client_fut.await {
        Ok(client) => client,
        Err(why) => panic!(
            "{:?}",
            Error::new(why).context("failed to create discord client")
        ),
    };

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

    let settings_content = match tokio::fs::read_to_string("src/server_settings.json").await {
        Ok(content) => content,
        Err(why) => panic!(
            "{:?}",
            Error::new(why).context("failed to read `src/server_settings.json`")
        ),
    };

    let settings = match serde_json::from_str(&settings_content) {
        Ok(settings) => settings,
        Err(why) => panic!(
            "{:?}",
            Error::new(why).context("failed to deserialize server settings")
        ),
    };

    let http = Arc::clone(&client.cache_and_http.http);
    let (sender, receiver) = mpsc::unbounded_channel();
    tokio::spawn(process_replay(receiver, osu, http, reqwest_client));

    {
        let mut data = client.data.write().await;
        data.insert::<ReplayHandler>(sender);
        data.insert::<ServerSettings>(settings);
    }

    if let Err(why) = client.start().await {
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

    if !Path::new("src/server_settings.json").exists() {
        let mut file = File::create("src/server_settings.json")
            .context("failed to create file `src/server_settings.json`")?;
        file.write_all(b"{\"Servers\":[]}")
            .context("failed writing to `src/server_settings.json`")?;
    }

    Ok(())
}

#[hook]
async fn log_command(_: &Context, msg: &Message, cmd_name: &str) -> bool {
    info!("Got command '{}' by user '{}'", cmd_name, msg.author.name);

    true
}

#[hook]
async fn finished_command(_: &Context, _: &Message, cmd_name: &str, cmd_result: CommandResult) {
    match cmd_result {
        Ok(_) => info!("Processed command '{}'", cmd_name),
        Err(why) => {
            warn!("Command '{}' returned error: {}", cmd_name, why);
            let mut e = &*why as &dyn std::error::Error;

            while let Some(src) = e.source() {
                warn!("  - caused by: {}", src);
                e = src;
            }
        }
    }
}

fn dynamic_prefix<'fut>(
    ctx: &'fut Context,
    msg: &'fut Message,
) -> Pin<Box<(dyn Future<Output = Option<String>> + Send + 'fut)>> {
    let fut = async move {
        if let Some(ref guild_id) = msg.guild_id {
            let data = ctx.data.read().await;
            let settings = data.get::<ServerSettings>().unwrap();

            let prefix = settings
                .servers
                .get(guild_id)
                .and_then(|server| {
                    server.prefixes.iter().reduce(|longest, prefix| {
                        if prefix.len() > longest.len() && msg.content.starts_with(prefix) {
                            prefix
                        } else {
                            longest
                        }
                    })
                })
                .map_or(DEFAULT_PREFIX, |prefix| prefix.as_str());

            Some(prefix.to_owned())
        } else {
            Some(DEFAULT_PREFIX.to_owned())
        }
    };

    Box::pin(fut)
}
