use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine as _;
use dbx_plugin_sdk::{PluginEmitter, PluginError, PluginHandler, PluginMetadata, PluginServer, RequestContext};
use reqwest::blocking::Client;
use reqwest::header::{CONTENT_TYPE, ETAG, IF_MATCH};
use reqwest::{Method, StatusCode, Url};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

mod merge;

const FORMAT_VERSION: u32 = 1;
const MAX_NOTE_BYTES: usize = 7 * 1024 * 1024;
const MAX_ATTACHMENT_BYTES: u64 = 512 * 1024 * 1024;
const DEFAULT_BRANCH: &str = "main";
const MIRROR_DIR: &str = "DBX-notes";

// ---------------------------------------------------------------------------
// Data model
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Note {
    id: String,
    #[serde(default)]
    title: String,
    #[serde(default)]
    body: String,
    #[serde(default)]
    parent_id: Option<String>,
    #[serde(default)]
    order: i64,
    #[serde(default)]
    tags: Vec<String>,
    #[serde(default)]
    favorite: bool,
    #[serde(default)]
    icon: Option<String>,
    #[serde(default)]
    created_at: i64,
    #[serde(default)]
    updated_at: i64,
    #[serde(default)]
    deleted_at: Option<i64>,
}

impl Note {
    fn new(title: impl Into<String>, body: impl Into<String>, parent_id: Option<String>, order: i64) -> Self {
        let now = now_ms();
        Self {
            id: new_note_id(),
            title: title.into(),
            body: body.into(),
            parent_id,
            order,
            tags: Vec::new(),
            favorite: false,
            icon: None,
            created_at: now,
            updated_at: now,
            deleted_at: None,
        }
    }

