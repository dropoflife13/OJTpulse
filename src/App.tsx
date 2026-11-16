import { useEffect, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { getCurrentWindow, LogicalSize } from "@tauri-apps/api/window";
import {
  Volume2,
  VolumeX,
  ChevronUp,
  ChevronDown,
  X,
  ExternalLink,
  Check,
  Ban,
  Terminal,
  Pin,
  Move,
  Cpu,
  Trash2,
  Zap,
  RefreshCw,
  Clock,
  Play,
  Square,
  Plus,
  Copy,
  FileText,
  Settings,
  History,
  ListChecks,
  Save,
  Pause,
  ShieldCheck,
  Wifi,
  GitCommitHorizontal,
  BookOpen,
} from "lucide-react";
import "./App.css";

type AgentStatus = "running" | "waiting_permission" | "paused" | "queued" | "done" | string;
type WindowMode = "top-right" | "floating";
type ActiveTab = "agents" | "reaper" | "ojt";
type AgentFilter = "active" | "paused" | "done" | "all";
type OjtSubTab = "today" | "history" | "settings";

interface GitCommitEvidence {
  hash: string;
  timestamp: string;
  repo: string;
  message: string;
  files_changed: number;
  insertions: number;
  deletions: number;
}

interface ShiftSession {
  start_ts: number;
  date_str: string;
  time_in_str: string;
  notes: string[];
  active_minutes: number;
  idle_minutes: number;
  network_stamp?: string | null;
  app_minutes: Record<string, number>;
  app_durations?: Record<string, number>;
  commit_evidence: GitCommitEvidence[];
  challenges: string;
  learnings: string;
  is_paused?: boolean;
  pause_started_ts?: number | null;
  total_paused_seconds?: number;
  break_logs?: string[];
}

interface CompletedShift {
  id: string;
  date_str: string;
  time_in: string;
  time_out: string;
  duration_hours: number;
  activities: string[];
  summary_markdown: string;
  active_minutes: number;
  idle_minutes: number;
  focus_percent: number;
  network_stamp_in?: string | null;
  network_stamp_out?: string | null;
  app_breakdown: Record<string, number>;
  app_durations?: Record<string, number>;
  commit_evidence: GitCommitEvidence[];
  challenges: string;
  learnings: string;
  verification_hash: string;
  total_paused_seconds?: number;
  break_logs?: string[];
}

function formatMinutesCompact(mins: number): string {
  if (mins >= 60) {
    const h = Math.floor(mins / 60);
    const m = mins % 60;
    return m > 0 ? `${h}h ${m}m` : `${h}h`;
  }
  return `${mins}m`;
}

interface OjtDashboardResponse {
  target_hours: number;
  rendered_hours: number;
  remaining_hours: number;
  progress_percent: number;
  shifts_remaining: number;
  estimated_completion_date: string;
  watched_repos: string[];
  obsidian_export_dir?: string | null;
  active_shift?: ShiftSession | null;
  completed_shifts: CompletedShift[];
  today_activities: string[];
  today_commits: GitCommitEvidence[];
  today_challenges: string;
  today_learnings: string;
  current_focus_percent: number;
  top_active_apps: [string, number][];
  current_network_stamp: string;
}

function formatElapsedHms(totalSecs: number): string {
  const s = Math.max(0, Math.floor(totalSecs));
  const hrs = Math.floor(s / 3600);
  const mins = Math.floor((s % 3600) / 60);
  const secs = s % 60;
  return `${String(hrs).padStart(2, "0")}:${String(mins).padStart(2, "0")}:${String(secs).padStart(2, "0")}`;
}

interface AgentSession {
  id: string;
  name: string;
  task: string;
  status: AgentStatus;
  current_step: number;
  total_steps: number;
  step_label: string;
  question?: string | null;
  workspace_path?: string | null;
  decision?: boolean | null;
  updated_at: number;
}

interface HeavyProcess {
  pid: number;
  name: string;
  memory_mb: number;
}

interface SystemHealth {
  used_gb: number;
  total_gb: number;
  usage_percent: number;
  processes: HeavyProcess[];
}

interface SweepResult {
  killed_count: number;
  freed_mb: number;
}

let sharedAudioCtx: AudioContext | null = null;

function playRetroChime(kind: "waiting_permission" | "done") {
  try {
    const AudioContextClass =
      window.AudioContext ||
      (window as unknown as { webkitAudioContext: typeof AudioContext })
        .webkitAudioContext;
    if (!AudioContextClass) return;

    if (!sharedAudioCtx) {
      sharedAudioCtx = new AudioContextClass();
    }
    const ctx = sharedAudioCtx;
    if (ctx.state === "suspended") {
      ctx.resume();
    }

    const now = ctx.currentTime;
    const notes =
      kind === "waiting_permission"
        ? [
            // 2-note 8-bit attention blip (E5 -> A5)
            { freq: 659.25, start: 0, duration: 0.08 },
            { freq: 880.0, start: 0.09, duration: 0.14 },
          ]
        : [
            // 3-note 8-bit victory arpeggio (C5 -> E5 -> G5)
            { freq: 523.25, start: 0, duration: 0.07 },
            { freq: 659.25, start: 0.08, duration: 0.07 },
            { freq: 783.99, start: 0.16, duration: 0.16 },
          ];

    for (const note of notes) {
      const osc = ctx.createOscillator();
      const gain = ctx.createGain();
      osc.type = "square";
      osc.frequency.setValueAtTime(note.freq, now + note.start);

      gain.gain.setValueAtTime(0.06, now + note.start);
      gain.gain.exponentialRampToValueAtTime(
        0.001,
        now + note.start + note.duration
      );

      osc.connect(gain);
      gain.connect(ctx.destination);

      osc.start(now + note.start);
      osc.stop(now + note.start + note.duration);
    }
  } catch (err) {
    console.warn("Audio chime blocked or unavailable:", err);
  }
}

function formatWorkspaceName(path?: string | null): string | null {
  if (!path) return null;
  const normalized = path.replace(/\\/g, "/").replace(/\/+$/, "");
  const parts = normalized.split("/");
  return parts[parts.length - 1] || normalized;
}

function PixelDinoAvatar({
  status,
  compact = false,
}: {
  status: AgentStatus;
  compact?: boolean;
}) {
  const boxSize = compact ? "w-6 h-6 rounded-lg" : "w-9 h-9 rounded-xl";
  const svgSize = compact ? "w-4 h-4" : "w-6 h-6";

  if (status === "running") {
    // Animated Emerald/Blue Brachiosaurus (#38bdf8 / #34d399)
    return (
      <div
        aria-hidden="true"
        className={`dino-avatar ui-chrome relative flex items-center justify-center ${boxSize} bg-emerald-500/10 border border-emerald-500/30 shrink-0`}
      >
        <svg
          viewBox="0 0 16 16"
          className={svgSize}
          shapeRendering="crispEdges"
        >
          <g className="dino-march">
            <rect x="9" y="2" width="4" height="2" fill="#34d399" />
            <rect
              x="11"
              y="3"
              width="1"
              height="1"
              fill="#09090b"
              className="dino-eye-blink"
            />
            <rect x="9" y="4" width="2" height="4" fill="#34d399" />
            <rect x="4" y="8" width="7" height="4" fill="#34d399" />
            <rect x="5" y="7" width="4" height="1" fill="#38bdf8" />
            <rect x="6" y="9" width="3" height="2" fill="#38bdf8" />
            <g className="dino-tail-wag">
              <rect x="2" y="9" width="2" height="2" fill="#34d399" />
              <rect x="1" y="10" width="1" height="1" fill="#38bdf8" />
            </g>
          </g>
          <rect
            x="4"
            y="12"
            width="2"
            height="2"
            fill="#34d399"
            className="dino-leg-back"
          />
          <rect
            x="9"
            y="12"
            width="2"
            height="2"
            fill="#34d399"
            className="dino-leg-front"
          />
          <rect
            x="2"
            y="13"
            width="1"
            height="1"
            fill="#6ee7b7"
            className="dino-foot-dust"
          />
          <rect
            x="1"
            y="12"
            width="1"
            height="1"
            fill="#38bdf8"
            className="dino-foot-dust"
            style={{ animationDelay: "0.25s" }}
          />
        </svg>
      </div>
    );
  }

  if (status === "waiting_permission") {
    // Animated Amber/Red T-Rex (#f43f5e / #fb923c) with chomp + alert hop
    return (
      <div
        aria-hidden="true"
        className={`dino-avatar ui-chrome relative flex items-center justify-center ${boxSize} bg-rose-500/15 border border-rose-500/80 shrink-0`}
      >
        <svg
          viewBox="0 0 16 16"
          className={svgSize}
          shapeRendering="crispEdges"
        >
          <g className="dino-alert-bob">
            <rect x="2" y="2" width="1" height="3" fill="#fde047" />
            <rect x="2" y="6" width="1" height="1" fill="#fde047" />
          </g>
          <g className="dino-trex-hop">
            <rect x="7" y="2" width="6" height="3" fill="#f43f5e" />
            <rect
              x="10"
              y="3"
              width="1"
              height="1"
              fill="#09090b"
              className="dino-eye-blink"
            />
            <rect x="10" y="5" width="1" height="1" fill="#fecdd3" />
            <rect x="12" y="5" width="1" height="1" fill="#fecdd3" />
            <rect
              x="9"
              y="6"
              width="4"
              height="1"
              fill="#fb923c"
              className="dino-jaw-chomp"
            />
            <rect x="5" y="6" width="5" height="5" fill="#f43f5e" />
            <rect
              x="10"
              y="8"
              width="2"
              height="1"
              fill="#fb923c"
              className="dino-arm-wave"
            />
            <rect x="6" y="7" width="3" height="3" fill="#fb923c" />
            <rect
              x="2"
              y="8"
              width="3"
              height="2"
              fill="#f43f5e"
              className="dino-tail-wag"
            />
            <rect
              x="5"
              y="11"
              width="2"
              height="3"
              fill="#f43f5e"
              className="dino-leg-back"
            />
            <rect
              x="8"
              y="11"
              width="2"
              height="3"
              fill="#f43f5e"
              className="dino-leg-front"
            />
          </g>
        </svg>
      </div>
    );
  }

  if (status === "paused") {
    // Sleeping Amber/Slate Dino (#fbbf24 / #94a3b8) with calm "Zz" breath
    return (
      <div
        aria-hidden="true"
        className={`dino-avatar ui-chrome relative flex items-center justify-center ${boxSize} bg-amber-500/10 border border-amber-500/35 shrink-0`}
      >
        <svg viewBox="0 0 16 16" className={svgSize} shapeRendering="crispEdges">
          <g className="dino-sparkle">
            <rect x="12" y="2" width="2" height="1" fill="#fde68a" />
            <rect x="12" y="3" width="1" height="1" fill="#fde68a" />
            <rect x="12" y="4" width="2" height="1" fill="#fde68a" />
          </g>
          <g className="dino-tri-breathe">
            <rect x="7" y="6" width="5" height="3" fill="#f59e0b" />
            <rect x="9" y="7" width="2" height="1" fill="#451a03" />
            <rect x="3" y="9" width="8" height="4" fill="#f59e0b" />
            <rect x="4" y="10" width="5" height="2" fill="#fbbf24" />
            <rect x="1" y="11" width="2" height="2" fill="#d97706" />
            <rect x="4" y="13" width="2" height="1" fill="#b45309" />
            <rect x="8" y="13" width="2" height="1" fill="#b45309" />
          </g>
        </svg>
      </div>
    );
  }

  if (status === "queued") {
    // Animated Wobbling Yellow Dino Egg (#facc15)
    return (
      <div
        aria-hidden="true"
        className={`dino-avatar ui-chrome relative flex items-center justify-center ${boxSize} bg-yellow-400/10 border border-yellow-400/30 shrink-0`}
      >
        <svg
          viewBox="0 0 16 16"
          className={svgSize}
          shapeRendering="crispEdges"
        >
          <g className="dino-egg-wobble">
            <rect x="6" y="3" width="4" height="2" fill="#facc15" />
            <rect x="5" y="5" width="6" height="3" fill="#facc15" />
            <rect x="4" y="8" width="8" height="4" fill="#facc15" />
            <rect x="5" y="12" width="6" height="2" fill="#facc15" />
            <rect x="6" y="5" width="2" height="2" fill="#ca8a04" />
            <rect x="9" y="9" width="2" height="2" fill="#ca8a04" />
            <rect x="5" y="10" width="1" height="1" fill="#ca8a04" />
            <g className="dino-egg-crack">
              <rect x="5" y="7" width="1" height="1" fill="#fef08a" />
              <rect x="6" y="8" width="2" height="1" fill="#fef08a" />
              <rect x="8" y="7" width="2" height="1" fill="#fef08a" />
              <rect x="10" y="8" width="1" height="1" fill="#fef08a" />
            </g>
          </g>
        </svg>
      </div>
    );
  }

  // done: Content Purple Triceratops (#a855f7) with victory sparkles
  return (
    <div
      aria-hidden="true"
      className={`dino-avatar ui-chrome relative flex items-center justify-center ${boxSize} bg-purple-500/10 border border-purple-500/30 shrink-0`}
    >
      <svg viewBox="0 0 16 16" className={svgSize} shapeRendering="crispEdges">
        <g className="dino-sparkle">
          <rect x="2" y="3" width="1" height="1" fill="#e9d5ff" />
          <rect x="14" y="2" width="1" height="1" fill="#f0abfc" />
        </g>
        <g className="dino-tri-breathe">
          <rect x="8" y="3" width="3" height="5" fill="#a855f7" />
          <rect x="13" y="4" width="2" height="1" fill="#e9d5ff" />
          <rect x="14" y="6" width="1" height="1" fill="#e9d5ff" />
          <rect x="10" y="5" width="4" height="4" fill="#a855f7" />
          <rect
            x="11"
            y="6"
            width="1"
            height="1"
            fill="#09090b"
            className="dino-eye-blink"
          />
          <rect x="4" y="6" width="6" height="5" fill="#a855f7" />
          <rect x="5" y="7" width="4" height="3" fill="#c084fc" />
          <rect
            x="2"
            y="8"
            width="2"
            height="2"
            fill="#a855f7"
            className="dino-tail-wag"
          />
          <rect x="4" y="11" width="2" height="3" fill="#a855f7" />
          <rect x="8" y="11" width="2" height="3" fill="#a855f7" />
        </g>
      </svg>
    </div>
  );
}

function StatusBadge({ status }: { status: AgentStatus }) {
  switch (status) {
    case "waiting_permission":
      return (
        <span className="ui-chrome px-1.5 py-0.5 text-[10px] font-semibold uppercase tracking-wider rounded bg-rose-500/20 text-rose-300 border border-rose-500/40">
          Needs Approval
        </span>
      );
    case "running":
      return (
        <span className="ui-chrome px-1.5 py-0.5 text-[10px] font-medium uppercase tracking-wider rounded bg-emerald-500/15 text-emerald-300 border border-emerald-500/30">
          Running
        </span>
      );
    case "paused":
      return (
        <span className="ui-chrome px-1.5 py-0.5 text-[10px] font-medium uppercase tracking-wider rounded bg-amber-500/15 text-amber-300 border border-amber-500/30">
          Paused
        </span>
      );
    case "queued":
      return (
        <span className="ui-chrome px-1.5 py-0.5 text-[10px] font-medium uppercase tracking-wider rounded bg-yellow-500/15 text-yellow-300 border border-yellow-500/30">
          Queued
        </span>
      );
    default:
      return (
        <span className="ui-chrome px-1.5 py-0.5 text-[10px] font-medium uppercase tracking-wider rounded bg-purple-500/15 text-purple-300 border border-purple-500/30">
          Done
        </span>
      );
  }
}

export default function App() {
  const [agents, setAgents] = useState<AgentSession[]>([]);
  const [muted, setMuted] = useState(false);
  const [collapsed, setCollapsed] = useState(false);
  const [activeTab, setActiveTab] = useState<ActiveTab>("agents");
  const [agentFilter, setAgentFilter] = useState<AgentFilter>("active");
  const [windowMode, setWindowMode] = useState<WindowMode>(() => {
    const saved = localStorage.getItem("dinobuds_window_mode");
    return saved === "floating" ? "floating" : "top-right";
  });

  const [health, setHealth] = useState<SystemHealth | null>(null);
  const [sweepBanner, setSweepBanner] = useState<string | null>(null);
  const [isRefreshingHealth, setIsRefreshingHealth] = useState(false);

  const [ojt, setOjt] = useState<OjtDashboardResponse | null>(null);
  const [ojtSubTab, setOjtSubTab] = useState<OjtSubTab>("today");
  const [manualNote, setManualNote] = useState("");
  const [challengesInput, setChallengesInput] = useState("");
  const [learningsInput, setLearningsInput] = useState("");
  const [isSavingReflections, setIsSavingReflections] = useState(false);
  const [ojtBanner, setOjtBanner] = useState<string | null>(null);
  const [isSyncingOjt, setIsSyncingOjt] = useState(false);
  const [shiftElapsedSecs, setShiftElapsedSecs] = useState(0);
  const [breakElapsedSecs, setBreakElapsedSecs] = useState(0);
  const manualNoteInputRef = useRef<HTMLInputElement | null>(null);
  const [targetHoursInput, setTargetHoursInput] = useState("486");
  const [reposInput, setReposInput] = useState("");
  const [exportDirInput, setExportDirInput] = useState("");

  const mutedRef = useRef(muted);
  const prevStatusesRef = useRef<Map<string, AgentStatus>>(new Map());
  const dragStartPosRef = useRef<{ x: number; y: number } | null>(null);

  useEffect(() => {
    mutedRef.current = muted;
  }, [muted]);

  const applyOjtResponse = (res: OjtDashboardResponse, syncInputs = false) => {
    setOjt(res);
    if (syncInputs) {
      setTargetHoursInput(String(res.target_hours));
      setReposInput(res.watched_repos.join("\n"));
      setExportDirInput(res.obsidian_export_dir ?? "");
      setChallengesInput(res.today_challenges ?? "");
      setLearningsInput(res.today_learnings ?? "");
    }
  };

  const fetchOjtState = async (syncInputs = false) => {
    try {
      const res = await invoke<OjtDashboardResponse>("get_ojt_state");
      applyOjtResponse(res, syncInputs);
    } catch (err) {
      console.error("Failed to fetch OJT state:", err);
    }
  };

  useEffect(() => {
    const active = ojt?.active_shift;
    if (!active) {
      setShiftElapsedSecs(0);
      setBreakElapsedSecs(0);
      return;
    }
    const updateTick = () => {
      const nowSec = Math.floor(Date.now() / 1000);
      const totalPaused = active.total_paused_seconds ?? 0;
      if (active.is_paused) {
        const pauseStart = active.pause_started_ts ?? nowSec;
        const netAtPause = Math.max(0, pauseStart - active.start_ts - totalPaused);
        setShiftElapsedSecs(netAtPause);
        setBreakElapsedSecs(Math.max(0, nowSec - pauseStart));
      } else {
        setShiftElapsedSecs(Math.max(0, nowSec - active.start_ts - totalPaused));
        setBreakElapsedSecs(0);
      }
    };
    updateTick();
    const timer = setInterval(updateTick, 1000);
    return () => clearInterval(timer);
  }, [
    ojt?.active_shift?.start_ts,
    ojt?.active_shift?.is_paused,
    ojt?.active_shift?.pause_started_ts,
    ojt?.active_shift?.total_paused_seconds,
  ]);

  const fetchHealth = async () => {
    setIsRefreshingHealth(true);
    try {
      const data = await invoke<SystemHealth>("get_system_health");
      setHealth(data);
    } catch (err) {
      console.error("Failed to fetch system health:", err);
    } finally {
      setIsRefreshingHealth(false);
    }
  };

  useEffect(() => {
    invoke<AgentSession[]>("get_sessions")
      .then((initial) => {
        const statusMap = new Map<string, AgentStatus>();
        for (const s of initial) {
          statusMap.set(s.id, s.status);
        }
        prevStatusesRef.current = statusMap;
        setAgents(initial);
      })
      .catch((err) => console.error("Failed to fetch initial sessions:", err));

    fetchHealth();
    fetchOjtState(true);
    const healthInterval = setInterval(fetchHealth, 4000);
    const ojtInterval = setInterval(() => fetchOjtState(false), 30000);

    const unlistenPromise = listen<AgentSession[]>(
      "dinobuds-update",
      (event) => {
        const nextSessions = event.payload;
        const prevMap = prevStatusesRef.current;

        if (!mutedRef.current) {
          let shouldPlayPermission = false;
          let shouldPlayDone = false;

          for (const session of nextSessions) {
            const prevStatus = prevMap.get(session.id);
            if (
              session.status === "waiting_permission" &&
              prevStatus !== "waiting_permission"
            ) {
              shouldPlayPermission = true;
            } else if (
              session.status === "done" &&
              prevStatus !== undefined &&
              prevStatus !== "done"
            ) {
              shouldPlayDone = true;
            }
          }

          if (shouldPlayPermission) {
            playRetroChime("waiting_permission");
          } else if (shouldPlayDone) {
            playRetroChime("done");
          }
        }

        const nextMap = new Map<string, AgentStatus>();
        for (const s of nextSessions) {
          nextMap.set(s.id, s.status);
        }
        prevStatusesRef.current = nextMap;
        setAgents(nextSessions);
      }
    );

    return () => {
      clearInterval(healthInterval);
      clearInterval(ojtInterval);
      unlistenPromise.then((unlisten) => unlisten());
    };
  }, []);

  const handleCloseWindow = async () => {
    try {
      await getCurrentWindow().close();
    } catch (err) {
      console.error("Failed to close window:", err);
    }
  };

  const handleToggleWindowMode = async () => {
    const nextMode: WindowMode =
      windowMode === "top-right" ? "floating" : "top-right";
    setWindowMode(nextMode);
    localStorage.setItem("dinobuds_window_mode", nextMode);

    if (nextMode === "top-right") {
      try {
        await invoke("snap_to_top_right", {
          width: collapsed ? 220 : 340,
        });
      } catch (err) {
        console.error("Failed to snap to top-right:", err);
      }
    }
  };

  const handleToggleCollapse = async () => {
    const nextCollapsed = !collapsed;
    setCollapsed(nextCollapsed);
    const nextWidth = nextCollapsed ? 220 : 340;
    const nextHeight = nextCollapsed ? 48 : 460;

    try {
      await getCurrentWindow().setSize(new LogicalSize(nextWidth, nextHeight));
      if (windowMode === "top-right") {
        await invoke("snap_to_top_right", { width: nextWidth });
      }
    } catch (err) {
      console.error("Failed to resize window:", err);
    }
  };

  const handleHeaderMouseDown = async () => {
    try {
      const pos = await getCurrentWindow().outerPosition();
      dragStartPosRef.current = { x: pos.x, y: pos.y };
    } catch {
      dragStartPosRef.current = null;
    }
  };

  const handleHeaderMouseUp = async () => {
    if (!dragStartPosRef.current || windowMode !== "top-right") return;
    try {
      const pos = await getCurrentWindow().outerPosition();
      const dx = Math.abs(pos.x - dragStartPosRef.current.x);
      const dy = Math.abs(pos.y - dragStartPosRef.current.y);
      if (dx > 12 || dy > 12) {
        setWindowMode("floating");
        localStorage.setItem("dinobuds_window_mode", "floating");
      }
    } catch {
      // ignore
    } finally {
      dragStartPosRef.current = null;
    }
  };

  const handleSubmitPrompt = async (
    id: string,
    option: number | null,
    approved: boolean
  ) => {
    try {
      const updated = await invoke<AgentSession[]>("submit_agent_prompt", {
        id,
        option,
        approved,
      });
      setAgents(updated);
    } catch (err) {
      console.error("Failed to submit agent prompt:", err);
    }
  };

  const handleDismiss = async (id: string) => {
    try {
      const updated = await invoke<AgentSession[]>("dismiss_session", { id });
      setAgents(updated);
    } catch (err) {
      console.error("Failed to dismiss session:", err);
    }
  };

  const handleFocusWorkspace = async (path?: string | null) => {
    if (!path) return;
    try {
      await invoke("focus_workspace", { path });
    } catch (err) {
      console.error("Failed to focus workspace:", err);
    }
  };

  const handleKillProcess = async (pid: number, name: string) => {
    try {
      const freedMb = await invoke<number>("kill_process", { pid });
      setSweepBanner(`Killed ${name} (${pid}) — freed ${freedMb} MB`);
      setTimeout(() => setSweepBanner(null), 3500);
      fetchHealth();
    } catch (err) {
      setSweepBanner(`Could not kill PID ${pid}: ${String(err)}`);
      setTimeout(() => setSweepBanner(null), 3500);
    }
  };

  const handleSweepNode = async () => {
    try {
      const res = await invoke<SweepResult>("sweep_orphan_node");
      if (res.killed_count > 0) {
        setSweepBanner(
          `Swept ${res.killed_count} process(es) — freed ${res.freed_mb} MB!`
        );
      } else {
        setSweepBanner("No heavy orphan Node processes (>=120 MB) found.");
      }
      setTimeout(() => setSweepBanner(null), 4000);
      fetchHealth();
    } catch (err) {
      console.error("Sweep failed:", err);
    }
  };

  const showOjtBanner = (msg: string) => {
    setOjtBanner(msg);
    setTimeout(() => setOjtBanner(null), 3500);
  };

  const handleToggleShift = async () => {
    try {
      const wasActive = Boolean(ojt?.active_shift);
      const res = await invoke<OjtDashboardResponse>("toggle_shift");
      applyOjtResponse(res);
      showOjtBanner(wasActive ? "Shift saved to DTR History!" : "Time In recorded — shift started!");
    } catch (err) {
      showOjtBanner(`Shift update failed: ${String(err)}`);
    }
  };

  const handleToggleShiftBreak = async () => {
    try {
      const wasPaused = Boolean(ojt?.active_shift?.is_paused);
      const res = await invoke<OjtDashboardResponse>("toggle_shift_break");
      applyOjtResponse(res);
      showOjtBanner(
        wasPaused ? "Back on duty — break deducted from DTR." : "On Lunch / Break — timer & heartbeat paused."
      );
    } catch (err) {
      showOjtBanner(`Break toggle failed: ${String(err)}`);
    }
  };

  const handleSelectCategoryPill = (tag: string) => {
    setManualNote((prev) => {
      const cleaned = prev.replace(/^\[[^\]]+\]\s*/, "").trimStart();
      return `[${tag}] ${cleaned}`;
    });
    setTimeout(() => manualNoteInputRef.current?.focus(), 0);
  };

  const handleScanActivity = async () => {
    setIsSyncingOjt(true);
    try {
      const res = await invoke<OjtDashboardResponse>("scan_today_activity");
      applyOjtResponse(res);
      showOjtBanner(`Synced ${res.today_activities.length} activity item(s) for today.`);
    } catch (err) {
      showOjtBanner(`Sync failed: ${String(err)}`);
    } finally {
      setIsSyncingOjt(false);
    }
  };

  const handleAddManualActivity = async (e: React.FormEvent) => {
    e.preventDefault();
    const trimmed = manualNote.trim();
    if (!trimmed) return;
    try {
      const res = await invoke<OjtDashboardResponse>("add_manual_activity", {
        bullet: trimmed,
      });
      applyOjtResponse(res);
      setManualNote("");
      showOjtBanner("Activity bullet added.");
    } catch (err) {
      showOjtBanner(`Failed to add activity: ${String(err)}`);
    }
  };

  const handleSaveReflections = async (e: React.FormEvent) => {
    e.preventDefault();
    setIsSavingReflections(true);
    try {
      const res = await invoke<OjtDashboardResponse>("update_today_reflections", {
        challenges: challengesInput,
        learnings: learningsInput,
      });
      applyOjtResponse(res, true);
      showOjtBanner("Saved daily challenges & key learnings!");
    } catch (err) {
      showOjtBanner(`Failed to save reflections: ${String(err)}`);
    } finally {
      setIsSavingReflections(false);
    }
  };

  const handleExportMarkdown = async (period: "daily" | "weekly" | "all") => {
    try {
      const md = await invoke<string>("export_markdown_report", { period });
      if (navigator.clipboard && navigator.clipboard.writeText) {
        await navigator.clipboard.writeText(md);
      }
      const label =
        period === "daily"
          ? "Daily Record"
          : period === "weekly"
            ? "Weekly Journal"
            : "Complete DTR";
      showOjtBanner(
        ojt?.obsidian_export_dir
          ? `Saved ${label} (.md) to Obsidian folder & copied!`
          : `Copied formal ${label} (.md) to clipboard!`
      );
    } catch (err) {
      showOjtBanner(`Export failed: ${String(err)}`);
    }
  };

  const handleCopyShiftSummary = async (summaryMarkdown: string) => {
    try {
      if (navigator.clipboard && navigator.clipboard.writeText) {
        await navigator.clipboard.writeText(summaryMarkdown);
      }
      showOjtBanner("Copied shift Markdown to clipboard!");
    } catch (err) {
      showOjtBanner(`Clipboard copy failed: ${String(err)}`);
    }
  };

  const handleSaveOjtSettings = async (e: React.FormEvent) => {
    e.preventDefault();
    const parsedTarget = Number.parseFloat(targetHoursInput);
    const targetHours = Number.isFinite(parsedTarget) && parsedTarget > 0 ? parsedTarget : 486;
    const watchedRepos = reposInput
      .split(/\r?\n/)
      .map((s) => s.trim())
      .filter(Boolean);
    const exportDirTrimmed = exportDirInput.trim();
    try {
      const res = await invoke<OjtDashboardResponse>("update_ojt_settings", {
        targetHours,
        watchedRepos,
        obsidianExportDir: exportDirTrimmed.length > 0 ? exportDirTrimmed : null,
      });
      applyOjtResponse(res, true);
      showOjtBanner("OJT settings saved!");
    } catch (err) {
      showOjtBanner(`Failed to save settings: ${String(err)}`);
    }
  };

  const handleTogglePause = async (id: string) => {
    try {
      const updated = await invoke<AgentSession[]>("toggle_pause_session", { id });
      setAgents(updated);
    } catch (err) {
      console.error("Failed to toggle pause:", err);
    }
  };

  const statusRank = (status: AgentStatus) => {
    if (status === "waiting_permission") return 0;
    if (status === "running") return 1;
    if (status === "paused") return 2;
    if (status === "queued") return 3;
    return 4;
  };

  const sortSessionsByPriority = (list: AgentSession[]) =>
    [...list].sort((a, b) => {
      const rankDiff = statusRank(a.status) - statusRank(b.status);
      if (rankDiff !== 0) return rankDiff;
      return b.updated_at - a.updated_at;
    });

  const activeAgents = sortSessionsByPriority(
    agents.filter(
      (a) =>
        a.status === "waiting_permission" ||
        a.status === "running" ||
        a.status === "queued"
    )
  );
  const pausedAgents = sortSessionsByPriority(
    agents.filter((a) => a.status === "paused")
  );
  const doneAgents = sortSessionsByPriority(
    agents.filter((a) => a.status === "done")
  );
  const allAgents = sortSessionsByPriority(agents);
  const filteredAgents =
    agentFilter === "active"
      ? activeAgents
      : agentFilter === "paused"
        ? pausedAgents
        : agentFilter === "done"
          ? doneAgents
          : allAgents;

  const waitingCount = agents.filter(
    (a) => a.status === "waiting_permission"
  ).length;
  const pillDinoStatus: AgentStatus =
    waitingCount > 0
      ? "waiting_permission"
      : agents.some((a) => a.status === "running")
        ? "running"
        : pausedAgents.length > 0
          ? "paused"
          : "done";

  return (
    <div
      className={`bg-clip-padding border text-zinc-100 h-screen flex flex-col overflow-hidden transition-colors ${
        collapsed
          ? "rounded-[13px] bg-zinc-950"
          : "rounded-[14px] bg-zinc-950/98"
      } ${
        waitingCount > 0
          ? "border-rose-500/70"
          : "border-zinc-700/80"
      }`}
    >
      {/* Draggable Header Bar / Floating Pill */}
      <header
        data-tauri-drag-region
        onMouseDown={handleHeaderMouseDown}
        onMouseUp={handleHeaderMouseUp}
        className={`flex items-center justify-between border-zinc-800/80 cursor-grab active:cursor-grabbing select-none shrink-0 ${
          collapsed
            ? "px-2 h-full bg-zinc-900/40"
            : "px-3 py-2 border-b bg-zinc-900/60"
        }`}
      >
        <div
          data-tauri-drag-region
          className={`flex items-center pointer-events-none min-w-0 ${
            collapsed ? "gap-2" : "gap-1.5"
          }`}
        >
          {collapsed ? (
            <PixelDinoAvatar status={pillDinoStatus} compact />
          ) : (
            <span className="relative flex h-2.5 w-2.5 shrink-0">
              <span
                className={`animate-ping absolute inline-flex h-full w-full rounded-full opacity-75 ${
                  waitingCount > 0 ? "bg-rose-400" : "bg-emerald-400"
                }`}
              ></span>
              <span
                className={`relative inline-flex rounded-full h-2.5 w-2.5 ${
                  waitingCount > 0 ? "bg-rose-500" : "bg-emerald-500"
                }`}
              ></span>
            </span>
          )}

          <span className="font-semibold text-xs tracking-tight text-zinc-100 truncate">
            DinoBuds
          </span>

          <span
            className={`px-1.5 py-0.5 text-[10px] font-semibold rounded-full border shrink-0 ${
              waitingCount > 0
                ? "bg-rose-500/20 text-rose-300 border-rose-500/40 animate-pulse"
                : "bg-zinc-800 text-zinc-300 border-zinc-700/70"
            }`}
          >
            {waitingCount > 0 ? `${waitingCount}!` : agents.length}
          </span>

          {!collapsed && ojt?.active_shift && (
            <button
              type="button"
              onClick={() => setActiveTab("ojt")}
              title={
                ojt.active_shift.is_paused
                  ? `On Lunch / Break (Shift started @ ${ojt.active_shift.time_in_str})`
                  : `Active OJT shift started @ ${ojt.active_shift.time_in_str}`
              }
              className={`pointer-events-auto flex items-center gap-1 px-1.5 py-0.5 rounded-full border text-[10px] font-mono font-semibold shrink-0 cursor-pointer transition-colors ${
                ojt.active_shift.is_paused
                  ? "bg-amber-500/15 border-amber-500/40 text-amber-300 hover:bg-amber-500/25"
                  : "bg-emerald-500/15 border-emerald-500/40 text-emerald-300 hover:bg-emerald-500/25"
              }`}
            >
              <span
                className={`w-1.5 h-1.5 rounded-full animate-pulse ${
                  ojt.active_shift.is_paused ? "bg-amber-400" : "bg-emerald-400"
                }`}
              />
              {ojt.active_shift.is_paused
                ? `BREAK ${formatElapsedHms(breakElapsedSecs)}`
                : formatElapsedHms(shiftElapsedSecs)}
            </button>
          )}
        </div>

        <div className="flex items-center gap-1 shrink-0">
          <button
            type="button"
            onClick={handleToggleWindowMode}
            title={
              windowMode === "top-right"
                ? "Mode: Top-Right Corner (Click to switch to Free Floating)"
                : "Mode: Free Floating (Click to snap to Top-Right Corner)"
            }
            className={`p-1.5 rounded-lg transition-colors cursor-pointer ${
              windowMode === "top-right"
                ? "text-emerald-400 hover:bg-emerald-500/10"
                : "text-sky-400 hover:bg-sky-500/10"
            }`}
          >
            {windowMode === "top-right" ? (
              <Pin className="w-3.5 h-3.5" />
            ) : (
              <Move className="w-3.5 h-3.5" />
            )}
          </button>

          {!collapsed && (
            <button
              type="button"
              onClick={() => setMuted((m) => !m)}
              title={muted ? "Unmute sound effects" : "Mute sound effects"}
              className="p-1.5 rounded-lg text-zinc-400 hover:text-zinc-100 hover:bg-zinc-800/80 transition-colors cursor-pointer"
            >
              {muted ? (
                <VolumeX className="w-3.5 h-3.5 text-rose-400" />
              ) : (
                <Volume2 className="w-3.5 h-3.5" />
              )}
            </button>
          )}

          <button
            type="button"
            onClick={handleToggleCollapse}
            title={collapsed ? "Expand HUD" : "Shrink to Floating Dino Pill"}
            className="p-1.5 rounded-lg text-zinc-400 hover:text-zinc-100 hover:bg-zinc-800/80 transition-colors cursor-pointer"
          >
            {collapsed ? (
              <ChevronDown className="w-3.5 h-3.5" />
            ) : (
              <ChevronUp className="w-3.5 h-3.5" />
            )}
          </button>

          {!collapsed && (
            <button
              type="button"
              onClick={handleCloseWindow}
              title="Close DinoBuds"
              className="p-1.5 rounded-lg text-zinc-400 hover:text-rose-400 hover:bg-rose-500/10 transition-colors cursor-pointer"
            >
              <X className="w-3.5 h-3.5" />
            </button>
          )}
        </div>
      </header>

      {/* Navigation Tabs: Agents | RAM Reaper | OJT Pulse */}
      {!collapsed && (
        <div className="ui-chrome flex items-center justify-between px-2 py-1.5 bg-zinc-900/40 border-b border-zinc-800/60 shrink-0 gap-1">
          <div className="flex items-center gap-1 min-w-0">
            <button
              type="button"
              onClick={() => setActiveTab("agents")}
              className={`px-2 py-1 rounded-lg text-[10.5px] font-medium transition-colors cursor-pointer whitespace-nowrap ${
                activeTab === "agents"
                  ? "bg-zinc-800 text-zinc-100 border border-zinc-700/70"
                  : "text-zinc-400 hover:text-zinc-200"
              }`}
            >
              Agents ({agents.length})
            </button>
            <button
              type="button"
              onClick={() => {
                setActiveTab("reaper");
                fetchHealth();
              }}
              className={`flex items-center gap-1 px-2 py-1 rounded-lg text-[10.5px] font-medium transition-colors cursor-pointer whitespace-nowrap ${
                activeTab === "reaper"
                  ? "bg-zinc-800 text-emerald-300 border border-emerald-500/30"
                  : "text-zinc-400 hover:text-zinc-200"
              }`}
            >
              <Cpu className="w-3 h-3 shrink-0" />
              RAM Reaper
            </button>
            <button
              type="button"
              onClick={() => {
                setActiveTab("ojt");
                fetchOjtState(false);
              }}
              className={`flex items-center gap-1 px-2 py-1 rounded-lg text-[10.5px] font-medium transition-colors cursor-pointer whitespace-nowrap ${
                activeTab === "ojt"
                  ? "bg-zinc-800 text-amber-300 border border-amber-500/30"
                  : "text-zinc-400 hover:text-zinc-200"
              }`}
            >
              <Clock className="w-3 h-3 shrink-0" />
              OJT Pulse
            </button>
          </div>

          {health && (
            <button
              type="button"
              onClick={() => {
                setActiveTab("reaper");
                fetchHealth();
              }}
              title={`System RAM: ${health.used_gb}/${health.total_gb} GB (${health.usage_percent}%)`}
              className="flex items-center gap-1 text-[10px] font-mono text-zinc-400 hover:text-zinc-200 cursor-pointer shrink-0"
            >
              <span
                className={`inline-block w-1.5 h-1.5 rounded-full ${
                  health.usage_percent >= 85
                    ? "bg-rose-500"
                    : health.usage_percent >= 70
                      ? "bg-amber-400"
                      : "bg-emerald-400"
                }`}
              />
              {health.usage_percent}%
            </button>
          )}
        </div>
      )}

      {/* Main Content Area */}
      {!collapsed && activeTab === "agents" && (
        <>
          <div className="ui-chrome shrink-0 px-2.5 pt-2 pb-1 flex items-center justify-between gap-1.5 border-b border-zinc-800/50 bg-zinc-950/60">
            <div
              role="group"
              aria-label="Filter agents by status"
              className="inline-flex items-center gap-0.5 rounded-lg bg-zinc-900/90 border border-zinc-800/80 p-0.5"
            >
              {(
                [
                  { id: "active", label: "Active", count: activeAgents.length },
                  { id: "paused", label: "Paused", count: pausedAgents.length },
                  { id: "done", label: "Done", count: doneAgents.length },
                  { id: "all", label: "All", count: agents.length },
                ] as const
              ).map((option) => {
                const isSelected = agentFilter === option.id;
                return (
                  <button
                    key={option.id}
                    type="button"
                    aria-pressed={isSelected}
                    onClick={() => setAgentFilter(option.id)}
                    className={`flex items-center gap-1 px-1.5 py-0.5 rounded-md text-[10px] font-medium transition-colors cursor-pointer ${
                      isSelected
                        ? "bg-zinc-800 text-zinc-100 border border-zinc-700/70 shadow-sm"
                        : "text-zinc-400 hover:text-zinc-200 border border-transparent"
                    }`}
                  >
                    <span>{option.label}</span>
                    <span
                      className={`font-mono text-[9px] px-1 rounded ${
                        option.id === "paused" && option.count > 0
                          ? "bg-amber-500/20 text-amber-300"
                          : isSelected
                            ? option.id === "active" && activeAgents.length > 0
                              ? "bg-emerald-500/20 text-emerald-300"
                              : "bg-zinc-700/70 text-zinc-300"
                            : "text-zinc-500"
                      }`}
                    >
                      {option.count}
                    </span>
                  </button>
                );
              })}
            </div>

            {waitingCount > 0 && (
              <span className="text-[9.5px] font-medium text-rose-300 bg-rose-500/15 border border-rose-500/30 rounded-full px-1.5 py-0.5 shrink-0">
                {waitingCount} review
              </span>
            )}
          </div>

          <main
            tabIndex={0}
            aria-label="DinoBuds agents list"
            className="dino-scroller flex-1 overflow-y-auto p-2.5 space-y-2 focus:outline-none"
          >
            {agents.length === 0 ? (
              <div className="flex flex-col items-center justify-center h-full text-center text-zinc-500 py-12">
                <p className="text-xs font-medium">No active DinoBuds</p>
                <p className="text-[11px] text-zinc-600 mt-1">
                  Listening on 127.0.0.1:4747
                </p>
              </div>
            ) : filteredAgents.length === 0 ? (
              <div className="flex flex-col items-center justify-center h-full text-center text-zinc-500 py-10 px-4">
                <p className="text-xs font-medium text-zinc-400">
                  {agentFilter === "active"
                    ? "No active agents right now"
                    : agentFilter === "paused"
                      ? "No paused checkpoints right now"
                      : "No completed agents yet"}
                </p>
                <p className="text-[11px] text-zinc-600 mt-1">
                  {agentFilter === "active" && pausedAgents.length > 0
                    ? `${pausedAgents.length} paused checkpoint${pausedAgents.length === 1 ? "" : "s"} in Paused.`
                    : agentFilter === "active" && doneAgents.length > 0
                      ? `${doneAgents.length} completed run${doneAgents.length === 1 ? "" : "s"} in Done.`
                      : `Total tracked sessions: ${agents.length}.`}
                </p>
                <div className="flex flex-wrap items-center justify-center gap-1.5 mt-3">
                  {agentFilter !== "paused" && pausedAgents.length > 0 && (
                    <button
                      type="button"
                      onClick={() => setAgentFilter("paused")}
                      className="px-2.5 py-1 rounded-lg bg-amber-500/15 hover:bg-amber-500/25 text-amber-300 border border-amber-500/30 text-[11px] font-medium transition-colors cursor-pointer"
                    >
                      View Paused ({pausedAgents.length})
                    </button>
                  )}
                  {agentFilter !== "done" && doneAgents.length > 0 && (
                    <button
                      type="button"
                      onClick={() => setAgentFilter("done")}
                      className="px-2.5 py-1 rounded-lg bg-zinc-800 hover:bg-zinc-700 text-zinc-200 border border-zinc-700/70 text-[11px] font-medium transition-colors cursor-pointer"
                    >
                      View Done ({doneAgents.length})
                    </button>
                  )}
                  {agentFilter !== "active" && activeAgents.length > 0 && (
                    <button
                      type="button"
                      onClick={() => setAgentFilter("active")}
                      className="px-2.5 py-1 rounded-lg bg-zinc-800 hover:bg-zinc-700 text-zinc-200 border border-zinc-700/70 text-[11px] font-medium transition-colors cursor-pointer"
                    >
                      View Active ({activeAgents.length})
                    </button>
                  )}
                  <button
                    type="button"
                    onClick={() => setAgentFilter("all")}
                    className="px-2.5 py-1 rounded-lg bg-zinc-900 hover:bg-zinc-800 text-zinc-400 hover:text-zinc-200 border border-zinc-800 text-[11px] font-medium transition-colors cursor-pointer"
                  >
                    Show All ({agents.length})
                  </button>
                </div>
              </div>
            ) : (
              filteredAgents.map((agent) => {
              const progressPct =
                agent.current_step && agent.total_steps
                  ? Math.min(
                      100,
                      Math.round((agent.current_step / agent.total_steps) * 100)
                    )
                  : 0;
              const workspaceName = formatWorkspaceName(agent.workspace_path);

              return (
                <div
                  key={agent.id}
                  className={`group relative rounded-xl p-2.5 transition-all border ${
                    agent.status === "waiting_permission"
                      ? "bg-rose-950/20 border-rose-500/40 shadow-lg shadow-rose-950/20"
                      : agent.status === "paused"
                        ? "bg-amber-950/10 border-amber-500/30 hover:border-amber-500/50"
                        : "bg-zinc-900/70 border-zinc-800/80 hover:border-zinc-700/80"
                  }`}
                >
                  <div className="flex items-start gap-2.5">
                    <PixelDinoAvatar status={agent.status} />

                    <div className="flex-1 min-w-0">
                      <div className="flex items-center justify-between gap-1">
                        <div className="flex items-center gap-1.5 min-w-0">
                          <span className="select-text text-xs font-semibold text-zinc-100 truncate">
                            {agent.name}
                          </span>
                          {workspaceName && (
                            <span className="select-text text-[10px] text-zinc-500 font-mono truncate">
                              /{workspaceName}
                            </span>
                          )}
                        </div>

                        <div className="ui-chrome flex items-center gap-1 shrink-0">
                          {(agent.status === "running" ||
                            agent.status === "waiting_permission") && (
                            <button
                              type="button"
                              onClick={() => handleTogglePause(agent.id)}
                              title="Pause session at current checkpoint"
                              className="p-1 rounded-md text-amber-400/90 hover:text-amber-300 hover:bg-amber-500/15 transition-colors cursor-pointer"
                            >
                              <Pause className="w-3 h-3" />
                            </button>
                          )}
                          {agent.status === "paused" && (
                            <button
                              type="button"
                              onClick={() => handleTogglePause(agent.id)}
                              title="Resume checkpoint"
                              className="flex items-center gap-0.5 px-1.5 py-0.5 rounded-md bg-emerald-500/15 hover:bg-emerald-500/30 text-emerald-300 border border-emerald-500/30 text-[9.5px] font-semibold transition-colors cursor-pointer"
                            >
                              <Play className="w-2.5 h-2.5 fill-current" />
                              <span>Resume</span>
                            </button>
                          )}
                          {agent.workspace_path && (
                            <button
                              type="button"
                              onClick={() =>
                                handleFocusWorkspace(agent.workspace_path)
                              }
                              title={`Open ${agent.workspace_path}`}
                              className="p-1 rounded-md text-zinc-400 hover:text-zinc-100 hover:bg-zinc-800 transition-colors cursor-pointer"
                            >
                              <ExternalLink className="w-3 h-3" />
                            </button>
                          )}
                          <button
                            type="button"
                            onClick={() => handleDismiss(agent.id)}
                            title="Dismiss session"
                            className="p-1 rounded-md text-zinc-500 hover:text-zinc-200 hover:bg-zinc-800 transition-colors cursor-pointer"
                          >
                            <X className="w-3 h-3" />
                          </button>
                        </div>
                      </div>

                      <div className="flex items-center gap-1.5 mt-1">
                        <StatusBadge status={agent.status} />
                        {agent.total_steps > 0 && (
                          <span className="select-text text-[11px] font-mono text-zinc-400 truncate">
                            {agent.current_step}/{agent.total_steps}{" "}
                            <span className="text-zinc-300 font-sans">
                              {agent.step_label}
                            </span>
                          </span>
                        )}
                      </div>

                      <p className="select-text text-[11px] text-zinc-400 truncate mt-1.5">
                        {agent.task}
                      </p>

                      {/* Progress Bar for running / paused / done steps */}
                      {agent.total_steps > 0 && (
                        <div className="w-full h-1.5 bg-zinc-800 rounded-full overflow-hidden mt-2">
                          <div
                            className={`h-full rounded-full transition-all duration-300 ${
                              agent.status === "done"
                                ? "bg-purple-500"
                                : agent.status === "waiting_permission"
                                  ? "bg-amber-400"
                                  : agent.status === "paused"
                                    ? "bg-amber-400/80"
                                    : agent.status === "queued"
                                      ? "bg-yellow-400"
                                      : "dino-progress-active"
                            }`}
                            style={{ width: `${progressPct}%` }}
                          />
                        </div>
                      )}

                      {/* Native Antigravity Approval Action Bar */}
                      {agent.status === "waiting_permission" && (
                        <div className="mt-2.5 pt-2 border-t border-rose-500/20">
                          <div className="flex items-start gap-1.5 px-2 py-1.5 mb-2 rounded-lg bg-zinc-950/90 border border-zinc-800 text-[10px] font-mono text-amber-300">
                            <Terminal className="w-3 h-3 shrink-0 text-rose-400 mt-0.5" />
                            <span className="select-text line-clamp-2 break-all leading-relaxed">
                              {agent.question ||
                                "Agent stopped — waiting for Submit"}
                            </span>
                          </div>
                          <div className="ui-chrome flex items-center gap-1.5">
                            <button
                              type="button"
                              onClick={() =>
                                handleSubmitPrompt(agent.id, 1, true)
                              }
                              title="Focus Antigravity & Submit option 1 (Enter)"
                              className="flex-[1.3] flex items-center justify-center gap-1 py-1 px-2.5 rounded-lg bg-blue-600 hover:bg-blue-500 text-white border border-blue-400/50 shadow-sm shadow-blue-950/50 text-[11px] font-semibold transition-colors cursor-pointer"
                            >
                              <span>Submit ⏎</span>
                            </button>
                            <button
                              type="button"
                              onClick={() =>
                                handleSubmitPrompt(agent.id, 2, true)
                              }
                              title="Focus Antigravity & Always Allow (2 + Enter)"
                              className="flex-1 flex items-center justify-center gap-1 py-1 px-2 rounded-lg bg-zinc-800 hover:bg-zinc-700 text-zinc-200 border border-zinc-700 text-[10px] font-medium transition-colors cursor-pointer"
                            >
                              <Check className="w-3 h-3 text-emerald-400" />
                              <span>Allow (2)</span>
                            </button>
                            <button
                              type="button"
                              onClick={() => handleTogglePause(agent.id)}
                              title="Mark as Paused & stop alarm"
                              className="flex items-center justify-center gap-1 py-1 px-2 rounded-lg bg-amber-500/15 hover:bg-amber-500/25 text-amber-300 border border-amber-500/30 text-[10px] font-medium transition-colors cursor-pointer"
                            >
                              <Pause className="w-3 h-3" />
                            </button>
                            <button
                              type="button"
                              onClick={() =>
                                handleSubmitPrompt(agent.id, null, false)
                              }
                              title="Skip / Deny prompt"
                              className="flex items-center justify-center gap-1 py-1 px-2 rounded-lg bg-rose-500/15 hover:bg-rose-500/25 text-rose-300 border border-rose-500/30 text-[10px] font-medium transition-colors cursor-pointer"
                            >
                              <Ban className="w-3 h-3" />
                              <span>Skip</span>
                            </button>
                          </div>
                        </div>
                      )}
                    </div>
                  </div>
                </div>
              );
            })
          )}
          </main>
        </>
      )}

      {/* RAM & Orphan Process Reaper View */}
      {!collapsed && activeTab === "reaper" && (
        <main
          tabIndex={0}
          aria-label="System RAM and heavy dev processes"
          className="dino-scroller flex-1 overflow-y-auto p-2.5 space-y-2.5 focus:outline-none"
        >
          {/* RAM Pressure Card */}
          <div className="rounded-xl p-2.5 bg-zinc-900/70 border border-zinc-800/80">
            <div className="flex items-center justify-between text-xs">
              <span className="font-semibold text-zinc-200">
                System Memory Pressure
              </span>
              <span className="select-text font-mono text-zinc-300">
                {health
                  ? `${health.used_gb} / ${health.total_gb} GB (${health.usage_percent}%)`
                  : "Loading..."}
              </span>
            </div>

            <div className="w-full h-2 bg-zinc-800 rounded-full overflow-hidden mt-2">
              <div
                className={`h-full rounded-full transition-all duration-300 ${
                  (health?.usage_percent ?? 0) >= 85
                    ? "bg-rose-500"
                    : (health?.usage_percent ?? 0) >= 70
                      ? "bg-amber-400"
                      : "bg-emerald-400"
                }`}
                style={{ width: `${health?.usage_percent ?? 0}%` }}
              />
            </div>

            <div className="ui-chrome flex items-center gap-2 mt-2.5">
              <button
                type="button"
                onClick={handleSweepNode}
                className="flex-1 flex items-center justify-center gap-1.5 py-1.5 px-2.5 rounded-lg bg-amber-500/15 hover:bg-amber-500/25 text-amber-300 border border-amber-500/30 text-xs font-medium transition-colors cursor-pointer"
              >
                <Zap className="w-3.5 h-3.5" />
                Sweep Node RAM
              </button>
              <button
                type="button"
                onClick={fetchHealth}
                title="Refresh process list"
                className="p-1.5 rounded-lg bg-zinc-800 hover:bg-zinc-700 text-zinc-300 border border-zinc-700/70 transition-colors cursor-pointer"
              >
                <RefreshCw
                  className={`w-3.5 h-3.5 ${
                    isRefreshingHealth ? "animate-spin" : ""
                  }`}
                />
              </button>
            </div>

            {sweepBanner && (
              <div className="select-text mt-2 px-2 py-1 rounded bg-emerald-500/15 border border-emerald-500/30 text-[11px] text-emerald-300">
                {sweepBanner}
              </div>
            )}
          </div>

          {/* Heavy Dev Processes List */}
          <div className="space-y-1.5">
            <div className="flex items-center justify-between px-1">
              <span className="text-[11px] font-semibold uppercase tracking-wider text-zinc-400">
                Heavy Dev Processes
              </span>
              <span className="text-[10px] text-zinc-500">
                Node / TS / Rust / Vite
              </span>
            </div>

            {!health || health.processes.length === 0 ? (
              <div className="rounded-xl p-4 text-center bg-zinc-900/40 border border-zinc-800/60 text-xs text-zinc-500">
                No heavy dev processes (&ge;25 MB) detected.
              </div>
            ) : (
              health.processes.map((proc) => (
                <div
                  key={proc.pid}
                  className="flex items-center justify-between px-2.5 py-2 rounded-xl bg-zinc-900/70 border border-zinc-800/80 hover:border-zinc-700/80"
                >
                  <div className="min-w-0">
                    <div className="flex items-center gap-1.5">
                      <span className="select-text text-xs font-medium text-zinc-200 truncate">
                        {proc.name}
                      </span>
                      <span className="select-text text-[10px] font-mono text-zinc-500">
                        PID {proc.pid}
                      </span>
                    </div>
                  </div>

                  <div className="flex items-center gap-2 shrink-0">
                    <span
                      className={`select-text text-xs font-mono font-semibold ${
                        proc.memory_mb >= 300
                          ? "text-rose-400"
                          : proc.memory_mb >= 150
                            ? "text-amber-300"
                            : "text-emerald-300"
                      }`}
                    >
                      {proc.memory_mb} MB
                    </span>
                    <button
                      type="button"
                      onClick={() => handleKillProcess(proc.pid, proc.name)}
                      title={`Kill ${proc.name} (PID ${proc.pid})`}
                      className="ui-chrome flex items-center gap-1 px-2 py-1 rounded-lg bg-rose-500/15 hover:bg-rose-500/30 text-rose-300 border border-rose-500/30 text-[10px] font-medium transition-colors cursor-pointer"
                    >
                      <Trash2 className="w-3 h-3" />
                      Kill
                    </button>
                  </div>
                </div>
              ))
            )}
          </div>
        </main>
      )}

      {/* OJT Pulse Tab View */}
      {!collapsed && activeTab === "ojt" && (
        <main className="dino-scroll flex-1 overflow-y-auto px-2.5 py-2 space-y-2.5">
          {/* Progress & Active Shift Card */}
          <div className="rounded-xl p-2.5 bg-zinc-900/80 border border-zinc-800/80 space-y-2">
            <div className="flex items-center justify-between">
              <div className="flex items-center gap-1.5">
                <Clock className="w-3.5 h-3.5 text-amber-400" />
                <span className="text-[11px] font-semibold uppercase tracking-wider text-zinc-400">
                  OJT Hours Tracker
                </span>
              </div>
              <span className="select-text text-[11px] font-mono font-bold text-amber-300">
                {ojt
                  ? `${ojt.rendered_hours.toFixed(2)} / ${ojt.target_hours.toFixed(0)} hrs`
                  : "0.00 / 486 hrs"}
              </span>
            </div>

            <div className="w-full h-2 rounded-full bg-zinc-950 border border-zinc-800 overflow-hidden">
              <div
                className="h-full rounded-full bg-gradient-to-r from-emerald-500 via-teal-400 to-amber-400 transition-all duration-300"
                style={{
                  width: `${Math.min(100, Math.max(0, ojt?.progress_percent ?? 0))}%`,
                }}
              />
            </div>

            <div className="flex items-center justify-between text-[10px] text-zinc-400 font-mono">
              <span>
                {ojt ? `${ojt.progress_percent.toFixed(1)}%` : "0.0%"} &bull;{" "}
                {ojt
                  ? `${ojt.remaining_hours.toFixed(1)}h left (${ojt.shifts_remaining}d)`
                  : "486.0h left"}
              </span>
              <span>Est: {ojt?.estimated_completion_date ?? "Calculating..."}</span>
            </div>

            {ojt?.active_shift ? (
              <div className="space-y-1.5">
                {ojt.active_shift.is_paused && (
                  <div className="flex items-center justify-between px-2 py-1 rounded-lg bg-amber-500/15 border border-amber-500/35 text-[10px] font-mono text-amber-200">
                    <span className="font-bold tracking-wider">ON BREAK</span>
                    <span>
                      {formatElapsedHms(breakElapsedSecs)} (Duty: {formatElapsedHms(shiftElapsedSecs)})
                    </span>
                  </div>
                )}
                <div className="grid grid-cols-2 gap-1.5">
                  <button
                    type="button"
                    onClick={handleToggleShiftBreak}
                    className={`ui-chrome flex items-center justify-center gap-1 px-2 py-1.5 rounded-lg text-[11px] font-semibold border transition-colors cursor-pointer ${
                      ojt.active_shift.is_paused
                        ? "bg-amber-500/20 hover:bg-amber-500/30 text-amber-200 border-amber-500/40"
                        : "bg-zinc-800 hover:bg-zinc-700 text-zinc-100 border-zinc-700/80"
                    }`}
                  >
                    {ojt.active_shift.is_paused ? (
                      <>
                        <Play className="w-3 h-3 fill-current" />
                        Resume Duty
                      </>
                    ) : (
                      <>
                        <Pause className="w-3 h-3" />
                        Lunch / Break
                      </>
                    )}
                  </button>
                  <button
                    type="button"
                    onClick={handleToggleShift}
                    title={`Clock out (In @ ${ojt.active_shift.time_in_str})`}
                    className="ui-chrome flex items-center justify-center gap-1 px-2 py-1.5 rounded-lg text-[11px] font-semibold border transition-colors cursor-pointer bg-rose-500/20 hover:bg-rose-500/30 text-rose-200 border-rose-500/40"
                  >
                    <Square className="w-3 h-3 fill-current" />
                    Time Out &bull; {formatElapsedHms(shiftElapsedSecs)}
                  </button>
                </div>
              </div>
            ) : (
              <button
                type="button"
                onClick={handleToggleShift}
                className="ui-chrome w-full flex items-center justify-center gap-1.5 px-2.5 py-1.5 rounded-lg text-xs font-semibold border transition-colors cursor-pointer bg-emerald-500/20 hover:bg-emerald-500/30 text-emerald-200 border-emerald-500/40"
              >
                <Play className="w-3.5 h-3.5 fill-current" />
                Time In &mdash; Start OJT Shift
              </button>
            )}

            {/* Live Proof-of-Work Telemetry Strip */}
            <div className="rounded-lg p-1.5 bg-zinc-950/80 border border-zinc-800/80 space-y-1">
              <div className="flex flex-wrap items-center gap-1 text-[9.5px] font-mono">
                <span
                  title="Active input heartbeats sampled every 60s during shift"
                  className={`inline-flex items-center gap-1 px-1.5 py-0.5 rounded border ${
                    (ojt?.current_focus_percent ?? 0) >= 70
                      ? "bg-emerald-500/15 text-emerald-300 border-emerald-500/30"
                      : "bg-amber-500/10 text-amber-200 border-amber-500/25"
                  }`}
                >
                  <ShieldCheck className="w-2.5 h-2.5" />
                  Verified Active:{" "}
                  {ojt?.active_shift
                    ? ojt.active_shift.active_minutes
                    : ojt?.completed_shifts[0]?.active_minutes ?? 0}
                  m ({(ojt?.current_focus_percent ?? 0).toFixed(0)}%)
                </span>

                <span
                  title="Network / Workstation stamp captured for DTR audit"
                  className="inline-flex items-center gap-1 px-1.5 py-0.5 rounded bg-zinc-800/90 text-zinc-300 border border-zinc-700/70 truncate max-w-[140px]"
                >
                  <Wifi className="w-2.5 h-2.5 text-sky-400 shrink-0" />
                  <span className="truncate">
                    {ojt?.current_network_stamp ?? "Workstation"}
                  </span>
                </span>

                <span
                  title="Git commits and logged tasks captured today"
                  className="inline-flex items-center gap-1 px-1.5 py-0.5 rounded bg-zinc-800/90 text-zinc-300 border border-zinc-700/70"
                >
                  <GitCommitHorizontal className="w-2.5 h-2.5 text-violet-400" />
                  {ojt?.today_commits.length ?? 0}c &bull;{" "}
                  {ojt?.today_activities.length ?? 0} tasks
                </span>
              </div>

              {ojt && ojt.top_active_apps.length > 0 && (
                <div className="text-[9.5px] text-zinc-400 truncate px-0.5 font-mono">
                  {ojt.top_active_apps
                    .map(([app, mins]) => `${app} ${formatMinutesCompact(mins)}`)
                    .join(" • ")}
                </div>
              )}
            </div>

            {ojtBanner && (
              <div className="select-text px-2 py-1 rounded bg-emerald-500/15 border border-emerald-500/30 text-[10.5px] text-emerald-300">
                {ojtBanner}
              </div>
            )}
          </div>

          {/* Sub-tabs: Today's Log | DTR History | Settings */}
          <div className="ui-chrome flex items-center gap-1 rounded-lg bg-zinc-900/90 border border-zinc-800/80 p-0.5">
            {(
              [
                { id: "today", label: "Today's Log", icon: ListChecks },
                { id: "history", label: "DTR History", icon: History },
                { id: "settings", label: "Settings", icon: Settings },
              ] as const
            ).map((sub) => {
              const Icon = sub.icon;
              const isSelected = ojtSubTab === sub.id;
              return (
                <button
                  key={sub.id}
                  type="button"
                  onClick={() => {
                    setOjtSubTab(sub.id);
                    if (sub.id === "settings" && ojt) {
                      setTargetHoursInput(String(ojt.target_hours));
                      setReposInput(ojt.watched_repos.join("\n"));
                      setExportDirInput(ojt.obsidian_export_dir ?? "");
                    }
                  }}
                  className={`flex-1 flex items-center justify-center gap-1 px-2 py-1 rounded-md text-[10px] font-medium transition-colors cursor-pointer ${
                    isSelected
                      ? "bg-zinc-800 text-zinc-100 border border-zinc-700/70"
                      : "text-zinc-400 hover:text-zinc-200"
                  }`}
                >
                  <Icon className="w-3 h-3" />
                  {sub.label}
                </button>
              );
            })}
          </div>

          {/* Sub-view: Today's Log */}
          {ojtSubTab === "today" && (
            <div className="space-y-2">
              <button
                type="button"
                onClick={handleScanActivity}
                disabled={isSyncingOjt}
                className="ui-chrome w-full flex items-center justify-center gap-1.5 px-2 py-1.5 rounded-lg bg-zinc-800 hover:bg-zinc-700 text-zinc-100 border border-zinc-700/80 text-[11px] font-medium transition-colors cursor-pointer disabled:opacity-60"
              >
                <RefreshCw
                  className={`w-3 h-3 ${isSyncingOjt ? "animate-spin" : ""}`}
                />
                Sync Docs, Git &amp; Agent Tasks
              </button>

              <div className="grid grid-cols-2 gap-1.5">
                <button
                  type="button"
                  onClick={() => handleExportMarkdown("daily")}
                  title="Generate verifiable Daily Accomplishment Record & copy to clipboard"
                  className="ui-chrome flex items-center justify-center gap-1 px-2 py-1.5 rounded-lg bg-amber-500/15 hover:bg-amber-500/25 text-amber-300 border border-amber-500/30 text-[10px] font-medium transition-colors cursor-pointer"
                >
                  <FileText className="w-3 h-3 shrink-0" />
                  <span className="truncate">Copy Daily Record (.md)</span>
                </button>
                <button
                  type="button"
                  onClick={() => handleExportMarkdown("weekly")}
                  title="Compile formal Weekly OJT Journal & DTR table & copy to clipboard"
                  className="ui-chrome flex items-center justify-center gap-1 px-2 py-1.5 rounded-lg bg-emerald-500/15 hover:bg-emerald-500/25 text-emerald-300 border border-emerald-500/30 text-[10px] font-medium transition-colors cursor-pointer"
                >
                  <BookOpen className="w-3 h-3 shrink-0" />
                  <span className="truncate">Compile Weekly Journal (.md)</span>
                </button>
              </div>

              {/* Git Commit Evidence Badges */}
              {ojt && ojt.today_commits.length > 0 && (
                <div className="rounded-xl p-2 bg-zinc-900/70 border border-zinc-800/80 space-y-1">
                  <div className="flex items-center gap-1 text-[9.5px] font-semibold uppercase tracking-wider text-zinc-400">
                    <GitCommitHorizontal className="w-3 h-3 text-violet-400" />
                    Git Commit Evidence ({ojt.today_commits.length})
                  </div>
                  <div className="flex flex-wrap gap-1">
                    {ojt.today_commits.slice(0, 6).map((c) => (
                      <span
                        key={`${c.repo}-${c.hash}`}
                        title={`${c.repo} @ ${c.timestamp}: ${c.message}`}
                        className="select-text inline-flex items-center gap-1 px-1.5 py-0.5 rounded bg-zinc-950 border border-zinc-800 text-[9.5px] font-mono text-zinc-300"
                      >
                        <span className="text-violet-300">{c.hash}</span>
                        <span className="text-zinc-500">{c.repo}</span>
                        {c.files_changed > 0 && (
                          <span className="text-emerald-400">
                            +{c.insertions}/-{c.deletions}
                          </span>
                        )}
                      </span>
                    ))}
                    {ojt.today_commits.length > 6 && (
                      <span className="text-[9.5px] text-zinc-500 px-1 py-0.5">
                        +{ojt.today_commits.length - 6} more
                      </span>
                    )}
                  </div>
                </div>
              )}

              {/* 1-Click Office/IT Work Category Pills */}
              <div className="flex flex-wrap items-center gap-1">
                {[
                  "Data Entry",
                  "Docs / Reports",
                  "IT Support",
                  "UI / Design",
                  "Briefing",
                ].map((cat) => (
                  <button
                    key={cat}
                    type="button"
                    onClick={() => handleSelectCategoryPill(cat)}
                    className="ui-chrome px-1.5 py-0.5 rounded-md bg-zinc-900 hover:bg-zinc-800 text-zinc-300 hover:text-amber-200 border border-zinc-800 hover:border-amber-500/40 text-[9.5px] font-medium transition-colors cursor-pointer"
                  >
                    + {cat}
                  </button>
                ))}
              </div>

              <form onSubmit={handleAddManualActivity} className="flex items-center gap-1.5">
                <input
                  ref={manualNoteInputRef}
                  type="text"
                  value={manualNote}
                  onChange={(e) => setManualNote(e.target.value)}
                  placeholder="Add manual accomplishment bullet..."
                  className="flex-1 min-w-0 px-2.5 py-1.5 rounded-lg bg-zinc-900/90 border border-zinc-800 text-[11px] text-zinc-100 placeholder:text-zinc-500 focus:outline-none focus:border-amber-500/50"
                />
                <button
                  type="submit"
                  title="Add manual activity"
                  className="ui-chrome p-1.5 rounded-lg bg-zinc-800 hover:bg-zinc-700 text-zinc-200 border border-zinc-700/80 cursor-pointer"
                >
                  <Plus className="w-3.5 h-3.5" />
                </button>
              </form>

              {/* Daily Reflection Inputs */}
              <form
                onSubmit={handleSaveReflections}
                className="rounded-xl p-2 bg-zinc-900/70 border border-zinc-800/80 space-y-1.5"
              >
                <div className="flex items-center justify-between">
                  <span className="text-[9.5px] font-semibold uppercase tracking-wider text-zinc-400">
                    Daily Reflections (for Coordinator Journal)
                  </span>
                  <button
                    type="submit"
                    disabled={isSavingReflections}
                    className="ui-chrome inline-flex items-center gap-1 px-1.5 py-0.5 rounded bg-amber-500/15 hover:bg-amber-500/25 text-amber-300 border border-amber-500/30 text-[9.5px] font-medium cursor-pointer disabled:opacity-60"
                  >
                    <Save className="w-2.5 h-2.5" />
                    Save
                  </button>
                </div>
                <input
                  type="text"
                  value={challengesInput}
                  onChange={(e) => setChallengesInput(e.target.value)}
                  placeholder="Challenge / Blocker Solved..."
                  className="w-full px-2 py-1 rounded-lg bg-zinc-950/90 border border-zinc-800 text-[10.5px] text-zinc-100 placeholder:text-zinc-500 focus:outline-none focus:border-amber-500/50"
                />
                <input
                  type="text"
                  value={learningsInput}
                  onChange={(e) => setLearningsInput(e.target.value)}
                  placeholder="Key Learning / Technical Solution..."
                  className="w-full px-2 py-1 rounded-lg bg-zinc-950/90 border border-zinc-800 text-[10.5px] text-zinc-100 placeholder:text-zinc-500 focus:outline-none focus:border-amber-500/50"
                />
              </form>

              <div className="space-y-1.5">
                {!ojt || ojt.today_activities.length === 0 ? (
                  <div className="rounded-xl p-4 text-center bg-zinc-900/40 border border-zinc-800/60 text-[11px] text-zinc-500">
                    No activity bullets captured today yet. Click{" "}
                    <span className="text-zinc-300">Sync Git Commits &amp; Agent Tasks</span>{" "}
                    or add a manual note above.
                  </div>
                ) : (
                  ojt.today_activities.map((item, idx) => (
                    <div
                      key={`${idx}-${item.slice(0, 24)}`}
                      className="select-text px-2.5 py-1.5 rounded-lg bg-zinc-900/70 border border-zinc-800/80 text-[11px] text-zinc-200 leading-snug"
                    >
                      &bull; {item}
                    </div>
                  ))
                )}
              </div>
            </div>
          )}

          {/* Sub-view: DTR History */}
          {ojtSubTab === "history" && (
            <div className="space-y-2">
              <div className="flex items-center justify-between gap-1.5 px-0.5">
                <span className="text-[10.5px] font-semibold uppercase tracking-wider text-zinc-400">
                  Logged Shifts ({ojt?.completed_shifts.length ?? 0})
                </span>
                <div className="flex items-center gap-1">
                  <button
                    type="button"
                    onClick={() => handleExportMarkdown("weekly")}
                    className="ui-chrome flex items-center gap-1 px-2 py-1 rounded-lg bg-emerald-500/15 hover:bg-emerald-500/25 text-emerald-300 border border-emerald-500/30 text-[10px] font-medium cursor-pointer"
                  >
                    <BookOpen className="w-3 h-3" />
                    Compile Weekly Journal (.md)
                  </button>
                  <button
                    type="button"
                    onClick={() => handleExportMarkdown("all")}
                    title="Copy complete DTR table & journal"
                    className="ui-chrome flex items-center gap-1 px-1.5 py-1 rounded-lg bg-amber-500/15 hover:bg-amber-500/25 text-amber-300 border border-amber-500/30 text-[10px] font-medium cursor-pointer"
                  >
                    <Copy className="w-3 h-3" />
                  </button>
                </div>
              </div>

              {!ojt || ojt.completed_shifts.length === 0 ? (
                <div className="rounded-xl p-4 text-center bg-zinc-900/40 border border-zinc-800/60 text-[11px] text-zinc-500">
                  No completed OJT shifts yet. Use{" "}
                  <span className="text-emerald-300">Time In</span> /{" "}
                  <span className="text-rose-300">Time Out</span> to record your
                  DTR.
                </div>
              ) : (
                <div className="space-y-1.5">
                  {ojt.completed_shifts.map((shift) => (
                    <div
                      key={shift.id}
                      className="rounded-xl p-2.5 bg-zinc-900/75 border border-zinc-800/80 space-y-1.5"
                    >
                      <div className="flex items-center justify-between gap-2">
                        <div className="min-w-0">
                          <span className="select-text text-[11px] font-semibold text-zinc-100">
                            {shift.date_str}
                          </span>
                          <span className="select-text ml-2 text-[10px] font-mono text-zinc-400">
                            {shift.time_in} &rarr; {shift.time_out}
                          </span>
                        </div>
                        <div className="flex items-center gap-1.5 shrink-0">
                          <span className="px-1.5 py-0.5 rounded bg-emerald-500/15 border border-emerald-500/30 text-[10px] font-mono font-semibold text-emerald-300">
                            {shift.duration_hours.toFixed(2)}h
                          </span>
                          <button
                            type="button"
                            onClick={() =>
                              handleCopyShiftSummary(shift.summary_markdown)
                            }
                            title="Copy shift Markdown summary"
                            className="p-1 rounded bg-zinc-800 hover:bg-zinc-700 text-zinc-300 border border-zinc-700/70 cursor-pointer"
                          >
                            <Copy className="w-3 h-3" />
                          </button>
                        </div>
                      </div>

                      {/* Proof-of-Work Verification Row */}
                      <div className="flex flex-wrap items-center gap-1 text-[9.5px] font-mono">
                        {shift.verification_hash && (
                          <span
                            title="Deterministic Verification ID"
                            className="select-text inline-flex items-center gap-1 px-1.5 py-0.5 rounded bg-zinc-950 border border-zinc-800 text-amber-300"
                          >
                            <ShieldCheck className="w-2.5 h-2.5 text-emerald-400" />
                            {shift.verification_hash}
                          </span>
                        )}
                        <span className="inline-flex items-center px-1.5 py-0.5 rounded bg-zinc-800/80 border border-zinc-700/60 text-emerald-300">
                          {shift.focus_percent.toFixed(0)}% Active ({shift.active_minutes}m)
                        </span>
                        {shift.network_stamp_in && (
                          <span className="inline-flex items-center gap-1 px-1.5 py-0.5 rounded bg-zinc-800/80 border border-zinc-700/60 text-zinc-300 truncate max-w-[130px]">
                            <Wifi className="w-2.5 h-2.5 text-sky-400 shrink-0" />
                            <span className="truncate">{shift.network_stamp_in}</span>
                          </span>
                        )}
                        {shift.commit_evidence.length > 0 && (
                          <span className="inline-flex items-center gap-1 px-1.5 py-0.5 rounded bg-zinc-800/80 border border-zinc-700/60 text-violet-300">
                            <GitCommitHorizontal className="w-2.5 h-2.5" />
                            {shift.commit_evidence.length}c
                          </span>
                        )}
                      </div>

                      {shift.activities.length > 0 && (
                        <div className="space-y-0.5 border-t border-zinc-800/70 pt-1">
                          {shift.activities.slice(0, 3).map((act, i) => (
                            <div
                              key={`${shift.id}-${i}`}
                              className="select-text text-[10px] text-zinc-400 truncate"
                              title={act}
                            >
                              &bull; {act}
                            </div>
                          ))}
                          {shift.activities.length > 3 && (
                            <div className="text-[9.5px] text-zinc-500">
                              +{shift.activities.length - 3} more activity bullet(s)
                            </div>
                          )}
                        </div>
                      )}
                    </div>
                  ))}
                </div>
              )}
            </div>
          )}

          {/* Sub-view: Settings */}
          {ojtSubTab === "settings" && (
            <form
              onSubmit={handleSaveOjtSettings}
              className="rounded-xl p-2.5 bg-zinc-900/75 border border-zinc-800/80 space-y-2.5"
            >
              <div>
                <label className="block text-[10px] font-semibold uppercase tracking-wider text-zinc-400 mb-1">
                  Target Required OJT Hours
                </label>
                <input
                  type="number"
                  step="0.5"
                  min="1"
                  value={targetHoursInput}
                  onChange={(e) => setTargetHoursInput(e.target.value)}
                  className="w-full px-2.5 py-1.5 rounded-lg bg-zinc-950 border border-zinc-800 text-xs font-mono text-zinc-100 focus:outline-none focus:border-amber-500/50"
                />
              </div>

              <div>
                <label className="block text-[10px] font-semibold uppercase tracking-wider text-zinc-400 mb-1">
                  Watched Git Repos (one path per line)
                </label>
                <textarea
                  rows={4}
                  value={reposInput}
                  onChange={(e) => setReposInput(e.target.value)}
                  placeholder="C:\Users\neil\dinobuds"
                  className="w-full px-2.5 py-1.5 rounded-lg bg-zinc-950 border border-zinc-800 text-[10.5px] font-mono text-zinc-200 focus:outline-none focus:border-amber-500/50 resize-none"
                />
              </div>

              <div>
                <label className="block text-[10px] font-semibold uppercase tracking-wider text-zinc-400 mb-1">
                  Obsidian Export Folder (optional)
                </label>
                <input
                  type="text"
                  value={exportDirInput}
                  onChange={(e) => setExportDirInput(e.target.value)}
                  placeholder="C:\Users\neil\Documents\Obsidian\OJT"
                  className="w-full px-2.5 py-1.5 rounded-lg bg-zinc-950 border border-zinc-800 text-[10.5px] font-mono text-zinc-200 focus:outline-none focus:border-amber-500/50"
                />
              </div>

              <button
                type="submit"
                className="ui-chrome w-full flex items-center justify-center gap-1.5 px-2.5 py-1.5 rounded-lg bg-amber-500/20 hover:bg-amber-500/30 text-amber-200 border border-amber-500/40 text-xs font-semibold transition-colors cursor-pointer"
              >
                <Save className="w-3.5 h-3.5" />
                Save OJT Settings
              </button>
            </form>
          )}
        </main>
      )}
    </div>
  );
}
