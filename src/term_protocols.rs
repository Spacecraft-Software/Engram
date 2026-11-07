// SPDX-FileCopyrightText: 2026 Mohamed Hammad & Spacecraft Software
// SPDX-License-Identifier: GPL-3.0-or-later
//! Modern terminal protocols, spoken only where they cannot do harm
//! (`spacecraft-tui-protocols`).
//!
//! Engram uses three of the protocols the skill names:
//!
//! - **Kitty keyboard** (Tier 1): the explore browser pushes "disambiguate
//!   escape codes" after a DA1-fenced probe, so `Esc` is never confused with
//!   the start of a sequence. The probe lives in [`crate::explore`].
//! - **OSC 52 clipboard write** (Tier 1): `y` in the explore browser copies
//!   the highlighted record. See [`osc52`].
//! - **OSC 9;4 progress** (Tier 2): `import` and `index` show progress in
//!   the terminal's tab or taskbar, only on terminals known to handle it.
//!   See [`Progress`].
//!
//! Graphics, animation, rich clipboard and pointer shapes are not used:
//! engram shows no images and has no mouse-interactive regions. Their names
//! are still accepted by the override so one `SPACECRAFT_TERM_PROTOCOLS`
//! value works across every Spacecraft Software program.
//!
//! Every protocol obeys the five rules of the skill's §2: a hard gate before
//! anything is written ([`gate`]), probes fenced by DA1 with a deadline,
//! restore on every exit path ([`restore_on_signal`] plus `Drop`), no sequence
//! another meaning could catch (the OSC 9;4 allow-list in
//! [`progress_terminal`]), and a user override ([`init`]).

use serde_json::{json, Value};
use std::io::{IsTerminal, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

/// The environment variable holding the override list. `--term-protocols`
/// replaces it wholesale when both are given.
pub const ENV_VAR: &str = "SPACECRAFT_TERM_PROTOCOLS";

/// A protocol name from the skill's §2.5 vocabulary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Protocol {
    Keyboard,
    Graphics,
    Animation,
    Clipboard,
    ClipboardRich,
    Progress,
    Pointer,
}

impl Protocol {
    pub const ALL: [Self; 7] = [
        Self::Keyboard,
        Self::Graphics,
        Self::Animation,
        Self::Clipboard,
        Self::ClipboardRich,
        Self::Progress,
        Self::Pointer,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Self::Keyboard => "keyboard",
            Self::Graphics => "graphics",
            Self::Animation => "animation",
            Self::Clipboard => "clipboard",
            Self::ClipboardRich => "clipboard-rich",
            Self::Progress => "progress",
            Self::Pointer => "pointer",
        }
    }

    fn index(self) -> usize {
        Self::ALL
            .iter()
            .position(|&p| p == self)
            .expect("every protocol is in ALL")
    }

    /// What engram does with it, or `None` when engram never emits it.
    fn used_by(self) -> Option<&'static str> {
        match self {
            Self::Keyboard => Some("explore: pushes kitty flag 1 after a DA1-fenced probe"),
            Self::Clipboard => Some("explore: `y` copies the highlighted record with OSC 52"),
            Self::Progress => {
                Some("import, index: OSC 9;4 on allow-listed terminals, text in accessible mode")
            }
            Self::Graphics | Self::Animation | Self::ClipboardRich | Self::Pointer => None,
        }
    }
}

/// Per-protocol forced state: `Some(true)` on, `Some(false)` off, `None`
/// leaves the protocol to detection.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Overrides([Option<bool>; 7]);

impl Overrides {
    pub fn get(self, protocol: Protocol) -> Option<bool> {
        self.0[protocol.index()]
    }
}

