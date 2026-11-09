// Hash routes: #/ every project's runs, #/run/<project>/<run>?f=&t=&p= one run, its view range (µs)
// and panel, #/project/<project> all of a project's runs, #/query the query tab. No router dependency; the URL is the state, so a view is shareable.

import { useCallback, useEffect, useState, type ReactNode } from "react";
import { loadIndex, type Project } from "./api";
import { useInFlight } from "./sql";
import { RunPage } from "./run";
import { QueryPage } from "./query";
import { ProjectPage, Projects } from "./projects";

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
  const busy = useInFlight();
  const [path, qs = ""] = hash.replace(/^#\/?/, "").split("?");
  const parts = path.split("/").map(decodeURIComponent).filter(Boolean);
  const params = new URLSearchParams(qs);
  const [projects, setProjects] = useState<Project[] | null>(null);
  const [error, setError] = useState<string | null>(null);
  const refresh = useCallback(() => loadIndex().then(setProjects, (e) => setError(String(e))), []);
  useEffect(() => {
    refresh();
    // the lists keep their runs' status current; a run's page has its own reload
    if (parts[0] === "run") return;
    const timer = setInterval(refresh, 10_000);
    return () => clearInterval(timer);
  }, [refresh, parts[0]]); // eslint-disable-line react-hooks/exhaustive-deps

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
  let crumb: ReactNode = null;
  if (error) page = <div className="empty">{error}</div>;
  else if (projects && parts[0] === "run") {
    const entry = projects.find((p) => p.name === parts[1])?.runs.find((r) => r.manifest.run === parts[2]);
    crumb = <><a href={`#/project/${parts[1]}`}>{parts[1]}</a> / {parts[2]?.slice(0, 12)}</>;
    page = entry ? <RunPage project={parts[1]} entry={entry} params={params} setParams={setParams} reload={refresh} /> : <div className="empty">no run {parts[2]} in {parts[1]}</div>;
  } else if (projects && parts[0] === "project") {
    const p = projects.find((p) => p.name === parts[1]);
    crumb = parts[1];
    page = p ? <ProjectPage project={p} /> : <div className="empty">no project {parts[1]}</div>;
  } else if (projects && parts[0] === "query") page = <QueryPage projects={projects} />;
  else if (projects) page = <Projects projects={projects} />;

  return (
    <>
      <header className="top">
        <a className="brand" href="#/">prow</a>
        <nav className="tabs">
          <a href="#/" className={parts[0] !== "query" ? "on" : ""}>runs</a>
          <a href="#/query" className={parts[0] === "query" ? "on" : ""}>query</a>
        </nav>
        {crumb && <span className="crumb">{crumb}</span>}
        <div className="right">{busy && <span className="spinner" title="loading" />}</div>
      </header>
      <main className="page">{page}</main>
    </>
  );
}