    fn is_deleted(&self) -> bool {
        self.deleted_at.is_some()
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Attachment {
    id: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    mime: String,
    #[serde(default)]
    size: u64,
    #[serde(default)]
    created_at: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Vault {
    format: u32,
    id: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    updated_at: i64,
    #[serde(default)]
    notes: Vec<Note>,
    #[serde(default)]
    attachments: Vec<Attachment>,
}

impl Vault {
    fn new(name: impl Into<String>) -> Self {
        Self { format: FORMAT_VERSION, id: new_vault_id(), name: name.into(), updated_at: now_ms(), notes: Vec::new(), attachments: Vec::new() }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct NoteState {
    #[serde(default)]
    rev: String,
    /// Content-only fingerprint (v2). Older states don't have it; merge falls
    /// back to `rev` in that case so upgrading never changes sync semantics.
    #[serde(default)]
    content_rev: Option<String>,
    #[serde(default)]
    updated_at: i64,
    #[serde(default)]
    mirror: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct AttachmentState {
    #[serde(default)]
    rev: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SyncState {
    #[serde(default)]
    last_sync_at: Option<i64>,
    #[serde(default)]
    last_result: Option<String>,
    #[serde(default)]
    notes: HashMap<String, NoteState>,
    #[serde(default)]
    attachments: HashMap<String, AttachmentState>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
struct SyncConfig {
    mode: String,
    webdav_url: String,
    webdav_username: String,
    webdav_password: String,
    git_remote: String,
    git_branch: String,
    git_username: String,
    git_token: String,
    git_author_name: String,
    git_author_email: String,
    auto_sync_minutes: u64,
    export_markdown: bool,
    connect_timeout_secs: u64,
}

impl Default for SyncConfig {
    fn default() -> Self {
        Self {
            mode: "none".into(),
            webdav_url: String::new(),
            webdav_username: String::new(),
            webdav_password: String::new(),
            git_remote: String::new(),
            git_branch: DEFAULT_BRANCH.into(),
            git_username: "git".into(),
            git_token: String::new(),
            git_author_name: "DBX Notes".into(),
            git_author_email: "notes@dbx.local".into(),
            auto_sync_minutes: 5,
            export_markdown: true,
            connect_timeout_secs: 30,
        }
    }
}

impl SyncConfig {
    fn from_connection(connection: &Value) -> Self {
        let cfg = connection.get("external_config").cloned().unwrap_or_else(|| json!({}));
        let secrets = connection.get("connection_secrets").cloned().unwrap_or_else(|| json!({}));
        let text = |v: &Value, key: &str| v.get(key).and_then(Value::as_str).unwrap_or("").trim().to_string();
        let bool_value = |v: &Value, key: &str, fallback: bool| {
            v.get(key).and_then(Value::as_bool).unwrap_or(fallback)
        };
        let number = |v: &Value, key: &str, fallback: u64| v.get(key).and_then(Value::as_u64).unwrap_or(fallback);

        let mut mode = text(&cfg, "sync_mode").to_lowercase();
        if !["none", "webdav", "git"].contains(&mode.as_str()) {
            mode = "none".into();
        }
        let mut git_branch = text(&cfg, "git_branch");
        if git_branch.is_empty() {
            git_branch = DEFAULT_BRANCH.into();
        }
        let mut git_username = text(&cfg, "git_username");
        if git_username.is_empty() {
            git_username = "git".into();
        }
        let mut author_name = text(&cfg, "git_author_name");
        if author_name.is_empty() {
            author_name = "DBX Notes".into();
        }
        let mut author_email = text(&cfg, "git_author_email");
        if author_email.is_empty() {
            author_email = "notes@dbx.local".into();
        }

        Self {
            mode,
            webdav_url: text(&cfg, "webdav_url"),
            webdav_username: text(&cfg, "webdav_username"),
            webdav_password: text(&secrets, "webdav_password"),
            git_remote: text(&cfg, "git_remote"),
            git_branch,
            git_username,
            git_token: text(&secrets, "git_token"),
            git_author_name: author_name,
            git_author_email: author_email,
            auto_sync_minutes: number(&cfg, "auto_sync_minutes", 5).min(1440),
            export_markdown: bool_value(&cfg, "export_markdown", true),
            connect_timeout_secs: number(&cfg, "connect_timeout_secs", 30).clamp(5, 600),
        }
    }

    fn public(&self) -> Value {
        json!({
            "mode": self.mode,
            "webdavUrl": self.webdav_url,
            "webdavUsername": self.webdav_username,
            "hasWebdavPassword": !self.webdav_password.is_empty(),
            "gitRemote": self.git_remote,
            "gitBranch": self.git_branch,
            "gitUsername": self.git_username,
            "gitAuthorName": self.git_author_name,
            "gitAuthorEmail": self.git_author_email,
            "hasGitToken": !self.git_token.is_empty(),
            "autoSyncMinutes": self.auto_sync_minutes,
            "exportMarkdown": self.export_markdown,
            "configured": match self.mode.as_str() {
                "webdav" => !self.webdav_url.is_empty(),
                "git" => !self.git_remote.is_empty(),
                _ => false,
            }
        })
    }
}

struct Upload {
    path: PathBuf,
    name: String,
    mime: String,
    size: u64,
    received: u64,
}

struct Session {
    dir: PathBuf,
    config: SyncConfig,
    base_config: SyncConfig,
    vault: Vault,
    state: SyncState,
    uploads: HashMap<String, Upload>,
}

// ---------------------------------------------------------------------------
// Plugin
// ---------------------------------------------------------------------------

#[derive(Default)]
struct Plugin {
    sessions: Mutex<HashMap<String, Session>>,
}

impl PluginHandler for Plugin {
    fn handle(
        &self,
        _context: RequestContext,
        method: &str,
        params: Value,
        emitter: &PluginEmitter,
    ) -> Result<Value, PluginError> {
        match method {
            "connection/test" => test_connection(&params),
            "connection/connect" => self.connect(&params),
            "connection/disconnect" => self.disconnect(&params),
            "notes/snapshot" => self.snapshot(&params),
            "notes/save" => self.save_note(&params),
            "notes/create" => self.create_note(&params),
            "notes/delete" => self.delete_note(&params),
            "notes/restore" => self.restore_note(&params),
            "notes/purge" => self.purge_note(&params),
            "notes/purgeTrash" => self.purge_trash(&params),
            "notes/move" => self.move_note(&params),
            "notes/reorder" => self.reorder_note(&params),
            "attachments/begin" => self.attachment_begin(&params),
            "attachments/chunk" => self.attachment_chunk(&params),
            "attachments/finish" => self.attachment_finish(&params),
            "attachments/cancel" => self.attachment_cancel(&params),
            "attachments/get" => self.attachment_get(&params),
            "attachments/read" => self.attachment_read(&params),
            "sync/config/get" => self.sync_config_get(&params),
            "sync/config/save" => self.sync_config_save(&params),
            "sync/config/reset" => self.sync_config_reset(&params),
            "sync/status" => self.sync_status(&params),
            "sync/now" => self.sync_now(&params, emitter),
            _ => Err(PluginError::method_not_found(method)),
        }
    }
}

impl Plugin {
    fn connect(&self, params: &Value) -> Result<Value, PluginError> {
        let connection = params.get("connection").ok_or_else(|| PluginError::new(-32602, "Missing connection"))?;
        let connection_id = connection
            .get("id")
            .and_then(Value::as_str)
            .ok_or_else(|| PluginError::new(-32602, "Missing connection id"))?;
        let config = SyncConfig::from_connection(connection);
        let name = connection.get("name").and_then(Value::as_str).unwrap_or("DBX Notes");
        let session = load_session(connection_id, name, config)?;
        let summary = json!({
            "mode": session.config.mode,
            "notes": session.vault.notes.iter().filter(|n| !n.is_deleted()).count()
        });
        self.sessions
            .lock()
            .map_err(|_| PluginError::new(-32000, "Session registry is poisoned"))?
            .insert(connection_id.to_string(), session);
        Ok(json!({ "success": true, "message": "Notes vault is ready.", "vault": summary }))
    }

    fn disconnect(&self, params: &Value) -> Result<Value, PluginError> {
        let connection_id = params
            .get("connection")
            .and_then(|c| c.get("id"))
            .and_then(Value::as_str)
            .ok_or_else(|| PluginError::new(-32602, "Missing connection id"))?;
        if let Ok(mut sessions) = self.sessions.lock() {
            sessions.remove(connection_id);
        }
        Ok(json!({ "success": true }))
    }

    fn snapshot(&self, params: &Value) -> Result<Value, PluginError> {
        let key = connection_key(params);
        let mut guard = self
            .sessions
            .lock()
            .map_err(|_| PluginError::new(-32000, "Session registry is poisoned"))?;
        if !guard.contains_key(&key) {
            if key == "local" {
                guard.insert(key.clone(), load_session("", "Local notes", SyncConfig::default())?);
            } else {
                return Err(PluginError::new(-32010, "Open the saved Notes connection first"));
            }
        }
        let session = guard.get(&key).expect("session was inserted");
        Ok(json!({
            "vault": session.vault,
            "settings": session.config.public(),
            "sync": sync_status_value(session),
            "connectionId": if key == "local" { Value::Null } else { json!(params.get("connectionId").and_then(Value::as_str).unwrap_or("")) }
        }))
    }

    fn save_note(&self, params: &Value) -> Result<Value, PluginError> {
        let key = connection_key(params);
        let note_input = params.get("note").ok_or_else(|| PluginError::new(-32602, "Missing note"))?;
        let mut guard = self
            .sessions
            .lock()
            .map_err(|_| PluginError::new(-32000, "Session registry is poisoned"))?;
        let session = guard.get_mut(&key).ok_or_else(|| PluginError::new(-32010, "Open the saved Notes connection first"))?;
        let note = apply_note_input(&mut session.vault, note_input)?;
        session.vault.updated_at = now_ms();
        save_vault(session)?;
        Ok(json!({ "note": note, "savedAt": now_ms() }))
    }

    fn create_note(&self, params: &Value) -> Result<Value, PluginError> {
        let key = connection_key(params);
        let title = params.get("title").and_then(Value::as_str).unwrap_or("Untitled").trim().to_string();
        let parent_id = params.get("parentId").and_then(Value::as_str).filter(|id| *id != "note-welcome").map(str::to_string);
        let mut guard = self
            .sessions
            .lock()
            .map_err(|_| PluginError::new(-32000, "Session registry is poisoned"))?;
        let session = guard.get_mut(&key).ok_or_else(|| PluginError::new(-32010, "Open the saved Notes connection first"))?;
        if let Some(parent) = parent_id.as_deref() {
            if !session.vault.notes.iter().any(|n| n.id == parent && !n.is_deleted()) {
                return Err(PluginError::new(-32602, "Parent page does not exist"));
            }
        }
        let order = next_order(&session.vault, parent_id.as_deref());
        let mut note = Note::new(if title.is_empty() { "Untitled" } else { &title }, "", parent_id, order);
        if let Some(requested) = params.get("id").and_then(Value::as_str) {
            if !requested.trim().is_empty() && !session.vault.notes.iter().any(|item| item.id == requested) {
                note.id = requested.to_string();
            }
        }
        session.vault.notes.push(note.clone());
        session.vault.updated_at = now_ms();
        save_vault(session)?;
        Ok(json!({ "note": note }))
    }

    fn delete_note(&self, params: &Value) -> Result<Value, PluginError> {
        self.set_deleted(params, true)
    }

    fn restore_note(&self, params: &Value) -> Result<Value, PluginError> {
        self.set_deleted(params, false)
    }

    fn set_deleted(&self, params: &Value, deleted: bool) -> Result<Value, PluginError> {
        let key = connection_key(params);
        let id = params.get("id").and_then(Value::as_str).ok_or_else(|| PluginError::new(-32602, "Missing note id"))?;
        let mut guard = self
            .sessions
            .lock()
            .map_err(|_| PluginError::new(-32000, "Session registry is poisoned"))?;
        let session = guard.get_mut(&key).ok_or_else(|| PluginError::new(-32010, "Open the saved Notes connection first"))?;
        let marker = session
            .vault
            .notes
            .iter()
            .find(|n| n.id == id)
            .ok_or_else(|| PluginError::new(-32044, "Note not found"))?
            .deleted_at;
        let now = now_ms();
        // A page moves to the trash together with its subpages, and comes back
        // with the same group, so restoring one page does not strand children.
        let mut group: HashSet<String> = HashSet::new();
        group.insert(id.to_string());
        loop {
            let mut added = false;
            for note in &session.vault.notes {
                if let Some(parent) = note.parent_id.as_deref() {
                    if group.contains(parent) && group.insert(note.id.clone()) {
                        added = true;
                    }
                }
            }
            if !added {
                break;
            }
        }
        let mut result: Option<Note> = None;
        for note in &mut session.vault.notes {
            if !group.contains(&note.id) {
                continue;
            }
            let touched = if deleted {
                if note.deleted_at.is_none() {
                    note.deleted_at = Some(now);
                    true
                } else {
                    false
                }
            } else if note.deleted_at.is_some() && (note.deleted_at == marker || note.id == id) {
                note.deleted_at = None;
                true
            } else {
                false
            };
            if touched {
                note.updated_at = now;
            }
            if note.id == id {
                result = Some(note.clone());
            }
        }
        session.vault.updated_at = now;
        save_vault(session)?;
        Ok(json!({ "note": result }))
    }

    fn purge_note(&self, params: &Value) -> Result<Value, PluginError> {
        let key = connection_key(params);
        let id = params.get("id").and_then(Value::as_str).ok_or_else(|| PluginError::new(-32602, "Missing note id"))?;
        let mut guard = self
            .sessions
            .lock()
            .map_err(|_| PluginError::new(-32000, "Session registry is poisoned"))?;
        let session = guard.get_mut(&key).ok_or_else(|| PluginError::new(-32010, "Open the saved Notes connection first"))?;
        let before = session.vault.notes.len();
        let mut removed = HashSet::new();
        removed.insert(id.to_string());
        // Removing a page also removes its descendants.
        loop {
            let mut added = false;
            for note in &session.vault.notes {
                if let Some(parent) = note.parent_id.as_deref() {
                    if removed.contains(parent) && removed.insert(note.id.clone()) {
                        added = true;
                    }
                }
            }
            if !added {
                break;
            }
        }
        session.vault.notes.retain(|n| !removed.contains(&n.id));
        if session.vault.notes.len() == before {
            return Err(PluginError::new(-32044, "Note not found"));
        }
        session.vault.updated_at = now_ms();
        save_vault(session)?;
        Ok(json!({ "removed": removed.len() }))
    }

    fn purge_trash(&self, params: &Value) -> Result<Value, PluginError> {
        let key = connection_key(params);
        let mut guard = self
            .sessions
            .lock()
            .map_err(|_| PluginError::new(-32000, "Session registry is poisoned"))?;
        let session = guard.get_mut(&key).ok_or_else(|| PluginError::new(-32010, "Open the saved Notes connection first"))?;
        let removed = session.vault.notes.iter().filter(|n| n.is_deleted()).count();
        session.vault.notes.retain(|n| !n.is_deleted());
        session.vault.updated_at = now_ms();
        save_vault(session)?;
        Ok(json!({ "removed": removed }))
    }

    fn move_note(&self, params: &Value) -> Result<Value, PluginError> {
        let key = connection_key(params);
        let id = params.get("id").and_then(Value::as_str).ok_or_else(|| PluginError::new(-32602, "Missing note id"))?.to_string();
        let parent_id = params.get("parentId").and_then(Value::as_str).filter(|parent| *parent != "note-welcome").map(str::to_string);
        let mut guard = self
            .sessions
            .lock()
            .map_err(|_| PluginError::new(-32000, "Session registry is poisoned"))?;
        let session = guard.get_mut(&key).ok_or_else(|| PluginError::new(-32010, "Open the saved Notes connection first"))?;

        if let Some(parent) = parent_id.as_deref() {
            if parent == id {
                return Err(PluginError::new(-32602, "A page cannot be its own parent"));
            }
            if !session.vault.notes.iter().any(|n| n.id == parent && !n.is_deleted()) {
                return Err(PluginError::new(-32602, "Target page does not exist"));
            }
            if is_descendant(&session.vault, parent, &id) {
                return Err(PluginError::new(-32602, "A page cannot be moved into its own child"));
            }
        }

        let order = params
            .get("order")
            .and_then(Value::as_i64)
            .unwrap_or_else(|| next_order(&session.vault, parent_id.as_deref()));
        let note = session
            .vault
            .notes
            .iter_mut()
            .find(|n| n.id == id)
            .ok_or_else(|| PluginError::new(-32044, "Note not found"))?;
        note.parent_id = parent_id;
        note.order = order;
        note.updated_at = now_ms();
        let result = note.clone();
        session.vault.updated_at = now_ms();
        save_vault(session)?;
        Ok(json!({ "note": result }))
    }

    fn reorder_note(&self, params: &Value) -> Result<Value, PluginError> {
        let key = connection_key(params);
        let id = params.get("id").and_then(Value::as_str).ok_or_else(|| PluginError::new(-32602, "Missing note id"))?.to_string();
        let parent_id = params.get("parentId").and_then(Value::as_str).map(str::to_string);
        let index = params.get("index").and_then(Value::as_i64).unwrap_or(i64::MAX);
        let mut guard = self
            .sessions
            .lock()
            .map_err(|_| PluginError::new(-32000, "Session registry is poisoned"))?;
        let session = guard.get_mut(&key).ok_or_else(|| PluginError::new(-32010, "Open the saved Notes connection first"))?;

        if !session.vault.notes.iter().any(|n| n.id == id && !n.is_deleted()) {
            return Err(PluginError::new(-32044, "Note not found"));
        }
        if let Some(parent) = parent_id.as_deref() {
            if parent == id {
                return Err(PluginError::new(-32602, "A page cannot be its own parent"));
            }
            if !session.vault.notes.iter().any(|n| n.id == parent && !n.is_deleted()) {
                return Err(PluginError::new(-32602, "Target page does not exist"));
            }
            if is_descendant(&session.vault, parent, &id) {
                return Err(PluginError::new(-32602, "A page cannot be moved into its own child"));
            }
        }

        let mut siblings: Vec<String> = session
            .vault
            .notes
            .iter()
            .filter(|note| note.id != id && !note.is_deleted() && note.parent_id == parent_id)
            .map(|note| note.id.clone())
            .collect();
        siblings.sort_by(|a, b| {
            let left = session.vault.notes.iter().find(|n| &n.id == a);
            let right = session.vault.notes.iter().find(|n| &n.id == b);
            match (left, right) {
                (Some(left), Some(right)) => left.order.cmp(&right.order).then(left.updated_at.cmp(&right.updated_at)),
                _ => std::cmp::Ordering::Equal,
            }
        });
        let position = index.max(0).min(siblings.len() as i64) as usize;
        siblings.insert(position, id.clone());

        let now = now_ms();
        for (order, sibling) in siblings.iter().enumerate() {
            if let Some(note) = session.vault.notes.iter_mut().find(|n| &n.id == sibling) {
                note.order = order as i64;
                if sibling == &id {
                    note.parent_id = parent_id.clone();
                    note.updated_at = now;
                }
            }
        }
        let result = session
            .vault
            .notes
            .iter()
            .find(|n| n.id == id)
            .cloned()
            .ok_or_else(|| PluginError::new(-32044, "Note not found"))?;
        session.vault.updated_at = now;
        save_vault(session)?;
        Ok(json!({ "note": result }))
    }

    fn attachment_begin(&self, params: &Value) -> Result<Value, PluginError> {
        let key = connection_key(params);
        let name = params.get("name").and_then(Value::as_str).unwrap_or("file").to_string();
        let mime = params.get("mime").and_then(Value::as_str).unwrap_or("application/octet-stream").to_string();
        let size = params.get("size").and_then(Value::as_u64).unwrap_or(0);
        if size > MAX_ATTACHMENT_BYTES {
            return Err(PluginError::new(-32602, "Attachment is too large"));
        }
        let mut guard = self
            .sessions
            .lock()
            .map_err(|_| PluginError::new(-32000, "Session registry is poisoned"))?;
        let session = guard.get_mut(&key).ok_or_else(|| PluginError::new(-32010, "Open the saved Notes connection first"))?;
        let upload_id = new_upload_id();
        let uploads = session.dir.join("uploads");
        fs::create_dir_all(&uploads).map_err(|e| PluginError::new(-32050, format!("Cannot create upload directory: {e}")))?;
        let path = uploads.join(&upload_id);
        fs::write(&path, b"").map_err(|e| PluginError::new(-32050, format!("Cannot create upload: {e}")))?;
        session.uploads.insert(upload_id.clone(), Upload { path, name, mime, size, received: 0 });
        Ok(json!({ "uploadId": upload_id }))
    }

    fn attachment_chunk(&self, params: &Value) -> Result<Value, PluginError> {
        let key = connection_key(params);
        let upload_id = params.get("uploadId").and_then(Value::as_str).ok_or_else(|| PluginError::new(-32602, "Missing uploadId"))?;
        let offset = params.get("offset").and_then(Value::as_u64).unwrap_or(0);
        let encoded = params.get("dataBase64").and_then(Value::as_str).ok_or_else(|| PluginError::new(-32602, "Missing dataBase64"))?;
        let bytes = BASE64.decode(encoded).map_err(|e| PluginError::new(-32602, format!("Invalid attachment chunk: {e}")))?;
        let mut guard = self
            .sessions
            .lock()
            .map_err(|_| PluginError::new(-32000, "Session registry is poisoned"))?;
        let session = guard.get_mut(&key).ok_or_else(|| PluginError::new(-32010, "Open the saved Notes connection first"))?;
        let upload = session.uploads.get_mut(upload_id).ok_or_else(|| PluginError::new(-32044, "Upload not found"))?;
        if offset != upload.received {
            return Err(PluginError::new(-32602, "Attachment chunk offset does not match"));
        }
        let next = upload.received + bytes.len() as u64;
        if next > MAX_ATTACHMENT_BYTES || (upload.size > 0 && next > upload.size) {
            return Err(PluginError::new(-32602, "Attachment is too large"));
        }
        let mut file = fs::OpenOptions::new()
            .append(true)
            .open(&upload.path)
            .map_err(|e| PluginError::new(-32050, format!("Cannot open upload: {e}")))?;
        file.write_all(&bytes).map_err(|e| PluginError::new(-32050, format!("Cannot write upload: {e}")))?;
        upload.received = next;
        Ok(json!({ "received": next }))
    }

    fn attachment_finish(&self, params: &Value) -> Result<Value, PluginError> {
        let key = connection_key(params);
        let upload_id = params.get("uploadId").and_then(Value::as_str).ok_or_else(|| PluginError::new(-32602, "Missing uploadId"))?.to_string();
        let mut guard = self
            .sessions
            .lock()
            .map_err(|_| PluginError::new(-32000, "Session registry is poisoned"))?;
        let session = guard.get_mut(&key).ok_or_else(|| PluginError::new(-32010, "Open the saved Notes connection first"))?;
        let upload = session.uploads.remove(&upload_id).ok_or_else(|| PluginError::new(-32044, "Upload not found"))?;
        if upload.size > 0 && upload.received != upload.size {
            let _ = fs::remove_file(&upload.path);
            return Err(PluginError::new(-32602, "Attachment upload is incomplete"));
        }
        let attachments = attachments_dir(&session.dir);
        fs::create_dir_all(&attachments).map_err(|e| PluginError::new(-32050, format!("Cannot create attachments directory: {e}")))?;
        let id = new_attachment_id();
        let target = attachments.join(&id);
        fs::rename(&upload.path, &target).map_err(|e| PluginError::new(-32050, format!("Cannot store attachment: {e}")))?;
        let attachment = Attachment {
            id: id.clone(),
            name: if upload.name.trim().is_empty() { "file".to_string() } else { upload.name },
            mime: if upload.mime.trim().is_empty() { "application/octet-stream".to_string() } else { upload.mime },
            size: upload.received,
            created_at: now_ms(),
        };
        session.vault.attachments.push(attachment.clone());
        session.vault.updated_at = now_ms();
        save_vault(session)?;
        Ok(json!({ "attachment": attachment }))
    }

    fn attachment_cancel(&self, params: &Value) -> Result<Value, PluginError> {
        let key = connection_key(params);
        let upload_id = params.get("uploadId").and_then(Value::as_str).ok_or_else(|| PluginError::new(-32602, "Missing uploadId"))?;
        let mut guard = self
            .sessions
            .lock()
            .map_err(|_| PluginError::new(-32000, "Session registry is poisoned"))?;
        let session = guard.get_mut(&key).ok_or_else(|| PluginError::new(-32010, "Open the saved Notes connection first"))?;
        if let Some(upload) = session.uploads.remove(upload_id) {
            let _ = fs::remove_file(&upload.path);
        }
        Ok(json!({ "success": true }))
    }

    fn attachment_get(&self, params: &Value) -> Result<Value, PluginError> {
        let key = connection_key(params);
        let id = params.get("id").and_then(Value::as_str).ok_or_else(|| PluginError::new(-32602, "Missing attachment id"))?;
        let max_bytes = params.get("maxBytes").and_then(Value::as_u64).unwrap_or(8 * 1024 * 1024);
        let guard = self
            .sessions
            .lock()
            .map_err(|_| PluginError::new(-32000, "Session registry is poisoned"))?;
        let session = guard.get(&key).ok_or_else(|| PluginError::new(-32010, "Open the saved Notes connection first"))?;
        let attachment = session
            .vault
            .attachments
            .iter()
            .find(|item| item.id == id)
            .cloned()
            .ok_or_else(|| PluginError::new(-32044, "Attachment not found"))?;
        let path = attachments_dir(&session.dir).join(&attachment.id);
        let bytes = fs::read(&path).map_err(|e| PluginError::new(-32050, format!("Cannot read attachment: {e}")))?;
        if bytes.len() as u64 > max_bytes {
            return Ok(json!({ "attachment": attachment, "tooLarge": true }));
        }
        Ok(json!({ "attachment": attachment, "dataBase64": BASE64.encode(bytes) }))
    }

    fn attachment_read(&self, params: &Value) -> Result<Value, PluginError> {
        let key = connection_key(params);
        let id = params.get("id").and_then(Value::as_str).ok_or_else(|| PluginError::new(-32602, "Missing attachment id"))?;
        let offset = params.get("offset").and_then(Value::as_u64).unwrap_or(0);
        let length = params.get("length").and_then(Value::as_u64).unwrap_or(512 * 1024).min(4 * 1024 * 1024);
        let guard = self
            .sessions
            .lock()
            .map_err(|_| PluginError::new(-32000, "Session registry is poisoned"))?;
        let session = guard.get(&key).ok_or_else(|| PluginError::new(-32010, "Open the saved Notes connection first"))?;
        if !session.vault.attachments.iter().any(|item| item.id == id) {
            return Err(PluginError::new(-32044, "Attachment not found"));
        }
        let path = attachments_dir(&session.dir).join(id);
        let mut file = fs::File::open(&path).map_err(|e| PluginError::new(-32050, format!("Cannot read attachment: {e}")))?;
        let size = file.metadata().map_err(|e| PluginError::new(-32050, format!("Cannot inspect attachment: {e}")))?.len();
        if offset > size {
            return Err(PluginError::new(-32602, "Attachment offset is out of range"));
        }
        file.seek(SeekFrom::Start(offset)).map_err(|e| PluginError::new(-32050, format!("Cannot seek attachment: {e}")))?;
        let mut buffer = vec![0u8; length as usize];
        let read = file.read(&mut buffer).map_err(|e| PluginError::new(-32050, format!("Cannot read attachment: {e}")))?;
        buffer.truncate(read);
        let eof = offset + read as u64 >= size;
        Ok(json!({ "dataBase64": BASE64.encode(buffer), "length": read, "eof": eof, "size": size }))
    }

    fn sync_config_get(&self, params: &Value) -> Result<Value, PluginError> {
        let key = connection_key(params);
        let guard = self
            .sessions
            .lock()
            .map_err(|_| PluginError::new(-32000, "Session registry is poisoned"))?;
        let session = guard.get(&key).ok_or_else(|| PluginError::new(-32010, "Open the saved Notes connection first"))?;
        Ok(json!({ "settings": session.config.public() }))
    }

    fn sync_config_save(&self, params: &Value) -> Result<Value, PluginError> {
        let key = connection_key(params);
        let mut guard = self
            .sessions
            .lock()
            .map_err(|_| PluginError::new(-32000, "Session registry is poisoned"))?;
        let session = guard.get_mut(&key).ok_or_else(|| PluginError::new(-32010, "Open the saved Notes connection first"))?;
        let text = |name: &str, current: &str| {
            params.get(name).and_then(Value::as_str).map(|value| value.trim().to_string()).unwrap_or_else(|| current.to_string())
        };
        let mode = params.get("mode").and_then(Value::as_str).unwrap_or(&session.config.mode).to_lowercase();
        if !["none", "webdav", "git"].contains(&mode.as_str()) {
            return Err(PluginError::new(-32602, "Unsupported sync mode"));
        }
        session.config.mode = mode;
        session.config.webdav_url = text("webdavUrl", &session.config.webdav_url);
        session.config.webdav_username = text("webdavUsername", &session.config.webdav_username);
        session.config.git_remote = text("gitRemote", &session.config.git_remote);
        session.config.git_branch = text("gitBranch", &session.config.git_branch);
        session.config.git_username = text("gitUsername", &session.config.git_username);
        session.config.git_author_name = text("gitAuthorName", &session.config.git_author_name);
        session.config.git_author_email = text("gitAuthorEmail", &session.config.git_author_email);
        if let Some(value) = params.get("webdavPassword").and_then(Value::as_str) {
            if !value.is_empty() {
                session.config.webdav_password = value.to_string();
            }
        }
        if params.get("clearWebdavPassword").and_then(Value::as_bool).unwrap_or(false) {
            session.config.webdav_password.clear();
        }
        if let Some(value) = params.get("gitToken").and_then(Value::as_str) {
            if !value.is_empty() {
                session.config.git_token = value.to_string();
            }
        }
        if params.get("clearGitToken").and_then(Value::as_bool).unwrap_or(false) {
            session.config.git_token.clear();
        }
        if let Some(value) = params.get("autoSyncMinutes").and_then(Value::as_u64) {
            session.config.auto_sync_minutes = value.min(1440);
        }
        if let Some(value) = params.get("exportMarkdown").and_then(Value::as_bool) {
            session.config.export_markdown = value;
        }
        if let Some(value) = params.get("connectTimeoutSecs").and_then(Value::as_u64) {
            session.config.connect_timeout_secs = value.clamp(5, 600);
        }
        if session.config.git_branch.trim().is_empty() {
            session.config.git_branch = DEFAULT_BRANCH.to_string();
        }
        if session.config.git_username.trim().is_empty() {
            session.config.git_username = "git".to_string();
        }
        save_sync_override(session)?;
        Ok(json!({ "settings": session.config.public() }))
    }

    fn sync_config_reset(&self, params: &Value) -> Result<Value, PluginError> {
        let key = connection_key(params);
        let mut guard = self
            .sessions
            .lock()
            .map_err(|_| PluginError::new(-32000, "Session registry is poisoned"))?;
        let session = guard.get_mut(&key).ok_or_else(|| PluginError::new(-32010, "Open the saved Notes connection first"))?;
        let _ = fs::remove_file(session.dir.join("sync-config.json"));
        session.config = session.base_config.clone();
        Ok(json!({ "settings": session.config.public() }))
    }

    fn sync_status(&self, params: &Value) -> Result<Value, PluginError> {
        let key = connection_key(params);
        let guard = self
            .sessions
            .lock()
            .map_err(|_| PluginError::new(-32000, "Session registry is poisoned"))?;
        let session = guard.get(&key).ok_or_else(|| PluginError::new(-32010, "Open the saved Notes connection first"))?;
        Ok(json!({ "sync": sync_status_value(session), "settings": session.config.public() }))
    }

    fn sync_now(&self, params: &Value, emitter: &PluginEmitter) -> Result<Value, PluginError> {
        let key = connection_key(params);
        let mut guard = self
            .sessions
            .lock()
            .map_err(|_| PluginError::new(-32000, "Session registry is poisoned"))?;
        let session = guard.get_mut(&key).ok_or_else(|| PluginError::new(-32010, "Open the saved Notes connection first"))?;
        let report = match session.config.mode.as_str() {
            "webdav" => sync_webdav(session)?,
            "git" => sync_git(session)?,
            _ => {
                return Ok(json!({
                    "ok": true,
                    "mode": "none",
                    "message": "Sync is not configured for this connection.",
                    "pulled": 0,
                    "pushed": 0,
                    "conflicts": 0,
                    "at": now_ms()
                }))
            }
        };
        let result = json!({
            "ok": true,
            "mode": report.mode,
            "message": report.message,
            "pulled": report.pulled,
            "pushed": report.pushed,
            "conflicts": report.conflicts,
            "attachmentsUploaded": report.attachments_uploaded,
            "attachmentsDownloaded": report.attachments_downloaded,
            "at": report.at
        });
        let _ = emitter.event("notes/sync", result.clone());
        Ok(result)
    }
}

// ---------------------------------------------------------------------------
// Vault persistence
// ---------------------------------------------------------------------------

fn detach_welcome_children(vault: &mut Vault) -> bool {
    let mut changed = false;
    for note in &mut vault.notes {
        if note.parent_id.as_deref() == Some("note-welcome") {
            note.parent_id = None;
            changed = true;
        }
    }
    if changed {
        vault.updated_at = now_ms();
    }
    changed
}

fn attachments_dir(dir: &Path) -> PathBuf {
    dir.join(MIRROR_DIR).join("attachments")
}

fn migrate_attachments(dir: &Path) {
    let old = dir.join("attachments");
    if !old.is_dir() {
        return;
    }
    let new = attachments_dir(dir);
    let _ = fs::create_dir_all(&new);
    if let Ok(entries) = fs::read_dir(&old) {
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_file() {
                continue;
            }
            let target = new.join(entry.file_name());
            if target.exists() {
                let _ = fs::remove_file(&path);
            } else if fs::rename(&path, &target).is_err() {
                let _ = fs::copy(&path, &target).and_then(|_| fs::remove_file(&path));
            }
        }
    }
    let _ = fs::remove_dir(&old);
}

fn load_session(connection_id: &str, name: &str, config: SyncConfig) -> Result<Session, PluginError> {
    let dir = vault_directory(connection_id);
    fs::create_dir_all(&dir).map_err(|e| PluginError::new(-32050, format!("Cannot create vault directory: {e}")))?;
    migrate_attachments(&dir);
    let vault_path = dir.join("vault.json");
    let existed = vault_path.exists();
    let vault = if existed {
        match fs::read_to_string(&vault_path).ok().and_then(|text| serde_json::from_str::<Vault>(&text).ok()) {
            Some(vault) => vault,
            None => {
                let backup = dir.join(format!("vault.corrupt-{}.json", now_ms()));
                let _ = fs::rename(&vault_path, &backup);
                Vault::new(name)
            }
        }
    } else {
        Vault::new(name)
    };
    let base_config = config;
    let config = load_sync_override(&dir).unwrap_or_else(|| base_config.clone());
    let mut session = Session { dir, config, base_config, vault, state: SyncState::default(), uploads: HashMap::new() };
    session.state = load_state(&session.dir)?;
    if detach_welcome_children(&mut session.vault) {
        save_vault(&mut session)?;
    }
    if !existed {
        seed_vault(&mut session.vault, &session.config);
        save_vault(&mut session)?;
    }
    if session.vault.name.is_empty() {
        session.vault.name = name.to_string();
    }
    Ok(session)
}

fn seed_vault(vault: &mut Vault, config: &SyncConfig) {
    vault.name = "Notes".into();
    let sync_hint = match config.mode.as_str() {
        "webdav" => "Wi-Fi 同步已切换为 WebDAV。点击工具栏的同步按钮即可上传笔记。",
        "git" => "Wi-Fi 同步已切换为 Git。点击工具栏的同步按钮即可提交并推送笔记。",
        _ => "当前是本地笔记库。在连接设置中启用 WebDAV 或 Git 同步后，点击工具栏的同步按钮即可多设备同步。",
    };
    let body = format!(
        "# 欢迎使用 DBX Notes\n\n这是一个轻量、Notion 风格的 Markdown 笔记库。\n\n- 左侧新建页面、拖出层级（通过页面菜单移动）\n- 正文支持 Markdown，编辑与预览实时切换\n- 支持收藏、标签、搜索和回收站\n- {sync_hint}\n\n> 所有内容都保存在 DBX 插件数据目录中，同步时会上传 `vault.json` 和可读的 `DBX-notes/*.md` 镜像。\n"
    );
    let mut welcome = Note::new("欢迎使用 DBX Notes", body, None, 0);
    // A stable id keeps the welcome page from duplicating on every device.
    welcome.id = "note-welcome".to_string();
    vault.notes.push(welcome);
}

fn save_vault(session: &mut Session) -> Result<(), PluginError> {
    let text = serde_json::to_string_pretty(&session.vault)
        .map_err(|e| PluginError::new(-32000, format!("Cannot encode vault: {e}")))?;
    write_atomic(&session.dir.join("vault.json"), text.as_bytes())
}

fn load_sync_override(dir: &Path) -> Option<SyncConfig> {
    let text = fs::read_to_string(dir.join("sync-config.json")).ok()?;
    serde_json::from_str::<SyncConfig>(&text).ok()
}

fn save_sync_override(session: &Session) -> Result<(), PluginError> {
    let text = serde_json::to_string_pretty(&session.config)
        .map_err(|e| PluginError::new(-32000, format!("Cannot encode sync config: {e}")))?;
    let path = session.dir.join("sync-config.json");
    write_atomic(&path, text.as_bytes())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(&path, fs::Permissions::from_mode(0o600));
    }
    Ok(())
}

fn load_state(dir: &Path) -> Result<SyncState, PluginError> {
    let path = dir.join("sync-state.json");
    match fs::read_to_string(&path) {
        Ok(text) => Ok(serde_json::from_str(&text).unwrap_or_default()),
        Err(_) => Ok(SyncState::default()),
    }
}

fn save_state(session: &Session) -> Result<(), PluginError> {
    let text = serde_json::to_string_pretty(&session.state)
        .map_err(|e| PluginError::new(-32000, format!("Cannot encode sync state: {e}")))?;
    write_atomic(&session.dir.join("sync-state.json"), text.as_bytes())
}

fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), PluginError> {
    let tmp = path.with_extension(format!("tmp{}", std::process::id()));
    fs::write(&tmp, bytes).map_err(|e| PluginError::new(-32050, format!("Cannot write {}: {e}", path.display())))?;
    replace_file(&tmp, path).map_err(|e| PluginError::new(-32050, format!("Cannot replace {}: {e}", path.display())))
}

#[cfg(windows)]
fn replace_file(tmp: &Path, path: &Path) -> std::io::Result<()> {
    // Windows cannot rename over an existing file.
    let _ = fs::remove_file(path);
    fs::rename(tmp, path)
}

#[cfg(not(windows))]
fn replace_file(tmp: &Path, path: &Path) -> std::io::Result<()> {
    fs::rename(tmp, path)
}

fn vault_directory(connection_id: &str) -> PathBuf {
    let data_dir = std::env::var("DBX_PLUGIN_DATA_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")).join(".dbx-notes-data"));
    if connection_id.trim().is_empty() {
        return data_dir.join("vaults").join("local");
    }
    let mut safe = String::new();
    for ch in connection_id.chars().take(64) {
        if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' || ch == '.' {
            safe.push(ch);
        } else {
            safe.push('_');
        }
    }
    if safe.is_empty() {
        safe.push_str("vault");
    }
    data_dir.join("vaults").join(format!("{safe}-{:016x}", fnv1a(connection_id.as_bytes())))
}

fn connection_key(params: &Value) -> String {
    match params.get("connectionId").and_then(Value::as_str) {
        Some(id) if !id.trim().is_empty() => id.to_string(),
        _ => "local".to_string(),
    }
}

fn sync_status_value(session: &Session) -> Value {
    json!({
        "lastSyncAt": session.state.last_sync_at,
        "lastResult": session.state.last_result,
        "mode": session.config.mode,
        "autoSyncMinutes": session.config.auto_sync_minutes,
        "config": session.config.public()
    })
}

// ---------------------------------------------------------------------------
// Note operations
// ---------------------------------------------------------------------------

fn apply_note_input(vault: &mut Vault, input: &Value) -> Result<Note, PluginError> {
    let id = input.get("id").and_then(Value::as_str).ok_or_else(|| PluginError::new(-32602, "Missing note id"))?;
    let index = vault
        .notes
        .iter()
        .position(|n| n.id == id)
        .ok_or_else(|| PluginError::new(-32044, "Note not found"))?;
    let now = now_ms();

    let parent_id = match input.get("parentId") {
        Some(Value::Null) => None,
        Some(Value::String(value)) if value.trim().is_empty() => None,
        Some(Value::String(value)) => Some(value.clone()),
        Some(_) => return Err(PluginError::new(-32602, "parentId must be a string or null")),
        None => vault.notes[index].parent_id.clone(),
    };
    if let Some(parent) = parent_id.as_deref() {
        if parent == id {
            return Err(PluginError::new(-32602, "A page cannot be its own parent"));
        }
        if !vault.notes.iter().any(|n| n.id == parent && !n.is_deleted()) {
            return Err(PluginError::new(-32602, "Parent page does not exist"));
        }
        if is_descendant(vault, parent, id) {
            return Err(PluginError::new(-32602, "A page cannot be moved into its own child"));
        }
    }

    let body = match input.get("body") {
        Some(Value::String(value)) => value.clone(),
        _ => vault.notes[index].body.clone(),
    };
    if body.len() > MAX_NOTE_BYTES {
        return Err(PluginError::new(-32602, "A single note cannot exceed 7 MB"));
    }
    let title = match input.get("title").and_then(Value::as_str) {
        Some(value) => {
            let trimmed = value.trim();
            if trimmed.is_empty() { "Untitled".to_string() } else { trimmed.to_string() }
        }
        None => vault.notes[index].title.clone(),
    };

    let note = &mut vault.notes[index];
    note.title = title;
    note.body = body;
    note.parent_id = parent_id;
    if let Some(order) = input.get("order").and_then(Value::as_i64) {
        note.order = order;
    }
    if let Some(tags) = input.get("tags").and_then(Value::as_array) {
        let mut seen = HashSet::new();
        note.tags = tags
            .iter()
            .filter_map(Value::as_str)
            .map(|tag| tag.trim().trim_start_matches('#').to_string())
            .filter(|tag| !tag.is_empty() && tag.len() <= 48 && seen.insert(tag.clone()))
            .take(32)
            .collect();
    }
    if let Some(favorite) = input.get("favorite").and_then(Value::as_bool) {
        note.favorite = favorite;
    }
    if let Some(icon) = input.get("icon") {
        note.icon = icon.as_str().map(str::to_string);
    }
    note.updated_at = now;
    Ok(note.clone())
}

fn next_order(vault: &Vault, parent_id: Option<&str>) -> i64 {
    vault
        .notes
        .iter()
        .filter(|n| n.parent_id.as_deref() == parent_id)
        .map(|n| n.order)
        .max()
        .map(|value| value + 1)
        .unwrap_or(0)
}

fn is_descendant(vault: &Vault, candidate: &str, ancestor: &str) -> bool {
    let mut current = Some(candidate.to_string());
    let mut guard = 0;
    while let Some(id) = current {
        if id == ancestor {
            return true;
        }
        guard += 1;
        if guard > 256 {
            return true;
        }
        current = vault.notes.iter().find(|n| n.id == id).and_then(|n| n.parent_id.clone());
    }
    false
}

// ---------------------------------------------------------------------------
// Merge — the decision table, fingerprints and unit tests live in merge.rs
// ---------------------------------------------------------------------------

use merge::{attachment_state_snapshot, content_rev, merge_vaults, note_rev, state_snapshot, MergeStats};

fn refresh_sync_state(state: &mut SyncState, vault: &Vault) {
    state.notes = state_snapshot(vault);
    state.attachments = attachment_state_snapshot(vault);
}

/// A mirror only depends on content, so timestamps must not trigger re-uploads.
fn mirror_unchanged(state: &NoteState, note: &Note) -> bool {
    let same_rev = match state.content_rev.as_deref() {
        Some(rev) => rev == content_rev(note),
        None => state.rev == note_rev(note),
    };
    same_rev && state.mirror.as_deref() == Some(mirror_name(note).as_str())
}

// ---------------------------------------------------------------------------
// WebDAV
// ---------------------------------------------------------------------------

struct SyncReport {
    mode: &'static str,
    message: String,
    pulled: u32,
    pushed: u32,
    conflicts: u32,
    attachments_uploaded: u32,
    attachments_downloaded: u32,
    at: i64,
}

fn sync_webdav(session: &mut Session) -> Result<SyncReport, PluginError> {
    if session.config.webdav_url.trim().is_empty() {
        return Err(PluginError::new(-32602, "WebDAV URL is empty"));
    }
    let base = webdav_base(&session.config.webdav_url)?;
    let client = Client::builder()
        .timeout(Duration::from_secs(session.config.connect_timeout_secs.max(10)))
        .user_agent("DBX-Notes/0.1")
        .build()
        .map_err(|e| PluginError::new(-32050, format!("Cannot create HTTP client: {e}")))?;

    ensure_collections(&client, &base, &session.config)?;
    let vault_url = base.join("vault.json").map_err(|e| PluginError::new(-32050, e.to_string()))?;

    let mut last_error: Option<PluginError> = None;
    for _attempt in 0..3 {
        let (remote, etag) = webdav_get_vault(&client, &vault_url, &session.config)?;
        let (merged, mut stats) = match remote.as_ref() {
            Some(remote) => merge_vaults(&session.vault, remote, &session.state),
            None => (session.vault.clone(), MergeStats::default()),
        };
        if remote.is_none() {
            stats.pushed = merged.notes.iter().filter(|note| !note.is_deleted()).count() as u32;
        }
        session.vault = merged;
        session.vault.updated_at = now_ms();
        save_vault(session)?;

        let body = serde_json::to_vec_pretty(&session.vault)
            .map_err(|e| PluginError::new(-32000, format!("Cannot encode vault: {e}")))?;
        let mut request = client
            .put(vault_url.clone())
            .header(CONTENT_TYPE, "application/json; charset=utf-8")
            .body(body);
        if let Some(etag) = etag.as_deref() {
            request = request.header(IF_MATCH, etag);
        }
        if !session.config.webdav_username.is_empty() {
            request = request.basic_auth(&session.config.webdav_username, Some(&session.config.webdav_password));
        }
        let response = request.send().map_err(|e| PluginError::new(-32050, format!("WebDAV upload failed: {e}")))?;
        if response.status() == StatusCode::PRECONDITION_FAILED {
            last_error = Some(PluginError::new(-32053, "The remote vault changed during sync; retrying."));
            continue;
        }
        if !response.status().is_success() {
            return Err(PluginError::new(-32052, format!("WebDAV upload failed with HTTP {}", response.status())));
        }

        let (attachments_uploaded, attachments_downloaded) = sync_webdav_attachments(&client, &base, session)?;
        if session.config.export_markdown {
            sync_webdav_mirrors(&client, &base, session)?;
        }
        refresh_sync_state(&mut session.state, &session.vault);
        session.state.last_sync_at = Some(now_ms());
        session.state.last_result = Some(format!(
            "WebDAV ok · {} downloaded · {} uploaded · {} conflicts · {} attachments",
            stats.pulled, stats.pushed, stats.conflicts, attachments_uploaded + attachments_downloaded
        ));
        save_state(session)?;
        return Ok(SyncReport {
            mode: "webdav",
            message: format!(
                "Synced with WebDAV. Downloaded {}, uploaded {}, conflicts {}, attachments {}.",
                stats.pulled, stats.pushed, stats.conflicts, attachments_uploaded + attachments_downloaded
            ),
            pulled: stats.pulled,
            pushed: stats.pushed,
            conflicts: stats.conflicts,
            attachments_uploaded,
            attachments_downloaded,
            at: now_ms(),
        });
    }
    Err(last_error.unwrap_or_else(|| PluginError::new(-32053, "WebDAV sync did not converge")))
}

fn webdav_base(input: &str) -> Result<Url, PluginError> {
    let mut url = Url::parse(input.trim()).map_err(|e| PluginError::new(-32602, format!("Invalid WebDAV URL: {e}")))?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(PluginError::new(-32602, "WebDAV URL must start with http:// or https://"));
    }
    if !url.path().ends_with('/') {
        let path = format!("{}/", url.path());
        url.set_path(&path);
    }
    Ok(url)
}

fn webdav_get_vault(
    client: &Client,
    vault_url: &Url,
    config: &SyncConfig,
) -> Result<(Option<Vault>, Option<String>), PluginError> {
    let mut request = client.get(vault_url.clone());
    if !config.webdav_username.is_empty() {
        request = request.basic_auth(&config.webdav_username, Some(&config.webdav_password));
    }
    let response = request.send().map_err(|e| PluginError::new(-32050, format!("Cannot reach WebDAV server: {e}")))?;
    let status = response.status();
    if status == StatusCode::NOT_FOUND {
        return Ok((None, None));
    }
    if status == StatusCode::UNAUTHORIZED || status == StatusCode::FORBIDDEN {
        return Err(PluginError::new(-32051, "WebDAV authentication failed (check the username and password)"));
    }
    if !status.is_success() {
        return Err(PluginError::new(-32052, format!("WebDAV download failed with HTTP {status}")));
    }
    let etag = response
        .headers()
        .get(ETAG)
        .and_then(|value| value.to_str().ok())
        .map(str::to_string);
    let text = response.text().map_err(|e| PluginError::new(-32050, format!("Cannot read WebDAV response: {e}")))?;
    let vault = serde_json::from_str::<Vault>(&text)
        .map_err(|e| PluginError::new(-32054, format!("Remote vault.json is not a DBX Notes vault: {e}")))?;
    Ok((Some(vault), etag))
}

fn ensure_collections(client: &Client, base: &Url, config: &SyncConfig) -> Result<(), PluginError> {
    let origin = base.origin().ascii_serialization();
    let path = base.path().trim_end_matches('/');
    let mut prefix = String::new();
    for segment in path.split('/').filter(|segment| !segment.is_empty()) {
        prefix.push('/');
        prefix.push_str(segment);
        let url = Url::parse(&format!("{origin}{prefix}/")).map_err(|e| PluginError::new(-32602, e.to_string()))?;
        let mut request = client.request(Method::from_bytes(b"MKCOL").expect("MKCOL is valid"), url.clone());
        if !config.webdav_username.is_empty() {
            request = request.basic_auth(&config.webdav_username, Some(&config.webdav_password));
        }
        let response = request.send().map_err(|e| PluginError::new(-32050, format!("WebDAV MKCOL failed: {e}")))?;
        let status = response.status();
        if status.is_success()
            || status == StatusCode::METHOD_NOT_ALLOWED
            || status == StatusCode::CONFLICT
            || status == StatusCode::MOVED_PERMANENTLY
            || status == StatusCode::FOUND
        {
            continue;
        }
        if status == StatusCode::UNAUTHORIZED || status == StatusCode::FORBIDDEN {
            return Err(PluginError::new(-32051, "WebDAV authentication failed"));
        }
        return Err(PluginError::new(-32052, format!("WebDAV cannot create folder {url}: HTTP {status}")));
    }
    Ok(())
}

fn sync_webdav_attachments(client: &Client, base: &Url, session: &Session) -> Result<(u32, u32), PluginError> {
    if session.vault.attachments.is_empty() {
        return Ok((0, 0));
    }
    let mut uploaded = 0u32;
    let mut downloaded = 0u32;
    let parent = base.join(&format!("{MIRROR_DIR}/")).map_err(|e| PluginError::new(-32050, e.to_string()))?;
    let dir = base.join(&format!("{MIRROR_DIR}/attachments/")).map_err(|e| PluginError::new(-32050, e.to_string()))?;
    for target in [parent, dir.clone()] {
        let mut mkcol = client.request(Method::from_bytes(b"MKCOL").expect("MKCOL is valid"), target);
        if !session.config.webdav_username.is_empty() {
            mkcol = mkcol.basic_auth(&session.config.webdav_username, Some(&session.config.webdav_password));
        }
        let _ = mkcol.send();
    }
    let local_dir = attachments_dir(&session.dir);
    for attachment in &session.vault.attachments {
        let local = local_dir.join(&attachment.id);
        let url = dir.join(&attachment.id).map_err(|e| PluginError::new(-32050, e.to_string()))?;
        if local.exists() {
            let bytes = fs::read(&local).map_err(|e| PluginError::new(-32050, format!("Cannot read attachment: {e}")))?;
            let mut request = client
                .put(url)
                .header(CONTENT_TYPE, if attachment.mime.is_empty() { "application/octet-stream" } else { attachment.mime.as_str() })
                .body(bytes);
            if !session.config.webdav_username.is_empty() {
                request = request.basic_auth(&session.config.webdav_username, Some(&session.config.webdav_password));
            }
            let response = request.send().map_err(|e| PluginError::new(-32050, format!("WebDAV attachment upload failed: {e}")))?;
            if !response.status().is_success() && response.status() != StatusCode::PRECONDITION_FAILED {
                return Err(PluginError::new(-32052, format!("WebDAV attachment upload failed with HTTP {}", response.status())));
            }
            uploaded += 1;
        } else {
            let mut response = {
                let mut request = client.get(url);
                if !session.config.webdav_username.is_empty() {
                    request = request.basic_auth(&session.config.webdav_username, Some(&session.config.webdav_password));
                }
                request.send().map_err(|e| PluginError::new(-32050, format!("WebDAV attachment download failed: {e}")))?
            };
            if response.status() == StatusCode::NOT_FOUND {
                let legacy = base.join(&format!("attachments/{}", attachment.id)).map_err(|e| PluginError::new(-32050, e.to_string()))?;
                let mut request = client.get(legacy);
                if !session.config.webdav_username.is_empty() {
                    request = request.basic_auth(&session.config.webdav_username, Some(&session.config.webdav_password));
                }
                response = request.send().map_err(|e| PluginError::new(-32050, format!("WebDAV attachment download failed: {e}")))?;
            }
            if response.status().is_success() {
                let bytes = response.bytes().map_err(|e| PluginError::new(-32050, format!("Cannot read attachment response: {e}")))?;
                fs::create_dir_all(&local_dir).map_err(|e| PluginError::new(-32050, format!("Cannot create attachments directory: {e}")))?;
                fs::write(&local, &bytes).map_err(|e| PluginError::new(-32050, format!("Cannot write attachment: {e}")))?;
                downloaded += 1;
            } else if response.status() != StatusCode::NOT_FOUND {
                return Err(PluginError::new(-32052, format!("WebDAV attachment download failed with HTTP {}", response.status())));
            }
        }
    }
    Ok((uploaded, downloaded))
}

fn sync_webdav_mirrors(client: &Client, base: &Url, session: &mut Session) -> Result<(), PluginError> {
    let notes_dir = base.join(&format!("{MIRROR_DIR}/")).map_err(|e| PluginError::new(-32050, e.to_string()))?;
    let _ = client
        .request(Method::from_bytes(b"MKCOL").expect("MKCOL is valid"), notes_dir.clone())
        .basic_auth(&session.config.webdav_username, Some(&session.config.webdav_password))
        .send();

    let mut wanted: HashSet<String> = HashSet::new();
    for note in &session.vault.notes {
        if note.is_deleted() {
            continue;
        }
        let name = mirror_name(note);
        wanted.insert(name.clone());
        let previous = session.state.notes.get(&note.id);
        if previous.map_or(false, |state| mirror_unchanged(state, note)) {
            continue;
        }
        if let Some(old) = previous.and_then(|state| state.mirror.as_deref()) {
            if old != name {
                let _ = delete_webdav(client, base, old, &session.config);
            }
        }
        let url = base.join(&name).map_err(|e| PluginError::new(-32050, e.to_string()))?;
        let mut request = client
            .put(url)
            .header(CONTENT_TYPE, "text/markdown; charset=utf-8")
            .body(mirror_markdown(note));
        if !session.config.webdav_username.is_empty() {
            request = request.basic_auth(&session.config.webdav_username, Some(&session.config.webdav_password));
        }
        let response = request.send().map_err(|e| PluginError::new(-32050, format!("Mirror upload failed: {e}")))?;
        if !response.status().is_success() {
            return Err(PluginError::new(-32052, format!("Mirror upload failed with HTTP {}", response.status())));
        }
    }

    // `wanted` is the full set of mirrors the merged vault needs. Anything
    // recorded in state but no longer wanted is stale (deleted page, renamed
    // title, hard-deleted page) and can be removed on the remote as well.
    for state in session.state.notes.values() {
        if let Some(old) = state.mirror.as_deref() {
            if !wanted.contains(old) {
                let _ = delete_webdav(client, base, old, &session.config);
            }
        }
    }
    Ok(())
}

fn delete_webdav(client: &Client, base: &Url, name: &str, config: &SyncConfig) -> Result<(), PluginError> {
    let url = base.join(name).map_err(|e| PluginError::new(-32050, e.to_string()))?;
    let mut request = client.delete(url);
    if !config.webdav_username.is_empty() {
        request = request.basic_auth(&config.webdav_username, Some(&config.webdav_password));
    }
    let _ = request.send();
    Ok(())
}

// ---------------------------------------------------------------------------
// Git
// ---------------------------------------------------------------------------

fn sync_git(session: &mut Session) -> Result<SyncReport, PluginError> {
    if session.config.git_remote.trim().is_empty() {
        return Err(PluginError::new(-32602, "Git remote is empty"));
    }
    let dir = session.dir.clone();
    ensure_git_repo(session)?;

    let branch = session.config.git_branch.clone();
    let mut last_error: Option<PluginError> = None;

    for _attempt in 0..3 {
        commit_all(session, "DBX Notes: local snapshot")?;
        let has_remote = fetch_remote(session)?;
        let remote_vault = if has_remote { git_show_vault(&dir, &branch, &session.config)? } else { None };

        let (merged, mut stats) = match remote_vault.as_ref() {
            Some(remote) => merge_vaults(&session.vault, remote, &session.state),
            None => (session.vault.clone(), MergeStats::default()),
        };
        if remote_vault.is_none() {
            stats.pushed = merged.notes.iter().filter(|note| !note.is_deleted()).count() as u32;
        }
        session.vault = merged;
        session.vault.updated_at = now_ms();

        let attachment_backup = backup_attachments(session)?;
        if has_remote {
            run_git(&dir, &session.config, &["reset", "--hard", &format!("origin/{branch}")])?;
        }
        restore_attachments(session, &attachment_backup)?;
        save_vault(session)?;
        write_git_mirrors(session)?;
        commit_all(session, "DBX Notes: sync")?;

        let push = Command::new("git")
            .current_dir(&dir)
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_ASKPASS", askpass_program())
            .env("DBX_NOTES_ASKPASS", "1")
            .env("DBX_GIT_USERNAME", &session.config.git_username)
            .env("DBX_GIT_TOKEN", &session.config.git_token)
            .args(["push", "origin", &format!("HEAD:{branch}")])
            .output()
            .map_err(|e| PluginError::new(-32050, format!("Cannot run git: {e}")))?;
        if push.status.success() {
            refresh_sync_state(&mut session.state, &session.vault);
            session.state.last_sync_at = Some(now_ms());
            session.state.last_result = Some(format!(
                "Git ok · {} downloaded · {} uploaded · {} conflicts · {} attachments",
                stats.pulled, stats.pushed, stats.conflicts, session.vault.attachments.len()
            ));
            save_state(session)?;
            return Ok(SyncReport {
                mode: "git",
                message: format!(
                    "Synced with Git. Downloaded {}, uploaded {}, conflicts {}, attachments {}.",
                    stats.pulled, stats.pushed, stats.conflicts, session.vault.attachments.len()
                ),
                pulled: stats.pulled,
                pushed: stats.pushed,
                conflicts: stats.conflicts,
                attachments_uploaded: session.vault.attachments.len() as u32,
                attachments_downloaded: 0,
                at: now_ms(),
            });
        }
        let stderr = String::from_utf8_lossy(&push.stderr).trim().to_string();
        last_error = Some(PluginError::new(-32055, format!("Git push failed: {}", clean_git_error(&stderr))));
        if is_auth_error(&stderr) {
            break;
        }
    }

    if let Some(error) = &last_error {
        session.state.last_result = Some(error.message.clone());
        let _ = save_state(session);
    }
    Err(last_error.unwrap_or_else(|| PluginError::new(-32055, "Git sync did not converge")))
}

fn backup_attachments(session: &Session) -> Result<PathBuf, PluginError> {
    let backup = session.dir.join(format!(".attachments-backup-{}", std::process::id()));
    let _ = fs::remove_dir_all(&backup);
    fs::create_dir_all(&backup).map_err(|e| PluginError::new(-32050, format!("Cannot create attachment backup: {e}")))?;
    let source = attachments_dir(&session.dir);
    for attachment in &session.vault.attachments {
        let path = source.join(&attachment.id);
        if path.exists() {
            fs::copy(&path, backup.join(&attachment.id))
                .map_err(|e| PluginError::new(-32050, format!("Cannot back up attachment: {e}")))?;
        }
    }
    Ok(backup)
}

fn restore_attachments(session: &Session, backup: &Path) -> Result<(), PluginError> {
    let target = attachments_dir(&session.dir);
    fs::create_dir_all(&target).map_err(|e| PluginError::new(-32050, format!("Cannot create attachments directory: {e}")))?;
    for attachment in &session.vault.attachments {
        let path = target.join(&attachment.id);
        let saved = backup.join(&attachment.id);
        if !path.exists() && saved.exists() {
            fs::copy(&saved, &path).map_err(|e| PluginError::new(-32050, format!("Cannot restore attachment: {e}")))?;
        }
    }
    let _ = fs::remove_dir_all(backup);
    Ok(())
}

fn ensure_git_repo(session: &mut Session) -> Result<(), PluginError> {
    let dir = session.dir.clone();
    if !dir.join(".git").exists() {
        let initialized = Command::new("git").current_dir(&dir).args(["init", "-b", "main"]).output();
        if !matches!(&initialized, Ok(out) if out.status.success()) {
            run_git(&dir, &session.config, &["init"])?;
            let _ = run_git(&dir, &session.config, &["symbolic-ref", "HEAD", "refs/heads/main"]);
        }
    }
    let ignore = "sync-state.json\nsync-state.json.tmp\nsync-config.json\naskpass.sh\n.DS_Store\n*.tmp\nuploads/\n.attachments-backup-*\n";
    fs::write(dir.join(".gitignore"), ignore).map_err(|e| PluginError::new(-32050, format!("Cannot write .gitignore: {e}")))?;
    run_git(&dir, &session.config, &["config", "user.name", &session.config.git_author_name])?;
    run_git(&dir, &session.config, &["config", "user.email", &session.config.git_author_email])?;

    let remotes = run_git(&dir, &session.config, &["remote"])?;
    if remotes.lines().any(|line| line.trim() == "origin") {
        run_git(&dir, &session.config, &["remote", "set-url", "origin", &session.config.git_remote])?;
    } else {
        run_git(&dir, &session.config, &["remote", "add", "origin", &session.config.git_remote])?;
    }
    Ok(())
}

fn fetch_remote(session: &Session) -> Result<bool, PluginError> {
    let dir = &session.dir;
    let branch = &session.config.git_branch;
    let output = Command::new("git")
        .current_dir(dir)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_ASKPASS", askpass_program())
        .env("DBX_NOTES_ASKPASS", "1")
        .env("DBX_GIT_USERNAME", &session.config.git_username)
        .env("DBX_GIT_TOKEN", &session.config.git_token)
        .args(["fetch", "--prune", "origin", branch])
        .output()
        .map_err(|e| PluginError::new(-32050, format!("Cannot run git: {e}")))?;
    if output.status.success() {
        return Ok(true);
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    if stderr.contains("couldn't find remote ref") || stderr.contains("could not find remote ref") {
        return Ok(false);
    }
    Err(PluginError::new(-32055, format!("Git fetch failed: {}", clean_git_error(stderr.trim()))))
}

fn git_show_vault(dir: &Path, branch: &str, _config: &SyncConfig) -> Result<Option<Vault>, PluginError> {
    let output = Command::new("git")
        .current_dir(dir)
        .args(["show", &format!("origin/{branch}:vault.json")])
        .output()
        .map_err(|e| PluginError::new(-32050, format!("Cannot run git: {e}")))?;
    if !output.status.success() {
        return Ok(None);
    }
    let text = String::from_utf8_lossy(&output.stdout);
    serde_json::from_str::<Vault>(&text)
        .map(Some)
        .map_err(|e| PluginError::new(-32054, format!("Remote vault.json is not a DBX Notes vault: {e}")))
}

fn commit_all(session: &Session, message: &str) -> Result<(), PluginError> {
    let dir = &session.dir;
    run_git(dir, &session.config, &["add", "-A"])?;
    let output = Command::new("git")
        .current_dir(dir)
        .args(["commit", "--no-gpg-sign", "-m", message])
        .env("GIT_AUTHOR_NAME", &session.config.git_author_name)
        .env("GIT_AUTHOR_EMAIL", &session.config.git_author_email)
        .env("GIT_COMMITTER_NAME", &session.config.git_author_name)
        .env("GIT_COMMITTER_EMAIL", &session.config.git_author_email)
        .output()
        .map_err(|e| PluginError::new(-32050, format!("Cannot run git: {e}")))?;
    if output.status.success() {
        return Ok(());
    }
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    if text.contains("nothing to commit") || text.contains("no changes added") {
        return Ok(());
    }
    Err(PluginError::new(-32055, format!("Git commit failed: {}", clean_git_error(text.trim()))))
}

fn write_git_mirrors(session: &Session) -> Result<(), PluginError> {
    let notes_dir = session.dir.join(MIRROR_DIR);
    if notes_dir.exists() {
        if let Ok(entries) = fs::read_dir(&notes_dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_file() && path.extension().and_then(|ext| ext.to_str()) == Some("md") {
                    fs::remove_file(&path)
                        .map_err(|e| PluginError::new(-32050, format!("Cannot clean notes mirror: {e}")))?;
                }
            }
        }
    }
    fs::create_dir_all(&notes_dir).map_err(|e| PluginError::new(-32050, format!("Cannot create notes mirror: {e}")))?;
    for note in &session.vault.notes {
        if note.is_deleted() {
            continue;
        }
        let path = notes_dir.join(format!("{}.md", mirror_filename(note)));
        fs::write(&path, mirror_markdown(note))
            .map_err(|e| PluginError::new(-32050, format!("Cannot write mirror {}: {e}", path.display())))?;
    }
    Ok(())
}

fn run_git(dir: &Path, config: &SyncConfig, args: &[&str]) -> Result<String, PluginError> {
    let output = Command::new("git")
        .current_dir(dir)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_ASKPASS", askpass_program())
        .env("DBX_NOTES_ASKPASS", "1")
        .env("DBX_GIT_USERNAME", &config.git_username)
        .env("DBX_GIT_TOKEN", &config.git_token)
        .args(args)
        .output()
        .map_err(|e| PluginError::new(-32050, format!("Cannot run git: {e}")))?;
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).to_string())
    } else {
        Err(PluginError::new(
            -32055,
            format!("git {} failed: {}", args.join(" "), clean_git_error(String::from_utf8_lossy(&output.stderr).trim())),
        ))
    }
}

