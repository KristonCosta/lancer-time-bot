use std::env;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use chrono::offset::Utc;
use chrono::{DateTime, Local, NaiveDateTime, TimeZone};
use chrono_tz::America::Toronto;
use chrono_tz::Tz;
use serenity::async_trait;
use serenity::model::channel::Message;
use serenity::model::gateway::{Activity, Ready};
use serenity::model::id::{ChannelId, GuildId};
use serenity::prelude::*;
use sled::{Db, IVec};
use uuid::Uuid;

const TIMESTAMP_FORMAT: &str = "%Y-%m-%d %H:%M";
const REMINDER_PREFIX: &str = "reminder-";
const CHANNEL_KEY: &str = "channel";
const PACKED_VALUE_SEPARATOR: &str = "|";

struct Handler {
    is_loop_running: AtomicBool,
    db: Db,
}

// This is bad, but we are just going to serialize and deserialize from strings
struct SledType {
    inner: IVec,
}

impl From<IVec> for SledType {
    fn from(inner: IVec) -> Self {
        Self { inner }
    }
}

impl From<String> for SledType {
    fn from(input: String) -> Self {
        Self {
            inner: input.as_str().into(),
        }
    }
}

impl Into<String> for SledType {
    fn into(self) -> String {
        std::str::from_utf8(&self.inner).unwrap().to_string()
    }
}

impl Into<IVec> for SledType {
    fn into(self) -> IVec {
        self.inner
    }
}

fn pack_value(channel_id: &ChannelId, datetime: &DateTime<Tz>) -> String {
    format!(
        "{}{}{}",
        channel_id.0,
        PACKED_VALUE_SEPARATOR,
        datetime.naive_local().format(TIMESTAMP_FORMAT).to_string()
    )
}

fn unpack_value(packed_value: &str) -> Option<(ChannelId, DateTime<Tz>)> {
    packed_value
        .split_once(PACKED_VALUE_SEPARATOR)
        .map(|(channel, datetime)| {
            (
                ChannelId(channel.parse::<u64>().unwrap()),
                parse_timestamp(datetime).unwrap(),
            )
        })
}

async fn send_message(ctx: &Context, channel_id: &ChannelId, reply: &str) {
    if let Err(why) = channel_id.say(&ctx.http, reply).await {
        eprintln!("Error sending message: {:?}", why);
    }
}

fn make_reminder_key(uuid: String) -> String {
    format!("{}{}", REMINDER_PREFIX, uuid)
}

fn make_reminder(db: &Db, channel_id: &ChannelId, datetime: DateTime<Tz>) {
    let uuid = Uuid::new_v4().to_string();
    db.insert(
        make_reminder_key(uuid),
        pack_value(channel_id, &datetime).as_str(),
    )
    .unwrap();
}

async fn process_reminder(handler: &Handler, ctx: &Context, msg: Message) {
    if let Some((_, datetime)) = msg.content.split_once(" ") {
        if let Some(datetime) = parse_timestamp(datetime) {
            send_message(
                &ctx,
                &msg.channel_id,
                format!("Set reminder for {}", datetime).as_str(),
            )
            .await;
            make_reminder(&handler.db, &msg.channel_id, datetime);
        } else {
            send_message(
                &ctx,
                &msg.channel_id,
                format!(
                    "Invalid datetime {}! Format is `!reminder YYYY-MM-DD HH:SS`",
                    datetime
                )
                .as_str(),
            )
            .await;
        }
    } else {
        send_message(
            &ctx,
            &msg.channel_id,
            "No arguments supplied to !reminder. Format is `!reminder YYYY-MM-DD HH:SS`",
        )
        .await;
    }

    handler
        .db
        .insert("channel", msg.channel_id.0.to_string().as_str())
        .unwrap();
}

async fn process_pending_reminders(handler: &Handler, ctx: &Context, msg: Message) {
    let scanner = handler.db.scan_prefix(REMINDER_PREFIX);
    let mut pending_reminders = Vec::new();
    for reminder in scanner {
        if let Ok((key, value)) = reminder {
            let packed_value: String = SledType::from(value).into();

            if let Some((channel_id, datetime)) = unpack_value(&packed_value) {
                if channel_id == msg.channel_id {
                    pending_reminders.push((SledType::from(key), datetime));
                }
            }
        }
    }

    msg.channel_id
        .send_message(ctx, |m| {
            m.embed(|e| {
                let c = e.title("Pending reminders");
                for (key, value) in pending_reminders {
                    let key: String = key.into();
                    c.field(value, key, false);
                }
                c
            })
        })
        .await
        .unwrap();
}

