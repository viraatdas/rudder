import { useEffect, useMemo, useState } from "preact/hooks";
import {
  type AccountQuota,
  type MachineReport,
  type ProjectRow,
  type QuotaReport,
  type TokenUsageReport,
  type Totals,
  type UsageRange,
  fetchMachine,
  fetchQuota,
  fetchTokenUsage,
} from "./types";

type Mode = "tokens" | "machine";
type Metric = "cost" | "tokens";
type Provider = "claude" | "codex";

const PROVIDER_LABEL: Record<Provider, string> = { claude: "Claude Code", codex: "Codex" };

function fmtUsd(n: number): string {
  if (n >= 100) return `$${n.toFixed(0)}`;
  if (n >= 10) return `$${n.toFixed(1)}`;
  return `$${n.toFixed(2)}`;
}

function fmtTokens(n: number): string {
  if (n >= 1_000_000_000) return `${(n / 1_000_000_000).toFixed(2)}B`;
  if (n >= 1_000_000) return `${(n / 1_000_000).toFixed(2)}M`;
  if (n >= 1_000) return `${(n / 1_000).toFixed(1)}k`;
  return String(Math.round(n));
}

function fmtBytes(n: number): string {
  if (n >= 1 << 30) return `${(n / (1 << 30)).toFixed(2)} GB`;
  if (n >= 1 << 20) return `${(n / (1 << 20)).toFixed(0)} MB`;
  return `${(n / 1024).toFixed(0)} KB`;
}

function tokensOf(t: Totals): number {
  return t.input + t.output + t.cacheWrite + t.cacheRead;
}

function metricOf(t: Totals, metric: Metric): number {
  return metric === "cost" ? t.cost : tokensOf(t);
}

function fmtMetric(n: number, metric: Metric): string {
  return metric === "cost" ? fmtUsd(n) : fmtTokens(n);
}

function untilReset(iso: string | null, now: number): string {
  if (!iso) return "";
  const ms = new Date(iso).getTime() - now;
  if (Number.isNaN(ms)) return "";
  if (ms <= 0) return "resets now";
  const mins = Math.round(ms / 60_000);
  if (mins < 60) return `resets in ${mins}m`;
  const hours = Math.floor(mins / 60);
  if (hours < 24) return `resets in ${hours}h ${mins % 60}m`;
  const days = Math.floor(hours / 24);
  return `resets in ${days}d ${hours % 24}h`;
}

function shortModel(model: string): string {
  const m = model.replace(/^claude-/, "").replace(/-\d{8}$/, "");
  return m;
}

function shortDay(day: string): string {
  const [, m, d] = day.split("-");
  return `${Number(m)}/${Number(d)}`;
}

export function UsageView() {
  const [mode, setMode] = useState<Mode>("tokens");
  return (
    <div class="page usage-page">
      <header class="topbar">
        <div class="brand">
          <a class="brand-link" href="/rudder" title="Back to all projects">
            <span class="brand-mark">▰</span>
            <span class="brand-back">← all projects</span>
          </a>
          <span class="brand-name">Usage</span>
          <span class="brand-sub mono">this machine</span>
        </div>
        <div class="toolbar-actions">
          <div class="view-toggle" role="group" aria-label="Usage view">
            <button
              type="button"
              aria-pressed={mode === "tokens"}
              class={`toggle ${mode === "tokens" ? "toggle-on" : ""}`}
              onClick={() => setMode("tokens")}
            >
              Token usage
            </button>
            <button
              type="button"
              aria-pressed={mode === "machine"}
              class={`toggle ${mode === "machine" ? "toggle-on" : ""}`}
              onClick={() => setMode("machine")}
            >
              Machine resources
            </button>
          </div>
        </div>
      </header>
      <main class="usage-main">{mode === "tokens" ? <TokenUsage /> : <MachineResources />}</main>
    </div>
  );
}

// ---------------------------------------------------------------------------
// Quota meters.
// ---------------------------------------------------------------------------

