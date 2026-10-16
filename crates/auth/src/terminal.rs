//! Running a command in a terminal window the user can type into (CLI
//! sign-ins such as `kiro-cli login`). The terminal is a setting: IT often
//! blocks the default one (PowerShell or cmd on Windows), so the user can
//! pick another, give their own command, or run it themselves.
//!
//! Every launch is checked: the script first creates a marker file, and a
//! terminal that doesn't create it within a few seconds counts as blocked.
//! Then the next candidate is tried ("auto"), and in the end the caller gets
//! the command to show the user (`Launch::Manual`).

use base64::Engine as _;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

/// Setting `terminal`: `kind` = "auto" (or empty), "manual", a preset id
/// from [`options`], or "custom" with `custom` = a command line.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct TerminalSetting {
    pub kind: String,
    pub custom: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct TerminalOption {
    pub id: String,
    pub label: String,
    /// Where it was found (found terminals only).
    pub detail: Option<String>,
}

/// What to run.
pub struct TerminalJob<'a> {
    /// File name part of the script (`kiro-login`).
    pub name: &'a str,
    pub title: &'a str,
    /// Program and arguments; `None` only prints `done_message` (a test).
    pub program: Option<&'a str>,
    pub args: &'a [&'a str],
    /// Environment variables removed before running it.
    pub unset_env: &'a [&'a str],
    pub done_message: &'a str,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Launch {
    /// A terminal window runs the command.
    Opened { terminal: String },
    /// The user runs `command` themselves (chosen, or no terminal started:
    /// `reason` says which ones failed).
    Manual { command: String, reason: Option<String> },
}

/// Terminals found on this computer, then "custom" and "manual" ("auto"
/// tries the found ones in this order).
pub fn options() -> Vec<TerminalOption> {
    let mut v = vec![TerminalOption { id: "auto".into(), label: "Automatic".into(), detail: None }];
    for f in detect() {
        v.push(TerminalOption { id: f.id, label: f.label, detail: Some(f.where_) });
    }
    v.push(TerminalOption { id: "custom".into(), label: "Custom command…".into(), detail: None });
    v.push(TerminalOption { id: "manual".into(), label: "Don't open a terminal (I run the command myself)".into(), detail: None });
    v
}

/// A terminal found on this computer and how to run a script in it.
#[derive(Debug, Clone)]
struct Found {
    id: String,
    label: String,
    /// Where it was found (shown in Settings).
    where_: String,
    how: How,
}

#[derive(Debug, Clone)]
enum How {
    /// macOS: `open -a <app> <script>.command` (Terminal, iTerm).
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    OpenScript(String),
    /// `program before… <run>`.
    Exec { program: PathBuf, before: Vec<String>, run: Run },
}

/// What follows the terminal's own arguments.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(not(windows), allow(dead_code))]
enum Run {
    /// `sh <script>`
    Sh,
    /// `cmd.exe /K <script>.cmd`
    Cmd,
    /// `/K <script>.cmd` (the terminal is cmd itself)
    CmdSelf,
    /// `-NoExit -NoProfile -EncodedCommand …` (the terminal is PowerShell)
    Ps,
    /// `-l <script>` with an MSYS path (Git Bash)
    Bash,
}

fn exec(id: &str, label: &str, program: PathBuf, before: &[&str], run: Run) -> Found {
    Found {
        id: id.into(),
        label: label.into(),
        where_: program.to_string_lossy().into(),
        how: How::Exec { program, before: before.iter().map(|s| s.to_string()).collect(), run },
    }
}