async fn process_cancel(handler: &Handler, ctx: &Context, msg: Message) {
    if let Some((_, key)) = msg.content.split_once(" ") {
        if let Ok(_) = handler.db.remove(key) {
            send_message(&ctx, &msg.channel_id, "Cancelled reminder.").await;
        } else {
            send_message(
            &ctx,
            &msg.channel_id,
            "Failed to cancel reminder. Please supply a valid uuid from the `!pending` command.\n Format is `!cancel reminder-<uuid>`",
        )
        .await;
        }
    } else {
        send_message(
            &ctx,
            &msg.channel_id,
            "No arguments supplied to !cancel. Format is `!cancel reminder-<uuid>`",
        )
        .await;
    }
}

#[async_trait]
impl EventHandler for Handler {
    async fn message(&self, ctx: Context, msg: Message) {
        if msg.content.starts_with("!reminder") {
            process_reminder(self, &ctx, msg).await
        } else if msg.content.starts_with("!pending") {
            process_pending_reminders(self, &ctx, msg).await
        } else if msg.content.starts_with("!cancel") {
            process_cancel(self, &ctx, msg).await
        }
    }

    async fn ready(&self, _ctx: Context, ready: Ready) {
        println!("{} is connected!", ready.user.name);
    }

    // We use the cache_ready event just in case some cache operation is required in whatever use
    // case you have for this.
    async fn cache_ready(&self, ctx: Context, _guilds: Vec<GuildId>) {
        println!("Cache built successfully!");

        // it's safe to clone Context, but Arc is cheaper for this use case.
        // Untested claim, just theoretically. :P
        let ctx = Arc::new(ctx);

        // We need to check that the loop is not already running when this event triggers,
        // as this event triggers every time the bot enters or leaves a guild, along every time the
        // ready shard event triggers.
        //
        // An AtomicBool is used because it doesn't require a mutable reference to be changed, as
        // we don't have one due to self being an immutable reference.
        if !self.is_loop_running.load(Ordering::Relaxed) {
            // We have to clone the Arc, as it gets moved into the new thread.
            let ctx1 = Arc::clone(&ctx);
            // tokio::spawn creates a new green thread that can run in parallel with the rest of
            // the application.
            // let mut r = sled.clone();
            let db_ref = self.db.clone();
            tokio::spawn(async move {
                loop {
                    // We clone Context again here, because Arc is owned, so it moves to the
                    // new function.
                    process_reminders(Arc::clone(&ctx1), &db_ref).await;
                    tokio::time::sleep(Duration::from_secs(10)).await;
                }
            });

            // Now that the loop is running, we set the bool to true
            self.is_loop_running.swap(true, Ordering::Relaxed);
        }
    }
}

async fn process_reminders(ctx: Arc<Context>, db: &Db) {
    let scanner = db.scan_prefix(REMINDER_PREFIX);
    let current_time = Local::now();

    for reminder in scanner {
        if let Ok((key, value)) = reminder {
            let packed_value: String = SledType::from(value).into();

            if let Some((channel_id, datetime)) = unpack_value(&packed_value) {
                if datetime < current_time {
                    send_message(
                        &ctx,
                        &channel_id,
                        format!("Triggering a pending reminder {} @here", datetime).as_str(),
                    )
                    .await;
                    db.remove(key).unwrap();
                }
            } else {
                eprintln!(
                    "Found a pending reminder but it was broken {}. Removing it",
                    packed_value
                );

                db.remove(key).unwrap();
            }
        }
    }
}

fn parse_timestamp(timestamp: &str) -> Option<DateTime<Tz>> {
    if let Ok(naive_datetime) = NaiveDateTime::parse_from_str(timestamp, TIMESTAMP_FORMAT) {
        Some(Toronto.from_local_datetime(&naive_datetime).unwrap())
    } else {
        None
    }
}

fn get_string_value(db: &Db, key: &str) -> Option<String> {
    if let Ok(res) = db.get(key) {
        res.map(|v| SledType::from(v).into())
    } else {
        None
    }
}

#[tokio::main]
async fn main() {
    let token = env::var("DISCORD_TOKEN").expect("Expected a token in the environment");

    let intents = GatewayIntents::GUILD_MESSAGES
        | GatewayIntents::DIRECT_MESSAGES
        | GatewayIntents::GUILDS
        | GatewayIntents::MESSAGE_CONTENT;
    let mut client = Client::builder(&token, intents)
        .event_handler(Handler {
            db: sled::open("my_db").unwrap(),
            is_loop_running: AtomicBool::new(false),
        })
        .await
        .expect("Error creating client");

    if let Err(why) = client.start().await {
        eprintln!("Client error: {:?}", why);
    }
}
