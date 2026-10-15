use anyhow::{anyhow, Result};
use chrono::{DateTime, Local, Utc};
use log::{debug, error, info};
use rusqlite::{params, Connection, OptionalExtension};
use rusqlite_migration::{Migrations, M};
use serde::{Deserialize, Serialize};
use specta::Type;
use std::fs;
use std::path::PathBuf;
use tauri::AppHandle;
use tauri_specta::Event;

/// Database migrations for transcription history.
/// Each migration is applied in order. The library tracks which migrations
/// have been applied using SQLite's user_version pragma.
///
/// Note: For users upgrading from tauri-plugin-sql, migrate_from_tauri_plugin_sql()
/// converts the old _sqlx_migrations table tracking to the user_version pragma,
/// ensuring migrations don't re-run on existing databases.
static MIGRATIONS: &[M] = &[
    M::up(
        "CREATE TABLE IF NOT EXISTS transcription_history (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            file_name TEXT NOT NULL,
            timestamp INTEGER NOT NULL,
            saved BOOLEAN NOT NULL DEFAULT 0,
            title TEXT NOT NULL,
            transcription_text TEXT NOT NULL
        );",
    ),
    M::up("ALTER TABLE transcription_history ADD COLUMN post_processed_text TEXT;"),
    M::up("ALTER TABLE transcription_history ADD COLUMN post_process_prompt TEXT;"),
    M::up("ALTER TABLE transcription_history ADD COLUMN post_process_requested BOOLEAN NOT NULL DEFAULT 0;"),
];

/// Nullable columns added outside [`MIGRATIONS`] so user_version stays at the
/// last released value and an older build still starts: rusqlite_migration
/// refuses a database ahead of its list. Older builds name their columns in
/// every SELECT and INSERT, so they never see these, and a deleted row takes
/// its profile with it. A future migration must not add these names.
const PROFILE_COLUMNS: [&str; 2] = ["profile_id", "profile_name"];

/// Highest history schema (`user_version`) this build can open. A newer
/// database would make startup fail in the migration step.
pub fn supported_schema_version() -> usize {
    MIGRATIONS.len()
}

/// Bring a history database to the schema this build reads, as startup does:
/// convert tauri-plugin-sql tracking, run [`MIGRATIONS`] and add the profile
/// columns. The upstream import runs it on a staged copy before that copy
/// replaces anything.
pub fn prepare_database(conn: &mut Connection) -> Result<()> {
    // Handle migration from tauri-plugin-sql to rusqlite_migration
    // tauri-plugin-sql used _sqlx_migrations table, rusqlite_migration uses user_version pragma
    HistoryManager::migrate_from_tauri_plugin_sql(conn)?;

    HistoryManager::prepare_schema(conn)
}

/// Fail unless every column [`HistoryManager`] reads exists. Migrations can
/// succeed on a database whose `user_version` claims a schema its table does
/// not have.
pub fn check_readable(conn: &Connection) -> Result<()> {
    conn.prepare(
        "SELECT id, file_name, timestamp, saved, title, transcription_text, post_processed_text, post_process_prompt, post_process_requested, profile_id, profile_name
         FROM transcription_history LIMIT 0",
    )?;
    Ok(())
}

#[derive(Clone, Debug, Serialize, Deserialize, Type)]
pub struct PaginatedHistory {
    pub entries: Vec<HistoryEntry>,
    pub has_more: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize, Type, tauri_specta::Event)]
#[serde(tag = "action")]
pub enum HistoryUpdatePayload {
    #[serde(rename = "added")]
    Added { entry: HistoryEntry },
    #[serde(rename = "updated")]
    Updated { entry: HistoryEntry },
    #[serde(rename = "deleted")]
    Deleted { id: i64 },
    #[serde(rename = "toggled")]
    Toggled { id: i64 },
}

