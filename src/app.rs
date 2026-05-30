use anyhow::Result;
use serde::Serialize;
use std::collections::HashSet;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::Shutdown;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::process::exit;
use std::sync::mpsc;
use std::thread;
use std::{env, fs};

use crate::hyprland;
use crate::hyprland::Workspace;
use crate::hyprland::hyprctl_dispatch as hyprctl;
use crate::state::{
    DEFAULT_GROUP_ID, DEFAULT_VISIBLE_WORKSPACE, FIRST_INTERNAL_WORKSPACE_ID, GroupId,
    PersistedState, Slot, SlotId, State, VISIBLE_WORKSPACES_PER_SLOT, VisibleWorkspace,
};

const COMMAND_SOCKET: &str = ".hywoma-commands.sock";
const EVENT_SOCKET: &str = ".hywoma-events.sock";

#[derive(Debug)]
pub enum Message {
    ActiveWorkspaceChanged {
        workspace_id: u64,
        monitor_name: Option<String>,
    },
    WorkspaceCreated {
        workspace_id: u64,
    },
    WorkspaceDestroyed {
        workspace_id: u64,
    },
    MonitorTopologyChanged,
    Status(mpsc::Sender<String>),
    TmpSlots(mpsc::Sender<String>),
    TmpSwapWithSlot(SlotId, mpsc::Sender<String>),
    SelectWorkspace(VisibleWorkspace),
    SelectWorkspaceDelta(i64),
    MoveToWorkspace(VisibleWorkspace),
    SwitchGroup(GroupId),
    CreateGroup(String),
    RenameGroup(GroupId, String),
    DeleteGroup(GroupId),
    MoveToGroup(GroupId),
    SelectSlot(u64),
    MoveToSlot(u64),
    SwapSlot(u64),
    SubscribeEvents(UnixStream),
}

#[derive(Debug, Serialize)]
struct StatusSnapshot {
    active_workspace_id: u64,
    focused_slot: SlotId,
    present_workspace_ids: Vec<u64>,
    detached_slots: Vec<SlotWorkspaceSummary>,
    state: crate::state::StateSnapshot,
}

#[derive(Debug, Clone, Serialize)]
struct SlotWorkspaceSummary {
    slot: SlotId,
    key: String,
    attached_output: Option<String>,
    workspace_count: usize,
    detached: bool,
}

fn slot_to_monitor_pos(slot: u64) -> Option<u64> {
    slot.checked_sub(1)
}

fn default_state() -> State {
    State::new(default_slots())
}

fn default_slots() -> [Slot; 3] {
    [
        Slot::new(1, "u", "left"),
        Slot::new(2, "i", "middle"),
        Slot::new(3, "o", "right"),
    ]
}

fn hostname() -> Option<String> {
    fs::read_to_string("/etc/hostname")
        .ok()
        .map(|hostname| hostname.trim().to_string())
}

fn attach_monitors_for_host(state: &mut State, monitors: &[hyprland::MonitorInfo]) {
    match hostname().as_deref() {
        Some("pavellt") => {
            // Laptop muscle memory: the built-in panel is the main/default slot on Win+i. Hotplugged
            // outputs are assigned by arrival/preservation order to Win+o first, then Win+u.
            state.attach_monitors_primary_and_hotplug(monitors, "eDP-1", 2, &[3, 1]);
        }
        Some("pavelpc") => {
            // Desktop layout is fixed physically: left/middle/right are DP-1/DP-3/DP-2. Do not let
            // hotplug order or x positions reshuffle these slots.
            state.attach_monitors_fixed_outputs(monitors, &[("DP-1", 1), ("DP-3", 2), ("DP-2", 3)]);
        }
        _ => {
            // VM/unknown hosts keep the old simple behavior until they get an explicit policy.
            state.attach_monitors_in_order(monitors);
        }
    }
}

fn runtime_state_path() -> Result<PathBuf> {
    let xdg_runtime_dir = env::var("XDG_RUNTIME_DIR")?;
    // This is intentionally under XDG_RUNTIME_DIR, not XDG_STATE_HOME. It lets the daemon survive
    // development restarts without carrying workspace groups/mappings across logout or reboot.
    Ok(PathBuf::from(xdg_runtime_dir)
        .join("hywoma")
        .join("state.json"))
}

fn load_runtime_state() -> Option<State> {
    let path = runtime_state_path().ok()?;
    let data = fs::read_to_string(&path).ok()?;
    let persisted: PersistedState = match serde_json::from_str(&data) {
        Ok(persisted) => persisted,
        Err(err) => {
            eprintln!("Ignoring invalid hywoma runtime state {path:?}: {err:?}");
            return None;
        }
    };
    // If the schema version does not match, State::from_persisted returns None and the daemon
    // starts from a clean state. That is safer than trying to interpret stale mappings.
    State::from_persisted(default_slots(), persisted)
}

fn save_runtime_state(state: &State) -> Result<()> {
    let path = runtime_state_path()?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }

    let tmp_path = path.with_extension("json.tmp");
    let data = serde_json::to_string_pretty(&state.persisted())?;
    // Write-then-rename keeps the persisted state from being truncated if the daemon is killed
    // during a write. This is mostly for development restarts, but it costs almost nothing.
    fs::write(&tmp_path, data)?;
    fs::rename(tmp_path, path)?;
    Ok(())
}

