use anyhow::Result;
use std::env;

mod app;
mod hyprland;
mod state;

fn main() -> Result<()> {
    let args: Vec<String> = env::args().skip(1).collect();
    if args.len() < 1 {
        eprintln!("Requires argument");
        return Ok(());
    }
    if args[0] == "server" {
        return app::server();
    }
    if args[0] == "events" {
        return app::stream_events();
    }

    app::send_command(&args)?;

    Ok(())
}