fn askpass_program() -> PathBuf {
    std::env::current_exe().unwrap_or_else(|_| PathBuf::from("dbx-plugin-dbx-notes-plugin"))
}

fn is_auth_error(stderr: &str) -> bool {
    let lower = stderr.to_lowercase();
    lower.contains("authentication failed")
        || lower.contains("could not read username")
        || lower.contains("terminal prompts disabled")
        || lower.contains("permission denied")
        || lower.contains("access denied")
}

fn clean_git_error(stderr: &str) -> String {
    let line = stderr
        .lines()
        .find(|line| line.contains("fatal:") || line.contains("error:"))
        .unwrap_or_else(|| stderr.lines().next().unwrap_or("unknown error"));
    line.trim().trim_start_matches("fatal:").trim_start_matches("error:").trim().to_string()
}

fn test_connection(params: &Value) -> Result<Value, PluginError> {
    let connection = params.get("connection").cloned().unwrap_or_default();
    let config = SyncConfig::from_connection(&connection);
    match config.mode.as_str() {
        "webdav" => {
            if config.webdav_url.trim().is_empty() {
                return Ok(json!({ "success": false, "message": "WebDAV URL is empty." }));
            }
            let base = webdav_base(&config.webdav_url)?;
            let client = Client::builder()
                .timeout(Duration::from_secs(12))
                .build()
                .map_err(|e| PluginError::new(-32050, format!("Cannot create HTTP client: {e}")))?;
            let vault_url = base.join("vault.json").map_err(|e| PluginError::new(-32050, e.to_string()))?;
            let mut request = client.get(vault_url);
            if !config.webdav_username.is_empty() {
                request = request.basic_auth(&config.webdav_username, Some(&config.webdav_password));
            }
            let response = request.send().map_err(|e| PluginError::new(-32050, format!("Cannot reach WebDAV: {e}")))?;
            let status = response.status();
            let success = status.is_success() || status == StatusCode::NOT_FOUND || status == StatusCode::FORBIDDEN;
            let message = if success {
                format!("WebDAV is reachable ({status}). Vault file will be created on first sync.")
            } else if status == StatusCode::UNAUTHORIZED {
                "WebDAV authentication failed.".to_string()
            } else {
                format!("WebDAV returned HTTP {status}.")
            };
            Ok(json!({ "success": success, "message": message }))
        }
        "git" => {
            if config.git_remote.trim().is_empty() {
                return Ok(json!({ "success": false, "message": "Git remote is empty." }));
            }
            let dir = std::env::temp_dir().join(format!("dbx-notes-test-{}", std::process::id()));
            let _ = fs::create_dir_all(&dir);
            let output = Command::new("git")
                .current_dir(&dir)
                .env("GIT_TERMINAL_PROMPT", "0")
                .env("GIT_ASKPASS", askpass_program())
                .env("DBX_NOTES_ASKPASS", "1")
                .env("DBX_GIT_USERNAME", &config.git_username)
                .env("DBX_GIT_TOKEN", &config.git_token)
                .args(["ls-remote", &config.git_remote, &config.git_branch])
                .output()
                .map_err(|e| PluginError::new(-32050, format!("Cannot run git: {e}")))?;
            let success = output.status.success();
            let message = if success {
                format!("Git remote is reachable ({}).", config.git_branch)
            } else {
                format!("Git remote check failed: {}", clean_git_error(String::from_utf8_lossy(&output.stderr).trim()))
            };
            Ok(json!({ "success": success, "message": message }))
        }
        _ => Ok(json!({ "success": true, "message": "Local-only vault. Choose WebDAV or Git to enable sync." })),
    }
}

