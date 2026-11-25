use crate::{
    bundle::*,
    command::{lines, run},
    p4::prune,
};
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    fs,
    path::{Path, PathBuf},
    process::Command,
};

pub struct Lore {
    pub repo: PathBuf,
    pub access: Access,
}
#[derive(Clone, Copy, PartialEq)]
pub enum Access {
    Offline,
    Remote,
    Connected,
}
type Events = Vec<Value>;

fn data<'a>(events: &'a [Value], tag: &'a str) -> impl Iterator<Item = &'a Value> {
    events
        .iter()
        .filter(move |v| v["tagName"] == tag)
        .map(|v| &v["data"])
}
fn one<'a>(events: &'a [Value], tag: &str) -> Result<&'a Value> {
    events
        .iter()
        .find(|v| v["tagName"] == tag)
        .map(|v| &v["data"])
        .with_context(|| format!("missing Lore event {tag}"))
}
fn nonzero(s: &str) -> bool {
    s.bytes().any(|b| b != b'0')
}
fn parents(info: &Value) -> Result<Vec<String>> {
    info["parent"]
        .as_array()
        .context("missing revision parents")?
        .iter()
        .map(|v| Ok(v.as_str().context("invalid revision parent")?.to_owned()))
        .filter(|v| v.as_ref().is_err() || v.as_ref().is_ok_and(|s| nonzero(s)))
        .collect()
}

#[derive(Clone)]
struct FileNode {
    address: String,
    size: u64,
    mode: u64,
    id: u64,
}
struct Tree {
    files: BTreeMap<String, FileNode>,
    dirs: BTreeMap<String, u64>,
}

