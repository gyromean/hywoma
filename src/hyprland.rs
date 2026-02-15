use anyhow::{Result, anyhow};
use serde::Deserialize;
use std::env;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::mpsc;

use crate::app::Message;

#[derive(Debug)]
pub enum HyprlandSocketKind {
    Command,
    Event,
}

#[derive(Debug, Clone, Copy)]
pub struct Workspace {
    pub workspace: u64,
    pub monitor: u64,
    pub group: u64,
}

impl Workspace {
    pub fn from_id(mut id: u64) -> Self {
        id -= 1;
        let workspace = id % 10 + 1;
        id /= 10;
        let monitor = id % 10 + 1;
        id /= 10;
        let group = id % 10;
        Workspace {
            workspace,
            monitor,
            group,
        }
    }
    pub fn to_id(&self) -> u64 {
        (self.workspace - 1) + 10 * (self.monitor - 1) + 100 * self.group + 1
    }
}

// returns monitor ids sorted by their x position
pub fn get_monitor_ids() -> Result<Vec<u64>> {
    #[derive(Debug, Deserialize)]
    struct MonitorEntry {
        id: u64,
        x: u64,
    }
    let monitors_json = hyprctl("-j/monitors")?;
    let mut parsed: Vec<MonitorEntry> = serde_json::from_str(&monitors_json)?;
    parsed.sort_unstable_by_key(|m| m.x);
    Ok(parsed.into_iter().map(|m| m.id).collect())
}

pub fn get_active_workspace() -> Result<Workspace> {
    let activeworkspace_json = hyprctl("-j/activeworkspace")?;
    let v: serde_json::Value = serde_json::from_str(&activeworkspace_json)?;
    let workspace_id = v["id"].as_u64().unwrap();
    Ok(Workspace::from_id(workspace_id))
}

fn get_socket_path(kind: HyprlandSocketKind) -> Result<PathBuf> {
    let xdg_runtime_dir = env::var("XDG_RUNTIME_DIR")?;
    let hyprland_instance_signature = env::var("HYPRLAND_INSTANCE_SIGNATURE")?;
    let path = PathBuf::from(xdg_runtime_dir)
        .join("hypr")
        .join(hyprland_instance_signature)
        .join(match kind {
            HyprlandSocketKind::Command => ".socket.sock",
            HyprlandSocketKind::Event => ".socket2.sock",
        });
    Ok(path)
}

pub fn event_reader(tx: mpsc::Sender<Message>) -> Result<()> {
    let path = get_socket_path(HyprlandSocketKind::Event)?;
    let stream = UnixStream::connect(path)?;
    let reader = BufReader::new(stream);

    for line in reader.lines() {
        let line = line?;
        let (event, data) = line.split_once(">>").ok_or(anyhow!(
            "Hyprland socket provided a line in an unexpected format: '{line}'"
        ))?;
        let msg: Message = match event {
            "workspacev2" => {
                Message::ActiveWorkspaceChangedID(data.split_once(",").unwrap().0.parse()?)
            }
            "focusedmonv2" => {
                Message::ActiveWorkspaceChangedID(data.split_once(",").unwrap().1.parse()?)
            }
            _ => continue,
        };
        tx.send(msg)?;
    }
    Ok(())
}

pub fn hyprctl(command: &str) -> Result<String> {
    let path = get_socket_path(HyprlandSocketKind::Command)?;
    let mut stream = UnixStream::connect(path)?;

    stream.write_all(command.as_bytes())?;
    stream.flush()?;

    let mut reader = BufReader::new(stream);
    let mut response = String::new();
    reader.read_to_string(&mut response)?;

    Ok(response)
}