/// Parses a list such as `-all,+clipboard` or `-graphics,+progress`.
/// Entries apply left to right, so a later entry wins over an earlier one;
/// a bare name means `+name`. Blank entries are ignored.
///
/// # Errors
///
/// An unknown protocol name, with the accepted names in the message.
pub fn parse(list: &str) -> Result<Overrides, String> {
    let mut out = Overrides::default();
    for entry in list.split(',').map(str::trim).filter(|e| !e.is_empty()) {
        let (on, name) = match entry.as_bytes()[0] {
            b'+' => (true, &entry[1..]),
            b'-' => (false, &entry[1..]),
            _ => (true, entry),
        };
        if name == "all" {
            out.0 = [Some(on); 7];
            continue;
        }
        let Some(protocol) = Protocol::ALL.iter().find(|p| p.name() == name) else {
            return Err(format!(
                "unknown terminal protocol '{name}'; known: all, {}",
                Protocol::ALL.map(Protocol::name).join(", ")
            ));
        };
        out.0[protocol.index()] = Some(on);
    }
    Ok(out)
}

/// Where the resolved overrides came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    Flag,
    Env,
    None,
}

#[derive(Debug, Clone)]
struct Settings {
    overrides: Overrides,
    source: Source,
    raw: Option<String>,
}

static SETTINGS: OnceLock<Settings> = OnceLock::new();

/// Accessible mode as `main` resolved it (`--accessible` or
/// `SPACECRAFT_A11Y`), so [`Progress`] can add its text form.
static ACCESSIBLE: AtomicBool = AtomicBool::new(false);

/// Resolves the override from `--term-protocols` or, without the flag, from
/// [`ENV_VAR`], and records whether accessible mode is on. Called once from
/// `main`, before any command runs.
///
/// # Errors
///
/// The list does not parse; the message says which variable or flag held
/// it, so the user knows which one to fix.
pub fn init(flag: Option<&str>, accessible: bool) -> Result<(), String> {
    ACCESSIBLE.store(accessible, Ordering::Relaxed);
    let (raw, source) = match flag {
        Some(f) => (Some(f.to_owned()), Source::Flag),
        None => match std::env::var(ENV_VAR) {
            Ok(v) if !v.trim().is_empty() => (Some(v), Source::Env),
            _ => (None, Source::None),
        },
    };
    let overrides = match &raw {
        Some(list) => parse(list).map_err(|e| match source {
            Source::Env => format!("{ENV_VAR}: {e}"),
            _ => format!("--term-protocols: {e}"),
        })?,
        None => Overrides::default(),
    };
    let _ = SETTINGS.set(Settings {
        overrides,
        source,
        raw,
    });
    Ok(())
}

fn settings() -> &'static Settings {
    SETTINGS.get_or_init(|| Settings {
        overrides: Overrides::default(),
        source: Source::None,
        raw: None,
    })
}

/// The user's forced state for one protocol, if any.
pub fn forced(protocol: Protocol) -> Option<bool> {
    settings().overrides.get(protocol)
}

/// The skill's §2.1 gate: why no protocol sequence may be written to a
/// stream, or `None` when it may. An override cannot lift this gate —
/// "force on" beats detection, but escape sequences in a pipe or in an
/// agent's transcript are never what the user meant.
pub fn gate(env: impl Fn(&str) -> Option<String>, is_tty: bool) -> Option<&'static str> {
    let set = |name: &str| env(name).is_some_and(|v| !v.is_empty());
    if !is_tty {
        Some("the stream is not a terminal")
    } else if set("AI_AGENT") || set("AGENT") {
        Some("an agent environment is detected (AI_AGENT or AGENT is set)")
    } else if env("CI")
        .is_some_and(|v| !v.is_empty() && v != "0" && !v.eq_ignore_ascii_case("false"))
    {
        Some("CI is set")
    } else {
        match env("TERM").as_deref() {
            None | Some("") => Some("TERM is unset"),
            Some("dumb") => Some("TERM is dumb"),
            Some(_) => None,
        }
    }
}

/// The multiplexer engram is running inside, if any (skill §3).
pub fn multiplexer(env: impl Fn(&str) -> Option<String>) -> Option<&'static str> {
    let set = |name: &str| env(name).is_some_and(|v| !v.is_empty());
    if set("TMUX") {
        Some("tmux")
    } else if set("ZELLIJ") {
        Some("zellij")
    } else if set("STY") {
        Some("GNU screen")
    } else {
        None
    }
}