/// Look for terminals: app bundles on macOS, install folders and PATH on
/// Windows, PATH and `.desktop` entries (TerminalEmulator) on Linux.
fn detect() -> Vec<Found> {
    let mut out: Vec<Found> = Vec::new();
    #[cfg(target_os = "macos")]
    {
        let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
        let dirs = [PathBuf::from("/System/Applications/Utilities"), PathBuf::from("/Applications/Utilities"), PathBuf::from("/Applications"), home.join("Applications")];
        let app = |name: &str| dirs.iter().map(|d| d.join(format!("{name}.app"))).find(|p| p.exists());
        if let Some(p) = app("Terminal") {
            out.push(Found { id: "terminal".into(), label: "Terminal".into(), where_: p.to_string_lossy().into(), how: How::OpenScript("Terminal".into()) });
        }
        if let Some(p) = app("iTerm").or_else(|| app("iTerm2")) {
            out.push(Found { id: "iterm".into(), label: "iTerm".into(), where_: p.to_string_lossy().into(), how: How::OpenScript(p.to_string_lossy().into()) });
        }
        if let Some(p) = app("Ghostty") {
            let ps = p.to_string_lossy().to_string();
            out.push(Found { where_: ps.clone(), ..exec("ghostty", "Ghostty", "open".into(), &["-na", &ps, "--args", "-e"], Run::Sh) });
        }
        for (name, bin, id, before) in [("WezTerm", "wezterm", "wezterm", &["start", "--"][..]), ("Alacritty", "alacritty", "alacritty", &["-e"][..]), ("kitty", "kitty", "kitty", &[][..])] {
            if let Some(p) = app(name) {
                let b = p.join("Contents/MacOS").join(bin);
                if b.exists() {
                    out.push(exec(id, name, b, before, Run::Sh));
                }
            }
        }
    }
    #[cfg(windows)]
    {
        let env = |v: &str| std::env::var_os(v).map(PathBuf::from);
        let sysroot = env("SystemRoot").unwrap_or_else(|| PathBuf::from(r"C:\Windows"));
        let cmd = env("ComSpec").filter(|p| p.exists()).unwrap_or_else(|| sysroot.join(r"System32\cmd.exe"));
        if cmd.exists() {
            out.push(exec("cmd", "Command Prompt (cmd)", cmd, &[], Run::CmdSelf));
        }
        let ps = sysroot.join(r"System32\WindowsPowerShell\v1.0\powershell.exe");
        if ps.exists() {
            out.push(exec("powershell", "Windows PowerShell", ps, &[], Run::Ps));
        }
        let pf: Vec<PathBuf> = ["ProgramFiles", "ProgramW6432", "ProgramFiles(x86)"].iter().filter_map(|v| env(v)).collect();
        let local = env("LOCALAPPDATA");
        let first = |cands: Vec<PathBuf>, exe: &str| cands.into_iter().find(|p| p.is_file()).or_else(|| which(exe));
        if let Some(p) = first(pf.iter().map(|d| d.join(r"PowerShell\7\pwsh.exe")).collect(), "pwsh") {
            out.push(exec("pwsh", "PowerShell 7 (pwsh)", p, &[], Run::Ps));
        }
        if let Some(p) = first(local.iter().map(|l| l.join(r"Microsoft\WindowsApps\wt.exe")).collect(), "wt") {
            out.push(exec("wt", "Windows Terminal", p, &["new-tab"], Run::Cmd));
        }
        let mut git: Vec<PathBuf> = pf.iter().map(|d| d.join(r"Git\bin\bash.exe")).collect();
        git.extend(local.iter().map(|l| l.join(r"Programs\Git\bin\bash.exe")));
        if let Some(p) = git.into_iter().find(|p| p.is_file()) {
            out.push(exec("git-bash", "Git Bash", p, &[], Run::Bash));
        }
        if let Some(p) = first(pf.iter().map(|d| d.join(r"WezTerm\wezterm.exe")).collect(), "wezterm") {
            out.push(exec("wezterm", "WezTerm", p, &["start", "--"], Run::Cmd));
        }
        if let Some(p) = first(pf.iter().map(|d| d.join(r"Alacritty\alacritty.exe")).collect(), "alacritty") {
            out.push(exec("alacritty", "Alacritty", p, &["-e"], Run::Cmd));
        }
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        // The desktop's default first, then well-known ones, then any
        // other TerminalEmulator a .desktop file lists.
        for (bin, label) in LINUX_KNOWN {
            if let Some(p) = which(bin) {
                out.push(exec(bin, label, p, linux_flag(bin), Run::Sh));
            }
        }
        for (name, bin) in desktop_terminals() {
            if out.iter().any(|f| f.id == bin) {
                continue;
            }
            if let Some(p) = which(&bin) {
                let flag = linux_flag(&bin);
                out.push(exec(&bin, &name, p, flag, Run::Sh));
            }
        }
    }
    out
}

