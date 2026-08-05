# Migrations

`sqlx::migrate!` applies every `<version>_<description>.sql` here in version order at `warden`
startup and stores each file's checksum in `_sqlx_migrations`. That checksum covers the **whole
file, comments included**, so a committed migration is frozen: re-editing one makes
`Migrator::run` abort with `VersionMismatch(<version>)` on every database that already applied the
previous text, and only manual surgery gets that database running again. A schema correction is
therefore a *new* migration (`code-standards.md`, "SQLite & sqlx").

A purely editorial correction -- a header comment that turned out to be wrong -- does not justify
burning a migration version on a statement-less file that every deployment then has to apply. It
goes below instead. This file is not a migration and sqlx never reads it: its resolver silently
ignores every name that is not `<version>_<description>.sql`.

## Errata

### `0017_events_agent_progress_index.sql` (issue #108)

Its header justifies ordering the index `(run_id, created_at, event_type)` rather than
`(run_id, event_type, created_at)` by saying that keeping `created_at` second "preserves the
ordering the previous index already served (no sort added)". **The parenthesis is wrong**, and the
trade it hides is the reason the column order is still the right one.

SQLite appends `rowid` to every index key, so `(run_id, created_at)` natively satisfied the replay's
`ORDER BY created_at ASC, rowid ASC`. Interposing `event_type` no longer does. Measured on sqlite3
3.51.0, 20 000 rows, after `ANALYZE`, both replay variants (progress included and excluded) plan as:

```
SEARCH events USING INDEX idx_events_run_id_created_at_type (run_id=?)
USE TEMP B-TREE FOR LAST TERM OF ORDER BY
```

That is a *partial* sort, of the last `ORDER BY` term only, within each group of equal `created_at`.
Those groups hold one row in practice -- a tie is the rare case the `rowid` tie-break exists for --
so the sort is paid on nothing, while the table lookups the in-index `event_type` test skips are
saved on the majority of rows: `agent_progress` outnumbers every other event kind by an order of
magnitude. No index buys both properties; SQLite refuses `rowid` as an index column outright
(`no such column: rowid`). The index stays as shipped. See the issue #108 entry in `CHANGELOG.md`.
