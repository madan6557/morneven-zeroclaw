import { useCallback, useEffect, useMemo, useState } from "react";
import {
  Activity,
  AlertTriangle,
  Bot,
  CheckCircle2,
  Clock,
  Database,
  FileText,
  FolderTree,
  KeyRound,
  Loader2,
  Play,
  RefreshCw,
  RotateCw,
  ShieldCheck,
  Square,
} from "lucide-react";
import {
  clearMornevenToken,
  getMornevenProviderUsage,
  getMornevenStatus,
  getMornevenTelegramTopics,
  getMornevenToken,
  getMornevenWorkspaceChanges,
  reloadMornevenBundle,
  runMornevenGatewayAction,
  runMornevenRuntimeAction,
  saveMornevenToken,
  type MornevenMaterializedRuntime,
  type MornevenProviderUsageEvent,
  type MornevenRuntimeAction,
  type MornevenRuntimeStatus,
  type MornevenStatusResponse,
  type MornevenTelegramTopicsResponse,
  type MornevenWorkspaceChangesResponse,
  type MornevenWorkspaceRuntime,
} from "@/lib/api";

const USAGE_WINDOW_DAYS = 7;

type Tone = "success" | "warning" | "error" | "info" | "muted";

interface RuntimeView extends MornevenRuntimeStatus {
  materialized?: MornevenMaterializedRuntime;
  workspaceAudit?: MornevenWorkspaceRuntime;
  topicGroups?: unknown[];
}

function toneStyle(tone: Tone): { color: string; borderColor: string; background: string } {
  switch (tone) {
    case "success":
      return {
        color: "var(--color-status-success)",
        borderColor: "var(--color-status-success-alpha-20)",
        background: "var(--color-status-success-alpha-08)",
      };
    case "warning":
      return {
        color: "var(--color-status-warning)",
        borderColor: "var(--color-status-warning-alpha-20)",
        background: "var(--color-status-warning-alpha-05)",
      };
    case "error":
      return {
        color: "var(--color-status-error)",
        borderColor: "var(--color-status-error-alpha-20)",
        background: "var(--color-status-error-alpha-08)",
      };
    case "info":
      return {
        color: "var(--pc-accent)",
        borderColor: "var(--pc-accent-dim)",
        background: "var(--pc-accent-glow)",
      };
    case "muted":
    default:
      return {
        color: "var(--pc-text-muted)",
        borderColor: "var(--pc-border)",
        background: "var(--pc-bg-elevated)",
      };
  }
}

function Badge({ tone, children }: { tone: Tone; children: React.ReactNode }) {
  return (
    <span
      className="inline-flex items-center gap-1 rounded-full border px-2.5 py-1 text-xs font-semibold"
      style={toneStyle(tone)}
    >
      {children}
    </span>
  );
}

function formatDuration(seconds?: number | null): string {
  if (!seconds || seconds < 0) return "0s";
  const days = Math.floor(seconds / 86400);
  const hours = Math.floor((seconds % 86400) / 3600);
  const minutes = Math.floor((seconds % 3600) / 60);
  if (days > 0) return `${days}d ${hours}h`;
  if (hours > 0) return `${hours}h ${minutes}m`;
  if (minutes > 0) return `${minutes}m`;
  return `${Math.floor(seconds)}s`;
}

function formatNumber(value: number): string {
  return new Intl.NumberFormat(undefined, { maximumFractionDigits: 0 }).format(value);
}

function formatUsd(value: number): string {
  return `$${value.toFixed(value >= 1 ? 4 : 6)}`;
}

function formatDate(value?: string | null): string {
  if (!value) return "-";
  const date = new Date(value);
  if (Number.isNaN(date.getTime())) return value;
  return date.toLocaleString(undefined, {
    month: "short",
    day: "2-digit",
    hour: "2-digit",
    minute: "2-digit",
  });
}