function QuotaMeters() {
  const [report, setReport] = useState<QuotaReport | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [now, setNow] = useState(Date.now());

  const load = (force: boolean) => {
    setBusy(true);
    fetchQuota(force)
      .then((r) => {
        setReport(r);
        setError(null);
      })
      .catch((e) => setError(String(e?.message ?? e)))
      .finally(() => setBusy(false));
  };

  useEffect(() => {
    load(false);
    const tick = setInterval(() => setNow(Date.now()), 30_000);
    const refresh = setInterval(() => load(false), 5 * 60_000);
    return () => {
      clearInterval(tick);
      clearInterval(refresh);
    };
  }, []);

  return (
    <section class="usage-section">
      <div class="usage-section-head">
        <h2 class="usage-h2">Subscription quota</h2>
        <div class="usage-section-actions">
          {report && (
            <span class="usage-muted mono">
              refreshed {new Date(report.fetchedAt).toLocaleTimeString()} · every {Math.round(report.ttlSeconds / 60)}m
            </span>
          )}
          <button type="button" class="btn" disabled={busy} onClick={() => load(true)}>
            {busy ? "refreshing…" : "refresh"}
          </button>
        </div>
      </div>
      {error && <div class="banner banner-error mono">quota failed: {error}</div>}
      <div class="quota-grid">
        {(report?.accounts ?? []).map((account) => (
          <QuotaCard key={account.provider} account={account} now={now} />
        ))}
        {!report && !error && (
          <>
            <div class="quota-card quota-card-skeleton" />
            <div class="quota-card quota-card-skeleton" />
          </>
        )}
      </div>
      <p class="usage-note">
        Read from the providers' own usage endpoints with the sign-ins the CLIs already hold. Rudder never handles a
        password and never stores a token.
      </p>
    </section>
  );
}

function meterClass(percent: number): string {
  if (percent >= 90) return "meter-fill meter-hot";
  if (percent >= 70) return "meter-fill meter-warm";
  return "meter-fill";
}

export function QuotaCard({ account, now }: { account: AccountQuota; now: number }) {
  return (
    <div class={`quota-card ${account.error ? "quota-card-error" : ""}`}>
      <div class="quota-head">
        <span class={`provider-dot provider-${account.provider}`} aria-hidden="true" />
        <span class="quota-provider">{PROVIDER_LABEL[account.provider]}</span>
        {account.plan && <span class="quota-plan mono">{account.plan}</span>}
      </div>
      {account.email && <div class="quota-email mono">{account.email}</div>}
      {account.error ? (
        <div class="quota-error mono">{account.error}</div>
      ) : (
        <div class="meters">
          {account.windows.map((w) => (
            <div class="meter" key={w.label}>
              <div class="meter-row">
                <span class="meter-label">{w.label}</span>
                <span class="meter-value mono">{w.percent}%</span>
              </div>
              <div class="meter-track" role="progressbar" aria-valuenow={w.percent} aria-valuemin={0} aria-valuemax={100}>
                <div class={meterClass(w.percent)} style={{ width: `${Math.min(100, w.percent)}%` }} />
              </div>
              <div class="meter-reset mono">{untilReset(w.resetsAt, now)}</div>
            </div>
          ))}
        </div>
      )}
    </div>
  );
}

// ---------------------------------------------------------------------------
// Token costs.
// ---------------------------------------------------------------------------

function TokenUsage() {
  const [range, setRange] = useState<UsageRange>("30d");
  const [report, setReport] = useState<TokenUsageReport | null>(null);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    let cancelled = false;
    setReport(null);
    fetchTokenUsage(range)
      .then((r) => {
        if (cancelled) return;
        setReport(r);
        setError(null);
      })
      .catch((e) => {
        if (!cancelled) setError(String(e?.message ?? e));
      });
    return () => {
      cancelled = true;
    };
  }, [range]);

  return (
    <>
      <QuotaMeters />
      <TokenReportView report={report} error={error} range={range} onRange={setRange} />
    </>
  );
}