#[cfg_attr(any(target_os = "macos", windows), allow(dead_code))]
const LINUX_KNOWN: &[(&str, &str)] = &[
    ("x-terminal-emulator", "System default (x-terminal-emulator)"),
    ("gnome-terminal", "GNOME Terminal"),
    ("ptyxis", "Ptyxis"),
    ("kgx", "GNOME Console"),
    ("konsole", "Konsole"),
    ("xfce4-terminal", "Xfce Terminal"),
    ("mate-terminal", "MATE Terminal"),
    ("tilix", "Tilix"),
    ("terminator", "Terminator"),
    ("lxterminal", "LXTerminal"),
    ("qterminal", "QTerminal"),
    ("kitty", "kitty"),
    ("alacritty", "Alacritty"),
    ("foot", "foot"),
    ("wezterm", "WezTerm"),
    ("ghostty", "Ghostty"),
    ("xterm", "xterm"),
    ("urxvt", "rxvt-unicode"),
];

/// How a Linux terminal takes a command to run (most use `-e`).
#[cfg_attr(any(target_os = "macos", windows), allow(dead_code))]
fn linux_flag(bin: &str) -> &'static [&'static str] {
    match bin {
        "gnome-terminal" | "ptyxis" | "kgx" => &["--"],
        "xfce4-terminal" | "mate-terminal" | "terminator" => &["-x"],
        "wezterm" => &["start", "--"],
        "kitty" | "foot" => &[],
        _ => &["-e"],
    }
}

/// (Name, program) of a `.desktop` entry in the TerminalEmulator category.
#[cfg_attr(any(target_os = "macos", windows), allow(dead_code))]
fn parse_desktop(text: &str) -> Option<(String, String)> {
    let (mut name, mut exec, mut terminal, mut hidden, mut section) = (None, None, false, false, false);
    for line in text.lines() {
        let l = line.trim();
        if l.starts_with('[') {
            section = l == "[Desktop Entry]";
            continue;
        }
        if !section {
            continue;
        }
        if let Some(v) = l.strip_prefix("Name=") {
            name.get_or_insert_with(|| v.trim().to_string());
        } else if let Some(v) = l.strip_prefix("Exec=") {
            // `env VAR=x prog …` → prog.
            exec = split_args(v).into_iter().find(|w| w != "env" && !w.contains('='));
        } else if let Some(v) = l.strip_prefix("Categories=") {
            terminal = v.split(';').any(|c| c.trim() == "TerminalEmulator");
        } else if l == "NoDisplay=true" || l == "Hidden=true" {
            hidden = true;
        }
    }
    let bin = exec?;
    let bin = Path::new(&bin).file_name()?.to_string_lossy().to_string();
    // Flatpak/Snap wrappers can't take a command the usual way.
    (terminal && !hidden && !["flatpak", "snap"].contains(&bin.as_str())).then(|| (name.unwrap_or_else(|| bin.clone()), bin))
}

#[cfg(all(unix, not(target_os = "macos")))]
fn desktop_terminals() -> Vec<(String, String)> {
    let mut dirs: Vec<PathBuf> = Vec::new();
    if let Some(h) = std::env::var_os("XDG_DATA_HOME").map(PathBuf::from).or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share"))) {
        dirs.push(h.join("applications"));
    }
    let data = std::env::var("XDG_DATA_DIRS").unwrap_or_else(|_| "/usr/local/share:/usr/share".into());
    dirs.extend(data.split(':').filter(|d| !d.is_empty()).map(|d| PathBuf::from(d).join("applications")));
    dirs.push(PathBuf::from("/var/lib/flatpak/exports/share/applications"));
    let mut out: Vec<(String, String)> = Vec::new();
    for d in dirs {
        let Ok(rd) = std::fs::read_dir(&d) else { continue };
        for e in rd.flatten() {
            if e.path().extension().is_some_and(|x| x == "desktop") {
                if let Some(t) = std::fs::read_to_string(e.path()).ok().and_then(|t| parse_desktop(&t)) {
                    if !out.iter().any(|(_, b)| b == &t.1) {
                        out.push(t);
                    }
                }
            }
        }
    }
    out
}

/// A program on PATH.
#[cfg_attr(target_os = "macos", allow(dead_code))]
fn which(exe: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    let exts: &[&str] = if cfg!(windows) { &[".exe", ""] } else { &[""] };
    std::env::split_paths(&path).flat_map(|d| exts.iter().map(move |e| d.join(format!("{exe}{e}")))).find(|p| p.is_file())
}