function compactPath(value?: string): string {
  if (!value) return "-";
  const parts = value.replace(/\\/g, "/").split("/").filter(Boolean);
  if (parts.length <= 4) return value;
  return `.../${parts.slice(-4).join("/")}`;
}

function providerLabel(value: unknown): string {
  if (typeof value === "string" && value.trim()) return value;
  if (value && typeof value === "object") {
    const record = value as Record<string, unknown>;
    for (const key of ["provider", "name", "type", "alias", "modelProvider"]) {
      const raw = record[key];
      if (typeof raw === "string" && raw.trim()) return raw;
    }
  }
  return "auto";
}

function countTopics(groups: unknown[] | undefined): number {
  if (!Array.isArray(groups)) return 0;
  return groups.reduce<number>((total, group) => {
    if (!group || typeof group !== "object") return total;
    const record = group as Record<string, unknown>;
    const topics = record.topics ?? record.threads ?? record.topicRegistry;
    if (Array.isArray(topics)) return total + topics.length;
    if (topics && typeof topics === "object") return total + Object.keys(topics).length;
    return total;
  }, 0);
}

function usageCost(event: MornevenProviderUsageEvent): number {
  const cost = event.usage?.cost_usd ?? event.usage?.cost ?? 0;
  return Number.isFinite(cost) ? cost : 0;
}

function actionLabel(action: MornevenRuntimeAction): string {
  if (action === "start") return "Start";
  if (action === "stop") return "Stop";
  return "Restart";
}

function actionIcon(action: MornevenRuntimeAction, spinning = false) {
  const className = `h-4 w-4 ${spinning ? "animate-spin" : ""}`;
  if (action === "start") return <Play className={className} />;
  if (action === "stop") return <Square className={className} />;
  return <RotateCw className={className} />;
}

function MetricCard({
  icon,
  label,
  value,
  detail,
}: {
  icon: React.ReactNode;
  label: string;
  value: string;
  detail?: string;
}) {
  return (
    <div className="card p-4">
      <div className="mb-3 flex items-center gap-2">
        <span style={{ color: "var(--pc-accent)" }}>{icon}</span>
        <span className="text-xs font-semibold uppercase tracking-wider" style={{ color: "var(--pc-text-muted)" }}>
          {label}
        </span>
      </div>
      <div className="text-xl font-semibold" style={{ color: "var(--pc-text-primary)" }}>
        {value}
      </div>
      {detail && (
        <div className="mt-1 truncate text-sm" style={{ color: "var(--pc-text-muted)" }}>
          {detail}
        </div>
      )}
    </div>
  );
}