/** The token-usage section for one loaded report. Pure: exported for tests. */
export function TokenReportView({
  report,
  error,
  range,
  onRange,
}: {
  report: TokenUsageReport | null;
  error: string | null;
  range: UsageRange;
  onRange: (r: UsageRange) => void;
}) {
  const [metric, setMetric] = useState<Metric>("cost");
  const [hidden, setHidden] = useState<Set<Provider>>(new Set());
  const [selectedDay, setSelectedDay] = useState<string | null>(null);
  const [selectedModel, setSelectedModel] = useState<string | null>(null);
  const [selectedProject, setSelectedProject] = useState<string | null>(null);
  const [showAllProjects, setShowAllProjects] = useState(false);

  const toggleProvider = (p: Provider) => {
    setHidden((prev) => {
      const next = new Set(prev);
      if (next.has(p)) next.delete(p);
      else next.add(p);
      return next;
    });
  };

  const visibleTotal = useMemo(() => {
    if (!report) return 0;
    let sum = 0;
    for (const p of ["claude", "codex"] as Provider[]) {
      if (!hidden.has(p)) sum += metricOf(report.providers[p], metric);
    }
    return sum;
  }, [report, hidden, metric]);

  const day = report?.days.find((d) => d.day === selectedDay) ?? null;

  return (
    <>
      <section class="usage-section">
        <div class="usage-section-head">
          <h2 class="usage-h2">Token usage</h2>
          <div class="usage-section-actions">
            <div class="view-toggle" role="group" aria-label="Metric">
              {(["cost", "tokens"] as Metric[]).map((m) => (
                <button
                  key={m}
                  type="button"
                  aria-pressed={metric === m}
                  class={`toggle ${metric === m ? "toggle-on" : ""}`}
                  onClick={() => setMetric(m)}
                >
                  {m === "cost" ? "Cost" : "Tokens"}
                </button>
              ))}
            </div>
            <div class="view-toggle" role="group" aria-label="Range">
              {(["7d", "30d", "90d"] as UsageRange[]).map((r) => (
                <button
                  key={r}
                  type="button"
                  aria-pressed={range === r}
                  class={`toggle ${range === r ? "toggle-on" : ""}`}
                  onClick={() => onRange(r)}
                >
                  {r}
                </button>
              ))}
            </div>
          </div>
        </div>

        {error && <div class="banner banner-error mono">usage failed: {error}</div>}
        {!report && !error && <div class="usage-loading mono">scanning session logs…</div>}

        {report && (
          <>
            <div class="stat-row">
              <div class="stat">
                <div class="stat-label">Cost to you</div>
                <div class="stat-value">$0</div>
                <div class="stat-sub">subscription</div>
              </div>
              <div class="stat">
                <div class="stat-label">At API rates</div>
                <div class="stat-value">{fmtUsd(report.total.cost)}</div>
                <div class="stat-sub">{fmtTokens(tokensOf(report.total))} tokens · {report.total.calls} calls</div>
              </div>
              <div class="stat">
                <div class="stat-label">Caching saved</div>
                <div class="stat-value">{fmtUsd(report.total.cacheSavings)}</div>
                <div class="stat-sub">{fmtTokens(report.total.cacheRead)} cache reads</div>
              </div>
              <div class="stat">
                <div class="stat-label">Output</div>
                <div class="stat-value">{fmtTokens(report.total.output)}</div>
                <div class="stat-sub">{fmtTokens(report.total.input + report.total.cacheWrite)} fresh input</div>
              </div>
            </div>

            <div class="provider-chips">
              {(["claude", "codex"] as Provider[]).map((p) => (
                <button
                  key={p}
                  type="button"
                  class={`chip ${hidden.has(p) ? "chip-off" : ""}`}
                  aria-pressed={!hidden.has(p)}
                  onClick={() => toggleProvider(p)}
                  title="Click to show or hide in the chart"
                >
                  <span class={`provider-dot provider-${p}`} aria-hidden="true" />
                  {PROVIDER_LABEL[p]}
                  <span class="chip-value mono">{fmtMetric(metricOf(report.providers[p], metric), metric)}</span>
                </button>
              ))}
              <span class="usage-muted mono">
                {fmtMetric(visibleTotal, metric)} shown · {report.scannedFiles} logs
              </span>
            </div>

            <DailyChart report={report} metric={metric} hidden={hidden} selected={selectedDay} onSelect={setSelectedDay} />

            {day && (
              <div class="day-detail">
                <div class="day-detail-head">
                  <strong>{day.day}</strong>
                  <button type="button" class="btn" onClick={() => setSelectedDay(null)}>
                    close
                  </button>
                </div>
                <div class="day-detail-grid">
                  {(["claude", "codex"] as Provider[]).map((p) => (
                    <div key={p} class="day-detail-cell">
                      <span class={`provider-dot provider-${p}`} aria-hidden="true" /> {PROVIDER_LABEL[p]}
                      <div class="mono">
                        {fmtUsd(day[p].cost)} · {fmtTokens(tokensOf(day[p]))} tokens · {day[p].calls} calls
                      </div>
                    </div>
                  ))}
                </div>
              </div>
            )}

            <div class="usage-columns">
              <div>
                <h3 class="usage-h3">Models</h3>
                <table class="usage-table">
                  <thead>
                    <tr>
                      <th>model</th>
                      <th class="num">{metric === "cost" ? "cost" : "tokens"}</th>
                      <th class="num">output</th>
                      <th class="num">cache read</th>
                      <th class="num">sessions</th>
                    </tr>
                  </thead>
                  <tbody>
                    {report.models.map((row) => {
                      const key = `${row.provider}:${row.model}`;
                      const open = selectedModel === key;
                      return (
                        <>
                          <tr
                            key={key}
                            class={`usage-row ${open ? "usage-row-open" : ""}`}
                            onClick={() => setSelectedModel(open ? null : key)}
                          >
                            <td>
                              <span class={`provider-dot provider-${row.provider}`} aria-hidden="true" />
                              <span class="mono">{shortModel(row.model)}</span>
                            </td>
                            <td class="num mono">{fmtMetric(metricOf(row.totals, metric), metric)}</td>
                            <td class="num mono">{fmtTokens(row.totals.output)}</td>
                            <td class="num mono">{fmtTokens(row.totals.cacheRead)}</td>
                            <td class="num mono">{row.sessions}</td>
                          </tr>
                          {open && (
                            <tr class="usage-drill" key={`${key}-drill`}>
                              <td colSpan={5}>
                                <TotalsBreakdown totals={row.totals} />
                              </td>
                            </tr>
                          )}
                        </>
                      );
                    })}
                    {report.models.length === 0 && (
                      <tr>
                        <td colSpan={5} class="usage-muted mono">
                          no usage in this range
                        </td>
                      </tr>
                    )}
                  </tbody>
                </table>
              </div>

              <div>
                <div class="usage-h3-row">
                  <h3 class="usage-h3">Workspaces</h3>
                  {report.projects.length > 8 && (
                    <button type="button" class="btn-link mono" onClick={() => setShowAllProjects((v) => !v)}>
                      {showAllProjects ? "top 8" : `All ${report.projects.length} →`}
                    </button>
                  )}
                </div>
                <ProjectBars
                  projects={showAllProjects ? report.projects : report.projects.slice(0, 8)}
                  metric={metric}
                  selected={selectedProject}
                  onSelect={setSelectedProject}
                />
              </div>
            </div>
            <p class="usage-note">{report.pricingNote}</p>
          </>
        )}
      </section>
    </>
  );
}

