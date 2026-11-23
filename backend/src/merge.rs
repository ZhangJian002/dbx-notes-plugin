//! Vault merge decision table, content fingerprints and their unit tests.
//!
//! Kept in its own module so the merge rules can be exercised offline with
//! `cargo test` without a DBX host, network or filesystem.

use std::collections::{HashMap, HashSet};

use serde::Serialize;

use super::{
    fnv1a, mirror_name, new_note_id, now_ms, Attachment, AttachmentState, Note, NoteState, SyncState,
    Vault, FORMAT_VERSION,
};

#[derive(Default, Debug, PartialEq, Eq)]
pub struct MergeStats {
    pub pulled: u32,
    pub pushed: u32,
    pub conflicts: u32,
}

/// The exact set of fields that make two notes "the same" from a user's point
/// of view. IDs and timestamps are deliberately excluded: touching a page
/// without changing its content must not look like an edit to the merger.
#[derive(Serialize)]
struct CanonicalNote<'a> {
    title: &'a str,
    body: &'a str,
    parent_id: Option<&'a str>,
    order: i64,
    tags: &'a [String],
    favorite: bool,
    icon: Option<&'a str>,
    deleted_at: Option<i64>,
}

#[derive(Serialize)]
struct CanonicalAttachment<'a> {
    name: &'a str,
    mime: &'a str,
    size: u64,
    created_at: i64,
}

/// New content-only fingerprint. `v2:` keeps it distinct from legacy hashes.
pub fn content_rev(note: &Note) -> String {
    let canonical = CanonicalNote {
        title: &note.title,
        body: &note.body,
        parent_id: note.parent_id.as_deref(),
        order: note.order,
        tags: &note.tags,
        favorite: note.favorite,
        icon: note.icon.as_deref(),
        deleted_at: note.deleted_at,
    };
    let bytes = serde_json::to_vec(&canonical).unwrap_or_default();
    format!("v2:{:016x}", fnv1a(&bytes))
}

/// Legacy fingerprint: hashes the whole Note (including timestamps). Kept so
/// old `sync-state.json` files remain valid after an upgrade.
pub fn note_rev(note: &Note) -> String {
    let bytes = serde_json::to_vec(note).unwrap_or_default();
    format!("{:016x}", fnv1a(&bytes))
}

/// Fingerprint the *current* note in the same format as the stored baseline:
/// new states use content-only hashes, legacy states keep using the full hash.
/// This makes upgrades behaviour-preserving.
pub(crate) fn effective_rev(note: &Note, previous: Option<&NoteState>) -> String {
    match previous.and_then(|state| state.content_rev.as_deref()) {
        Some(_) => content_rev(note),
        None => note_rev(note),
    }
}

fn attachment_rev(attachment: &Attachment) -> String {
    let canonical = CanonicalAttachment {
        name: &attachment.name,
        mime: &attachment.mime,
        size: attachment.size,
        created_at: attachment.created_at,
    };
    let bytes = serde_json::to_vec(&canonical).unwrap_or_default();
    format!("v2:{:016x}", fnv1a(&bytes))
}

pub fn same_content(a: &Note, b: &Note) -> bool {
    a.title == b.title
        && a.body == b.body
        && a.parent_id == b.parent_id
        && a.order == b.order
        && a.tags == b.tags
        && a.favorite == b.favorite
        && a.icon == b.icon
        && a.deleted_at == b.deleted_at
}

fn conflict_copy(note: &Note, known_ids: &HashSet<&str>) -> Note {
    let now = now_ms();
    let mut copy = note.clone();
    copy.id = new_note_id();
    copy.title = format!("{} (conflict)", note.title);
    copy.created_at = now;
    copy.updated_at = now;
    copy.deleted_at = None;
    copy.favorite = false;
    // A tag needs no schema change and lets users search for every conflict.
    if copy.tags.len() < 32 && !copy.tags.iter().any(|tag| tag.eq_ignore_ascii_case("conflict")) {
        copy.tags.push("conflict".to_string());
    }
    // If the loser's parent was hard-deleted on the winning side, keep the
    // copy visible at the top level instead of stranding it in the tree.
    if let Some(parent) = copy.parent_id.as_deref() {
        if !known_ids.contains(parent) {
            copy.parent_id = None;
        }
    }
    copy
}

