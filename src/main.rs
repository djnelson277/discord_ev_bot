use discord_ev_bot::config::Config;
use std::path::PathBuf;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let path = std::env::var("EVBOT_CONFIG")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("config/default.toml"));
    let cfg = Config::load(&path)?;
    println!("loaded config: {cfg:?}");
    Ok(())
}
