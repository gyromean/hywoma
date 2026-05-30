use std::collections::HashMap;

use serde::{Deserialize, Serialize};

pub type GroupId = u64;
pub type SlotId = u64;
pub type VisibleWorkspace = u64;
pub type InternalWorkspaceId = u64;

pub const DEFAULT_GROUP_ID: GroupId = 0;
pub const DEFAULT_GROUP_NAME: &str = "Main";
pub const DEFAULT_VISIBLE_WORKSPACE: VisibleWorkspace = 1;
pub const FIRST_INTERNAL_WORKSPACE_ID: InternalWorkspaceId = 1000;
pub const VISIBLE_WORKSPACES_PER_SLOT: u64 = 10;
pub const PERSISTED_STATE_VERSION: u64 = 2;

// Logical identity for a visible workspace. This must stay separate from Hyprland's workspace ID so
// a visible label can remain attached to a slot while `swapactiveworkspaces` swaps the internal IDs
// underneath it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct WorkspaceKey {
    pub group: GroupId,
    pub slot: SlotId,
    pub visible: VisibleWorkspace,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct WorkspaceEntry {
    pub group: GroupId,
    pub slot: SlotId,
    pub visible: VisibleWorkspace,
    pub internal_id: InternalWorkspaceId,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct GroupSnapshot {
    pub id: GroupId,
    pub name: String,
    pub active_visible_by_slot: Vec<(SlotId, VisibleWorkspace)>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SlotSnapshot {
    pub id: SlotId,
    pub key: String,
    pub label: String,
    pub attached_output: Option<String>,
    pub runtime_monitor_id: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct StateSnapshot {
    pub active_group: GroupId,
    pub previous_group: Option<GroupId>,
    pub groups: Vec<GroupSnapshot>,
    pub slots: Vec<SlotSnapshot>,
    pub workspaces: Vec<WorkspaceEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PersistedGroup {
    pub id: GroupId,
    pub name: String,
    pub active_visible_by_slot: Vec<(SlotId, VisibleWorkspace)>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PersistedWorkspaceEntry {
    pub group: GroupId,
    pub slot: SlotId,
    pub visible: VisibleWorkspace,
    pub internal_id: InternalWorkspaceId,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PersistedState {
    pub version: u64,
    pub active_group: GroupId,
    pub previous_group: Option<GroupId>,
    pub groups: Vec<PersistedGroup>,
    pub workspaces: Vec<PersistedWorkspaceEntry>,
    pub next_workspace_id: InternalWorkspaceId,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Group {
    pub id: GroupId,
    pub name: String,
    active_visible_by_slot: HashMap<SlotId, VisibleWorkspace>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Slot {
    pub id: SlotId,
    pub key: String,
    pub label: String,
    pub attached_output: Option<String>,
    pub runtime_monitor_id: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct State {
    pub active_group: GroupId,
    pub previous_group: Option<GroupId>,
    pub groups: HashMap<GroupId, Group>,
    pub slots: HashMap<SlotId, Slot>,
    workspace_ids: HashMap<WorkspaceKey, InternalWorkspaceId>,
    next_workspace_id: InternalWorkspaceId,
}

impl Group {
    pub fn new(id: GroupId, name: impl Into<String>, slots: impl Iterator<Item = SlotId>) -> Self {
        let mut active_visible_by_slot = HashMap::new();
        for slot in slots {
            active_visible_by_slot.insert(slot, DEFAULT_VISIBLE_WORKSPACE);
        }

        Group {
            id,
            name: name.into(),
            active_visible_by_slot,
        }
    }

    pub fn active_visible(&self, slot: SlotId) -> VisibleWorkspace {
        self.active_visible_by_slot
            .get(&slot)
            .copied()
            .unwrap_or(DEFAULT_VISIBLE_WORKSPACE)
    }

    pub fn set_active_visible(&mut self, slot: SlotId, visible: VisibleWorkspace) {
        self.active_visible_by_slot.insert(slot, visible);
    }
}

impl Slot {
    pub fn new(id: SlotId, key: impl Into<String>, label: impl Into<String>) -> Self {
        Slot {
            id,
            key: key.into(),
            label: label.into(),
            attached_output: None,
            runtime_monitor_id: None,
        }
    }
}

impl State {
    pub fn new(slots: impl IntoIterator<Item = Slot>) -> Self {
        let slots: HashMap<SlotId, Slot> = slots.into_iter().map(|slot| (slot.id, slot)).collect();
        let mut groups = HashMap::new();
        groups.insert(
            DEFAULT_GROUP_ID,
            Group::new(DEFAULT_GROUP_ID, DEFAULT_GROUP_NAME, slots.keys().copied()),
        );

        let next_workspace_id =
            FIRST_INTERNAL_WORKSPACE_ID + slots.len() as u64 * VISIBLE_WORKSPACES_PER_SLOT;

        State {
            active_group: DEFAULT_GROUP_ID,
            previous_group: None,
            groups,
            slots,
            workspace_ids: HashMap::new(),
            next_workspace_id,
        }
    }

    pub fn from_persisted(
        slots: impl IntoIterator<Item = Slot>,
        persisted: PersistedState,
    ) -> Option<Self> {
        // Runtime state is deliberately versioned. If the schema changes, starting clean is safer
        // than applying stale workspace mappings to the wrong groups or slots.
        if persisted.version != PERSISTED_STATE_VERSION {
            return None;
        }

        let slots: HashMap<SlotId, Slot> = slots.into_iter().map(|slot| (slot.id, slot)).collect();
        let slot_ids: Vec<SlotId> = slots.keys().copied().collect();
        let mut groups: HashMap<GroupId, Group> = persisted
            .groups
            .into_iter()
            .map(|group| {
                let mut active_visible_by_slot: HashMap<SlotId, VisibleWorkspace> =
                    group.active_visible_by_slot.into_iter().collect();
                for slot_id in &slot_ids {
                    // Slot definitions are code/config, not persisted state. When a new slot is
                    // added, old runtime state should still load and get a default active visible.
                    active_visible_by_slot
                        .entry(*slot_id)
                        .or_insert(DEFAULT_VISIBLE_WORKSPACE);
                }
                (
                    group.id,
                    Group {
                        id: group.id,
                        name: group.name,
                        active_visible_by_slot,
                    },
                )
            })
            .collect();

        // Always keep Main around. A corrupted or hand-edited runtime file should not leave hywoma
        // without a fallback group.
        if groups.is_empty() || !groups.contains_key(&DEFAULT_GROUP_ID) {
            groups.insert(
                DEFAULT_GROUP_ID,
                Group::new(
                    DEFAULT_GROUP_ID,
                    DEFAULT_GROUP_NAME,
                    slot_ids.iter().copied(),
                ),
            );
        }

        let active_group = if groups.contains_key(&persisted.active_group) {
            persisted.active_group
        } else {
            DEFAULT_GROUP_ID
        };
        let previous_group = persisted
            .previous_group
            .filter(|group| groups.contains_key(group));

        let workspace_ids: HashMap<WorkspaceKey, InternalWorkspaceId> = persisted
            .workspaces
            .into_iter()
            // Drop mappings for missing groups. They can happen after a schema change or a manually
            // edited runtime file, and keeping them would make IDs resolve to invisible groups.
            .filter(|workspace| groups.contains_key(&workspace.group))
            .map(|workspace| {
                (
                    WorkspaceKey {
                        group: workspace.group,
                        slot: workspace.slot,
                        visible: workspace.visible,
                    },
                    workspace.internal_id,
                )
            })
            .collect();

        // next_workspace_id must be greater than any restored mapping, otherwise a later allocation
        // could reuse an existing internal Hyprland workspace ID.
        let max_workspace_id = workspace_ids.values().copied().max().unwrap_or(0) + 1;
        let next_workspace_id = persisted
            .next_workspace_id
            .max(FIRST_INTERNAL_WORKSPACE_ID)
            .max(FIRST_INTERNAL_WORKSPACE_ID + slot_ids.len() as u64 * VISIBLE_WORKSPACES_PER_SLOT)
            .max(max_workspace_id);

        Some(State {
            active_group,
            previous_group,
            groups,
            slots,
            workspace_ids,
            next_workspace_id,
        })
    }

    pub fn persisted(&self) -> PersistedState {
        // Persist only logical state. Runtime monitor IDs and present workspace IDs are intentionally
        // excluded because Hyprland regenerates them per session/hotplug event.
        let mut groups: Vec<PersistedGroup> = self
            .groups
            .values()
            .map(|group| {
                let mut active_visible_by_slot: Vec<(SlotId, VisibleWorkspace)> = group
                    .active_visible_by_slot
                    .iter()
                    .map(|(slot, visible)| (*slot, *visible))
                    .collect();
                active_visible_by_slot.sort_unstable_by_key(|(slot, _)| *slot);

                PersistedGroup {
                    id: group.id,
                    name: group.name.clone(),
                    active_visible_by_slot,
                }
            })
            .collect();
        groups.sort_unstable_by_key(|group| group.id);

        let mut workspaces: Vec<PersistedWorkspaceEntry> = self
            .workspace_ids
            .iter()
            .map(|(key, internal_id)| PersistedWorkspaceEntry {
                group: key.group,
                slot: key.slot,
                visible: key.visible,
                internal_id: *internal_id,
            })
            .collect();
        workspaces
            .sort_unstable_by_key(|workspace| (workspace.group, workspace.slot, workspace.visible));

        PersistedState {
            version: PERSISTED_STATE_VERSION,
            active_group: self.active_group,
            previous_group: self.previous_group,
            groups,
            workspaces,
            next_workspace_id: self.next_workspace_id,
        }
    }

    pub fn active_visible(&self, slot: SlotId) -> VisibleWorkspace {
        self.group(self.active_group).active_visible(slot)
    }

    pub fn active_visible_in_group(&self, group: GroupId, slot: SlotId) -> VisibleWorkspace {
        self.group(group).active_visible(slot)
    }

    pub fn select_workspace(
        &mut self,
        slot: SlotId,
        visible: VisibleWorkspace,
    ) -> InternalWorkspaceId {
        self.set_active_visible(slot, visible);
        self.workspace_id_for(self.active_group, slot, visible)
    }

    pub fn set_active_visible(&mut self, slot: SlotId, visible: VisibleWorkspace) {
        self.group_mut(self.active_group)
            .set_active_visible(slot, visible);
    }

    pub fn workspace_id_for(
        &mut self,
        group: GroupId,
        slot: SlotId,
        visible: VisibleWorkspace,
    ) -> InternalWorkspaceId {
        // Lazy allocation keeps startup and group creation cheap: empty workspaces do not get
        // Hyprland IDs until the user actually selects/moves to them.
        let key = WorkspaceKey {
            group,
            slot,
            visible,
        };

        if let Some(id) = self.workspace_ids.get(&key) {
            return *id;
        }

        let id = self
            .default_workspace_id(group, slot, visible)
            .filter(|id| !self.workspace_ids.values().any(|used| used == id))
            .unwrap_or_else(|| self.next_available_workspace_id());
        self.workspace_ids.insert(key, id);
        id
    }

    pub fn default_workspace_id(
        &self,
        group: GroupId,
        slot: SlotId,
        visible: VisibleWorkspace,
    ) -> Option<InternalWorkspaceId> {
        if group != DEFAULT_GROUP_ID || !self.slots.contains_key(&slot) {
            return None;
        }
        if !(1..=VISIBLE_WORKSPACES_PER_SLOT).contains(&visible) {
            return None;
        }

        Some(FIRST_INTERNAL_WORKSPACE_ID + (slot - 1) * VISIBLE_WORKSPACES_PER_SLOT + visible - 1)
    }

    pub fn default_key_for_workspace_id(
        &self,
        workspace_id: InternalWorkspaceId,
    ) -> Option<WorkspaceKey> {
        if workspace_id < FIRST_INTERNAL_WORKSPACE_ID {
            return None;
        }

        let offset = workspace_id - FIRST_INTERNAL_WORKSPACE_ID;
        let slot = offset / VISIBLE_WORKSPACES_PER_SLOT + 1;
        let visible = offset % VISIBLE_WORKSPACES_PER_SLOT + 1;
        if !self.slots.contains_key(&slot) {
            return None;
        }

        Some(WorkspaceKey {
            group: DEFAULT_GROUP_ID,
            slot,
            visible,
        })
    }

    fn next_available_workspace_id(&mut self) -> InternalWorkspaceId {
        while self
            .workspace_ids
            .values()
            .any(|id| *id == self.next_workspace_id)
        {
            self.next_workspace_id += 1;
        }

        let id = self.next_workspace_id;
        self.next_workspace_id += 1;
        id
    }

    pub fn known_workspace_id(
        &self,
        group: GroupId,
        slot: SlotId,
        visible: VisibleWorkspace,
    ) -> Option<InternalWorkspaceId> {
        self.workspace_ids
            .get(&WorkspaceKey {
                group,
                slot,
                visible,
            })
            .copied()
    }

    pub fn set_workspace_id(
        &mut self,
        group: GroupId,
        slot: SlotId,
        visible: VisibleWorkspace,
        internal_id: InternalWorkspaceId,
    ) {
        // Used by runtime-state loading and best-effort startup recovery. Bump next_workspace_id so
        // future lazy allocations cannot collide with recovered IDs.
        self.workspace_ids.retain(|_, id| *id != internal_id);
        self.workspace_ids.insert(
            WorkspaceKey {
                group,
                slot,
                visible,
            },
            internal_id,
        );
        self.next_workspace_id = self.next_workspace_id.max(internal_id + 1);
    }

    pub fn key_for_workspace_id(&self, workspace_id: InternalWorkspaceId) -> Option<WorkspaceKey> {
        self.workspace_ids
            .iter()
            .find(|(_, id)| **id == workspace_id)
            .map(|(key, _)| *key)
    }

    pub fn swap_active_workspace_ids(
        &mut self,
        source_slot: SlotId,
        target_slot: SlotId,
    ) -> (InternalWorkspaceId, InternalWorkspaceId) {
        // Swap the IDs assigned to the visible labels, not the active visible labels themselves.
        // That matches the UX requirement: labels stay on slots while contents/layout move.
        let group = self.active_group;
        let source_visible = self.active_visible(source_slot);
        let target_visible = self.active_visible(target_slot);
        let source_key = WorkspaceKey {
            group,
            slot: source_slot,
            visible: source_visible,
        };
        let target_key = WorkspaceKey {
            group,
            slot: target_slot,
            visible: target_visible,
        };
        let source_id = self.workspace_id_for(group, source_slot, source_visible);
        let target_id = self.workspace_id_for(group, target_slot, target_visible);

        self.workspace_ids.insert(source_key, target_id);
        self.workspace_ids.insert(target_key, source_id);

        (source_id, target_id)
    }

    pub fn swap_slot_workspace_mappings(&mut self, source_slot: SlotId, target_slot: SlotId) {
        if source_slot == target_slot {
            return;
        }

        let swapped: Vec<(WorkspaceKey, InternalWorkspaceId)> = self
            .workspace_ids
            .iter()
            .filter_map(|(key, internal_id)| {
                let slot = if key.slot == source_slot {
                    target_slot
                } else if key.slot == target_slot {
                    source_slot
                } else {
                    return None;
                };

                Some((WorkspaceKey { slot, ..*key }, *internal_id))
            })
            .collect();

        self.workspace_ids
            .retain(|key, _| key.slot != source_slot && key.slot != target_slot);
        self.workspace_ids.extend(swapped);
    }

    pub fn slot_key(&self, slot: SlotId) -> Option<&str> {
        self.slots.get(&slot).map(|slot| slot.key.as_str())
    }

    pub fn create_group(&mut self, name: impl Into<String>) -> GroupId {
        let id = self.next_group_id();
        self.groups
            .insert(id, Group::new(id, name, self.slots.keys().copied()));
        id
    }

    pub fn rename_group(&mut self, group: GroupId, name: impl Into<String>) {
        self.group_mut(group).name = name.into();
    }

    pub fn delete_group(&mut self, group: GroupId) {
        self.group(group);
        self.groups.remove(&group);
        // previous_group is used to put the last group first in the quick selector. Clear it when
        // deleting that group so Win+P Enter cannot point at a removed ID.
        if self.previous_group == Some(group) {
            self.previous_group = None;
        }
        self.workspace_ids.retain(|key, _| key.group != group);
    }

    pub fn ensure_group(&mut self, group: GroupId, name: impl Into<String>) {
        self.groups
            .entry(group)
            .or_insert_with(|| Group::new(group, name, self.slots.keys().copied()));
    }

    pub fn has_group(&self, group: GroupId) -> bool {
        self.groups.contains_key(&group)
    }

    pub fn switch_group(&mut self, group: GroupId) {
        self.group(group);
        // Track previous only for real group changes; repeated switch_group(active) should not break
        // Win+P Enter toggling between two groups.
        if group != self.active_group {
            self.previous_group = Some(self.active_group);
        }
        self.active_group = group;
    }

    pub fn restore_active_group(&mut self, group: GroupId) {
        self.group(group);
        // Startup/hotplug reconciliation should not update previous_group. It is restoring known
        // reality, not a user-requested group switch.
        self.active_group = group;
    }

    pub fn attach_output(
        &mut self,
        slot: SlotId,
        output: impl Into<String>,
        runtime_monitor_id: u64,
    ) {
        let slot = self.slot_mut(slot);
        slot.attached_output = Some(output.into());
        slot.runtime_monitor_id = Some(runtime_monitor_id);
    }

    pub fn detach_slot(&mut self, slot: SlotId) {
        let slot = self.slot_mut(slot);
        slot.attached_output = None;
        slot.runtime_monitor_id = None;
    }

    pub fn attach_monitors_in_order(&mut self, monitors: &[crate::hyprland::MonitorInfo]) {
        // Simple fallback policy: sorted monitor order maps to slots 1/2/3. Host-specific policies
        // below should be preferred where the physical layout is known.
        let mut slot_ids: Vec<SlotId> = self.slots.keys().copied().collect();
        slot_ids.sort_unstable();

        for (index, slot_id) in slot_ids.into_iter().enumerate() {
            if let Some(monitor) = monitors.get(index) {
                self.attach_output(slot_id, monitor.name.clone(), monitor.id);
            } else {
                self.detach_slot(slot_id);
            }
        }
    }

    pub fn attach_monitors_fixed_outputs(
        &mut self,
        monitors: &[crate::hyprland::MonitorInfo],
        output_slots: &[(&str, SlotId)],
    ) {
        let mut attached_slots = Vec::new();
        for (output_name, slot_id) in output_slots {
            if let Some(monitor) = monitors.iter().find(|monitor| monitor.name == *output_name) {
                self.attach_output(*slot_id, monitor.name.clone(), monitor.id);
                attached_slots.push(*slot_id);
            }
        }

        for slot_id in self.slot_ids() {
            if !attached_slots.contains(&slot_id) {
                self.detach_slot(slot_id);
            }
        }
    }

    pub fn attach_monitors_primary_and_hotplug(
        &mut self,
        monitors: &[crate::hyprland::MonitorInfo],
        primary_output: &str,
        primary_slot: SlotId,
        hotplug_slots: &[SlotId],
    ) {
        let mut planned: Vec<(SlotId, crate::hyprland::MonitorInfo)> = Vec::new();
        if let Some(primary_monitor) = monitors
            .iter()
            .find(|monitor| monitor.name == primary_output)
        {
            planned.push((primary_slot, primary_monitor.clone()));
        }

        let mut external_monitors: Vec<crate::hyprland::MonitorInfo> = monitors
            .iter()
            .filter(|monitor| monitor.name != primary_output)
            .cloned()
            .collect();
        external_monitors.sort_unstable_by_key(|monitor| monitor.x);

        let mut used_external_names: Vec<String> = Vec::new();
        for slot_id in hotplug_slots {
            if let Some(attached_output) = self
                .slots
                .get(slot_id)
                .and_then(|slot| slot.attached_output.as_deref())
            {
                if let Some(monitor) = external_monitors
                    .iter()
                    .find(|monitor| monitor.name == attached_output)
                {
                    planned.push((*slot_id, monitor.clone()));
                    used_external_names.push(monitor.name.clone());
                }
            }
        }

        for monitor in external_monitors {
            if used_external_names.contains(&monitor.name) {
                continue;
            }
            if let Some(slot_id) = hotplug_slots.iter().find(|slot_id| {
                !planned
                    .iter()
                    .any(|(planned_slot_id, _)| planned_slot_id == *slot_id)
            }) {
                planned.push((*slot_id, monitor));
            }
        }

        for slot_id in self.slot_ids() {
            if let Some((_, monitor)) = planned
                .iter()
                .find(|(planned_slot_id, _)| *planned_slot_id == slot_id)
            {
                self.attach_output(slot_id, monitor.name.clone(), monitor.id);
            } else {
                self.detach_slot(slot_id);
            }
        }
    }

    pub fn runtime_monitor_id_for_slot(&self, slot: SlotId) -> Option<u64> {
        self.slots
            .get(&slot)
            .and_then(|slot| slot.runtime_monitor_id)
    }

    pub fn slot_for_monitor_id(&self, runtime_monitor_id: u64) -> Option<SlotId> {
        self.slots
            .values()
            .find(|slot| slot.runtime_monitor_id == Some(runtime_monitor_id))
            .map(|slot| slot.id)
    }

    pub fn slot_for_output_name(&self, output_name: &str) -> Option<SlotId> {
        self.slots
            .values()
            .find(|slot| slot.attached_output.as_deref() == Some(output_name))
            .map(|slot| slot.id)
    }

    pub fn snapshot(&self) -> StateSnapshot {
        let mut groups: Vec<GroupSnapshot> = self
            .groups
            .values()
            .map(|group| {
                let mut active_visible_by_slot: Vec<(SlotId, VisibleWorkspace)> = group
                    .active_visible_by_slot
                    .iter()
                    .map(|(slot, visible)| (*slot, *visible))
                    .collect();
                active_visible_by_slot.sort_unstable_by_key(|(slot, _)| *slot);

                GroupSnapshot {
                    id: group.id,
                    name: group.name.clone(),
                    active_visible_by_slot,
                }
            })
            .collect();
        groups.sort_unstable_by_key(|group| group.id);

        let mut slots: Vec<SlotSnapshot> = self
            .slots
            .values()
            .map(|slot| SlotSnapshot {
                id: slot.id,
                key: slot.key.clone(),
                label: slot.label.clone(),
                attached_output: slot.attached_output.clone(),
                runtime_monitor_id: slot.runtime_monitor_id,
            })
            .collect();
        slots.sort_unstable_by_key(|slot| slot.id);

        let mut workspaces: Vec<WorkspaceEntry> = self
            .workspace_ids
            .iter()
            .map(|(key, internal_id)| WorkspaceEntry {
                group: key.group,
                slot: key.slot,
                visible: key.visible,
                internal_id: *internal_id,
            })
            .collect();
        workspaces
            .sort_unstable_by_key(|workspace| (workspace.group, workspace.slot, workspace.visible));

        StateSnapshot {
            active_group: self.active_group,
            previous_group: self.previous_group,
            groups,
            slots,
            workspaces,
        }
    }

    fn next_group_id(&self) -> GroupId {
        let mut id = DEFAULT_GROUP_ID;
        while self.groups.contains_key(&id) {
            id += 1;
        }
        id
    }

    fn group(&self, group: GroupId) -> &Group {
        self.groups
            .get(&group)
            .unwrap_or_else(|| panic!("unknown workspace group {group}"))
    }

    fn group_mut(&mut self, group: GroupId) -> &mut Group {
        self.groups
            .get_mut(&group)
            .unwrap_or_else(|| panic!("unknown workspace group {group}"))
    }

    fn slot_mut(&mut self, slot: SlotId) -> &mut Slot {
        self.slots
            .get_mut(&slot)
            .unwrap_or_else(|| panic!("unknown slot {slot}"))
    }

    fn slot_ids(&self) -> Vec<SlotId> {
        let mut slot_ids: Vec<SlotId> = self.slots.keys().copied().collect();
        slot_ids.sort_unstable();
        slot_ids
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_state() -> State {
        State::new([
            Slot::new(1, "u", "left"),
            Slot::new(2, "i", "middle"),
            Slot::new(3, "o", "right"),
        ])
    }

    fn monitor(id: u64, name: &str, x: i64) -> crate::hyprland::MonitorInfo {
        crate::hyprland::MonitorInfo {
            id,
            name: name.to_string(),
            x,
        }
    }

    #[test]
    fn initializes_main_group_and_default_active_workspaces() {
        let state = test_state();

        assert_eq!(state.active_group, DEFAULT_GROUP_ID);
        assert_eq!(state.groups[&DEFAULT_GROUP_ID].name, DEFAULT_GROUP_NAME);
        assert_eq!(state.active_visible(1), DEFAULT_VISIBLE_WORKSPACE);
        assert_eq!(state.active_visible(2), DEFAULT_VISIBLE_WORKSPACE);
        assert_eq!(state.active_visible(3), DEFAULT_VISIBLE_WORKSPACE);
    }

    #[test]
    fn lazily_allocates_stable_workspace_ids() {
        let mut state = test_state();

        let first = state.workspace_id_for(0, 1, 1);
        let same = state.workspace_id_for(0, 1, 1);
        let different_visible = state.workspace_id_for(0, 1, 2);
        let different_slot = state.workspace_id_for(0, 2, 1);

        assert_eq!(first, FIRST_INTERNAL_WORKSPACE_ID);
        assert_eq!(same, first);
        assert_ne!(different_visible, first);
        assert_ne!(different_slot, first);
        assert_eq!(state.known_workspace_id(0, 1, 1), Some(first));
    }

    #[test]
    fn default_group_uses_seeded_workspace_ids() {
        let mut state = test_state();

        assert_eq!(state.workspace_id_for(0, 1, 1), 1000);
        assert_eq!(state.workspace_id_for(0, 2, 1), 1010);
        assert_eq!(state.workspace_id_for(0, 3, 10), 1029);
        assert_eq!(
            state.default_key_for_workspace_id(1010),
            Some(WorkspaceKey {
                group: 0,
                slot: 2,
                visible: 1,
            })
        );
    }

    #[test]
    fn selecting_workspace_updates_active_visible_for_slot() {
        let mut state = test_state();

        let workspace_id = state.select_workspace(2, 7);

        assert_eq!(workspace_id, 1016);
        assert_eq!(state.active_visible(1), 1);
        assert_eq!(state.active_visible(2), 7);
        assert_eq!(state.active_visible(3), 1);
    }

    #[test]
    fn swaps_all_workspace_mappings_between_slots() {
        let mut state = test_state();
        let slot_1_group_0 = state.workspace_id_for(0, 1, 1);
        let slot_2_group_0 = state.workspace_id_for(0, 2, 1);
        let group = state.create_group("Other");
        let slot_1_group_1 = state.workspace_id_for(group, 1, 3);

        state.swap_slot_workspace_mappings(1, 2);

        assert_eq!(state.known_workspace_id(0, 2, 1), Some(slot_1_group_0));
        assert_eq!(state.known_workspace_id(0, 1, 1), Some(slot_2_group_0));
        assert_eq!(state.known_workspace_id(group, 2, 3), Some(slot_1_group_1));
        assert_eq!(state.known_workspace_id(group, 1, 3), None);
    }

    #[test]
    fn setting_active_visible_does_not_allocate_workspace_id() {
        let mut state = test_state();

        state.set_active_visible(1, 8);

        assert_eq!(state.active_visible(1), 8);
        assert!(state.snapshot().workspaces.is_empty());
    }

    #[test]
    fn groups_keep_independent_active_visible_workspaces() {
        let mut state = test_state();

        state.select_workspace(1, 4);
        let other = state.create_group("Work");
        state.switch_group(other);
        state.select_workspace(1, 2);

        assert_eq!(state.active_visible(1), 2);
        state.switch_group(DEFAULT_GROUP_ID);
        assert_eq!(state.active_visible(1), 4);
    }

    #[test]
    fn tracks_previous_group_on_switch() {
        let mut state = test_state();
        let other = state.create_group("Other");

        state.switch_group(other);

        assert_eq!(state.active_group, other);
        assert_eq!(state.previous_group, Some(DEFAULT_GROUP_ID));

        state.switch_group(DEFAULT_GROUP_ID);

        assert_eq!(state.active_group, DEFAULT_GROUP_ID);
        assert_eq!(state.previous_group, Some(other));
    }

    #[test]
    fn ensure_group_creates_specific_group_id() {
        let mut state = test_state();

        state.ensure_group(10, "Group 10");
        state.switch_group(10);

        assert_eq!(state.active_group, 10);
        assert_eq!(state.groups[&10].name, "Group 10");
    }

    #[test]
    fn renames_group() {
        let mut state = test_state();
        let group = state.create_group("Temp");

        state.rename_group(group, "Renamed");

        assert_eq!(state.groups[&group].name, "Renamed");
    }

    #[test]
    fn delete_group_removes_group_and_workspace_mappings() {
        let mut state = test_state();
        let group = state.create_group("Temp");
        let workspace_id = state.workspace_id_for(group, 1, 1);
        state.switch_group(group);
        state.switch_group(DEFAULT_GROUP_ID);

        state.delete_group(group);

        assert!(!state.has_group(group));
        assert_eq!(state.previous_group, None);
        assert_eq!(state.key_for_workspace_id(workspace_id), None);
    }

    #[test]
    fn persisted_state_roundtrips_logical_state_without_runtime_slots() {
        let mut state = test_state();
        let group = state.create_group("Other");
        state.switch_group(group);
        state.set_active_visible(1, 3);
        let workspace_id = state.workspace_id_for(group, 1, 3);
        state.attach_output(1, "eDP-1", 42);

        let restored = State::from_persisted(
            [
                Slot::new(1, "u", "left"),
                Slot::new(2, "i", "middle"),
                Slot::new(3, "o", "right"),
            ],
            state.persisted(),
        )
        .unwrap();

        assert_eq!(restored.active_group, group);
        assert_eq!(restored.previous_group, Some(DEFAULT_GROUP_ID));
        assert_eq!(restored.groups[&group].name, "Other");
        assert_eq!(restored.active_visible(1), 3);
        assert_eq!(restored.known_workspace_id(group, 1, 3), Some(workspace_id));
        assert_eq!(restored.runtime_monitor_id_for_slot(1), None);
    }

    #[test]
    fn tracks_slot_attachment_by_runtime_monitor_id() {
        let mut state = test_state();

        state.attach_output(2, "DP-1", 7);

        assert_eq!(state.slot_for_monitor_id(7), Some(2));
        assert_eq!(state.slots[&2].attached_output.as_deref(), Some("DP-1"));

        state.detach_slot(2);

        assert_eq!(state.slot_for_monitor_id(7), None);
        assert_eq!(state.slots[&2].attached_output, None);
    }

    #[test]
    fn fixed_output_policy_ignores_monitor_position_order() {
        let mut state = test_state();

        state.attach_monitors_fixed_outputs(
            &[
                monitor(7, "DP-2", 0),
                monitor(4, "DP-1", 2560),
                monitor(5, "DP-3", 5120),
            ],
            &[("DP-1", 1), ("DP-3", 2), ("DP-2", 3)],
        );

        assert_eq!(state.slot_for_output_name("DP-1"), Some(1));
        assert_eq!(state.slot_for_output_name("DP-3"), Some(2));
        assert_eq!(state.slot_for_output_name("DP-2"), Some(3));
    }

    #[test]
    fn laptop_policy_pins_primary_and_preserves_hotplug_slots() {
        let mut state = test_state();

        state.attach_monitors_primary_and_hotplug(
            &[monitor(0, "eDP-1", 0), monitor(1, "HEADLESS-2", 1600)],
            "eDP-1",
            2,
            &[3, 1],
        );
        assert_eq!(state.slot_for_output_name("eDP-1"), Some(2));
        assert_eq!(state.slot_for_output_name("HEADLESS-2"), Some(3));

        state.attach_monitors_primary_and_hotplug(
            &[
                monitor(0, "eDP-1", 0),
                monitor(2, "HEADLESS-3", -1600),
                monitor(1, "HEADLESS-2", 1600),
            ],
            "eDP-1",
            2,
            &[3, 1],
        );
        assert_eq!(state.slot_for_output_name("eDP-1"), Some(2));
        assert_eq!(state.slot_for_output_name("HEADLESS-2"), Some(3));
        assert_eq!(state.slot_for_output_name("HEADLESS-3"), Some(1));
    }

    #[test]
    fn attaches_monitor_ids_to_slots_in_slot_order() {
        let mut state = test_state();

        state.attach_monitors_in_order(&[
            crate::hyprland::MonitorInfo {
                id: 4,
                name: "eDP-1".to_string(),
                x: 0,
            },
            crate::hyprland::MonitorInfo {
                id: 7,
                name: "HEADLESS-2".to_string(),
                x: 1600,
            },
        ]);

        assert_eq!(state.runtime_monitor_id_for_slot(1), Some(4));
        assert_eq!(state.runtime_monitor_id_for_slot(2), Some(7));
        assert_eq!(state.runtime_monitor_id_for_slot(3), None);
        assert_eq!(state.slot_for_monitor_id(7), Some(2));
        assert_eq!(state.slot_for_output_name("HEADLESS-2"), Some(2));
    }

    #[test]
    fn finds_workspace_key_for_allocated_internal_id() {
        let mut state = test_state();

        let id = state.workspace_id_for(0, 2, 5);

        assert_eq!(
            state.key_for_workspace_id(id),
            Some(WorkspaceKey {
                group: 0,
                slot: 2,
                visible: 5,
            })
        );
        assert_eq!(state.key_for_workspace_id(id + 1), None);
    }

    #[test]
    fn swaps_active_workspace_ids_between_slots() {
        let mut state = test_state();
        state.select_workspace(1, 2);
        state.select_workspace(2, 4);
        let source_id = state.known_workspace_id(0, 1, 2).unwrap();
        let target_id = state.known_workspace_id(0, 2, 4).unwrap();

        assert_eq!(
            state.swap_active_workspace_ids(1, 2),
            (source_id, target_id)
        );

        assert_eq!(state.known_workspace_id(0, 1, 2), Some(target_id));
        assert_eq!(state.known_workspace_id(0, 2, 4), Some(source_id));
        assert_eq!(state.active_visible(1), 2);
        assert_eq!(state.active_visible(2), 4);
    }

    #[test]
    fn swap_active_workspace_ids_allocates_missing_workspace_ids() {
        let mut state = test_state();
        state.set_active_visible(1, 3);
        state.set_active_visible(2, 5);

        let (source_id, target_id) = state.swap_active_workspace_ids(1, 2);

        assert_eq!(source_id, 1002);
        assert_eq!(target_id, 1014);
        assert_eq!(state.known_workspace_id(0, 1, 3), Some(target_id));
        assert_eq!(state.known_workspace_id(0, 2, 5), Some(source_id));
    }

    #[test]
    fn snapshot_lists_state_in_stable_order() {
        let mut state = test_state();
        state.attach_monitors_in_order(&[
            crate::hyprland::MonitorInfo {
                id: 4,
                name: "eDP-1".to_string(),
                x: 0,
            },
            crate::hyprland::MonitorInfo {
                id: 7,
                name: "HEADLESS-2".to_string(),
                x: 1600,
            },
        ]);
        let id = state.workspace_id_for(0, 2, 5);

        let snapshot = state.snapshot();

        assert_eq!(snapshot.active_group, 0);
        assert_eq!(snapshot.groups[0].id, 0);
        assert_eq!(snapshot.slots[0].id, 1);
        assert_eq!(snapshot.slots[0].runtime_monitor_id, Some(4));
        assert_eq!(snapshot.slots[2].runtime_monitor_id, None);
        assert_eq!(snapshot.workspaces.len(), 1);
        assert_eq!(snapshot.workspaces[0].internal_id, id);
        assert_eq!(snapshot.workspaces[0].visible, 5);
    }
}