#[derive(Clone, Debug, Serialize, Deserialize, Type)]
pub struct HistoryEntry {
    pub id: i64,
    pub file_name: String,
    pub timestamp: i64,
    pub saved: bool,
    pub title: String,
    pub transcription_text: String,
    pub post_processed_text: Option<String>,
    pub post_process_prompt: Option<String>,
    pub post_process_requested: bool,
    /// The processing profile that produced this entry; None for raw
    /// dictation and for entries older than HANDY-008.
    pub profile_id: Option<String>,
    /// The profile name when the entry was saved; None for an unnamed built-in.
    pub profile_name: Option<String>,
}

/// The profile columns of a new entry. The name is a snapshot that survives
/// renames and deletes; an unnamed built-in stores NULL so its entries look as
/// before.
fn profile_columns(
    profile: Option<&crate::settings::ProfileRef>,
) -> (Option<String>, Option<String>) {
    (
        profile.map(|p| p.id.clone()),
        profile
            .map(|p| p.name.clone())
            .filter(|name| !name.is_empty()),
    )
}

pub struct HistoryManager {
    app_handle: AppHandle,
    recordings_dir: PathBuf,
    db_path: PathBuf,
}

impl HistoryManager {
    pub fn new(app_handle: &AppHandle) -> Result<Self> {
        // Create recordings directory in app data dir
        let app_data_dir = crate::portable::app_data_dir(app_handle)?;
        let recordings_dir = app_data_dir.join("recordings");
        let db_path = app_data_dir.join("history.db");

        // Ensure recordings directory exists
        if !recordings_dir.exists() {
            fs::create_dir_all(&recordings_dir)?;
            debug!("Created recordings directory: {:?}", recordings_dir);
        }

        let manager = Self {
            app_handle: app_handle.clone(),
            recordings_dir,
            db_path,
        };

        // Initialize database and run migrations synchronously
        manager.init_database()?;

        Ok(manager)
    }

    fn init_database(&self) -> Result<()> {
        info!("Initializing database at {:?}", self.db_path);

        let mut conn = Connection::open(&self.db_path)?;

        prepare_database(&mut conn)
    }

    fn prepare_schema(conn: &mut Connection) -> Result<()> {
        // Create migrations object and run to latest version
        let migrations = Migrations::new(MIGRATIONS.to_vec());

        // Validate migrations in debug builds
        #[cfg(debug_assertions)]
        migrations.validate().expect("Invalid migrations");

        // Get current version before migration
        let version_before: i32 =
            conn.pragma_query_value(None, "user_version", |row| row.get(0))?;
        debug!("Database version before migration: {}", version_before);

        // Apply any pending migrations
        migrations.to_latest(conn)?;

        // Get version after migration
        let version_after: i32 = conn.pragma_query_value(None, "user_version", |row| row.get(0))?;

        if version_after > version_before {
            info!(
                "Database migrated from version {} to {}",
                version_before, version_after
            );
        } else {
            debug!("Database already at latest version {}", version_after);
        }

        Self::ensure_profile_columns(conn)
    }

    /// Add the missing [`PROFILE_COLUMNS`]; idempotent and leaves user_version
    /// alone.
    fn ensure_profile_columns(conn: &Connection) -> Result<()> {
        let mut stmt =
            conn.prepare("SELECT name FROM pragma_table_info('transcription_history')")?;
        let existing = stmt
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        for column in PROFILE_COLUMNS {
            if !existing.iter().any(|name| name == column) {
                conn.execute_batch(&format!(
                    "ALTER TABLE transcription_history ADD COLUMN {column} TEXT;"
                ))?;
                info!("Added history column {column}");
            }
        }
        Ok(())
    }

