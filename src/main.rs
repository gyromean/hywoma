use anyhow::{Result, anyhow};
use serde::Deserialize;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::process::exit;
use std::sync::mpsc;
use std::thread;
use std::{env, fs};

const HYWOMA_SOCKET: &str = ".hywoma.sock";

#[derive(Debug)]
enum Message {
    ActiveWorkspaceChangedID(u64),
    SelectWorkspace(u64),
    MoveToWorkspace(u64),
    SelectMonitor(u64),
    MoveToMonitor(u64),
}

#[derive(Debug)]
enum HyprlandSocketKind {
    Command,
    Event,
}

#[derive(Debug, Clone, Copy)]
struct Workspace {
    workspace: u64,
    monitor: u64,
    group: u64,
}

impl Workspace {
    fn from_id(mut id: u64) -> Self {
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
    fn to_id(&self) -> u64 {
        (self.workspace - 1) + 10 * (self.monitor - 1) + 100 * self.group + 1
    }
}

fn hywoma_process_command(command: Vec<String>, tx: &mpsc::Sender<Message>) -> Result<()> {
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
    let monitor_ids = get_monitor_ids()?;
    let mut active_workspace = get_active_workspace()?;
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

////////////////////////////////////////////////////////////////////////////////////////////////////////////////////////////////////////////////////////////////////////////////////////////////////////////////////////////////////////////////

// returns monitor ids sorted by their x position
fn get_monitor_ids() -> Result<Vec<u64>> {
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

// idk jestli tahle funkce ve finale dava smysl, jestli ta funkcionalita nema byt encapsulated nejak jinak
fn get_active_workspace() -> Result<Workspace> {
    let activeworkspace_json = hyprctl("-j/activeworkspace")?;
    let v: serde_json::Value = serde_json::from_str(&activeworkspace_json)?;
    let workspace_id = v["id"].as_u64().unwrap();
    Ok(Workspace::from_id(workspace_id))
}

fn get_hyprland_socket_path(kind: HyprlandSocketKind) -> Result<PathBuf> {
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

fn get_hywoma_socket_path() -> Result<PathBuf> {
    let xdg_runtime_dir = env::var("XDG_RUNTIME_DIR")?;
    let path = PathBuf::from(xdg_runtime_dir).join(HYWOMA_SOCKET);
    Ok(path)
}

fn hyprland_socket_reader(tx: mpsc::Sender<Message>) -> Result<()> {
    let path = get_hyprland_socket_path(HyprlandSocketKind::Event)?;
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

// processes incoming connections synchronously, so the clients must open connection, send command and close the connection
fn hywoma_socket_reader(tx: mpsc::Sender<Message>) -> Result<()> {
    let path = get_hywoma_socket_path()?;
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
                hywoma_process_command(command, &tx)?;
            }
            Err(_err) => {
                break;
            }
        }
    }
    Ok(())
}

fn hywoma_socket_write(command: &Vec<String>) -> Result<()> {
    let path = get_hywoma_socket_path()?;
    let mut stream = UnixStream::connect(path)?;

    let serialized = bincode::serialize(command)?;

    stream.write_all(&serialized)?;
    stream.flush()?;

    Ok(())
}

fn hyprctl(command: &str) -> Result<String> {
    let path = get_hyprland_socket_path(HyprlandSocketKind::Command)?;
    let mut stream = UnixStream::connect(path)?;

    stream.write_all(command.as_bytes())?;
    stream.flush()?;

    let mut reader = BufReader::new(stream);
    let mut response = String::new();
    reader.read_to_string(&mut response)?;

    Ok(response)
}

fn server() -> Result<()> {
    let (tx, rx) = mpsc::channel::<Message>();

    thread::spawn({
        let tx = tx.clone();
        move || {
            if let Err(x) = hyprland_socket_reader(tx) {
                eprintln!("Hyprland socket reader returned an error: {x:?}");
                exit(1);
            }
        }
    });

    thread::spawn({
        let tx = tx.clone();
        move || {
            if let Err(x) = hywoma_socket_reader(tx) {
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

fn main() -> Result<()> {
    let args: Vec<String> = env::args().skip(1).collect();
    if args.len() < 1 {
        eprintln!("Requires argument");
        return Ok(());
    }
    if args[0] == "server" {
        return server();
    }

    hywoma_socket_write(&args)?;

    Ok(())
}
