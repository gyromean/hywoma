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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Workspace {
    pub workspace: u64,
    pub monitor: u64,
    pub group: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MonitorInfo {
    pub id: u64,
    pub name: String,
    pub x: i64,
}

impl Workspace {
    pub fn from_id(mut id: u64) -> Self {
        // Legacy encoded workspace layout. New opaque IDs (1000+) should not be decoded this way
        // unless we are explicitly in the compatibility path.
        id -= 1;
        let workspace = id % 10 + 1;
        id /= 10;
        let monitor = id % 10 + 1;
        id /= 10;
        let group = id;
        Workspace {
            workspace,
            monitor,
            group,
        }
    }
    #[cfg(test)]
    pub fn to_id(&self) -> u64 {
        (self.workspace - 1) + 10 * (self.monitor - 1) + 100 * self.group + 1
    }
}

pub fn get_monitors() -> Result<Vec<MonitorInfo>> {
    #[derive(Debug, Deserialize)]
    struct MonitorEntry {
        id: u64,
        name: String,
        x: i64,
    }
    let monitors_json = hyprctl("-j/monitors")?;
    let mut parsed: Vec<MonitorEntry> = serde_json::from_str(&monitors_json)?;
    // Slot assignment currently follows left-to-right layout. This is simple, but not a permanent
    // identity policy: an external monitor placed left of eDP-1 can become slot 1.
    parsed.sort_unstable_by_key(|m| m.x);
    Ok(parsed
        .into_iter()
        .map(|m| MonitorInfo {
            id: m.id,
            name: m.name,
            x: m.x,
        })
        .collect())
}

pub fn get_active_workspace_id() -> Result<u64> {
    let activeworkspace_json = hyprctl("-j/activeworkspace")?;
    let v: serde_json::Value = serde_json::from_str(&activeworkspace_json)?;
    Ok(v["id"].as_u64().unwrap())
}

pub fn get_active_workspace_monitor_id() -> Result<Option<u64>> {
    let activeworkspace_json = hyprctl("-j/activeworkspace")?;
    let v: serde_json::Value = serde_json::from_str(&activeworkspace_json)?;
    Ok(v["monitorID"].as_u64())
}

pub fn get_workspace_ids() -> Result<Vec<u64>> {
    #[derive(Debug, Deserialize)]
    struct WorkspaceEntry {
        id: u64,
    }

    let workspaces_json = hyprctl("-j/workspaces")?;
    let parsed: Vec<WorkspaceEntry> = serde_json::from_str(&workspaces_json)?;
    Ok(parsed.into_iter().map(|workspace| workspace.id).collect())
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
            // create/destroy events drive present_workspace_ids. Allocated mappings can outlive
            // destroyed Hyprland workspaces, but AGS should only display present IDs.
            "createworkspacev2" => Message::WorkspaceCreated {
                workspace_id: data.split_once(",").unwrap().0.parse()?,
            },
            "destroyworkspacev2" => Message::WorkspaceDestroyed {
                workspace_id: data.split_once(",").unwrap().0.parse()?,
            },
            "workspacev2" => Message::ActiveWorkspaceChanged {
                workspace_id: data.split_once(",").unwrap().0.parse()?,
                monitor_name: None,
            },
            "focusedmonv2" => {
                let (monitor_name, workspace_id) = data.split_once(",").unwrap();
                // focusedmonv2 includes the output name, which is critical for old encoded fallback
                // events on hotplugged/headless monitors where the encoded monitor number is not
                // trustworthy.
                Message::ActiveWorkspaceChanged {
                    workspace_id: workspace_id.parse()?,
                    monitor_name: Some(monitor_name.to_string()),
                }
            }
            "monitoradded" | "monitoraddedv2" | "monitorremoved" | "monitorremovedv2" => {
                // Topology events are intentionally coarse. The app layer re-reads monitors and the
                // active workspace outside the hot path to recover from Hyprland's transient events
                // during monitor removal.
                Message::MonitorTopologyChanged
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

pub fn hyprctl_dispatch(command: &str) -> Result<String> {
    let response = hyprctl(command)?;
    let trimmed = response.trim();
    let lower = trimmed.to_ascii_lowercase();
    if lower.starts_with("error")
        || lower.starts_with("invalid")
        || lower.starts_with("unknown")
        || lower.contains("failed")
    {
        return Err(anyhow!("hyprctl `{command}` failed: {trimmed}"));
    }

    Ok(response)
}

#[cfg(test)]
mod tests {
    use super::Workspace;

    #[test]
    fn workspace_id_roundtrip_preserves_single_digit_group() {
        let workspace = Workspace {
            workspace: 10,
            monitor: 3,
            group: 9,
        };

        assert_eq!(Workspace::from_id(workspace.to_id()), workspace);
    }

    #[test]
    fn workspace_id_roundtrip_preserves_multi_digit_group() {
        let workspace = Workspace {
            workspace: 1,
            monitor: 1,
            group: 10,
        };

        assert_eq!(Workspace::from_id(workspace.to_id()), workspace);
    }
}