impl Lore {
    fn events(&self, args: &[&str]) -> Result<Events> {
        let mut cmd = Command::new("lore");
        cmd.current_dir(&self.repo)
            .env_remove("LORE_P4_CHANGELIST")
            .args(["--json", "--no-pager", "--non-interactive"]);
        match self.access {
            Access::Offline => {
                cmd.arg("--offline");
            }
            Access::Remote => {
                cmd.arg("--remote");
            }
            Access::Connected => {}
        }
        cmd.args(args);
        let events = lines(&run(cmd, None)?)?;
        // Some 0.14 commands resolve display names as a second operation. Its
        // offline "No remote configured" terminal is independent of the actual
        // operation. Require an explicit success as well as expected data.
        let complete: Vec<_> = data(&events, "complete").collect();
        ensure!(
            complete.iter().any(|v| v["status"] == 0),
            "Lore {} produced no successful completion",
            args.join(" ")
        );
        for v in complete {
            let optional_lookup = matches!(
                v["error"]["message"].as_str(),
                Some(
                    "No remote configured"
                        | "Operation not supported: No authentication configured on server"
                )
            ) && events.iter().any(|e| {
                matches!(
                    e["tagName"].as_str(),
                    Some("revisionInfo" | "revisionCommitRevision" | "metadata")
                )
            });
            ensure!(v["status"] == 0 || optional_lookup, "Lore: {}", v["error"]);
        }
        Ok(events)
    }
    fn tree(&self, id: &str) -> Result<Tree> {
        let events = self.events(&["repository", "dump", "--revision", id])?;
        ensure!(
            one(&events, "repositoryStateDump")?["revision"] == id,
            "Lore returned the wrong tree for {id}"
        );
        one(&events, "repositoryDumpEnd")?;
        let mut tree = Tree {
            files: BTreeMap::new(),
            dirs: BTreeMap::new(),
        };
        for v in data(&events, "repositoryStateDumpNode") {
            let path = field(v, "name")?;
            let desc = field(v, "typeData")?;
            let mode = v["mode"].as_u64().context("invalid file mode")?;
            if desc.starts_with("child ") {
                let p = path
                    .strip_suffix('/')
                    .context("unexpected Lore directory")?;
                checked_path(p)?;
                tree.dirs.insert(p.to_owned(), mode);
            } else {
                let address = desc
                    .strip_prefix("addr ")
                    .with_context(|| format!("unsupported Lore node {path}: {desc}"))?;
                if path != MARKER {
                    checked_path(path)?;
                }
                ensure!(
                    tree.files
                        .insert(
                            path.into(),
                            FileNode {
                                address: address.into(),
                                mode,
                                size: v["size"].as_u64().context("missing file size")?,
                                id: v["id"].as_u64().context("missing node ID")?
                            }
                        )
                        .is_none(),
                    "duplicate Lore path {path}"
                );
            }
        }
        Ok(tree)
    }
    fn write(&self, id: &str, path: &str, out: &Path) -> Result<()> {
        let out = std::path::absolute(out)?;
        let events = self.events(&[
            "--force",
            "file",
            "write",
            "--revision",
            id,
            "--path",
            path,
            "--output",
            out.to_str().context("non-UTF-8 output path")?,
        ])?;
        one(&events, "fileWrite")?;
        Ok(())
    }
    pub fn export(&self, b: &Bundle) -> Result<History> {
        let rows = self.events(&["branch", "list", "--archived"])?;
        let branches: BTreeMap<_, _> = data(&rows, "branchListEntry")
            .map(|v| Ok((field(v, "id")?.to_owned(), v.clone())))
            .collect::<Result<_>>()?;
        ensure!(!branches.is_empty(), "no branches reported by Lore");
        let mut h = History {
            format: FORMAT.into(),
            system: "lore".into(),
            scope: json!({"branches":branches}),
            revisions: vec![],
        };
        // Parent-first traversal of both edges, including shared ancestry just once.
        let mut stack: Vec<_> = branches
            .values()
            .map(|b| Ok((field(b, "latest")?.to_owned(), false)))
            .collect::<Result<_>>()?;
        let mut pending = HashMap::new();
        let mut done = HashSet::new();
        let mut prior: HashMap<String, Tree> = HashMap::new();
        let mut branch_tip = HashMap::new();
        let mut revision_branch = HashMap::new();
        let mut blobs = HashMap::new();
        let mut repository = None;
        while let Some((id, ready)) = stack.pop() {
            if !nonzero(&id) || done.contains(&id) {
                continue;
            }
            if !ready {
                ensure!(!pending.contains_key(&id), "cyclic history at {id}");
                let info = self.events(&["revision", "info", "--metadata", "--delta", &id])?;
                let header = one(&info, "revisionInfo")?;
                ensure!(header["revision"] == id, "Lore returned the wrong revision");
                repository = Some(field(header, "repository")?.to_owned());
                stack.push((id.clone(), true));
                for p in parents(header)?.into_iter().rev() {
                    stack.push((p, false));
                }
                pending.insert(id, info);
                continue;
            }
            eprintln!("Reading Lore revision {id}");
            let events = pending
                .remove(&id)
                .context("missing revision information")?;
            let header = one(&events, "revisionInfo")?;
            let mut meta = BTreeMap::new();
            // File metadata follows revisionInfoDelta in the event stream.
            for v in events
                .iter()
                .skip_while(|v| v["tagName"] != "revisionInfo")
                .skip(1)
                .take_while(|v| v["tagName"] == "metadata")
            {
                meta.insert(
                    field(&v["data"], "key")?.to_owned(),
                    v["data"]["value"]["data"].clone(),
                );
            }
            let branch = meta
                .get("branch")
                .and_then(Value::as_str)
                .context("revision has no branch ID")?
                .to_owned();
            ensure!(
                branches.contains_key(&branch),
                "ancestry uses an unlisted branch {branch}; ensure all branches are visible"
            );
            let prefix = if branches.len() == 1 {
                String::new()
            } else {
                format!("branches/{branch}/")
            };
            let next = self.tree(&id)?;
            let previous = prior.get(&branch);
            let old = previous.map(|t| &t.files);
            let old_paths: HashMap<_, _> = old
                .into_iter()
                .flat_map(|m| m.iter())
                .map(|(p, n)| (n.id, p))
                .collect();
            let mut edits = vec![];
            if let Some(old) = old {
                for p in old
                    .keys()
                    .filter(|p| p.as_str() != MARKER && !next.files.contains_key(*p))
                {
                    edits.push(Edit {
                        path: format!("{prefix}{p}"),
                        blob: None,
                        file_type: None,
                        moved_from: None,
                    });
                }
            }
            let mut original = None;
            for (path, n) in &next.files {
                if old
                    .and_then(|t| t.get(path))
                    .is_some_and(|o| o.address == n.address && o.mode == n.mode)
                {
                    continue;
                }
                let blob = if let Some(hit) = blobs.get(&n.address) {
                    String::clone(hit)
                } else {
                    let blob = b.capture(|out| {
                        self.write(&id, path, out)?;
                        ensure!(
                            fs::metadata(out)?.len() == n.size,
                            "size mismatch in {id}: {path}"
                        );
                        Ok(())
                    })?;
                    blobs.insert(n.address.clone(), blob.clone());
                    blob
                };
                if path == MARKER {
                    original = Some(read_marker(&b.blob(&blob))?);
                    continue;
                }
                let moved_from = old_paths
                    .get(&n.id)
                    .filter(|p| p.as_str() != path && !next.files.contains_key(p.as_str()))
                    .map(|p| format!("{prefix}{p}"));
                edits.push(Edit {
                    path: format!("{prefix}{path}"),
                    blob: Some(blob),
                    file_type: Some(
                        if n.mode & 0o111 != 0 {
                            "binary+x"
                        } else {
                            "binary"
                        }
                        .into(),
                    ),
                    moved_from,
                });
            }
            let mut rev = Revision {
                id: id.clone(),
                parents: parents(header)?,
                author: meta
                    .get("created-by")
                    .or_else(|| meta.get("committed-by"))
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .into(),
                time_ms: meta
                    .get("timestamp")
                    .and_then(Value::as_i64)
                    .context("missing Lore timestamp")?,
                message: meta
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .into(),
                edits,
                metadata: json!({"branch":branch,"directories":next.dirs,"events":events.iter().filter(|e|matches!(e["tagName"].as_str(),Some("revisionInfo"|"metadata"|"revisionInfoDelta"))).collect::<Vec<_>>()}),
            };
            if branches.len() > 1 {
                for parent in &rev.parents {
                    let source_branch = revision_branch
                        .get(parent)
                        .context("missing parent branch")?;
                    if source_branch == &branch {
                        continue;
                    }
                    let parent_tree = self.tree(parent)?;
                    for e in &rev.edits {
                        let Some(blob) = &e.blob else { continue };
                        let path = e
                            .path
                            .strip_prefix(&prefix)
                            .context("invalid branch projection")?;
                        if e.moved_from.is_none()
                            && parent_tree
                                .files
                                .get(path)
                                .and_then(|n| blobs.get(&n.address))
                                == Some(blob)
                        {
                            rev.metadata["branchSources"][&e.path] = json!({"path":format!("branches/{source_branch}/{path}"),"revision":parent});
                        }
                    }
                }
            }
            if let Some(o) = original {
                // Lore may elide an unchanged file or a P4-only type change
                // from its delta. Reconstitute the touched revision only if
                // the committed tree independently confirms the original bytes.
                for old_edit in &o.edits {
                    let Some(old_blob) = &old_edit.blob else {
                        continue;
                    };
                    let dest = format!("{prefix}{}", old_edit.path);
                    if !rev.edits.iter().any(|e| e.path == dest)
                        && next
                            .files
                            .get(&old_edit.path)
                            .and_then(|n| blobs.get(&n.address))
                            == Some(old_blob)
                    {
                        let mut e = old_edit.clone();
                        e.path = dest;
                        rev.edits.push(e);
                    }
                }
                original_fields(&mut rev, o);
            }
            prior.insert(branch.clone(), next);
            revision_branch.insert(id.clone(), branch.clone());
            branch_tip.insert(branch, id.clone());
            done.insert(id);
            h.revisions.push(rev);
        }
        // A newly created branch may still point into its parent's history.
        // Materialize that shared snapshot as a separate projection in P4.
        if branches.len() > 1 {
            for (branch, info) in &branches {
                let tip = field(info, "latest")?;
                if !nonzero(tip) || branch_tip.get(branch).is_some_and(|t| t == tip) {
                    continue;
                }
                ensure!(
                    !branch_tip.contains_key(branch),
                    "branch {branch} has inconsistent local history"
                );
                let tree = self.tree(tip)?;
                let mut edits = vec![];
                for (p, n) in &tree.files {
                    if p == MARKER {
                        continue;
                    }
                    let blob = blobs
                        .get(&n.address)
                        .context("missing ancestor blob")?
                        .clone();
                    edits.push(Edit {
                        path: format!("branches/{branch}/{p}"),
                        blob: Some(blob),
                        file_type: Some(
                            if n.mode & 0o111 != 0 {
                                "binary+x"
                            } else {
                                "binary"
                            }
                            .into(),
                        ),
                        moved_from: None,
                    });
                }
                h.revisions.push(Revision {
                    id: format!("branch-{branch}-{tip}"),
                    parents: vec![tip.into()],
                    author: info["creator"].as_str().unwrap_or("").into(),
                    time_ms: info["created"].as_i64().unwrap_or(0),
                    message: format!("Create Lore branch {}", field(info, "name")?),
                    edits,
                    metadata: json!({"branchPoint":info,"directories":tree.dirs}),
                });
            }
        }
        h.scope["repository"] = json!(repository);
        Ok(h)
    }
    pub fn import(&self, b: &Bundle, h: &History, url: Option<&str>) -> Result<()> {
        fs::create_dir(&self.repo).with_context(|| {
            format!(
                "Lore destination must be a new directory: {}",
                self.repo.display()
            )
        })?;
        let name = self
            .repo
            .file_name()
            .and_then(|v| v.to_str())
            .context("invalid repository name")?;
        one(
            &self.events(&["repository", "create", "--", url.unwrap_or(name)])?,
            "repositoryCreate",
        )?;
        let verify = tempfile::NamedTempFile::new()?;
        let mut expected = BTreeMap::new();
        let mut tip = String::new();
        for rev in &h.revisions {
            eprintln!("Committing source revision {}", rev.id);
            for e in rev.edits.iter().filter(|e| e.blob.is_none()) {
                let p = self.repo.join(&e.path);
                fs::remove_file(&p)?;
                prune(&p, &self.repo)?;
            }
            for e in rev.edits.iter().filter(|e| e.blob.is_some()) {
                let path = self.repo.join(&e.path);
                fs::create_dir_all(path.parent().unwrap())?;
                fs::copy(b.blob(e.blob.as_ref().unwrap()), &path)?;
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    let executable = e
                        .file_type
                        .as_deref()
                        .is_some_and(|t| t.split('+').skip(1).any(|m| m.contains('x')));
                    fs::set_permissions(
                        &path,
                        fs::Permissions::from_mode(if executable { 0o755 } else { 0o644 }),
                    )?;
                }
            }
            let record = self.repo.join(MARKER);
            fs::write(&record, serde_json::to_vec(&marker(h, rev))?)?;
            for (from, to) in paired_moves(rev) {
                self.events(&["--force", "file", "stage", "move", "--", from, &to.path])?;
            }
            self.events(&["--force", "stage", "--scan", "."])?;
            let mut args = vec![
                "revision",
                "metadata",
                "set",
                "--",
                "lorep4-source-id",
                &rev.id,
            ];
            let time = rev.time_ms.to_string();
            args.extend(["lorep4-source-time-ms", &time]);
            if !rev.author.is_empty() {
                args.extend(["created-by", &rev.author]);
            }
            if h.system == "p4" {
                args.extend(["p4-changelist", &rev.id]);
            }
            self.events(&args)?;
            let committed = self.events(&["commit", "--", &rev.message])?;
            let id = field(one(&committed, "revisionCommitRevision")?, "revision")?;
            let tree = self.tree(id)?;
            for e in &rev.edits {
                if let Some(blob) = &e.blob {
                    self.write(id, &e.path, verify.path())?;
                    ensure!(
                        hash(verify.path())? == *blob,
                        "Lore content mismatch at {id}: {}",
                        e.path
                    );
                    expected.insert(
                        e.path.clone(),
                        tree.files
                            .get(&e.path)
                            .context("missing committed file")?
                            .address
                            .clone(),
                    );
                } else {
                    expected.remove(&e.path);
                }
            }
            self.write(id, MARKER, verify.path())?;
            ensure!(
                hash(&record)? == hash(verify.path())?,
                "Lore history marker mismatch"
            );
            let actual: BTreeMap<_, _> = tree
                .files
                .into_iter()
                .filter(|(p, _)| p != MARKER)
                .map(|(p, n)| (p, n.address))
                .collect();
            ensure!(actual == expected, "Lore tree mismatch at {id}");
            println!("{} -> Lore {id}", rev.id);
            tip = id.into();
        }
        if url.is_some() {
            self.events(&["push"])?;
            let remote = self.events(&["--remote", "branch", "list"])?;
            ensure!(
                data(&remote, "branchListEntry")
                    .any(|v| v["location"] == "remote" && v["latest"] == tip),
                "push completed but remote branch tip differs"
            );
        }
        Ok(())
    }
}