export default function MornevenBotManager() {
  const [token, setToken] = useState(() => getMornevenToken());
  const [tokenDraft, setTokenDraft] = useState(() => getMornevenToken());
  const [status, setStatus] = useState<MornevenStatusResponse | null>(null);
  const [workspaceAudit, setWorkspaceAudit] = useState<MornevenWorkspaceChangesResponse | null>(null);
  const [telegramTopics, setTelegramTopics] = useState<MornevenTelegramTopicsResponse | null>(null);
  const [usageEvents, setUsageEvents] = useState<MornevenProviderUsageEvent[]>([]);
  const [loading, setLoading] = useState(false);
  const [actionKey, setActionKey] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [lastLoadedAt, setLastLoadedAt] = useState<string | null>(null);

  const load = useCallback(
    async (options: { quiet?: boolean; tokenOverride?: string } = {}) => {
      const activeToken = options.tokenOverride ?? token;
      if (!activeToken) {
        setStatus(null);
        return;
      }
      if (!options.quiet) setLoading(true);
      setError(null);
      try {
        const nextStatus = await getMornevenStatus(activeToken);
        setStatus(nextStatus);
        setLastLoadedAt(new Date().toISOString());

        const now = new Date();
        const from = new Date(now);
        from.setDate(from.getDate() - USAGE_WINDOW_DAYS);
        const [workspaceResult, topicsResult, usageResult] = await Promise.allSettled([
          getMornevenWorkspaceChanges(false, activeToken),
          getMornevenTelegramTopics(activeToken),
          getMornevenProviderUsage(from, now, activeToken),
        ]);
        if (workspaceResult.status === "fulfilled") setWorkspaceAudit(workspaceResult.value);
        if (topicsResult.status === "fulfilled") setTelegramTopics(topicsResult.value);
        if (usageResult.status === "fulfilled") setUsageEvents(usageResult.value.events ?? []);
      } catch (err) {
        setError(err instanceof Error ? err.message : String(err));
      } finally {
        if (!options.quiet) setLoading(false);
      }
    },
    [token],
  );

  useEffect(() => {
    void load();
  }, [load]);

  useEffect(() => {
    if (!token) return undefined;
    const timer = window.setInterval(() => {
      void load({ quiet: true });
    }, 10000);
    return () => window.clearInterval(timer);
  }, [load, token]);

  const materializedById = useMemo(() => {
    const map = new Map<string, MornevenMaterializedRuntime>();
    for (const runtime of status?.morneven?.runtimes ?? []) {
      if (runtime.identityId) map.set(runtime.identityId, runtime);
    }
    return map;
  }, [status]);

  const workspaceById = useMemo(() => {
    const map = new Map<string, MornevenWorkspaceRuntime>();
    for (const runtime of workspaceAudit?.runtimes ?? []) {
      if (runtime.identityId) map.set(runtime.identityId, runtime);
    }
    return map;
  }, [workspaceAudit]);

  const topicsById = useMemo(() => {
    const map = new Map<string, unknown[]>();
    for (const runtime of telegramTopics?.runtimes ?? []) {
      if (runtime.identityId) map.set(runtime.identityId, runtime.groups ?? []);
    }
    return map;
  }, [telegramTopics]);

  const runtimes: RuntimeView[] = useMemo(() => {
    return (status?.gateway?.runtimes ?? []).map((runtime) => ({
      ...runtime,
      materialized: materializedById.get(runtime.identityId),
      workspaceAudit: workspaceById.get(runtime.identityId),
      topicGroups: topicsById.get(runtime.identityId),
    }));
  }, [materializedById, status, topicsById, workspaceById]);

  const usageSummary = useMemo(() => {
    return usageEvents.reduce(
      (summary, event) => {
        summary.requests += event.requestCount ?? 1;
        summary.tokens += event.totalTokens ?? event.usage?.total_tokens ?? 0;
        summary.cost += usageCost(event);
        if (event.provider) {
          summary.providers.add(event.provider);
        }
        return summary;
      },
      { requests: 0, tokens: 0, cost: 0, providers: new Set<string>() },
    );
  }, [usageEvents]);

  const running = status?.gateway?.running ?? 0;
  const stopped = status?.gateway?.stopped ?? 0;
  const runtimeCount = status?.gateway?.runtimeCount ?? status?.morneven?.runtimeCount ?? 0;
  const fileCount = status?.morneven?.fileCount ?? runtimes.reduce((total, runtime) => total + (runtime.materialized?.fileCount ?? 0), 0);
  const legacyFileCount = runtimes.reduce((total, runtime) => total + (runtime.materialized?.legacyNanobotFileCount ?? 0), 0);
  const groupCount = runtimes.reduce((total, runtime) => total + (runtime.topicGroups?.length ?? 0), 0);
  const topicCount = runtimes.reduce((total, runtime) => total + countTopics(runtime.topicGroups), 0);

  const handleSaveToken = () => {
    const nextToken = tokenDraft.trim();
    saveMornevenToken(nextToken);
    setToken(nextToken);
    void load({ tokenOverride: nextToken });
  };

  const handleClearToken = () => {
    clearMornevenToken();
    setToken("");
    setTokenDraft("");
    setStatus(null);
    setWorkspaceAudit(null);
    setTelegramTopics(null);
    setUsageEvents([]);
  };

  const runAction = async (action: MornevenRuntimeAction, identityId?: string) => {
    const key = identityId ? `${identityId}:${action}` : `all:${action}`;
    setActionKey(key);
    setError(null);
    try {
      if (identityId) {
        await runMornevenRuntimeAction(identityId, action, token);
      } else {
        await runMornevenGatewayAction(action, token);
      }
      await load({ quiet: true });
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err));
    } finally {
      setActionKey(null);
    }
  };

  const syncBundle = async (restartGateway: boolean) => {
    setActionKey(restartGateway ? "sync-restart" : "sync");
    setError(null);
    try {
      await reloadMornevenBundle(restartGateway, token);
      await load({ quiet: true });
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err));
    } finally {
      setActionKey(null);
    }
  };

  const tokenSaved = token.length > 0;

  return (
    <div className="p-6 space-y-6 animate-fade-in">
      <div className="flex flex-col gap-4 xl:flex-row xl:items-end xl:justify-between">
        <div>
          <div className="flex items-center gap-3">
            <Bot className="h-5 w-5" style={{ color: "var(--pc-accent)" }} />
            <h2 className="text-sm font-semibold uppercase tracking-wider" style={{ color: "var(--pc-text-primary)" }}>
              Morneven Bot Manager
            </h2>
            <Badge tone={tokenSaved ? "success" : "warning"}>
              {tokenSaved ? "Token saved" : "Token required"}
            </Badge>
          </div>
          <p className="mt-2 text-sm" style={{ color: "var(--pc-text-muted)" }}>
            Multi-personality runtime control for Morneven compatibility.
          </p>
        </div>

        <div className="flex w-full flex-col gap-2 sm:flex-row xl:w-auto">
          <div className="relative flex-1 xl:w-96">
            <KeyRound className="pointer-events-none absolute left-3 top-1/2 h-4 w-4 -translate-y-1/2" style={{ color: "var(--pc-text-muted)" }} />
            <input
              type="password"
              value={tokenDraft}
              onChange={(event) => setTokenDraft(event.target.value)}
              placeholder="Morneven reload token"
              className="input-electric h-11 w-full pl-10 pr-3 text-sm"
              autoComplete="off"
            />
          </div>
          <button type="button" className="btn-secondary flex h-11 items-center justify-center gap-2" onClick={handleSaveToken}>
            <ShieldCheck className="h-4 w-4" />
            Save
          </button>
          <button type="button" className="btn-secondary flex h-11 items-center justify-center gap-2" onClick={handleClearToken}>
            Clear
          </button>
        </div>
      </div>

      {error && (
        <div
          className="flex items-start gap-3 rounded-xl border p-4 text-sm"
          style={{
            background: "var(--color-status-error-alpha-08)",
            borderColor: "var(--color-status-error-alpha-20)",
            color: "var(--color-status-error)",
          }}
        >
          <AlertTriangle className="mt-0.5 h-4 w-4 flex-shrink-0" />
          <span className="min-w-0 break-words">{error}</span>
        </div>
      )}

      <div className="flex flex-wrap items-center gap-2">
        <button
          type="button"
          className="btn-electric flex items-center gap-2 px-4 py-2 text-sm"
          disabled={!tokenSaved || loading || actionKey === "sync"}
          onClick={() => syncBundle(false)}
        >
          {actionKey === "sync" ? <Loader2 className="h-4 w-4 animate-spin" /> : <RefreshCw className="h-4 w-4" />}
          Sync bundle
        </button>
        <button
          type="button"
          className="btn-secondary flex items-center gap-2"
          disabled={!tokenSaved || loading || actionKey === "sync-restart"}
          onClick={() => syncBundle(true)}
        >
          {actionKey === "sync-restart" ? <Loader2 className="h-4 w-4 animate-spin" /> : <RotateCw className="h-4 w-4" />}
          Sync and restart
        </button>
        <button
          type="button"
          className="btn-secondary flex items-center gap-2"
          disabled={!tokenSaved || loading}
          onClick={() => load()}
        >
          {loading ? <Loader2 className="h-4 w-4 animate-spin" /> : <RefreshCw className="h-4 w-4" />}
          Refresh
        </button>
        <span className="text-xs" style={{ color: "var(--pc-text-muted)" }}>
          Last refresh: {formatDate(lastLoadedAt)}
        </span>
      </div>

      <div className="grid grid-cols-1 gap-4 md:grid-cols-2 xl:grid-cols-4">
        <MetricCard
          icon={<Activity className="h-4 w-4" />}
          label="Runtime state"
          value={`${running} running / ${stopped} stopped`}
          detail={`${runtimeCount} materialized runtime(s)`}
        />
        <MetricCard
          icon={<FileText className="h-4 w-4" />}
          label="Translated files"
          value={formatNumber(fileCount)}
          detail={`${formatNumber(legacyFileCount)} from Nanobot legacy`}
        />
        <MetricCard
          icon={<Database className="h-4 w-4" />}
          label="Provider usage"
          value={`${formatNumber(usageSummary.requests)} request(s)`}
          detail={`${formatNumber(usageSummary.tokens)} tokens, ${formatUsd(usageSummary.cost)}`}
        />
        <MetricCard
          icon={<FolderTree className="h-4 w-4" />}
          label="Telegram topics"
          value={`${formatNumber(groupCount)} group(s)`}
          detail={`${formatNumber(topicCount)} observed topic(s)`}
        />
      </div>

      <div className="flex flex-wrap gap-2">
        {(["start", "stop", "restart"] as MornevenRuntimeAction[]).map((action) => (
          <button
            key={action}
            type="button"
            className={action === "stop" ? "btn-danger flex items-center gap-2" : "btn-secondary flex items-center gap-2"}
            disabled={!tokenSaved || loading || actionKey === `all:${action}`}
            onClick={() => runAction(action)}
          >
            {actionIcon(action, actionKey === `all:${action}`)}
            {actionLabel(action)} all
          </button>
        ))}
      </div>

      <section>
        <div className="mb-3 flex items-center gap-2">
          <Activity className="h-4 w-4" style={{ color: "var(--pc-accent)" }} />
          <h3 className="text-xs font-semibold uppercase tracking-wider" style={{ color: "var(--pc-text-muted)" }}>
            Runtimes
          </h3>
        </div>
        {runtimes.length === 0 ? (
          <div className="surface-panel p-8 text-center text-sm" style={{ color: "var(--pc-text-muted)" }}>
            No Morneven runtimes loaded.
          </div>
        ) : (
          <div className="grid grid-cols-1 gap-4 xl:grid-cols-2">
            {runtimes.map((runtime) => {
              const isRunning = runtime.state === "running";
              const identityId = runtime.identityId;
              return (
                <article key={identityId || runtime.name} className="card p-5">
                  <div className="mb-4 flex flex-col gap-3 sm:flex-row sm:items-start sm:justify-between">
                    <div className="min-w-0">
                      <div className="flex flex-wrap items-center gap-2">
                        <h4 className="truncate text-lg font-semibold" style={{ color: "var(--pc-text-primary)" }}>
                          {runtime.name || runtime.slug || identityId}
                        </h4>
                        {runtime.isMain && <Badge tone="info">Main</Badge>}
                        <Badge tone={isRunning ? "success" : "muted"}>{runtime.state}</Badge>
                        {runtime.desiredState && <Badge tone={runtime.desiredState === runtime.state ? "success" : "warning"}>desired {runtime.desiredState}</Badge>}
                      </div>
                      <div className="mt-1 text-sm" style={{ color: "var(--pc-text-muted)" }}>
                        {providerLabel(runtime.provider ?? runtime.materialized?.provider)} provider, {runtime.enabledChannels?.join(", ") || "no channels"}
                      </div>
                    </div>
                    <div className="flex flex-wrap gap-2">
                      {(["start", "stop", "restart"] as MornevenRuntimeAction[]).map((action) => (
                        <button
                          key={action}
                          type="button"
                          className={action === "stop" ? "btn-danger flex items-center gap-2 px-3 py-2" : "btn-secondary flex items-center gap-2 px-3 py-2"}
                          disabled={!tokenSaved || !identityId || actionKey === `${identityId}:${action}` || (action === "start" && isRunning) || (action === "stop" && !isRunning)}
                          onClick={() => runAction(action, identityId)}
                        >
                          {actionIcon(action, actionKey === `${identityId}:${action}`)}
                          {actionLabel(action)}
                        </button>
                      ))}
                    </div>
                  </div>

                  <div className="grid grid-cols-2 gap-3 lg:grid-cols-4">
                    <RuntimeFact label="Uptime" value={formatDuration(runtime.uptime)} icon={<Clock className="h-3.5 w-3.5" />} />
                    <RuntimeFact label="PID" value={runtime.pid ? String(runtime.pid) : "-"} />
                    <RuntimeFact label="Port" value={runtime.gatewayPort ? String(runtime.gatewayPort) : "-"} />
                    <RuntimeFact label="Files" value={formatNumber(runtime.materialized?.fileCount ?? 0)} />
                  </div>

                  <div className="mt-4 grid grid-cols-1 gap-3 md:grid-cols-2">
                    <RuntimeLine label="Workspace" value={compactPath(runtime.workspacePath ?? runtime.materialized?.workspacePath)} />
                    <RuntimeLine label="Synced" value={formatDate(runtime.materialized?.syncedAt)} />
                    <RuntimeLine label="Changed files" value={formatNumber(runtime.workspaceAudit?.changedCount ?? 0)} />
                    <RuntimeLine label="Topics" value={`${formatNumber(runtime.topicGroups?.length ?? 0)} group(s), ${formatNumber(countTopics(runtime.topicGroups))} topic(s)`} />
                  </div>

                  {(runtime.lastError || runtime.lastLogLine) && (
                    <div className="mt-4 rounded-xl border p-3 text-xs" style={{ borderColor: "var(--pc-border)", color: "var(--pc-text-muted)", background: "var(--pc-bg-input)" }}>
                      <span className="font-semibold" style={{ color: runtime.lastError ? "var(--color-status-error)" : "var(--pc-text-secondary)" }}>
                        {runtime.lastError ? "Last error" : "Last log"}:
                      </span>{" "}
                      {runtime.lastError ?? runtime.lastLogLine}
                    </div>
                  )}
                </article>
              );
            })}
          </div>
        )}
      </section>

      <section className="grid grid-cols-1 gap-4 xl:grid-cols-2">
        <AuditPanel
          title="Workspace audit"
          rows={(workspaceAudit?.runtimes ?? []).map((runtime) => ({
            key: runtime.identityId,
            name: runtime.identity?.name ?? runtime.identityId,
            value: `${formatNumber(runtime.changedCount ?? 0)} changed, ${formatNumber(runtime.skipped?.length ?? 0)} skipped`,
            detail: `Synced ${formatDate(runtime.syncedAt)}`,
          }))}
          empty="No workspace audit loaded."
        />
        <AuditPanel
          title="Provider usage"
          rows={Array.from(
            usageEvents.reduce((map, event) => {
              const key = `${event.provider ?? "unknown"}:${event.runtimeName ?? event.identityId ?? "runtime"}`;
              const current = map.get(key) ?? {
                key,
                name: `${event.provider ?? "unknown"} / ${event.runtimeName ?? event.identityId ?? "runtime"}`,
                requests: 0,
                tokens: 0,
                cost: 0,
              };
              current.requests += event.requestCount ?? 1;
              current.tokens += event.totalTokens ?? event.usage?.total_tokens ?? 0;
              current.cost += usageCost(event);
              map.set(key, current);
              return map;
            }, new Map<string, { key: string; name: string; requests: number; tokens: number; cost: number }>()),
          ).map(([, row]) => ({
            key: row.key,
            name: row.name,
            value: `${formatNumber(row.requests)} request(s), ${formatNumber(row.tokens)} tokens`,
            detail: formatUsd(row.cost),
          }))}
          empty="No provider usage events in the last 7 days."
        />
      </section>

      <section>
        <div className="mb-3 flex items-center gap-2">
          <CheckCircle2 className="h-4 w-4" style={{ color: "var(--pc-accent)" }} />
          <h3 className="text-xs font-semibold uppercase tracking-wider" style={{ color: "var(--pc-text-muted)" }}>
            Recent Morneven logs
          </h3>
        </div>
        <div className="surface-panel overflow-hidden">
          {(status?.gateway?.logs ?? status?.logs ?? []).length === 0 ? (
            <div className="p-6 text-sm" style={{ color: "var(--pc-text-muted)" }}>
              No logs loaded.
            </div>
          ) : (
            <div className="max-h-80 overflow-auto p-4 font-mono text-xs leading-6" style={{ color: "var(--pc-text-secondary)" }}>
              {(status?.gateway?.logs ?? status?.logs ?? []).slice().reverse().map((line, index) => (
                <div key={`${index}-${line}`} className="whitespace-pre-wrap break-words">
                  {line}
                </div>
              ))}
            </div>
          )}
        </div>
      </section>
    </div>
  );
}

