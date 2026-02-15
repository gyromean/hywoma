use anyhow::Result;
use std::io::{BufReader, Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::process::exit;
use std::sync::mpsc;
use std::thread;
use std::{env, fs};

use crate::hyprland;
use crate::hyprland::Workspace;
use crate::hyprland::hyprctl;

const COMMAND_SOCKET: &str = ".hywoma.sock";

#[derive(Debug)]
pub enum Message {
    ActiveWorkspaceChangedID(u64),
    SelectWorkspace(u64),
    MoveToWorkspace(u64),
    SelectMonitor(u64),
    MoveToMonitor(u64),
}

fn process_command(command: Vec<String>, tx: &mpsc::Sender<Message>) -> Result<()> {
    let command: Vec<&str> = command.iter().map(|s| s.as_str()).collect();
    let msg: Message = match command.as_slice() {
        ["select_workspace", workspace] => Message::SelectWorkspace(workspace.parse()?),
        ["move_to_workspace", workspace] => Message::MoveToWorkspace(workspace.parse()?),
        ["select_monitor", monitor] => Message::SelectMonitor(monitor.parse()?),
        ["move_to_monitor", monitor] => Message::MoveToMonitor(monitor.parse()?),
        _ => return Ok(()),
    };
    tx.send(msg)?;
    Ok(())
}

fn main_loop(rx: mpsc::Receiver<Message>) -> Result<()> {
    let monitor_ids = hyprland::get_monitor_ids()?;
    let mut active_workspace = hyprland::get_active_workspace()?;
    println!("Sorted monitor ids: {monitor_ids:?}");
    println!("Initial workspace: {active_workspace:?}");
    for msg in rx {
        println!("Msg: {msg:?}");
        match msg {
            Message::ActiveWorkspaceChangedID(new_id) => {
                active_workspace = Workspace::from_id(new_id);
                println!("Workspace update: {active_workspace:?}");
            }
            Message::SelectWorkspace(workspace) => {
                active_workspace.workspace = workspace;
                let workspace_id = active_workspace.to_id();
                hyprctl(&format!("dispatch workspace {workspace_id}"))?;
            }
            Message::MoveToWorkspace(workspace) => {
                let mut target_workspace = active_workspace;
                target_workspace.workspace = workspace;
                let workspace_id = target_workspace.to_id();
                hyprctl(&format!("dispatch movetoworkspacesilent {workspace_id}"))?;
            }
            Message::SelectMonitor(monitor_pos) => {
                let monitor_id = monitor_ids[monitor_pos as usize]; // NOTE: panics when called with non existent monitor
                hyprctl(&format!("dispatch focusmonitor {monitor_id}"))?;
            }
            Message::MoveToMonitor(monitor_pos) => {
                let monitor_id = monitor_ids[monitor_pos as usize]; // NOTE: panics when called with non existent monitor
                hyprctl(&format!("dispatch movewindow mon:{monitor_id} silent"))?;
            }
        }
    }
    Ok(())
}

fn get_command_socket_path() -> Result<PathBuf> {
    let xdg_runtime_dir = env::var("XDG_RUNTIME_DIR")?;
    let path = PathBuf::from(xdg_runtime_dir).join(COMMAND_SOCKET);
    Ok(path)
}

// processes incoming connections synchronously, so the clients must open connection, send command and close the connection
fn command_reader(tx: mpsc::Sender<Message>) -> Result<()> {
    let path = get_command_socket_path()?;
    let _ = fs::remove_file(&path);

    let listener = UnixListener::bind(path)?;

    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                let mut reader = BufReader::new(stream);
                let mut buf = Vec::<u8>::new();
                reader.read_to_end(&mut buf)?;
                let command: Vec<String> = bincode::deserialize(&buf)?;
                println!("Received command: {command:?}");
                process_command(command, &tx)?;
            }
            Err(_err) => {
                break;
            }
        }
    }
    Ok(())
}

pub fn send_command(command: &Vec<String>) -> Result<()> {
    let path = get_command_socket_path()?;
    let mut stream = UnixStream::connect(path)?;

    let serialized = bincode::serialize(command)?;

    stream.write_all(&serialized)?;
    stream.flush()?;

    Ok(())
}

pub fn server() -> Result<()> {
    let (tx, rx) = mpsc::channel::<Message>();

    thread::spawn({
        let tx = tx.clone();
        move || {
            if let Err(x) = hyprland::event_reader(tx) {
                eprintln!("Hyprland socket reader returned an error: {x:?}");
                exit(1);
            }
        }
    });

    thread::spawn({
        let tx = tx.clone();
        move || {
            if let Err(x) = command_reader(tx) {
                eprintln!("Hywoma socket reader returned an error: {x:?}");
                exit(2);
            }
        }
    });

    drop(tx);
    thread::spawn(move || main_loop(rx))
        .join()
        .expect("Main loop panicked")?;
    Ok(())
}
