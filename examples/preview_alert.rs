//! Posts embed format previews to the ops channel so the layout can be judged
//! before the poller is wired up.
//!
//! Previews go to the OPS channel deliberately: the alert channel must stay
//! signal-only, and seeding it with synthetic alerts would be the exact habit
//! the design's anti-goals warn against.
//!
//! Run with the environment loaded:
//!
//! ```sh
//! set -a; . ./.env; set +a
//! cargo run --example preview_alert
//! ```

use discord_ev_bot::discord::publisher::{DiscordPublisher, build_embed};
use discord_ev_bot::owls::models::EvResponse;

const DISCORD_API: &str = "https://discord.com";

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let token = std::env::var("DISCORD_TOKEN")?;
    let ops_channel: u64 = std::env::var("DISCORD_OPS_CHANNEL_ID")?.parse()?;

    let parsed: EvResponse =
        serde_json::from_str(include_str!("../tests/fixtures/ev_soccer_populated.json"))?;
    let event = parsed.data[0].clone();
    let real = event.opportunities[0].clone();

    let publisher = DiscordPublisher::new(DISCORD_API, &token, ops_channel)?;

    // 1. The real captured opportunity, exactly as the API returned it:
    //    caesars at 1.15% EV, booksInConsensus 1, tradeable false.
    publisher
        .post_embed(&build_embed(&event, &real, true))
        .await?;
    println!("posted: real captured opportunity (single-source -> amber)");

    // 2. The same game with a healthy multi-source fair and a book the operator
    //    actually holds, to show the normal case.
    let mut healthy = real.clone();
    healthy.book = "fanduel".into();
    healthy.ev_pct = 4.6;
    healthy.book_price = -108.0;
    healthy.books_in_consensus = Some(4.0);
    healthy.kelly_fraction = Some(0.0212);
    healthy.quote_age_ms = Some(8_400.0);
    healthy.link = Some("https://sportsbook.fanduel.com/".into());
    publisher
        .post_embed(&build_embed(&event, &healthy, true))
        .await?;
    println!("posted: healthy multi-source opportunity (-> green)");

    // 3. The degraded case: enrichment unavailable, so the alert publishes
    //    marked rather than being held back.
    let mut bare = healthy.clone();
    bare.books_in_consensus = None;
    bare.kelly_fraction = None;
    publisher
        .post_embed(&build_embed(&event, &bare, false))
        .await?;
    println!("posted: unenriched opportunity (-> amber, marked)");

    Ok(())
}
