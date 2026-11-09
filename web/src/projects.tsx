// The run lists: the index (each project's latest runs) and a project's page (all of them). A
// project's attributes that every listed run shares are said once above its table; those that
// differ between runs are its columns, a long one (a command line) by the words that differ.

import type { Manifest, Project, RunEntry } from "./api";
import { dur } from "./fmt";

const LATEST = 10;
/** A run that wrote within this long is receiving: parts land every 5 s while rows arrive. */
const LIVE_MS = 30_000;
/** Kept by the server or shown in their own columns, so never an attribute column. */
const OWN = new Set(["run", "project", "opened", "started", "ended", "pid", "durable_seq", "connections", "state", "build.commit", "build.dirty"]);
/** A shared attribute longer than this is left out of the line above the table. */
const SHORT = 48;

/** When the run's data stopped: its END, else its newest file, else its opening. */
export function lastWrite(r: RunEntry): number {
  return Date.parse(String(r.manifest.ended ?? r.last ?? r.manifest.opened));
}

/** When a project last received data. */
export function lastActivity(p: Project): number {
  return Math.max(0, ...p.runs.map(lastWrite).filter(Number.isFinite));
}

function status(r: RunEntry, now: number): "live" | "ended" | "stopped" {
  if (r.manifest.state === "complete") return "ended";
  return now - lastWrite(r) < LIVE_MS ? "live" : "stopped";
}

/** The categories compiled in, without `default` and the `track-` prefix. */
function features(v: unknown): string {
  return String(v ?? "").split(",").filter((f) => f && f !== "default").map((f) => f.replace(/^track-/, "")).join(" ");
}

const show = (k: string, v: Manifest[string] | undefined) => (k === "build.features" ? features(v) : v === null || v === undefined ? "" : String(v));

function bytes(n: number): string {
  const units = ["B", "KB", "MB", "GB", "TB"];
  let i = 0;
  while (n >= 1000 && i < units.length - 1) {
    n /= 1000;
    i++;
  }
  return `${n < 10 && i > 0 ? n.toFixed(1) : Math.round(n)} ${units[i]}`;
}

const runCount = (n: number) => `${n} run${n === 1 ? "" : "s"}`;

const when = (t: number) => new Date(t).toLocaleString(undefined, { month: "short", day: "numeric", hour: "2-digit", minute: "2-digit" });

const label = (k: string) => (k === "build.features" ? "categories" : k);

/** The attributes of `runs`: those every run shares (with their value), and those that differ,
 * each with how a run's value reads in its column. */
function attributes(runs: RunEntry[]): { shared: [string, string][]; differing: { key: string; cell: (r: RunEntry) => string }[] } {
  const keys = [...new Set(runs.flatMap((r) => Object.keys(r.manifest)))].filter((k) => !OWN.has(k) && !k.startsWith("clock.")).sort();
  const shared: [string, string][] = [];
  const differing: { key: string; cell: (r: RunEntry) => string }[] = [];
  for (const k of keys) {
    const values = new Set(runs.map((r) => show(k, r.manifest[k])));
    if (values.size === 1) {
      const v = [...values][0];
      if (v && v.length <= SHORT) shared.push([k, v]);
    } else if ([...values].some((v) => v.length > SHORT)) {
      // a long value against the usual one (the words most runs have): `+word` added, `−word` left out
      const split = (r: RunEntry) => show(k, r.manifest[k]).split(/\s+/).filter(Boolean);
      const counts = new Map<string, number>();
      for (const r of runs) for (const w of new Set(split(r))) counts.set(w, (counts.get(w) ?? 0) + 1);
      const usual = [...counts].filter(([, n]) => n > runs.length / 2).map(([w]) => w);
      differing.push({
        key: k,
        cell: (r) => {
          const has = new Set(split(r));
          return [...[...has].filter((w) => !usual.includes(w)).map((w) => `+${w}`), ...usual.filter((w) => !has.has(w)).map((w) => `−${w}`)].join(" ");
        },
      });
    } else differing.push({ key: k, cell: (r) => show(k, r.manifest[k]) });
  }
  return { shared, differing };
}

/** A project's runs as a table, the attributes they share above it. */
function RunList({ project, runs }: { project: string; runs: RunEntry[] }) {
  const now = Date.now();
  const { shared, differing } = attributes(runs);
  return (
    <>
      {shared.length > 0 && (
        <div className="shared">
          {shared.map(([k, v]) => <span key={k}><span className="dim">{label(k)}</span> {v}</span>)}
        </div>
      )}
      <div className="runs-box">
        <table className="stats runs">
          <thead>
            <tr>
              <th>started</th><th>run</th><th>status</th><th className="num">length</th><th className="num">data</th><th>commit</th>
              {differing.map(({ key }) => <th key={key}>{label(key)}</th>)}
            </tr>
          </thead>
          <tbody>
            {runs.map((r) => {
              const m = r.manifest;
              // the producer's own start: the connection opens with its first row, possibly later
              const started = Date.parse(String(m.started ?? m.opened));
              const st = status(r, now);
              return (
                <tr key={String(m.run)} onClick={() => (window.location.hash = `#/run/${project}/${m.run}`)}>
                  <td title={new Date(started).toLocaleString()}>{when(started)}</td>
                  <td className="dim">{String(m.run).slice(0, 8)}</td>
                  <td className={`status ${st}`}>{st}</td>
                  <td className="num">{dur((lastWrite(r) - started) * 1e3)}</td>
                  <td className="num">{bytes(r.bytes ?? 0)}</td>
                  <td>{String(m["build.commit"] ?? "")}{m["build.dirty"] ? "*" : ""}</td>
                  {differing.map(({ key, cell }) => {
                    const v = cell(r);
                    return <td key={key} className="attr" title={v}>{v}</td>;
                  })}
                </tr>
              );
            })}
          </tbody>
        </table>
      </div>
    </>
  );
}

/** The index: every project, most recently active first, with its latest runs. */
export function Projects({ projects }: { projects: Project[] }) {
  if (!projects.length) return <div className="empty">no runs yet</div>;
  const ordered = [...projects].sort((a, b) => lastActivity(b) - lastActivity(a));
  return (
    <>
      {ordered.map((p) => (
        <section className="project" key={p.name}>
          <div className="rule">
            <a className="name" href={`#/project/${p.name}`}>{p.name}</a>
            <span className="dim">{runCount(p.runs.length)}</span>
          </div>
          <RunList project={p.name} runs={p.runs.slice(0, LATEST)} />
          {p.runs.length > LATEST && <a className="more" href={`#/project/${p.name}`}>all {p.runs.length} runs →</a>}
        </section>
      ))}
    </>
  );
}

/** One project: all its runs. */
export function ProjectPage({ project }: { project: Project }) {
  return (
    <section className="project">
      <div className="rule">
        <span className="name">{project.name}</span>
        <span className="dim">{runCount(project.runs.length)}</span>
      </div>
      <RunList project={project.name} runs={project.runs} />
    </section>
  );
}