// ---------------------------------------------------------------------------
// Markdown mirror + helpers
// ---------------------------------------------------------------------------

fn mirror_name(note: &Note) -> String {
    format!("{MIRROR_DIR}/{}.md", mirror_filename(note))
}

fn mirror_filename(note: &Note) -> String {
    let mut slug = String::new();
    for ch in note.title.chars() {
        if ch.is_ascii_alphanumeric() {
            slug.push(ch.to_ascii_lowercase());
        } else if ch.is_whitespace() || ch == '-' || ch == '_' {
            if !slug.ends_with('-') && !slug.is_empty() {
                slug.push('-');
            }
        }
        if slug.len() >= 40 {
            break;
        }
    }
    let slug = slug.trim_matches('-');
    let slug = if slug.is_empty() { "untitled" } else { slug };
    let short = format!("{:08x}", fnv1a(note.id.as_bytes()) as u32);
    format!("{slug}-{short}")
}

fn mirror_markdown(note: &Note) -> String {
    let mut out = String::new();
    out.push_str(&format!("# {}\n\n", note.title.trim()));
    if !note.tags.is_empty() {
        out.push_str(&format!("Tags: {}\n\n", note.tags.iter().map(|t| format!("#{t}")).collect::<Vec<_>>().join(" ")));
    }
    out.push_str(note.body.trim_end());
    out.push('\n');
    out
}

