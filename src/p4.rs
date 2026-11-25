use crate::{
    bundle::*,
    command::{lines, run},
};
use anyhow::{Context, Result, bail, ensure};
use serde_json::{Value, json};
use std::{collections::BTreeMap, fs, path::Path, process::Command};

#[derive(clap::Args, Clone)]
pub struct P4 {
    /// P4PORT (explicit to avoid accidentally selecting a server from local config).
    #[arg(long)]
    pub port: String,
    /// Perforce user. Defaults to the authenticated CLI user.
    #[arg(long)]
    pub user: Option<String>,
    /// Depot subtree, e.g. //depot/game/... .
    #[arg(long)]
    pub depot: String,
}

fn unescape(p: &str) -> String {
    p.replace("%40", "@")
        .replace("%23", "#")
        .replace("%2A", "*")
        .replace("%2a", "*")
        .replace("%25", "%")
}
fn escape(p: &str) -> String {
    p.replace('%', "%25")
        .replace('@', "%40")
        .replace('#', "%23")
        .replace('*', "%2A")
}
fn portable_type(t: &str) -> Result<()> {
    let base = t.split('+').next().unwrap_or("");
    ensure!(
        matches!(base, "text" | "binary" | "utf8" | "utf16" | "unicode"),
        "unsupported P4 type {t}; cannot verify a portable regular-file representation"
    );
    ensure!(
        t.chars().all(|c| c.is_ascii_alphanumeric() || c == '+'),
        "invalid P4 type {t}"
    );
    Ok(())
}
impl P4 {
    fn prefix(&self) -> Result<&str> {
        let prefix = self
            .depot
            .strip_suffix("...")
            .context("depot scope must end in /...")?;
        let root = prefix
            .strip_prefix("//")
            .and_then(|s| s.strip_suffix('/'))
            .context("expected //depot/path/...")?;
        checked_path(root)?;
        ensure!(!root.contains("..."), "invalid depot wildcard");
        ensure!(
            !root.contains(['@', '#', '%']),
            "scope must be a literal depot prefix without revision selectors or escapes"
        );
        Ok(prefix)
    }
    fn cmd(&self, client: Option<&str>) -> Command {
        let mut cmd = Command::new("p4");
        cmd.env_remove("PWD");
        cmd.args(["-p", &self.port]);
        if let Some(u) = &self.user {
            cmd.args(["-u", u]);
        }
        if let Some(c) = client {
            cmd.args(["-c", c]);
        }
        cmd
    }
    fn records(&self, client: Option<&str>, args: &[&str]) -> Result<Vec<Value>> {
        let mut cmd = self.cmd(client);
        cmd.args(["-ztag", "-Mj"]);
        // Older Windows p4 clients receive process arguments in the ANSI code
        // page. Feed UTF-8 file operands through p4's argument-file interface.
        let suffix = args.iter().position(|s| !s.is_ascii());
        let input = suffix.map(|i| args[i..].join("\n") + "\n");
        if let Some(i) = suffix {
            cmd.args(["-x", "-"]).args(&args[..i]);
        } else {
            cmd.args(args);
        }
        let records = lines(&run(cmd, input.as_deref().map(str::as_bytes))?)?;
        for r in &records {
            ensure!(
                r["severity"].as_i64().unwrap_or(0) < 2,
                "p4 {}: {}",
                args[0],
                r["data"]
            );
        }
        Ok(records)
    }
    fn changes(&self) -> Result<Vec<String>> {
        self.prefix()?;
        let mut ids = self
            .records(None, &["changes", "-r", "-s", "submitted", &self.depot])?
            .iter()
            .map(|v| Ok(field(v, "change")?.parse::<u64>()?))
            .collect::<Result<Vec<_>>>()?;
        ids.sort_unstable();
        ids.dedup();
        Ok(ids.into_iter().map(|id| id.to_string()).collect())
    }
    fn latest_two(&self) -> Result<Vec<String>> {
        self.records(
            None,
            &["changes", "-m", "2", "-s", "submitted", &self.depot],
        )?
        .iter()
        .map(|v| Ok(field(v, "change")?.to_string()))
        .collect()
    }
    fn print(&self, spec: &str, path: &Path) -> Result<()> {
        let mut cmd = self.cmd(None);
        // The 2024.2 client honors -L only with -o. Use a fresh location
        // so a missing revision cannot accidentally reuse a previous payload.
        let dir = tempfile::tempdir()?;
        let output = dir.path().join("payload");
        cmd.current_dir(dir.path()).args([
            "-x", "-", "print", "-q", "-K", "-L", "unix", "-o", "payload",
        ]);
        run(cmd, Some(format!("{spec}\n").as_bytes()))?;
        let meta = fs::symlink_metadata(&output)
            .with_context(|| format!("p4 print produced no file for {spec}"))?;
        ensure!(meta.is_file(), "P4 returned a non-regular file for {spec}");
        #[cfg(windows)]
        {
            let mut perm = meta.permissions();
            #[allow(clippy::permissions_set_readonly_false)] // Windows-only read-only attribute.
            perm.set_readonly(false);
            fs::set_permissions(&output, perm)?;
        }
        std::io::copy(&mut fs::File::open(output)?, &mut fs::File::create(path)?)?;
        Ok(())
    }
    pub fn export(&self, b: &Bundle) -> Result<History> {
        let prefix = self.prefix()?;
        let mut versions = BTreeMap::new();
        let mut h = History {
            format: FORMAT.into(),
            system: "p4".into(),
            scope: json!({"depot":self.depot}),
            revisions: vec![],
        };
        for id in self.changes()? {
            eprintln!("Reading changelist {id}");
            let rows = self.records(None, &["describe", "-s", &id])?;
            let d = rows.first().context("missing p4 describe result")?;
            ensure!(
                field(d, "change")? == id && d["status"] == "submitted",
                "inaccessible changelist {id}"
            );
            let mut rev = Revision {
                id: id.clone(),
                parents: h
                    .revisions
                    .last()
                    .map(|r| r.id.clone())
                    .into_iter()
                    .collect(),
                author: field(d, "user")?.into(),
                time_ms: field(d, "time")?
                    .parse::<i64>()?
                    .checked_mul(1000)
                    .context("invalid date")?,
                message: field(d, "desc")?.into(),
                edits: vec![],
                metadata: json!({"client":d["client"],"changeType":d["changeType"]}),
            };
            let mut files = BTreeMap::new();
            let mut original = None;
            for i in 0.. {
                let Some(depot) = d[format!("depotFile{i}")].as_str() else {
                    break;
                };
                let Some(rel) = depot.strip_prefix(prefix) else {
                    continue;
                };
                let path = unescape(rel);
                let version = field(d, &format!("rev{i}"))?;
                let last = versions.entry(path.clone()).or_insert(0u64);
                ensure!(
                    last.checked_add(1) == Some(version.parse()?),
                    "incomplete history at {depot}#{version}"
                );
                *last += 1;
                let spec = format!("{depot}#{version}");
                let action = field(d, &format!("action{i}"))?;
                let kind = field(d, &format!("type{i}"))?;
                if path == MARKER {
                    ensure!(
                        kind == "binary",
                        "reserved history file has an unexpected type"
                    );
                    ensure!(
                        !action.contains("delete"),
                        "history marker was removed at {id}"
                    );
                    let tmp = tempfile::NamedTempFile::new()?;
                    self.print(&spec, tmp.path())?;
                    original = Some(read_marker(tmp.path())?);
                    continue;
                }
                checked_path(&path)?;
                portable_type(kind)?;
                let deleted = match action {
                    "move/delete" | "delete" => true,
                    "add" | "edit" | "move/add" | "integrate" | "branch" | "import" => false,
                    _ => bail!("unavailable or unsupported {action} revision {spec}"),
                };
                let logs = self.records(
                    None,
                    &[
                        "filelog",
                        "-m",
                        "1",
                        &format!("{depot}#{version},#{version}"),
                    ],
                )?;
                let log = logs
                    .iter()
                    .find(|r| r["depotFile"] == depot && r["rev0"] == version)
                    .context("filelog omitted a selected revision")?;
                let mut integration = vec![];
                let mut moved_from = None;
                for j in 0.. {
                    let Some(how) = log[format!("how0,{j}")].as_str() else {
                        break;
                    };
                    let file = field(log, &format!("file0,{j}"))?;
                    if how == "moved from" {
                        moved_from = file.strip_prefix(prefix).map(unescape);
                    }
                    integration.push(json!({"how":how,"file":file,"relative":file.strip_prefix(prefix).map(unescape),"start":log[format!("srev0,{j}")],"end":log[format!("erev0,{j}")]}));
                }
                let blob = if deleted {
                    None
                } else {
                    Some(b.capture(|tmp| {
                        self.print(&spec, tmp)?;
                        if matches!(kind.split('+').next(), Some("text" | "binary")) {
                            let size = field(d, &format!("fileSize{i}"))?.parse::<u64>()?;
                            ensure!(
                                fs::metadata(tmp)?.len() == size,
                                "size mismatch reading {spec}"
                            );
                            let expected = field(d, &format!("digest{i}"))?;
                            let mut md5 = md5::Md5::default();
                            std::io::copy(&mut fs::File::open(tmp)?, &mut md5)?;
                            use md5::Digest;
                            ensure!(
                                format!("{:x}", md5.finalize()).eq_ignore_ascii_case(expected),
                                "digest mismatch reading {spec}"
                            );
                        }
                        Ok(())
                    })?)
                };
                files.insert(path.clone(),json!({"revision":version,"action":action,"type":kind,"integration":integration,"digest":d[format!("digest{i}")]}));
                rev.edits.push(Edit {
                    path,
                    blob,
                    file_type: Some(kind.into()),
                    moved_from,
                });
            }
            ensure!(
                !files.is_empty() || original.is_some(),
                "no visible in-scope files in changelist {id}; check scope casing and permissions"
            );
            rev.metadata["files"] = serde_json::to_value(files)?;
            if let Some(o) = original {
                original_fields(&mut rev, o);
            }
            h.revisions.push(rev);
        }
        Ok(h)
    }

