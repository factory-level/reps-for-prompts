//! SQLite persistence. Only this module touches the database.

use std::path::Path;

use rusqlite::{params, Connection};

use crate::types::{ExerciseDef, ExerciseKind, SetRecord};

pub struct Store {
    conn: Connection,
}

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS routine_days (date TEXT PRIMARY KEY, completed INTEGER NOT NULL, target INTEGER NOT NULL, updated_at TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS destination_uploads (destination TEXT NOT NULL, public_id TEXT NOT NULL, PRIMARY KEY(destination,public_id));
CREATE TABLE IF NOT EXISTS schema_version (version INTEGER NOT NULL);
CREATE TABLE IF NOT EXISTS settings (key TEXT PRIMARY KEY, value TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS rotation (
    position INTEGER PRIMARY KEY,
    name TEXT NOT NULL,
    kind TEXT NOT NULL,
    default_reps INTEGER NOT NULL,
    default_weight REAL NOT NULL,
    target_seconds REAL NOT NULL
);
CREATE TABLE IF NOT EXISTS pointer_state (
    id INTEGER PRIMARY KEY CHECK (id = 1),
    pointer INTEGER NOT NULL,
    capacity_used INTEGER NOT NULL,
    capacity_date TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS workouts (
    id TEXT PRIMARY KEY,
    routine_id TEXT NOT NULL,
    routine_name TEXT NOT NULL,
    slot_id TEXT NOT NULL,
    status TEXT NOT NULL CHECK (status IN ('active','completed','partial','canceled')),
    started_at TEXT NOT NULL,
    completed_at TEXT,
    local_date TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS exercise_history (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    date TEXT NOT NULL,
    exercise TEXT NOT NULL,
    kind TEXT NOT NULL,
    reps INTEGER NOT NULL,
    seconds REAL NOT NULL,
    weight REAL NOT NULL,
    verified INTEGER NOT NULL
);
";

fn kind_to_str(k: ExerciseKind) -> &'static str {
    match k {
        ExerciseKind::Rep => "rep",
        ExerciseKind::Continuous => "continuous",
    }
}

fn kind_from_str(s: &str) -> ExerciseKind {
    if s == "continuous" { ExerciseKind::Continuous } else { ExerciseKind::Rep }
}

impl Store {
    pub fn open(path: &Path) -> rusqlite::Result<Self> {
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let conn = Connection::open(path)?;
        conn.execute_batch(SCHEMA)?;
        let version: Option<i64> = conn
            .query_row("SELECT version FROM schema_version", [], |r| r.get(0))
            .ok();
        if version.is_none() {
            conn.execute("INSERT INTO schema_version (version) VALUES (1)", [])?;
        }
        conn.execute_batch("BEGIN IMMEDIATE")?;
        let migration = (|| -> rusqlite::Result<()> {
            let has_id: bool = conn.query_row("SELECT count(*) FROM pragma_table_info('exercise_history') WHERE name='public_id'", [], |r| r.get::<_, i64>(0)).map(|n| n > 0)?;
            if !has_id {
                conn.execute_batch("ALTER TABLE exercise_history ADD COLUMN public_id TEXT;
                    ALTER TABLE exercise_history ADD COLUMN recorded_at TEXT;
                    ALTER TABLE exercise_history ADD COLUMN weight_unit TEXT;
                    ALTER TABLE exercise_history ADD COLUMN synced INTEGER NOT NULL DEFAULT 0;
                    UPDATE exercise_history SET public_id=lower(hex(randomblob(16)));
                    CREATE UNIQUE INDEX history_public_id ON exercise_history(public_id);
                    UPDATE schema_version SET version=2;")?;
            }
            let has_workout: bool = conn.query_row("SELECT count(*) FROM pragma_table_info('exercise_history') WHERE name='workout_id'", [], |r| r.get::<_, i64>(0)).map(|n| n > 0)?;
            if !has_workout {
                conn.execute_batch("ALTER TABLE exercise_history ADD COLUMN workout_id TEXT;
                    UPDATE schema_version SET version=3;")?;
            }
            Ok(())
        })();
        if let Err(error) = migration { let _ = conn.execute_batch("ROLLBACK"); return Err(error); }
        conn.execute_batch("COMMIT")?;
        conn.busy_timeout(std::time::Duration::from_secs(3))?;
        let store = Self { conn };
        if store.load_rotation()?.is_empty() {
            store.save_rotation(&default_rotation())?;
        }
        Ok(store)
    }

    pub fn open_readonly(path: &Path) -> rusqlite::Result<Self> {
        let conn = Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        conn.busy_timeout(std::time::Duration::from_secs(3))?;
        Ok(Self { conn })
    }

    pub fn records(&self, from: Option<&str>, to: Option<&str>, cursor: i64, limit: u32, pending: bool) -> rusqlite::Result<Vec<crate::history::HistoryRecord>> {
        let mut stmt = self.conn.prepare("SELECT id,public_id,date,exercise,kind,reps,seconds,weight,verified,recorded_at,weight_unit FROM exercise_history WHERE (?1 IS NULL OR date>=?1) AND (?2 IS NULL OR date<=?2) AND (?3=0 OR id<?3) AND (?4=0 OR synced=0) ORDER BY id DESC LIMIT ?5")?;
        let rows = stmt.query_map(params![from,to,cursor,pending,limit], |r| Ok(crate::history::HistoryRecord {
            sequence: r.get(0)?, id:r.get(1)?, date:r.get(2)?, exercise:r.get(3)?, kind:kind_from_str(&r.get::<_,String>(4)?),reps:r.get(5)?,seconds:r.get(6)?,weight:r.get(7)?,verified:r.get(8)?,recorded_at:r.get(9)?,weight_unit:r.get(10)?
        }))?.collect();
        rows
    }

    pub fn record_routine_day(&self,date:&str,completed:u32,target:u32)->rusqlite::Result<()> {
        if target==0{return Ok(());}
        self.conn.execute("INSERT INTO routine_days(date,completed,target,updated_at) VALUES(?1,?2,?3,?4) ON CONFLICT(date) DO UPDATE SET completed=excluded.completed,target=excluded.target,updated_at=excluded.updated_at WHERE completed!=excluded.completed OR target!=excluded.target",params![date,completed,target,chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis,true)])?;Ok(())
    }
    pub fn routine_days(&self)->rusqlite::Result<Vec<serde_json::Value>> {
        let mut q=self.conn.prepare("SELECT date,completed,target,updated_at FROM routine_days ORDER BY date DESC LIMIT 28")?;
        let rows=q.query_map([],|r|Ok(serde_json::json!({"date":r.get::<_,String>(0)?,"completed":r.get::<_,u32>(1)?,"target":r.get::<_,u32>(2)?,"updatedAt":r.get::<_,String>(3)?})))?.collect();rows
    }

    pub fn pending_for_destination(&self, destination:&str, limit:u32)->rusqlite::Result<Vec<crate::history::HistoryRecord>> {
        let mut stmt=self.conn.prepare("SELECT id,public_id,date,exercise,kind,reps,seconds,weight,verified,recorded_at,weight_unit FROM exercise_history h WHERE NOT EXISTS (SELECT 1 FROM destination_uploads d WHERE d.destination=?1 AND d.public_id=h.public_id) ORDER BY id ASC LIMIT ?2")?;
        let rows=stmt.query_map(params![destination,limit],|r|Ok(crate::history::HistoryRecord{sequence:r.get(0)?,id:r.get(1)?,date:r.get(2)?,exercise:r.get(3)?,kind:kind_from_str(&r.get::<_,String>(4)?),reps:r.get(5)?,seconds:r.get(6)?,weight:r.get(7)?,verified:r.get(8)?,recorded_at:r.get(9)?,weight_unit:r.get(10)?}))?.collect();rows
    }
    pub fn acknowledge_destination(&self,destination:&str,ids:&[String])->rusqlite::Result<()> {
        let tx=self.conn.unchecked_transaction()?;
        for id in ids {tx.execute("INSERT OR IGNORE INTO destination_uploads(destination,public_id) VALUES(?1,?2)",params![destination,id])?;}
        tx.commit()
    }

    pub fn summary(&self, from: Option<&str>, to: Option<&str>) -> rusqlite::Result<serde_json::Value> {
        self.conn.query_row("SELECT count(*),coalesce(sum(reps),0),coalesce(sum(seconds),0) FROM exercise_history WHERE (?1 IS NULL OR date>=?1) AND (?2 IS NULL OR date<=?2)",params![from,to],|r| Ok(serde_json::json!({"schemaVersion":1,"source":"local","sets":r.get::<_,i64>(0)?,"reps":r.get::<_,i64>(1)?,"seconds":r.get::<_,f64>(2)?})))
    }

    pub fn acknowledge(&self, ids: &[String]) -> rusqlite::Result<()> {
        for id in ids { self.conn.execute("UPDATE exercise_history SET synced=1 WHERE public_id=?1", [id])?; }
        Ok(())
    }

    pub fn load_rotation(&self) -> rusqlite::Result<Vec<ExerciseDef>> {
        let mut stmt = self.conn.prepare(
            "SELECT name, kind, default_reps, default_weight, target_seconds
             FROM rotation ORDER BY position",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(ExerciseDef {
                name: r.get(0)?,
                kind: kind_from_str(&r.get::<_, String>(1)?),
                default_reps: r.get(2)?,
                default_weight: r.get(3)?,
                target_seconds: r.get(4)?,
            })
        })?;
        rows.collect()
    }

    pub fn save_rotation(&self, defs: &[ExerciseDef]) -> rusqlite::Result<()> {
        self.conn.execute("DELETE FROM rotation", [])?;
        for (i, d) in defs.iter().enumerate() {
            self.conn.execute(
                "INSERT INTO rotation (position, name, kind, default_reps, default_weight, target_seconds)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![i as i64, d.name, kind_to_str(d.kind), d.default_reps, d.default_weight, d.target_seconds],
            )?;
        }
        Ok(())
    }

    pub fn load_pointer_state(&self) -> rusqlite::Result<(usize, u32, String)> {
        let row = self
            .conn
            .query_row(
                "SELECT pointer, capacity_used, capacity_date FROM pointer_state WHERE id = 1",
                [],
                |r| Ok((r.get::<_, i64>(0)? as usize, r.get(1)?, r.get(2)?)),
            );
        match row {
            Ok(v) => Ok(v),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok((0, 0, String::new())),
            Err(e) => Err(e),
        }
    }

    pub fn save_pointer_state(
        &self,
        pointer: usize,
        capacity_used: u32,
        capacity_date: &str,
    ) -> rusqlite::Result<()> {
        self.conn.execute(
            "INSERT INTO pointer_state (id, pointer, capacity_used, capacity_date)
             VALUES (1, ?1, ?2, ?3)
             ON CONFLICT(id) DO UPDATE SET
               pointer = ?1, capacity_used = ?2, capacity_date = ?3",
            params![pointer as i64, capacity_used, capacity_date],
        )?;
        Ok(())
    }

    pub fn record_set(&self, rec: &SetRecord) -> rusqlite::Result<()> {
        self.conn.execute(
            "INSERT INTO exercise_history (date, exercise, kind, reps, seconds, weight, verified,public_id,recorded_at,weight_unit)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7,lower(hex(randomblob(16))),strftime('%Y-%m-%dT%H:%M:%fZ','now'),'lb')",
            params![
                rec.date,
                rec.exercise,
                kind_to_str(rec.kind),
                rec.reps,
                rec.seconds,
                rec.weight,
                rec.verified as i64
            ],
        )?;
        Ok(())
    }

    /// Attach the set just recorded to its workout session.
    pub fn tag_last_set(&self, workout_id: &str) -> rusqlite::Result<()> {
        self.conn.execute("UPDATE exercise_history SET workout_id=?1 WHERE id=last_insert_rowid()", [workout_id])?;
        Ok(())
    }

    pub fn insert_workout(&self, id: &str, routine_id: &str, routine_name: &str, slot_id: &str, local_date: &str) -> rusqlite::Result<()> {
        self.conn.execute("INSERT INTO workouts(id,routine_id,routine_name,slot_id,status,started_at,local_date) VALUES(?1,?2,?3,?4,'active',strftime('%Y-%m-%dT%H:%M:%fZ','now'),?5)",
            params![id, routine_id, routine_name, slot_id, local_date])?;
        Ok(())
    }

    /// Close a session. `local_date` is the completion date (where a full
    /// routine counts); only an active session can be closed.
    pub fn finish_workout(&self, id: &str, status: &str, local_date: &str) -> rusqlite::Result<bool> {
        let n = self.conn.execute("UPDATE workouts SET status=?2, completed_at=strftime('%Y-%m-%dT%H:%M:%fZ','now'), local_date=?3 WHERE id=?1 AND status='active'",
            params![id, status, local_date])?;
        Ok(n == 1)
    }

    /// Sessions whose local date is in [from, to], oldest first.
    pub fn workouts_between(&self, from: &str, to: &str) -> rusqlite::Result<Vec<serde_json::Value>> {
        let mut q = self.conn.prepare("SELECT w.id,w.routine_name,w.status,w.started_at,w.completed_at,w.local_date,
            (SELECT count(*) FROM exercise_history h WHERE h.workout_id=w.id) FROM workouts w WHERE w.local_date BETWEEN ?1 AND ?2 ORDER BY w.started_at")?;
        let rows = q.query_map(params![from, to], |r| Ok(serde_json::json!({
            "id": r.get::<_, String>(0)?, "routine": r.get::<_, String>(1)?, "status": r.get::<_, String>(2)?,
            "startedAt": r.get::<_, String>(3)?, "completedAt": r.get::<_, Option<String>>(4)?, "date": r.get::<_, String>(5)?,
            "sets": r.get::<_, i64>(6)?,
        })))?.collect();
        rows
    }

    /// Confirmed sets in [from, to] (or of one exercise), oldest first.
    pub fn sets(&self, from: Option<&str>, to: Option<&str>, exercise: Option<&str>) -> rusqlite::Result<Vec<serde_json::Value>> {
        let mut q = self.conn.prepare("SELECT id,date,exercise,kind,reps,seconds,weight,verified,workout_id,recorded_at FROM exercise_history
            WHERE (?1 IS NULL OR date>=?1) AND (?2 IS NULL OR date<=?2) AND (?3 IS NULL OR exercise=?3) ORDER BY id")?;
        let rows = q.query_map(params![from, to, exercise], |r| Ok(serde_json::json!({
            "id": r.get::<_, i64>(0)?, "date": r.get::<_, String>(1)?, "exercise": r.get::<_, String>(2)?,
            "kind": r.get::<_, String>(3)?, "reps": r.get::<_, i64>(4)?, "seconds": r.get::<_, f64>(5)?,
            "weight": r.get::<_, f64>(6)?, "verified": r.get::<_, i64>(7)? != 0,
            "workoutId": r.get::<_, Option<String>>(8)?, "recordedAt": r.get::<_, Option<String>>(9)?,
        })))?.collect();
        rows
    }

    /// The weight of the last confirmed set of this exercise, if any.
    pub fn last_weight(&self, exercise: &str) -> Option<f64> {
        self.conn.query_row("SELECT weight FROM exercise_history WHERE exercise=?1 AND kind='rep' ORDER BY id DESC LIMIT 1", [exercise], |r| r.get(0)).ok()
    }

    /// Whether a session was started on this local date.
    pub fn started_on(&self, local_date: &str) -> bool {
        self.conn.query_row("SELECT count(*) FROM workouts WHERE local_date=?1", [local_date], |r| r.get::<_, i64>(0)).map(|n| n > 0).unwrap_or(false)
    }

    pub fn history(&self, limit: u32) -> rusqlite::Result<Vec<SetRecord>> {
        let mut stmt = self.conn.prepare(
            "SELECT date, exercise, kind, reps, seconds, weight, verified
             FROM exercise_history ORDER BY id DESC LIMIT ?1",
        )?;
        let rows = stmt.query_map([limit], |r| {
            Ok(SetRecord {
                date: r.get(0)?,
                exercise: r.get(1)?,
                kind: kind_from_str(&r.get::<_, String>(2)?),
                reps: r.get(3)?,
                seconds: r.get(4)?,
                weight: r.get(5)?,
                verified: r.get::<_, i64>(6)? != 0,
            })
        })?;
        rows.collect()
    }

    pub fn setting(&self, key: &str, default: &str) -> String {
        self.conn
            .query_row("SELECT value FROM settings WHERE key = ?1", [key], |r| r.get(0))
            .unwrap_or_else(|_| default.to_string())
    }

    pub fn set_setting(&self, key: &str, value: &str) -> rusqlite::Result<()> {
        self.conn.execute(
            "INSERT INTO settings (key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = ?2",
            params![key, value],
        )?;
        Ok(())
    }
}

pub fn default_rotation() -> Vec<ExerciseDef> {
    let lift = |name: &str, weight: f64| ExerciseDef {
        name: name.into(),
        kind: ExerciseKind::Rep,
        default_reps: 10,
        default_weight: weight,
        target_seconds: 0.0,
    };
    vec![
        lift("bench", 95.0),
        lift("row", 65.0),
        lift("squat", 115.0),
        lift("overhead", 55.0),
        lift("curl", 25.0),
    ]
}

pub fn default_continuous_pool() -> Vec<ExerciseDef> {
    vec![
        ExerciseDef {
            name: "jumprope".into(),
            kind: ExerciseKind::Continuous,
            default_reps: 0,
            default_weight: 0.0,
            target_seconds: 60.0,
        },
        ExerciseDef {
            name: "stretch".into(),
            kind: ExerciseKind::Continuous,
            default_reps: 0,
            default_weight: 0.0,
            target_seconds: 30.0,
        },
    ]
}

#[cfg(test)]
mod workout_tests {
    use super::*;
    #[test]
    fn workouts_close_once_and_sets_join_them() {
        let path = std::env::temp_dir().join(format!("rfp-store-{}.sqlite", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let s = Store::open(&path).unwrap();
        s.insert_workout("w1", "upper-a", "Upper A", "slot1", "2026-11-23").unwrap();
        assert!(s.started_on("2026-11-23"));
        s.record_set(&SetRecord { date: "2026-11-23".into(), exercise: "overhead".into(), kind: ExerciseKind::Rep, reps: 12, seconds: 0., weight: 35.0, verified: false }).unwrap();
        s.tag_last_set("w1").unwrap();
        s.record_set(&SetRecord { date: "2026-11-24".into(), exercise: "overhead".into(), kind: ExerciseKind::Rep, reps: 10, seconds: 0., weight: 40.0, verified: true }).unwrap();
        s.tag_last_set("w1").unwrap();
        assert_eq!(s.last_weight("overhead"), Some(40.0));
        assert!(s.finish_workout("w1", "completed", "2026-11-24").unwrap());
        assert!(!s.finish_workout("w1", "partial", "2026-11-24").unwrap()); // exactly once
        let w = s.workouts_between("2026-11-23", "2026-11-29").unwrap();
        assert_eq!((w[0]["status"].as_str(), w[0]["sets"].as_i64(), w[0]["date"].as_str()), (Some("completed"), Some(2), Some("2026-11-24")));
        assert_eq!(s.sets(None, None, Some("overhead")).unwrap().len(), 2);
        drop(s); let _ = std::fs::remove_file(&path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{ExerciseKind, SetRecord};

    fn open_tmp() -> (tempfile::TempDir, Store) {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("reps.sqlite")).unwrap();
        (dir, store)
    }

    #[test]
    fn seeds_default_rotation_once() {
        let (dir, store) = open_tmp();
        let rot = store.load_rotation().unwrap();
        assert!(!rot.is_empty());
        // reopen: still there, not duplicated
        drop(store);
        let store = Store::open(&dir.path().join("reps.sqlite")).unwrap();
        assert_eq!(store.load_rotation().unwrap().len(), rot.len());
    }

    #[test]
    fn pointer_state_roundtrips() {
        let (_dir, store) = open_tmp();
        assert_eq!(store.load_pointer_state().unwrap(), (0, 0, String::new()));
        store.save_pointer_state(3, 5, "2026-07-19").unwrap();
        assert_eq!(
            store.load_pointer_state().unwrap(),
            (3, 5, "2026-07-19".to_string())
        );
    }

    #[test]
    fn history_newest_first() {
        let (_dir, store) = open_tmp();
        for (i, name) in ["bench", "row"].iter().enumerate() {
            store
                .record_set(&SetRecord {
                    date: format!("2026-07-1{i}"),
                    exercise: name.to_string(),
                    kind: ExerciseKind::Rep,
                    reps: 10,
                    seconds: 0.0,
                    weight: 100.0,
                    verified: true,
                })
                .unwrap();
        }
        let h = store.history(10).unwrap();
        assert_eq!(h.len(), 2);
        assert_eq!(h[0].exercise, "row");
    }

    #[test]
    fn settings_roundtrip_with_default() {
        let (_dir, store) = open_tmp();
        assert_eq!(store.setting("work_minutes", "6"), "6");
        store.set_setting("work_minutes", "25").unwrap();
        assert_eq!(store.setting("work_minutes", "6"), "25");
    }

    // Source of truth: vision/src/reps_vision/exercises.py SPECS (rep
    // exercises) and the jumprope/stretch activities under
    // vision/src/reps_vision/activities/. Seeded rotation/pool names must
    // stay in sync with what the vision sidecar actually knows how to
    // detect, or the app will prescribe an exercise the detector rejects.
    const KNOWN_VISION_REP_EXERCISES: &[&str] =
        &["bench", "curl", "deadlift", "incline_bench", "overhead", "pullup", "pushup", "row", "squat"];
    const KNOWN_VISION_CONTINUOUS_EXERCISES: &[&str] = &["jumprope", "stretch"];

    #[test]
    fn default_rotation_names_match_known_vision_exercises() {
        for def in default_rotation() {
            assert!(
                KNOWN_VISION_REP_EXERCISES.contains(&def.name.as_str()),
                "default_rotation() seeds {:?}, which is not in the known-vision set {:?}",
                def.name,
                KNOWN_VISION_REP_EXERCISES
            );
        }
    }

    #[test]
    fn default_continuous_pool_names_match_known_vision_exercises() {
        for def in default_continuous_pool() {
            assert!(
                KNOWN_VISION_CONTINUOUS_EXERCISES.contains(&def.name.as_str()),
                "default_continuous_pool() seeds {:?}, which is not in the known-vision set {:?}",
                def.name,
                KNOWN_VISION_CONTINUOUS_EXERCISES
            );
        }
    }
}