/// Merge two vaults. See the module tests for every decision branch.
pub fn merge_vaults(local: &Vault, remote: &Vault, state: &SyncState) -> (Vault, MergeStats) {
    let mut stats = MergeStats::default();
    let lmap: HashMap<&str, &Note> = local.notes.iter().map(|note| (note.id.as_str(), note)).collect();
    let rmap: HashMap<&str, &Note> = remote.notes.iter().map(|note| (note.id.as_str(), note)).collect();
    let known_ids: HashSet<&str> = lmap.keys().chain(rmap.keys()).copied().collect();

    let mut ids: Vec<String> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    for note in local.notes.iter().chain(remote.notes.iter()) {
        if seen.insert(note.id.clone()) {
            ids.push(note.id.clone());
        }
    }

    let mut notes: Vec<Note> = Vec::new();
    let mut conflict_copies: Vec<Note> = Vec::new();

    for id in ids {
        match (lmap.get(id.as_str()), rmap.get(id.as_str())) {
            (Some(local_note), Some(remote_note)) => {
                if local_note == remote_note {
                    notes.push((*local_note).clone());
                    continue;
                }
                let previous = state.notes.get(&id);
                let first_seen = previous.is_none();
                let local_changed = previous_changed(local_note, previous);
                let remote_changed = previous_changed(remote_note, previous);
                let both = local_changed && remote_changed;
                let content_differs = !same_content(local_note, remote_note);
                let (winner, loser) = if both {
                    if local_note.updated_at >= remote_note.updated_at {
                        (local_note, remote_note)
                    } else {
                        (remote_note, local_note)
                    }
                } else if local_changed {
                    (local_note, remote_note)
                } else if remote_changed {
                    (remote_note, local_note)
                } else if local_note.updated_at >= remote_note.updated_at {
                    (local_note, remote_note)
                } else {
                    (remote_note, local_note)
                };
                // The welcome page is seeded on every device: on a first sync
                // prefer the newer copy instead of leaving a conflict page.
                let welcome_first_sync = first_seen && id == "note-welcome";
                if both && content_differs && !loser.is_deleted() && !welcome_first_sync {
                    conflict_copies.push(conflict_copy(loser, &known_ids));
                    stats.conflicts += 1;
                }
                if content_differs {
                    if std::ptr::eq(winner, local_note) {
                        stats.pushed += 1;
                    } else {
                        stats.pulled += 1;
                    }
                }
                notes.push((*winner).clone());
            }
            (Some(local_note), None) => {
                // Missing remotely: keep local only if it changed since the
                // baseline (otherwise the remote hard-delete wins).
                if state.notes.contains_key(&id) {
                    if previous_changed(local_note, state.notes.get(&id)) {
                        notes.push((*local_note).clone());
                        stats.pushed += 1;
                    }
                } else {
                    notes.push((*local_note).clone());
                    stats.pushed += 1;
                }
            }
            (None, Some(remote_note)) => {
                if state.notes.contains_key(&id) {
                    if previous_changed(remote_note, state.notes.get(&id)) {
                        notes.push((*remote_note).clone());
                        stats.pulled += 1;
                    }
                } else {
                    notes.push((*remote_note).clone());
                    stats.pulled += 1;
                }
            }
            (None, None) => {}
        }
    }

    notes.extend(conflict_copies);
    let attachments = merge_attachments(local, remote, state);
    let updated_at = local.updated_at.max(remote.updated_at).max(now_ms());
    (
        Vault {
            format: FORMAT_VERSION,
            id: local.id.clone(),
            name: local.name.clone(),
            updated_at,
            notes,
            attachments,
        },
        stats,
    )
}