    /// Migrate from tauri-plugin-sql's migration tracking to rusqlite_migration's.
    /// tauri-plugin-sql used a _sqlx_migrations table, while rusqlite_migration uses
    /// SQLite's user_version pragma. This function checks if the old system was in use
    /// and sets the user_version accordingly so migrations don't re-run.
    fn migrate_from_tauri_plugin_sql(conn: &Connection) -> Result<()> {
        // Check if the old _sqlx_migrations table exists
        let has_sqlx_migrations: bool = conn
            .query_row(
                "SELECT COUNT(*) > 0 FROM sqlite_master WHERE type='table' AND name='_sqlx_migrations'",
                [],
                |row| row.get(0),
            )
            .unwrap_or(false);

        if !has_sqlx_migrations {
            return Ok(());
        }

        // Check current user_version
        let current_version: i32 =
            conn.pragma_query_value(None, "user_version", |row| row.get(0))?;

        if current_version > 0 {
            // Already migrated to rusqlite_migration system
            return Ok(());
        }

        // Get the highest version from the old migrations table
        let old_version: i32 = conn
            .query_row(
                "SELECT COALESCE(MAX(version), 0) FROM _sqlx_migrations WHERE success = 1",
                [],
                |row| row.get(0),
            )
            .unwrap_or(0);

        if old_version > 0 {
            info!(
                "Migrating from tauri-plugin-sql (version {}) to rusqlite_migration",
                old_version
            );

            // Set user_version to match the old migration state
            conn.pragma_update(None, "user_version", old_version)?;

            // Optionally drop the old migrations table (keeping it doesn't hurt)
            // conn.execute("DROP TABLE IF EXISTS _sqlx_migrations", [])?;

            info!(
                "Migration tracking converted: user_version set to {}",
                old_version
            );
        }

        Ok(())
    }

    fn get_connection(&self) -> Result<Connection> {
        Ok(Connection::open(&self.db_path)?)
    }

    fn map_history_entry(row: &rusqlite::Row<'_>) -> rusqlite::Result<HistoryEntry> {
        Ok(HistoryEntry {
            id: row.get("id")?,
            file_name: row.get("file_name")?,
            timestamp: row.get("timestamp")?,
            saved: row.get("saved")?,
            title: row.get("title")?,
            transcription_text: row.get("transcription_text")?,
            post_processed_text: row.get("post_processed_text")?,
            post_process_prompt: row.get("post_process_prompt")?,
            post_process_requested: row.get("post_process_requested")?,
            profile_id: row.get("profile_id")?,
            profile_name: row.get("profile_name")?,
        })
    }

    pub fn recordings_dir(&self) -> &std::path::Path {
        &self.recordings_dir
    }

    /// Save a new history entry to the database.
    /// The WAV file should already have been written to the recordings directory.
    pub fn save_entry(
        &self,
        file_name: String,
        transcription_text: String,
        post_process_requested: bool,
        post_processed_text: Option<String>,
        post_process_prompt: Option<String>,
        profile: Option<&crate::settings::ProfileRef>,
    ) -> Result<HistoryEntry> {
        let timestamp = Utc::now().timestamp();
        let title = self.format_timestamp_title(timestamp);

        let conn = self.get_connection()?;
        let (profile_id, profile_name) = profile_columns(profile);
        let entry = Self::insert_entry_with_conn(
            &conn,
            HistoryEntry {
                id: 0,
                file_name,
                timestamp,
                saved: false,
                title,
                transcription_text,
                post_processed_text,
                post_process_prompt,
                post_process_requested,
                profile_id,
                profile_name,
            },
        )?;

        debug!("Saved history entry with id {}", entry.id);

        self.cleanup_old_entries()?;

        // Emit typed event for real-time frontend updates
        if let Err(e) = (HistoryUpdatePayload::Added {
            entry: entry.clone(),
        })
        .emit(&self.app_handle)
        {
            error!("Failed to emit history-updated event: {}", e);
        }

        Ok(entry)
    }

