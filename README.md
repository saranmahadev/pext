# pext

An agent-friendly personal finance engine. Your money lives in one local SQLite file; every capability is a command that
returns typed, queryable data, and the same engine powers the CLI, an MCP server for AI agents, a local HTTP API, a web
dashboard and a terminal UI.

```
pext init
pext acct add --name "HDFC Bank" --kind checking --opening 250000
pext txn add --account "HDFC Bank" --amount -1200 --category food.groceries --payee "Big Bazaar"
pext report summary                      # table on a terminal, JSON when piped
pext report spending -o table --chart bar
pext budget status | pext alert check --fail-on critical
```

## What it does

| Area | Commands |
|---|---|
| Ledger | `acct`, `txn` (splits, transfers, tags), `cat`, `payee`, `import` (CSV/OFX, de-dupe, rollback), `rule`, `fx` |
| Plan and track | `budget` (rollover), `recur`, `goal`, `alert`, `sub`, `cashflow`, `runway`, `report`, `query`, `view` |
| Wealth | `loan` (amortisation), `debt plan`, `iou`, `invest` / `trade` / `price` (FIFO, XIRR), `asset`, `networth`, `snapshot` |
| Experience and reach | `tui`, `serve` (web + API), `mcp`, `backup`, `sync`, `plugin`, `token`, `completions`, `--chart`, `--redact`, `--private` |
| Integrity | `audit` (hash-chained log), `undo`, `doctor`, `--dry-run`, `--idempotency-key` |

Run `pext commands` for every command and parameter (machine-readable), `pext schema <command>` for its JSON Schema.

## Correctness first

* Money is `i64` minor units end to end. Core crates forbid float arithmetic (`clippy::float_arithmetic`); the only float
  code is XIRR, isolated in one file and reported in basis points. Rounding is half-even; allocation always sums exactly.
* Every write runs in one SQLite transaction, is checked by ledger invariants, and appends to a tamper-evident hash chain.
  `pext undo` reverses any operation; `pext doctor` verifies the database and the chain.
* Property tests cover money, loans and investments; each command group has end-to-end tests. `cargo test --workspace`.

## For agents

`pext mcp --profile read|append|write` speaks MCP over stdio. Tools are the same commands (`txn.add` is `txn_add`),
filtered by profile; admin and local-only commands (backup, sync, plugins, tokens, wipe) are never exposed. Results carry
`untrusted_text_fields` so payees and memos are treated as data, not instructions. Destructive confirmation is off unless
you pass `--allow-confirm`. Writes accept `dry_run` and `idempotency_key`.

Exit codes: 0 ok, 2 usage, 3 validation, 4 not found, 5 conflict, 6 auth, 7 I/O, 8 integrity, 10 alerts present
(`alert check --fail-on`), 70 internal. Errors are JSON on stdout in machine formats.

## Access control and privacy

* `pext token create --name agent --role read|append|write|full [--account A] [--redact] [--expires DATE]`. Tokens are
  stored as hashes; the secret is shown once. `config set require_token true` closes the database to anonymous use.
* `--redact` masks payees, memos and notes for that identity; `--private` hides every amount in the output.

## Backup, sync, plugins

* `pext backup create|list|verify|restore` writes Argon2id + XChaCha20-Poly1305 encrypted snapshots. The passphrase comes
  from `PEXT_PASSPHRASE` or `--passphrase-file`, never from the command line. Restore keeps a copy of what it replaces.
* `pext sync init --dir FOLDER`, `push`, `pull`, `status`, `resolve` sync through a shared folder using the audit chain to
  tell *ahead / behind / diverged*. Nothing is merged silently; the losing side of a conflict is always kept.
* `pext plugin install` registers an executable (pinned by SHA-256, re-checked before each run). Plugins speak JSON on
  stdin/stdout; `pext price fetch --plugin NAME` records prices through the normal audited path.

## Serving

`pext serve` binds to `127.0.0.1:7878` and serves the dashboard at `/` and the API under `/v1` (bearer token required;
`POST /v1/call/<command>`). It checks the Host header, rate-limits per token and per failing client, caps bodies and
responses, and refuses non-loopback addresses unless `--allow-remote` (put TLS in front). `pext tui` is the read-only
terminal dashboard.

## Known limits

* The database file itself is not encrypted at rest (backups and sync snapshots are). Use OS disk encryption.
* Plugins run as ordinary processes with an empty environment, a timeout and an output cap; declared permissions are
  recorded for review but not enforced by an OS sandbox. Only install plugins you trust.
* Sync is snapshot-based through a folder you provide; there is no server and no automatic merge.
* `--chart` supports bar charts only. The TUI is read-only (no inline edit or command palette). No PDF reports yet.
* Phase 5 (forecasting intelligence, tax packs, compliance exports) is not built.

## Build

```
cargo build --release -p pext-cli      # binary: target/release/pext
cargo test --workspace
pext completions bash > /etc/bash_completion.d/pext
```