function RuntimeFact({ label, value, icon }: { label: string; value: string; icon?: React.ReactNode }) {
  return (
    <div className="rounded-xl border p-3" style={{ borderColor: "var(--pc-border)", background: "var(--pc-bg-input)" }}>
      <div className="mb-1 flex items-center gap-1.5 text-[11px] font-semibold uppercase tracking-wider" style={{ color: "var(--pc-text-muted)" }}>
        {icon}
        {label}
      </div>
      <div className="truncate text-sm font-semibold" style={{ color: "var(--pc-text-primary)" }}>
        {value}
      </div>
    </div>
  );
}

function RuntimeLine({ label, value }: { label: string; value: string }) {
  return (
    <div className="min-w-0">
      <div className="text-[11px] font-semibold uppercase tracking-wider" style={{ color: "var(--pc-text-muted)" }}>
        {label}
      </div>
      <div className="mt-1 truncate text-sm" style={{ color: "var(--pc-text-secondary)" }} title={value}>
        {value}
      </div>
    </div>
  );
}

function AuditPanel({
  title,
  rows,
  empty,
}: {
  title: string;
  rows: Array<{ key: string; name: string; value: string; detail?: string }>;
  empty: string;
}) {
  return (
    <div className="surface-panel p-5">
      <div className="mb-4 flex items-center gap-2">
        <FileText className="h-4 w-4" style={{ color: "var(--pc-accent)" }} />
        <h3 className="text-xs font-semibold uppercase tracking-wider" style={{ color: "var(--pc-text-muted)" }}>
          {title}
        </h3>
      </div>
      {rows.length === 0 ? (
        <div className="text-sm" style={{ color: "var(--pc-text-muted)" }}>
          {empty}
        </div>
      ) : (
        <div className="space-y-3">
          {rows.map((row) => (
            <div key={row.key} className="flex flex-col gap-1 rounded-xl border p-3 sm:flex-row sm:items-center sm:justify-between" style={{ borderColor: "var(--pc-border)", background: "var(--pc-bg-input)" }}>
              <div className="min-w-0">
                <div className="truncate text-sm font-semibold" style={{ color: "var(--pc-text-primary)" }}>
                  {row.name}
                </div>
                {row.detail && (
                  <div className="truncate text-xs" style={{ color: "var(--pc-text-muted)" }}>
                    {row.detail}
                  </div>
                )}
              </div>
              <div className="text-sm font-medium" style={{ color: "var(--pc-text-secondary)" }}>
                {row.value}
              </div>
            </div>
          ))}
        </div>
      )}
    </div>
  );
}