fn persist_runtime_state(state: &State) {
    if let Err(err) = save_runtime_state(state) {
        eprintln!("Failed to save hywoma runtime state: {err:?}");
    }
}

fn status_snapshot(
    active_workspace_id: u64,
    focused_slot: SlotId,
    present_workspace_ids: &HashSet<u64>,
    state: &State,
) -> StatusSnapshot {
    let mut present_workspace_id_list: Vec<u64> = present_workspace_ids.iter().copied().collect();
    present_workspace_id_list.sort_unstable();

    StatusSnapshot {
        active_workspace_id,
        focused_slot,
        present_workspace_ids: present_workspace_id_list,
        detached_slots: slot_workspace_summaries(state, present_workspace_ids)
            .into_iter()
            .filter(|summary| summary.detached)
            .collect(),
        state: state.snapshot(),
    }
}

fn slot_workspace_summaries(
    state: &State,
    present_workspace_ids: &HashSet<u64>,
) -> Vec<SlotWorkspaceSummary> {
    let snapshot = state.snapshot();
    let mut summaries: Vec<SlotWorkspaceSummary> = snapshot
        .slots
        .into_iter()
        .map(|slot| {
            let workspace_count = present_workspace_ids
                .iter()
                .filter(|workspace_id| {
                    state
                        .key_for_workspace_id(**workspace_id)
                        .is_some_and(|key| key.slot == slot.id)
                })
                .count();

            SlotWorkspaceSummary {
                slot: slot.id,
                key: slot.key,
                attached_output: slot.attached_output,
                workspace_count,
                detached: slot.runtime_monitor_id.is_none() && workspace_count > 0,
            }
        })
        .collect();
    summaries.sort_unstable_by_key(|summary| summary.slot);
    summaries
}