function TotalsBreakdown({ totals }: { totals: Totals }) {
  return (
    <div class="totals-breakdown mono">
      <span>input {fmtTokens(totals.input)}</span>
      <span>output {fmtTokens(totals.output)}</span>
      <span>cache write {fmtTokens(totals.cacheWrite)}</span>
      <span>cache read {fmtTokens(totals.cacheRead)}</span>
      <span>{fmtUsd(totals.cost)} at API rates</span>
      <span>{fmtUsd(totals.cacheSavings)} saved by caching</span>
    </div>
  );
}

function DailyChart({
  report,
  metric,
  hidden,
  selected,
  onSelect,
}: {
  report: TokenUsageReport;
  metric: Metric;
  hidden: Set<Provider>;
  selected: string | null;
  onSelect: (day: string | null) => void;
}) {
  const days = report.days;
  const values = days.map((d) => ({
    day: d.day,
    claude: hidden.has("claude") ? 0 : metricOf(d.claude, metric),
    codex: hidden.has("codex") ? 0 : metricOf(d.codex, metric),
  }));
  const max = Math.max(1e-9, ...values.map((v) => v.claude + v.codex));
  const width = 900;
  const height = 180;
  const padL = 44;
  const padB = 22;
  const padT = 8;
  const innerW = width - padL - 8;
  const innerH = height - padB - padT;
  const slot = innerW / Math.max(1, values.length);
  const barW = Math.max(2, slot * 0.7);
  const labelEvery = values.length > 45 ? 10 : values.length > 14 ? 5 : 1;
  const ticks = [0, 0.5, 1].map((f) => ({ f, label: fmtMetric(max * f, metric) }));

  return (
    <div class="chart-wrap">
      <svg class="daily-chart" viewBox={`0 0 ${width} ${height}`} role="img" aria-label="Daily usage">
        {ticks.map((t) => {
          const y = padT + innerH - innerH * t.f;
          return (
            <g key={t.f}>
              <line x1={padL} x2={width - 8} y1={y} y2={y} class="chart-grid" />
              <text x={padL - 6} y={y + 4} class="chart-tick" text-anchor="end">
                {t.label}
              </text>
            </g>
          );
        })}
        {values.map((v, i) => {
          const x = padL + i * slot + (slot - barW) / 2;
          const hClaude = (v.claude / max) * innerH;
          const hCodex = (v.codex / max) * innerH;
          const yCodex = padT + innerH - hCodex;
          const yClaude = yCodex - hClaude;
          const isSel = selected === v.day;
          return (
            <g
              key={v.day}
              class={`chart-bar ${isSel ? "chart-bar-selected" : ""}`}
              onClick={() => onSelect(isSel ? null : v.day)}
            >
              <title>
                {v.day}: {fmtMetric(v.claude + v.codex, metric)}
              </title>
              <rect x={padL + i * slot} y={padT} width={slot} height={innerH} class="chart-hit" />
              <rect x={x} y={yCodex} width={barW} height={hCodex} class="bar-codex" />
              <rect x={x} y={yClaude} width={barW} height={hClaude} class="bar-claude" />
              {i % labelEvery === 0 && (
                <text x={x + barW / 2} y={height - 6} class="chart-tick" text-anchor="middle">
                  {shortDay(v.day)}
                </text>
              )}
            </g>
          );
        })}
      </svg>
    </div>
  );
}