/// First WezTerm build that draws OSC 9;4 as progress. The 20240203 stable
/// release shows `OSC 9 ; text` as a desktop notification, so it would pop
/// a notification reading `4;1;50` per update.
const WEZTERM_PROGRESS_SINCE: u32 = 20_250_210;

/// First iTerm2 release (major, minor) that draws OSC 9;4 as progress;
/// earlier ones treat OSC 9 as a notification.
const ITERM_PROGRESS_SINCE: (u32, u32) = (3, 6);

/// The terminal OSC 9;4 may be sent to, or why it may not. Allow-list only
/// (skill §2.4): no query tells progress terminals from notification ones,
/// so an unknown terminal gets nothing.
///
/// kitty is absent on purpose: it would need a version, which only an
/// XTVERSION probe can give, and the CLI commands that report progress do
/// not put the terminal into raw mode to ask.
///
/// # Errors
///
/// The reason the terminal is not eligible, for `describe`.
pub fn progress_terminal(
    env: impl Fn(&str) -> Option<String>,
) -> Result<&'static str, &'static str> {
    let set = |name: &str| env(name).is_some_and(|v| !v.is_empty());
    if multiplexer(&env).is_some() {
        return Err("inside a multiplexer; progress is skipped there rather than wrapped");
    }
    if set("WT_SESSION") {
        return Ok("Windows Terminal");
    }
    if set("ConEmuPID") || env("ConEmuANSI").is_some_and(|v| v == "ON") {
        return Ok("ConEmu");
    }
    let program = env("TERM_PROGRAM").unwrap_or_default().to_lowercase();
    let version = env("TERM_PROGRAM_VERSION").unwrap_or_default();
    if program == "wezterm" {
        let date: Option<u32> = version.get(..8).and_then(|d| d.parse().ok());
        return match date {
            Some(d) if d >= WEZTERM_PROGRESS_SINCE => Ok("WezTerm"),
            _ => Err("this WezTerm build shows OSC 9 as a notification (needs 20250210 or later)"),
        };
    }
    if program.contains("iterm") {
        let mut parts = version.split('.').map(|p| p.parse::<u32>().ok());
        let found = (parts.next().flatten(), parts.next().flatten());
        return match found {
            (Some(major), Some(minor)) if (major, minor) >= ITERM_PROGRESS_SINCE => Ok("iTerm2"),
            _ => Err("this iTerm2 shows OSC 9 as a notification (needs 3.6 or later)"),
        };
    }
    for (needle, name) in [
        ("ghostty", "Ghostty"),
        ("vscode", "VS Code"),
        ("rio", "Rio"),
        ("contour", "contour"),
    ] {
        if program.contains(needle) {
            return Ok(name);
        }
    }
    if env("TERM").is_some_and(|t| t.starts_with("foot")) {
        return Ok("foot");
    }
    if set("KITTY_WINDOW_ID") || env("TERM").is_some_and(|t| t == "xterm-kitty") {
        return Err("kitty's version cannot be read without a probe, so progress is skipped");
    }
    Err("the terminal is not on the OSC 9;4 allow-list")
}

/// The resolved state of every protocol, for `describe`. `stderr_is_tty`
/// is passed in because that is the stream every sequence goes to.
pub fn describe(env: impl Fn(&str) -> Option<String>, stderr_is_tty: bool) -> Value {
    let s = settings();
    let gate = gate(&env, stderr_is_tty);
    let mut protocols = serde_json::Map::new();
    for p in Protocol::ALL {
        let forced = s.overrides.get(p);
        let state = match (p.used_by(), gate, forced) {
            (None, _, _) => "unused".to_owned(),
            (Some(_), Some(reason), _) => format!("off: {reason}"),
            (Some(_), None, Some(false)) => "off: turned off by the override".to_owned(),
            (Some(_), None, Some(true)) => "on: forced by the override".to_owned(),
            (Some(_), None, None) => match p {
                Protocol::Keyboard => "auto: probed when explore starts".to_owned(),
                Protocol::Clipboard => "auto: sent on request; no query exists".to_owned(),
                Protocol::Progress => match progress_terminal(&env) {
                    Ok(name) => format!("on: {name}"),
                    Err(reason) => format!("off: {reason}"),
                },
                _ => unreachable!("only used protocols reach detection"),
            },
        };
        protocols.insert(
            p.name().to_owned(),
            json!({
                "used_by": p.used_by(),
                "override": forced.map(|on| if on { "on" } else { "off" }),
                "state": state,
            }),
        );
    }
    json!({
        "override_flag": "--term-protocols",
        "override_env": ENV_VAR,
        "override_source": match s.source {
            Source::Flag => "flag",
            Source::Env => "env",
            Source::None => "none",
        },
        "override_value": s.raw,
        "gate": gate,
        "multiplexer": multiplexer(&env),
        "protocols": protocols,
    })
}