fn tmp_slots_response(state: &State, present_workspace_ids: &HashSet<u64>) -> String {
    slot_workspace_summaries(state, present_workspace_ids)
        .into_iter()
        .map(|summary| {
            let output = summary.attached_output.as_deref().unwrap_or("---");
            let mut line = format!(
                "slot {} ({}): {}, {} workspace(s)",
                summary.slot, summary.key, output, summary.workspace_count
            );
            if summary.detached {
                line.push_str(&format!(
                    ", detached, hint=\"hywoma tmp-swap-with-slot {}\"",
                    summary.slot
                ));
            }
            line
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn write_event_snapshot(
    stream: &mut UnixStream,
    active_workspace_id: u64,
    focused_slot: SlotId,
    present_workspace_ids: &HashSet<u64>,
    state: &State,
) -> Result<()> {
    // Event clients get the same full snapshot as `hywoma status`, but compact and newline
    // delimited. Full snapshots keep AGS simple and avoid ordering dependencies between fine
    // grained events.
    let status = status_snapshot(
        active_workspace_id,
        focused_slot,
        present_workspace_ids,
        state,
    );
    let mut response = serde_json::to_string(&status)?;
    response.push('\n');
    stream.write_all(response.as_bytes())?;
    stream.flush()?;
    Ok(())
}

fn broadcast_event_snapshot(
    subscribers: &mut Vec<UnixStream>,
    active_workspace_id: u64,
    focused_slot: SlotId,
    present_workspace_ids: &HashSet<u64>,
    state: &State,
) {
    // Broadcast is best-effort. AGS or any diagnostic client must never block workspace switching,
    // so a failed write simply removes that subscriber.
    subscribers.retain_mut(|stream| {
        match write_event_snapshot(
            stream,
            active_workspace_id,
            focused_slot,
            present_workspace_ids,
            state,
        ) {
            Ok(()) => true,
            Err(err) => {
                eprintln!("Dropping hywoma event subscriber after write failure: {err:?}");
                false
            }
        }
    });
}

fn sync_old_workspace(
    state: &mut State,
    workspace: Workspace,
    monitor_name: Option<&str>,
) -> Workspace {
    // Old encoded workspace IDs contain a monitor position, but Hyprland's event can also tell us
    // the real output name. Prefer the output name when available so events from a hotplugged
    // HEADLESS-* output map back to the correct logical slot instead of whatever monitor number the
    // old encoding happened to contain.
    let slot = monitor_name
        .and_then(|name| state.slot_for_output_name(name))
        .unwrap_or(workspace.monitor);
    let workspace = Workspace {
        monitor: slot,
        ..workspace
    };
    state.ensure_group(workspace.group, format!("Group {}", workspace.group));
    if workspace.group == state.active_group {
        state.set_active_visible(workspace.monitor, workspace.workspace);
    }
    workspace
}

fn sync_active_workspace_id(
    state: &mut State,
    active_workspace: &mut Option<Workspace>,
    focused_slot: &mut SlotId,
    workspace_id: u64,
    monitor_name: Option<&str>,
) {
    // Opaque IDs identify the slot and visible label for Hyprland events, but Hyprland events are
    // not allowed to change the global active group. Hotplug can report a stale workspace from an
    // old group on the reattached monitor; group changes must come from hywoma commands only.
    if let Some(key) = state.key_for_workspace_id(workspace_id) {
        *active_workspace = None;
        *focused_slot = key.slot;
        if key.group == state.active_group {
            state.set_active_visible(key.slot, key.visible);
        }
        println!("Workspace update: opaque {key:?}, id {workspace_id}");
    } else {
        // Temporary compatibility path for old encoded workspace IDs. This is kept while the config
        // still has fallback binds and while external/manual workspace changes can produce old IDs.
        let workspace = sync_old_workspace(state, Workspace::from_id(workspace_id), monitor_name);
        *active_workspace = Some(workspace);
        *focused_slot = workspace.monitor;
        println!("Workspace update: {workspace:?}");
    }
}

fn select_workspace(
    state: &mut State,
    focused_slot: SlotId,
    visible: VisibleWorkspace,
) -> Result<u64> {
    // Return the target ID so the main loop can update active_workspace_id before Hyprland's async
    // event arrives. Without this optimistic update AGS can briefly render a new workspace with the
    // previous active highlight.
    let workspace_id = state.select_workspace(focused_slot, visible);
    hyprctl(&format!("dispatch workspace {workspace_id}"))?;
    Ok(workspace_id)
}

fn select_workspace_delta(
    state: &mut State,
    present_workspace_ids: &HashSet<u64>,
    focused_slot: SlotId,
    delta: i64,
) -> Result<Option<u64>> {
    if delta == 0 {
        return Ok(None);
    }

    let mut target = state.active_visible(focused_slot);
    loop {
        let Some(next_target) = target.checked_add_signed(delta) else {
            eprintln!("Cannot select workspace {target} + {delta}: out of visible range");
            return Ok(None);
        };
        if !(1..=VISIBLE_WORKSPACES_PER_SLOT).contains(&next_target) {
            eprintln!(
                "Cannot select workspace {next_target}: visible workspaces are 1..={VISIBLE_WORKSPACES_PER_SLOT}"
            );
            return Ok(None);
        }
        target = next_target;

        if let Some(workspace_id) =
            state.known_workspace_id(state.active_group, focused_slot, target)
            && present_workspace_ids.contains(&workspace_id)
        {
            return select_workspace(state, focused_slot, target).map(Some);
        }
    }
}

fn move_to_workspace(
    state: &mut State,
    focused_slot: SlotId,
    visible: VisibleWorkspace,
) -> Result<()> {
    let workspace_id = state.workspace_id_for(state.active_group, focused_slot, visible);
    hyprctl(&format!("dispatch movetoworkspacesilent {workspace_id}"))?;
    Ok(())
}

fn move_to_slot(state: &mut State, slot: SlotId) -> Result<()> {
    // Detached slots are intentionally not merged into any attached slot. If a monitor disappears,
    // the logical slot remains addressable but commands that need a real monitor become no-ops.
    if state.runtime_monitor_id_for_slot(slot).is_none() {
        eprintln!("Cannot move window to detached slot {slot}");
        return Ok(());
    }

    let visible = state.active_visible(slot);
    let workspace_id = state.workspace_id_for(state.active_group, slot, visible);
    hyprctl(&format!("dispatch movetoworkspacesilent {workspace_id}"))?;
    Ok(())
}

fn select_slot(state: &mut State, slot: SlotId) -> Result<Option<u64>> {
    let Some(monitor_id) = state.runtime_monitor_id_for_slot(slot) else {
        eprintln!("Cannot select detached slot {slot}");
        return Ok(None);
    };

    let visible = state.active_visible(slot);
    let workspace_id = state.workspace_id_for(state.active_group, slot, visible);
    hyprctl(&format!("dispatch focusmonitor {monitor_id}"))?;
    hyprctl(&format!("dispatch workspace {workspace_id}"))?;
    Ok(Some(workspace_id))
}

fn swap_slot(state: &mut State, source_slot: SlotId, target_slot: SlotId) -> Result<()> {
    if source_slot == target_slot {
        println!("Skipping swap of slot {source_slot} with itself");
        return Ok(());
    }

    let Some(source_monitor_id) = state.runtime_monitor_id_for_slot(source_slot) else {
        eprintln!("Cannot swap from detached slot {source_slot}");
        return Ok(());
    };
    let Some(target_monitor_id) = state.runtime_monitor_id_for_slot(target_slot) else {
        eprintln!("Cannot swap with detached slot {target_slot}");
        return Ok(());
    };

    let source_visible = state.active_visible(source_slot);
    let target_visible = state.active_visible(target_slot);
    // Only swap mappings that have actually been displayed/allocated. That prevents a swap from
    // silently inventing a hidden workspace and then claiming Hyprland swapped it.
    let Some(source_workspace_id) =
        state.known_workspace_id(state.active_group, source_slot, source_visible)
    else {
        eprintln!(
            "Cannot swap slot {source_slot} visible {source_visible}: opaque workspace is not displayed yet"
        );
        return Ok(());
    };
    let Some(target_workspace_id) =
        state.known_workspace_id(state.active_group, target_slot, target_visible)
    else {
        eprintln!(
            "Cannot swap slot {target_slot} visible {target_visible}: opaque workspace is not displayed yet"
        );
        return Ok(());
    };

    hyprctl(&format!(
        "dispatch swapactiveworkspaces {source_monitor_id} {target_monitor_id}"
    ))?;
    // Hyprland swaps monitor contents. To keep visible labels pinned to logical slots, hywoma swaps
    // the internal IDs underneath those labels only after Hyprland accepted the dispatch.
    let swapped_ids = state.swap_active_workspace_ids(source_slot, target_slot);
    debug_assert_eq!(swapped_ids, (source_workspace_id, target_workspace_id));
    println!(
        "Swapped state mapping: slot {source_slot} visible {source_visible} workspace {source_workspace_id} monitor {source_monitor_id} <-> slot {target_slot} visible {target_visible} workspace {target_workspace_id} monitor {target_monitor_id}"
    );
    Ok(())
}

fn tmp_swap_with_slot(
    state: &mut State,
    present_workspace_ids: &mut HashSet<u64>,
    focused_slot: SlotId,
    target_slot: SlotId,
    active_workspace_id: &mut u64,
) -> Result<String> {
    if focused_slot == target_slot {
        return Ok(format!("Skipping swap of slot {focused_slot} with itself"));
    }

    let Some(source_monitor_id) = state.runtime_monitor_id_for_slot(focused_slot) else {
        return Ok(format!("Cannot tmp-swap from detached slot {focused_slot}"));
    };
    if !state
        .snapshot()
        .slots
        .iter()
        .any(|slot| slot.id == target_slot)
    {
        return Ok(format!("Cannot tmp-swap with unknown slot {target_slot}"));
    }
    if state.runtime_monitor_id_for_slot(target_slot).is_some() {
        return Ok(format!(
            "Cannot tmp-swap with attached slot {target_slot}; use hywoma swap_slot {target_slot} instead"
        ));
    }

    let target_present_in_active_group = present_workspace_ids
        .iter()
        .filter_map(|workspace_id| {
            state.key_for_workspace_id(*workspace_id).and_then(|key| {
                (key.group == state.active_group && key.slot == target_slot)
                    .then_some((key.visible, *workspace_id))
            })
        })
        .min_by_key(|(visible, _)| *visible);

    state.swap_slot_workspace_mappings(focused_slot, target_slot);
    hyprctl(&format!("dispatch focusmonitor {source_monitor_id}"))?;

    if let Some((visible, workspace_id)) = target_present_in_active_group {
        state.set_active_visible(focused_slot, visible);
        hyprctl(&format!("dispatch workspace {workspace_id}"))?;
        *active_workspace_id = workspace_id;
        present_workspace_ids.insert(workspace_id);
    } else {
        *active_workspace_id = select_workspace(state, focused_slot, DEFAULT_VISIBLE_WORKSPACE)?;
        present_workspace_ids.insert(*active_workspace_id);
    }

    let source_key = state.slot_key(focused_slot).unwrap_or("?");
    let target_key = state.slot_key(target_slot).unwrap_or("?");
    Ok(format!(
        "Swapped current slot {focused_slot} ({source_key}) with slot {target_slot} ({target_key}).\nTo go back, focus slot {focused_slot} ({source_key}) and run: hywoma tmp-swap-with-slot {target_slot}"
    ))
}

fn switch_group(state: &mut State, focused_slot: SlotId, group: GroupId) -> Result<Option<u64>> {
    if !state.has_group(group) {
        eprintln!("Cannot switch to unknown workspace group {group}");
        return Ok(None);
    }

    state.switch_group(group);

    let mut slots: Vec<(SlotId, Option<u64>)> = state
        .snapshot()
        .slots
        .iter()
        .map(|slot| (slot.id, slot.runtime_monitor_id))
        .collect();
    // Focus the previously focused slot last. On multi-monitor setups that keeps keyboard focus on
    // the user's current logical slot after all attached monitors have been moved into the group.
    slots.sort_unstable_by_key(|(slot, _)| (*slot == focused_slot, *slot));

    let mut focused_workspace_id = None;
    for (slot, monitor_id) in slots {
        if let Some(monitor_id) = monitor_id {
            let visible = state.active_visible(slot);
            let workspace_id = state.workspace_id_for(group, slot, visible);
            if slot == focused_slot {
                focused_workspace_id = Some(workspace_id);
            }
            hyprctl(&format!("dispatch focusmonitor {monitor_id}"))?;
            hyprctl(&format!("dispatch workspace {workspace_id}"))?;
        }
    }

    Ok(focused_workspace_id)
}

fn sync_attached_slots_to_active_group(
    state: &mut State,
    focused_slot: SlotId,
) -> Result<Option<u64>> {
    let mut slots: Vec<(SlotId, Option<u64>)> = state
        .snapshot()
        .slots
        .iter()
        .map(|slot| (slot.id, slot.runtime_monitor_id))
        .collect();
    // Re-attached monitors can still be showing an old group's workspace. Move every attached slot
    // into the daemon's active group, then focus the user's current slot last.
    slots.sort_unstable_by_key(|(slot, _)| (*slot == focused_slot, *slot));

    let mut focused_workspace_id = None;
    for (slot, monitor_id) in slots {
        if let Some(monitor_id) = monitor_id {
            let visible = state.active_visible(slot);
            let workspace_id = state.workspace_id_for(state.active_group, slot, visible);
            if slot == focused_slot {
                focused_workspace_id = Some(workspace_id);
            }
            hyprctl(&format!("dispatch focusmonitor {monitor_id}"))?;
            hyprctl(&format!("dispatch workspace {workspace_id}"))?;
        }
    }

    Ok(focused_workspace_id)
}

fn group_has_present_workspaces(
    state: &State,
    present_workspace_ids: &HashSet<u64>,
    group: GroupId,
) -> bool {
    // Groups may contain persisted mappings for empty workspaces. Deletion is blocked only by
    // present Hyprland workspaces, not by stale/empty mappings kept for stable IDs.
    present_workspace_ids.iter().any(|workspace_id| {
        state
            .key_for_workspace_id(*workspace_id)
            .is_some_and(|key| key.group == group)
    })
}

fn delete_group(state: &mut State, present_workspace_ids: &HashSet<u64>, group: GroupId) -> bool {
    if group == state.active_group {
        eprintln!("Cannot delete the active workspace group {group}");
        return false;
    }
    if !state.has_group(group) {
        eprintln!("Cannot delete unknown workspace group {group}");
        return false;
    }
    if group_has_present_workspaces(state, present_workspace_ids, group) {
        eprintln!("Cannot delete non-empty workspace group {group}");
        return false;
    }

    state.delete_group(group);
    true
}

fn move_to_group(state: &mut State, focused_slot: SlotId, group: GroupId) -> Result<()> {
    if !state.has_group(group) {
        eprintln!("Cannot move window to unknown workspace group {group}");
        return Ok(());
    }

    let visible = state.active_visible_in_group(group, focused_slot);
    // Move to the destination group's active visible workspace on the same logical slot. This keeps
    // the old behavior where a window moves to the corresponding monitor/slot in another group.
    let workspace_id = state.workspace_id_for(group, focused_slot, visible);
    hyprctl(&format!("dispatch movetoworkspacesilent {workspace_id}"))?;
    Ok(())
}

fn process_command(command: Vec<String>, tx: &mpsc::Sender<Message>) -> Result<()> {
    if command.first().map(|cmd| cmd.as_str()) == Some("create_group") && command.len() > 1 {
        tx.send(Message::CreateGroup(command[1..].join(" ")))?;
        return Ok(());
    }
    if command.first().map(|cmd| cmd.as_str()) == Some("rename_group") && command.len() > 2 {
        tx.send(Message::RenameGroup(
            command[1].parse()?,
            command[2..].join(" "),
        ))?;
        return Ok(());
    }

    let command: Vec<&str> = command.iter().map(|s| s.as_str()).collect();
    let msg: Message = match command.as_slice() {
        ["select_workspace", workspace] => Message::SelectWorkspace(workspace.parse()?),
        ["select_workspace_delta", delta] => Message::SelectWorkspaceDelta(delta.parse()?),
        ["move_to_workspace", workspace] => Message::MoveToWorkspace(workspace.parse()?),
        ["switch_group", group] => Message::SwitchGroup(group.parse()?),
        ["delete_group", group] => Message::DeleteGroup(group.parse()?),
        ["move_to_group", group] => Message::MoveToGroup(group.parse()?),
        ["select_slot", slot] => Message::SelectSlot(slot.parse()?),
        ["move_to_slot", slot] => Message::MoveToSlot(slot.parse()?),
        ["swap_slot", slot] => Message::SwapSlot(slot.parse()?),
        _ => return Ok(()),
    };
    tx.send(msg)?;
    Ok(())
}

fn is_status_command(command: &[String]) -> bool {
    matches!(command, [cmd] if cmd == "status")
}

fn is_response_command(command: &[String]) -> bool {
    matches!(command, [cmd] if cmd == "status" || cmd == "tmp-slots")
        || matches!(command, [cmd, _] if cmd == "tmp-swap-with-slot")
}

fn write_status_response(mut stream: UnixStream, response: &str) {
    if let Err(err) = stream.write_all(response.as_bytes()) {
        eprintln!("Failed to write status response: {err:?}");
        return;
    }
    if let Err(err) = stream.write_all(b"\n") {
        eprintln!("Failed to terminate status response: {err:?}");
        return;
    }
    if let Err(err) = stream.flush() {
        eprintln!("Failed to flush status response: {err:?}");
    }
}

fn main_loop(rx: mpsc::Receiver<Message>) -> Result<()> {
    let mut monitors = hyprland::get_monitors()?;
    let initial_workspace_id = hyprland::get_active_workspace_id()?;
    let initial_monitor_id = hyprland::get_active_workspace_monitor_id()?;
    let initial_workspace = Workspace::from_id(initial_workspace_id);
    let mut active_workspace_id = initial_workspace_id;
    let mut active_workspace = Some(initial_workspace);
    let mut focused_slot = initial_workspace.monitor;
    let mut present_workspace_ids: HashSet<u64> =
        hyprland::get_workspace_ids()?.into_iter().collect();
    present_workspace_ids.insert(active_workspace_id);
    let runtime_state = load_runtime_state();
    let loaded_runtime_state = runtime_state.is_some();
    let mut state = runtime_state.unwrap_or_else(default_state);
    let mut event_subscribers = Vec::new();
    attach_monitors_for_host(&mut state, &monitors);
    if let Some(key) = state.key_for_workspace_id(initial_workspace_id) {
        // Normal daemon restart path: the runtime state tells us what the active opaque ID means,
        // so recover group/slot/visible from the persisted mapping instead of unpacking the ID as an
        // old encoded workspace. If the slot policy changed since the state was written, the
        // persisted slot can now be detached; then prefer Hyprland's active monitor -> slot mapping.
        focused_slot = initial_monitor_id
            .and_then(|monitor_id| state.slot_for_monitor_id(monitor_id))
            .filter(|slot| state.runtime_monitor_id_for_slot(*slot).is_some())
            .unwrap_or(key.slot);
        active_workspace = None;
        state.restore_active_group(key.group);
        state.set_workspace_id(key.group, focused_slot, key.visible, initial_workspace_id);
        state.set_active_visible(focused_slot, key.visible);
    } else if initial_workspace_id >= FIRST_INTERNAL_WORKSPACE_ID {
        // Clean-session bootstrap path: Hyprland can be configured to start monitors on hywoma's
        // seeded opaque IDs before the daemon has allocated any runtime mappings. Recognize those
        // IDs as the default group mapping; runtime state and swaps can still overwrite them later.
        active_workspace = None;
        if let Some(key) = state.default_key_for_workspace_id(initial_workspace_id) {
            focused_slot = initial_monitor_id
                .and_then(|monitor_id| state.slot_for_monitor_id(monitor_id))
                .unwrap_or(key.slot);
            state.restore_active_group(key.group);
            state.set_workspace_id(key.group, focused_slot, key.visible, initial_workspace_id);
            state.set_active_visible(focused_slot, key.visible);
        } else {
            // Development fallback for a restart on opaque workspaces without a runtime state file.
            // The mapping cannot be recovered perfectly from Hyprland alone, so present opaque IDs are
            // mapped to visible labels in sorted order. This is only best-effort recovery.
            focused_slot = initial_monitor_id
                .and_then(|monitor_id| state.slot_for_monitor_id(monitor_id))
                .unwrap_or(1);
            let mut opaque_workspace_ids: Vec<u64> = present_workspace_ids
                .iter()
                .copied()
                .filter(|id| *id >= FIRST_INTERNAL_WORKSPACE_ID)
                .collect();
            opaque_workspace_ids.sort_unstable();

            for (index, workspace_id) in opaque_workspace_ids.into_iter().enumerate() {
                state.set_workspace_id(
                    DEFAULT_GROUP_ID,
                    focused_slot,
                    index as u64 + DEFAULT_VISIBLE_WORKSPACE,
                    workspace_id,
                );
            }
            state.switch_group(DEFAULT_GROUP_ID);
            if let Some(key) = state.key_for_workspace_id(initial_workspace_id) {
                state.set_active_visible(focused_slot, key.visible);
            } else {
                state.set_active_visible(focused_slot, DEFAULT_VISIBLE_WORKSPACE);
            }
        }
    } else {
        // Legacy startup path for old encoded IDs. Keep this until the old fallback binds and manual
        // old-ID workflows are removed.
        state.ensure_group(
            initial_workspace.group,
            format!("Group {}", initial_workspace.group),
        );
        state.switch_group(initial_workspace.group);
        state.set_active_visible(initial_workspace.monitor, initial_workspace.workspace);
    }
    if loaded_runtime_state {
        println!("Loaded hywoma runtime state");
    }
    if let Some(workspace_id) = sync_attached_slots_to_active_group(&mut state, focused_slot)? {
        active_workspace_id = workspace_id;
        active_workspace = None;
        present_workspace_ids.insert(active_workspace_id);
    }
    persist_runtime_state(&state);
    println!("Sorted monitors: {monitors:?}");
    println!("Initial workspace: {initial_workspace:?}");
    for msg in rx {
        println!("Msg: {msg:?}");
        let mut should_broadcast = false;
        let mut should_persist = false;
        match msg {
            Message::ActiveWorkspaceChanged {
                workspace_id,
                monitor_name,
            } => {
                active_workspace_id = workspace_id;
                present_workspace_ids.insert(workspace_id);
                sync_active_workspace_id(
                    &mut state,
                    &mut active_workspace,
                    &mut focused_slot,
                    workspace_id,
                    monitor_name.as_deref(),
                );
                should_broadcast = true;
                should_persist = true;
            }
            Message::WorkspaceCreated { workspace_id } => {
                if present_workspace_ids.insert(workspace_id) {
                    should_broadcast = true;
                }
            }
            Message::WorkspaceDestroyed { workspace_id } => {
                // Hyprland can destroy the workspace that just disappeared from a removed monitor.
                // Do not remove the active ID until topology reconciliation has read Hyprland's real
                // active workspace, otherwise AGS can briefly lose the active indicator.
                if workspace_id != active_workspace_id
                    && present_workspace_ids.remove(&workspace_id)
                {
                    should_broadcast = true;
                }
            }
            Message::MonitorTopologyChanged => {
                let previous_active_group = state.active_group;
                let previous_focused_slot = focused_slot;
                monitors = hyprland::get_monitors()?;
                attach_monitors_for_host(&mut state, &monitors);
                // Monitor removal can emit transitional old workspace IDs such as `1` before the
                // final active opaque workspace event arrives. Re-read Hyprland's current active
                // workspace and present workspace list here to recover from those transient events.
                present_workspace_ids = hyprland::get_workspace_ids()?.into_iter().collect();
                active_workspace_id = hyprland::get_active_workspace_id()?;
                present_workspace_ids.insert(active_workspace_id);
                sync_active_workspace_id(
                    &mut state,
                    &mut active_workspace,
                    &mut focused_slot,
                    active_workspace_id,
                    None,
                );
                if state.has_group(previous_active_group) {
                    state.restore_active_group(previous_active_group);
                }
                if state
                    .runtime_monitor_id_for_slot(previous_focused_slot)
                    .is_some()
                {
                    focused_slot = previous_focused_slot;
                }
                if let Some(workspace_id) =
                    sync_attached_slots_to_active_group(&mut state, focused_slot)?
                {
                    active_workspace_id = workspace_id;
                    active_workspace = None;
                    present_workspace_ids.insert(active_workspace_id);
                }
                println!("Monitor topology update, sorted monitors: {monitors:?}");
                should_broadcast = true;
                should_persist = true;
            }
            Message::Status(response_tx) => {
                let status = status_snapshot(
                    active_workspace_id,
                    focused_slot,
                    &present_workspace_ids,
                    &state,
                );
                let response = serde_json::to_string_pretty(&status)?;
                let _ = response_tx.send(response);
            }
            Message::TmpSlots(response_tx) => {
                let _ = response_tx.send(tmp_slots_response(&state, &present_workspace_ids));
            }
            Message::TmpSwapWithSlot(slot, response_tx) => {
                let response = if slot_to_monitor_pos(slot).is_some() {
                    tmp_swap_with_slot(
                        &mut state,
                        &mut present_workspace_ids,
                        focused_slot,
                        slot,
                        &mut active_workspace_id,
                    )?
                } else {
                    format!("Slot numbers start at 1, got {slot}")
                };
                active_workspace = None;
                should_broadcast = true;
                should_persist = true;
                let _ = response_tx.send(response);
            }
            Message::SelectWorkspace(workspace) => {
                active_workspace_id = select_workspace(&mut state, focused_slot, workspace)?;
                active_workspace = None;
                present_workspace_ids.insert(active_workspace_id);
                should_broadcast = true;
                should_persist = true;
            }
            Message::SelectWorkspaceDelta(delta) => {
                if let Some(workspace_id) =
                    select_workspace_delta(&mut state, &present_workspace_ids, focused_slot, delta)?
                {
                    active_workspace_id = workspace_id;
                    active_workspace = None;
                    present_workspace_ids.insert(active_workspace_id);
                    should_broadcast = true;
                    should_persist = true;
                }
            }
            Message::MoveToWorkspace(workspace) => {
                move_to_workspace(&mut state, focused_slot, workspace)?;
                should_persist = true;
            }
            Message::SwitchGroup(group) => {
                if let Some(workspace_id) = switch_group(&mut state, focused_slot, group)? {
                    active_workspace_id = workspace_id;
                    active_workspace = None;
                    present_workspace_ids.insert(active_workspace_id);
                }
                should_broadcast = true;
                should_persist = true;
            }
            Message::CreateGroup(name) => {
                let group = state.create_group(name);
                if let Some(workspace_id) = switch_group(&mut state, focused_slot, group)? {
                    active_workspace_id = workspace_id;
                    active_workspace = None;
                    present_workspace_ids.insert(active_workspace_id);
                }
                should_broadcast = true;
                should_persist = true;
            }
            Message::RenameGroup(group, name) => {
                if state.has_group(group) {
                    state.rename_group(group, name);
                    should_broadcast = true;
                    should_persist = true;
                } else {
                    eprintln!("Cannot rename unknown workspace group {group}");
                }
            }
            Message::DeleteGroup(group) => {
                should_broadcast = delete_group(&mut state, &present_workspace_ids, group);
                should_persist = should_broadcast;
            }
            Message::MoveToGroup(group) => {
                move_to_group(&mut state, focused_slot, group)?;
                should_persist = true;
            }
            Message::SelectSlot(slot) => {
                if slot_to_monitor_pos(slot).is_some() {
                    if let Some(workspace_id) = select_slot(&mut state, slot)? {
                        focused_slot = slot;
                        active_workspace_id = workspace_id;
                        active_workspace = None;
                        present_workspace_ids.insert(active_workspace_id);
                        should_broadcast = true;
                        should_persist = true;
                    }
                } else {
                    eprintln!("Slot numbers start at 1, got {slot}");
                }
            }
            Message::MoveToSlot(slot) => {
                if slot_to_monitor_pos(slot).is_some() {
                    move_to_slot(&mut state, slot)?;
                    should_persist = true;
                } else {
                    eprintln!("Slot numbers start at 1, got {slot}");
                }
            }
            Message::SwapSlot(slot) => {
                if slot_to_monitor_pos(slot).is_some() {
                    swap_slot(&mut state, focused_slot, slot)?;
                    should_broadcast = true;
                    should_persist = true;
                } else {
                    eprintln!("Slot numbers start at 1, got {slot}");
                }
            }
            Message::SubscribeEvents(mut stream) => {
                stream.set_nonblocking(true)?;
                // Subscribers receive an initial snapshot immediately, so AGS can start with a
                // correct bar before any future Hyprland event happens.
                if let Err(err) = write_event_snapshot(
                    &mut stream,
                    active_workspace_id,
                    focused_slot,
                    &present_workspace_ids,
                    &state,
                ) {
                    eprintln!("Failed to write initial hywoma event snapshot: {err:?}");
                } else {
                    event_subscribers.push(stream);
                }
            }
        }
        if should_persist {
            // Persist after state mutations, not after pure present-workspace changes. Present IDs are
            // runtime Hyprland state and are recomputed on startup.
            persist_runtime_state(&state);
        }
        if should_broadcast {
            broadcast_event_snapshot(
                &mut event_subscribers,
                active_workspace_id,
                focused_slot,
                &present_workspace_ids,
                &state,
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::slot_to_monitor_pos;

    #[test]
    fn slot_to_monitor_position_is_one_based() {
        assert_eq!(slot_to_monitor_pos(1), Some(0));
        assert_eq!(slot_to_monitor_pos(3), Some(2));
    }

    #[test]
    fn slot_zero_is_invalid() {
        assert_eq!(slot_to_monitor_pos(0), None);
    }
}

fn get_command_socket_path() -> Result<PathBuf> {
    let xdg_runtime_dir = env::var("XDG_RUNTIME_DIR")?;
    let path = PathBuf::from(xdg_runtime_dir).join(COMMAND_SOCKET);
    Ok(path)
}

fn get_event_socket_path() -> Result<PathBuf> {
    let xdg_runtime_dir = env::var("XDG_RUNTIME_DIR")?;
    let path = PathBuf::from(xdg_runtime_dir).join(EVENT_SOCKET);
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
                if is_status_command(&command) {
                    let (response_tx, response_rx) = mpsc::channel();
                    tx.send(Message::Status(response_tx))?;
                    let response = response_rx.recv()?;
                    write_status_response(reader.into_inner(), &response);
                } else if matches!(command.as_slice(), [cmd] if cmd == "tmp-slots") {
                    let (response_tx, response_rx) = mpsc::channel();
                    tx.send(Message::TmpSlots(response_tx))?;
                    let response = response_rx.recv()?;
                    write_status_response(reader.into_inner(), &response);
                } else if matches!(command.as_slice(), [cmd, _] if cmd == "tmp-swap-with-slot") {
                    let Ok(slot) = command[1].parse() else {
                        write_status_response(
                            reader.into_inner(),
                            &format!("Invalid slot number: {}", command[1]),
                        );
                        continue;
                    };
                    let (response_tx, response_rx) = mpsc::channel();
                    tx.send(Message::TmpSwapWithSlot(slot, response_tx))?;
                    let response = response_rx.recv()?;
                    write_status_response(reader.into_inner(), &response);
                } else {
                    process_command(command, &tx)?;
                }
            }
            Err(_err) => {
                break;
            }
        }
    }
    Ok(())
}

fn event_reader(tx: mpsc::Sender<Message>) -> Result<()> {
    let path = get_event_socket_path()?;
    let _ = fs::remove_file(&path);

    let listener = UnixListener::bind(path)?;

    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                tx.send(Message::SubscribeEvents(stream))?;
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
    stream.shutdown(Shutdown::Write)?;

    if is_response_command(command) {
        let mut response = String::new();
        stream.read_to_string(&mut response)?;
        print!("{response}");
        return Ok(());
    }

    println!("Sent command to server: {command:?}");

    Ok(())
}

pub fn stream_events() -> Result<()> {
    let path = get_event_socket_path()?;
    let stream = UnixStream::connect(path)?;
    let mut reader = BufReader::new(stream);
    let mut stdout = std::io::stdout().lock();
    let mut line = Vec::new();

    loop {
        line.clear();
        let bytes_read = reader.read_until(b'\n', &mut line)?;
        if bytes_read == 0 {
            break;
        }

        stdout.write_all(&line)?;
        stdout.flush()?;
    }

    Ok(())
}

pub fn server() -> Result<()> {
    println!("Server started");
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
                eprintln!("Hywoma command socket reader returned an error: {x:?}");
                exit(2);
            }
        }
    });

    thread::spawn({
        let tx = tx.clone();
        move || {
            if let Err(x) = event_reader(tx) {
                eprintln!("Hywoma event socket reader returned an error: {x:?}");
                exit(3);
            }
        }
    });

    drop(tx);
    thread::spawn(move || main_loop(rx))
        .join()
        .expect("Main loop panicked")?;
    Ok(())
}
