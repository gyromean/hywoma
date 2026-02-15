use anyhow::Result;
use std::env;

mod app;
mod hyprland;

fn main() -> Result<()> {
    let args: Vec<String> = env::args().skip(1).collect();
    if args.len() < 1 {
        eprintln!("Requires argument");
        return Ok(());
    }
    if args[0] == "server" {
        return app::server();
    }

    app::send_command(&args)?;

    Ok(())
}
