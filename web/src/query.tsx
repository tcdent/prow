// The query tab: SQL against a project (or one run), and the server's query log as a history to
// pick from. A picked query fills the editor; ▶ runs it as it was.

import { useCallback, useEffect, useRef, useState } from "react";
import type { Project } from "./api";
import { objects, sql, type Result } from "./sql";
import { dur } from "./fmt";

const MAX_ROWS = 10_000;
const SHOWN = 1_000;
const DRAFT = "prow.query";

interface Past { ts: string; source: string; project: string | null; run: string | null; ms: number; rows: number | null; error: string | null; sql: string }

export function QueryPage({ projects }: { projects: Project[] }) {
  const saved = JSON.parse(localStorage.getItem(DRAFT) ?? "{}");
  const [project, setProject] = useState<string>(saved.project ?? projects[0]?.name ?? "");
  const [run, setRun] = useState<string>(saved.run ?? "");
  const [text, setText] = useState<string>(saved.sql ?? "SELECT * FROM runs");
  const [result, setResult] = useState<Result | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [running, setRunning] = useState(false);
  const [history, setHistory] = useState<Past[]>([]);
  const [withPage, setWithPage] = useState(false);
  const editor = useRef<HTMLTextAreaElement>(null);

  useEffect(() => localStorage.setItem(DRAFT, JSON.stringify({ project, run, sql: text })), [project, run, text]);

  const loadHistory = useCallback(() => {
    const where = withPage ? "" : "WHERE source <> 'page'";
    sql("", null, `SELECT ts, source, project, run, ms, rows, error, sql FROM queries ${where} ORDER BY ts DESC LIMIT 200`).then(
      (r) => setHistory(objects(r) as unknown as Past[]),
      () => setHistory([]),
    );
  }, [withPage]);
  useEffect(loadHistory, [loadHistory]);

  const go = useCallback(
    (p: string, r: string, q: string) => {
      setRunning(true);
      setError(null);
      sql(p, r || null, q, MAX_ROWS, undefined, "console")
        .then(setResult, (e) => {
          setResult(null);
          setError(String(e).replace(/^Error: /, ""));
        })
        .finally(() => {
          setRunning(false);
          loadHistory();
        });
    },
    [loadHistory],
  );

  const pick = (h: Past, now: boolean) => {
    setProject(h.project ?? "");
    setRun(h.run ?? "");
    setText(h.sql);
    window.scrollTo({ top: 0 });
    editor.current?.focus();
    if (now) go(h.project ?? "", h.run ?? "", h.sql);
  };

  const runs = projects.find((p) => p.name === project)?.runs ?? [];
  return (
    <div className="query">
      <div className="query-bar">
        <select value={project} onChange={(e) => { setProject(e.target.value); setRun(""); }}>
          {projects.map((p) => <option key={p.name} value={p.name}>{p.name}</option>)}
          <option value="">(the query log only)</option>
        </select>
        <select value={run} onChange={(e) => setRun(e.target.value)} disabled={!project}>
          <option value="">every run</option>
          {runs.map(({ manifest: m }) => (
            <option key={String(m.run)} value={String(m.run)}>{String(m.run).slice(0, 12)} · {new Date(String(m.started ?? m.opened)).toLocaleString()}{m.workload ? ` · ${m.workload}` : ""}</option>
          ))}
        </select>
        <button onClick={() => go(project, run, text)} disabled={running}>{running ? "running…" : "▶ run"}</button>
        <span className="dim">ctrl+enter</span>
      </div>
      <textarea
        ref={editor}
        className="editor"
        value={text}
        spellCheck={false}
        onChange={(e) => setText(e.target.value)}
        onKeyDown={(e) => {
          if (e.key === "Enter" && (e.ctrlKey || e.metaKey)) {
            e.preventDefault();
            go(project, run, text);
          }
        }}
      />
      {error && <pre className="query-error">{error}</pre>}
      {result && <ResultTable r={result} />}

      <div className="rule history-rule">
        <span className="name">history</span>
        <label className="dim"><input type="checkbox" checked={withPage} onChange={(e) => setWithPage(e.target.checked)} /> the page's own queries</label>
      </div>
      <table className="history">
        <tbody>
          {history.map((h, i) => (
            <tr key={i} onClick={() => pick(h, false)} className={h.error ? "failed" : ""} title={h.error ?? undefined}>
              <td className="dim">{new Date(h.ts).toLocaleString()}</td>
              <td className="dim">{h.source}</td>
              <td>{h.project ?? ""}{h.run ? <span className="dim"> / {h.run.slice(0, 8)}</span> : ""}</td>
              <td className="num">{dur(h.ms * 1e3)}</td>
              <td className="num">{h.error ? "error" : h.rows}</td>
              <td className="sql">{h.sql.replace(/\s+/g, " ")}</td>
              <td><button className="again" onClick={(e) => { e.stopPropagation(); pick(h, true); }} title="run again">▶</button></td>
            </tr>
          ))}
        </tbody>
      </table>
    </div>
  );
}

function ResultTable({ r }: { r: Result }) {
  const shown = r.rows.slice(0, SHOWN);
  const numeric = r.columns.map((_, i) => shown.every((row) => row[i] === null || typeof row[i] === "number"));
  const cell = (v: unknown) => (v === null ? <span className="dim">NULL</span> : typeof v === "object" ? JSON.stringify(v) : String(v));
  return (
    <>
      <div className="dim result-facts">
        {r.rows.length.toLocaleString()} rows{r.truncated ? `, cut at ${MAX_ROWS.toLocaleString()}` : ""}
        {r.rows.length > SHOWN ? `, the first ${SHOWN.toLocaleString()} shown` : ""} · {dur(r.ms * 1e3)}
      </div>
      <div className="result-box">
        <table className="result">
          <thead>
            <tr>{r.columns.map((c, i) => <th key={i} className={numeric[i] ? "num" : ""}>{c}</th>)}</tr>
          </thead>
          <tbody>
            {shown.map((row, j) => (
              <tr key={j}>{row.map((v, i) => <td key={i} className={numeric[i] ? "num" : ""}>{cell(v)}</td>)}</tr>
            ))}
          </tbody>
        </table>
      </div>
    </>
  );
}