// --- Encoders ---------------------------------------------------------------

/// Largest OSC 52 payload engram sends, measured in base64 bytes. Several
/// terminals and tmux cap the sequence near 100 KB; past that the copy
/// would fail silently, so the browser refuses it and offers export.
pub const OSC52_MAX_BASE64: usize = 100_000;

const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// Standard base64 with padding (RFC 4648 §4). Ten lines here are cheaper
/// than a dependency for the one place engram needs it.
fn base64(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let n = chunk
            .iter()
            .enumerate()
            .fold(0u32, |acc, (i, &b)| acc | (u32::from(b) << (16 - 8 * i)));
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(char::from(B64[((n >> (18 - 6 * i)) & 0x3f) as usize]));
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// `OSC 52 ; c ; <base64> ST`: write `text` to the system clipboard.
pub fn osc52(text: &str) -> String {
    format!("\x1b]52;c;{}\x1b\\", base64(text.as_bytes()))
}

/// The base64 length [`osc52`] would carry for `text`.
pub fn osc52_len(text: &str) -> usize {
    text.len().div_ceil(3) * 4
}

/// `OSC 9 ; 4 ; 1 ; <percent> ST`: normal progress.
fn osc9_4(percent: u8) -> String {
    format!("\x1b]9;4;1;{percent}\x1b\\")
}

/// `OSC 9 ; 4 ; 0 ; 0 ST`: remove the indicator.
const OSC9_4_CLEAR: &str = "\x1b]9;4;0;0\x1b\\";

// --- Restore on signals -----------------------------------------------------

/// Restore functions the signal watcher runs, in registration order. Each
/// must be idempotent and a no-op when nothing is on screen, because the
/// watcher outlives the mode that registered it.
static RESTORERS: std::sync::Mutex<Vec<fn()>> = std::sync::Mutex::new(Vec::new());

/// Arranges for `restore` to run when SIGINT, SIGTERM, SIGHUP or SIGQUIT
/// arrives, after which the signal ends the process as it would have.
///
/// The first call starts one watcher thread for the rest of the process.
/// It is never stopped: signal-hook cannot put back the default
/// disposition, so unregistering would leave those signals ignored. A
/// signal that arrives after the mode has ended therefore still runs the
/// (by then idle) restores and still terminates the process.
///
/// A watcher thread rather than a handler, because the restore writes to
/// the terminal and nothing that takes a lock is async-signal-safe. If the
/// handlers cannot be registered, the program runs without signal-time
/// restore, which is how it ran before, rather than refusing to start.
pub fn restore_on_signal(restore: fn()) {
    {
        let mut list = RESTORERS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !list.contains(&restore) {
            list.push(restore);
        }
    }
    #[cfg(unix)]
    {
        static WATCHER: std::sync::Once = std::sync::Once::new();
        WATCHER.call_once(|| {
            use signal_hook::consts::{SIGHUP, SIGINT, SIGQUIT, SIGTERM};
            let Ok(mut signals) =
                signal_hook::iterator::Signals::new([SIGINT, SIGTERM, SIGHUP, SIGQUIT])
            else {
                return;
            };
            let _ = std::thread::Builder::new()
                .name("engram-term-restore".to_owned())
                .spawn(move || {
                    if let Some(signal) = signals.forever().next() {
                        let list = RESTORERS
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .clone();
                        for restore in list {
                            restore();
                        }
                        let _ = signal_hook::low_level::emulate_default_handler(signal);
                    }
                });
        });
    }
}

// --- Progress ---------------------------------------------------------------

/// Set while an OSC 9;4 indicator is on screen, so the signal path clears
/// exactly what the normal path would.
static PROGRESS_SHOWN: AtomicBool = AtomicBool::new(false);

fn clear_progress() {
    if PROGRESS_SHOWN.swap(false, Ordering::SeqCst) {
        let mut err = std::io::stderr();
        let _ = err.write_all(OSC9_4_CLEAR.as_bytes());
        let _ = err.flush();
    }
}

/// Minimum gap between two OSC 9;4 updates: about five a second.
const PROGRESS_MIN_GAP: Duration = Duration::from_millis(200);

/// Progress for one long operation, on stderr.
///
/// - **OSC 9;4** when stderr passes [`gate`] and the terminal is on the
///   allow-list (or the user forced `+progress`): updated only when the
///   integer percent changes, at most five times a second, and cleared on
///   drop and on a terminating signal.
/// - **Text** in accessible mode (Standard §18 linear mode): one
///   `[INFO]` line at each tenth, because the indicator alone is visual.
///
/// Neither runs when stderr is not a terminal or an agent is detected, so
/// logs and transcripts never see either.
#[derive(Debug)]
pub struct Progress {
    label: &'static str,
    unit: &'static str,
    total: usize,
    osc: bool,
    text: bool,
    last_percent: Option<u8>,
    last_emit: Option<Instant>,
    last_tenth: Option<usize>,
}

impl Progress {
    /// Starts reporting `total` units of work (`unit` names them in the
    /// text form, e.g. "files").
    pub fn start(label: &'static str, unit: &'static str, total: usize) -> Self {
        Self::start_with(label, unit, total, ACCESSIBLE.load(Ordering::Relaxed))
    }

    fn start_with(label: &'static str, unit: &'static str, total: usize, accessible: bool) -> Self {
        let env = |k: &str| std::env::var(k).ok();
        let open = total > 0 && gate(env, std::io::stderr().is_terminal()).is_none();
        let osc = open
            && match forced(Protocol::Progress) {
                Some(on) => on,
                None => progress_terminal(env).is_ok(),
            };
        let text = open && accessible;
        let progress = Self {
            label,
            unit,
            total,
            osc,
            text,
            last_percent: None,
            last_emit: None,
            last_tenth: None,
        };
        if progress.osc {
            restore_on_signal(clear_progress);
        }
        progress
    }

    /// Records that `done` of the units are finished.
    pub fn update(&mut self, done: usize) {
        if self.total == 0 {
            return;
        }
        let done = done.min(self.total);
        let percent = u8::try_from(done * 100 / self.total).unwrap_or(100);
        if self.osc && self.last_percent != Some(percent) {
            let due = self
                .last_emit
                .is_none_or(|t| t.elapsed() >= PROGRESS_MIN_GAP);
            if due {
                let mut err = std::io::stderr();
                PROGRESS_SHOWN.store(true, Ordering::SeqCst);
                let _ = err.write_all(osc9_4(percent).as_bytes());
                let _ = err.flush();
                self.last_percent = Some(percent);
                self.last_emit = Some(Instant::now());
            }
        }
        let tenth = done * 10 / self.total;
        if self.text && self.last_tenth != Some(tenth) {
            self.last_tenth = Some(tenth);
            eprintln!(
                "[INFO] {}: {done}/{} {} ({percent}%)",
                self.label, self.total, self.unit
            );
        }
    }
}

impl Drop for Progress {
    fn drop(&mut self) {
        if self.osc {
            clear_progress();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |k| {
            pairs
                .iter()
                .find(|(name, _)| *name == k)
                .map(|(_, v)| (*v).to_owned())
        }
    }

    #[test]
    fn the_override_list_applies_left_to_right() {
        let o = parse("-all,+clipboard").unwrap();
        assert_eq!(o.get(Protocol::Clipboard), Some(true));
        assert_eq!(o.get(Protocol::Keyboard), Some(false));
        assert_eq!(o.get(Protocol::Progress), Some(false));

        let o = parse(" progress , -keyboard ,").unwrap();
        assert_eq!(o.get(Protocol::Progress), Some(true));
        assert_eq!(o.get(Protocol::Keyboard), Some(false));
        assert_eq!(o.get(Protocol::Graphics), None);

        assert_eq!(parse("").unwrap(), Overrides::default());
        let err = parse("+sparkles").unwrap_err();
        assert!(
            err.contains("sparkles") && err.contains("clipboard-rich"),
            "{err}"
        );
    }

    #[test]
    fn the_gate_closes_on_pipes_agents_ci_and_dumb_terminals() {
        let term = [("TERM", "xterm-256color")];
        assert_eq!(gate(env(&term), true), None);
        assert!(gate(env(&term), false).is_some());
        // Presence, not `=1`: harnesses export descriptive strings.
        let agent = [("TERM", "xterm"), ("AI_AGENT", "claude-code_2-1-319_agent")];
        assert!(gate(env(&agent), true).is_some());
        assert!(gate(env(&[("TERM", "xterm"), ("CI", "true")]), true).is_some());
        assert_eq!(gate(env(&[("TERM", "xterm"), ("CI", "false")]), true), None);
        assert!(gate(env(&[("TERM", "dumb")]), true).is_some());
        assert!(gate(env(&[]), true).is_some());
    }

    #[test]
    fn progress_goes_only_to_allow_listed_terminals() {
        let ok = |pairs: &[(&str, &str)]| progress_terminal(env(pairs)).is_ok();
        assert!(ok(&[("WT_SESSION", "x")]));
        assert!(ok(&[("TERM_PROGRAM", "ghostty")]));
        assert!(ok(&[("TERM", "foot-extra")]));
        // WezTerm's 2024 stable shows OSC 9 as a notification.
        assert!(!ok(&[
            ("TERM_PROGRAM", "WezTerm"),
            ("TERM_PROGRAM_VERSION", "20240203-110809-5046fc22")
        ]));
        assert!(ok(&[
            ("TERM_PROGRAM", "WezTerm"),
            ("TERM_PROGRAM_VERSION", "20250210-075238-c5f9ccb1")
        ]));
        assert!(!ok(&[
            ("TERM_PROGRAM", "iTerm.app"),
            ("TERM_PROGRAM_VERSION", "3.5.11")
        ]));
        assert!(ok(&[
            ("TERM_PROGRAM", "iTerm.app"),
            ("TERM_PROGRAM_VERSION", "3.6.1")
        ]));
        assert!(!ok(&[("TERM", "xterm-kitty")]));
        assert!(!ok(&[("TERM_PROGRAM", "Orca"), ("TERM", "xterm-256color")]));
        // A multiplexer hides the outer terminal; skip rather than guess.
        assert!(!ok(&[
            ("TERM_PROGRAM", "ghostty"),
            ("TMUX", "/tmp/tmux-1000/default,1,0")
        ]));
    }

    #[test]
    fn osc52_carries_standard_base64() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
        assert_eq!(base64("é✓".as_bytes()), "w6ninJM=");
        assert_eq!(osc52("hi"), "\x1b]52;c;aGk=\x1b\\");
        for text in ["", "a", "ab", "abc", "abcd", "é✓"] {
            assert_eq!(osc52_len(text), base64(text.as_bytes()).len());
        }
    }

    #[test]
    fn describe_reports_unused_protocols_and_the_gate() {
        let report = describe(env(&[("TERM", "xterm-256color")]), false);
        assert_eq!(report["gate"], "the stream is not a terminal");
        assert_eq!(report["protocols"]["graphics"]["state"], "unused");
        assert!(report["protocols"]["keyboard"]["state"]
            .as_str()
            .unwrap()
            .starts_with("off:"));
    }

    #[test]
    fn progress_with_no_work_never_turns_on() {
        let mut p = Progress::start_with("test", "items", 0, true);
        assert!(!p.osc && !p.text);
        p.update(0);
    }
}