function ProjectBars({
  projects,
  metric,
  selected,
  onSelect,
}: {
  projects: ProjectRow[];
  metric: Metric;
  selected: string | null;
  onSelect: (key: string | null) => void;
}) {
  const max = Math.max(1e-9, ...projects.map((p) => metricOf(p.totals, metric)));
  const [copied, setCopied] = useState<string | null>(null);
  const copy = (id: string) => {
    navigator.clipboard?.writeText(id).then(
      () => {
        setCopied(id);
        setTimeout(() => setCopied(null), 1200);
      },
      () => setCopied(null),
    );
  };
  if (projects.length === 0) return <div class="usage-muted mono">no workspaces in this range</div>;
  return (
    <div class="project-bars">
      {projects.map((p) => {
        const value = metricOf(p.totals, metric);
        const open = selected === p.key;
        return (
          <div key={p.key} class={`project-bar ${open ? "project-bar-open" : ""}`}>
            <button type="button" class="project-bar-btn" onClick={() => onSelect(open ? null : p.key)}>
              <div class="project-bar-row">
                <span class="project-name">
                  {p.slug ? (
                    <a href={`/rudder/${encodeURIComponent(p.slug)}`} onClick={(e) => e.stopPropagation()}>
                      {p.name}
                    </a>
                  ) : (
                    p.name
                  )}
                </span>
                <span class="mono project-value">{fmtMetric(value, metric)}</span>
              </div>
              <div class="project-track">
                <div class="project-fill" style={{ width: `${(value / max) * 100}%` }} />
              </div>
              <div class="project-path mono">{p.key}</div>
            </button>
            {open && (
              <div class="project-sessions">
                <div class="usage-muted mono">top sessions by cost · click to copy the id for --resume</div>
                {p.sessions.slice(0, 10).map((s) => (
                  <button
                    key={s.id}
                    type="button"
                    class="session-row"
                    title={`${s.provider} ${s.id}`}
                    onClick={() => copy(s.id)}
                  >
                    <span class={`provider-dot provider-${s.provider}`} aria-hidden="true" />
                    <span class="mono session-id">{copied === s.id ? "copied ✓" : s.id.slice(0, 8)}</span>
                    <span class="mono session-model">{shortModel(s.model)}</span>
                    <span class="mono session-when">{new Date(s.lastAt).toLocaleString()}</span>
                    <span class="mono session-cost">{fmtMetric(metricOf(s.totals, metric), metric)}</span>
                  </button>
                ))}
              </div>
            )}
          </div>
        );
      })}
    </div>
  );
}

// ---------------------------------------------------------------------------
// Machine resources.
// ---------------------------------------------------------------------------