/// The command line as the user would type it.
pub fn command_line(job: &TerminalJob<'_>) -> String {
    let q = |s: &str| if s.is_empty() || s.contains([' ', '\'', '"', '&', '(', ')']) { format!("\"{s}\"") } else { s.to_string() };
    match job.program {
        Some(p) => std::iter::once(q(p)).chain(job.args.iter().map(|a| q(a))).collect::<Vec<_>>().join(" "),
        None => format!("echo {}", job.done_message),
    }
}

/// Run `job` in the terminal chosen in `setting`.
pub async fn launch(setting: &TerminalSetting, job: &TerminalJob<'_>) -> Launch {
    let manual = |reason: Option<String>| Launch::Manual { command: command_line(job), reason };
    let kind = setting.kind.trim();
    if kind == "manual" {
        return manual(None);
    }
    let scripts = match Scripts::write(job) {
        Ok(s) => s,
        Err(e) => return manual(Some(format!("couldn't write the script: {e}"))),
    };
    let found = detect();
    let candidates: Vec<Option<&Found>> = match kind {
        "" | "auto" => found.iter().map(Some).collect(),
        "custom" if setting.custom.trim().is_empty() => return manual(Some("no custom terminal command is set".into())),
        "custom" => vec![None],
        k => match found.iter().find(|f| f.id == k) {
            Some(f) => vec![Some(f)],
            None => return manual(Some(format!("\"{k}\" was not found on this computer; choose another in Settings → Terminal"))),
        },
    };
    let mut failed = Vec::new();
    for c in candidates {
        let name = c.map(|f| f.label.clone()).unwrap_or_else(|| "the custom terminal command".into());
        match try_terminal(c, &setting.custom, &scripts).await {
            Ok(()) => return Launch::Opened { terminal: name },
            Err(e) => failed.push(format!("{name}: {e}")),
        }
    }
    let _ = std::fs::remove_file(&scripts.marker);
    manual(Some(if failed.is_empty() { "no terminal was found".into() } else { failed.join("; ") }))
}

struct Scripts {
    /// sh (`.command` on macOS, so Terminal runs it).
    sh: PathBuf,
    cmd: PathBuf,
    ps1: PathBuf,
    /// PowerShell code (also sent with -EncodedCommand, which execution
    /// policies don't block, unlike script files).
    ps_code: String,
    marker: PathBuf,
}

