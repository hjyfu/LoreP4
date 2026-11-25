use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::{BTreeSet, HashMap, HashSet},
    fs::{self, File},
    io::{BufReader, BufWriter, Write},
    path::{Path, PathBuf},
};
use tempfile::NamedTempFile;

pub const MARKER: &str = ".lorep4-history.json";
pub const FORMAT: &str = "LoreP4/1";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Edit {
    pub path: String,
    pub blob: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub moved_from: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Revision {
    pub id: String,
    pub parents: Vec<String>,
    pub author: String,
    pub time_ms: i64,
    pub message: String,
    pub edits: Vec<Edit>,
    pub metadata: Value,
}

#[derive(Serialize, Deserialize)]
pub struct History {
    pub format: String,
    pub system: String,
    pub scope: Value,
    pub revisions: Vec<Revision>,
}

pub struct Bundle(pub PathBuf);

pub fn hash(path: &Path) -> Result<String> {
    let mut hasher = blake3::Hasher::new();
    std::io::copy(&mut File::open(path)?, &mut hasher)?;
    Ok(hasher.finalize().to_hex().to_string())
}

// Apply the same portable path rules on every host, including for a bundle
// produced on Linux and subsequently replayed on Windows.
pub fn checked_path(path: &str) -> Result<()> {
    ensure!(!path.is_empty(), "empty repository path");
    for c in path.split('/') {
        let lower = c.to_ascii_lowercase();
        let stem = lower.split('.').next().unwrap_or("");
        ensure!(
            !c.is_empty()
                && !c.ends_with(['.', ' '])
                && !c.contains(['\\', ':', '*', '?', '"', '<', '>', '|'])
                && !c.chars().any(char::is_control)
                && !matches!(lower.as_str(), ".lore" | ".git" | ".hg" | ".svn")
                && !matches!(stem, "con" | "aux" | "prn" | "nul" | "conin$" | "conout$")
                && !((stem.starts_with("com") || stem.starts_with("lpt"))
                    && stem.chars().count() == 4
                    && stem
                        .chars()
                        .last()
                        .is_some_and(|v| "0123456789¹²³".contains(v))),
            "unrepresentable or reserved path {path:?}"
        );
    }
    ensure!(
        !path.split('/').next().unwrap().eq_ignore_ascii_case(MARKER),
        "reserved path {path:?}"
    );
    Ok(())
}

impl Bundle {
    pub fn create(dir: &Path) -> Result<Self> {
        fs::create_dir(dir)
            .with_context(|| format!("bundle path must be new: {}", dir.display()))?;
        fs::create_dir(dir.join("blobs"))?;
        Ok(Self(dir.to_owned()))
    }
    pub fn blob(&self, id: &str) -> PathBuf {
        self.0.join("blobs").join(id)
    }
    pub fn capture(&self, write: impl FnOnce(&Path) -> Result<()>) -> Result<String> {
        let tmp = NamedTempFile::new_in(self.0.join("blobs"))?.into_temp_path();
        write(&tmp)?;
        let id = hash(&tmp)?;
        let target = self.blob(&id);
        if !target.exists() {
            tmp.persist_noclobber(&target)?;
        }
        Ok(id)
    }
    pub fn finish(&self, h: &History) -> Result<()> {
        self.validate(h)?;
        let mut tmp = NamedTempFile::new_in(&self.0)?;
        {
            let mut out = BufWriter::new(tmp.as_file_mut());
            serde_json::to_writer(&mut out, h)?;
            out.flush()?;
        }
        tmp.persist_noclobber(self.0.join("history.json"))?;
        Ok(())
    }
    pub fn read(&self) -> Result<History> {
        let history =
            serde_json::from_reader(BufReader::new(File::open(self.0.join("history.json"))?))?;
        self.validate(&history)?;
        Ok(history)
    }
    fn validate(&self, h: &History) -> Result<()> {
        ensure!(
            h.format == FORMAT && matches!(h.system.as_str(), "p4" | "lore"),
            "unknown bundle format"
        );
        ensure!(
            !h.revisions.is_empty(),
            "no submitted revisions in the selected source"
        );
        let mut ids = HashSet::new();
        let mut blobs = HashSet::new();
        let mut live = BTreeSet::new();
        let mut names: HashMap<String, (&str, usize)> = HashMap::new();
        for rev in &h.revisions {
            ensure!(
                rev.parents.iter().all(|p| ids.contains(p.as_str())),
                "incomplete history at {}",
                rev.id
            );
            ensure!(
                !rev.id.is_empty() && ids.insert(rev.id.as_str()),
                "duplicate or empty revision ID"
            );
            let mut changed = HashSet::new();
            for e in &rev.edits {
                checked_path(&e.path)?;
                ensure!(changed.insert(&e.path), "duplicate edit for {}", e.path);
                if let Some(p) = &e.moved_from {
                    checked_path(p)?;
                }
                match &e.blob {
                    Some(b) => {
                        ensure!(
                            b.len() == 64 && b.bytes().all(|c| matches!(c,b'0'..=b'9'|b'a'..=b'f')),
                            "invalid blob ID"
                        );
                        if blobs.insert(b) {
                            ensure!(hash(&self.blob(b))? == *b, "corrupt blob {b}");
                        }
                    }
                    None => {
                        ensure!(
                            live.remove(e.path.as_str()),
                            "delete without source history: {}",
                            e.path
                        );
                        for p in prefixes(&e.path) {
                            let key = p.to_lowercase();
                            let (_, count) = names.get_mut(&key).context("invalid path history")?;
                            *count -= 1;
                            if *count == 0 {
                                names.remove(&key);
                            }
                        }
                    }
                }
            }
            for e in rev.edits.iter().filter(|e| e.blob.is_some()) {
                if live.contains(e.path.as_str()) {
                    continue;
                }
                ensure!(
                    !names.contains_key(&e.path.to_lowercase()),
                    "file/directory or case collision: {}",
                    e.path
                );
                for part in prefixes(&e.path) {
                    ensure!(!live.contains(part), "file/directory conflict: {part}");
                    let (spelling, count) = names.entry(part.to_lowercase()).or_insert((part, 0));
                    ensure!(*spelling == part, "case collision: {spelling} and {part}");
                    *count += 1;
                }
                live.insert(e.path.as_str());
            }
        }
        Ok(())
    }
}

fn prefixes(path: &str) -> impl Iterator<Item = &str> {
    path.match_indices('/')
        .map(|(i, _)| &path[..i])
        .chain(std::iter::once(path))
}

// Tracked alongside each changeset. A new version also represents a commit
// that changes only a file type, an empty directory, or commit metadata.
pub fn marker(h: &History, rev: &Revision) -> Value {
    serde_json::json!({"format":FORMAT,"system":h.system,"scope":h.scope,"revision":rev})
}

pub fn read_marker(path: &Path) -> Result<Revision> {
    let value: Value = serde_json::from_reader(BufReader::new(File::open(path)?))
        .context("reserved history filename contains unrelated data")?;
    ensure!(
        value["format"] == FORMAT,
        "reserved history filename contains unrelated data"
    );
    Ok(serde_json::from_value(value["revision"].clone())?)
}

pub fn original_fields(rev: &mut Revision, original: Revision) {
    rev.author = original.author.clone();
    rev.time_ms = original.time_ms;
    if rev.message.trim_end_matches(['\r', '\n']) == original.message.trim_end_matches(['\r', '\n'])
    {
        rev.message.clone_from(&original.message);
    }
    for e in &mut rev.edits {
        if let Some(old) = original
            .edits
            .iter()
            .find(|o| o.path == e.path && o.blob == e.blob)
        {
            e.file_type.clone_from(&old.file_type);
            e.moved_from.clone_from(&old.moved_from);
        }
    }
    rev.metadata["original"] = serde_json::to_value(original).expect("serializable revision");
}

/// A move crossing the selected subtree has only one visible half. Replay
/// those as adds/deletes and keep the complete integration record as metadata.
pub fn paired_moves(rev: &Revision) -> impl Iterator<Item = (&str, &Edit)> {
    let deleted: HashSet<&str> = rev
        .edits
        .iter()
        .filter(|e| e.blob.is_none())
        .map(|e| e.path.as_str())
        .collect();
    rev.edits.iter().filter_map(move |e| {
        let from = e.moved_from.as_deref()?;
        (e.blob.is_some() && deleted.contains(from)).then_some((from, e))
    })
}

pub fn field<'a>(v: &'a Value, k: &str) -> Result<&'a str> {
    v[k].as_str()
        .with_context(|| format!("missing string field {k}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rejects_unsafe_paths_on_all_hosts() {
        for p in [
            "../x",
            "a/../x",
            "AUX.txt",
            ".LoRe/HEAD",
            "a\\b",
            "x:stream",
            "/abs",
            "a//b",
            "x./y",
            MARKER,
        ] {
            assert!(checked_path(p).is_err(), "{p}");
        }
        assert!(checked_path("赛车/发动机#1@50%.bin").is_ok());
    }
}