function MachineResources() {
  const [report, setReport] = useState<MachineReport | null>(null);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    let cancelled = false;
    const load = () =>
      fetchMachine()
        .then((r) => {
          if (cancelled) return;
          setReport(r);
          setError(null);
        })
        .catch((e) => {
          if (!cancelled) setError(String(e?.message ?? e));
        });
    load();
    const timer = setInterval(load, 2000);
    return () => {
      cancelled = true;
      clearInterval(timer);
    };
  }, []);

  if (error) return <div class="banner banner-error mono">machine failed: {error}</div>;
  if (!report) return <div class="usage-loading mono">sampling…</div>;
  return <MachineReportView report={report} />;
}

/** The machine section for one sample. Pure: exported for tests. */
export function MachineReportView({ report }: { report: MachineReport }) {
  const used = report.totalMem - report.freeMem;
  const agentRss = report.byKind.claude.rss + report.byKind.codex.rss + report.byKind.opencode.rss;
  const rudderRss = report.byKind.rudder.rss;
  const otherUsed = Math.max(0, used - agentRss - rudderRss);
  const pct = (n: number) => `${((n / report.totalMem) * 100).toFixed(1)}%`;

  return (
    <section class="usage-section">
      <div class="usage-section-head">
        <h2 class="usage-h2">Machine resources</h2>
        <span class="usage-muted mono">
          live · every 2s · load {report.load.map((l) => l.toFixed(2)).join(" ")} on {report.cpus} cores
        </span>
      </div>
      <div class="stat-row">
        <div class="stat">
          <div class="stat-label">Rudder</div>
          <div class="stat-value">{fmtBytes(rudderRss)}</div>
          <div class="stat-sub">{report.byKind.rudder.cpu.toFixed(1)}% cpu · {report.byKind.rudder.count} procs</div>
        </div>
        <div class="stat">
          <div class="stat-label">Agents</div>
          <div class="stat-value">{fmtBytes(agentRss)}</div>
          <div class="stat-sub">
            {(report.byKind.claude.cpu + report.byKind.codex.cpu + report.byKind.opencode.cpu).toFixed(1)}% cpu ·{" "}
            {report.byKind.claude.count + report.byKind.codex.count + report.byKind.opencode.count} procs
          </div>
        </div>
        <div class="stat">
          <div class="stat-label">System RAM</div>
          <div class="stat-value">
            {fmtBytes(used)} / {fmtBytes(report.totalMem)}
          </div>
          <div class="stat-sub">{pct(used)} in use</div>
        </div>
      </div>
      <div class="ram-bar" role="img" aria-label="RAM split">
        <div class="ram-seg ram-rudder" style={{ width: pct(rudderRss) }} title={`rudder ${fmtBytes(rudderRss)}`} />
        <div class="ram-seg ram-agents" style={{ width: pct(agentRss) }} title={`agents ${fmtBytes(agentRss)}`} />
        <div class="ram-seg ram-other" style={{ width: pct(otherUsed) }} title={`everything else ${fmtBytes(otherUsed)}`} />
      </div>
      <div class="ram-legend mono">
        <span>
          <i class="ram-swatch ram-rudder" /> rudder
        </span>
        <span>
          <i class="ram-swatch ram-agents" /> agents
        </span>
        <span>
          <i class="ram-swatch ram-other" /> everything else
        </span>
        <span>free {fmtBytes(report.freeMem)}</span>
      </div>

      <h3 class="usage-h3">Processes</h3>
      <table class="usage-table">
        <thead>
          <tr>
            <th>kind</th>
            <th>pid</th>
            <th class="num">cpu</th>
            <th class="num">memory</th>
            <th>uptime</th>
            <th>command</th>
          </tr>
        </thead>
        <tbody>
          {report.processes.map((p) => (
            <tr key={p.pid}>
              <td>
                <span class={`provider-dot provider-${p.kind}`} aria-hidden="true" />
                {p.kind}
              </td>
              <td class="mono">{p.pid}</td>
              <td class="num mono">{p.cpu.toFixed(1)}%</td>
              <td class="num mono">{fmtBytes(p.rss)}</td>
              <td class="mono">{p.elapsed}</td>
              <td class="mono cmd" title={p.command}>
                {p.command.length > 90 ? `${p.command.slice(0, 90)}…` : p.command}
              </td>
            </tr>
          ))}
          {report.processes.length === 0 && (
            <tr>
              <td colSpan={6} class="usage-muted mono">
                no agent processes running
              </td>
            </tr>
          )}
        </tbody>
      </table>
    </section>
  );
}