fn previous_changed(note: &Note, previous: Option<&NoteState>) -> bool {
    match previous {
        None => true,
        Some(entry) => effective_rev(note, Some(entry)) != baseline_rev(entry),
    }
}

fn baseline_rev(entry: &NoteState) -> &str {
    entry.content_rev.as_deref().unwrap_or(&entry.rev)
}

pub fn merge_attachments(local: &Vault, remote: &Vault, state: &SyncState) -> Vec<Attachment> {
    let lmap: HashMap<&str, &Attachment> = local.attachments.iter().map(|item| (item.id.as_str(), item)).collect();
    let rmap: HashMap<&str, &Attachment> = remote.attachments.iter().map(|item| (item.id.as_str(), item)).collect();
    let mut result = Vec::new();
    let mut seen = HashSet::new();
    for attachment in local.attachments.iter().chain(remote.attachments.iter()) {
        if !seen.insert(attachment.id.clone()) {
            continue;
        }
        let previous = state.attachments.get(&attachment.id);
        match (lmap.get(attachment.id.as_str()), rmap.get(attachment.id.as_str())) {
            (Some(left), Some(right)) => {
                if left == right {
                    result.push((*left).clone());
                    continue;
                }
                let left_changed = previous.map_or(true, |entry| &entry.rev != &attachment_rev(left));
                let right_changed = previous.map_or(true, |entry| &entry.rev != &attachment_rev(right));
                if left_changed && !right_changed {
                    result.push((*left).clone());
                } else if right_changed && !left_changed {
                    result.push((*right).clone());
                } else {
                    // Same id with different metadata: keep the local copy.
                    result.push((*left).clone());
                }
            }
            (Some(left), None) => {
                let unchanged = previous.map_or(false, |entry| entry.rev == attachment_rev(left));
                if !unchanged {
                    result.push((*left).clone());
                }
            }
            (None, Some(right)) => {
                let unchanged = previous.map_or(false, |entry| entry.rev == attachment_rev(right));
                if !unchanged {
                    result.push((*right).clone());
                }
            }
            (None, None) => {}
        }
    }
    result
}

pub fn state_snapshot(vault: &Vault) -> HashMap<String, NoteState> {
    vault
        .notes
        .iter()
        .map(|note| {
            (
                note.id.clone(),
                NoteState {
                    rev: note_rev(note),
                    content_rev: Some(content_rev(note)),
                    updated_at: note.updated_at,
                    mirror: if note.is_deleted() { None } else { Some(mirror_name(note)) },
                },
            )
        })
        .collect()
}

