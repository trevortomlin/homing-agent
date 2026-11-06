//! Device data layout and crash-safe JSON storage.

use std::collections::BTreeMap;
use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

/// Paths under the data dir.
#[derive(Debug, Clone)]
pub struct Layout {
    pub root: PathBuf,
}

impl Layout {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }
    pub fn agent_state(&self) -> PathBuf {
        self.root.join("agent-state.json")
    }
    pub fn floors(&self) -> PathBuf {
        self.root.join("floors.json")
    }
    pub fn incoming(&self) -> PathBuf {
        self.root.join("incoming")
    }
    pub fn apps(&self) -> PathBuf {
        self.root.join("apps")
    }
    pub fn app(&self, id: &str) -> PathBuf {
        self.apps().join(id)
    }
    pub fn slot(&self, id: &str, slot: Slot) -> PathBuf {
        self.app(id).join(slot.name())
    }
    pub fn current(&self, id: &str) -> PathBuf {
        self.app(id).join("current")
    }
    pub fn slots_file(&self, id: &str) -> PathBuf {
        self.app(id).join("slots.json")
    }
    pub fn state_dir(&self, id: &str) -> PathBuf {
        self.app(id).join("state")
    }

    /// Create the top-level directories.
    pub fn ensure(&self) -> Result<()> {
        for d in [self.root.clone(), self.incoming(), self.apps()] {
            fs::create_dir_all(&d).with_context(|| format!("cannot create {}", d.display()))?;
        }
        Ok(())
    }
}

/// An A/B slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Slot {
    A,
    B,
}

impl Slot {
    pub fn name(self) -> &'static str {
        match self {
            Slot::A => "a",
            Slot::B => "b",
        }
    }
    pub fn other(self) -> Slot {
        match self {
            Slot::A => Slot::B,
            Slot::B => Slot::A,
        }
    }
    pub fn parse(s: &str) -> Option<Slot> {
        match s {
            "a" => Some(Slot::A),
            "b" => Some(Slot::B),
            _ => None,
        }
    }
}

/// Status of a slot's content.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SlotStatus {
    /// Extracted and verified, not yet running.
    Staged,
    /// Running on trial.
    Trial,
    /// The committed version.
    Committed,
    /// A previously committed version kept as rollback target.
    Previous,
    /// Failed its trial.
    Bad,
}

/// What a slot holds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SlotInfo {
    pub version: String,
    pub version_code: u64,
    pub status: SlotStatus,
    /// sha256 of `.homing-meta/manifest.json`, checked by the launcher.
    pub manifest_sha256: String,
}

/// `apps/<id>/slots.json`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SlotsFile {
    #[serde(default)]
    pub a: Option<SlotInfo>,
    #[serde(default)]
    pub b: Option<SlotInfo>,
    #[serde(default)]
    pub committed: Option<Slot>,
}

impl SlotsFile {
    pub fn get(&self, s: Slot) -> Option<&SlotInfo> {
        match s {
            Slot::A => self.a.as_ref(),
            Slot::B => self.b.as_ref(),
        }
    }
    pub fn get_mut(&mut self, s: Slot) -> &mut Option<SlotInfo> {
        match s {
            Slot::A => &mut self.a,
            Slot::B => &mut self.b,
        }
    }
    pub fn set_status(&mut self, s: Slot, st: SlotStatus) {
        if let Some(i) = self.get_mut(s).as_mut() {
            i.status = st;
        }
    }
    /// The slot a new install goes into: never the committed one.
    pub fn inactive(&self) -> Slot {
        match self.committed {
            Some(s) => s.other(),
            None => Slot::A,
        }
    }
}

/// `floors.json`: app id -> minimum accepted version_code.
pub type Floors = BTreeMap<String, u64>;

/// Read JSON, returning `Default` if the file does not exist.
pub fn read_json<T: DeserializeOwned + Default>(path: &Path) -> Result<T> {
    match fs::read(path) {
        Ok(b) => serde_json::from_slice(&b).with_context(|| format!("corrupt {}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(T::default()),
        Err(e) => Err(e).with_context(|| format!("cannot read {}", path.display())),
    }
}

/// Atomic, durable write: tmp file, fsync, rename, fsync dir.
pub fn write_json<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    let mut bytes = serde_json::to_vec_pretty(value)?;
    bytes.push(b'\n');
    write_atomic(path, &bytes)
}

/// Atomic, durable write of raw bytes.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let tmp = path.with_extension("tmp");
    {
        let mut f =
            fs::File::create(&tmp).with_context(|| format!("cannot create {}", tmp.display()))?;
        f.write_all(bytes)?;
        f.sync_all()?;
    }
    fs::rename(&tmp, path).with_context(|| format!("cannot rename onto {}", path.display()))?;
    fsync_dir(path.parent().unwrap_or(Path::new(".")))
}

/// fsync a directory so renames inside it are durable.
pub fn fsync_dir(dir: &Path) -> Result<()> {
    fs::File::open(dir)
        .and_then(|f| f.sync_all())
        .with_context(|| format!("cannot fsync {}", dir.display()))
}

/// Recursively fsync every file and directory under `dir`.
pub fn fsync_tree(dir: &Path) -> Result<()> {
    for ent in fs::read_dir(dir)? {
        let ent = ent?;
        let ft = ent.file_type()?;
        if ft.is_dir() {
            fsync_tree(&ent.path())?;
        } else if ft.is_file() {
            fs::File::open(ent.path())?.sync_all()?;
        }
    }
    fsync_dir(dir)
}

/// Atomically point `link` at `target` (relative), via symlink + rename.
pub fn swap_symlink(link: &Path, target: &str) -> Result<()> {
    let tmp = link.with_extension("tmp");
    let _ = fs::remove_file(&tmp);
    std::os::unix::fs::symlink(target, &tmp)
        .with_context(|| format!("cannot create {}", tmp.display()))?;
    fs::rename(&tmp, link).with_context(|| format!("cannot rename onto {}", link.display()))?;
    fsync_dir(link.parent().unwrap_or(Path::new(".")))
}

/// Which slot `current` points to, if any.
pub fn current_slot(layout: &Layout, id: &str) -> Option<Slot> {
    let t = fs::read_link(layout.current(id)).ok()?;
    Slot::parse(t.to_str()?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_round_trip_and_default() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("slots.json");
        let s: SlotsFile = read_json(&p).unwrap();
        assert_eq!(s, SlotsFile::default());
        let mut s = SlotsFile {
            committed: Some(Slot::B),
            ..Default::default()
        };
        *s.get_mut(Slot::B) = Some(SlotInfo {
            version: "1.0.0".into(),
            version_code: 1,
            status: SlotStatus::Committed,
            manifest_sha256: "x".into(),
        });
        write_json(&p, &s).unwrap();
        assert_eq!(read_json::<SlotsFile>(&p).unwrap(), s);
        assert_eq!(s.inactive(), Slot::A);
        assert!(!p.with_extension("tmp").exists());
    }

    #[test]
    fn symlink_swap() {
        let d = tempfile::tempdir().unwrap();
        let l = Layout::new(d.path());
        fs::create_dir_all(l.app("com.x.y")).unwrap();
        assert_eq!(current_slot(&l, "com.x.y"), None);
        swap_symlink(&l.current("com.x.y"), "a").unwrap();
        assert_eq!(current_slot(&l, "com.x.y"), Some(Slot::A));
        swap_symlink(&l.current("com.x.y"), "b").unwrap();
        assert_eq!(current_slot(&l, "com.x.y"), Some(Slot::B));
    }
}