fn now_ms() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0)
}

static ID_COUNTER: AtomicU64 = AtomicU64::new(1);

fn new_note_id() -> String {
    format!("n{:x}{:04x}", now_ms(), ID_COUNTER.fetch_add(1, Ordering::Relaxed) & 0xffff)
}

fn new_vault_id() -> String {
    format!("v{:x}{:04x}", now_ms(), ID_COUNTER.fetch_add(1, Ordering::Relaxed) & 0xffff)
}

fn new_attachment_id() -> String {
    format!("a{:x}{:04x}", now_ms(), ID_COUNTER.fetch_add(1, Ordering::Relaxed) & 0xffff)
}

fn new_upload_id() -> String {
    format!("u{:x}{:04x}", now_ms(), ID_COUNTER.fetch_add(1, Ordering::Relaxed) & 0xffff)
}

fn fnv1a(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf29ce484222325;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

fn main() -> std::io::Result<()> {
    // Git invokes GIT_ASKPASS as a separate process. Using this same binary as
    // the helper keeps HTTPS credentials working on every platform without a
    // shell script (Windows has no /bin/sh and cannot set Unix permissions).
    if std::env::var_os("DBX_NOTES_ASKPASS").is_some() {
        let prompt = std::env::args().nth(1).unwrap_or_default().to_lowercase();
        let value = if prompt.contains("username") || prompt.contains("user") {
            std::env::var("DBX_GIT_USERNAME").unwrap_or_else(|_| "git".to_string())
        } else {
            std::env::var("DBX_GIT_TOKEN").unwrap_or_default()
        };
        println!("{value}");
        return Ok(());
    }
    let metadata = PluginMetadata::new("io.github.zjay.notes", env!("CARGO_PKG_VERSION")).with_capability("connections");
    PluginServer::new(metadata, Plugin::default()).serve()
}
