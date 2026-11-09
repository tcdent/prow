# prow

Profiling rows: structured, timestamped measurements from a program and its devices, stored as
one columnar table per site per run, and queried with SQL. The hooks stay in the code; which ones
exist is decided at compile time.

```rust
prow::Run::new("myapp").attrs(prow::build_attrs!()).attr("workload", "nightly").server("10.0.0.2:7380").start();

prow::row!("track-step", "step", { "kind" => "prefill", "width" => 512u32, "pos" => pos, "ms" => ms });
prow::zone!("track-zones", "attention");                 // a span to the end of the scope
prow::zone_at!("track-zones", "experts", "card0", card.at(start_ts), card.at(end_ts));
prow::plot!("track-store", "store.bytes", used);
prow::message!("track-store", "spilled to NVMe");
```

| Path | What |
|---|---|
| `src/` | the client (`prow`): hooks, the sender, `protocol`, `clock`, `stamp`. std and `libc` only |
| `server/` | `prow-server`: receives runs, writes Parquet, answers SQL (DuckDB embedded) |
| `bin/prow-sql` | SQL over a server's runs (`POST /api/sql`) |
| `web/` | the web view: flame chart, flame graph and charts over a selected range, queried from the server |
| `demo/` | every hook in the shape of a serving loop, for trying a server end to end |

## Categories: what is compiled in

A hook's first argument is a category: a Cargo feature of the crate that calls the hook.

```toml
[features]
track-step = []
track-zones = []
```

- **Off, the hook is gone:** it expands to nothing, and its arguments are never evaluated.
- **A misspelled category is a compiler warning,** because Cargo checks feature names:
  ``unexpected `cfg` condition value: `track-stpe` ``.
- **A build with no categories records nothing and starts nothing.** `Run::start` only stores the
  run; the sender thread starts with the first row a hook records. That build is the baseline
  for what tracking costs.
- **A category that spans crates is forwarded** through each crate's features, like any feature.

There is no runtime switch.

## The hooks

Tracy's hooks, adapted to rows:

| Hook | Records | Table |
|---|---|---|
| `row!(cat, site, { field => value, .. })` | a row of named fields; `at = ts` gives its time | `<site>` |
| `zone!(cat, name)` | a span to the end of the scope, on the calling thread's lane | `zones` |
| `zone_at!(cat, name, lane, start, end)` | a span timed elsewhere, on a named lane: a card's work | `zones` |
| `plot!(cat, name, value)` | one number now | `plots` |
| `message!(cat, text)` | a line of text now | `messages` |

- **Every row's first column is `ts`,** UTC in nanoseconds, taken when the hook runs, on the thread
  that runs it.
- **A value** is an integer, a float, a `bool`, a string (a label) or a `Timestamp`.
- **A site's fields and types are fixed by its first row.**
- **Names** use `a-z 0-9 _ . -`, checked at compile time; `ts` is reserved.
- **The same site name at two call sites** shares one table if the fields match. If they don't,
  the second call site records nothing, with a warning.

The built-in tables have these fields:
- `zones`: `category`, `name`, `lane`, `dur_ns`
- `plots`: `category`, `name`, `value`
- `messages`: `category`, `text`

## A run

`Run::new(project)` takes:
- **attributes:** `.attr(name, value)`, and `.attrs(prow::build_attrs!())` for build identity;
- **the server:** `.server("host:port")`, or `PROW_SERVER`;
- **a token:** `.token(..)`, or `PROW_TOKEN`;
- **the sender's core:** `.core(n)`.

Otherwise the sender inherits the core of the thread that started it.

Every run also records, automatically:
- `host`, `pid` and `started`;
- `clock.synced`, `clock.maxerror_us` and `clock.esterror_us` (from `adjtimex`);
- `prow.version`.

**Build identity.** A crate's `build.rs` calls `prow::stamp::emit()`, with `prow` in its
`[build-dependencies]`. That bakes in:
- `build.commit`, the short commit;
- `build.dirty`, whether tracked files differ from that commit;
- `build.features`, the enabled features, which include the compiled categories.

**Exit.** A normal exit ends the run within 2 s; `prow::finish()` ends it early.

**Delivery.** Delivery is in memory and best-effort:
- a thread buffers at most 8 MiB between flushes (every 100 ms);
- the sender keeps at most 64 MiB of unacknowledged frames;
- anything past those limits is dropped and counted (`prow: dropped N rows`).

**Clocks.** `prow::clock`:
- `Timestamp::now()` reads the host's real-time clock;
- `clock::from_raw` converts a `CLOCK_MONOTONIC_RAW` reading to UTC;
- `clock::Card` converts an AMD GPU's 100 MHz command-processor ticks to UTC, through KFD's clock
  counters. This is untested on hardware.

