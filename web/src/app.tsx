// Hash routes: #/ every project's runs, #/run/<project>/<run>?f=&t=&p= one run, its view range (µs)
// and panel. No router dependency; the URL is the state, so a view is shareable.

import { useCallback, useEffect, useState } from "react";
import { loadIndex, type Project } from "./api";
import { dur } from "./fmt";
import { RunPage } from "./run";

function useHash(): string {
  const [h, setH] = useState(window.location.hash);
  useEffect(() => {
    const f = () => setH(window.location.hash);
    window.addEventListener("hashchange", f);
    return () => window.removeEventListener("hashchange", f);
  }, []);
  return h;
}

export function App() {
  const hash = useHash();
  const [path, qs = ""] = hash.replace(/^#\/?/, "").split("?");
  const parts = path.split("/").map(decodeURIComponent).filter(Boolean);
  const params = new URLSearchParams(qs);
  const [projects, setProjects] = useState<Project[] | null>(null);
  const [error, setError] = useState<string | null>(null);
  const refresh = useCallback(() => loadIndex().then(setProjects, (e) => setError(String(e))), []);
  useEffect(() => {
    refresh();
  }, [refresh, parts.length === 0]); // eslint-disable-line react-hooks/exhaustive-deps

  const setParams = useCallback(
    (p: Record<string, string | null>) => {
      const next = new URLSearchParams(qs);
      for (const [k, v] of Object.entries(p)) v === null ? next.delete(k) : next.set(k, v);
      const s = next.toString();
      window.history.replaceState(null, "", `#/${path}${s ? `?${s}` : ""}`);
      window.dispatchEvent(new HashChangeEvent("hashchange"));
    },
    [path, qs],
  );

  let page = <div className="empty">reading the index…</div>;
  let crumb: string | null = null;
  if (error) page = <div className="empty">{error}</div>;
  else if (projects && parts[0] === "run") {
    const entry = projects.find((p) => p.name === parts[1])?.runs.find((r) => r.manifest.run === parts[2]);
    crumb = `${parts[1]} / ${parts[2]?.slice(0, 12)}`;
    page = entry ? <RunPage entry={entry} params={params} setParams={setParams} /> : <div className="empty">no run {parts[2]} in {parts[1]}</div>;
  } else if (projects) page = <Runs projects={projects} />;

  return (
    <>
      <header className="top">
        <a className="brand" href="#/">prow</a>
        {crumb && <span className="crumb">{crumb}</span>}
        <div className="right">
          <a href="#/" onClick={() => refresh()}>runs</a>
        </div>
      </header>
      <main className="page">{page}</main>
    </>
  );
}

function Runs({ projects }: { projects: Project[] }) {
  if (!projects.length) return <div className="empty">no runs yet</div>;
  return (
    <>
      {projects.map((p) => (
        <section className="project" key={p.name}>
          <div className="rule"><span className="name">{p.name}</span><span className="dim">{p.runs.length} runs</span></div>
          <table className="stats runs">
            <thead>
              <tr><th>opened</th><th>run</th><th>state</th><th>workload</th><th>commit</th><th>host</th><th>length</th><th>sites</th></tr>
            </thead>
            <tbody>
              {p.runs.map(({ manifest: m, sites }) => {
                // the producer's own start: the connection opens with its first row, possibly later
                const opened = Date.parse(String(m.started ?? m.opened));
                const ended = m.ended ? Date.parse(String(m.ended)) : Date.now();
                return (
                  <tr key={String(m.run)} onClick={() => (window.location.hash = `#/run/${p.name}/${m.run}`)}>
                    <td>{new Date(opened).toLocaleString()}</td>
                    <td className="dim">{String(m.run).slice(0, 12)}</td>
                    <td className={m.state === "complete" ? "" : "warn"}>{String(m.state)}</td>
                    <td>{String(m.workload ?? "")}</td>
                    <td>{String(m["build.commit"] ?? "")}{m["build.dirty"] ? "*" : ""}</td>
                    <td>{String(m.host ?? "")}</td>
                    <td className="num">{dur((ended - opened) * 1e3)}</td>
                    <td className="dim">{Object.keys(sites).sort().join(" ")}</td>
                  </tr>
                );
              })}
            </tbody>
          </table>
        </section>
      ))}
    </>
  );
}
