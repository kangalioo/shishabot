use crate::{server_settings::Server, PoiseContext};
use anyhow::{Context, Error};
use serenity::builder::ParseValue;
use serenity::model::id::ChannelId;
use serenity::utils::Color;

/// Setup the input and output channels for your server
///
/// Usage `setup [input-channel] [output-channel]`
/// Example: `setup #channel-1 #channel-2`
#[poise::command(
    prefix_command,
    slash_command,
    required_permissions = "MANAGE_CHANNELS",
    guild_only
)]
pub async fn setup(
    ctx: PoiseContext<'_>,
    #[description = "Input channel"] id1: Option<ChannelId>,
    #[description = "Output channel"] id2: Option<ChannelId>,
) -> Result<(), Error> {
    if let (Some(id1), Some(id2)) = (id1, id2) {
        let guild_id = ctx.guild_id().unwrap_or_default();

        let edited_settings = {
            let mut settings = ctx.data().settings.lock().unwrap();
            settings
                .servers
                .entry(guild_id)
                .and_modify(|server| {
                    server.input_channel = id1;
                    server.output_channel = id2;
                })
                .or_insert_with(|| Server {
                    input_channel: id1,
                    output_channel: id2,
                    prefixes: Vec::new(),
                });

            serde_json::to_string(&*settings).context("failed to serialize server settings")?
        };

        if let Err(why) = tokio::fs::write("src/server_settings.json", edited_settings).await {
            let err = Error::new(why).context("failed to edit server specific settings");
            warn!("{err:?}");
        }

        ctx.say("Successfully changed settings!").await?;
    } else if let Some(o) = {
        let settings = ctx.data().settings.lock().unwrap();
        let o = settings
            .servers
            .get(&ctx.guild_id().unwrap_or_default())
            .cloned();
        o
    } {
        if o.output_channel != 0 {
            ctx.send(|m| {
                if let PoiseContext::Prefix(ctx) = ctx {
                    m.reference_message((ctx.msg.channel_id, ctx.msg.id));
                }
                m.allowed_mentions(|f| {
                    f.replied_user(false)
                        .parse(ParseValue::Everyone)
                        .parse(ParseValue::Users)
                        .parse(ParseValue::Roles)
                });
                m.embed(|e| {
                    e.title(format!(
                        "Current channel setup{}",
                        if let Some(guild) = ctx.guild_id() {
                            format!(" for {}", guild.name(ctx.discord()).unwrap_or_default())
                        } else {
                            String::new()
                        }
                    ))
                    .description(format!(
                        "**Input Channel:** <#{}>\n**Output Channel:** <#{}>",
                        o.input_channel, o.output_channel
                    ))
                    .footer(|f| f.text("Use !!setup [input-channel] [output-channel] to edit this"))
                    .color(Color::new(15785176))
                })
            })
            .await?;
        }
    } else {
        ctx.say("You need to mention 2 channels!").await?;
    }

    Ok(())
}