## The protocol

One TCP connection is one run. The frames are specified in `src/protocol.rs`, which both sides
use:
- **`HELLO`** carries the project, the run id and the attributes. The server replies `OK`, or
  `ERR` with a reason.
- **`SCHEMA`** and **`STRING`** are sent once per connection, before the rows that use them.
- **`ROWS`** carries rows packed by their schema; labels are ids. Each frame has a sequence
  number.
- **`ACK`** comes back from the server once a flush is on disk, carrying the sequence it reaches.
  The producer keeps every frame until an `ACK` covers it.
- **`END`** finishes the run; the server replies `OK` once its rows are on disk.

A producer that reconnects sends the same run id, then every frame not yet acknowledged.

## The server

`prow-server` is configured from its environment. Ports, addresses and volumes belong to the
deployment.

| Variable | Default | |
|---|---|---|
| `PROW_DATA` | `/data` | the data tree |
| `PROW_LISTEN` | `0.0.0.0:7380` | |
| `PROW_TOKEN` | unset | when set, a `HELLO` must carry it |
| `PROW_HTTP` | `0.0.0.0:7381` | the web view and the read side: `/api/index`, `/api/sql` |
| `PROW_WEB` | `/web` | the built web app (`web/dist`) |
| `PROW_QUERY_MEMORY` | `4GB` | the embedded DuckDB's memory limit |
| `PROW_QUERY_THREADS` | `8` | the embedded DuckDB's threads |
| `PROW_EXTENSIONS` | `/usr/local/lib/prow/extensions` | DuckDB extensions loaded at start: ICU, for time zones |

```
<PROW_DATA>/project=<project>/run=<run id>/manifest.json
<PROW_DATA>/project=<project>/run=<run id>/<site>/part-00001.parquet   while the run is open
<PROW_DATA>/project=<project>/run=<run id>/<site>/data.parquet         once it closes
```

**How it writes:**
- **Parts.** The server buffers rows and writes a part for each site every 5 s or 100,000 rows,
  in Snappy, which costs almost nothing to write. A part is written to a hidden temporary file,
  synced and renamed into place, so readers see a live run within seconds and never see a
  partial file.
- **The durable sequence.** After a flush's parts are on disk, the manifest records the flush's
  sequence (`durable_seq`) and the server sends the `ACK`.
- **Closing.** When a run's connection ends — `END`, a disconnect, or a producer whose machine
  went down, which TCP keepalive notices in about a minute — the server compacts each site into
  `data.parquet` in zstd at level 19, about 2× smaller than the parts. Reading costs the same at
  any level; only the one write is slow. At start, the server closes every run left open. A
  producer that reconnects to a closed run continues it, and the next close takes its new parts
  in.
- **Dictionaries only for text.** Labels repeat and compress to almost nothing; timestamps and
  durations rarely repeat, and are written plain.
- **The manifest** records the run's attributes and its `state`: `open`, `complete`, or
  `disconnected` for a connection that ended without `END` (its `ended` is its last write).

**Durability: a server crash neither loses nor repeats rows.**
- A crash before a flush completes leaves parts newer than `durable_seq`. Reopening the run
  deletes them.
- The producer then resends every unacknowledged frame, and the server skips those at or below
  `durable_seq`.
- What can be lost is what the producer holds when it dies itself: rows not yet acknowledged,
  about 5 s of them.

**A container.** `bin/build-image` builds the server (DuckDB embedded, so it needs glibc and
libstdc++) and the web app into a `debian:bookworm-slim` image that runs as uid 1000.
- **The tag** is `<version>-dev.<UTC time>`, so a redeploy never bumps the version. `PROW_TAG`
  names it explicitly, e.g. the plain version for a release.
- **The image** is `prow-server`, or `$PROW_IMAGE` for your registry's path.

```bash
podman run -d --network host -v /srv/prow:/data \
  -e PROW_LISTEN=10.0.0.2:7380 -e PROW_HTTP=10.0.0.2:7381 prow-server:<tag>
```

Mount a directory, not a file, at `/data`; uid 1000 needs to be able to write it. The server
exits at once on SIGTERM or SIGINT: rows not yet in a part are unacknowledged, so their producers
resend them.

## Querying

The server answers SQL: DuckDB is embedded in it, and both the web view and `bin/prow-sql` query
through `POST /api/sql`.

```bash
export PROW_URL=http://10.0.0.2:7381
bin/prow-sql                                              # projects
bin/prow-sql myapp "SELECT kind, width, median(ms) FROM step GROUP BY ALL"
bin/prow-sql myapp <run id> "SELECT name, count(*) FROM zones GROUP BY name"   # one run: faster
bin/prow-sql myapp < query.sql
```

