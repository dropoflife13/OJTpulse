use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
#[cfg(target_os = "windows")]
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System};
use tauri::{AppHandle, Emitter, LogicalPosition, Manager, Position, State};
use tiny_http::{Header, Method, Response, Server, StatusCode};

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn lock_or_recover<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    match mutex.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AgentSession {
    pub id: String,
    pub name: String,
    pub task: String,
    pub status: String,
    #[serde(default)]
    pub current_step: u32,
    #[serde(default)]
    pub total_steps: u32,
    #[serde(default)]
    pub step_label: String,
    #[serde(default)]
    pub question: Option<String>,
    #[serde(default)]
    pub workspace_path: Option<String>,
    #[serde(default)]
    pub decision: Option<bool>,
    #[serde(default = "now_secs")]
    pub updated_at: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HeavyProcess {
    pub pid: u32,
    pub name: String,
    pub memory_mb: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SystemHealth {
    pub used_gb: f64,
    pub total_gb: f64,
    pub usage_percent: u32,
    pub processes: Vec<HeavyProcess>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SweepResult {
    pub killed_count: usize,
    pub freed_mb: u64,
}

type SharedSessions = Arc<Mutex<Vec<AgentSession>>>;
type ManualSessions = Arc<Mutex<HashMap<String, AgentSession>>>;
type DismissedSessions = Arc<Mutex<HashMap<String, u64>>>;
#[derive(Clone)]
struct AutoDiscoveredCache(Arc<Mutex<Vec<AgentSession>>>);
#[derive(Clone, Default)]
struct ApprovedPrompts(Arc<Mutex<HashMap<String, (u64, bool)>>>);
#[derive(Clone, Default)]
struct PausedSessions(Arc<Mutex<HashMap<String, u64>>>);
#[derive(Clone, Default)]
struct ResumedSessions(Arc<Mutex<HashMap<String, u64>>>);

fn paused_ids_file_path() -> Option<PathBuf> {
    get_user_profile_dir().map(|home| home.join(".dinobuds").join("paused-ids.json"))
}

fn load_paused_ids() -> HashMap<String, u64> {
    paused_ids_file_path()
        .and_then(|p| fs::read_to_string(p).ok())
        .and_then(|raw| serde_json::from_str::<HashMap<String, u64>>(&raw).ok())
        .unwrap_or_default()
}

fn save_paused_ids(map: &HashMap<String, u64>) {
    if let Some(path) = paused_ids_file_path() {
        if let Some(parent) = path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        if let Ok(json) = serde_json::to_string_pretty(map) {
            let _ = fs::write(&path, json);
        }
    }
}

fn status_rank(status: &str) -> u8 {
    match status {
        "waiting_permission" => 0,
        "running" => 1,
        "paused" => 2,
        "queued" => 3,
        _ => 4,
    }
}

fn reconcile_and_emit(
    shared: &SharedSessions,
    manual: &ManualSessions,
    dismissed: &DismissedSessions,
    approved_prompts: &ApprovedPrompts,
    paused_sessions: &PausedSessions,
    resumed_sessions: &ResumedSessions,
    auto_sessions: &[AgentSession],
    app: &AppHandle,
) -> Vec<AgentSession> {
    let now = now_secs();
    let manual_map = lock_or_recover(manual).clone();
    let mut merged: Vec<AgentSession> = Vec::new();
    let mut seen_ids: HashSet<String> = HashSet::new();

    for (id, session) in &manual_map {
        seen_ids.insert(id.clone());
        merged.push(session.clone());
    }

    {
        let mut dismissed_map = lock_or_recover(dismissed);
        let mut approved_map = lock_or_recover(&approved_prompts.0);
        let mut paused_map = lock_or_recover(&paused_sessions.0);
        let mut paused_dirty = false;
        let mut resumed_map = lock_or_recover(&resumed_sessions.0);

        for raw_session in auto_sessions {
            let mut session = raw_session.clone();
            if seen_ids.contains(&session.id) {
                continue;
            }
            if let Some(&dismissed_at) = dismissed_map.get(&session.id) {
                if session.updated_at <= dismissed_at {
                    continue;
                }
                dismissed_map.remove(&session.id);
            }

            if let Some(&paused_at) = paused_map.get(&session.id) {
                if session.status == "done" {
                    paused_map.remove(&session.id);
                    paused_dirty = true;
                } else if session.updated_at > paused_at + 10
                    && now.saturating_sub(session.updated_at) <= 20
                {
                    paused_map.remove(&session.id);
                    paused_dirty = true;
                } else {
                    session.status = "paused".to_string();
                    session.question = None;
                    session.decision = None;
                    session.step_label = if session.total_steps > 0 {
                        format!(
                            "Paused at checkpoint ({}/{})",
                            session.current_step, session.total_steps
                        )
                    } else {
                        "Paused".to_string()
                    };
                }
            } else if let Some(&resumed_at) = resumed_map.get(&session.id) {
                if now.saturating_sub(resumed_at) <= 25 && session.status == "paused" {
                    session.status = "running".to_string();
                    session.question = None;
                    session.step_label = "Resuming checkpoint...".to_string();
                } else if now.saturating_sub(resumed_at) > 25 || session.status == "done" {
                    resumed_map.remove(&session.id);
                }
            }

            if session.status != "paused" {
                if let Some(&(action_ts, was_approved)) = approved_map.get(&session.id) {
                    if was_approved {
                        if now.saturating_sub(action_ts) <= 4
                            && session.status == "waiting_permission"
                        {
                            session.status = "running".to_string();
                            session.question = None;
                            session.decision = Some(true);
                            session.step_label = "Submitted ⏎".to_string();
                        } else if now.saturating_sub(action_ts) > 4 {
                            approved_map.remove(&session.id);
                        }
                    } else if session.updated_at <= action_ts
                        && session.status == "waiting_permission"
                    {
                        session.status = "done".to_string();
                        session.question = None;
                        session.decision = Some(false);
                        session.step_label = "Skipped".to_string();
                    } else if session.updated_at > action_ts {
                        approved_map.remove(&session.id);
                    }
                }
            }

            seen_ids.insert(session.id.clone());
            merged.push(session);
        }

        if paused_dirty {
            save_paused_ids(&paused_map);
        }
    }

    merged.sort_by(|a, b| {
        status_rank(&a.status)
            .cmp(&status_rank(&b.status))
            .then_with(|| b.updated_at.cmp(&a.updated_at))
            .then_with(|| a.name.cmp(&b.name))
    });

    if merged.len() > 24 {
        merged.truncate(24);
    }

    let changed = {
        let mut current = lock_or_recover(shared);
        if *current != merged {
            *current = merged.clone();
            true
        } else {
            false
        }
    };

    if changed {
        let _ = app.emit("dinobuds-update", &merged);
    }

    merged
}

fn position_window_top_right(window: &tauri::WebviewWindow, width: u32) -> Result<(), String> {
    if let Some(monitor) = window.primary_monitor().map_err(|e| e.to_string())? {
        let scale = monitor.scale_factor();
        let monitor_logical_width = (monitor.size().width as f64) / scale;
        let x = (monitor_logical_width - (width as f64) - 20.0).max(0.0);
        let y = 20.0;
        window
            .set_position(Position::Logical(LogicalPosition::new(x, y)))
            .map_err(|e| e.to_string())?;
    }
    Ok(())
}

#[tauri::command]
fn snap_to_top_right(window: tauri::WebviewWindow, width: u32) -> Result<(), String> {
    position_window_top_right(&window, width)
}

#[tauri::command]
fn get_sessions(state: State<'_, SharedSessions>) -> Vec<AgentSession> {
    lock_or_recover(&state).clone()
}

#[tauri::command]
fn resolve_permission(
    id: String,
    approved: bool,
    state: State<'_, SharedSessions>,
    manual: State<'_, ManualSessions>,
    paused_sessions: State<'_, PausedSessions>,
    app: AppHandle,
) -> Vec<AgentSession> {
    {
        let mut paused_map = lock_or_recover(&paused_sessions.0);
        if paused_map.remove(&id).is_some() {
            save_paused_ids(&paused_map);
        }
    }
    {
        let mut manual_map = lock_or_recover(&manual);
        if let Some(session) = manual_map.get_mut(&id) {
            session.decision = Some(approved);
            if approved {
                session.status = "running".to_string();
                session.task = "Permission approved — continuing execution...".to_string();
                session.question = None;
                if session.total_steps > 0 && session.current_step < session.total_steps {
                    session.current_step += 1;
                }
            } else {
                session.status = "done".to_string();
                session.task = "Action denied by user".to_string();
                session.question = None;
                session.step_label = "Denied".to_string();
            }
            session.updated_at = now_secs();
        }
    }

    let updated_list = {
        let mut sessions = lock_or_recover(&state);
        if let Some(session) = sessions.iter_mut().find(|s| s.id == id) {
            session.decision = Some(approved);
            if approved {
                session.status = "running".to_string();
                session.task = "Permission approved — continuing execution...".to_string();
                session.question = None;
                if session.total_steps > 0 && session.current_step < session.total_steps {
                    session.current_step += 1;
                }
            } else {
                session.status = "done".to_string();
                session.task = "Action denied by user".to_string();
                session.question = None;
                session.step_label = "Denied".to_string();
            }
            session.updated_at = now_secs();
        }
        sessions.clone()
    };

    let _ = app.emit("dinobuds-update", &updated_list);
    updated_list
}

#[tauri::command]
fn submit_agent_prompt(
    id: String,
    option: Option<u8>,
    approved: bool,
    state: State<'_, SharedSessions>,
    manual: State<'_, ManualSessions>,
    approved_prompts: State<'_, ApprovedPrompts>,
    paused_sessions: State<'_, PausedSessions>,
    app: AppHandle,
) -> Vec<AgentSession> {
    let action_ts = now_secs();
    {
        let mut paused_map = lock_or_recover(&paused_sessions.0);
        if paused_map.remove(&id).is_some() {
            save_paused_ids(&paused_map);
        }
    }
    {
        let mut approved_map = lock_or_recover(&approved_prompts.0);
        approved_map.insert(id.clone(), (action_ts, approved));
    }

    {
        let mut manual_map = lock_or_recover(&manual);
        if let Some(session) = manual_map.get_mut(&id) {
            session.decision = Some(approved);
            session.question = None;
            session.updated_at = action_ts;
            if approved {
                session.status = "running".to_string();
                session.step_label = "Submitted ⏎".to_string();
            } else {
                session.status = "done".to_string();
                session.step_label = "Skipped".to_string();
            }
        }
    }

    let mut workspace_hint: Option<String> = None;
    let updated_list = {
        let mut sessions = lock_or_recover(&state);
        if let Some(session) = sessions.iter_mut().find(|s| s.id == id) {
            workspace_hint = session.workspace_path.clone();
            session.decision = Some(approved);
            session.question = None;
            session.updated_at = action_ts;
            if approved {
                session.status = "running".to_string();
                session.step_label = "Submitted ⏎".to_string();
            } else {
                session.status = "done".to_string();
                session.step_label = "Skipped".to_string();
            }
        }
        sessions.sort_by(|a, b| {
            status_rank(&a.status)
                .cmp(&status_rank(&b.status))
                .then_with(|| b.updated_at.cmp(&a.updated_at))
                .then_with(|| a.name.cmp(&b.name))
        });
        sessions.clone()
    };

    let _ = app.emit("dinobuds-update", &updated_list);

    #[cfg(target_os = "windows")]
    {
        let opt_str = option.map(|o| o.to_string()).unwrap_or_default();
        let approved_str = if approved { "1" } else { "0" };
        let ws_hint = workspace_hint
            .and_then(|p| {
                Path::new(&p)
                    .file_name()
                    .and_then(|n| n.to_str())
                    .map(|s| s.to_string())
            })
            .unwrap_or_default();

        std::thread::spawn(move || {
            let script = r#"
Start-Sleep -Milliseconds 80
Add-Type -AssemblyName UIAutomationClient
Add-Type -AssemblyName UIAutomationTypes
Add-Type -TypeDefinition @"
using System;
using System.Runtime.InteropServices;
public static class DinoUiaMouse {
    [StructLayout(LayoutKind.Sequential)]
    public struct POINT { public int X; public int Y; }
    [DllImport("user32.dll")] public static extern bool GetCursorPos(out POINT lpPoint);
    [DllImport("user32.dll")] public static extern bool SetCursorPos(int X, int Y);
    [DllImport("user32.dll")] public static extern void mouse_event(uint dwFlags, int dx, int dy, uint dwData, UIntPtr dwExtraInfo);
    [DllImport("user32.dll")] public static extern bool SetForegroundWindow(IntPtr hWnd);
    [DllImport("user32.dll")] public static extern bool ShowWindowAsync(IntPtr hWnd, int nCmdShow);
    [DllImport("user32.dll")] public static extern bool IsIconic(IntPtr hWnd);
    public static void ClickAt(int x, int y) {
        POINT orig;
        bool hasOrig = GetCursorPos(out orig);
        SetCursorPos(x, y);
        mouse_event(0x0002, 0, 0, 0, UIntPtr.Zero);
        mouse_event(0x0004, 0, 0, 0, UIntPtr.Zero);
        if (hasOrig) { SetCursorPos(orig.X, orig.Y); }
    }
}
"@

$hint = $env:DINOBUDS_WS_HINT
$opt = $env:DINOBUDS_OPTION
$approved = $env:DINOBUDS_APPROVED -eq "1"

$proc = $null
if ($hint) {
    $proc = Get-Process | Where-Object {
        $_.MainWindowHandle -ne 0 -and $_.MainWindowTitle -like "*Antigravity*" -and $_.MainWindowTitle -like "*$hint*"
    } | Select-Object -First 1
}
if (-not $proc) {
    $proc = Get-Process | Where-Object {
        $_.MainWindowHandle -ne 0 -and ($_.ProcessName -match "Antigravity" -or $_.MainWindowTitle -like "*Antigravity*")
    } | Select-Object -First 1
}
if (-not $proc -or $proc.MainWindowHandle -eq 0) { exit 0 }

$hwnd = $proc.MainWindowHandle
if ([DinoUiaMouse]::IsIconic($hwnd)) {
    [void][DinoUiaMouse]::ShowWindowAsync($hwnd, 9)
    Start-Sleep -Milliseconds 60
}
[void][DinoUiaMouse]::SetForegroundWindow($hwnd)
Start-Sleep -Milliseconds 100

$root = [System.Windows.Automation.AutomationElement]::FromHandle($hwnd)
if (-not $root) { exit 0 }

function Invoke-OrClickElement($el) {
    if (-not $el) { return $false }
    try {
        $sel = $null
        if ($el.TryGetCurrentPattern([System.Windows.Automation.SelectionItemPattern]::Pattern, [ref]$sel)) {
            ([System.Windows.Automation.SelectionItemPattern]$sel).Select()
            return $true
        }
    } catch {}
    try {
        $inv = $null
        if ($el.TryGetCurrentPattern([System.Windows.Automation.InvokePattern]::Pattern, [ref]$inv)) {
            ([System.Windows.Automation.InvokePattern]$inv).Invoke()
            return $true
        }
    } catch {}
    try {
        $rect = $el.Current.BoundingRectangle
        if ($rect.Width -gt 0 -and $rect.Height -gt 0 -and -not [double]::IsInfinity($rect.Left)) {
            $cx = [int][Math]::Round($rect.Left + $rect.Width / 2.0)
            $cy = [int][Math]::Round($rect.Top + $rect.Height / 2.0)
            [DinoUiaMouse]::ClickAt($cx, $cy)
            return $true
        }
    } catch {}
    return $false
}

function Find-MatchingElement($elements, [string[]]$phrases) {
    foreach ($phrase in $phrases) {
        foreach ($el in $elements) {
            try {
                $rawName = $el.Current.Name
                if (-not $rawName) { continue }
                $trimmed = $rawName.Trim()
                if ($trimmed.Length -eq 0 -or $trimmed.Length -gt 48) { continue }
                $lower = $trimmed.ToLowerInvariant()
                if ($lower.Contains($phrase)) {
                    return $el
                }
            } catch {}
        }
    }
    return $null
}

$all = $root.FindAll([System.Windows.Automation.TreeScope]::Descendants, [System.Windows.Automation.Condition]::TrueCondition)

if ($approved -and ($opt -eq "2" -or $opt -eq "3")) {
    $optTarget = $null
    if ($opt -eq "2") {
        $optTarget = Find-MatchingElement $all @("yes, and always allow", "always allow", "always", "2.")
    } else {
        foreach ($el in $all) {
            try {
                $name = $el.Current.Name
                if (-not $name -or $name.Length -gt 48) { continue }
                $lower = $name.ToLowerInvariant()
                if (($lower.StartsWith("yes, and ") -and -not $lower.Contains("always")) -or $name -eq "3" -or $name.StartsWith("3.")) {
                    $optTarget = $el
                    break
                }
            } catch {}
        }
    }
    if ($optTarget) {
        [void](Invoke-OrClickElement $optTarget)
        Start-Sleep -Milliseconds 60
    }
}

$btnCond = New-Object System.Windows.Automation.PropertyCondition(
    [System.Windows.Automation.AutomationElement]::ControlTypeProperty,
    [System.Windows.Automation.ControlType]::Button
)
$buttons = $root.FindAll([System.Windows.Automation.TreeScope]::Descendants, $btnCond)

$targetPhrases = if ($approved -and $opt -eq "2") {
    @("always allow", "yes, and always", "submit", "allow", "approve", "run", "accept")
} elseif ($approved) {
    @("submit", "allow", "approve", "run", "accept", "continue", "yes")
} else {
    @("skip", "deny", "reject", "cancel")
}

$btnTarget = Find-MatchingElement $buttons $targetPhrases
if (-not $btnTarget) {
    $btnTarget = Find-MatchingElement $all $targetPhrases
}
if ($btnTarget) {
    [void](Invoke-OrClickElement $btnTarget)
}
"#;
            let _ = Command::new("powershell.exe")
                .args([
                    "-NoProfile",
                    "-NonInteractive",
                    "-WindowStyle",
                    "Hidden",
                    "-Command",
                    script,
                ])
                .env("DINOBUDS_WS_HINT", ws_hint)
                .env("DINOBUDS_OPTION", opt_str)
                .env("DINOBUDS_APPROVED", approved_str)
                .creation_flags(0x08000000)
                .spawn();
        });
    }

    updated_list
}

#[tauri::command]
fn dismiss_session(
    id: String,
    state: State<'_, SharedSessions>,
    manual: State<'_, ManualSessions>,
    dismissed: State<'_, DismissedSessions>,
    approved_prompts: State<'_, ApprovedPrompts>,
    paused_sessions: State<'_, PausedSessions>,
    resumed_sessions: State<'_, ResumedSessions>,
    auto_cache: State<'_, AutoDiscoveredCache>,
    app: AppHandle,
) -> Vec<AgentSession> {
    {
        let mut dismissed_map = lock_or_recover(&dismissed);
        dismissed_map.insert(id.clone(), now_secs());
    }
    {
        let mut manual_map = lock_or_recover(&manual);
        manual_map.remove(&id);
    }
    {
        let mut paused_map = lock_or_recover(&paused_sessions.0);
        if paused_map.remove(&id).is_some() {
            save_paused_ids(&paused_map);
        }
    }
    {
        let mut resumed_map = lock_or_recover(&resumed_sessions.0);
        resumed_map.remove(&id);
    }
    let cached_auto = lock_or_recover(&auto_cache.0).clone();
    reconcile_and_emit(
        &state,
        &manual,
        &dismissed,
        &approved_prompts,
        &paused_sessions,
        &resumed_sessions,
        &cached_auto,
        &app,
    )
}

fn resolve_axiom_workspace(workspace_hint: Option<&str>) -> Option<PathBuf> {
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Some(ws) = workspace_hint {
        candidates.push(PathBuf::from(ws));
    }
    if let Some(home) = get_user_profile_dir() {
        candidates.push(home.join("axiom"));
    }
    for cand in candidates {
        if cand.join("src").join("eval").join("ablation-v3.ts").is_file() {
            return Some(cand);
        }
    }
    None
}

fn extract_model_for_axiom_run(
    workspace_hint: Option<&str>,
    run_id: &str,
    task_text: &str,
) -> Option<String> {
    let mut dirs_to_check: Vec<PathBuf> = Vec::new();
    if let Some(ws) = workspace_hint {
        dirs_to_check.push(PathBuf::from(ws));
    }
    if let Some(home) = get_user_profile_dir() {
        dirs_to_check.push(home.join("axiom"));
    }

    for dir in dirs_to_check {
        let run_dir = dir.join(".axiom-state").join("runs-v3").join(run_id);
        for fname in ["checkpoint.json", "results.json"] {
            let p = run_dir.join(fname);
            if p.is_file() {
                if let Ok(mut f) = File::open(&p) {
                    let mut buf = vec![0u8; 16_384];
                    if let Ok(n) = f.read(&mut buf) {
                        buf.truncate(n);
                        let head = String::from_utf8_lossy(&buf);
                        if let Some(m) = extract_json_string_field(&head, "requestedModel")
                            .or_else(|| extract_json_string_field(&head, "model"))
                        {
                            if !m.trim().is_empty() {
                                return Some(m);
                            }
                        }
                    }
                }
            }
        }
    }

    if let Some((prefix, _)) = task_text.split_once(" — ") {
        let trimmed = prefix.trim();
        if !trimmed.is_empty() && !trimmed.contains(' ') && !trimmed.eq_ignore_ascii_case("completed") {
            return Some(trimmed.to_string());
        }
    }

    None
}

fn list_live_axiom_run_ids() -> HashSet<String> {
    let mut active_runs = HashSet::new();
    let mut sys = System::new();
    sys.refresh_processes_specifics(
        ProcessesToUpdate::All,
        true,
        ProcessRefreshKind::nothing().with_cmd(sysinfo::UpdateKind::OnlyIfNotSet),
    );
    for proc_info in sys.processes().values() {
        let name = proc_info.name().to_string_lossy().to_lowercase();
        if !name.contains("node") && !name.contains("cmd") {
            continue;
        }
        let args: Vec<String> = proc_info
            .cmd()
            .iter()
            .map(|s| s.to_string_lossy().to_string())
            .collect();
        let joined = args.join(" ").to_lowercase();
        if !joined.contains("ablation") || !joined.contains("--run-id") {
            continue;
        }
        for i in 0..args.len() {
            if args[i] == "--run-id" {
                if let Some(val) = args.get(i + 1) {
                    let clean = val.trim().trim_matches('"').to_lowercase();
                    if !clean.is_empty() {
                        active_runs.insert(clean);
                    }
                }
            } else if let Some(val) = args[i].strip_prefix("--run-id=") {
                let clean = val.trim().trim_matches('"').to_lowercase();
                if !clean.is_empty() {
                    active_runs.insert(clean);
                }
            }
        }
    }
    active_runs
}

fn has_live_axiom_process(run_id: &str) -> bool {
    let needle = run_id.trim().to_lowercase();
    if needle.is_empty() {
        return false;
    }
    list_live_axiom_run_ids().contains(&needle)
}

fn trigger_antigravity_resume_ui(ws_hint: Option<&str>) {
    #[cfg(target_os = "windows")]
    {
        let hint = ws_hint
            .and_then(|p| Path::new(p).file_name().and_then(|n| n.to_str()))
            .unwrap_or("")
            .to_string();

        std::thread::spawn(move || {
            let script = r#"
Start-Sleep -Milliseconds 60
Add-Type -AssemblyName UIAutomationClient
Add-Type -AssemblyName UIAutomationTypes
Add-Type -TypeDefinition @"
using System;
using System.Runtime.InteropServices;
public static class DinoResumeWin {
    [StructLayout(LayoutKind.Sequential)]
    public struct POINT { public int X; public int Y; }
    [DllImport("user32.dll")] public static extern bool GetCursorPos(out POINT lpPoint);
    [DllImport("user32.dll")] public static extern bool SetCursorPos(int X, int Y);
    [DllImport("user32.dll")] public static extern void mouse_event(uint dwFlags, int dx, int dy, uint dwData, UIntPtr dwExtraInfo);
    [DllImport("user32.dll")] public static extern bool SetForegroundWindow(IntPtr hWnd);
    [DllImport("user32.dll")] public static extern bool ShowWindowAsync(IntPtr hWnd, int nCmdShow);
    [DllImport("user32.dll")] public static extern bool IsIconic(IntPtr hWnd);
    public static void ClickAt(int x, int y) {
        POINT orig;
        bool hasOrig = GetCursorPos(out orig);
        SetCursorPos(x, y);
        mouse_event(0x0002, 0, 0, 0, UIntPtr.Zero);
        mouse_event(0x0004, 0, 0, 0, UIntPtr.Zero);
        if (hasOrig) { SetCursorPos(orig.X, orig.Y); }
    }
}
"@

$hint = $env:DINOBUDS_WS_HINT
$proc = $null
if ($hint) {
    $proc = Get-Process | Where-Object {
        $_.MainWindowHandle -ne 0 -and $_.MainWindowTitle -like "*Antigravity*" -and $_.MainWindowTitle -like "*$hint*"
    } | Select-Object -First 1
}
if (-not $proc) {
    $proc = Get-Process | Where-Object {
        $_.MainWindowHandle -ne 0 -and ($_.ProcessName -match "Antigravity" -or $_.MainWindowTitle -like "*Antigravity*")
    } | Select-Object -First 1
}
if (-not $proc -or $proc.MainWindowHandle -eq 0) { exit 0 }

$hwnd = $proc.MainWindowHandle
if ([DinoResumeWin]::IsIconic($hwnd)) {
    [void][DinoResumeWin]::ShowWindowAsync($hwnd, 9)
    Start-Sleep -Milliseconds 60
}
[void][DinoResumeWin]::SetForegroundWindow($hwnd)
Start-Sleep -Milliseconds 100

$root = [System.Windows.Automation.AutomationElement]::FromHandle($hwnd)
if (-not $root) { exit 0 }

$all = $root.FindAll([System.Windows.Automation.TreeScope]::Descendants, [System.Windows.Automation.Condition]::TrueCondition)
$phrases = @("submit", "allow", "accept", "continue", "retry", "run command", "run")
foreach ($phrase in $phrases) {
    foreach ($el in $all) {
        try {
            $ct = $el.Current.ControlType
            if ($ct -ne [System.Windows.Automation.ControlType]::Button) { continue }
            $rawName = $el.Current.Name
            if (-not $rawName) { continue }
            $trimmed = $rawName.Trim()
            if ($trimmed.Length -eq 0 -or $trimmed.Length -gt 36) { continue }
            if ($trimmed.ToLowerInvariant().Contains($phrase)) {
                try {
                    $inv = $null
                    if ($el.TryGetCurrentPattern([System.Windows.Automation.InvokePattern]::Pattern, [ref]$inv)) {
                        ([System.Windows.Automation.InvokePattern]$inv).Invoke()
                        exit 0
                    }
                } catch {}
                try {
                    $rect = $el.Current.BoundingRectangle
                    if ($rect.Width -gt 0 -and $rect.Height -gt 0 -and -not [double]::IsInfinity($rect.Left)) {
                        [DinoResumeWin]::ClickAt([int][Math]::Round($rect.Left + $rect.Width / 2.0), [int][Math]::Round($rect.Top + $rect.Height / 2.0))
                        exit 0
                    }
                } catch {}
            }
        } catch {}
    }
}
"#;
            let _ = Command::new("powershell.exe")
                .args([
                    "-NoProfile",
                    "-NonInteractive",
                    "-WindowStyle",
                    "Hidden",
                    "-Command",
                    script,
                ])
                .env("DINOBUDS_WS_HINT", hint)
                .creation_flags(0x08000000)
                .spawn();
        });
    }
}

#[tauri::command]
fn toggle_pause_session(
    id: String,
    state: State<'_, SharedSessions>,
    manual: State<'_, ManualSessions>,
    dismissed: State<'_, DismissedSessions>,
    approved_prompts: State<'_, ApprovedPrompts>,
    paused_sessions: State<'_, PausedSessions>,
    resumed_sessions: State<'_, ResumedSessions>,
    auto_cache: State<'_, AutoDiscoveredCache>,
    app: AppHandle,
) -> Vec<AgentSession> {
    let now = now_secs();
    let existing_session = {
        let cur = lock_or_recover(&state);
        cur.iter().find(|s| s.id == id).cloned()
    }
    .or_else(|| {
        let cache = lock_or_recover(&auto_cache.0);
        cache.iter().find(|s| s.id == id).cloned()
    })
    .or_else(|| lock_or_recover(&manual).get(&id).cloned());

    let is_currently_paused = {
        let paused_map = lock_or_recover(&paused_sessions.0);
        paused_map.contains_key(&id)
            || existing_session
                .as_ref()
                .map(|s| s.status == "paused")
                .unwrap_or(false)
    };

    if is_currently_paused {
        {
            let mut paused_map = lock_or_recover(&paused_sessions.0);
            if paused_map.remove(&id).is_some() {
                save_paused_ids(&paused_map);
            }
        }
        {
            let mut resumed_map = lock_or_recover(&resumed_sessions.0);
            resumed_map.insert(id.clone(), now);
        }
        {
            let mut manual_map = lock_or_recover(&manual);
            if let Some(session) = manual_map.get_mut(&id) {
                session.status = "running".to_string();
                session.question = None;
                session.step_label = "Resuming checkpoint...".to_string();
                session.updated_at = now;
            }
        }

        let mut launched_axiom = false;
        if id.starts_with("axiom-") {
            if let Some(ref session) = existing_session {
                let run_id = session.name.clone();
                let ws_hint = session.workspace_path.as_deref();
                let model_opt = extract_model_for_axiom_run(ws_hint, &run_id, &session.task);
                let axiom_ws_opt = resolve_axiom_workspace(ws_hint);

                #[cfg(target_os = "windows")]
                if let (Some(model), Some(axiom_ws)) = (model_opt, axiom_ws_opt) {
                    launched_axiom = true;
                    let resumed_ref = resumed_sessions.0.clone();
                    let session_id = id.clone();
                    std::thread::spawn(move || {
                        if has_live_axiom_process(&run_id) {
                            return;
                        }
                        let log_path = get_user_profile_dir()
                            .map(|h| h.join(".dinobuds").join("logs").join(format!("axiom-{}.log", sanitize_id(&run_id))));
                        let mut cmd = Command::new("cmd.exe");
                        cmd.args([
                            "/C",
                            "npx.cmd",
                            "tsx",
                            "src/eval/ablation-v3.ts",
                            "--model",
                            &model,
                            "--run-id",
                            &run_id,
                            "--allow-dirty",
                        ])
                        .current_dir(&axiom_ws)
                        .creation_flags(0x08000000);

                        if let Some(ref lp) = log_path {
                            if let Some(parent) = lp.parent() {
                                let _ = fs::create_dir_all(parent);
                            }
                            if let Ok(file) = File::create(lp) {
                                if let Ok(err_file) = file.try_clone() {
                                    cmd.stdout(std::process::Stdio::from(file));
                                    cmd.stderr(std::process::Stdio::from(err_file));
                                }
                            }
                        }

                        if let Ok(mut child) = cmd.spawn() {
                            let _ = child.wait();
                            let mut rmap = lock_or_recover(&resumed_ref);
                            rmap.remove(&session_id);
                        } else {
                            let mut rmap = lock_or_recover(&resumed_ref);
                            rmap.remove(&session_id);
                        }
                    });
                }
            }
        }

        if !launched_axiom {
            let ws_hint = existing_session
                .as_ref()
                .and_then(|s| s.workspace_path.as_deref());
            trigger_antigravity_resume_ui(ws_hint);
        }
    } else {
        {
            let mut resumed_map = lock_or_recover(&resumed_sessions.0);
            resumed_map.remove(&id);
        }
        {
            let mut approved_map = lock_or_recover(&approved_prompts.0);
            approved_map.remove(&id);
        }
        {
            let mut paused_map = lock_or_recover(&paused_sessions.0);
            paused_map.insert(id.clone(), now);
            save_paused_ids(&paused_map);
        }
        {
            let mut manual_map = lock_or_recover(&manual);
            if let Some(session) = manual_map.get_mut(&id) {
                session.status = "paused".to_string();
                session.question = None;
                session.decision = None;
                session.step_label = if session.total_steps > 0 {
                    format!(
                        "Paused at checkpoint ({}/{})",
                        session.current_step, session.total_steps
                    )
                } else {
                    "Paused".to_string()
                };
                session.updated_at = now;
            }
        }

        if id.starts_with("axiom-") {
            let run_id = existing_session
                .as_ref()
                .map(|s| s.name.clone())
                .unwrap_or_else(|| id.trim_start_matches("axiom-").to_string());

            #[cfg(target_os = "windows")]
            if !run_id.is_empty() {
                std::thread::spawn(move || {
                    let script = r#"
$runId = $env:DINOBUDS_RUN_ID
if (-not $runId) { exit 0 }
Get-CimInstance Win32_Process -Filter "Name = 'node.exe' OR Name = 'cmd.exe'" | Where-Object {
    $_.CommandLine -and $_.CommandLine -like "*--run-id*$runId*"
} | ForEach-Object {
    Stop-Process -Id $_.ProcessId -Force -ErrorAction SilentlyContinue
}
"#;
                    let _ = Command::new("powershell.exe")
                        .args([
                            "-NoProfile",
                            "-NonInteractive",
                            "-WindowStyle",
                            "Hidden",
                            "-Command",
                            script,
                        ])
                        .env("DINOBUDS_RUN_ID", run_id)
                        .creation_flags(0x08000000)
                        .spawn();
                });
            }
        }
    }

    let cached_auto = lock_or_recover(&auto_cache.0).clone();
    reconcile_and_emit(
        &state,
        &manual,
        &dismissed,
        &approved_prompts,
        &paused_sessions,
        &resumed_sessions,
        &cached_auto,
        &app,
    )
}

#[tauri::command]
fn focus_workspace(path: String) -> Result<(), String> {
    println!("[DinoBuds] Focusing workspace: {}", path);
    #[cfg(target_os = "windows")]
    {
        if !path.trim().is_empty() {
            let _ = Command::new("cmd")
                .args(["/C", "start", "", &path])
                .spawn()
                .map_err(|e| e.to_string())?;
        }
    }
    Ok(())
}

fn is_dev_process_name(name_lower: &str) -> bool {
    let targets = [
        "node",
        "node.exe",
        "tsserver",
        "next-server",
        "vite",
        "esbuild",
        "esbuild.exe",
        "cargo",
        "cargo.exe",
        "rust-analyzer",
        "rust-analyzer.exe",
        "bun",
        "bun.exe",
        "deno",
        "deno.exe",
    ];
    targets.iter().any(|t| name_lower == *t || name_lower.contains(t))
}

#[tauri::command]
fn get_system_health() -> SystemHealth {
    let mut sys = System::new();
    sys.refresh_memory();
    sys.refresh_processes_specifics(
        ProcessesToUpdate::All,
        true,
        ProcessRefreshKind::nothing().with_memory(),
    );

    let total_bytes = sys.total_memory();
    let used_bytes = sys.used_memory();
    let total_gb = (total_bytes as f64) / 1_073_741_824.0;
    let used_gb = (used_bytes as f64) / 1_073_741_824.0;
    let usage_percent = if total_bytes > 0 {
        ((used_bytes as f64 / total_bytes as f64) * 100.0).round() as u32
    } else {
        0
    };

    let current_pid = std::process::id();
    let mut processes: Vec<HeavyProcess> = sys
        .processes()
        .iter()
        .filter_map(|(pid, proc_info)| {
            let pid_u32 = pid.as_u32();
            if pid_u32 == current_pid {
                return None;
            }
            let raw_name = proc_info.name().to_string_lossy().to_string();
            let lower = raw_name.to_lowercase();
            let mem_mb = proc_info.memory() / (1024 * 1024);
            if mem_mb >= 25 && is_dev_process_name(&lower) {
                Some(HeavyProcess {
                    pid: pid_u32,
                    name: raw_name,
                    memory_mb: mem_mb,
                })
            } else {
                None
            }
        })
        .collect();

    processes.sort_by(|a, b| b.memory_mb.cmp(&a.memory_mb));
    processes.truncate(8);

    SystemHealth {
        used_gb: (used_gb * 10.0).round() / 10.0,
        total_gb: (total_gb * 10.0).round() / 10.0,
        usage_percent,
        processes,
    }
}

#[tauri::command]
fn kill_process(pid: u32) -> Result<u64, String> {
    let current_pid = std::process::id();
    if pid == current_pid {
        return Err("Cannot kill DinoBuds itself".to_string());
    }

    let mut sys = System::new();
    sys.refresh_processes_specifics(
        ProcessesToUpdate::All,
        true,
        ProcessRefreshKind::nothing().with_memory(),
    );

    let target_pid = Pid::from_u32(pid);
    if let Some(process) = sys.process(target_pid) {
        let mem_mb = process.memory() / (1024 * 1024);
        if process.kill() {
            Ok(mem_mb)
        } else {
            Err(format!("Failed to terminate PID {}", pid))
        }
    } else {
        Err(format!("Process {} not found", pid))
    }
}

#[tauri::command]
fn sweep_orphan_node() -> SweepResult {
    let current_pid = std::process::id();
    let mut sys = System::new();
    sys.refresh_processes_specifics(
        ProcessesToUpdate::All,
        true,
        ProcessRefreshKind::nothing().with_memory(),
    );

    let mut killed_count = 0usize;
    let mut freed_mb = 0u64;

    for (pid, process) in sys.processes() {
        let pid_u32 = pid.as_u32();
        if pid_u32 == current_pid {
            continue;
        }
        let name_lower = process.name().to_string_lossy().to_lowercase();
        let mem_mb = process.memory() / (1024 * 1024);

        let is_target = name_lower == "node.exe"
            || name_lower == "node"
            || name_lower.contains("tsserver")
            || name_lower.contains("esbuild");

        if is_target && mem_mb >= 120 && process.kill() {
            killed_count += 1;
            freed_mb += mem_mb;
        }
    }

    SweepResult {
        killed_count,
        freed_mb,
    }
}

// ============================================================================
// Auto-Discovery Watcher (Antigravity brain/ & Axiom Benchmark Runs)
// ============================================================================

fn get_user_profile_dir() -> Option<PathBuf> {
    std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(PathBuf::from)
}

fn file_mtime_secs(path: &Path) -> u64 {
    fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn read_tail_text(path: &Path, max_bytes: u64) -> Option<String> {
    let mut file = File::open(path).ok()?;
    let len = file.metadata().ok()?.len();
    let start = len.saturating_sub(max_bytes);
    if start > 0 {
        file.seek(SeekFrom::Start(start)).ok()?;
    }
    let mut buf = Vec::new();
    file.read_to_end(&mut buf).ok()?;
    let mut text = String::from_utf8_lossy(&buf).into_owned();
    if start > 0 {
        if let Some(idx) = text.find('\n') {
            text = text[idx + 1..].to_string();
        }
    }
    Some(text)
}

fn read_head_and_tail(path: &Path, head_bytes: u64, tail_bytes: u64) -> Option<(String, String)> {
    let mut file = File::open(path).ok()?;
    let len = file.metadata().ok()?.len();
    if len <= head_bytes {
        let mut buf = Vec::new();
        file.read_to_end(&mut buf).ok()?;
        let text = String::from_utf8_lossy(&buf).into_owned();
        return Some((text.clone(), text));
    }

    let mut head_buf = vec![0u8; head_bytes as usize];
    let n = file.read(&mut head_buf).ok()?;
    head_buf.truncate(n);
    let head = String::from_utf8_lossy(&head_buf).into_owned();

    let tail = read_tail_text(path, tail_bytes).unwrap_or_default();
    Some((head, tail))
}

fn strip_ansi(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            if chars.peek() == Some(&'[') {
                chars.next();
                while let Some(next_c) = chars.next() {
                    if next_c.is_ascii_alphabetic() {
                        break;
                    }
                }
            }
        } else if !c.is_control() || c == '\t' {
            out.push(c);
        }
    }
    out
}

fn truncate_str(s: &str, max_chars: usize) -> String {
    let count = s.chars().count();
    if count <= max_chars {
        return s.to_string();
    }
    let truncated: String = s.chars().take(max_chars.saturating_sub(1)).collect();
    format!("{}…", truncated.trim_end())
}

fn extract_uuid(s: &str) -> Option<String> {
    let bytes = s.as_bytes();
    if bytes.len() < 36 {
        return None;
    }
    for i in 0..=bytes.len() - 36 {
        if i > 0 && bytes[i - 1].is_ascii_hexdigit() {
            continue;
        }
        if i + 36 < bytes.len() && bytes[i + 36].is_ascii_hexdigit() {
            continue;
        }
        let slice = &bytes[i..i + 36];
        let valid = slice.iter().enumerate().all(|(idx, &b)| match idx {
            8 | 13 | 18 | 23 => b == b'-',
            _ => b.is_ascii_hexdigit(),
        });
        if valid {
            return String::from_utf8(slice.to_vec()).ok();
        }
    }
    None
}

fn unquote_json_arg(val: &Value) -> Option<String> {
    let s = val.as_str()?.trim();
    if s.starts_with('"') && s.ends_with('"') && s.len() >= 2 {
        serde_json::from_str::<String>(s).ok()
    } else {
        Some(s.to_string())
    }
}

fn extract_workspace_from_text(text: &str) -> Option<PathBuf> {
    let mut normalized = text.replace("\\\\\\\\", "\\");
    normalized = normalized.replace("\\\\", "\\");
    normalized = normalized.replace('/', "\\");

    // ASCII lowercase preserves byte offsets 1:1 with `normalized`
    let lower = normalized.to_ascii_lowercase();
    let marker = ":\\users\\";
    let skip_dirs = [
        ".gemini",
        "appdata",
        ".cargo",
        ".npm",
        ".vscode",
        ".antigravity",
        "node_modules",
        ".git",
        "temp",
        "tmp",
        ".cache",
        "local settings",
        "application data",
    ];
    let parent_containers = [
        "desktop",
        "documents",
        "projects",
        "code",
        "dev",
        "repos",
        "workspace",
        "workspaces",
        "src",
    ];

    let mut counts: HashMap<String, (PathBuf, usize)> = HashMap::new();
    let mut search_from = 0;

    while let Some(haystack) = lower.get(search_from..) {
        let Some(pos) = haystack.find(marker) else {
            break;
        };
        let abs_pos = search_from + pos;
        search_from = abs_pos + marker.len();

        if abs_pos == 0 {
            continue;
        }
        let Some(&drive_byte) = normalized.as_bytes().get(abs_pos - 1) else {
            continue;
        };
        if !drive_byte.is_ascii_alphabetic() {
            continue;
        }
        let Some(rest) = normalized.get(abs_pos + marker.len()..) else {
            continue;
        };
        let Some(user_end) = rest.find('\\') else {
            continue;
        };
        let Some(username) = rest.get(..user_end) else {
            continue;
        };
        if username.is_empty()
            || username.contains(|c: char| !c.is_alphanumeric() && c != '_' && c != '-' && c != '.')
        {
            continue;
        }

        let Some(after_user) = rest.get(user_end + 1..) else {
            continue;
        };
        let seg1_end = after_user
            .find(|c: char| {
                c == '\\' || c == '"' || c == '\'' || c == ' ' || c == '\n' || c == '\r' || c == ',' || c == ')' || c == ']'
            })
            .unwrap_or(after_user.len());
        let Some(seg1_raw) = after_user.get(..seg1_end) else {
            continue;
        };
        let seg1 = seg1_raw.trim();
        if seg1.is_empty() {
            continue;
        }
        let seg1_lower = seg1.to_lowercase();
        if skip_dirs.contains(&seg1_lower.as_str()) {
            continue;
        }

        let mut candidate =
            PathBuf::from(format!("{}:\\Users\\{}\\{}", drive_byte as char, username, seg1));

        if parent_containers.contains(&seg1_lower.as_str())
            && after_user.as_bytes().get(seg1_end) == Some(&b'\\')
        {
            if let Some(after_seg1) = after_user.get(seg1_end + 1..) {
                let seg2_end = after_seg1
                    .find(|c: char| {
                        c == '\\' || c == '"' || c == '\'' || c == ' ' || c == '\n' || c == '\r' || c == ',' || c == ')' || c == ']'
                    })
                    .unwrap_or(after_seg1.len());
                if let Some(seg2_raw) = after_seg1.get(..seg2_end) {
                    let seg2 = seg2_raw.trim();
                    if !seg2.is_empty() && !skip_dirs.contains(&seg2.to_lowercase().as_str()) {
                        let deeper = candidate.join(seg2);
                        if deeper.is_dir() {
                            candidate = deeper;
                        }
                    }
                }
            }
        }

        if candidate.is_dir() {
            let key = candidate.to_string_lossy().to_lowercase();
            counts
                .entry(key)
                .and_modify(|e| e.1 += 1)
                .or_insert((candidate, 1));
        }
    }

    if let Some((path, _)) = counts.into_values().max_by_key(|(_, count)| *count) {
        return Some(path);
    }

    if lower.contains(".axiom-state")
        || lower.contains("ablation-v3")
        || lower.contains("realtime-research-daemon")
    {
        if let Some(axiom) = get_user_profile_dir().map(|p| p.join("axiom")) {
            if axiom.is_dir() {
                return Some(axiom);
            }
        }
    }

    None
}

#[derive(Debug, Clone)]
struct TaskMdInfo {
    title: Option<String>,
    completed: u32,
    in_progress: u32,
    total: u32,
    active_label: Option<String>,
}

fn parse_task_md(path: &Path) -> Option<TaskMdInfo> {
    let mut file = File::open(path).ok()?;
    let mut buf = vec![0u8; 65536];
    let n = file.read(&mut buf).ok()?;
    buf.truncate(n);
    let content = String::from_utf8_lossy(&buf);

    let mut title: Option<String> = None;
    let mut completed = 0u32;
    let mut in_progress = 0u32;
    let mut pending = 0u32;
    let mut active_label: Option<String> = None;

    for raw_line in content.lines() {
        let line = raw_line.trim();
        if title.is_none() && line.starts_with("# ") {
            let cleaned = line
                .trim_start_matches('#')
                .trim()
                .trim_matches('*')
                .trim_matches('`')
                .trim();
            if !cleaned.is_empty() {
                title = Some(cleaned.to_string());
            }
        }

        let item_body = if let Some(rest) = line
            .strip_prefix("- [x]")
            .or_else(|| line.strip_prefix("- [X]"))
            .or_else(|| line.strip_prefix("* [x]"))
            .or_else(|| line.strip_prefix("* [X]"))
        {
            completed += 1;
            let _ = rest;
            None
        } else if let Some(rest) = line
            .strip_prefix("- [/]")
            .or_else(|| line.strip_prefix("* [/]"))
        {
            in_progress += 1;
            Some(rest.trim())
        } else if line.starts_with("- [ ]") || line.starts_with("* [ ]") {
            pending += 1;
            None
        } else {
            None
        };

        if active_label.is_none() {
            if let Some(body) = item_body {
                let cleaned = body.trim_matches('*').trim_matches('`').trim();
                if !cleaned.is_empty() {
                    active_label = Some(truncate_str(cleaned, 48));
                }
            }
        }
    }

    let total = completed + in_progress + pending;
    Some(TaskMdInfo {
        title,
        completed,
        in_progress,
        total,
        active_label,
    })
}

#[derive(Debug, Clone)]
struct BenchmarkRecord {
    name: String,
    current_step: u32,
    total_steps: u32,
    step_label: String,
    task: String,
    status: String,
    question: Option<String>,
    linked_uuid: Option<String>,
    extra_uuids: Vec<String>,
    workspace_path: Option<String>,
    updated_at: u64,
    priority_source: u8, // 2 = disk/task-log, 1 = axiom transcript
}

fn sanitize_id(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect()
}

fn strip_leading_tags(mut s: &str) -> &str {
    s = s.trim();
    loop {
        let trimmed = s
            .trim_start_matches(|c: char| c == '-' || c == '*' || c == '•' || c == '>' || c.is_whitespace());
        if trimmed.starts_with('[') {
            if let Some(close) = trimmed.find(']') {
                let after = trimmed[close + 1..].trim();
                if !after.is_empty() {
                    s = after;
                    continue;
                }
            }
        }
        s = trimmed;
        break;
    }
    s
}

fn parse_benchmark_line(
    raw_line: &str,
    timestamp: u64,
    workspace_path: Option<&str>,
    priority_source: u8,
) -> Option<BenchmarkRecord> {
    let clean = strip_ansi(raw_line);
    if clean.len() > 600 || clean.trim().is_empty() {
        return None;
    }

    let lower_full = clean.to_lowercase();
    if lower_full.contains("http://") || lower_full.contains("https://") {
        return None;
    }

    let bytes = clean.as_bytes();
    for slash_idx in 0..bytes.len() {
        if bytes[slash_idx] != b'/' {
            continue;
        }

        // Read current digits before '/'
        let mut cur_end = slash_idx;
        while cur_end > 0 && bytes[cur_end - 1] == b' ' {
            cur_end -= 1;
        }
        let mut cur_start = cur_end;
        while cur_start > 0 && bytes[cur_start - 1].is_ascii_digit() {
            cur_start -= 1;
        }
        if cur_start == cur_end || cur_end - cur_start > 7 {
            continue;
        }

        // Check boundary before current number
        if cur_start > 0 {
            let prev = bytes[cur_start - 1] as char;
            if !prev.is_whitespace() && !matches!(prev, ':' | '(' | '[' | '|' | '=') {
                continue;
            }
        }

        // Read total digits after '/'
        let mut tot_start = slash_idx + 1;
        while tot_start < bytes.len() && bytes[tot_start] == b' ' {
            tot_start += 1;
        }
        let mut tot_end = tot_start;
        while tot_end < bytes.len() && bytes[tot_end].is_ascii_digit() {
            tot_end += 1;
        }
        if tot_start == tot_end || tot_end - tot_start > 7 {
            continue;
        }

        // Check boundary after total number (rejects dates like 11/15/2026)
        if tot_end < bytes.len() {
            let next = bytes[tot_end] as char;
            if !next.is_whitespace()
                && !matches!(next, ')' | ']' | ',' | '.' | '|' | ';' | '-' | '%')
            {
                continue;
            }
        }

        let Ok(current) = clean[cur_start..cur_end].parse::<u32>() else {
            continue;
        };
        let Ok(total) = clean[tot_start..tot_end].parse::<u32>() else {
            continue;
        };
        if total < 10 || current > total || total > 1_000_000 {
            continue;
        }

        // Extract run name from prefix
        let raw_prefix = clean[..cur_start]
            .trim_end_matches(|c: char| c.is_whitespace() || matches!(c, ':' | '=' | '|' | '-'));
        let prefix = strip_leading_tags(raw_prefix);
        if prefix.is_empty() {
            continue;
        }

        let (name_part, detail) = if let Some(paren_idx) = prefix.rfind('(') {
            if prefix.ends_with(')') && paren_idx > 1 {
                let d = prefix[paren_idx + 1..prefix.len() - 1].trim();
                let n = prefix[..paren_idx].trim();
                (n, if d.is_empty() { None } else { Some(d.to_string()) })
            } else {
                (prefix, None)
            }
        } else {
            (prefix, None)
        };

        let cleaned_name = name_part
            .trim_matches(|c: char| matches!(c, '"' | '\'' | '`' | '*' | '[' | ']' | '|'))
            .trim();
        let word_count = cleaned_name.split_whitespace().count();
        let run_name = if word_count == 0 || word_count > 4 {
            continue;
        } else if word_count > 1 {
            // If multi-word like "Model glm5-v3", prefer last token with '-' or digits
            let last = cleaned_name.split_whitespace().last().unwrap_or("");
            if last.contains('-') || last.contains('_') || last.chars().any(|c| c.is_ascii_digit())
            {
                last.to_string()
            } else if cleaned_name.len() <= 28 {
                cleaned_name.to_string()
            } else {
                continue;
            }
        } else {
            cleaned_name.to_string()
        };

        if run_name.len() < 2
            || run_name.len() > 48
            || run_name.contains('\\')
            || !run_name.chars().any(|c| c.is_ascii_alphabetic())
        {
            continue;
        }

        let run_lower = run_name.to_lowercase();
        let stop_names = [
            "step", "steps", "item", "items", "test", "tests", "file", "files", "line", "lines",
            "page", "pages", "task", "tasks", "left", "eta", "progress", "total", "done",
        ];
        if stop_names.contains(&run_lower.as_str()) {
            continue;
        }

        let linked_uuid = extract_uuid(&clean);
        let suffix = clean[tot_end..].trim();
        let suffix_lower = suffix.to_lowercase();

        let is_done = current >= total
            || suffix_lower.contains("completed")
            || suffix_lower.contains("finalized");

        let clean_suffix = suffix
            .trim_start_matches('(')
            .trim_end_matches(')')
            .trim();
        let clean_suffix = if let Some(ref uuid) = linked_uuid {
            clean_suffix
                .replace(uuid, "")
                .trim_end_matches(|c: char| c.is_whitespace() || c == '—' || c == '-' || c == '|')
                .trim()
                .to_string()
        } else {
            clean_suffix.to_string()
        };

        let task_desc = if is_done {
            match &detail {
                Some(d) => format!("{} — Completed & Finalized", d),
                None => "Completed & Finalized".to_string(),
            }
        } else {
            let rem_text = if !clean_suffix.is_empty() {
                clean_suffix
            } else {
                format!("{} left", total.saturating_sub(current))
            };
            match &detail {
                Some(d) => format!("{} — {}", d, rem_text),
                None => rem_text,
            }
        };

        let step_label = if is_done {
            "Finalized".to_string()
        } else if run_lower.contains("-v3") {
            "Ablation v3".to_string()
        } else {
            "Benchmark".to_string()
        };

        let status = if is_done {
            "done".to_string()
        } else if current == 0 {
            "queued".to_string()
        } else {
            "running".to_string()
        };

        return Some(BenchmarkRecord {
            name: run_name,
            current_step: current,
            total_steps: total,
            step_label,
            task: truncate_str(&task_desc, 96),
            status,
            question: None,
            linked_uuid,
            extra_uuids: Vec::new(),
            workspace_path: workspace_path.map(|s| s.to_string()),
            updated_at: timestamp,
            priority_source,
        });
    }

    None
}

fn push_unique_uuid(target: &mut BenchmarkRecord, uuid: String) {
    if target.linked_uuid.as_ref() != Some(&uuid) && !target.extra_uuids.contains(&uuid) {
        target.extra_uuids.push(uuid);
    }
}

fn merge_benchmark_uuids(
    target: &mut BenchmarkRecord,
    incoming_uuid: Option<String>,
    incoming_extra: Vec<String>,
    prefer_incoming: bool,
) {
    for u in incoming_extra {
        push_unique_uuid(target, u);
    }
    if let Some(new_uuid) = incoming_uuid {
        if target.linked_uuid.is_none() {
            target.linked_uuid = Some(new_uuid);
        } else if target.linked_uuid.as_deref() != Some(new_uuid.as_str()) {
            if prefer_incoming {
                if let Some(old_uuid) = target.linked_uuid.replace(new_uuid) {
                    push_unique_uuid(target, old_uuid);
                }
            } else {
                push_unique_uuid(target, new_uuid);
            }
        }
    }
}

fn upsert_benchmark_record(map: &mut HashMap<String, BenchmarkRecord>, mut rec: BenchmarkRecord) {
    let key = rec.name.to_lowercase();
    match map.get_mut(&key) {
        Some(existing) => {
            let prefer_incoming_uuid = existing.linked_uuid.is_none()
                || rec.current_step > existing.current_step
                || rec.updated_at >= existing.updated_at;
            merge_benchmark_uuids(
                existing,
                rec.linked_uuid.take(),
                std::mem::take(&mut rec.extra_uuids),
                prefer_incoming_uuid,
            );
            if existing.workspace_path.is_none() {
                existing.workspace_path = rec.workspace_path.take();
            }
            let max_updated = existing.updated_at.max(rec.updated_at);

            if existing.total_steps == rec.total_steps {
                existing.updated_at = max_updated;
                if rec.current_step > existing.current_step {
                    existing.current_step = rec.current_step;
                    existing.status = rec.status;
                    existing.step_label = rec.step_label;
                    if !rec.task.contains(" — ") {
                        if let Some((detail, _)) = existing.task.split_once(" — ") {
                            existing.task =
                                truncate_str(&format!("{} — {}", detail.trim(), rec.task), 96);
                        } else {
                            existing.task = rec.task;
                        }
                    } else {
                        existing.task = rec.task;
                    }
                    if rec.priority_source > existing.priority_source {
                        existing.priority_source = rec.priority_source;
                    }
                } else if rec.current_step == existing.current_step {
                    if !existing.task.contains(" — ") && rec.task.contains(" — ") {
                        existing.task = rec.task.clone();
                    }
                    if rec.priority_source > existing.priority_source {
                        existing.priority_source = rec.priority_source;
                        existing.status = rec.status;
                        existing.step_label = rec.step_label;
                        if rec.task.contains(" — ") {
                            existing.task = rec.task;
                        }
                    }
                }
            } else {
                let should_replace = (rec.priority_source > existing.priority_source
                    && rec.updated_at + 120 >= existing.updated_at)
                    || (rec.priority_source == existing.priority_source
                        && rec.updated_at >= existing.updated_at);
                existing.updated_at = max_updated;
                if should_replace {
                    let prev_linked = existing.linked_uuid.take();
                    let prev_extra = std::mem::take(&mut existing.extra_uuids);
                    let prev_ws = existing.workspace_path.take();
                    rec.updated_at = max_updated;
                    if rec.workspace_path.is_none() {
                        rec.workspace_path = prev_ws;
                    }
                    merge_benchmark_uuids(&mut rec, prev_linked, prev_extra, false);
                    *existing = rec;
                }
            }
        }
        None => {
            map.insert(key, rec);
        }
    }
}

fn parse_json_value_for_benchmarks(
    val: &Value,
    hint_key: Option<&str>,
    timestamp: u64,
    workspace_path: Option<&str>,
    runs: &mut HashMap<String, BenchmarkRecord>,
) {
    match val {
        Value::Object(obj) => {
            let cur = obj
                .get("current")
                .or_else(|| obj.get("current_step"))
                .or_else(|| obj.get("completed"))
                .or_else(|| obj.get("done"))
                .or_else(|| obj.get("processed"))
                .or_else(|| obj.get("count"))
                .and_then(|v| v.as_u64())
                .map(|v| v as u32);
            let tot = obj
                .get("total")
                .or_else(|| obj.get("total_steps"))
                .or_else(|| obj.get("target"))
                .and_then(|v| v.as_u64())
                .map(|v| v as u32);

            if let (Some(c), Some(t)) = (cur, tot) {
                if t >= 10 && c <= t {
                    let name = obj
                        .get("name")
                        .or_else(|| obj.get("run"))
                        .or_else(|| obj.get("model"))
                        .or_else(|| obj.get("id"))
                        .and_then(|v| v.as_str())
                        .or(hint_key);

                    if let Some(n) = name {
                        let clean_name = n.trim();
                        if !clean_name.is_empty() && clean_name.len() <= 48 {
                            let is_done = c >= t
                                || obj
                                    .get("status")
                                    .and_then(|v| v.as_str())
                                    .map(|s| s.eq_ignore_ascii_case("done") || s.eq_ignore_ascii_case("completed"))
                                    .unwrap_or(false);
                            let model_detail = obj
                                .get("model")
                                .and_then(|v| v.as_str())
                                .filter(|m| *m != clean_name);
                            let task_desc = if is_done {
                                model_detail
                                    .map(|m| format!("{m} — Completed & Finalized"))
                                    .unwrap_or_else(|| "Completed & Finalized".to_string())
                            } else {
                                let left = format!("{} left", t.saturating_sub(c));
                                model_detail
                                    .map(|m| format!("{m} — {left}"))
                                    .unwrap_or(left)
                            };
                            let linked_uuid = obj
                                .get("conversation_id")
                                .or_else(|| obj.get("conversationId"))
                                .and_then(|v| v.as_str())
                                .and_then(extract_uuid);

                            upsert_benchmark_record(
                                runs,
                                BenchmarkRecord {
                                    name: clean_name.to_string(),
                                    current_step: c,
                                    total_steps: t,
                                    step_label: if is_done {
                                        "Finalized".to_string()
                                    } else {
                                        "Ablation v3".to_string()
                                    },
                                    task: truncate_str(&task_desc, 96),
                                    status: if is_done {
                                        "done".to_string()
                                    } else if c == 0 {
                                        "queued".to_string()
                                    } else {
                                        "running".to_string()
                                    },
                                    question: None,
                                    linked_uuid,
                                    extra_uuids: Vec::new(),
                                    workspace_path: workspace_path.map(|s| s.to_string()),
                                    updated_at: timestamp,
                                    priority_source: 2,
                                },
                            );
                        }
                    }
                }
            }

            for (k, v) in obj {
                parse_json_value_for_benchmarks(v, Some(k.as_str()), timestamp, workspace_path, runs);
            }
        }
        Value::Array(arr) => {
            for item in arr {
                parse_json_value_for_benchmarks(item, hint_key, timestamp, workspace_path, runs);
            }
        }
        Value::String(s) => {
            for line in s.lines() {
                if let Some(rec) = parse_benchmark_line(line, timestamp, workspace_path, 2) {
                    upsert_benchmark_record(runs, rec);
                }
            }
        }
        _ => {}
    }
}

fn find_axiom_dirs(discovered_workspaces: &[PathBuf], user_profile: Option<&Path>) -> Vec<PathBuf> {
    let mut candidates: Vec<PathBuf> = Vec::new();

    for ws in discovered_workspaces {
        if ws
            .file_name()
            .and_then(|n| n.to_str())
            .map(|n| n.to_lowercase().contains("axiom"))
            .unwrap_or(false)
        {
            candidates.push(ws.clone());
        }
        if let Some(parent) = ws.parent() {
            candidates.push(parent.join("axiom"));
        }
    }

    if let Some(profile) = user_profile {
        candidates.push(profile.join("axiom"));
        candidates.push(profile.join("Desktop").join("axiom"));
        candidates.push(profile.join("Documents").join("axiom"));
        candidates.push(profile.join("projects").join("axiom"));
        candidates.push(profile.join("code").join("axiom"));
    }

    let mut seen: HashSet<String> = HashSet::new();
    let mut out: Vec<PathBuf> = Vec::new();
    for dir in candidates {
        if dir.is_dir() {
            let key = dir.to_string_lossy().to_lowercase();
            if seen.insert(key) {
                out.push(dir);
            }
        }
    }
    out
}

static RUNS_V3_CACHE: std::sync::OnceLock<Mutex<HashMap<PathBuf, (u64, u64, BenchmarkRecord)>>> =
    std::sync::OnceLock::new();

fn extract_json_string_field(head: &str, key: &str) -> Option<String> {
    let needle = format!("\"{}\"", key);
    let idx = head.find(&needle)?;
    let after_key = head.get(idx + needle.len()..)?.trim_start();
    let after_colon = after_key.strip_prefix(':')?.trim_start();
    let after_quote = after_colon.strip_prefix('"')?;
    let mut out = String::new();
    let mut escaped = false;
    for c in after_quote.chars() {
        if escaped {
            out.push(c);
            escaped = false;
        } else if c == '\\' {
            escaped = true;
        } else if c == '"' {
            let trimmed = out.trim();
            return if trimmed.is_empty() {
                None
            } else {
                Some(trimmed.to_string())
            };
        } else {
            out.push(c);
        }
    }
    None
}

fn extract_v3_total_steps(head: &str) -> u32 {
    let mut cond_count = 8u32;
    if let Some(idx) = head.find("\"conditions\"") {
        if let Some(after) = head.get(idx..).and_then(|s| s.find('[').map(|i| &s[i + 1..])) {
            if let Some(end) = after.find(']') {
                let items = after[..end]
                    .split(',')
                    .filter(|part| part.trim().starts_with('"'))
                    .count() as u32;
                if (1..=32).contains(&items) {
                    cond_count = items;
                }
            }
        }
    }

    if let Some(idx) = head.find("\"targetTasks\"") {
        if let Some(after) = head.get(idx..).and_then(|s| s.find('[').map(|i| &s[i + 1..])) {
            if let Some(end) = after.find(']') {
                let tasks = after[..end]
                    .split(',')
                    .filter(|part| part.trim().starts_with('"'))
                    .count() as u32;
                let total = tasks.saturating_mul(cond_count);
                if total >= 10 {
                    return total;
                }
            }
        }
    }

    960
}

fn count_bytes_pattern(haystack: &[u8], needle: &[u8]) -> u32 {
    if needle.is_empty() || haystack.len() < needle.len() {
        return 0;
    }
    let first = needle[0];
    let mut count = 0u32;
    let mut i = 0usize;
    let limit = haystack.len() - needle.len();
    while i <= limit {
        if haystack[i] == first && &haystack[i..i + needle.len()] == needle {
            count = count.saturating_add(1);
            i += needle.len();
        } else {
            i += 1;
        }
    }
    count
}

fn count_trials_fallback_json(bytes: &[u8]) -> u32 {
    let Ok(val) = serde_json::from_slice::<Value>(bytes) else {
        return 0;
    };
    let Some(obj) = val.as_object() else {
        return 0;
    };
    for key in ["trials", "results", "completed", "records", "entries", "rows"] {
        if let Some(v) = obj.get(key) {
            match v {
                Value::Array(arr) if !arr.is_empty() => return arr.len() as u32,
                Value::Object(map) if !map.is_empty() => return map.len() as u32,
                _ => {}
            }
        }
    }
    0
}

fn scan_axiom_runs_v3(
    axiom_dir: &Path,
    now: u64,
    workspace_root: &str,
    runs: &mut HashMap<String, BenchmarkRecord>,
) {
    let runs_v3_dir = axiom_dir.join(".axiom-state").join("runs-v3");
    if !runs_v3_dir.is_dir() {
        return;
    }
    let Ok(entries) = fs::read_dir(&runs_v3_dir) else {
        return;
    };

    let cache = RUNS_V3_CACHE.get_or_init(|| Mutex::new(HashMap::new()));

    for entry in entries.flatten() {
        let run_dir = entry.path();
        if !run_dir.is_dir() {
            continue;
        }
        let Some(run_id) = run_dir
            .file_name()
            .and_then(|n| n.to_str())
            .map(|s| s.to_string())
        else {
            continue;
        };

        let checkpoint_path = run_dir.join("checkpoint.json");
        let results_path = run_dir.join("results.json");
        let ck_mtime = file_mtime_secs(&checkpoint_path);
        let res_mtime = file_mtime_secs(&results_path);
        let dir_mtime = file_mtime_secs(&run_dir);
        let max_mtime = ck_mtime.max(res_mtime).max(dir_mtime);

        // Retain runs-v3 entries up to 7 days so recently finalized confirmatory runs stay visible
        if max_mtime == 0 || now.saturating_sub(max_mtime) > 604_800 {
            continue;
        }

        let ck_len = fs::metadata(&checkpoint_path).map(|m| m.len()).unwrap_or(0);
        let res_len = fs::metadata(&results_path).map(|m| m.len()).unwrap_or(0);
        let combined_len = ck_len.saturating_add(res_len);
        if combined_len == 0 {
            continue;
        }

        {
            let guard = lock_or_recover(cache);
            if let Some((cached_mtime, cached_len, cached_rec)) = guard.get(&run_dir) {
                if *cached_mtime == max_mtime && *cached_len == combined_len {
                    upsert_benchmark_record(runs, cached_rec.clone());
                    continue;
                }
            }
        }

        let ordered_paths: Vec<&PathBuf> = if ck_len > 200_000 || ck_mtime >= res_mtime {
            vec![&checkpoint_path, &results_path]
        } else {
            vec![&results_path, &checkpoint_path]
        };

        let mut model_opt: Option<String> = None;
        let mut total_steps = 960u32;
        let mut found_total = false;
        let mut count = 0u32;

        for path in &ordered_paths {
            if !path.is_file() {
                continue;
            }
            let Ok(bytes) = fs::read(path) else {
                continue;
            };
            let head_slice = &bytes[..bytes.len().min(16_384)];
            let head = String::from_utf8_lossy(head_slice);

            if model_opt.is_none() {
                model_opt = extract_json_string_field(&head, "requestedModel")
                    .or_else(|| extract_json_string_field(&head, "model"));
            }
            if !found_total {
                let parsed_total = extract_v3_total_steps(&head);
                if head.contains("\"targetTasks\"") {
                    total_steps = parsed_total;
                    found_total = true;
                }
            }

            if count == 0 {
                let mut c = count_bytes_pattern(&bytes, b"\"boundPass\":");
                if c == 0 {
                    c = count_trials_fallback_json(&bytes);
                }
                if c > 0 {
                    count = c;
                }
            }
        }

        let current_step = count.min(total_steps);
        let is_done = current_step >= total_steps;
        let step_label = if is_done {
            "Finalized".to_string()
        } else if run_id.to_lowercase().contains("-v3") {
            "Ablation v3".to_string()
        } else {
            "Benchmark".to_string()
        };
        let status = if is_done {
            "done".to_string()
        } else if current_step == 0 {
            "queued".to_string()
        } else {
            "running".to_string()
        };
        let task_desc = if is_done {
            match &model_opt {
                Some(m) => format!("{m} — Completed & Finalized"),
                None => "Completed & Finalized".to_string(),
            }
        } else {
            let left = format!("{} left", total_steps.saturating_sub(current_step));
            match &model_opt {
                Some(m) => format!("{m} — {left}"),
                None => left,
            }
        };

        let rec = BenchmarkRecord {
            name: run_id,
            current_step,
            total_steps,
            step_label,
            task: truncate_str(&task_desc, 96),
            status,
            question: None,
            linked_uuid: None,
            extra_uuids: Vec::new(),
            workspace_path: Some(workspace_root.to_string()),
            updated_at: max_mtime,
            priority_source: 3,
        };

        {
            let mut guard = lock_or_recover(cache);
            guard.insert(run_dir, (max_mtime, combined_len, rec.clone()));
        }
        upsert_benchmark_record(runs, rec);
    }
}

fn scan_axiom_dir_recursive(
    dir: &Path,
    depth: usize,
    now: u64,
    workspace_root: &str,
    runs: &mut HashMap<String, BenchmarkRecord>,
) {
    if depth == 0 {
        scan_axiom_runs_v3(dir, now, workspace_root, runs);
    }
    if depth > 3 {
        return;
    }
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };

    let skip_dirs = [
        "node_modules",
        ".git",
        "dist",
        "target",
        ".next",
        "venv",
        ".venv",
        "fixtures",
        "src",
        "tests",
        "docs",
        "archive",
        "canary-outputs",
        "quarantine",
        "runs-v3",
        ".github",
        ".husky",
        ".vscode",
        ".tsupgrader",
        "dashboard",
        "agents",
        "scripts",
    ];

    for entry in entries.flatten() {
        let path = entry.path();
        let Some(fname) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        let fname_lower = fname.to_lowercase();

        if path.is_dir() {
            let should_skip = fname_lower.starts_with(".axiom-cache")
                || fname_lower.contains("cache")
                || skip_dirs.contains(&fname_lower.as_str());
            if !should_skip {
                scan_axiom_dir_recursive(&path, depth + 1, now, workspace_root, runs);
            }
            continue;
        }

        let mtime = file_mtime_secs(&path);
        if now.saturating_sub(mtime) > 172_800 {
            continue;
        }

        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_lowercase())
            .unwrap_or_default();

        if matches!(ext.as_str(), "log" | "txt" | "md") {
            if let Some(tail) = read_tail_text(&path, 65536) {
                for line in tail.lines() {
                    if let Some(rec) =
                        parse_benchmark_line(line, mtime, Some(workspace_root), 2)
                    {
                        upsert_benchmark_record(runs, rec);
                    }
                }
            }
        } else if ext == "json" {
            let size = fs::metadata(&path).map(|m| m.len()).unwrap_or(u64::MAX);
            if size <= 524_288
                && (fname_lower.contains("status")
                    || fname_lower.contains("progress")
                    || fname_lower.contains("ablation")
                    || fname_lower.contains("summary")
                    || fname_lower.contains("state")
                    || fname_lower.contains("result")
                    || fname_lower.contains("v3")
                    || fname_lower.contains("run")
                    || fname_lower.contains("daemon"))
            {
                if let Ok(text) = fs::read_to_string(&path) {
                    if let Ok(val) = serde_json::from_str::<Value>(&text) {
                        parse_json_value_for_benchmarks(
                            &val,
                            None,
                            mtime,
                            Some(workspace_root),
                            runs,
                        );
                    }
                }
            }
        } else if ext == "jsonl" {
            let size = fs::metadata(&path).map(|m| m.len()).unwrap_or(u64::MAX);
            if size <= 33_554_432 {
                if let Some((head, tail)) = read_head_and_tail(&path, 4096, 8192) {
                    for text in [&head, &tail] {
                        for line in text.lines() {
                            if let Ok(val) = serde_json::from_str::<Value>(line) {
                                parse_json_value_for_benchmarks(
                                    &val,
                                    None,
                                    mtime,
                                    Some(workspace_root),
                                    runs,
                                );
                            }
                        }
                    }
                }
                // Also count completed lines if this JSONL matches a known run stem (e.g. glm5-v3.jsonl)
                if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                    let stem_lower = stem.to_lowercase();
                    if let Some(existing) = runs.get_mut(&stem_lower) {
                        if mtime > existing.updated_at {
                            if let Ok(f) = File::open(&path) {
                                let count = BufReader::new(f).lines().count() as u32;
                                if count > existing.current_step && count <= existing.total_steps {
                                    existing.current_step = count;
                                    existing.updated_at = mtime;
                                    if count >= existing.total_steps {
                                        existing.status = "done".to_string();
                                        existing.step_label = "Finalized".to_string();
                                        existing.task = "Completed & Finalized".to_string();
                                    } else {
                                        existing.status = "running".to_string();
                                        existing.task = format!(
                                            "{} left",
                                            existing.total_steps.saturating_sub(count)
                                        );
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

struct DiscoveredConv {
    id: String,
    workspace_path: Option<String>,
    updated_at: u64,
    latest_log_mtime: u64,
    is_running: bool,
    is_waiting_permission: bool,
    is_paused: bool,
    is_subagent: bool,
    pause_signal_ts: u64,
    pending_question: Option<String>,
    mentioned_run_ids: Vec<String>,
    task_md: Option<TaskMdInfo>,
    last_user_prompt: Option<String>,
    last_tool_summary: Option<String>,
    tools_done: u32,
    tools_total: u32,
}

fn text_mentions_pause(text: &str) -> bool {
    let lower = text.to_lowercase();
    lower.contains("pause all")
        || lower.contains("have been stopped")
        || lower.contains("saved checkpoints")
        || lower.contains("stopped at checkpoint")
        || lower.contains("paused at checkpoint")
        || lower == "pause"
        || lower.starts_with("pause ")
        || lower.contains("\npause ")
}

fn clean_user_prompt_summary(content: &str) -> Option<String> {
    let inner = if let Some(start) = content.find("<USER_REQUEST>") {
        let after = &content[start + "<USER_REQUEST>".len()..];
        if let Some(end) = after.find("</USER_REQUEST>") {
            &after[..end]
        } else {
            after
        }
    } else if let Some(start) = content.find("<SYSTEM_MESSAGE>") {
        let after = &content[start + "<SYSTEM_MESSAGE>".len()..];
        let bounded = if let Some(end) = after.find("</SYSTEM_MESSAGE>") {
            &after[..end]
        } else {
            after
        };
        if let Some(cidx) = bounded.find("content=") {
            &bounded[cidx + "content=".len()..]
        } else {
            bounded
        }
    } else {
        content
    };

    for line in inner.lines() {
        let cleaned = line
            .trim()
            .trim_start_matches('#')
            .trim_start_matches('>')
            .trim_matches('*')
            .trim_matches('`')
            .trim();
        if cleaned.len() >= 3 && !cleaned.starts_with("---") && !cleaned.starts_with("```") {
            return Some(truncate_str(cleaned, 84));
        }
    }
    None
}

fn clean_cli_token(raw: &str) -> String {
    raw.trim_matches(|c: char| {
        c.is_whitespace()
            || matches!(
                c,
                '"' | '\'' | '`' | ',' | ';' | '(' | ')' | '[' | ']' | '<' | '>' | '\\'
            )
    })
    .to_string()
}

fn extract_flag_value(text: &str, flag: &str) -> Option<String> {
    let mut search_from = 0usize;
    while let Some(hay) = text.get(search_from..) {
        let Some(pos) = hay.find(flag) else {
            break;
        };
        let abs = search_from + pos + flag.len();
        search_from = abs;
        let Some(after) = text.get(abs..) else {
            break;
        };
        let trimmed = after.trim_start_matches(|c: char| c == ' ' || c == '\t' || c == '=');
        if trimmed.is_empty() {
            continue;
        }
        let end = trimmed
            .find(|c: char| {
                c.is_whitespace() || matches!(c, '"' | '\'' | '`' | ',' | ';' | ')' | ']' | '}')
            })
            .unwrap_or(trimmed.len());
        let token = clean_cli_token(&trimmed[..end]);
        if !token.is_empty() && !token.starts_with("--") {
            return Some(token);
        }
    }
    None
}

fn extract_subagent_benchmark_context(text: &str) -> Option<(String, Option<String>)> {
    let model_opt = extract_flag_value(text, "--model");
    if let Some(run_id) = extract_flag_value(text, "--run-id") {
        if run_id.len() >= 2 {
            return Some((run_id, model_opt));
        }
    }

    let normalized = text.replace('\\', "/");
    let lower = normalized.to_ascii_lowercase();
    if let Some(pos) = lower.find("runs-v3/") {
        if let Some(after) = normalized.get(pos + "runs-v3/".len()..) {
            let end = after
                .find(|c: char| {
                    c == '/'
                        || c.is_whitespace()
                        || matches!(c, '"' | '\'' | '`' | ',' | ';' | ')' | ']')
                })
                .unwrap_or(after.len());
            let run_id = clean_cli_token(&after[..end]);
            if run_id.len() >= 2 {
                return Some((run_id, model_opt));
            }
        }
    }

    None
}

fn extract_all_run_ids(text: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();

    let mut search_from = 0usize;
    while let Some(hay) = text.get(search_from..) {
        let Some(pos) = hay.find("--run-id") else {
            break;
        };
        let abs = search_from + pos + "--run-id".len();
        search_from = abs;
        let Some(after) = text.get(abs..) else {
            break;
        };
        let trimmed = after.trim_start_matches(|c: char| c == ' ' || c == '\t' || c == '=');
        if trimmed.is_empty() {
            continue;
        }
        let end = trimmed
            .find(|c: char| {
                c.is_whitespace() || matches!(c, '"' | '\'' | '`' | ',' | ';' | ')' | ']' | '}')
            })
            .unwrap_or(trimmed.len());
        let token = clean_cli_token(&trimmed[..end]);
        if token.len() >= 2 && !token.starts_with("--") {
            let low = token.to_ascii_lowercase();
            if seen.insert(low) {
                out.push(token);
            }
        }
    }

    let normalized = text.replace('\\', "/");
    let lower = normalized.to_ascii_lowercase();
    let mut r_from = 0usize;
    while let Some(hay) = lower.get(r_from..) {
        let Some(pos) = hay.find("runs-v3/") else {
            break;
        };
        let abs = r_from + pos + "runs-v3/".len();
        r_from = abs;
        let Some(after) = normalized.get(abs..) else {
            break;
        };
        let end = after
            .find(|c: char| {
                c == '/'
                    || c.is_whitespace()
                    || matches!(c, '"' | '\'' | '`' | ',' | ';' | ')' | ']')
            })
            .unwrap_or(after.len());
        let token = clean_cli_token(&after[..end]);
        if token.len() >= 2 && !token.starts_with("--") {
            let low = token.to_ascii_lowercase();
            if seen.insert(low) {
                out.push(token);
            }
        }
    }

    out
}

fn extract_ask_question_text(args: &Value) -> Option<String> {
    if let Some(q) = args.get("question").and_then(unquote_json_arg) {
        if !q.trim().is_empty() {
            return Some(truncate_str(q.trim(), 140));
        }
    }
    if let Some(qval) = args.get("questions") {
        let arr_opt = match qval {
            Value::Array(arr) => Some(arr.clone()),
            Value::String(s) => serde_json::from_str::<Vec<Value>>(s)
                .ok()
                .or_else(|| {
                    unquote_json_arg(qval).and_then(|u| serde_json::from_str::<Vec<Value>>(&u).ok())
                }),
            _ => None,
        };
        if let Some(arr) = arr_opt {
            if let Some(qtext) = arr
                .first()
                .and_then(|item| item.get("question"))
                .and_then(|v| v.as_str())
            {
                let trimmed = qtext.trim();
                if !trimmed.is_empty() {
                    return Some(truncate_str(trimmed, 140));
                }
            }
        }
    }
    None
}

fn detect_pending_approval(
    parsed_entries: &[Value],
    now: u64,
    transcript_mtime: u64,
    latest_log_mtime: u64,
    is_subagent_conv: bool,
) -> Option<(String, String)> {
    if transcript_mtime == 0 || now.saturating_sub(transcript_mtime) > 43_200 {
        return None;
    }

    let last_planner_idx = parsed_entries
        .iter()
        .rposition(|v| v.get("type").and_then(|t| t.as_str()) == Some("PLANNER_RESPONSE"))?;

    let tail_after = &parsed_entries[last_planner_idx + 1..];
    if tail_after.iter().any(|v| {
        matches!(
            v.get("type").and_then(|t| t.as_str()),
            Some("USER_INPUT" | "SYSTEM_MESSAGE")
        )
    }) {
        return None;
    }

    let calls = parsed_entries[last_planner_idx]
        .get("tool_calls")
        .and_then(|c| c.as_array())?;
    if calls.is_empty() {
        return None;
    }

    let generics_after: Vec<&Value> = tail_after
        .iter()
        .filter(|v| v.get("type").and_then(|t| t.as_str()) == Some("GENERIC"))
        .collect();

    if generics_after.iter().any(|v| {
        v.get("status").and_then(|s| s.as_str()) == Some("RUNNING")
    }) {
        return None;
    }

    let waiting_idx = generics_after.iter().position(|v| {
        matches!(
            v.get("status").and_then(|s| s.as_str()),
            Some("WAITING" | "WAITING_FOR_USER" | "PENDING" | "REQUIRES_APPROVAL")
        )
    });

    let pending_idx = if let Some(widx) = waiting_idx {
        widx
    } else if generics_after.len() < calls.len() {
        generics_after.len()
    } else {
        return None;
    };

    let pending_call = calls.get(pending_idx)?;
    let name = pending_call
        .get("name")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let args = pending_call.get("args");

    if name == "ask_question" {
        let q = args
            .and_then(extract_ask_question_text)
            .unwrap_or_else(|| "Agent paused for input — click Submit ⏎".to_string());
        return Some(("Needs Input".to_string(), q));
    }

    if matches!(
        name,
        "run_command" | "execute_command" | "bash" | "shell"
    ) {
        if waiting_idx.is_none() && latest_log_mtime >= transcript_mtime {
            return None;
        }
        if waiting_idx.is_none() && now.saturating_sub(transcript_mtime) > 180 {
            return None;
        }

        let bypass = args
            .and_then(|a| a.get("BypassSandbox"))
            .map(|v| v.as_bool() == Some(true) || unquote_json_arg(v).as_deref() == Some("true"))
            .unwrap_or(false);
        let cmd_line = args
            .and_then(|a| a.get("CommandLine").or_else(|| a.get("command")))
            .and_then(unquote_json_arg)
            .unwrap_or_default();
        let summary = args
            .and_then(|a| a.get("toolSummary"))
            .and_then(unquote_json_arg)
            .unwrap_or_default();

        let elapsed = now.saturating_sub(transcript_mtime);
        let triggered = waiting_idx.is_some()
            || (bypass && elapsed >= 2)
            || (is_subagent_conv && elapsed >= 2)
            || elapsed >= 6;
        if !triggered {
            return None;
        }

        let question = if !summary.is_empty() && !cmd_line.is_empty() {
            format!(
                "Allow {}? — {}",
                summary.trim().trim_end_matches('?'),
                truncate_str(&cmd_line, 140)
            )
        } else if !cmd_line.is_empty() {
            format!("Allow command? {}", truncate_str(&cmd_line, 140))
        } else if !summary.is_empty() {
            format!("Allow {}?", summary.trim().trim_end_matches('?'))
        } else {
            "Allow command? — waiting for Submit ⏎".to_string()
        };

        let step_label = if !summary.is_empty() {
            truncate_str(&format!("Allow: {}", summary), 42)
        } else {
            "Needs Approval".to_string()
        };

        return Some((step_label, question));
    }

    None
}

fn parse_subagent_trial_line(
    raw_line: &str,
    run_id: &str,
    model_opt: Option<&str>,
    conv_id: &str,
    timestamp: u64,
    workspace_path: Option<&str>,
) -> Option<BenchmarkRecord> {
    let clean = strip_ansi(raw_line);
    let trimmed = clean.trim();
    let after_open = trimmed.strip_prefix('[')?;
    let close_idx = after_open.find(']')?;
    let inside = after_open.get(..close_idx)?.trim();

    let (prog, rest_label) = inside.split_once(char::is_whitespace)?;
    let trial_label = rest_label.trim();
    if trial_label.is_empty() || !trial_label.chars().any(|c| c.is_ascii_alphanumeric()) {
        return None;
    }

    let (cur_s, tot_s) = prog.split_once('/')?;
    let cur: u32 = cur_s.trim().parse().ok()?;
    let tot: u32 = tot_s.trim().parse().ok()?;
    if tot < 10 || cur > tot || tot > 1_000_000 {
        return None;
    }

    let is_done = cur >= tot;
    let step_label = if is_done {
        "Finalized".to_string()
    } else {
        truncate_str(trial_label, 42)
    };
    let task_desc = if is_done {
        match model_opt {
            Some(m) => format!("{m} — Completed & Finalized"),
            None => "Completed & Finalized".to_string(),
        }
    } else {
        let left = format!("{} left", tot.saturating_sub(cur));
        match model_opt {
            Some(m) => format!("{m} — {left}"),
            None => left,
        }
    };

    Some(BenchmarkRecord {
        name: run_id.to_string(),
        current_step: cur,
        total_steps: tot,
        step_label,
        task: truncate_str(&task_desc, 96),
        status: if is_done {
            "done".to_string()
        } else {
            "running".to_string()
        },
        question: None,
        linked_uuid: Some(conv_id.to_string()),
        extra_uuids: Vec::new(),
        workspace_path: workspace_path.map(|s| s.to_string()),
        updated_at: timestamp,
        priority_source: 4,
    })
}

fn format_tool_summary(call: &Value) -> Option<String> {
    let name = call.get("name").and_then(|v| v.as_str())?;
    if let Some(args) = call.get("args") {
        if let Some(summary) = args.get("toolSummary").and_then(unquote_json_arg) {
            if !summary.is_empty() {
                return Some(truncate_str(&summary, 48));
            }
        }
        if let Some(path) = args.get("Path").and_then(unquote_json_arg) {
            let fname = Path::new(&path)
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or(&path);
            let verb = match name {
                "read_file" => "Reading",
                "edit_file" => "Editing",
                "write_file" => "Writing",
                _ => name,
            };
            return Some(truncate_str(&format!("{verb} {fname}"), 48));
        }
    }
    Some(name.replace('_', " "))
}

fn scan_brain_and_axiom() -> Vec<AgentSession> {
    let now = now_secs();
    let max_age = 172_800u64; // 48 hours
    let user_profile = get_user_profile_dir();
    let Some(profile) = user_profile.as_ref() else {
        return Vec::new();
    };
    let brain_dir = profile.join(".gemini").join("antigravity").join("brain");
    let Ok(entries) = fs::read_dir(&brain_dir) else {
        return Vec::new();
    };

    let mut convs: Vec<DiscoveredConv> = Vec::new();
    let mut discovered_workspaces: Vec<PathBuf> = Vec::new();
    let mut benchmark_runs: HashMap<String, BenchmarkRecord> = HashMap::new();

    for entry in entries.flatten() {
        let conv_dir = entry.path();
        if !conv_dir.is_dir() {
            continue;
        }
        let Some(conv_id) = conv_dir.file_name().and_then(|n| n.to_str()).map(|s| s.to_string()) else {
            continue;
        };

        let task_md_path = conv_dir.join("task.md");
        let transcript_path = conv_dir
            .join(".system_generated")
            .join("logs")
            .join("transcript.jsonl");
        let tasks_dir = conv_dir.join(".system_generated").join("tasks");

        let dir_mtime = file_mtime_secs(&conv_dir);
        let task_md_mtime = file_mtime_secs(&task_md_path);
        let transcript_mtime = file_mtime_secs(&transcript_path);
        let tasks_dir_mtime = file_mtime_secs(&tasks_dir);

        // Stat task-*.log files BEFORE opening large transcripts so stale (>48h) folders are skipped fast
        let mut all_task_logs: Vec<(PathBuf, String, u64)> = Vec::new();
        if tasks_dir.is_dir() {
            if let Ok(log_entries) = fs::read_dir(&tasks_dir) {
                for le in log_entries.flatten() {
                    let lp = le.path();
                    if let Some(lname) = lp.file_name().and_then(|n| n.to_str()) {
                        if lname.starts_with("task-") && lname.ends_with(".log") {
                            let lname_owned = lname.to_string();
                            let lmtime = file_mtime_secs(&lp);
                            all_task_logs.push((lp, lname_owned, lmtime));
                        }
                    }
                }
            }
        }
        let latest_all_log_mtime = all_task_logs.iter().map(|t| t.2).max().unwrap_or(0);
        let preliminary_updated = [
            dir_mtime,
            task_md_mtime,
            transcript_mtime,
            tasks_dir_mtime,
            latest_all_log_mtime,
        ]
        .into_iter()
        .max()
        .unwrap_or(0);

        if preliminary_updated == 0 || now.saturating_sub(preliminary_updated) > max_age {
            continue;
        }

        // Read transcript head + tail so we can identify dinobuds task logs & workspace
        let mut head_tail: Option<(String, String)> = None;
        if transcript_path.is_file() {
            head_tail = read_head_and_tail(&transcript_path, 16_384, 65_536);
        }

        let mut dino_task_ids: HashSet<String> = HashSet::new();
        let mut workspace_path_buf: Option<PathBuf> = None;
        let mut combined_text = String::new();

        if let Some((ref head, ref tail)) = head_tail {
            combined_text = format!("{}\n{}", head, tail);
            workspace_path_buf = extract_workspace_from_text(&combined_text);
            if let Some(ref ws) = workspace_path_buf {
                discovered_workspaces.push(ws.clone());
            }

            // Identify any task-<n> IDs that launched dinobuds.exe so we don't treat our own HUD daemon log as active work
            for line in tail.lines() {
                if line.contains("dinobuds.exe") {
                    if let Some(idx) = line.find("task-") {
                        let rest = &line[idx..];
                        let end = rest
                            .find(|c: char| !c.is_ascii_alphanumeric() && c != '-')
                            .unwrap_or(rest.len());
                        dino_task_ids.insert(format!("{}.log", &rest[..end]));
                    }
                }
            }
        }

        let mut task_logs: Vec<(PathBuf, u64)> = all_task_logs
            .into_iter()
            .filter(|(_, lname, _)| !dino_task_ids.contains(lname))
            .map(|(lp, _, lmtime)| (lp, lmtime))
            .collect();
        task_logs.sort_by(|a, b| b.1.cmp(&a.1));

        let latest_non_dino_log_mtime = task_logs.first().map(|t| t.1).unwrap_or(0);
        let updated_at = [
            dir_mtime,
            task_md_mtime,
            transcript_mtime,
            tasks_dir_mtime,
            latest_non_dino_log_mtime,
        ]
        .into_iter()
        .max()
        .unwrap_or(0);

        if updated_at == 0 || now.saturating_sub(updated_at) > max_age {
            continue;
        }

        let workspace_str = workspace_path_buf
            .as_ref()
            .map(|p| p.to_string_lossy().to_string());
        let is_axiom_workspace = workspace_str
            .as_ref()
            .map(|w| w.to_lowercase().contains("axiom"))
            .unwrap_or(false);

        // Parse transcript tail for latest prompt, tool calls, running status, approvals, pause signals, and benchmark updates
        let mut last_user_prompt: Option<String> = None;
        let mut last_tool_summary: Option<String> = None;
        let mut last_step_type = String::new();
        let mut last_planner_has_tools = false;
        let mut has_running_tool = false;
        let mut tools_total = 0u32;
        let mut tools_done = 0u32;
        let mut is_subagent_conv = false;
        let mut is_waiting_permission = false;
        let mut conv_is_paused = false;
        let mut pause_signal_ts = 0u64;
        let mut pending_question: Option<String> = None;

        if let Some((_, ref tail)) = head_tail {
            let mut parsed_entries: Vec<Value> = Vec::new();
            for line in tail.lines() {
                if let Ok(val) = serde_json::from_str::<Value>(line) {
                    parsed_entries.push(val);
                }
            }

            if let Some(last) = parsed_entries.last() {
                last_step_type = last
                    .get("type")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
            }

            let last_user_idx = parsed_entries
                .iter()
                .rposition(|v| v.get("type").and_then(|t| t.as_str()) == Some("USER_INPUT"));
            let last_sys_idx = parsed_entries
                .iter()
                .rposition(|v| v.get("type").and_then(|t| t.as_str()) == Some("SYSTEM_MESSAGE"));
            is_subagent_conv = last_user_idx.is_none() && last_sys_idx.is_some();

            let last_trigger_idx = last_user_idx.or(last_sys_idx);
            if let Some(tidx) = last_trigger_idx {
                if let Some(content) = parsed_entries[tidx].get("content").and_then(|v| v.as_str()) {
                    last_user_prompt = clean_user_prompt_summary(content);
                    if !is_subagent_conv {
                        if let Some(ref summary) = last_user_prompt {
                            if summary.len() <= 140 && text_mentions_pause(summary) {
                                conv_is_paused = true;
                            }
                        }
                    }
                }
            }

            let turn_slice = match last_trigger_idx {
                Some(idx) => &parsed_entries[idx..],
                None => &parsed_entries[..],
            };

            let mut turn_killed_subagents = false;
            let mut last_planner_content = String::new();
            for entry in turn_slice {
                let etype = entry.get("type").and_then(|t| t.as_str()).unwrap_or("");
                let estatus = entry.get("status").and_then(|s| s.as_str()).unwrap_or("");

                if etype == "PLANNER_RESPONSE" {
                    if let Some(content) = entry.get("content").and_then(|v| v.as_str()) {
                        last_planner_content = content.to_string();
                    }
                    if let Some(calls) = entry.get("tool_calls").and_then(|c| c.as_array()) {
                        if !calls.is_empty() {
                            last_planner_has_tools = true;
                            tools_total += calls.len() as u32;
                            for call in calls {
                                let cname = call.get("name").and_then(|v| v.as_str()).unwrap_or("");
                                if cname == "manage_subagents" {
                                    let action = call
                                        .get("args")
                                        .and_then(|a| a.get("Action").or_else(|| a.get("action")))
                                        .and_then(unquote_json_arg)
                                        .unwrap_or_default()
                                        .to_lowercase();
                                    if action == "kill" || action == "kill_all" {
                                        turn_killed_subagents = true;
                                    }
                                }
                            }
                            if let Some(last_call) = calls.last() {
                                if let Some(summary) = format_tool_summary(last_call) {
                                    last_tool_summary = Some(summary);
                                }
                            }
                        } else {
                            last_planner_has_tools = false;
                        }
                    } else {
                        last_planner_has_tools = false;
                    }
                } else if etype == "GENERIC" {
                    if estatus == "RUNNING" {
                        has_running_tool = true;
                    } else if estatus == "DONE" {
                        tools_done += 1;
                        has_running_tool = false;
                    }
                }

                if let Some(content) = entry.get("content").and_then(|v| v.as_str()) {
                    if is_axiom_workspace || content.contains("/960") || content.contains("-v3") {
                        for cline in content.lines() {
                            if let Some(rec) = parse_benchmark_line(
                                cline,
                                transcript_mtime,
                                workspace_str.as_deref(),
                                1,
                            ) {
                                upsert_benchmark_record(&mut benchmark_runs, rec);
                            }
                        }
                    }
                }
            }

            if tools_done > tools_total {
                tools_done = tools_total;
            }

            let planner_declares_pause = !last_planner_has_tools
                && !last_planner_content.is_empty()
                && (last_planner_content.to_lowercase().contains("have been stopped")
                    || last_planner_content.to_lowercase().contains("saved checkpoints")
                    || last_planner_content.to_lowercase().contains("paused at checkpoint"));

            if !has_running_tool && (turn_killed_subagents || planner_declares_pause) {
                conv_is_paused = true;
            }
            if has_running_tool && !turn_killed_subagents && !planner_declares_pause {
                conv_is_paused = false;
            }
            if conv_is_paused {
                pause_signal_ts = transcript_mtime;
            }

            if !conv_is_paused {
                if let Some((approval_label, approval_question)) = detect_pending_approval(
                    &parsed_entries,
                    now,
                    transcript_mtime,
                    latest_non_dino_log_mtime,
                    is_subagent_conv,
                ) {
                    is_waiting_permission = true;
                    pending_question = Some(approval_question);
                    last_tool_summary = Some(approval_label);
                }
            }
        }

        let mentioned_run_ids = if !combined_text.is_empty() {
            extract_all_run_ids(&combined_text)
        } else {
            Vec::new()
        };

        // Extract benchmark context if this conversation is a subagent running a specific run-id
        let subagent_ctx = if is_subagent_conv && !combined_text.is_empty() {
            extract_subagent_benchmark_context(&combined_text)
        } else {
            None
        };

        // Scan recent task-*.log files for live Axiom / daemon / subagent trial progress lines
        for (log_path, log_mtime) in task_logs.iter().take(6) {
            if now.saturating_sub(*log_mtime) > max_age {
                continue;
            }
            if let Some(tail) = read_tail_text(log_path, 32_768) {
                if tail.contains("[DinoBuds]") {
                    continue;
                }
                for line in tail.lines() {
                    if let Some(rec) = parse_benchmark_line(
                        line,
                        *log_mtime,
                        workspace_str.as_deref(),
                        2,
                    ) {
                        upsert_benchmark_record(&mut benchmark_runs, rec);
                    }
                    if let Some((ref run_id, ref model_opt)) = subagent_ctx {
                        if let Some(rec) = parse_subagent_trial_line(
                            line,
                            run_id,
                            model_opt.as_deref(),
                            &conv_id,
                            *log_mtime,
                            workspace_str.as_deref(),
                        ) {
                            upsert_benchmark_record(&mut benchmark_runs, rec);
                        }
                    }
                }
            }
        }

        let task_md = if task_md_path.is_file() {
            parse_task_md(&task_md_path)
        } else {
            None
        };

        let recent_activity = now.saturating_sub(transcript_mtime.max(latest_non_dino_log_mtime)) <= 120;
        let is_running = if conv_is_paused {
            false
        } else if is_waiting_permission {
            true
        } else if let Some(ref tm) = task_md {
            if tm.total > 0 && tm.completed == tm.total && tm.in_progress == 0 && !recent_activity {
                false
            } else {
                recent_activity
                    || last_step_type == "USER_INPUT"
                    || (last_step_type == "PLANNER_RESPONSE" && last_planner_has_tools)
                    || has_running_tool
                    || (tm.in_progress > 0 && now.saturating_sub(updated_at) <= 600)
            }
        } else {
            recent_activity
                || last_step_type == "USER_INPUT"
                || (last_step_type == "PLANNER_RESPONSE" && last_planner_has_tools)
                || has_running_tool
        };

        convs.push(DiscoveredConv {
            id: conv_id,
            workspace_path: workspace_str,
            updated_at,
            latest_log_mtime: latest_non_dino_log_mtime,
            is_running,
            is_waiting_permission,
            is_paused: conv_is_paused,
            is_subagent: is_subagent_conv,
            pause_signal_ts,
            pending_question,
            mentioned_run_ids,
            task_md,
            last_user_prompt,
            last_tool_summary,
            tools_done,
            tools_total,
        });
    }

    // Track the newest pause-all signal timestamp per workspace (from main conversations)
    let mut pause_ts_by_workspace: HashMap<String, u64> = HashMap::new();
    let mut axiom_global_pause_ts = 0u64;
    for conv in &convs {
        if conv.is_paused && !conv.is_subagent && conv.pause_signal_ts > 0 {
            if let Some(ref ws) = conv.workspace_path {
                let ws_key = ws.to_lowercase();
                let entry = pause_ts_by_workspace.entry(ws_key.clone()).or_insert(0);
                *entry = (*entry).max(conv.pause_signal_ts);
                if ws_key.contains("axiom") {
                    axiom_global_pause_ts = axiom_global_pause_ts.max(conv.pause_signal_ts);
                }
            }
        }
    }

    // Supersede stale waiting_permission flags on subagents/convs older than their workspace's pause turn
    for conv in &mut convs {
        let ws_pause_ts = conv
            .workspace_path
            .as_ref()
            .and_then(|ws| pause_ts_by_workspace.get(&ws.to_lowercase()).copied())
            .unwrap_or(0);
        if conv.is_paused || (ws_pause_ts > 0 && conv.updated_at <= ws_pause_ts + 5) {
            conv.is_waiting_permission = false;
            conv.pending_question = None;
            if conv.is_subagent || ws_pause_ts > 0 {
                conv.is_paused = true;
                conv.is_running = false;
            }
        }
    }

    // Source B: Scan Axiom directories on disk
    let axiom_dirs = find_axiom_dirs(&discovered_workspaces, Some(profile));
    for axiom_dir in &axiom_dirs {
        let ws_str = axiom_dir.to_string_lossy().to_string();
        scan_axiom_dir_recursive(axiom_dir, 0, now, &ws_str, &mut benchmark_runs);
    }

    // Link dedicated conversations/subagents referencing a specific --run-id to their benchmark record
    for conv in &convs {
        if conv.mentioned_run_ids.len() == 1
            || (conv.is_waiting_permission
                && conv
                    .pending_question
                    .as_deref()
                    .map(|q| !extract_all_run_ids(q).is_empty())
                    .unwrap_or(false))
        {
            for run_id in &conv.mentioned_run_ids {
                let key = run_id.to_lowercase();
                if let Some(rec) = benchmark_runs.get_mut(&key) {
                    let set_primary = rec.linked_uuid.is_none();
                    merge_benchmark_uuids(rec, Some(conv.id.clone()), Vec::new(), set_primary);
                }
            }
        }
    }

    // Build lookup map of conversations for merging linked UUIDs from benchmark runs
    let conv_by_id: HashMap<String, &DiscoveredConv> =
        convs.iter().map(|c| (c.id.clone(), c)).collect();
    let mut consumed_conv_ids: HashSet<String> = HashSet::new();
    let mut sessions: Vec<AgentSession> = Vec::new();
    let live_axiom_runs = list_live_axiom_run_ids();

    for (_, mut rec) in benchmark_runs {
        let disk_or_log_mtime = rec.updated_at;
        let mut candidate_uuids: Vec<String> = Vec::new();
        if let Some(ref uuid) = rec.linked_uuid {
            candidate_uuids.push(uuid.clone());
        }
        for uuid in &rec.extra_uuids {
            if !candidate_uuids.contains(uuid) {
                candidate_uuids.push(uuid.clone());
            }
        }
        for conv in &convs {
            if conv
                .mentioned_run_ids
                .iter()
                .any(|r| r.eq_ignore_ascii_case(&rec.name))
                && !candidate_uuids.contains(&conv.id)
            {
                candidate_uuids.push(conv.id.clone());
            }
        }

        // Sort candidates by updated_at descending so the newest retry subagent takes precedence
        candidate_uuids.sort_by(|a, b| {
            let ua = conv_by_id.get(a).map(|c| c.updated_at).unwrap_or(0);
            let ub = conv_by_id.get(b).map(|c| c.updated_at).unwrap_or(0);
            ub.cmp(&ua)
        });

        let mut max_linked_log_mtime = 0u64;
        let mut conv_explicitly_paused = false;
        let mut waiting_conv_question: Option<(u64, String, String)> = None;

        for uuid in candidate_uuids {
            if let Some(conv) = conv_by_id.get(&uuid) {
                if rec.linked_uuid.as_deref() == Some(uuid.as_str())
                    || rec.extra_uuids.contains(&uuid)
                {
                    consumed_conv_ids.insert(uuid.clone());
                }
                rec.updated_at = rec.updated_at.max(conv.updated_at);
                if rec.workspace_path.is_none() {
                    rec.workspace_path = conv.workspace_path.clone();
                }

                max_linked_log_mtime = max_linked_log_mtime.max(conv.latest_log_mtime);
                if conv.is_paused {
                    conv_explicitly_paused = true;
                }

                if conv.is_waiting_permission && !conv.is_paused {
                    let q_mentions_other = conv
                        .pending_question
                        .as_deref()
                        .map(|q| {
                            let ids = extract_all_run_ids(q);
                            !ids.is_empty() && !ids.iter().any(|r| r.eq_ignore_ascii_case(&rec.name))
                        })
                        .unwrap_or(false);
                    if !q_mentions_other && waiting_conv_question.is_none() {
                        waiting_conv_question = Some((
                            conv.updated_at,
                            conv.last_tool_summary
                                .clone()
                                .unwrap_or_else(|| "Needs Approval".to_string()),
                            conv.pending_question
                                .clone()
                                .unwrap_or_else(|| "Allow command? — waiting for Submit ⏎".to_string()),
                        ));
                    }
                }
            }
        }

        let ws_pause_ts = rec
            .workspace_path
            .as_ref()
            .and_then(|ws| pause_ts_by_workspace.get(&ws.to_lowercase()).copied())
            .unwrap_or(axiom_global_pause_ts);
        let last_active_write = disk_or_log_mtime.max(max_linked_log_mtime);

        if rec.current_step < rec.total_steps {
            let is_live_process = live_axiom_runs.contains(&rec.name.to_lowercase());
            let has_live_approval = waiting_conv_question
                .as_ref()
                .map(|(wait_ts, _, _)| *wait_ts > ws_pause_ts && !conv_explicitly_paused)
                .unwrap_or(false);

            if has_live_approval {
                if let Some((_, step_lbl, q)) = waiting_conv_question {
                    rec.status = "waiting_permission".to_string();
                    rec.step_label = step_lbl;
                    rec.question = Some(q);
                }
            } else if is_live_process {
                rec.status = "running".to_string();
                rec.question = None;
                rec.updated_at = rec.updated_at.max(now);
            } else if rec.current_step > 0
                && (now.saturating_sub(last_active_write) > 30
                    || conv_explicitly_paused
                    || (ws_pause_ts > 0 && last_active_write <= ws_pause_ts + 10))
            {
                rec.status = "paused".to_string();
                rec.step_label = format!(
                    "Paused at checkpoint ({}/{})",
                    rec.current_step, rec.total_steps
                );
                rec.question = None;
            } else if rec.current_step > 0 {
                rec.status = "running".to_string();
                rec.question = None;
            }
        }

        if rec.workspace_path.is_none() {
            rec.workspace_path = axiom_dirs
                .first()
                .map(|p| p.to_string_lossy().to_string());
        }

        sessions.push(AgentSession {
            id: format!("axiom-{}", sanitize_id(&rec.name)),
            name: rec.name,
            task: rec.task,
            status: rec.status,
            current_step: rec.current_step,
            total_steps: rec.total_steps,
            step_label: rec.step_label,
            question: rec.question,
            workspace_path: rec.workspace_path,
            decision: None,
            updated_at: rec.updated_at,
        });
    }

    // Add Source A Antigravity conversation cards (skipping ones already represented by benchmark cards)
    for conv in convs {
        if consumed_conv_ids.contains(&conv.id) {
            continue;
        }
        if conv.task_md.is_none() && conv.last_user_prompt.is_none() {
            continue;
        }

        let (current_step, total_steps, step_label, task_title) = if let Some(ref tm) = conv.task_md
        {
            let title = tm
                .title
                .clone()
                .or_else(|| conv.last_user_prompt.clone())
                .unwrap_or_else(|| "Antigravity Task".to_string());
            let label = if conv.is_paused {
                if tm.total > 0 && tm.completed < tm.total {
                    format!("Paused at checkpoint ({}/{})", tm.completed, tm.total)
                } else {
                    "Paused".to_string()
                }
            } else if conv.is_waiting_permission {
                conv.last_tool_summary
                    .clone()
                    .unwrap_or_else(|| "Needs Approval".to_string())
            } else {
                tm.active_label.clone().unwrap_or_else(|| {
                    if tm.total > 0 && tm.completed == tm.total {
                        "Completed".to_string()
                    } else {
                        conv.last_tool_summary
                            .clone()
                            .unwrap_or_else(|| "Active".to_string())
                    }
                })
            };
            (tm.completed, tm.total, label, title)
        } else {
            let title = conv
                .last_user_prompt
                .clone()
                .unwrap_or_else(|| "Antigravity Session".to_string());
            let label = if conv.is_paused {
                if conv.tools_total > 0 && conv.tools_done < conv.tools_total {
                    format!(
                        "Paused at checkpoint ({}/{})",
                        conv.tools_done, conv.tools_total
                    )
                } else {
                    "Paused".to_string()
                }
            } else {
                conv.last_tool_summary
                    .clone()
                    .unwrap_or_else(|| if conv.is_running { "Running" } else { "Idle" }.to_string())
            };
            (conv.tools_done, conv.tools_total, label, title)
        };

        let status = if conv.is_paused {
            "paused"
        } else if conv.is_waiting_permission {
            "waiting_permission"
        } else if conv.is_running {
            "running"
        } else {
            "done"
        }
        .to_string();

        let question = if status == "paused" {
            None
        } else {
            conv.pending_question
        };

        sessions.push(AgentSession {
            id: conv.id,
            name: "Antigravity".to_string(),
            task: truncate_str(&task_title, 96),
            status,
            current_step,
            total_steps,
            step_label: truncate_str(&step_label, 42),
            question,
            workspace_path: conv.workspace_path,
            decision: None,
            updated_at: conv.updated_at,
        });
    }

    sessions
}

fn spawn_auto_discovery_watcher(
    app_handle: AppHandle,
    shared: SharedSessions,
    manual: ManualSessions,
    dismissed: DismissedSessions,
    approved_prompts: ApprovedPrompts,
    paused_sessions: PausedSessions,
    resumed_sessions: ResumedSessions,
    auto_cache: AutoDiscoveredCache,
) {
    std::thread::spawn(move || loop {
        match std::panic::catch_unwind(scan_brain_and_axiom) {
            Ok(discovered) => {
                {
                    let mut cache = lock_or_recover(&auto_cache.0);
                    *cache = discovered.clone();
                }
                let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    reconcile_and_emit(
                        &shared,
                        &manual,
                        &dismissed,
                        &approved_prompts,
                        &paused_sessions,
                        &resumed_sessions,
                        &discovered,
                        &app_handle,
                    );
                }));
            }
            Err(_) => {
                eprintln!("[DinoBuds] auto-discovery scan panicked; will retry on next cycle");
            }
        }
        std::thread::sleep(Duration::from_secs(2));
    });
}

// ============================================================================
// Embedded HTTP Telemetry Server (127.0.0.1:4747)
// ============================================================================

fn cors_headers() -> Vec<Header> {
    let pairs: &[(&[u8], &[u8])] = &[
        (b"Access-Control-Allow-Origin", b"*"),
        (
            b"Access-Control-Allow-Methods",
            b"GET, POST, DELETE, OPTIONS",
        ),
        (b"Access-Control-Allow-Headers", b"Content-Type"),
        (b"Content-Type", b"application/json"),
    ];
    pairs
        .iter()
        .filter_map(|(k, v)| Header::from_bytes(*k, *v).ok())
        .collect()
}

fn spawn_telemetry_server(
    app_handle: AppHandle,
    shared: SharedSessions,
    manual: ManualSessions,
    dismissed: DismissedSessions,
    approved_prompts: ApprovedPrompts,
    paused_sessions: PausedSessions,
    resumed_sessions: ResumedSessions,
    auto_cache: AutoDiscoveredCache,
) {
    std::thread::spawn(move || {
        let server = match Server::http("127.0.0.1:4747") {
            Ok(s) => {
                println!("[DinoBuds] Telemetry server listening on http://127.0.0.1:4747");
                s
            }
            Err(e) => {
                eprintln!(
                    "[DinoBuds] Warning: Port 127.0.0.1:4747 already in use ({}), continuing without duplicate listener",
                    e
                );
                return;
            }
        };

        for mut request in server.incoming_requests() {
            let method = request.method().clone();
            let url = request.url().to_string();

            if method == Method::Options {
                let mut res = Response::from_string("").with_status_code(StatusCode(204));
                for h in cors_headers() {
                    res.add_header(h);
                }
                let _ = request.respond(res);
                continue;
            }

            if method == Method::Get && (url == "/agent" || url == "/agents") {
                let list = lock_or_recover(&shared).clone();
                let body = serde_json::to_string(&list).unwrap_or_else(|_| "[]".to_string());
                let mut res = Response::from_string(body).with_status_code(StatusCode(200));
                for h in cors_headers() {
                    res.add_header(h);
                }
                let _ = request.respond(res);
                continue;
            }

            if method == Method::Get && url.starts_with("/agent/") {
                let path_part = url.trim_start_matches("/agent/").trim_end_matches('/');
                let id = path_part
                    .strip_suffix("/decision")
                    .unwrap_or(path_part)
                    .trim();

                let found = {
                    let sessions = lock_or_recover(&shared);
                    sessions.iter().find(|s| s.id == id).cloned()
                };

                if let Some(session) = found {
                    let body = serde_json::to_string(&session).unwrap_or_else(|_| "{}".to_string());
                    let mut res = Response::from_string(body).with_status_code(StatusCode(200));
                    for h in cors_headers() {
                        res.add_header(h);
                    }
                    let _ = request.respond(res);
                } else {
                    let mut res = Response::from_string("{\"error\":\"Session not found\"}")
                        .with_status_code(StatusCode(404));
                    for h in cors_headers() {
                        res.add_header(h);
                    }
                    let _ = request.respond(res);
                }
                continue;
            }

            if method == Method::Post && url.starts_with("/agent") {
                let mut content = String::new();
                if request.as_reader().read_to_string(&mut content).is_ok() {
                    match serde_json::from_str::<AgentSession>(&content) {
                        Ok(mut incoming) => {
                            if incoming.updated_at == 0 {
                                incoming.updated_at = now_secs();
                            }
                            if incoming.status == "waiting_permission" {
                                incoming.decision = None;
                            }
                            {
                                let mut dismissed_map = lock_or_recover(&dismissed);
                                dismissed_map.remove(&incoming.id);
                            }
                            if incoming.status != "paused" {
                                let mut paused_map = lock_or_recover(&paused_sessions.0);
                                if paused_map.remove(&incoming.id).is_some() {
                                    save_paused_ids(&paused_map);
                                }
                            }
                            {
                                let mut manual_map = lock_or_recover(&manual);
                                manual_map.insert(incoming.id.clone(), incoming);
                            }

                            let cached_auto = lock_or_recover(&auto_cache.0).clone();
                            let updated_list = reconcile_and_emit(
                                &shared,
                                &manual,
                                &dismissed,
                                &approved_prompts,
                                &paused_sessions,
                                &resumed_sessions,
                                &cached_auto,
                                &app_handle,
                            );

                            let body = serde_json::to_string(&updated_list)
                                .unwrap_or_else(|_| "[]".to_string());
                            let mut res =
                                Response::from_string(body).with_status_code(StatusCode(200));
                            for h in cors_headers() {
                                res.add_header(h);
                            }
                            let _ = request.respond(res);
                        }
                        Err(err) => {
                            let body = serde_json::json!({ "error": err.to_string() }).to_string();
                            let mut res =
                                Response::from_string(body).with_status_code(StatusCode(400));
                            for h in cors_headers() {
                                res.add_header(h);
                            }
                            let _ = request.respond(res);
                        }
                    }
                } else {
                    let mut res = Response::from_string("{\"error\":\"Failed to read body\"}")
                        .with_status_code(StatusCode(400));
                    for h in cors_headers() {
                        res.add_header(h);
                    }
                    let _ = request.respond(res);
                }
                continue;
            }

            if method == Method::Delete && url.starts_with("/agent/") {
                let id = url.trim_start_matches("/agent/").trim().to_string();
                {
                    let mut dismissed_map = lock_or_recover(&dismissed);
                    dismissed_map.insert(id.clone(), now_secs());
                }
                {
                    let mut manual_map = lock_or_recover(&manual);
                    manual_map.remove(&id);
                }
                {
                    let mut paused_map = lock_or_recover(&paused_sessions.0);
                    if paused_map.remove(&id).is_some() {
                        save_paused_ids(&paused_map);
                    }
                }
                let cached_auto = lock_or_recover(&auto_cache.0).clone();
                let updated_list = reconcile_and_emit(
                    &shared,
                    &manual,
                    &dismissed,
                    &approved_prompts,
                    &paused_sessions,
                    &resumed_sessions,
                    &cached_auto,
                    &app_handle,
                );

                let body =
                    serde_json::to_string(&updated_list).unwrap_or_else(|_| "[]".to_string());
                let mut res = Response::from_string(body).with_status_code(StatusCode(200));
                for h in cors_headers() {
                    res.add_header(h);
                }
                let _ = request.respond(res);
                continue;
            }

            let mut res = Response::from_string("{\"error\":\"Not found\"}")
                .with_status_code(StatusCode(404));
            for h in cors_headers() {
                res.add_header(h);
            }
            let _ = request.respond(res);
        }
    });
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct GitCommitEvidence {
    pub hash: String,
    pub timestamp: String,
    pub repo: String,
    pub message: String,
    #[serde(default)]
    pub files_changed: u32,
    #[serde(default)]
    pub insertions: u32,
    #[serde(default)]
    pub deletions: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ShiftSession {
    pub start_ts: u64,
    pub date_str: String,
    pub time_in_str: String,
    #[serde(default)]
    pub notes: Vec<String>,
    #[serde(default)]
    pub active_minutes: u32,
    #[serde(default)]
    pub idle_minutes: u32,
    #[serde(default)]
    pub network_stamp: Option<String>,
    #[serde(default)]
    pub app_minutes: HashMap<String, u32>,
    #[serde(default)]
    pub app_durations: HashMap<String, u32>,
    #[serde(default)]
    pub commit_evidence: Vec<GitCommitEvidence>,
    #[serde(default)]
    pub challenges: String,
    #[serde(default)]
    pub learnings: String,
    #[serde(default)]
    pub is_paused: bool,
    #[serde(default)]
    pub pause_started_ts: Option<u64>,
    #[serde(default)]
    pub total_paused_seconds: u64,
    #[serde(default)]
    pub break_logs: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompletedShift {
    pub id: String,
    pub date_str: String,
    pub time_in: String,
    pub time_out: String,
    pub duration_hours: f64,
    #[serde(default)]
    pub activities: Vec<String>,
    #[serde(default)]
    pub summary_markdown: String,
    #[serde(default)]
    pub active_minutes: u32,
    #[serde(default)]
    pub idle_minutes: u32,
    #[serde(default)]
    pub focus_percent: f32,
    #[serde(default)]
    pub network_stamp_in: Option<String>,
    #[serde(default)]
    pub network_stamp_out: Option<String>,
    #[serde(default)]
    pub app_breakdown: HashMap<String, u32>,
    #[serde(default)]
    pub app_durations: HashMap<String, u32>,
    #[serde(default)]
    pub commit_evidence: Vec<GitCommitEvidence>,
    #[serde(default)]
    pub challenges: String,
    #[serde(default)]
    pub learnings: String,
    #[serde(default)]
    pub verification_hash: String,
    #[serde(default)]
    pub total_paused_seconds: u64,
    #[serde(default)]
    pub break_logs: Vec<String>,
}

fn default_target_hours() -> f64 {
    486.0
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OjtStateFile {
    #[serde(default = "default_target_hours")]
    pub target_hours: f64,
    #[serde(default)]
    pub watched_repos: Vec<String>,
    #[serde(default)]
    pub obsidian_export_dir: Option<String>,
    #[serde(default)]
    pub active_shift: Option<ShiftSession>,
    #[serde(default)]
    pub completed_shifts: Vec<CompletedShift>,
    #[serde(default)]
    pub today_date: String,
    #[serde(default)]
    pub today_activities: Vec<String>,
    #[serde(default)]
    pub today_commits: Vec<GitCommitEvidence>,
    #[serde(default)]
    pub today_challenges: String,
    #[serde(default)]
    pub today_learnings: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OjtDashboardResponse {
    pub target_hours: f64,
    pub rendered_hours: f64,
    pub remaining_hours: f64,
    pub progress_percent: f32,
    pub shifts_remaining: u32,
    pub estimated_completion_date: String,
    pub watched_repos: Vec<String>,
    pub obsidian_export_dir: Option<String>,
    pub active_shift: Option<ShiftSession>,
    pub completed_shifts: Vec<CompletedShift>,
    pub today_activities: Vec<String>,
    pub today_commits: Vec<GitCommitEvidence>,
    pub today_challenges: String,
    pub today_learnings: String,
    pub current_focus_percent: f32,
    pub top_active_apps: Vec<(String, u32)>,
    pub current_network_stamp: String,
}

type SharedOjtState = Arc<Mutex<OjtStateFile>>;

#[cfg(target_os = "windows")]
#[repr(C)]
struct WinSystemTime {
    w_year: u16,
    w_month: u16,
    w_day_of_week: u16,
    w_day: u16,
    w_hour: u16,
    w_minute: u16,
    w_second: u16,
    w_milliseconds: u16,
}

#[cfg(target_os = "windows")]
#[repr(C)]
struct LastInputInfo {
    cb_size: u32,
    dw_time: u32,
}

#[cfg(target_os = "windows")]
unsafe extern "system" {
    fn GetLocalTime(lp_system_time: *mut WinSystemTime);
    fn GetTickCount() -> u32;
    fn GetLastInputInfo(plii: *mut LastInputInfo) -> i32;
    fn GetForegroundWindow() -> isize;
    fn GetWindowTextW(h_wnd: isize, lp_string: *mut u16, n_max_count: i32) -> i32;
    fn GetWindowThreadProcessId(h_wnd: isize, lpdw_process_id: *mut u32) -> u32;
}

#[derive(Debug, Clone, Copy)]
struct LocalClock {
    year: i64,
    month: u32,
    day: u32,
    hour: u32,
    minute: u32,
    unix_secs: u64,
}

fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let m = m as i64;
    let d = d as i64;
    let y = y - if m <= 2 { 1 } else { 0 };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let doy = (153 * (m + if m > 2 { -3 } else { 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = (mp + if mp < 10 { 3 } else { -9 }) as u32;
    let y = y + if m <= 2 { 1 } else { 0 };
    (y, m, d)
}

fn current_local_clock() -> LocalClock {
    let unix_secs = now_secs();
    #[cfg(target_os = "windows")]
    {
        let mut st = WinSystemTime {
            w_year: 0,
            w_month: 0,
            w_day_of_week: 0,
            w_day: 0,
            w_hour: 0,
            w_minute: 0,
            w_second: 0,
            w_milliseconds: 0,
        };
        unsafe {
            GetLocalTime(&mut st);
        }
        return LocalClock {
            year: st.w_year as i64,
            month: st.w_month as u32,
            day: st.w_day as u32,
            hour: st.w_hour as u32,
            minute: st.w_minute as u32,
            unix_secs,
        };
    }
    #[cfg(not(target_os = "windows"))]
    {
        let days = (unix_secs / 86400) as i64;
        let rem = (unix_secs % 86400) as u32;
        let (year, month, day) = civil_from_days(days);
        LocalClock {
            year,
            month,
            day,
            hour: rem / 3600,
            minute: (rem % 3600) / 60,
            unix_secs,
        }
    }
}

fn format_date(clock: LocalClock) -> String {
    format!("{:04}-{:02}-{:02}", clock.year, clock.month, clock.day)
}

fn format_hm(clock: LocalClock) -> String {
    format!("{:02}:{:02}", clock.hour, clock.minute)
}

fn estimate_completion_date(clock: LocalClock, shifts_remaining: u32) -> String {
    if shifts_remaining == 0 {
        return format_date(clock);
    }
    let mut days = days_from_civil(clock.year, clock.month, clock.day);
    let mut left = shifts_remaining;
    while left > 0 {
        days += 1;
        let dow = (days + 4).rem_euclid(7); // 0 = Sun, 1..=5 = Mon..Fri, 6 = Sat
        if (1..=5).contains(&dow) {
            left -= 1;
        }
    }
    let (ey, em, ed) = civil_from_days(days);
    format!("{:04}-{:02}-{:02}", ey, em, ed)
}

fn ojt_state_file_path() -> Option<PathBuf> {
    get_user_profile_dir().map(|home| home.join(".dinobuds").join("ojt-state.json"))
}

fn discover_default_watched_repos() -> Vec<String> {
    let mut repos: Vec<String> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();

    if let Some(home) = get_user_profile_dir() {
        for priority in ["dinobuds", "axiom", "brain-web"] {
            let candidate = home.join(priority);
            if candidate.join(".git").exists() {
                let s = candidate.to_string_lossy().to_string();
                let key = s.to_lowercase();
                if seen.insert(key) {
                    repos.push(s);
                }
            }
        }

        if let Ok(entries) = fs::read_dir(&home) {
            for entry in entries.flatten() {
                if repos.len() >= 12 {
                    break;
                }
                let path = entry.path();
                if !path.is_dir() {
                    continue;
                }
                let name = entry.file_name().to_string_lossy().to_string();
                if name.starts_with('.') {
                    continue;
                }
                if path.join(".git").exists() {
                    let s = path.to_string_lossy().to_string();
                    let key = s.to_lowercase();
                    if seen.insert(key) {
                        repos.push(s);
                    }
                }
            }
        }
    }

    repos
}

fn reconcile_ojt_day(state: &mut OjtStateFile, today: &str) {
    if state.today_date.is_empty() {
        state.today_date = today.to_string();
    } else if state.today_date != today {
        state.today_date = today.to_string();
        state.today_activities.clear();
        state.today_commits.clear();
        state.today_challenges.clear();
        state.today_learnings.clear();
    }
    if state.target_hours <= 0.0 || !state.target_hours.is_finite() {
        state.target_hours = default_target_hours();
    }
}

fn default_workstation_stamp() -> String {
    let host = std::env::var("COMPUTERNAME")
        .or_else(|_| std::env::var("HOSTNAME"))
        .unwrap_or_else(|_| "Local".to_string());
    let cleaned = host.trim();
    if cleaned.is_empty() {
        "Workstation: Local".to_string()
    } else {
        format!("Workstation: {}", cleaned)
    }
}

fn capture_network_stamp() -> String {
    #[cfg(target_os = "windows")]
    {
        let mut cmd = Command::new("netsh");
        cmd.args(["wlan", "show", "interfaces"]);
        cmd.creation_flags(0x08000000);
        if let Ok(output) = cmd.output() {
            if output.status.success() {
                let text = String::from_utf8_lossy(&output.stdout);
                let mut connected = false;
                let mut ssid: Option<String> = None;
                let mut bssid: Option<String> = None;
                for raw_line in text.lines() {
                    let line = raw_line.trim();
                    if let Some((key, val)) = line.split_once(':') {
                        let k = key.trim();
                        let v = val.trim();
                        if k.eq_ignore_ascii_case("State") {
                            connected = v.eq_ignore_ascii_case("connected");
                        } else if k.eq_ignore_ascii_case("SSID") && !v.is_empty() && v != "None" {
                            ssid = Some(v.to_string());
                        } else if k.eq_ignore_ascii_case("BSSID") && !v.is_empty() && v != "None" {
                            bssid = Some(v.to_string());
                        }
                    }
                }
                if let Some(name) = ssid {
                    if connected {
                        if let Some(b) = bssid {
                            return format!("Wi-Fi: {} (BSSID: {})", name, b);
                        }
                        return format!("Wi-Fi: {}", name);
                    }
                }
            }
        }
    }
    default_workstation_stamp()
}

fn cached_startup_network_stamp() -> String {
    static STARTUP_STAMP: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    STARTUP_STAMP.get_or_init(capture_network_stamp).clone()
}

fn extract_sanitized_doc_name(title: &str, suffixes: &[&str]) -> Option<String> {
    let mut base = title.trim();
    for suf in suffixes {
        if let Some(stripped) = base.strip_suffix(suf) {
            base = stripped.trim();
            break;
        }
    }
    if base.is_empty() || base.eq_ignore_ascii_case("Excel") || base.eq_ignore_ascii_case("Word") {
        return None;
    }
    let filename = base
        .rsplit(['\\', '/'])
        .next()
        .unwrap_or(base)
        .trim();
    if filename.is_empty() {
        None
    } else {
        Some(truncate_str(filename, 30))
    }
}

fn sanitize_app_and_doc_label(proc_name: &str, title: &str) -> Option<(String, String)> {
    let p_lower = proc_name.trim().to_lowercase();
    let t_lower = title.trim().to_lowercase();

    if p_lower == "excel.exe" || t_lower.ends_with(" - excel") {
        let doc = extract_sanitized_doc_name(title, &[" - Excel", " - Microsoft Excel"]);
        let detail = match doc {
            Some(d) => format!("Microsoft Excel — {}", d),
            None => "Microsoft Excel (Data Encoding)".to_string(),
        };
        return Some(("Excel".to_string(), detail));
    }
    if p_lower == "winword.exe" || t_lower.ends_with(" - word") {
        let doc = extract_sanitized_doc_name(title, &[" - Word", " - Microsoft Word"]);
        let detail = match doc {
            Some(d) => format!("Microsoft Word — {}", d),
            None => "Microsoft Word (Documentation)".to_string(),
        };
        return Some(("Word".to_string(), detail));
    }
    if p_lower == "powerpnt.exe" || t_lower.ends_with(" - powerpoint") {
        let doc = extract_sanitized_doc_name(title, &[" - PowerPoint", " - Microsoft PowerPoint"]);
        let detail = match doc {
            Some(d) => format!("PowerPoint — {}", d),
            None => "PowerPoint".to_string(),
        };
        return Some(("PowerPoint".to_string(), detail));
    }
    if p_lower == "acrord32.exe"
        || p_lower == "acrobat.exe"
        || p_lower.contains("foxit")
        || t_lower.contains("adobe acrobat")
    {
        let doc = extract_sanitized_doc_name(title, &[" - Adobe Acrobat Reader", " - Adobe Acrobat", " - Foxit PDF Reader"]);
        let detail = match doc {
            Some(d) => format!("PDF Reader — {}", d),
            None => "PDF Reader".to_string(),
        };
        return Some(("PDF".to_string(), detail));
    }
    if p_lower == "chrome.exe" || t_lower.contains("google chrome") {
        return Some(("Chrome".to_string(), "Google Chrome".to_string()));
    }
    if p_lower == "msedge.exe" || t_lower.contains("microsoft edge") {
        return Some(("Edge".to_string(), "Microsoft Edge".to_string()));
    }
    if p_lower == "firefox.exe" || t_lower.contains("mozilla firefox") {
        return Some(("Firefox".to_string(), "Firefox".to_string()));
    }
    if p_lower == "antigravity.exe" || t_lower.contains("antigravity") {
        return Some(("Antigravity".to_string(), "Antigravity IDE".to_string()));
    }
    if p_lower == "code.exe" || t_lower.contains("visual studio code") || t_lower.ends_with(" - code") {
        return Some(("VS Code".to_string(), "VS Code".to_string()));
    }
    if p_lower == "obsidian.exe" || t_lower.contains("obsidian") {
        return Some(("Obsidian".to_string(), "Obsidian".to_string()));
    }
    if p_lower == "blender.exe" || t_lower.contains("blender") {
        let doc = extract_sanitized_doc_name(title, &[" - Blender", "Blender"]);
        let detail = match doc {
            Some(d) => format!("Blender — {}", d),
            None => "Blender".to_string(),
        };
        return Some(("Blender".to_string(), detail));
    }
    if p_lower == "figma.exe" || t_lower.contains("figma") {
        return Some(("Figma".to_string(), "Figma".to_string()));
    }
    if p_lower == "windowsterminal.exe"
        || p_lower == "powershell.exe"
        || p_lower == "pwsh.exe"
        || p_lower == "cmd.exe"
        || t_lower.contains("powershell")
        || t_lower.contains("windows terminal")
        || t_lower.contains("command prompt")
    {
        return Some(("Terminal".to_string(), "Terminal".to_string()));
    }
    if p_lower == "dinobuds.exe" || t_lower.contains("dinobuds") || t_lower.contains("ojtpulse") {
        return Some(("DinoBuds".to_string(), "DinoBuds".to_string()));
    }

    let clean_proc = proc_name
        .trim()
        .trim_end_matches(".exe")
        .trim_end_matches(".EXE");
    if !clean_proc.is_empty() {
        let label = truncate_str(clean_proc, 24);
        return Some((label.clone(), label));
    }
    let app_tail = title
        .trim()
        .rsplit(" - ")
        .next()
        .unwrap_or("")
        .trim();
    if app_tail.is_empty() {
        None
    } else {
        let label = truncate_str(app_tail, 24);
        Some((label.clone(), label))
    }
}

fn sample_user_activity() -> (bool, Option<(String, String)>) {
    #[cfg(target_os = "windows")]
    {
        let mut lii = LastInputInfo {
            cb_size: std::mem::size_of::<LastInputInfo>() as u32,
            dw_time: 0,
        };
        let ok = unsafe { GetLastInputInfo(&mut lii) };
        let is_active = if ok != 0 {
            let now_tick = unsafe { GetTickCount() };
            now_tick.wrapping_sub(lii.dw_time) <= 300_000
        } else {
            false
        };

        let hwnd = unsafe { GetForegroundWindow() };
        let app_label = if hwnd != 0 {
            let mut buf = [0u16; 512];
            let len = unsafe { GetWindowTextW(hwnd, buf.as_mut_ptr(), buf.len() as i32) };
            let title = if len > 0 {
                String::from_utf16_lossy(&buf[..len as usize])
            } else {
                String::new()
            };

            let mut pid = 0u32;
            unsafe {
                GetWindowThreadProcessId(hwnd, &mut pid);
            }
            let proc_name = if pid != 0 {
                let mut sys = System::new();
                let target_pid = Pid::from_u32(pid);
                sys.refresh_processes_specifics(
                    ProcessesToUpdate::Some(&[target_pid]),
                    false,
                    ProcessRefreshKind::nothing(),
                );
                sys.process(target_pid)
                    .map(|p| p.name().to_string_lossy().to_string())
                    .unwrap_or_default()
            } else {
                String::new()
            };

            sanitize_app_and_doc_label(&proc_name, &title)
        } else {
            None
        };

        return (is_active, app_label);
    }
    #[cfg(not(target_os = "windows"))]
    {
        (true, None)
    }
}

fn compute_verification_hash(
    date_str: &str,
    time_in: &str,
    time_out: &str,
    rendered_hours: f64,
    commit_hashes: &str,
    active_minutes: u32,
) -> String {
    let payload = format!(
        "{}|{}|{}|{:.2}|{}|{}",
        date_str, time_in, time_out, rendered_hours, commit_hashes, active_minutes
    );
    let mut hash: u64 = 0xcbf29ce484222325;
    for byte in payload.as_bytes() {
        hash ^= *byte as u64;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("OJT-{:016X}", hash)
}

fn parse_ymd(date_str: &str) -> Option<(i64, u32, u32)> {
    let mut parts = date_str.trim().split('-');
    let y = parts.next()?.parse::<i64>().ok()?;
    let m = parts.next()?.parse::<u32>().ok()?;
    let d = parts.next()?.parse::<u32>().ok()?;
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) {
        return None;
    }
    Some((y, m, d))
}

fn iso_week_number(date_str: &str) -> (i64, u32) {
    let (y, m, d) = parse_ymd(date_str).unwrap_or_else(|| {
        let clock = current_local_clock();
        (clock.year, clock.month, clock.day)
    });
    let z = days_from_civil(y, m, d);
    let dow0 = (z + 3).rem_euclid(7);
    let thu = z + (3 - dow0);
    let (ty, _, _) = civil_from_days(thu);
    let jan1 = days_from_civil(ty, 1, 1);
    let week = ((thu - jan1) / 7 + 1) as u32;
    (ty, week)
}

fn day_of_week_name(date_str: &str) -> &'static str {
    let Some((y, m, d)) = parse_ymd(date_str) else {
        return "";
    };
    let z = days_from_civil(y, m, d);
    match (z + 4).rem_euclid(7) {
        0 => "Sun",
        1 => "Mon",
        2 => "Tue",
        3 => "Wed",
        4 => "Thu",
        5 => "Fri",
        6 => "Sat",
        _ => "",
    }
}

fn save_ojt_state(state: &OjtStateFile) {
    if let Some(path) = ojt_state_file_path() {
        if let Some(parent) = path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        if let Ok(json) = serde_json::to_string_pretty(state) {
            let _ = fs::write(&path, json);
        }
    }
}

fn load_ojt_state() -> OjtStateFile {
    let today = format_date(current_local_clock());
    let mut state = ojt_state_file_path()
        .and_then(|p| fs::read_to_string(p).ok())
        .and_then(|raw| serde_json::from_str::<OjtStateFile>(&raw).ok())
        .unwrap_or_else(|| OjtStateFile {
            target_hours: default_target_hours(),
            watched_repos: discover_default_watched_repos(),
            obsidian_export_dir: None,
            active_shift: None,
            completed_shifts: Vec::new(),
            today_date: today.clone(),
            today_activities: Vec::new(),
            today_commits: Vec::new(),
            today_challenges: String::new(),
            today_learnings: String::new(),
        });

    if state.watched_repos.is_empty() {
        state.watched_repos = discover_default_watched_repos();
    }
    if let Some(ref mut shift) = state.active_shift {
        if shift.network_stamp.as_deref().unwrap_or("").trim().is_empty() {
            shift.network_stamp = Some(cached_startup_network_stamp());
        }
    }
    reconcile_ojt_day(&mut state, &today);
    state
}

fn round_two_decimals(v: f64) -> f64 {
    if !v.is_finite() {
        return 0.0;
    }
    (v * 100.0).round() / 100.0
}

fn top_apps_sorted(app_map: &HashMap<String, u32>, limit: usize) -> Vec<(String, u32)> {
    let mut apps: Vec<(String, u32)> = app_map
        .iter()
        .filter(|(_, mins)| **mins > 0)
        .map(|(k, v)| (k.clone(), *v))
        .collect();
    apps.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    apps.truncate(limit);
    apps
}

fn build_ojt_dashboard(state: &OjtStateFile) -> OjtDashboardResponse {
    let clock = current_local_clock();
    let target_hours = if state.target_hours > 0.0 && state.target_hours.is_finite() {
        state.target_hours
    } else {
        default_target_hours()
    };
    let rendered_sum: f64 = state.completed_shifts.iter().map(|s| s.duration_hours).sum();
    let rendered_hours = round_two_decimals(rendered_sum);
    let remaining_hours = round_two_decimals((target_hours - rendered_hours).max(0.0));
    let progress_percent = ((rendered_hours / target_hours) * 100.0).clamp(0.0, 100.0) as f32;
    let shifts_remaining = if remaining_hours <= 0.0 {
        0
    } else {
        (remaining_hours / 8.0).ceil() as u32
    };
    let estimated_completion_date = estimate_completion_date(clock, shifts_remaining);

    let (current_focus_percent, top_active_apps, current_network_stamp) =
        if let Some(ref shift) = state.active_shift {
            let total = shift.active_minutes + shift.idle_minutes;
            let focus = if total > 0 {
                ((shift.active_minutes as f32 / total as f32) * 1000.0).round() / 10.0
            } else {
                0.0
            };
            let apps = top_apps_sorted(&shift.app_minutes, 4);
            let net = shift
                .network_stamp
                .clone()
                .filter(|s| !s.trim().is_empty())
                .unwrap_or_else(cached_startup_network_stamp);
            (focus, apps, net)
        } else {
            let latest_focus = state
                .completed_shifts
                .first()
                .filter(|s| s.date_str == state.today_date)
                .map(|s| s.focus_percent)
                .unwrap_or(0.0);
            let latest_apps = state
                .completed_shifts
                .first()
                .filter(|s| s.date_str == state.today_date)
                .map(|s| top_apps_sorted(&s.app_breakdown, 4))
                .unwrap_or_default();
            (latest_focus, latest_apps, cached_startup_network_stamp())
        };

    let mut merged_commits = state.today_commits.clone();
    if let Some(ref shift) = state.active_shift {
        merge_commit_evidence(&mut merged_commits, &shift.commit_evidence);
    }

    OjtDashboardResponse {
        target_hours,
        rendered_hours,
        remaining_hours,
        progress_percent,
        shifts_remaining,
        estimated_completion_date,
        watched_repos: state.watched_repos.clone(),
        obsidian_export_dir: state.obsidian_export_dir.clone(),
        active_shift: state.active_shift.clone(),
        completed_shifts: state.completed_shifts.clone(),
        today_activities: state.today_activities.clone(),
        today_commits: merged_commits,
        today_challenges: state.today_challenges.clone(),
        today_learnings: state.today_learnings.clone(),
        current_focus_percent,
        top_active_apps,
        current_network_stamp,
    }
}

fn push_unique_activity(list: &mut Vec<String>, item: String) {
    let cleaned = item.trim();
    if cleaned.is_empty() {
        return;
    }
    let needle = cleaned.to_lowercase();
    if !list.iter().any(|existing| existing.trim().to_lowercase() == needle) {
        list.push(cleaned.to_string());
    }
}

fn merge_commit_evidence(target: &mut Vec<GitCommitEvidence>, incoming: &[GitCommitEvidence]) {
    for ev in incoming {
        if let Some(existing) = target.iter_mut().find(|c| {
            c.repo.eq_ignore_ascii_case(&ev.repo) && c.hash.eq_ignore_ascii_case(&ev.hash)
        }) {
            if existing.files_changed == 0 && ev.files_changed > 0 {
                existing.files_changed = ev.files_changed;
                existing.insertions = ev.insertions;
                existing.deletions = ev.deletions;
            }
        } else {
            target.push(ev.clone());
        }
    }
}

fn parse_git_shortstat(line: &str) -> (u32, u32, u32) {
    let mut files = 0u32;
    let mut insertions = 0u32;
    let mut deletions = 0u32;
    for seg in line.split(',') {
        let s = seg.trim();
        let num = s
            .split_whitespace()
            .next()
            .and_then(|t| t.parse::<u32>().ok())
            .unwrap_or(0);
        if s.contains("file") {
            files = num;
        } else if s.contains("insertion") {
            insertions = num;
        } else if s.contains("deletion") {
            deletions = num;
        }
    }
    (files, insertions, deletions)
}

fn format_commit_bullet(ev: &GitCommitEvidence) -> String {
    if ev.files_changed > 0 {
        format!(
            "[{}] {} ({}) {} [{}f, +{} / -{}]",
            ev.repo, ev.hash, ev.timestamp, ev.message, ev.files_changed, ev.insertions, ev.deletions
        )
    } else {
        format!("[{}] {} ({}) {}", ev.repo, ev.hash, ev.timestamp, ev.message)
    }
}

fn format_local_hm_for_ts(ts: u64) -> String {
    let clock = current_local_clock();
    let delta = clock.unix_secs.saturating_sub(ts) as i64;
    let now_day_secs = (clock.hour as i64) * 3600 + (clock.minute as i64) * 60;
    let target_secs = (now_day_secs - delta).rem_euclid(86_400);
    let h = target_secs / 3600;
    let m = (target_secs % 3600) / 60;
    format!("{:02}:{:02}", h, m)
}

fn format_12h_for_ts(ts: u64) -> String {
    let clock = current_local_clock();
    let delta = clock.unix_secs.saturating_sub(ts) as i64;
    let now_day_secs = (clock.hour as i64) * 3600 + (clock.minute as i64) * 60;
    let target_secs = (now_day_secs - delta).rem_euclid(86_400);
    let h24 = (target_secs / 3600) as u32;
    let m = ((target_secs % 3600) / 60) as u32;
    let ampm = if h24 >= 12 { "PM" } else { "AM" };
    let h12 = match h24 % 12 {
        0 => 12,
        v => v,
    };
    format!("{:02}:{:02} {}", h12, m, ampm)
}

fn format_break_duration_label(secs: u64) -> String {
    let mins = (secs / 60).max(if secs > 0 { 1 } else { 0 });
    if mins >= 60 {
        format!("{}h {:02}m", mins / 60, mins % 60)
    } else {
        format!("{}m", mins)
    }
}

fn collect_modified_docs_in_dir(
    dir: &Path,
    since_ts: u64,
    depth: u8,
    out: &mut Vec<(u64, String, &'static str)>,
) {
    if depth > 3 || !dir.is_dir() {
        return;
    }
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        if out.len() >= 80 {
            break;
        }
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().to_string();
        if path.is_dir() {
            let lower = name.to_lowercase();
            if name.starts_with('.')
                || matches!(
                    lower.as_str(),
                    "node_modules" | "target" | ".git" | ".venv" | "dist" | "build" | "__pycache__"
                )
            {
                continue;
            }
            collect_modified_docs_in_dir(&path, since_ts, depth + 1, out);
            continue;
        }
        if name.starts_with("~$") || name.starts_with('.') {
            continue;
        }
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_lowercase())
            .unwrap_or_default();
        let kind: &'static str = match ext.as_str() {
            "xlsx" | "xls" => "Excel spreadsheet",
            "csv" => "CSV dataset",
            "docx" => "Word document",
            "pdf" => "PDF document",
            "pptx" => "PowerPoint deck",
            "blend" => "Blender project",
            _ => continue,
        };
        let mtime = file_mtime_secs(&path);
        if mtime >= since_ts {
            out.push((mtime, name, kind));
        }
    }
}

fn scan_local_document_saves(watched_repos: &[String], since_ts: u64) -> Vec<String> {
    let mut scan_roots: Vec<PathBuf> = Vec::new();
    let mut seen_roots: HashSet<String> = HashSet::new();

    if let Some(home) = get_user_profile_dir() {
        for sub in ["Documents", "Desktop"] {
            let p = home.join(sub);
            if p.is_dir() && seen_roots.insert(p.to_string_lossy().to_lowercase()) {
                scan_roots.push(p);
            }
        }
    }
    for repo in watched_repos {
        let trimmed = repo.trim();
        if !trimmed.is_empty() {
            let p = PathBuf::from(trimmed);
            if p.is_dir() && seen_roots.insert(p.to_string_lossy().to_lowercase()) {
                scan_roots.push(p);
            }
        }
    }

    let mut raw_docs: Vec<(u64, String, &'static str)> = Vec::new();
    for root in scan_roots {
        collect_modified_docs_in_dir(&root, since_ts, 0, &mut raw_docs);
    }
    raw_docs.sort_by(|a, b| b.0.cmp(&a.0));

    let mut seen_names: HashSet<String> = HashSet::new();
    let mut bullets: Vec<String> = Vec::new();
    for (mtime, name, kind) in raw_docs {
        if !seen_names.insert(name.to_lowercase()) {
            continue;
        }
        let hm = format_local_hm_for_ts(mtime);
        bullets.push(format!("[{}] [Document Updated] {} ({})", hm, name, kind));
        if bullets.len() >= 20 {
            break;
        }
    }
    bullets
}

fn scan_git_and_agent_activities(
    watched_repos: &[String],
    since_ts: u64,
) -> (Vec<String>, Vec<GitCommitEvidence>) {
    let mut found: Vec<String> = Vec::new();
    let mut commits: Vec<GitCommitEvidence> = Vec::new();
    let now = now_secs();

    for doc_bullet in scan_local_document_saves(watched_repos, since_ts) {
        push_unique_activity(&mut found, doc_bullet);
    }

    for repo_raw in watched_repos {
        let repo = repo_raw.trim();
        if repo.is_empty() {
            continue;
        }
        let repo_path = Path::new(repo);
        if !repo_path.exists() {
            continue;
        }
        let repo_name = repo_path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("repo");

        let mut cmd = Command::new("git");
        cmd.args([
            "-C",
            repo,
            "log",
            "--since=midnight",
            "--pretty=format:COMMIT|%h|%ad|%s",
            "--date=format:%H:%M",
            "--shortstat",
            "--no-merges",
        ]);
        #[cfg(target_os = "windows")]
        {
            cmd.creation_flags(0x08000000);
        }
        if let Ok(output) = cmd.output() {
            if output.status.success() {
                let text = String::from_utf8_lossy(&output.stdout);
                let mut current: Option<GitCommitEvidence> = None;

                let mut finalize = |ev_opt: Option<GitCommitEvidence>| {
                    if let Some(ev) = ev_opt {
                        push_unique_activity(&mut found, format_commit_bullet(&ev));
                        merge_commit_evidence(&mut commits, &[ev]);
                    }
                };

                for raw_line in text.lines() {
                    let trimmed = raw_line.trim();
                    if trimmed.is_empty() {
                        continue;
                    }
                    if let Some(rest) = trimmed.strip_prefix("COMMIT|") {
                        finalize(current.take());
                        let mut parts = rest.splitn(3, '|');
                        let hash = parts.next().unwrap_or("").trim();
                        let timestamp = parts.next().unwrap_or("").trim();
                        let message = parts.next().unwrap_or("").trim();
                        if !hash.is_empty() {
                            current = Some(GitCommitEvidence {
                                hash: hash.to_string(),
                                timestamp: if timestamp.is_empty() {
                                    "--:--".to_string()
                                } else {
                                    timestamp.to_string()
                                },
                                repo: repo_name.to_string(),
                                message: truncate_str(message, 100),
                                files_changed: 0,
                                insertions: 0,
                                deletions: 0,
                            });
                        }
                    } else if trimmed.contains("file changed")
                        || trimmed.contains("files changed")
                        || trimmed.contains("insertion")
                        || trimmed.contains("deletion")
                    {
                        if let Some(ref mut ev) = current {
                            let (f, ins, del) = parse_git_shortstat(trimmed);
                            ev.files_changed = f;
                            ev.insertions = ins;
                            ev.deletions = del;
                        }
                    }
                }
                finalize(current.take());
            }
        }
    }

    if let Some(home) = get_user_profile_dir() {
        let brain_dir = home.join(".gemini").join("antigravity").join("brain");
        if let Ok(entries) = fs::read_dir(&brain_dir) {
            let mut task_paths: Vec<(u64, PathBuf)> = Vec::new();
            for entry in entries.flatten() {
                let p = entry.path();
                if !p.is_dir() {
                    continue;
                }
                let task_md = p.join("task.md");
                if task_md.exists() {
                    let mtime = file_mtime_secs(&task_md);
                    if now.saturating_sub(mtime) <= 18 * 3600 {
                        task_paths.push((mtime, task_md));
                    }
                }
            }
            task_paths.sort_by(|a, b| b.0.cmp(&a.0));

            let mut agent_count = 0usize;
            for (_mtime, task_md) in task_paths {
                if agent_count >= 15 {
                    break;
                }
                if let Ok(content) = fs::read_to_string(&task_md) {
                    for raw_line in content.lines() {
                        if agent_count >= 15 {
                            break;
                        }
                        let line = raw_line.trim();
                        if let Some(rest) = line
                            .strip_prefix("- [x]")
                            .or_else(|| line.strip_prefix("- [X]"))
                            .or_else(|| line.strip_prefix("* [x]"))
                            .or_else(|| line.strip_prefix("* [X]"))
                        {
                            let cleaned = rest
                                .trim()
                                .trim_matches('*')
                                .trim_matches('`')
                                .trim();
                            if !cleaned.is_empty() {
                                let before = found.len();
                                push_unique_activity(
                                    &mut found,
                                    format!("[Agent Task] {}", truncate_str(cleaned, 100)),
                                );
                                if found.len() > before {
                                    agent_count += 1;
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    (found, commits)
}

fn spawn_ojt_pow_watcher(ojt_state: SharedOjtState) {
    std::thread::spawn(move || loop {
        std::thread::sleep(Duration::from_secs(60));
        let (should_sample, needs_net) = {
            let state = lock_or_recover(&ojt_state);
            match state.active_shift {
                Some(ref s) if !s.is_paused => (
                    true,
                    s.network_stamp.as_deref().unwrap_or("").trim().is_empty(),
                ),
                _ => (false, false),
            }
        };
        if !should_sample {
            continue;
        }
        let (is_active, app_opt) = sample_user_activity();
        let net_opt = if needs_net {
            Some(capture_network_stamp())
        } else {
            None
        };

        let mut state = lock_or_recover(&ojt_state);
        if let Some(ref mut shift) = state.active_shift {
            if shift.is_paused {
                continue;
            }
            if is_active {
                shift.active_minutes += 1;
                if let Some((broad_app, detailed_label)) = app_opt {
                    *shift.app_minutes.entry(broad_app).or_insert(0) += 1;
                    *shift.app_durations.entry(detailed_label).or_insert(0) += 1;
                }
            } else {
                shift.idle_minutes += 1;
            }
            if let Some(net) = net_opt {
                if shift.network_stamp.as_deref().unwrap_or("").trim().is_empty() {
                    shift.network_stamp = Some(net);
                }
            }
            save_ojt_state(&state);
        }
    });
}

fn format_shift_summary_markdown(
    date_str: &str,
    time_in: &str,
    time_out: &str,
    duration_hours: f64,
    active_minutes: u32,
    idle_minutes: u32,
    focus_percent: f32,
    network_stamp: &str,
    verification_hash: &str,
    activities: &[String],
) -> String {
    let mut md = format!(
        "### OJT Shift Summary — {}\n- **Shift:** {} – {} | **Rendered:** {:.2} hrs\n- **Verified Focus:** {:.1}% ({}m active / {}m idle) | **Network:** {}\n- **Verification ID:** `{}`\n\n**Accomplishments:**\n",
        date_str,
        time_in,
        time_out,
        duration_hours,
        focus_percent,
        active_minutes,
        idle_minutes,
        network_stamp,
        verification_hash
    );
    if activities.is_empty() {
        md.push_str("- General OJT engineering & development work\n");
    } else {
        for item in activities {
            md.push_str(&format!("- {}\n", item));
        }
    }
    md
}

#[tauri::command]
fn get_ojt_state(ojt: State<'_, SharedOjtState>) -> OjtDashboardResponse {
    let today = format_date(current_local_clock());
    let mut state = lock_or_recover(&ojt);
    reconcile_ojt_day(&mut state, &today);
    build_ojt_dashboard(&state)
}

#[tauri::command]
fn scan_today_activity(ojt: State<'_, SharedOjtState>) -> OjtDashboardResponse {
    let clock = current_local_clock();
    let today = format_date(clock);
    let (repos, since_ts) = {
        let mut state = lock_or_recover(&ojt);
        reconcile_ojt_day(&mut state, &today);
        let fallback_midnight = clock
            .unix_secs
            .saturating_sub((clock.hour as u64) * 3600 + (clock.minute as u64) * 60);
        let since = state
            .active_shift
            .as_ref()
            .map(|s| s.start_ts)
            .unwrap_or(fallback_midnight);
        (state.watched_repos.clone(), since)
    };

    let (discovered, discovered_commits) = scan_git_and_agent_activities(&repos, since_ts);

    let mut state = lock_or_recover(&ojt);
    reconcile_ojt_day(&mut state, &today);
    for item in &discovered {
        push_unique_activity(&mut state.today_activities, item.clone());
    }
    merge_commit_evidence(&mut state.today_commits, &discovered_commits);

    let copy = state.today_activities.clone();
    let commits_copy = state.today_commits.clone();
    if let Some(ref mut shift) = state.active_shift {
        for item in copy {
            push_unique_activity(&mut shift.notes, item);
        }
        merge_commit_evidence(&mut shift.commit_evidence, &commits_copy);
    }
    save_ojt_state(&state);
    build_ojt_dashboard(&state)
}

#[tauri::command]
fn add_manual_activity(bullet: String, ojt: State<'_, SharedOjtState>) -> OjtDashboardResponse {
    let today = format_date(current_local_clock());
    let trimmed = bullet.trim().to_string();
    let mut state = lock_or_recover(&ojt);
    reconcile_ojt_day(&mut state, &today);
    if !trimmed.is_empty() {
        push_unique_activity(&mut state.today_activities, trimmed.clone());
        if let Some(ref mut shift) = state.active_shift {
            push_unique_activity(&mut shift.notes, trimmed);
        }
        save_ojt_state(&state);
    }
    build_ojt_dashboard(&state)
}

#[tauri::command]
fn update_today_reflections(
    challenges: String,
    learnings: String,
    ojt: State<'_, SharedOjtState>,
) -> OjtDashboardResponse {
    let today = format_date(current_local_clock());
    let clean_challenges = challenges.trim().to_string();
    let clean_learnings = learnings.trim().to_string();

    let mut state = lock_or_recover(&ojt);
    reconcile_ojt_day(&mut state, &today);
    state.today_challenges = clean_challenges.clone();
    state.today_learnings = clean_learnings.clone();
    if let Some(ref mut shift) = state.active_shift {
        shift.challenges = clean_challenges;
        shift.learnings = clean_learnings;
    }
    save_ojt_state(&state);
    build_ojt_dashboard(&state)
}

#[tauri::command]
fn toggle_shift_break(ojt: State<'_, SharedOjtState>) -> OjtDashboardResponse {
    let clock = current_local_clock();
    let today = format_date(clock);
    let mut state = lock_or_recover(&ojt);
    reconcile_ojt_day(&mut state, &today);

    if let Some(ref mut shift) = state.active_shift {
        if shift.is_paused {
            let start_break = shift.pause_started_ts.unwrap_or(clock.unix_secs);
            let elapsed_break = clock.unix_secs.saturating_sub(start_break);
            shift.total_paused_seconds = shift.total_paused_seconds.saturating_add(elapsed_break);
            let log_entry = format!(
                "{} - {} ({} break)",
                format_12h_for_ts(start_break),
                format_12h_for_ts(clock.unix_secs),
                format_break_duration_label(elapsed_break)
            );
            shift.break_logs.push(log_entry);
            shift.is_paused = false;
            shift.pause_started_ts = None;
        } else {
            shift.is_paused = true;
            shift.pause_started_ts = Some(clock.unix_secs);
        }
        save_ojt_state(&state);
    }

    build_ojt_dashboard(&state)
}

#[tauri::command]
fn toggle_shift(ojt: State<'_, SharedOjtState>) -> OjtDashboardResponse {
    let clock = current_local_clock();
    let today = format_date(clock);
    let hm = format_hm(clock);

    let (active_opt, repos) = {
        let mut state = lock_or_recover(&ojt);
        reconcile_ojt_day(&mut state, &today);
        (state.active_shift.clone(), state.watched_repos.clone())
    };

    if let Some(mut shift) = active_opt {
        let (discovered, discovered_commits) =
            scan_git_and_agent_activities(&repos, shift.start_ts);
        let net_out = capture_network_stamp();

        let mut state = lock_or_recover(&ojt);
        reconcile_ojt_day(&mut state, &today);

        if let Some(ref current_active) = state.active_shift {
            shift = current_active.clone();
        }
        if shift.is_paused {
            let start_break = shift.pause_started_ts.unwrap_or(clock.unix_secs);
            let elapsed_break = clock.unix_secs.saturating_sub(start_break);
            shift.total_paused_seconds = shift.total_paused_seconds.saturating_add(elapsed_break);
            let log_entry = format!(
                "{} - {} ({} break)",
                format_12h_for_ts(start_break),
                format_12h_for_ts(clock.unix_secs),
                format_break_duration_label(elapsed_break)
            );
            shift.break_logs.push(log_entry);
            shift.is_paused = false;
            shift.pause_started_ts = None;
        }

        for item in discovered {
            push_unique_activity(&mut state.today_activities, item);
        }
        merge_commit_evidence(&mut state.today_commits, &discovered_commits);

        let mut activities: Vec<String> = Vec::new();
        for item in &state.today_activities {
            push_unique_activity(&mut activities, item.clone());
        }
        for note in &shift.notes {
            push_unique_activity(&mut activities, note.clone());
        }
        if activities.is_empty() {
            activities.push("General OJT engineering & development work".to_string());
        }

        let mut commit_evidence = shift.commit_evidence.clone();
        merge_commit_evidence(&mut commit_evidence, &state.today_commits);

        let challenges = if !state.today_challenges.trim().is_empty() {
            state.today_challenges.trim().to_string()
        } else {
            shift.challenges.trim().to_string()
        };
        let learnings = if !state.today_learnings.trim().is_empty() {
            state.today_learnings.trim().to_string()
        } else {
            shift.learnings.trim().to_string()
        };

        let net_elapsed = clock
            .unix_secs
            .saturating_sub(shift.start_ts)
            .saturating_sub(shift.total_paused_seconds) as f64;
        let duration_hours = round_two_decimals(net_elapsed / 3600.0).max(0.01);

        let total_tracked = shift.active_minutes + shift.idle_minutes;
        let focus_percent = if total_tracked > 0 {
            ((shift.active_minutes as f32 / total_tracked as f32) * 1000.0).round() / 10.0
        } else {
            0.0
        };
        let commit_hashes = commit_evidence
            .iter()
            .map(|c| c.hash.as_str())
            .collect::<Vec<_>>()
            .join(",");
        let verification_hash = compute_verification_hash(
            &shift.date_str,
            &shift.time_in_str,
            &hm,
            duration_hours,
            &commit_hashes,
            shift.active_minutes,
        );
        let net_in = shift
            .network_stamp
            .clone()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| net_out.clone());

        let summary_markdown = format_shift_summary_markdown(
            &shift.date_str,
            &shift.time_in_str,
            &hm,
            duration_hours,
            shift.active_minutes,
            shift.idle_minutes,
            focus_percent,
            &net_in,
            &verification_hash,
            &activities,
        );

        let completed = CompletedShift {
            id: format!("shift-{}", clock.unix_secs),
            date_str: shift.date_str,
            time_in: shift.time_in_str,
            time_out: hm,
            duration_hours,
            activities,
            summary_markdown,
            active_minutes: shift.active_minutes,
            idle_minutes: shift.idle_minutes,
            focus_percent,
            network_stamp_in: Some(net_in),
            network_stamp_out: Some(net_out),
            app_breakdown: shift.app_minutes,
            app_durations: shift.app_durations,
            commit_evidence,
            challenges,
            learnings,
            verification_hash,
            total_paused_seconds: shift.total_paused_seconds,
            break_logs: shift.break_logs,
        };
        state.completed_shifts.insert(0, completed);
        state.active_shift = None;
        save_ojt_state(&state);
        build_ojt_dashboard(&state)
    } else {
        let net_in = capture_network_stamp();
        let mut state = lock_or_recover(&ojt);
        reconcile_ojt_day(&mut state, &today);
        state.active_shift = Some(ShiftSession {
            start_ts: clock.unix_secs,
            date_str: today,
            time_in_str: hm,
            notes: state.today_activities.clone(),
            active_minutes: 0,
            idle_minutes: 0,
            network_stamp: Some(net_in),
            app_minutes: HashMap::new(),
            app_durations: HashMap::new(),
            commit_evidence: state.today_commits.clone(),
            challenges: state.today_challenges.clone(),
            learnings: state.today_learnings.clone(),
            is_paused: false,
            pause_started_ts: None,
            total_paused_seconds: 0,
            break_logs: Vec::new(),
        });
        save_ojt_state(&state);
        build_ojt_dashboard(&state)
    }
}

#[tauri::command]
fn update_ojt_settings(
    target_hours: f64,
    watched_repos: Vec<String>,
    obsidian_export_dir: Option<String>,
    ojt: State<'_, SharedOjtState>,
) -> OjtDashboardResponse {
    let today = format_date(current_local_clock());
    let mut state = lock_or_recover(&ojt);
    reconcile_ojt_day(&mut state, &today);

    if target_hours > 0.0 && target_hours.is_finite() {
        state.target_hours = target_hours;
    }

    let mut clean_repos: Vec<String> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    for raw in watched_repos {
        let trimmed = raw.trim();
        if !trimmed.is_empty() && seen.insert(trimmed.to_lowercase()) {
            clean_repos.push(trimmed.to_string());
        }
    }
    state.watched_repos = clean_repos;

    state.obsidian_export_dir = obsidian_export_dir
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());

    save_ojt_state(&state);
    build_ojt_dashboard(&state)
}

fn format_tools_summary(app_map: &HashMap<String, u32>) -> String {
    let top = top_apps_sorted(app_map, 4);
    if top.is_empty() {
        "_None sampled yet_".to_string()
    } else {
        top.into_iter()
            .map(|(name, mins)| format!("{} ({}m)", name, mins))
            .collect::<Vec<_>>()
            .join(", ")
    }
}

fn format_app_durations_summary(app_map: &HashMap<String, u32>) -> String {
    let top = top_apps_sorted(app_map, 8);
    if top.is_empty() {
        "_No application focus data captured yet._".to_string()
    } else {
        top.into_iter()
            .map(|(name, mins)| {
                if mins >= 60 {
                    format!("{}: {:.1} hrs", name, (mins as f64) / 60.0)
                } else {
                    format!("{}: {}m", name, mins)
                }
            })
            .collect::<Vec<_>>()
            .join(" | ")
    }
}

fn build_daily_markdown(today: &str, dash: &OjtDashboardResponse) -> String {
    let mut today_shifts: Vec<&CompletedShift> = dash
        .completed_shifts
        .iter()
        .filter(|s| s.date_str == today)
        .collect();
    today_shifts.sort_by(|a, b| a.time_in.cmp(&b.time_in));

    let mut active_m: u32 = today_shifts.iter().map(|s| s.active_minutes).sum();
    let mut idle_m: u32 = today_shifts.iter().map(|s| s.idle_minutes).sum();
    let mut app_totals: HashMap<String, u32> = HashMap::new();
    let mut doc_app_totals: HashMap<String, u32> = HashMap::new();
    let mut total_break_secs: u64 = today_shifts.iter().map(|s| s.total_paused_seconds).sum();
    let mut all_break_logs: Vec<String> = Vec::new();

    for s in &today_shifts {
        for (app, mins) in &s.app_breakdown {
            *app_totals.entry(app.clone()).or_insert(0) += *mins;
        }
        let dur_src = if s.app_durations.is_empty() {
            &s.app_breakdown
        } else {
            &s.app_durations
        };
        for (app, mins) in dur_src {
            *doc_app_totals.entry(app.clone()).or_insert(0) += *mins;
        }
        all_break_logs.extend(s.break_logs.iter().cloned());
    }

    let mut rendered_today: f64 = today_shifts.iter().map(|s| s.duration_hours).sum();
    if let Some(ref active) = dash.active_shift {
        if active.date_str == today {
            active_m += active.active_minutes;
            idle_m += active.idle_minutes;
            for (app, mins) in &active.app_minutes {
                *app_totals.entry(app.clone()).or_insert(0) += *mins;
            }
            let dur_src = if active.app_durations.is_empty() {
                &active.app_minutes
            } else {
                &active.app_durations
            };
            for (app, mins) in dur_src {
                *doc_app_totals.entry(app.clone()).or_insert(0) += *mins;
            }
            let now = now_secs();
            let ongoing_pause = if active.is_paused {
                now.saturating_sub(active.pause_started_ts.unwrap_or(now))
            } else {
                0
            };
            let active_pause_secs = active.total_paused_seconds.saturating_add(ongoing_pause);
            total_break_secs = total_break_secs.saturating_add(active_pause_secs);
            all_break_logs.extend(active.break_logs.iter().cloned());

            let net_secs = now
                .saturating_sub(active.start_ts)
                .saturating_sub(active_pause_secs);
            let elapsed_hrs = round_two_decimals(net_secs as f64 / 3600.0);
            rendered_today = round_two_decimals(rendered_today + elapsed_hrs);
        }
    }

    let break_suffix = if total_break_secs >= 60 {
        format!(" ({} break) → {:.2} hrs net", format_break_duration_label(total_break_secs), rendered_today)
    } else {
        String::new()
    };

    let shift_window = match (today_shifts.first(), today_shifts.last(), &dash.active_shift) {
        (Some(first), Some(_), Some(active)) if active.date_str == today => {
            format!("{} – Active ({}+){}", first.time_in, active.time_in_str, break_suffix)
        }
        (Some(first), Some(last), _) => format!("{} – {}{}", first.time_in, last.time_out, break_suffix),
        (None, None, Some(active)) => format!("{} – In Progress{}", active.time_in_str, break_suffix),
        _ => "No shift clocked today".to_string(),
    };

    let total_tracked = active_m + idle_m;
    let focus_pct = if total_tracked > 0 {
        ((active_m as f32 / total_tracked as f32) * 1000.0).round() / 10.0
    } else {
        0.0
    };
    let tools_summary = format_tools_summary(&app_totals);

    let net_stamp = today_shifts
        .first()
        .and_then(|s| s.network_stamp_in.as_ref())
        .cloned()
        .unwrap_or_else(|| dash.current_network_stamp.clone());

    let mut commits = dash.today_commits.clone();
    for s in &today_shifts {
        merge_commit_evidence(&mut commits, &s.commit_evidence);
    }

    let verification_id = if !today_shifts.is_empty() {
        today_shifts
            .iter()
            .map(|s| {
                if s.verification_hash.is_empty() {
                    compute_verification_hash(
                        &s.date_str,
                        &s.time_in,
                        &s.time_out,
                        s.duration_hours,
                        "",
                        s.active_minutes,
                    )
                } else {
                    s.verification_hash.clone()
                }
            })
            .collect::<Vec<_>>()
            .join(", ")
    } else if let Some(ref active) = dash.active_shift {
        let hashes = commits
            .iter()
            .map(|c| c.hash.as_str())
            .collect::<Vec<_>>()
            .join(",");
        format!(
            "{} (Live Preview)",
            compute_verification_hash(
                &active.date_str,
                &active.time_in_str,
                "LIVE",
                rendered_today,
                &hashes,
                active.active_minutes,
            )
        )
    } else {
        "Pending Shift Completion".to_string()
    };

    let mut activities = dash.today_activities.clone();
    for s in &today_shifts {
        for a in &s.activities {
            push_unique_activity(&mut activities, a.clone());
        }
    }

    let challenges = if !dash.today_challenges.trim().is_empty() {
        dash.today_challenges.trim().to_string()
    } else {
        today_shifts
            .iter()
            .map(|s| s.challenges.trim())
            .find(|c| !c.is_empty())
            .unwrap_or("")
            .to_string()
    };
    let learnings = if !dash.today_learnings.trim().is_empty() {
        dash.today_learnings.trim().to_string()
    } else {
        today_shifts
            .iter()
            .map(|s| s.learnings.trim())
            .find(|l| !l.is_empty())
            .unwrap_or("")
            .to_string()
    };

    let dow = day_of_week_name(today);
    let date_header = if dow.is_empty() {
        today.to_string()
    } else {
        format!("{} ({})", today, dow)
    };

    let mut md = String::new();
    md.push_str(&format!("# OJT Daily Accomplishment Record — {}\n\n", today));
    md.push_str(&format!("- **Date:** {}\n", date_header));
    md.push_str(&format!("- **Shift Window:** {}\n", shift_window));
    md.push_str(&format!(
        "- **Net Rendered Hours:** {:.2} hrs (Cumulative: {:.2} / {:.1} hrs · {:.1}% · Est. Completion: {})\n",
        rendered_today,
        dash.rendered_hours,
        dash.target_hours,
        dash.progress_percent,
        dash.estimated_completion_date
    ));
    md.push_str(&format!(
        "- **Active Focus Ratio:** {:.1}% Active ({}m active / {}m idle) | **Top Tools:** {}\n",
        focus_pct, active_m, idle_m, tools_summary
    ));
    md.push_str(&format!("- **Network / Workstation Stamp:** {}\n", net_stamp));
    md.push_str(&format!("- **Shift Verification ID:** `{}`\n", verification_id));
    if !all_break_logs.is_empty() {
        md.push_str(&format!("- **Break / Lunch Deductions:** {}\n", all_break_logs.join("; ")));
    }
    md.push('\n');

    md.push_str("## Work & Application Breakdown\n\n");
    md.push_str(&format!("{}\n\n", format_app_durations_summary(&doc_app_totals)));

    md.push_str("## Verified Git Commit & File Evidence\n\n");
    if commits.is_empty() {
        md.push_str("_No git commits captured for this date._\n\n");
    } else {
        md.push_str("| Time | Repo | Hash | File Changes | Commit Message |\n");
        md.push_str("| --- | --- | --- | --- | --- |\n");
        for c in &commits {
            let stats = if c.files_changed > 0 {
                format!("{} file(s), +{} / -{}", c.files_changed, c.insertions, c.deletions)
            } else {
                "-".to_string()
            };
            let msg = c.message.replace('|', "/").replace(['\r', '\n'], " ");
            md.push_str(&format!(
                "| {} | {} | `{}` | {} | {} |\n",
                c.timestamp, c.repo, c.hash, stats, msg
            ));
        }
        md.push('\n');
    }

    md.push_str("## Accomplishments\n\n");
    if activities.is_empty() {
        md.push_str("_No accomplishments logged for today yet._\n\n");
    } else {
        for item in &activities {
            md.push_str(&format!("- {}\n", item));
        }
        md.push('\n');
    }

    md.push_str("## Problems Encountered\n\n");
    if challenges.is_empty() {
        md.push_str("_None logged for today._\n\n");
    } else {
        md.push_str(&format!("{}\n\n", challenges));
    }

    md.push_str("## Solutions & Key Learnings\n\n");
    if learnings.is_empty() {
        md.push_str("_None logged for today._\n");
    } else {
        md.push_str(&format!("{}\n", learnings));
    }

    md
}

fn build_weekly_journal_markdown(
    today: &str,
    scope: &str,
    dash: &OjtDashboardResponse,
) -> String {
    let (iso_year, iso_week) = iso_week_number(today);
    let mut shifts: Vec<&CompletedShift> = if scope == "weekly" {
        dash.completed_shifts.iter().take(7).collect()
    } else {
        dash.completed_shifts.iter().collect()
    };
    shifts.sort_by(|a, b| a.date_str.cmp(&b.date_str).then_with(|| a.time_in.cmp(&b.time_in)));

    let title = if scope == "weekly" {
        format!("# OJT Weekly Accomplishment Journal — {}-W{:02}", iso_year, iso_week)
    } else {
        "# Complete OJT DTR & Accomplishment Journal".to_string()
    };

    let period_hours: f64 = round_two_decimals(shifts.iter().map(|s| s.duration_hours).sum());
    let mut md = String::new();
    md.push_str(&format!("{}\n\n", title));
    md.push_str(&format!(
        "**Generated:** {} | **Period Hours:** {:.2} hrs | **Cumulative Progress:** {:.2} / {:.1} hrs ({:.1}%) | **Remaining:** {:.2} hrs ({} shifts est. → {})\n\n",
        today,
        period_hours,
        dash.rendered_hours,
        dash.target_hours,
        dash.progress_percent,
        dash.remaining_hours,
        dash.shifts_remaining,
        dash.estimated_completion_date
    ));

    md.push_str("## 1. Weekly DTR Attendance Table\n\n");
    md.push_str("| Date | Time In | Time Out | Breaks | Active Ratio | Net Hours | Verification ID |\n");
    md.push_str("| --- | --- | --- | --- | ---: | ---: | --- |\n");
    if shifts.is_empty() {
        md.push_str("| - | - | - | - | - | 0.00 | No completed shifts in selected period |\n");
    } else {
        for s in &shifts {
            let break_cell = if s.total_paused_seconds >= 60 {
                format_break_duration_label(s.total_paused_seconds)
            } else {
                "-".to_string()
            };
            let ratio_str = if s.active_minutes + s.idle_minutes > 0 {
                format!("{:.1}% ({}m/{}m)", s.focus_percent, s.active_minutes, s.active_minutes + s.idle_minutes)
            } else {
                "-".to_string()
            };
            let ver_id = if s.verification_hash.is_empty() {
                compute_verification_hash(
                    &s.date_str,
                    &s.time_in,
                    &s.time_out,
                    s.duration_hours,
                    "",
                    s.active_minutes,
                )
            } else {
                s.verification_hash.clone()
            };
            md.push_str(&format!(
                "| {} | {} | {} | {} | {} | {:.2} | `{}` |\n",
                s.date_str, s.time_in, s.time_out, break_cell, ratio_str, s.duration_hours, ver_id
            ));
        }
    }
    md.push_str(&format!(
        "\n- **Total Period Hours:** {:.2} hrs\n- **Cumulative OJT Hours:** {:.2} / {:.1} hrs ({:.1}%)\n\n",
        period_hours, dash.rendered_hours, dash.target_hours, dash.progress_percent
    ));

    #[derive(Default)]
    struct DayBucket {
        windows: Vec<String>,
        net_stamps: Vec<String>,
        ver_ids: Vec<String>,
        active_m: u32,
        idle_m: u32,
        commits: Vec<GitCommitEvidence>,
        activities: Vec<String>,
        challenges: Vec<String>,
        learnings: Vec<String>,
    }

    let mut days: std::collections::BTreeMap<String, DayBucket> = std::collections::BTreeMap::new();
    let mut period_app_totals: HashMap<String, u32> = HashMap::new();
    let mut period_doc_app_totals: HashMap<String, u32> = HashMap::new();
    let mut unique_repos: HashSet<String> = HashSet::new();
    let mut total_commits_logged = 0usize;

    for s in &shifts {
        let entry = days.entry(s.date_str.clone()).or_default();
        let brk_note = if s.total_paused_seconds >= 60 {
            format!(" ({} break)", format_break_duration_label(s.total_paused_seconds))
        } else {
            String::new()
        };
        entry
            .windows
            .push(format!("{}–{}{} ({:.2}h net)", s.time_in, s.time_out, brk_note, s.duration_hours));
        let dur_src = if s.app_durations.is_empty() {
            &s.app_breakdown
        } else {
            &s.app_durations
        };
        for (app, mins) in dur_src {
            *period_doc_app_totals.entry(app.clone()).or_insert(0) += *mins;
        }
        if let Some(ref net) = s.network_stamp_in {
            push_unique_activity(&mut entry.net_stamps, net.clone());
        }
        if !s.verification_hash.trim().is_empty() {
            push_unique_activity(&mut entry.ver_ids, s.verification_hash.clone());
        }
        entry.active_m += s.active_minutes;
        entry.idle_m += s.idle_minutes;
        for (app, mins) in &s.app_breakdown {
            *period_app_totals.entry(app.clone()).or_insert(0) += *mins;
        }
        merge_commit_evidence(&mut entry.commits, &s.commit_evidence);
        for act in &s.activities {
            push_unique_activity(&mut entry.activities, act.clone());
        }
        if !s.challenges.trim().is_empty() {
            push_unique_activity(&mut entry.challenges, s.challenges.trim().to_string());
        }
        if !s.learnings.trim().is_empty() {
            push_unique_activity(&mut entry.learnings, s.learnings.trim().to_string());
        }
    }

    if dash.active_shift.is_some()
        || !dash.today_activities.is_empty()
        || !dash.today_commits.is_empty()
        || !dash.today_challenges.trim().is_empty()
        || !dash.today_learnings.trim().is_empty()
    {
        let entry = days.entry(today.to_string()).or_default();
        if let Some(ref active) = dash.active_shift {
            entry
                .windows
                .push(format!("{}–Active", active.time_in_str));
            if let Some(ref net) = active.network_stamp {
                push_unique_activity(&mut entry.net_stamps, net.clone());
            }
            entry.active_m += active.active_minutes;
            entry.idle_m += active.idle_minutes;
            for (app, mins) in &active.app_minutes {
                *period_app_totals.entry(app.clone()).or_insert(0) += *mins;
            }
            let dur_src = if active.app_durations.is_empty() {
                &active.app_minutes
            } else {
                &active.app_durations
            };
            for (app, mins) in dur_src {
                *period_doc_app_totals.entry(app.clone()).or_insert(0) += *mins;
            }
        }
        merge_commit_evidence(&mut entry.commits, &dash.today_commits);
        for act in &dash.today_activities {
            push_unique_activity(&mut entry.activities, act.clone());
        }
        if !dash.today_challenges.trim().is_empty() {
            push_unique_activity(&mut entry.challenges, dash.today_challenges.trim().to_string());
        }
        if !dash.today_learnings.trim().is_empty() {
            push_unique_activity(&mut entry.learnings, dash.today_learnings.trim().to_string());
        }
    }

    md.push_str("## 2. Day-by-Day Technical Accomplishment Breakdown\n\n");
    if days.is_empty() {
        md.push_str("_No daily activity recorded in selected period yet._\n\n");
    } else {
        for (date, bucket) in &days {
            let dow = day_of_week_name(date);
            let label = if dow.is_empty() {
                date.clone()
            } else {
                format!("{} ({})", date, dow)
            };
            md.push_str(&format!("### {}\n", label));
            let win_str = if bucket.windows.is_empty() {
                "Activity Log Only".to_string()
            } else {
                bucket.windows.join(", ")
            };
            let net_str = if bucket.net_stamps.is_empty() {
                dash.current_network_stamp.clone()
            } else {
                bucket.net_stamps.join(", ")
            };
            let day_total = bucket.active_m + bucket.idle_m;
            let day_focus = if day_total > 0 {
                format!(
                    "{:.1}% active ({}m active / {}m idle)",
                    (bucket.active_m as f32 / day_total as f32) * 100.0,
                    bucket.active_m,
                    bucket.idle_m
                )
            } else {
                "No heartbeat data".to_string()
            };
            md.push_str(&format!(
                "- **Shift & Telemetry:** {} | **Focus:** {} | **Network:** {}\n",
                win_str, day_focus, net_str
            ));
            if !bucket.ver_ids.is_empty() {
                md.push_str(&format!(
                    "- **Verification ID(s):** `{}`\n",
                    bucket.ver_ids.join("`, `")
                ));
            }
            if !bucket.commits.is_empty() {
                total_commits_logged += bucket.commits.len();
                md.push_str("- **Git Commit Evidence:**\n");
                for c in &bucket.commits {
                    unique_repos.insert(c.repo.clone());
                    let stat = if c.files_changed > 0 {
                        format!(" · {}f +{} / -{}", c.files_changed, c.insertions, c.deletions)
                    } else {
                        String::new()
                    };
                    md.push_str(&format!(
                        "  - `{}` ({} @ {}{}): {}\n",
                        c.hash, c.repo, c.timestamp, stat, c.message
                    ));
                }
            }
            if !bucket.activities.is_empty() {
                md.push_str("- **Tasks & Engineering Work:**\n");
                for act in &bucket.activities {
                    md.push_str(&format!("  - {}\n", act));
                }
            }
            md.push('\n');
        }
    }

    md.push_str("## 3. Consolidated Problems Encountered & Technical Solutions Applied\n\n");
    let mut any_reflections = false;
    for (date, bucket) in &days {
        if !bucket.challenges.is_empty() || !bucket.learnings.is_empty() {
            any_reflections = true;
            md.push_str(&format!("### {}\n", date));
            if !bucket.challenges.is_empty() {
                md.push_str(&format!(
                    "- **Problems Encountered:** {}\n",
                    bucket.challenges.join("; ")
                ));
            }
            if !bucket.learnings.is_empty() {
                md.push_str(&format!(
                    "- **Solutions & Key Learnings:** {}\n",
                    bucket.learnings.join("; ")
                ));
            }
            md.push('\n');
        }
    }
    if !any_reflections {
        md.push_str(
            "_No daily challenges or solutions logged yet. Use the OJT Pulse reflections box during shifts to auto-populate this section._\n\n",
        );
    }

    let total_active: u32 = days.values().map(|b| b.active_m).sum();
    let total_tracked: u32 = days.values().map(|b| b.active_m + b.idle_m).sum();
    let avg_focus = if total_tracked > 0 {
        (total_active as f32 / total_tracked as f32) * 100.0
    } else {
        0.0
    };
    let tools_summary = format_tools_summary(&period_app_totals);

    let doc_breakdown_summary = format_app_durations_summary(&period_doc_app_totals);

    md.push_str("## Work & Application Breakdown\n\n");
    md.push_str(&format!("{}\n\n", doc_breakdown_summary));

    md.push_str("## 4. Weekly Learning Synthesis & Coordinator Verification\n\n");
    md.push_str(&format!(
        "- **Average Verified Active Focus:** {:.1}% ({}m active / {}m total tracked)\n",
        avg_focus, total_active, total_tracked
    ));
    md.push_str(&format!("- **Primary Development Tools:** {}\n", tools_summary));
    md.push_str(&format!(
        "- **Verified Commit Artifacts:** {} commit(s) across {} repository(ies)\n",
        total_commits_logged,
        unique_repos.len()
    ));
    md.push_str(
        "- **Audit Integrity:** Each completed shift embeds an FNV-1a Verification ID (`OJT-...`) derived from date, clock-in/out timestamps, rendered duration, commit hashes, and verified active minutes.\n\n",
    );
    md.push_str("| Trainee Signature | Date | OJT Coordinator / Supervisor | Date |\n");
    md.push_str("| --- | --- | --- | --- |\n");
    md.push_str(&format!(
        "| ________________________ | {} | ________________________ | ________ |\n",
        today
    ));

    md
}

#[tauri::command]
fn export_markdown_report(
    period: String,
    ojt: State<'_, SharedOjtState>,
) -> Result<String, String> {
    let today = format_date(current_local_clock());
    let (dash, export_dir) = {
        let mut state = lock_or_recover(&ojt);
        reconcile_ojt_day(&mut state, &today);
        (
            build_ojt_dashboard(&state),
            state.obsidian_export_dir.clone(),
        )
    };

    let scope = period.trim().to_lowercase();
    let md = if scope == "daily" {
        build_daily_markdown(&today, &dash)
    } else {
        build_weekly_journal_markdown(&today, &scope, &dash)
    };

    if let Some(dir) = export_dir {
        let dir_path = Path::new(&dir);
        fs::create_dir_all(dir_path)
            .map_err(|e| format!("Failed to create Obsidian export folder '{}': {}", dir, e))?;
        let filename = if scope == "daily" {
            format!("OJT-Daily-{}.md", today)
        } else if scope == "weekly" {
            let (_y, w) = iso_week_number(&today);
            format!("OJT-Weekly-Journal-W{:02}.md", w)
        } else {
            "OJT-DTR-Complete.md".to_string()
        };
        let target_file = dir_path.join(filename);
        fs::write(&target_file, &md)
            .map_err(|e| format!("Failed to write export file '{}': {}", target_file.display(), e))?;
    }

    Ok(md)
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let shared_sessions: SharedSessions = Arc::new(Mutex::new(Vec::new()));
    let manual_sessions: ManualSessions = Arc::new(Mutex::new(HashMap::new()));
    let dismissed_sessions: DismissedSessions = Arc::new(Mutex::new(HashMap::new()));
    let approved_prompts = ApprovedPrompts::default();
    let paused_sessions = PausedSessions(Arc::new(Mutex::new(load_paused_ids())));
    let resumed_sessions = ResumedSessions::default();
    let auto_cache = AutoDiscoveredCache(Arc::new(Mutex::new(Vec::new())));
    let ojt_state: SharedOjtState = Arc::new(Mutex::new(load_ojt_state()));

    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .manage(shared_sessions.clone())
        .manage(manual_sessions.clone())
        .manage(dismissed_sessions.clone())
        .manage(approved_prompts.clone())
        .manage(paused_sessions.clone())
        .manage(resumed_sessions.clone())
        .manage(auto_cache.clone())
        .manage(ojt_state.clone())
        .setup(move |app| {
            if let Some(main_win) = app.get_webview_window("main") {
                let _ = main_win.set_shadow(false);
                let _ = position_window_top_right(&main_win, 340);
            }
            let handle = app.handle().clone();
            let shared = app.state::<SharedSessions>().inner().clone();
            let manual = app.state::<ManualSessions>().inner().clone();
            let dismissed = app.state::<DismissedSessions>().inner().clone();
            let approved = app.state::<ApprovedPrompts>().inner().clone();
            let paused = app.state::<PausedSessions>().inner().clone();
            let resumed = app.state::<ResumedSessions>().inner().clone();
            let cache = app.state::<AutoDiscoveredCache>().inner().clone();
            let ojt = app.state::<SharedOjtState>().inner().clone();

            spawn_telemetry_server(
                handle.clone(),
                shared.clone(),
                manual.clone(),
                dismissed.clone(),
                approved.clone(),
                paused.clone(),
                resumed.clone(),
                cache.clone(),
            );
            spawn_auto_discovery_watcher(
                handle, shared, manual, dismissed, approved, paused, resumed, cache,
            );
            spawn_ojt_pow_watcher(ojt);
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            get_sessions,
            resolve_permission,
            submit_agent_prompt,
            toggle_pause_session,
            dismiss_session,
            focus_workspace,
            snap_to_top_right,
            get_system_health,
            kill_process,
            sweep_orphan_node,
            get_ojt_state,
            toggle_shift,
            toggle_shift_break,
            update_ojt_settings,
            scan_today_activity,
            add_manual_activity,
            update_today_reflections,
            export_markdown_report
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
