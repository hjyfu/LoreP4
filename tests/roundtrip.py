"""End-to-end regression. Starts its own loopback p4d and creates disposable fixtures."""
import argparse
import json
import os
from pathlib import Path
import socket
import subprocess
import tempfile
import time


def exercise(root, port, cli, env):
    def run(args, *, at=root, data=None, ok=True):
        result = subprocess.run(args, cwd=at, env={**env, "PWD": str(at)}, input=data, capture_output=True)
        if (result.returncode == 0) != ok:
            raise RuntimeError(f"{args}: {result.stdout.decode(errors='replace')}\n{result.stderr.decode(errors='replace')}")
        return result.stdout

    def p4(*args, at=root, data=None):
        return run(["p4", "-p", port, "-u", "original-writer", "-c", "seed", *args], at=at, data=data)

    def lore(at, *args):
        output = run(["lore", "--offline", "--json", *args], at=at)
        return [json.loads(line) for line in output.splitlines()]

    def bridge(*args, ok=True):
        return run([cli, *map(str, args)], ok=ok)

    ws = root / "seed"
    ws.mkdir()
    p4("client", "-i", data=f"Client: seed\nRoot: {ws}\nLineEnd: unix\nView:\n\t//depot/source/... //seed/...\n".encode())
    p4_args = ["--port", port, "--user", "migrator"]
    binary = bytes(range(256)) * 8192
    (ws / "vehicle.bin").write_bytes(binary)
    special = "赛车 #50%@.txt"
    (ws / special).write_bytes("记录一\n".encode())
    (ws / "empty").write_bytes(b"")
    for file, kind in [(special, "text"), ("vehicle.bin", "binary"), ("empty", "binary")]:
        p4("-x", "-", "add", "-f", "-t", kind, at=ws, data=(file+"\n").encode())
    form=p4("change","-o",at=ws).replace(b"<enter description here>", "第一版\n\t\n\tBinary, Unicode, empty file.".encode())
    p4("submit","-i",data=form,at=ws)
    p4("edit", "vehicle.bin", at=ws)
    p4("move", "vehicle.bin", "renamed.bin", at=ws)
    (ws / "renamed.bin").write_bytes(binary + b"\x00\xff")
    p4("delete", "empty", at=ws)
    p4("submit", "-d", "Move, edit and delete", at=ws)
    p4("edit", "-t", "binary+l", "renamed.bin", at=ws)
    p4("submit", "-d", "File type change only", at=ws)
    p4("integrate", "renamed.bin", "copy.bin", at=ws)
    p4("submit", "-d", "Branch a file", at=ws)
    bundle, repo, back = root / "bundle", root / "imported", root / "back"
    bridge("p4-to-lore", *p4_args, "--depot", "//depot/source/...", "--bundle", bundle, "--repo", repo)
    bridge("verify", "--bundle", bundle)
    for i, path, expected in [(1, "vehicle.bin", binary), (1, "empty", b""),
                              (2, "renamed.bin", binary + b"\x00\xff"), (4, "copy.bin", binary+b"\x00\xff")]:
        out = root / "readback"
        lore(repo, "--force", "file", "write", "--revision", f"@{i}", "--path", path, "--output", str(out))
        assert out.read_bytes() == expected
    lore_move = lore(repo, "revision", "info", "--delta", "@2")
    assert any(v["tagName"] == "revisionInfoDelta" and v["data"]["fromPath"] == "vehicle.bin" for v in lore_move)
    bridge("lore-to-p4", *p4_args, "--depot", "//depot/target/...", "--bundle", back, "--repo", repo, "--restore-credit")
    ids = [json.loads(v)["change"] for v in p4("-ztag", "-Mj", "changes", "-r", "//depot/target/...").splitlines()]
    assert len(ids) == 4
    for i, change in enumerate(ids, 1):
        source, target = [json.loads(p4("-ztag", "-Mj", "describe", "-s", c)) for c in [str(i), change]]
        for key in ["user", "time"]:
            assert source[key] == target[key], (key, source, target)
        assert source["desc"].rstrip("\n") == target["desc"].rstrip("\n")
        record=json.loads(p4("print","-q",f"//depot/target/.lorep4-history.json@={change}"))
        assert record["revision"]["message"] == source["desc"]

        def files(row, prefix):
            result = {}
            for j in range(len(row)):
                name = row.get(f"depotFile{j}")
                if not name:
                    break
                name = name.removeprefix(prefix)
                if name != ".lorep4-history.json":
                    result[name] = (row[f"type{j}"], row.get(f"digest{j}"), row[f"action{j}"])
            return result
        src_files, dst_files = files(source, "//depot/source/"), files(target, "//depot/target/")
        assert src_files == dst_files, (src_files, dst_files)
    bridge("import-p4", *p4_args, "--depot", "//depot/target/...", "--bundle", back, ok=False)
    bridge("import-p4", *p4_args, "--depot", "//depot/target/...", "--bundle", back, "--resume")
    assert len(p4("-ztag", "-Mj", "changes", "//depot/target/...").splitlines()) == 4
    manifest = back / "history.json"
    full = manifest.read_bytes()
    prefix = json.loads(full)
    prefix["revisions"] = prefix["revisions"][:2]
    manifest.write_text(json.dumps(prefix), encoding="utf8")
    bridge("import-p4", *p4_args, "--depot", "//depot/resume/...", "--bundle", back)
    manifest.write_bytes(full)
    bridge("import-p4", *p4_args, "--depot", "//depot/resume/...", "--bundle", back, "--resume")
    assert len(p4("-ztag", "-Mj", "changes", "//depot/resume/...").splitlines()) == 4

    # Independently authored graph: diverge, merge and materialize every committed snapshot.
    graph = root / "graph"
    graph.mkdir()
    lore(graph, "repository", "create", "graph")

    def commit(name):
        (graph / name).write_bytes(name.encode())
        lore(graph, "stage", "--scan", ".")
        lore(graph, "commit", name)
    commit("base")
    lore(graph, "branch", "create", "art")
    lore(graph, "branch", "switch", "art")
    commit("artwork")
    lore(graph, "branch", "switch", "main")
    commit("engine")
    lore(graph, "branch", "merge", "art", "--message", "Merge art")
    graph_bundle = root / "graph-bundle"
    bridge("lore-to-p4", "--repo", graph, "--bundle", graph_bundle, *p4_args, "--depot", "//depot/graph/...")
    history = json.loads((graph_bundle / "history.json").read_bytes())
    assert len(history["revisions"]) == 4
    assert sum(len(r["parents"]) == 2 for r in history["revisions"]) == 1
    for branch, info in history["scope"]["branches"].items():
        for name in (["base", "artwork", "engine"] if info["name"] == "main" else ["base", "artwork"]):
            assert p4("print", "-q", f"//depot/graph/branches/{branch}/{name}") == name.encode()
        inherited = "artwork" if info["name"]=="main" else "base"
        log = json.loads(p4("-ztag", "-Mj", "filelog", "-m", "1", f"//depot/graph/branches/{branch}/{inherited}").splitlines()[0])
        assert "branch from" in log.values()
    blob = next((graph_bundle / "blobs").iterdir())
    blob.write_bytes(b"corrupt")
    bridge("verify", "--bundle", graph_bundle, ok=False)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--cli", required=True, type=Path)
    parser.add_argument("--p4d", default="p4d")
    args = parser.parse_args()
    cli = str(args.cli.resolve(strict=True))
    with tempfile.TemporaryDirectory(prefix="lorep4-test-") as temp:
        root = Path(temp)
        db = root / "server"
        db.mkdir()
        with socket.socket() as sock:
            sock.bind(("127.0.0.1", 0))
            port = f"127.0.0.1:{sock.getsockname()[1]}"
        env = {k: v for k, v in os.environ.items() if not k.startswith(("P4", "LORE_"))}
        env.update(P4PORT=port, P4USER="migrator", P4CLIENT="seed", P4CHARSET="utf8",
                   P4CONFIG=".lorep4-test-config", P4TICKETS=str(root / "tickets"))
        subprocess.run([args.p4d, "-r", str(db), "-xi"], cwd=db, env=env, check=True, capture_output=True)
        server = subprocess.Popen([args.p4d, "-r", str(db), "-p", port, "-L", "server.log"],
                                  cwd=db, env=env, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
                                  creationflags=getattr(subprocess, "CREATE_NO_WINDOW", 0))
        try:
            for _ in range(100):
                if server.poll() is not None:
                    raise RuntimeError("p4d stopped during startup")
                if subprocess.run(["p4","-p",port,"info"], env=env, capture_output=True).returncode == 0:
                    break
                time.sleep(0.05)
            exercise(root, port, cli, env)
        finally:
            server.terminate()
            try:
                server.wait(timeout=10)
            except subprocess.TimeoutExpired:
                server.kill()
                server.wait()
    print("Passed P4/Lore round trip, source credit, moves, types, binary data, branch graph, resume and corruption checks.")


if __name__ == "__main__":
    main()