pub fn attachment_state_snapshot(vault: &Vault) -> HashMap<String, AttachmentState> {
    vault
        .attachments
        .iter()
        .map(|attachment| (attachment.id.clone(), AttachmentState { rev: attachment_rev(attachment) }))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn note(id: &str, body: &str, updated_at: i64) -> Note {
        Note {
            id: id.to_string(),
            title: "Title".to_string(),
            body: body.to_string(),
            parent_id: None,
            order: 0,
            tags: Vec::new(),
            favorite: false,
            icon: None,
            created_at: 100,
            updated_at,
            deleted_at: None,
        }
    }

    fn vault(notes: Vec<Note>, attachments: Vec<Attachment>) -> Vault {
        Vault {
            format: FORMAT_VERSION,
            id: "vault".to_string(),
            name: "Notes".to_string(),
            updated_at: 100,
            notes,
            attachments,
        }
    }

    fn attachment(id: &str) -> Attachment {
        Attachment {
            id: id.to_string(),
            name: "file.txt".to_string(),
            mime: "text/plain".to_string(),
            size: 10,
            created_at: 100,
        }
    }

    fn synced_state(v: &Vault) -> SyncState {
        SyncState {
            notes: state_snapshot(v),
            attachments: attachment_state_snapshot(v),
            ..Default::default()
        }
    }

    #[test]
    fn identical_notes_do_not_conflict() {
        let n = note("a", "body", 100);
        let v = vault(vec![n.clone()], vec![]);
        let (merged, stats) = merge_vaults(&v, &v, &synced_state(&v));
        assert_eq!(merged.notes.len(), 1);
        assert_eq!(stats, MergeStats::default());
    }

    #[test]
    fn content_rev_ignores_timestamps_and_ids() {
        let a = note("a", "body", 100);
        let mut b = note("b", "body", 999);
        b.updated_at = 42;
        assert_eq!(content_rev(&a), content_rev(&b));
        assert_ne!(note_rev(&a), note_rev(&b));
    }

    #[test]
    fn legacy_state_falls_back_to_legacy_rev() {
        let n = note("a", "body", 100);
        let legacy = NoteState {
            rev: note_rev(&n),
            content_rev: None,
            updated_at: n.updated_at,
            mirror: None,
        };
        assert_eq!(effective_rev(&n, Some(&legacy)), note_rev(&n));
        let modern = NoteState {
            content_rev: Some(content_rev(&n)),
            ..legacy
        };
        assert_eq!(effective_rev(&n, Some(&modern)), content_rev(&n));
    }

    #[test]
    fn timestamp_only_save_is_not_a_change() {
        let original = note("a", "body", 100);
        let state = synced_state(&vault(vec![original.clone()], vec![]));
        let local = note("a", "body", 500); // no-op save, timestamp moved
        let result = vault(vec![local], vec![]);
        let (merged, stats) = merge_vaults(&result, &vault(vec![original], vec![]), &state);
        assert_eq!(merged.notes.len(), 1);
        assert_eq!(merged.notes[0].updated_at, 500);
        assert_eq!(stats, MergeStats::default());
    }

    #[test]
    fn only_local_change_wins() {
        let original = note("a", "old", 100);
        let state = synced_state(&vault(vec![original.clone()], vec![]));
        let local = note("a", "local", 200);
        let (merged, stats) = merge_vaults(
            &vault(vec![local.clone()], vec![]),
            &vault(vec![original], vec![]),
            &state,
        );
        assert_eq!(merged.notes[0].body, "local");
        assert_eq!(stats.pushed, 1);
        assert_eq!(stats.conflicts, 0);
    }

    #[test]
    fn only_remote_change_wins() {
        let original = note("a", "old", 100);
        let state = synced_state(&vault(vec![original.clone()], vec![]));
        let remote = note("a", "remote", 200);
        let (merged, stats) = merge_vaults(
            &vault(vec![original], vec![]),
            &vault(vec![remote.clone()], vec![]),
            &state,
        );
        assert_eq!(merged.notes[0].body, "remote");
        assert_eq!(stats.pulled, 1);
    }

    #[test]
    fn both_changed_keeps_newer_and_creates_tagged_copy() {
        let original = note("a", "old", 100);
        let state = synced_state(&vault(vec![original.clone()], vec![]));
        let local = note("a", "local", 200);
        let remote = note("a", "remote", 300);
        let (merged, stats) = merge_vaults(
            &vault(vec![local.clone()], vec![]),
            &vault(vec![remote.clone()], vec![]),
            &state,
        );
        let winner = merged.notes.iter().find(|n| n.id == "a").unwrap();
        let copy = merged.notes.iter().find(|n| n.id != "a").unwrap();
        assert_eq!(winner.body, "remote");
        assert_eq!(copy.body, "local");
        assert_eq!(copy.title, "Title (conflict)");
        assert!(copy.tags.iter().any(|tag| tag == "conflict"));
        assert_ne!(copy.id, local.id);
        assert_eq!(stats.pulled, 1);
        assert_eq!(stats.conflicts, 1);
    }

    #[test]
    fn both_changed_with_same_content_has_no_conflict() {
        let original = note("a", "old", 100);
        let state = synced_state(&vault(vec![original.clone()], vec![]));
        let local = note("a", "same", 200);
        let remote = note("a", "same", 300);
        let (merged, stats) = merge_vaults(
            &vault(vec![local], vec![]),
            &vault(vec![remote], vec![]),
            &state,
        );
        assert_eq!(merged.notes.len(), 1);
        assert_eq!(stats.conflicts, 0);
        assert_eq!(stats.pushed, 0);
        assert_eq!(stats.pulled, 0);
    }

    #[test]
    fn welcome_page_on_first_sync_has_no_conflict() {
        let mut local = note("note-welcome", "local", 200);
        let mut remote = note("note-welcome", "remote", 300);
        local.tags = vec![];
        remote.tags = vec![];
        let (merged, stats) = merge_vaults(
            &vault(vec![local], vec![]),
            &vault(vec![remote], vec![]),
            &SyncState::default(),
        );
        assert_eq!(merged.notes.len(), 1);
        assert_eq!(merged.notes[0].body, "remote");
        assert_eq!(stats.conflicts, 0);
    }

    #[test]
    fn remote_hard_delete_wins_when_local_is_unchanged() {
        let original = note("a", "body", 100);
        let state = synced_state(&vault(vec![original.clone()], vec![]));
        let (merged, stats) = merge_vaults(
            &vault(vec![original], vec![]),
            &vault(vec![], vec![]),
            &state,
        );
        assert!(merged.notes.is_empty());
        assert_eq!(stats, MergeStats::default());
    }

    #[test]
    fn remote_hard_delete_is_revived_when_local_changed() {
        let original = note("a", "old", 100);
        let state = synced_state(&vault(vec![original.clone()], vec![]));
        let local = note("a", "local", 200);
        let (merged, stats) = merge_vaults(
            &vault(vec![local], vec![]),
            &vault(vec![], vec![]),
            &state,
        );
        assert_eq!(merged.notes.len(), 1);
        assert_eq!(stats.pushed, 1);
    }

    #[test]
    fn local_hard_delete_is_revived_when_remote_changed() {
        let original = note("a", "old", 100);
        let state = synced_state(&vault(vec![original.clone()], vec![]));
        let remote = note("a", "remote", 200);
        let (merged, stats) = merge_vaults(
            &vault(vec![], vec![]),
            &vault(vec![remote], vec![]),
            &state,
        );
        assert_eq!(merged.notes.len(), 1);
        assert_eq!(stats.pulled, 1);
    }

    #[test]
    fn conflict_copy_with_missing_parent_becomes_root() {
        let mut local = note("a", "local", 200);
        let mut remote = note("a", "remote", 300);
        local.parent_id = Some("gone".to_string());
        remote.parent_id = Some("gone".to_string());
        let base = note("a", "old", 100);
        let state = synced_state(&vault(vec![base], vec![]));
        let (merged, _) = merge_vaults(
            &vault(vec![local], vec![]),
            &vault(vec![remote], vec![]),
            &state,
        );
        let copy = merged.notes.iter().find(|n| n.id != "a").unwrap();
        assert_eq!(copy.parent_id, None);
    }

    #[test]
    fn attachments_union_and_hard_delete() {
        let a1 = attachment("a1");
        let a2 = attachment("a2");
        let base = vault(vec![], vec![a1.clone()]);
        let state = synced_state(&base);
        let local = vault(vec![], vec![a1.clone()]);
        let remote_with_new = vault(vec![], vec![a1.clone(), a2.clone()]);
        let merged = merge_vaults(&local, &remote_with_new, &state).0;
        assert_eq!(merged.attachments.len(), 2);

        let remote_deleted = vault(vec![], vec![a2]);
        let merged = merge_vaults(&local, &remote_deleted, &state).0;
        assert_eq!(merged.attachments.len(), 1);
        assert_eq!(merged.attachments[0].id, "a2");
    }

    #[test]
    fn newly_added_attachment_survives_remote_deletion() {
        let a1 = attachment("a1");
        let state = synced_state(&vault(vec![], vec![]));
        let local = vault(vec![], vec![a1.clone()]);
        let merged = merge_vaults(&local, &vault(vec![], vec![]), &state).0;
        assert_eq!(merged.attachments.len(), 1);
        assert_eq!(merged.attachments[0].id, a1.id);
    }
}