    fn insert_entry_with_conn(conn: &Connection, mut entry: HistoryEntry) -> Result<HistoryEntry> {
        conn.execute(
            "INSERT INTO transcription_history (
                file_name,
                timestamp,
                saved,
                title,
                transcription_text,
                post_processed_text,
                post_process_prompt,
                post_process_requested,
                profile_id,
                profile_name
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                &entry.file_name,
                entry.timestamp,
                entry.saved,
                &entry.title,
                &entry.transcription_text,
                &entry.post_processed_text,
                &entry.post_process_prompt,
                entry.post_process_requested,
                &entry.profile_id,
                &entry.profile_name,
            ],
        )?;
        entry.id = conn.last_insert_rowid();
        Ok(entry)
    }

    /// Update an existing history entry with new transcription results (used by
    /// retry). The profile columns are left untouched.
    pub fn update_transcription(
        &self,
        id: i64,
        transcription_text: String,
        post_processed_text: Option<String>,
        post_process_prompt: Option<String>,
    ) -> Result<HistoryEntry> {
        let conn = self.get_connection()?;
        let updated = conn.execute(
            "UPDATE transcription_history
             SET transcription_text = ?1,
                 post_processed_text = ?2,
                 post_process_prompt = ?3
             WHERE id = ?4",
            params![
                transcription_text,
                post_processed_text,
                post_process_prompt,
                id
            ],
        )?;

        if updated == 0 {
            return Err(anyhow!("History entry {} not found", id));
        }

        let entry = conn
            .query_row(
                "SELECT id, file_name, timestamp, saved, title, transcription_text, post_processed_text, post_process_prompt, post_process_requested, profile_id, profile_name
                 FROM transcription_history WHERE id = ?1",
                params![id],
                Self::map_history_entry,
            )?;

        debug!("Updated transcription for history entry {}", id);

        if let Err(e) = (HistoryUpdatePayload::Updated {
            entry: entry.clone(),
        })
        .emit(&self.app_handle)
        {
            error!("Failed to emit history-updated event: {}", e);
        }

        Ok(entry)
    }

    pub fn cleanup_old_entries(&self) -> Result<()> {
        let retention_period = crate::settings::get_recording_retention_period(&self.app_handle);

        match retention_period {
            crate::settings::RecordingRetentionPeriod::Never => {
                // Don't delete anything
                Ok(())
            }
            crate::settings::RecordingRetentionPeriod::PreserveLimit => {
                // Use the old count-based logic with history_limit
                let limit = crate::settings::get_history_limit(&self.app_handle);
                self.cleanup_by_count(limit)
            }
            _ => {
                // Use time-based logic
                self.cleanup_by_time(retention_period)
            }
        }
    }

    fn delete_entries_and_files(&self, entries: &[(i64, String)]) -> Result<usize> {
        if entries.is_empty() {
            return Ok(0);
        }

        let conn = self.get_connection()?;
        let mut deleted_count = 0;

        for (id, file_name) in entries {
            // Delete database entry
            conn.execute(
                "DELETE FROM transcription_history WHERE id = ?1",
                params![id],
            )?;

            // Delete WAV file
            let file_path = self.recordings_dir.join(file_name);
            if file_path.exists() {
                if let Err(e) = fs::remove_file(&file_path) {
                    error!("Failed to delete WAV file {}: {}", file_name, e);
                } else {
                    debug!("Deleted old WAV file: {}", file_name);
                    deleted_count += 1;
                }
            }
        }

        Ok(deleted_count)
    }

    fn cleanup_by_count(&self, limit: usize) -> Result<()> {
        let conn = self.get_connection()?;

        // Get all entries that are not saved, ordered by timestamp desc
        let mut stmt = conn.prepare(
            "SELECT id, file_name FROM transcription_history WHERE saved = 0 ORDER BY timestamp DESC"
        )?;

        let rows = stmt.query_map([], |row| {
            Ok((row.get::<_, i64>("id")?, row.get::<_, String>("file_name")?))
        })?;

        let mut entries: Vec<(i64, String)> = Vec::new();
        for row in rows {
            entries.push(row?);
        }

        if entries.len() > limit {
            let entries_to_delete = &entries[limit..];
            let deleted_count = self.delete_entries_and_files(entries_to_delete)?;

            if deleted_count > 0 {
                debug!("Cleaned up {} old history entries by count", deleted_count);
            }
        }

        Ok(())
    }

    fn cleanup_by_time(
        &self,
        retention_period: crate::settings::RecordingRetentionPeriod,
    ) -> Result<()> {
        let conn = self.get_connection()?;

        // Calculate cutoff timestamp (current time minus retention period)
        let now = Utc::now().timestamp();
        let cutoff_timestamp = match retention_period {
            crate::settings::RecordingRetentionPeriod::Days3 => now - (3 * 24 * 60 * 60), // 3 days in seconds
            crate::settings::RecordingRetentionPeriod::Weeks2 => now - (2 * 7 * 24 * 60 * 60), // 2 weeks in seconds
            crate::settings::RecordingRetentionPeriod::Months3 => now - (3 * 30 * 24 * 60 * 60), // 3 months in seconds (approximate)
            _ => unreachable!("Should not reach here"),
        };

        // Get all unsaved entries older than the cutoff timestamp
        let mut stmt = conn.prepare(
            "SELECT id, file_name FROM transcription_history WHERE saved = 0 AND timestamp < ?1",
        )?;

        let rows = stmt.query_map(params![cutoff_timestamp], |row| {
            Ok((row.get::<_, i64>("id")?, row.get::<_, String>("file_name")?))
        })?;

        let mut entries_to_delete: Vec<(i64, String)> = Vec::new();
        for row in rows {
            entries_to_delete.push(row?);
        }

        let deleted_count = self.delete_entries_and_files(&entries_to_delete)?;

        if deleted_count > 0 {
            debug!(
                "Cleaned up {} old history entries based on retention period",
                deleted_count
            );
        }

        Ok(())
    }

    pub async fn get_history_entries(
        &self,
        cursor: Option<i64>,
        limit: Option<usize>,
    ) -> Result<PaginatedHistory> {
        let conn = self.get_connection()?;
        let limit = limit.map(|l| l.min(100));

        let mut entries: Vec<HistoryEntry> = match (cursor, limit) {
            (Some(cursor_id), Some(lim)) => {
                let fetch_count = (lim + 1) as i64;
                let mut stmt = conn.prepare(
                    "SELECT id, file_name, timestamp, saved, title, transcription_text, post_processed_text, post_process_prompt, post_process_requested, profile_id, profile_name
                     FROM transcription_history
                     WHERE id < ?1
                     ORDER BY id DESC
                     LIMIT ?2",
                )?;
                let result = stmt
                    .query_map(params![cursor_id, fetch_count], Self::map_history_entry)?
                    .collect::<std::result::Result<Vec<_>, _>>()?;
                result
            }
            (None, Some(lim)) => {
                let fetch_count = (lim + 1) as i64;
                let mut stmt = conn.prepare(
                    "SELECT id, file_name, timestamp, saved, title, transcription_text, post_processed_text, post_process_prompt, post_process_requested, profile_id, profile_name
                     FROM transcription_history
                     ORDER BY id DESC
                     LIMIT ?1",
                )?;
                let result = stmt
                    .query_map(params![fetch_count], Self::map_history_entry)?
                    .collect::<std::result::Result<Vec<_>, _>>()?;
                result
            }
            (_, None) => {
                let mut stmt = conn.prepare(
                    "SELECT id, file_name, timestamp, saved, title, transcription_text, post_processed_text, post_process_prompt, post_process_requested, profile_id, profile_name
                     FROM transcription_history
                     ORDER BY id DESC",
                )?;
                let result = stmt
                    .query_map([], Self::map_history_entry)?
                    .collect::<std::result::Result<Vec<_>, _>>()?;
                result
            }
        };

        let has_more = limit.is_some_and(|lim| entries.len() > lim);
        if has_more {
            entries.pop();
        }

        Ok(PaginatedHistory { entries, has_more })
    }

    #[cfg(test)]
    fn get_latest_entry_with_conn(conn: &Connection) -> Result<Option<HistoryEntry>> {
        let mut stmt = conn.prepare(
            "SELECT
                id,
                file_name,
                timestamp,
                saved,
                title,
                transcription_text,
                post_processed_text,
                post_process_prompt,
                post_process_requested,
                profile_id,
                profile_name
             FROM transcription_history
             ORDER BY timestamp DESC
             LIMIT 1",
        )?;

        let entry = stmt.query_row([], Self::map_history_entry).optional()?;
        Ok(entry)
    }

    /// Get the latest entry with non-empty transcription text.
    pub fn get_latest_completed_entry(&self) -> Result<Option<HistoryEntry>> {
        let conn = self.get_connection()?;
        Self::get_latest_completed_entry_with_conn(&conn)
    }

    fn get_latest_completed_entry_with_conn(conn: &Connection) -> Result<Option<HistoryEntry>> {
        let mut stmt = conn.prepare(
            "SELECT
                id,
                file_name,
                timestamp,
                saved,
                title,
                transcription_text,
                post_processed_text,
                post_process_prompt,
                post_process_requested,
                profile_id,
                profile_name
             FROM transcription_history
             WHERE transcription_text != ''
             ORDER BY timestamp DESC
             LIMIT 1",
        )?;

        let entry = stmt.query_row([], Self::map_history_entry).optional()?;
        Ok(entry)
    }

    pub async fn toggle_saved_status(&self, id: i64) -> Result<()> {
        let conn = self.get_connection()?;

        // Get current saved status
        let current_saved: bool = conn.query_row(
            "SELECT saved FROM transcription_history WHERE id = ?1",
            params![id],
            |row| row.get("saved"),
        )?;

        let new_saved = !current_saved;

        conn.execute(
            "UPDATE transcription_history SET saved = ?1 WHERE id = ?2",
            params![new_saved, id],
        )?;

        debug!("Toggled saved status for entry {}: {}", id, new_saved);

        // Emit history updated event
        if let Err(e) = (HistoryUpdatePayload::Toggled { id }).emit(&self.app_handle) {
            error!("Failed to emit history-updated event: {}", e);
        }

        Ok(())
    }

    pub fn get_audio_file_path(&self, file_name: &str) -> PathBuf {
        self.recordings_dir.join(file_name)
    }

    pub async fn get_entry_by_id(&self, id: i64) -> Result<Option<HistoryEntry>> {
        let conn = self.get_connection()?;
        let mut stmt = conn.prepare(
            "SELECT
                id,
                file_name,
                timestamp,
                saved,
                title,
                transcription_text,
                post_processed_text,
                post_process_prompt,
                post_process_requested,
                profile_id,
                profile_name
             FROM transcription_history
             WHERE id = ?1",
        )?;

        let entry = stmt.query_row([id], Self::map_history_entry).optional()?;

        Ok(entry)
    }

    pub async fn delete_entry(&self, id: i64) -> Result<()> {
        let conn = self.get_connection()?;

        // Get the entry to find the file name
        if let Some(entry) = self.get_entry_by_id(id).await? {
            // Delete the audio file first
            let file_path = self.get_audio_file_path(&entry.file_name);
            if file_path.exists() {
                if let Err(e) = fs::remove_file(&file_path) {
                    error!("Failed to delete audio file {}: {}", entry.file_name, e);
                    // Continue with database deletion even if file deletion fails
                }
            }
        }

        // Delete from database
        conn.execute(
            "DELETE FROM transcription_history WHERE id = ?1",
            params![id],
        )?;

        debug!("Deleted history entry with id: {}", id);

        // Emit history updated event
        if let Err(e) = (HistoryUpdatePayload::Deleted { id }).emit(&self.app_handle) {
            error!("Failed to emit history-updated event: {}", e);
        }

        Ok(())
    }

    fn format_timestamp_title(&self, timestamp: i64) -> String {
        if let Some(utc_datetime) = DateTime::from_timestamp(timestamp, 0) {
            // Convert UTC to local timezone
            let local_datetime = utc_datetime.with_timezone(&Local);
            local_datetime.format("%B %e, %Y - %l:%M%p").to_string()
        } else {
            format!("Recording {}", timestamp)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::{params, Connection};

    fn setup_conn() -> Connection {
        let mut conn = Connection::open_in_memory().expect("open in-memory db");
        HistoryManager::prepare_schema(&mut conn).expect("prepare history schema");
        conn
    }

    /// The migration count of the last build before HANDY-008 (42b4630). An
    /// older build runs this list at startup and fails on a higher
    /// user_version.
    const RELEASED_MIGRATIONS: usize = 4;

    fn user_version(conn: &Connection) -> i32 {
        conn.pragma_query_value(None, "user_version", |row| row.get(0))
            .expect("read user_version")
    }

    fn released_build_init(conn: &mut Connection) {
        Migrations::new(MIGRATIONS[..RELEASED_MIGRATIONS].to_vec())
            .to_latest(conn)
            .expect("an older build initializes this database");
    }

    fn insert_profile_entry(conn: &Connection, timestamp: i64, profile_id: &str) -> HistoryEntry {
        HistoryManager::insert_entry_with_conn(
            conn,
            HistoryEntry {
                id: 0,
                file_name: format!("handy-{timestamp}.wav"),
                timestamp,
                saved: false,
                title: "Recording".into(),
                transcription_text: "text".into(),
                post_processed_text: Some("cleaned".into()),
                post_process_prompt: None,
                post_process_requested: true,
                profile_id: Some(profile_id.into()),
                profile_name: Some("Profile".into()),
            },
        )
        .expect("insert profile entry")
    }

    fn profile_rows(conn: &Connection) -> i64 {
        conn.query_row(
            "SELECT COUNT(*) FROM transcription_history WHERE profile_id IS NOT NULL OR profile_name IS NOT NULL",
            [],
            |row| row.get(0),
        )
        .expect("count profile rows")
    }

    #[test]
    fn handy008_new_database_still_opens_in_an_older_build() {
        let mut conn = Connection::open_in_memory().expect("open in-memory db");
        HistoryManager::prepare_schema(&mut conn).expect("prepare schema");
        insert_profile_entry(&conn, 100, "p1");
        released_build_init(&mut conn);
        assert_eq!(user_version(&conn), RELEASED_MIGRATIONS as i32);
    }

    #[test]
    fn handy008_older_database_gains_profile_columns_and_old_rows_read_none() {
        let mut conn = Connection::open_in_memory().expect("open in-memory db");
        released_build_init(&mut conn);
        insert_entry(&conn, 100, "old row", Some("cleaned"));
        HistoryManager::prepare_schema(&mut conn).expect("upgrade schema");
        assert_eq!(user_version(&conn), RELEASED_MIGRATIONS as i32);
        let entry = HistoryManager::get_latest_entry_with_conn(&conn)
            .expect("fetch latest entry")
            .expect("entry exists");
        assert_eq!(entry.transcription_text, "old row");
        assert_eq!(entry.profile_id, None);
        assert_eq!(entry.profile_name, None);
    }

    #[test]
    fn handy008_round_trip_through_an_older_build_keeps_no_stale_profile() {
        let mut conn = setup_conn();
        let tagged = insert_profile_entry(&conn, 100, "p1");

        // The older build starts, deletes the tagged entry the way its delete
        // and retention paths do, and saves an entry with its own INSERT.
        released_build_init(&mut conn);
        conn.execute(
            "DELETE FROM transcription_history WHERE id = ?1",
            params![tagged.id],
        )
        .expect("older build deletes the entry");
        assert_eq!(profile_rows(&conn), 0, "the profile left with its row");
        insert_entry(&conn, 200, "older build", None);

        // The newer build starts again: idempotent, still at the released version.
        HistoryManager::prepare_schema(&mut conn).expect("reopen schema");
        assert_eq!(user_version(&conn), RELEASED_MIGRATIONS as i32);
        let older = HistoryManager::get_latest_entry_with_conn(&conn)
            .expect("fetch latest entry")
            .expect("entry exists");
        assert_eq!(older.transcription_text, "older build");
        assert_eq!((older.profile_id, older.profile_name), (None, None));

        let newer = insert_profile_entry(&conn, 300, "p2");
        let loaded = HistoryManager::get_latest_entry_with_conn(&conn)
            .expect("fetch latest entry")
            .expect("entry exists");
        assert_eq!(loaded.id, newer.id);
        assert_eq!(loaded.profile_id.as_deref(), Some("p2"));
        assert_eq!(profile_rows(&conn), 1);
    }

    #[test]
    fn handy008_profile_columns_round_trip_and_unnamed_builtin_stores_null() {
        use crate::settings::ProfileRef;
        let conn = setup_conn();
        let custom = ProfileRef {
            id: "p1700000000000".into(),
            name: "Polish to English".into(),
            builtin: false,
        };
        let builtin = ProfileRef {
            id: "builtin_cleanup".into(),
            name: String::new(),
            builtin: true,
        };
        for (timestamp, profile, expected) in [
            (
                100,
                Some(&custom),
                (Some("p1700000000000"), Some("Polish to English")),
            ),
            (200, Some(&builtin), (Some("builtin_cleanup"), None)),
            (300, None, (None, None)),
        ] {
            let (profile_id, profile_name) = profile_columns(profile);
            let saved = HistoryManager::insert_entry_with_conn(
                &conn,
                HistoryEntry {
                    id: 0,
                    file_name: format!("handy-{timestamp}.wav"),
                    timestamp,
                    saved: false,
                    title: "Recording".into(),
                    transcription_text: "text".into(),
                    post_processed_text: None,
                    post_process_prompt: None,
                    post_process_requested: profile.is_some(),
                    profile_id,
                    profile_name,
                },
            )
            .expect("insert entry");
            let loaded = HistoryManager::get_latest_entry_with_conn(&conn)
                .expect("fetch latest entry")
                .expect("entry exists");
            assert_eq!(loaded.id, saved.id);
            assert_eq!(
                (loaded.profile_id.as_deref(), loaded.profile_name.as_deref()),
                expected
            );
        }
    }

    fn insert_entry(conn: &Connection, timestamp: i64, text: &str, post_processed: Option<&str>) {
        conn.execute(
            "INSERT INTO transcription_history (
                file_name,
                timestamp,
                saved,
                title,
                transcription_text,
                post_processed_text,
                post_process_prompt,
                post_process_requested
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                format!("handy-{}.wav", timestamp),
                timestamp,
                false,
                format!("Recording {}", timestamp),
                text,
                post_processed,
                Option::<String>::None,
                false,
            ],
        )
        .expect("insert history entry");
    }

    #[test]
    fn get_latest_entry_returns_none_when_empty() {
        let conn = setup_conn();
        let entry = HistoryManager::get_latest_entry_with_conn(&conn).expect("fetch latest entry");
        assert!(entry.is_none());
    }

    #[test]
    fn get_latest_entry_returns_newest_entry() {
        let conn = setup_conn();
        insert_entry(&conn, 100, "first", None);
        insert_entry(&conn, 200, "second", Some("processed"));

        let entry = HistoryManager::get_latest_entry_with_conn(&conn)
            .expect("fetch latest entry")
            .expect("entry exists");

        assert_eq!(entry.timestamp, 200);
        assert_eq!(entry.transcription_text, "second");
        assert_eq!(entry.post_processed_text.as_deref(), Some("processed"));
    }

    #[test]
    fn get_latest_completed_entry_skips_empty_entries() {
        let conn = setup_conn();
        insert_entry(&conn, 100, "completed", None);
        insert_entry(&conn, 200, "", None);

        let entry = HistoryManager::get_latest_completed_entry_with_conn(&conn)
            .expect("fetch latest completed entry")
            .expect("completed entry exists");

        assert_eq!(entry.timestamp, 100);
        assert_eq!(entry.transcription_text, "completed");
    }
}