impl Scripts {
    fn write(job: &TerminalJob<'_>) -> std::io::Result<Self> {
        let tmp = std::env::temp_dir();
        let base = format!("databrain-{}", job.name);
        let marker = tmp.join(format!("{base}.started"));
        let _ = std::fs::remove_file(&marker);
        let sh = tmp.join(format!("{base}.{}", if cfg!(target_os = "macos") { "command" } else { "sh" }));
        let cmd = tmp.join(format!("{base}.cmd"));
        let ps1 = tmp.join(format!("{base}.ps1"));
        std::fs::write(&sh, sh_script(job, &marker))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&sh, std::fs::Permissions::from_mode(0o700))?;
        }
        let ps_code = ps_script(job, &marker);
        if cfg!(windows) {
            std::fs::write(&cmd, cmd_script(job, &marker))?;
            std::fs::write(&ps1, &ps_code)?;
        }
        Ok(Self { sh, cmd, ps1, ps_code, marker })
    }

    async fn started(&self, secs: u64) -> bool {
        let t = tokio::time::Instant::now();
        while t.elapsed() < Duration::from_secs(secs) {
            if self.marker.exists() {
                let _ = std::fs::remove_file(&self.marker);
                return true;
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
        false
    }
}

fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

fn ps_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

/// cmd.exe: `"…"` around a path; `%` doubled so it isn't expanded.
fn cmd_quote(s: &str) -> String {
    format!("\"{}\"", s.replace('"', "").replace('%', "%%"))
}

fn sh_script(job: &TerminalJob<'_>, marker: &Path) -> String {
    let mut s = format!("#!/bin/sh\n# Opened by DataBrain: {}\ntouch {}\n", job.title.replace('\n', " "), sh_quote(&marker.to_string_lossy()));
    for v in job.unset_env {
        s.push_str(&format!("unset {v}\n"));
    }
    if let Some(p) = job.program {
        s.push_str(&std::iter::once(sh_quote(p)).chain(job.args.iter().map(|a| sh_quote(a))).collect::<Vec<_>>().join(" "));
        s.push_str("\necho\n");
    }
    s.push_str(&format!("echo {}\n", sh_quote(job.done_message)));
    // Some Linux terminals close as soon as the command ends.
    if !cfg!(target_os = "macos") {
        s.push_str("printf 'Press Enter to close. '\nread _\n");
    }
    s
}

fn cmd_script(job: &TerminalJob<'_>, marker: &Path) -> String {
    let mut s = format!("@echo off\r\ntitle {}\r\ntype nul > {}\r\n", job.title.replace(['&', '|', '<', '>', '^'], " "), cmd_quote(&marker.to_string_lossy()));
    for v in job.unset_env {
        s.push_str(&format!("set {v}=\r\n"));
    }
    if let Some(p) = job.program {
        s.push_str(&std::iter::once(cmd_quote(p)).chain(job.args.iter().map(|a| cmd_quote(a))).collect::<Vec<_>>().join(" "));
        s.push_str("\r\necho.\r\n");
    }
    s.push_str(&format!("echo {}\r\n", job.done_message.replace(['&', '|', '<', '>', '^', '%'], " ")));
    s
}

fn ps_script(job: &TerminalJob<'_>, marker: &Path) -> String {
    let mut s = format!(
        "$Host.UI.RawUI.WindowTitle = {}\nNew-Item -ItemType File -Force -Path {} | Out-Null\n",
        ps_quote(job.title),
        ps_quote(&marker.to_string_lossy())
    );
    for v in job.unset_env {
        s.push_str(&format!("Remove-Item Env:{v} -ErrorAction SilentlyContinue\n"));
    }
    if let Some(p) = job.program {
        s.push_str("& ");
        s.push_str(&std::iter::once(ps_quote(p)).chain(job.args.iter().map(|a| ps_quote(a))).collect::<Vec<_>>().join(" "));
        s.push_str("\nWrite-Host ''\n");
    }
    s.push_str(&format!("Write-Host {}\n", ps_quote(job.done_message)));
    s
}

/// `-EncodedCommand` argument: UTF-16LE, base64.
fn ps_encoded(code: &str) -> String {
    let bytes: Vec<u8> = code.encode_utf16().flat_map(|u| u.to_le_bytes()).collect();
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

/// Split a command line into words: spaces separate, "…" and '…' group
/// (no backslash escapes, so Windows paths stay as typed).
pub fn split_args(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quote: Option<char> = None;
    let mut has = false;
    for ch in s.chars() {
        match quote {
            Some(q) if ch == q => quote = None,
            Some(_) => cur.push(ch),
            None if ch == '"' || ch == '\'' => {
                quote = Some(ch);
                has = true;
            }
            None if ch.is_whitespace() => {
                if has || !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                }
                has = false;
            }
            None => cur.push(ch),
        }
    }
    if has || !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// A custom command line with `{script}` (sh on macOS/Linux, .cmd on
/// Windows), `{sh}`, `{cmd}`, `{ps1}` filled in; without any, the script
/// is added at the end.
fn custom_args(template: &str, script: &str, sh: &str, cmd: &str, ps1: &str) -> Vec<String> {
    let mut args = split_args(template);
    let has = args.iter().any(|a| ["{script}", "{sh}", "{cmd}", "{ps1}"].iter().any(|p| a.contains(p)));
    for a in &mut args {
        *a = a.replace("{script}", script).replace("{sh}", sh).replace("{cmd}", cmd).replace("{ps1}", ps1);
    }
    if !has {
        args.push(script.to_string());
    }
    args
}

fn spawn(program: &str, args: &[String]) -> std::io::Result<()> {
    let mut c = std::process::Command::new(program);
    c.args(args).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // Console programs (cmd, powershell, bash) get a window of their own.
        const CREATE_NEW_CONSOLE: u32 = 0x0000_0010;
        c.creation_flags(CREATE_NEW_CONSOLE);
    }
    c.spawn().map(|_| ())
}

async fn try_terminal(found: Option<&Found>, custom: &str, s: &Scripts) -> Result<(), String> {
    let p = |x: &Path| x.to_string_lossy().to_string();
    let (sh, cmd, ps1) = (p(&s.sh), p(&s.cmd), p(&s.ps1));
    let run = |program: &str, args: Vec<String>| spawn(program, &args).map_err(|e| format!("can't start it ({e})"));
    let Some(found) = found else {
        let script = if cfg!(windows) { cmd.clone() } else { sh.clone() };
        let args = custom_args(custom, &script, &sh, &cmd, &ps1);
        let Some((prog, rest)) = args.split_first() else { return Err("the command is empty".into()) };
        run(prog, rest.to_vec())?;
        return if s.started(15).await { Ok(()) } else { Err("it didn't run the command (blocked?)".into()) };
    };
    match &found.how {
        How::OpenScript(app) => open_script(app, &sh, s).await,
        How::Exec { program, before, run: r } => {
            let mut args = before.clone();
            match r {
                Run::Sh => args.extend(["sh".to_string(), sh.clone()]),
                Run::Cmd => args.extend(["cmd.exe".to_string(), "/K".into(), cmd.clone()]),
                Run::CmdSelf => args.extend(["/K".to_string(), cmd.clone()]),
                Run::Ps => args.extend(["-NoExit".to_string(), "-NoProfile".into(), "-EncodedCommand".into(), ps_encoded(&s.ps_code)]),
                Run::Bash => args.extend(["-l".to_string(), msys_path(&sh)]),
            }
            run(&program.to_string_lossy(), args)?;
            if s.started(12).await { Ok(()) } else { Err("it didn't run the command (blocked on this computer?)".into()) }
        }
    }
}

/// macOS Terminal/iTerm: a script opened while the app is still launching
/// can be dropped, so start it and wait first, then open the script (3 tries).
#[cfg(target_os = "macos")]
async fn open_script(app: &str, sh: &str, s: &Scripts) -> Result<(), String> {
    let run = |args: Vec<String>| spawn("open", &args).map_err(|e| format!("can't start it ({e})"));
    let proc = Path::new(app).file_stem().map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| app.to_string());
    let proc = if proc.starts_with("iTerm") { "iTerm2".to_string() } else { proc };
    if !app_running(&proc).await {
        run(vec!["-a".into(), app.into()])?;
        let t = tokio::time::Instant::now();
        while !app_running(&proc).await && t.elapsed() < Duration::from_secs(15) {
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
        tokio::time::sleep(Duration::from_millis(1500)).await;
    }
    for attempt in 0..3 {
        run(vec!["-a".into(), app.into(), sh.into()])?;
        if s.started(if attempt == 0 { 6 } else { 8 }).await {
            return Ok(());
        }
    }
    Err("it didn't run the command (check its settings → Profiles → Shell)".into())
}

#[cfg(not(target_os = "macos"))]
async fn open_script(_app: &str, _sh: &str, _s: &Scripts) -> Result<(), String> {
    Err("only on macOS".into())
}

#[cfg(target_os = "macos")]
async fn app_running(name: &str) -> bool {
    tokio::process::Command::new("pgrep")
        .args(["-x", name])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await
        .is_ok_and(|s| s.success())
}

/// Git Bash path: C:\x\y → /c/x/y.
fn msys_path(p: &str) -> String {
    let p = p.replace('\\', "/");
    match p.as_bytes() {
        [d, b':', ..] => format!("/{}{}", (*d as char).to_ascii_lowercase(), &p[2..]),
        _ => p,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn job() -> TerminalJob<'static> {
        TerminalJob {
            name: "t",
            title: "Kiro sign-in",
            program: Some("C:\\Program Files\\Kiro's\\kiro-cli.exe"),
            args: &["login"],
            unset_env: &["KIRO_API_KEY"],
            done_message: "Done. Return to DataBrain.",
        }
    }

    #[test]
    fn splits_command_lines() {
        assert_eq!(split_args(r#""C:\Program Files\Git\bin\bash.exe" -l {sh}"#), vec![r"C:\Program Files\Git\bin\bash.exe", "-l", "{sh}"]);
        assert_eq!(split_args("open -na 'Ghostty' --args -e {script}"), vec!["open", "-na", "Ghostty", "--args", "-e", "{script}"]);
        assert_eq!(split_args(r#"a "" b"#), vec!["a", "", "b"]);
        assert!(split_args("   ").is_empty());
    }

    #[test]
    fn fills_custom_commands() {
        assert_eq!(custom_args("alacritty -e sh {sh}", "S", "/t/x.sh", "C", "P"), vec!["alacritty", "-e", "sh", "/t/x.sh"]);
        assert_eq!(custom_args("kitty", "S", "x", "C", "P"), vec!["kitty", "S"], "script added when no placeholder");
        assert_eq!(custom_args("pwsh -File {ps1}", "S", "x", "C", "P"), vec!["pwsh", "-File", "P"]);
    }

    #[test]
    fn scripts_mark_the_start_and_quote_paths() {
        let m = Path::new("/tmp/m d.started");
        let sh = sh_script(&job(), m);
        assert!(sh.contains("touch '/tmp/m d.started'") && sh.contains("unset KIRO_API_KEY") && sh.contains("'C:\\Program Files\\Kiro'\\''s\\kiro-cli.exe' 'login'"), "{sh}");
        let cmd = cmd_script(&job(), m);
        assert!(cmd.contains("type nul > \"/tmp/m d.started\"") && cmd.contains("set KIRO_API_KEY=\r\n") && cmd.contains("\"C:\\Program Files\\Kiro's\\kiro-cli.exe\" \"login\""), "{cmd}");
        let ps = ps_script(&job(), m);
        assert!(ps.contains("New-Item -ItemType File -Force -Path '/tmp/m d.started'") && ps.contains("& 'C:\\Program Files\\Kiro''s\\kiro-cli.exe' 'login'"), "{ps}");
        // UTF-16LE base64 of "a" = "YQA=".
        assert_eq!(ps_encoded("a"), "YQA=");
        assert_eq!(command_line(&job()), "\"C:\\Program Files\\Kiro's\\kiro-cli.exe\" login");
    }

    #[tokio::test]
    async fn manual_and_failed_terminals_give_the_command() {
        let manual = TerminalSetting { kind: "manual".into(), custom: String::new() };
        assert_eq!(launch(&manual, &job()).await, Launch::Manual { command: command_line(&job()), reason: None });
        let broken = TerminalSetting { kind: "custom".into(), custom: "/nonexistent/terminal-app {script}".into() };
        match launch(&broken, &TerminalJob { name: "test-broken", ..job() }).await {
            Launch::Manual { reason: Some(r), .. } => assert!(r.contains("custom terminal command") && r.contains("can't start it"), "{r}"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn options_list_found_terminals() {
        let ids: Vec<_> = options().into_iter().map(|o| o.id).collect();
        assert_eq!((ids[0].as_str(), ids[ids.len() - 2].as_str(), ids[ids.len() - 1].as_str()), ("auto", "custom", "manual"));
        #[cfg(target_os = "macos")]
        assert!(ids.iter().any(|i| i == "terminal"), "Terminal.app is found: {ids:?}");
    }

    #[tokio::test]
    async fn a_terminal_not_on_this_computer_gives_the_command() {
        let s = TerminalSetting { kind: "no-such-term".into(), custom: String::new() };
        match launch(&s, &TerminalJob { name: "test-missing", ..job() }).await {
            Launch::Manual { reason: Some(r), .. } => assert!(r.contains("not found on this computer"), "{r}"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn reads_desktop_entries() {
        let t = "[Desktop Entry]\nName=Blackbox\nName[de]=Schwarz\nExec=env GTK_DEBUG=0 /usr/bin/blackbox-terminal --new-window %U\nCategories=GTK;System;TerminalEmulator;\n[Desktop Action new]\nExec=other\n";
        assert_eq!(parse_desktop(t), Some(("Blackbox".into(), "blackbox-terminal".into())));
        assert_eq!(parse_desktop("[Desktop Entry]\nName=W\nExec=flatpak run org.w\nCategories=TerminalEmulator;\n"), None);
        assert_eq!(parse_desktop("[Desktop Entry]\nName=Files\nExec=nautilus\nCategories=Utility;\n"), None);
        assert_eq!(parse_desktop("[Desktop Entry]\nName=T\nExec=t\nCategories=TerminalEmulator;\nNoDisplay=true\n"), None);
        assert_eq!(msys_path(r"C:\Users\a\x.sh"), "/c/Users/a/x.sh");
    }
}
