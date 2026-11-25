# LoreP4

LoreP4 migrates submitted version history between Epic Games Lore and Perforce Helix Core. Each in-scope P4 changelist becomes a Lore revision. A Lore export walks all visible branch tips and both parents of each merge. Payloads are deduplicated on disk and read back from the destination after every commit.

## Running a migration

Install the `p4` and `lore` commands on PATH. The reference versions are P4 2024.2 and Lore 0.14.0. Authenticate with their normal CLIs. Use a UTF-8 P4 connection on Unicode servers.

```sh
cargo install --locked --git https://github.com/hjyfu/LoreP4 --tag v0.1.0

lorep4 p4-to-lore --port p4.example.com:1666 --depot "//depot/game/..." --repo ./game-lore --bundle ./from-p4
lorep4 lore-to-p4 --repo ./game-lore --port p4.example.com:1666 --depot "//depot/imported/..." --bundle ./from-lore
lorep4 verify --bundle ./from-lore
```

Supply a new local directory for Lore. Add `--lore-url lore://host:41337/name` to create a remote repository, push the verified history and check the remote tip. Lore sources default to the local instance. Add `--remote` to read remote branch history.

Supply an empty P4 target in an existing classic depot or a directly mapped stream root and reserve it for the migration. LoreP4 creates its own client. Add `--restore-credit` with P4 admin rights to restore changelist authors and dates. Otherwise the native P4 fields show the migration account and submission time. Original values are always recorded.

## Fidelity and recovery

- Every target commit tracks `.lorep4-history.json` for source IDs, parents, authors, timestamps, messages, file types and integration metadata. This reserved filename also makes content-neutral revisions representable. Source descriptions survive native form newline normalization. Lore uses the replay time for its native timestamp.
- A single Lore branch maps directly into the depot subtree. Multiple branches map to `branches/<branch-id>/`. The manifest maps each ID to its name and tip. P4 gains native integrations for matching fork/merge files. P4 scopes replay onto one Lore branch. Exact reconstruction of arbitrary native stream specs and branch graphs is outside this release.
- Matched moves are replayed natively. Simple in-scope P4 branch/copy relations can be recreated on return. Other integration records retain their source paths and revision ranges in the manifest. Scope and access permissions determine what the source CLI can report.
- Regular files, empty files and binary data are supported. Native Lore files default to P4 binary. Empty Lore directory information is carried as metadata. Link nodes, symlinks, unavailable historical payloads and incompatible path/type combinations cause an error. P4 users, access controls, labels, jobs, shelves, pending edits and workspace administration require separate handling.
- Use `export-p4` or `export-lore` to write a portable directory, then `verify` and `import-p4` or `import-lore`. Each import checks every referenced BLAKE3 blob first. After an interrupted P4 import, use the same bundle with `import-p4 --resume`. It validates the existing prefix before continuing. A failed Lore import can be replayed from the retained bundle into a new destination.
- Bundle metadata can contain source identities and integration paths outside the selected subtree. Restrict access accordingly. Budget storage for unique historical bytes plus a work directory and benchmark a representative subtree before sizing a large run.

The [Chinese guide](../README.md) includes the validation commands. The Python end-to-end test starts its own loopback P4 server and uses disposable repositories.