```
POST /api/sql?[project=<p>][&run=<id>][&format=text][&max_rows=<n>]   the SQL is the body
→ {"columns", "types", "rows", "truncated", "ms"}, or an aligned text table with format=text
```

The web view's **query** tab runs SQL against a project or one run. Its history is the server's
query log, and a past query runs again in one click.

**What a query sees:**
- **`runs`:** one row per run, every attribute a column.
- **One view per site:** the site's fields, plus `ts` and `run`. Join to `runs` with `USING (run)`.
  With `run=<id>`, every view covers that run's files only; without it, every run of the project.
- **`queries`:** the server's query log, also without a project: `ts`, `source` (who asked:
  the `X-Prow-Source` header — `page`, `console`, `prow-sql`, else `api`), `project`, `run`, `sql`,
  `ms`, `rows`, `truncated`, `error`. Kept in `PROW_DATA/queries.jsonl`.
- **At most 200,000 rows** come back; `truncated` says when there were more.
- **At most 60 s:** a query running longer is stopped with an error.
- **UTC:** `ts` is a `TIMESTAMPTZ` and the session's time zone is UTC. With the ICU extension,
  interval arithmetic, `date_trunc` and `AT TIME ZONE` work as in the DuckDB CLI.

**What a query can't do:**
- **Run anything but `SELECT`.** Each request is parsed by DuckDB first (`json_serialize_sql`),
  which refuses `COPY`, `ATTACH`, `INSTALL`, `SET` and DDL.
- **Read outside `PROW_DATA`.** The engine allows only that directory, loads no extensions, and
  locks its configuration.

**A crash stays in its query.** DuckDB runs in a worker process. If a query crashes it
(DuckDB 1.5.6 segfaults on some valid SQL), the queries in flight fail with the crash, the
server starts a new worker, and ingest and the web view carry on.

**Changing a schema.** A field added to a site reads as NULL in runs from before it existed.
Hooks never record a null, so a NULL always means "not in that run's schema". If a field changes
meaning, give it a new name.

## The web view

The browser asks the server for what it draws, never for files:
- **A run's summary:** its sites, fields, row counts and extents, and its zone lanes.
- **The overview:** each lane's and site's events counted into buckets across the run.
- **The flame charts:** time tiles on a fixed grid of power-of-two widths. Each tile is fetched
  once and cached, and the zoom level puts two to four tiles across the view. A large source's
  tile keeps only what is at least a pixel wide at its level, so zooming in brings finer zones.
  Sites of up to 20,000 rows come whole. While tiles load, cached coarser ones stand in.
- **The flame graph:** `GET /api/flame?project=&run=&from=&to=` (µs since the epoch). The server
  streams the range's zones from DuckDB, in order, and walks each lane's stack in one pass: every
  zone counts, at any zoom (the whole 6.5M-zone run in 1.5 s).
- **The tables and other charts:** one aggregate query each over the view: zone figures, layer
  totals by group, a value's buckets, histogram bins, plots, messages.

**A run's page:**
- **The overview:** each lane's busy share over the whole run, plus messages. The selected range
  is shown as the bright window.
- **The flame chart:** one band per lane, one row per nesting depth.
  - Drag to select a range, or wheel to zoom around the pointer.
  - Click a zone to fit it; double-click for the whole run.
  - Card lanes from `zone_at!` sit under the host threads.
- **Panels over the selected range:**
  - **flame graph:** zones merged by their stack. Click a frame to focus it.
  - **zones:** count, time in the range, share of the range, mean, p50, p95 and max for each zone.
    Click a row to highlight that zone in the flame chart.
  - **plots:** one line chart per plot name.
  - **each site:** its rows as spans in a flame chart (each row ending at its time, its duration
    fields laid out beneath it as layers you can switch), or one value over time, or a histogram,
    with figures per label.

The range and each site's settings are kept in the URL, so a view can be shared.

```bash
cd web && bun install && bun run build      # web/dist
bun run dev        # PROW_DEV_HOST:5181 (default 127.0.0.1), proxying /api to PROW_HTTP_URL
```

## Trying it

```bash
cargo build --release
PROW_DATA=/tmp/prow PROW_LISTEN=127.0.0.1:7380 PROW_HTTP=127.0.0.1:7381 PROW_WEB=web/dist target/release/prow-server &
PROW_SERVER=127.0.0.1:7380 target/release/prow-demo 1000
PROW_URL=http://127.0.0.1:7381 bin/prow-sql demo "SELECT lane, name, count(*), avg(dur_ns) FROM zones GROUP BY ALL"
# the web view: http://127.0.0.1:7381
cargo build --release -p prow-demo --no-default-features   # the baseline: no categories
```