    pub fn import(
        &self,
        b: &Bundle,
        h: &History,
        resume: bool,
        restore_credit: bool,
    ) -> Result<()> {
        let prefix = self.prefix()?;
        let name = prefix.trim_start_matches("//").split('/').next().unwrap();
        for e in h.revisions.iter().flat_map(|r| &r.edits) {
            ensure!(
                !e.path.contains("..."),
                "P4 cannot represent the literal path {}",
                e.path
            );
            let ty = e.file_type.as_deref().unwrap_or("binary");
            portable_type(ty)?;
            ensure!(
                !ty.split('+')
                    .skip(1)
                    .any(|s| s.contains('S') || s.contains('X')),
                "target type {ty} uses archive retention or a trigger; historical content cannot be guaranteed"
            );
        }
        let depots = self.records(None, &["depots"])?;
        let depot = depots
            .iter()
            .find(|d| d["name"] == name)
            .context("destination depot does not exist")?;
        let stream = depot["type"] == "stream";
        ensure!(
            stream || depot["type"] == "local",
            "destination must be a local depot or stream"
        );
        if stream {
            let streams = self.records(None, &["stream", "-o", prefix.trim_end_matches('/')])?;
            ensure!(
                streams.first().is_some_and(|s| s["Access"].is_string()),
                "use an existing stream root"
            );
        }
        let mut existing = self.changes()?;
        ensure!(
            existing.is_empty() || resume,
            "destination has history; use an empty subtree or --resume for the same bundle"
        );
        ensure!(
            existing.len() <= h.revisions.len(),
            "destination has additional history"
        );
        let work = tempfile::Builder::new().prefix("lorep4-").tempdir()?;
        let name = work.path().file_name().unwrap().to_str().unwrap();
        let root = work.path().to_str().context("non-UTF-8 temporary path")?;
        let mut spec = format!(
            "Client: {name}\nRoot: {root}\nLineEnd: unix\nSubmitOptions: submitunchanged\n"
        );
        ensure!(
            self.records(None, &["clients", "-e", name])?.is_empty(),
            "generated client name already exists"
        );
        if stream {
            spec += &format!("Stream: {}\n", prefix.trim_end_matches('/'));
        } else {
            spec += &format!("View:\n\t\"{}\" //{name}/...\n", self.depot);
        }
        let mut create = self.cmd(Some(name));
        create.args(["client", "-i"]);
        run(create, Some(spec.as_bytes()))?;
        let result = (|| -> Result<()> {
            let mut mapping = BTreeMap::new();
            for (cl, rev) in existing.iter().zip(&h.revisions) {
                self.verify_change(prefix, cl, h, rev)?;
                mapping.insert(rev.id.as_str(), cl.clone());
                if restore_credit {
                    self.credit(name, cl, rev)?;
                }
            }
            if existing.len() == h.revisions.len() {
                return Ok(());
            }
            if let Some(last) = existing.last() {
                self.records(Some(name), &["sync", &format!("{}@{last}", self.depot)])?;
            }
            for rev in &h.revisions[existing.len()..] {
                ensure!(
                    self.latest_two()?.first() == existing.last(),
                    "destination changed during import"
                );
                eprintln!("Submitting source revision {}", rev.id);
                let mut edits = rev.edits.clone();
                let record = json_bytes(&marker(h, rev))?;
                let marker_existed = !existing.is_empty();
                // Per-file depot specs plus the dedicated, freshly created client
                // keep submit isolated even when the invoking user has open files.
                edits.sort_by_key(|e| e.blob.is_some());
                let mut moved = std::collections::HashSet::new();
                for (from, to) in paired_moves(rev) {
                    let src = format!("{prefix}{}", escape(from));
                    let dst = format!("{prefix}{}", escape(&to.path));
                    self.check_mapping(name, &src, &work.path().join(from))?;
                    self.check_mapping(name, &dst, &work.path().join(&to.path))?;
                    let ty = to.file_type.as_deref().unwrap_or("binary");
                    portable_type(ty)?;
                    fs::create_dir_all(work.path().join(&to.path).parent().unwrap())?;
                    self.records(Some(name), &["edit", &src])?;
                    self.records(Some(name), &["move", "-t", ty, &src, &dst])?;
                    prune(&work.path().join(from), work.path())?;
                    moved.insert(from);
                    moved.insert(&to.path);
                }
                for e in &edits {
                    let local = work.path().join(&e.path);
                    let spec = format!("{prefix}{}", escape(&e.path));
                    self.check_mapping(name, &spec, &local)?;
                    if e.blob.is_none() {
                        if moved.contains(e.path.as_str()) {
                            continue;
                        }
                        self.records(Some(name), &["delete", &spec])?;
                        prune(&local, work.path())?;
                        continue;
                    }
                    let ty = e.file_type.as_deref().unwrap_or("binary");
                    portable_type(ty)?;
                    let exists = local.is_file();
                    let integrated = if let Some(src) = rev.metadata["branchSources"].get(&e.path) {
                        let p = field(src, "path")?;
                        checked_path(p)?;
                        let cl = mapping
                            .get(field(src, "revision")?)
                            .context("missing source changelist for branch ancestry")?;
                        let from = format!("{prefix}{}@{cl}", escape(p));
                        self.integrate_file(name, &from, &spec, ty, exists)?;
                        true
                    } else {
                        self.integrate(name, prefix, rev, &e.path, &spec, ty)?
                    };
                    if exists && !integrated && !moved.contains(e.path.as_str()) {
                        self.records(Some(name), &["edit", "-t", ty, &spec])?;
                    }
                    fs::create_dir_all(local.parent().unwrap())?;
                    if !integrated || hash(&local)? != *e.blob.as_ref().unwrap() {
                        fs::copy(b.blob(e.blob.as_deref().unwrap()), &local)
                            .with_context(|| format!("writing client file {}", local.display()))?;
                    }
                    if !exists && !integrated {
                        self.records(
                            Some(name),
                            &[
                                "add",
                                "-I",
                                "-f",
                                "-t",
                                ty,
                                local.to_str().context("non-UTF-8 client path")?,
                            ],
                        )?;
                    }
                }
                let m = format!("{prefix}{MARKER}");
                self.check_mapping(name, &m, &work.path().join(MARKER))?;
                if marker_existed {
                    self.records(Some(name), &["edit", "-t", "binary", &m])?;
                }
                fs::write(work.path().join(MARKER), record)?;
                if !marker_existed {
                    self.records(Some(name), &["add", "-I", "-t", "binary", &m])?;
                }
                let msg = if rev.message.trim().is_empty() {
                    "(empty source description)"
                } else {
                    &rev.message
                };
                let opened = self.records(Some(name), &["opened"])?;
                let mut form = format!("Change: new\nClient: {name}\nDescription:\n");
                for line in msg.lines() {
                    form.push('\t');
                    form.push_str(line);
                    form.push('\n');
                }
                form.push_str("Files:\n");
                for file in opened {
                    form.push('\t');
                    form.push_str(field(&file, "depotFile")?);
                    form.push('\n');
                }
                let mut cmd = self.cmd(Some(name));
                cmd.args(["submit", "-i"]);
                run(cmd, Some(form.as_bytes()))?;
                let current = self.latest_two()?;
                ensure!(
                    !current.is_empty()
                        && current.get(1) == existing.last()
                        && current.first() != existing.last(),
                    "ambiguous submit outcome; inspect target history"
                );
                let cl = current[0].as_str();
                self.verify_change(prefix, cl, h, rev)?;
                if restore_credit {
                    self.credit(name, cl, rev)?;
                }
                existing.push(cl.to_string());
                mapping.insert(rev.id.as_str(), cl.to_string());
                println!("{} -> P4 {cl}", rev.id);
            }
            Ok(())
        })();
        // This unique client belongs solely to this invocation. Failure may leave a
        // pending numbered change after a rejected submit; keep it for diagnosis.
        if result.is_err() {
            let client = name.to_owned();
            eprintln!(
                "Diagnostic P4 client {client}, workspace {}",
                work.keep().display()
            );
        } else {
            self.records(Some(name), &["client", "-d", name])?;
        }
        result
    }
    fn integrate(
        &self,
        client: &str,
        prefix: &str,
        rev: &Revision,
        path: &str,
        spec: &str,
        ty: &str,
    ) -> Result<bool> {
        let mut metadata = &rev.metadata;
        while metadata.get("files").is_none() {
            let Some(parent) = metadata.pointer("/original/metadata") else {
                return Ok(false);
            };
            metadata = parent;
        }
        let Some(rows) = metadata["files"][path]["integration"].as_array() else {
            return Ok(false);
        };
        let incoming: Vec<_> = rows
            .iter()
            .filter(|r| {
                r["relative"].is_string()
                    && matches!(r["how"].as_str(), Some("branch from" | "copy from"))
            })
            .collect();
        let [row] = incoming.as_slice() else {
            return Ok(false);
        };
        let source = field(row, "relative")?;
        checked_path(source)?;
        let range = |k: &str| -> Result<u64> {
            let n = field(row, k)?
                .strip_prefix('#')
                .context("invalid P4 integration range")?;
            Ok(if n == "none" { 0 } else { n.parse()? })
        };
        let start = range("start")?
            .checked_add(1)
            .context("invalid integration start")?;
        let end = range("end")?;
        ensure!(start <= end, "invalid integration range");
        let from = format!("{prefix}{}#{start},#{end}", escape(source));
        self.integrate_file(client, &from, spec, ty, row["how"] == "copy from")?;
        Ok(true)
    }
    fn integrate_file(
        &self,
        client: &str,
        from: &str,
        spec: &str,
        ty: &str,
        resolve: bool,
    ) -> Result<()> {
        self.records(Some(client), &["integrate", from, spec])?;
        if resolve {
            self.records(Some(client), &["resolve", "-at", spec])?;
        }
        self.records(Some(client), &["reopen", "-t", ty, spec])?;
        Ok(())
    }
    fn credit(&self, client: &str, cl: &str, rev: &Revision) -> Result<()> {
        ensure!(
            !rev.author.chars().any(char::is_control),
            "invalid author in {}",
            rev.id
        );
        let mut cmd = self.cmd(Some(client));
        cmd.args(["change", "-o", cl]);
        let form = String::from_utf8(run(cmd, None)?)?;
        let seconds = rev.time_ms.div_euclid(1000).to_string();
        let mut edited = String::new();
        for line in form.lines() {
            if line.starts_with("Date:") {
                edited += &format!("Date: {seconds}\n");
            } else if line.starts_with("User:") && !rev.author.is_empty() {
                edited += &format!("User: {}\n", rev.author);
            } else {
                edited += line;
                edited.push('\n');
            }
        }
        let mut cmd = self.cmd(Some(client));
        cmd.args(["change", "-f", "-i"]);
        run(cmd, Some(edited.as_bytes()))
            .context("restoring changelist credit requires admin permission")?;
        let result = self.records(Some(client), &["describe", "-s", cl])?;
        ensure!(
            result.first().is_some_and(
                |v| v["time"] == seconds && (rev.author.is_empty() || v["user"] == rev.author)
            ),
            "changelist {cl} did not retain source author/time"
        );
        Ok(())
    }
    fn check_mapping(&self, client: &str, spec: &str, local: &Path) -> Result<()> {
        let rows = self.records(Some(client), &["where", spec])?;
        ensure!(
            rows.len() == 1
                && rows[0].get("unmap").is_none()
                && rows[0]["depotFile"] == spec
                && Path::new(field(&rows[0], "path")?) == local,
            "non-direct client mapping for {spec}"
        );
        Ok(())
    }
    fn verify_change(&self, prefix: &str, cl: &str, h: &History, rev: &Revision) -> Result<()> {
        let tmp = tempfile::NamedTempFile::new()?;
        self.print(&format!("{prefix}{MARKER}@={cl}"), tmp.path())?;
        let actual: Value = serde_json::from_reader(fs::File::open(tmp.path())?)?;
        ensure!(
            actual == marker(h, rev),
            "destination CL {cl} does not match source revision {}",
            rev.id
        );
        let desc = self.records(None, &["describe", "-s", cl])?;
        let d = desc.first().context("missing submitted change")?;
        let mut changed = BTreeMap::new();
        for i in 0.. {
            let Some(path) = d[format!("depotFile{i}")].as_str() else {
                break;
            };
            let path = path
                .strip_prefix(prefix)
                .context("destination CL also changes paths outside the migration scope")?;
            if path != MARKER {
                changed.insert(
                    unescape(path),
                    (
                        field(d, &format!("action{i}"))?,
                        field(d, &format!("type{i}"))?,
                    ),
                );
            }
        }
        ensure!(
            changed.len() == rev.edits.len(),
            "unexpected files in destination CL {cl}"
        );
        for e in &rev.edits {
            let (action, kind) = changed.get(&e.path).context("missing submitted path")?;
            if let Some(expected) = &e.blob {
                ensure!(
                    *kind == e.file_type.as_deref().unwrap_or("binary"),
                    "type mismatch at CL {cl}: {}",
                    e.path
                );
                self.print(&format!("{prefix}{}@={cl}", escape(&e.path)), tmp.path())?;
                ensure!(
                    hash(tmp.path())? == *expected,
                    "content mismatch at CL {cl}: {}",
                    e.path
                );
            } else {
                ensure!(
                    action.ends_with("delete"),
                    "expected deleted file at CL {cl}: {}",
                    e.path
                );
            }
        }
        Ok(())
    }
}

pub fn prune(path: &Path, root: &Path) -> Result<()> {
    let mut dir = path.parent();
    while let Some(d) = dir {
        if d == root || !d.is_dir() || fs::read_dir(d)?.next().is_some() {
            break;
        }
        fs::remove_dir(d)?;
        dir = d.parent();
    }
    Ok(())
}
fn json_bytes(v: &Value) -> Result<Vec<u8>> {
    Ok(serde_json::to_vec(v)?)
}
