// lvim-zbar — a theme-following zellij status bar with inline rename.
//
// zellij hands the plugin the ACTIVE theme's colours in every ModeUpdate event
// (which fires on a theme switch), so the bar repaints LIVE — unlike a zjstatus
// bar whose colours are baked into the layout at session start.
//
// Layout:
//   left  : mode pill · session pill · tabs · transient NOTICE (pipe "notice", payload = text —
//           any plugin broadcast or `zellij pipe -n notice -- "text"`; fades after ~6s)
//   right : CPU · MEM · online · clock
// CPU/MEM/online/clock come from helper scripts run via the RunCommands
// permission (like zjstatus command widgets), refreshed on a timer.
//
// Inline rename — the bar is loaded once per tab (one instance per tab), which
// shapes both flows:
//
// * TAB rename is fully native: the "r" keybind enters zellij's RenameTab input
//   mode, zellij core owns the keystrokes and updates the tab name live; every
//   non-frozen instance sees the buffer through TabUpdate and the visible one
//   renders the active tab pill as an edit field with a cursor. The plugin
//   never touches the keyboard here, so it can never lock it.
//
// * SESSION rename needs key interception (zellij has no RenameSession input
//   mode). The "R" keybind broadcasts a MessagePlugin pipe to ALL instances;
//   each arms intercept_key_presses() and shows the edit field. zellij routes
//   the intercepted keys to exactly one instance; that instance edits the
//   buffer and mirrors it to its siblings with a plugin-to-plugin pipe, so the
//   visible instance always shows the live buffer no matter which instance
//   owns the keys. Enter applies rename_session(), Esc cancels — and both
//   broadcast "rename_close", on which EVERY instance unconditionally clears
//   its intercept. The same "rename_close" pipe is also accepted from the CLI
//   (`zellij pipe --name rename_close`) as an external emergency unlock, so no
//   instance can ever stay armed and hold the keyboard.

use std::collections::BTreeMap;
use zellij_tile::prelude::*;

const TICK: f64 = 2.0;
const SCRIPTS: &str = "/home/biserstoilov/.config/zellij/scripts";
const MAX_NAME_LEN: usize = 60;

#[derive(Default)]
struct State {
    mode_info: ModeInfo,
    tabs: Vec<TabInfo>,
    cpu: String,
    mem: String,
    online: String,
    clock: String,
    permitted: bool,
    // Some(buffer) while an inline session rename is in progress. Every
    // instance keeps a mirror of the buffer; the key-owning instance is the
    // one whose edits are authoritative (it broadcasts them here).
    session_edit: Option<String>,
    // A transient system NOTICE shown next to the tabs — anything can put one
    // there with a pipe named "notice" (payload = the text): another plugin's
    // broadcast (lvim-zsession pipes "session saved") or the CLI
    // (`zellij pipe -n notice -- "text"`). Cleared after NOTICE_TICKS timer
    // ticks; a new notice simply replaces the old one.
    notice: Option<String>,
    notice_ticks: u8,
}

/// How many TICK periods a notice stays on the bar (TICK = 2s → ~6 seconds).
const NOTICE_TICKS: u8 = 3;

register_plugin!(State);

impl ZellijPlugin for State {
    fn load(&mut self, _configuration: BTreeMap<String, String>) {
        // A status bar must never take focus, or keys (splits, etc.) would land
        // on it instead of the shell.
        set_selectable(false);
        request_permission(&[
            PermissionType::ReadApplicationState,
            PermissionType::RunCommands,
            PermissionType::ChangeApplicationState,
            PermissionType::InterceptInput,
            PermissionType::ReadCliPipes,
            PermissionType::MessageAndLaunchOtherPlugins,
        ]);
        subscribe(&[
            EventType::ModeUpdate,
            EventType::TabUpdate,
            EventType::Timer,
            EventType::RunCommandResult,
            EventType::PermissionRequestResult,
            EventType::InterceptedKeyPress,
        ]);
    }

    fn update(&mut self, event: Event) -> bool {
        match event {
            Event::PermissionRequestResult(_) => {
                if !self.permitted {
                    self.permitted = true;
                    self.sample();
                }
                true
            }
            Event::ModeUpdate(mode_info) => {
                let entering_tab_rename = mode_info.mode == InputMode::RenameTab
                    && self.mode_info.mode != InputMode::RenameTab;
                let changed = self.mode_info != mode_info;
                self.mode_info = mode_info;
                // zellij's rename buffer IS the tab's name (core appends and
                // backspaces on it directly), so an auto-named tab would start
                // the edit as "Tab #1" — not our "Tab N" display format. On
                // entering RenameTab mode, normalise the name to the display
                // format so the user edits what the bar shows. Esc still
                // restores the original: core snapshots prev_name on the mode
                // change, before this rename lands.
                if entering_tab_rename {
                    if let Some(tab) = self.tabs.iter().find(|t| t.active) {
                        if is_auto_tab_name(&tab.name) {
                            rename_tab(tab.position as u32 + 1, tab_display_name(tab));
                        }
                    }
                }
                changed
            }
            Event::TabUpdate(tabs) => {
                let changed = self.tabs != tabs;
                self.tabs = tabs;
                changed
            }
            Event::Timer(_) => {
                self.sample();
                // Age the notice out: repaint only on the tick that clears it.
                if self.notice_ticks > 0 {
                    self.notice_ticks -= 1;
                    if self.notice_ticks == 0 {
                        self.notice = None;
                        return true;
                    }
                }
                false
            }
            Event::RunCommandResult(_code, stdout, _stderr, ctx) => {
                let out = String::from_utf8_lossy(&stdout).trim().to_string();
                match ctx.get("id").map(String::as_str) {
                    Some("cpu") => self.cpu = out,
                    Some("mem") => self.mem = out,
                    Some("online") => self.online = out,
                    Some("clock") => self.clock = out,
                    _ => return false,
                }
                true
            }
            Event::InterceptedKeyPress(key) => self.handle_session_edit_key(key),
            _ => false,
        }
    }

    // Pipes reach EVERY instance — even frozen background ones — which is what
    // makes the cross-instance rename protocol (and the emergency unlock) work.
    fn pipe(&mut self, message: PipeMessage) -> bool {
        match message.name.as_str() {
            // From the "R" keybind (MessagePlugin broadcast): start an inline
            // session rename on all instances. All of them arm the intercept;
            // zellij delivers the keys to exactly one of them.
            "rename_session_start" => {
                let name = self
                    .mode_info
                    .session_name
                    .clone()
                    .unwrap_or_default();
                self.session_edit = Some(name);
                intercept_key_presses();
                true
            }
            // Buffer mirror from the key-owning sibling instance.
            "rename_buf" => {
                if self.session_edit.is_some() {
                    self.session_edit = Some(message.payload.unwrap_or_default());
                    true
                } else {
                    false
                }
            }
            // End of rename (Enter/Esc broadcast by the key owner) AND the
            // external emergency unlock (`zellij pipe --name rename_close`).
            // Clearing an intercept that is not armed is harmless, so this is
            // unconditional by design: no instance can stay armed past it.
            //
            // On apply the payload carries the just-applied session name. The
            // ModeUpdate carrying it would arrive only on the NEXT mode change
            // — and even then a keybind pipe (routed straight to the plugin
            // thread) overtakes it — so without this, an immediate second
            // rename would pre-fill the OLD name. Update our copy right here.
            "rename_close" => {
                clear_key_presses_intercepts();
                if let Some(applied) = message.payload {
                    self.mode_info.session_name = Some(applied);
                }
                self.session_edit.take().is_some()
            }
            // A transient system notice for the zone next to the tabs. Generic on
            // purpose: any plugin broadcast or `zellij pipe -n notice -- "text"`
            // lands here. Empty payload clears an existing notice early.
            "notice" => {
                match message.payload.filter(|p| !p.trim().is_empty()) {
                    Some(text) => {
                        self.notice = Some(text.trim().to_string());
                        self.notice_ticks = NOTICE_TICKS;
                    }
                    None => {
                        self.notice = None;
                        self.notice_ticks = 0;
                    }
                }
                true
            }
            _ => false,
        }
    }

    fn render(&mut self, _rows: usize, cols: usize) {
        let c = self.mode_info.style.colors;
        // Accent pills carry dark text on a coloured background, so they want the
        // RAW palette accents — not the contrast-floored versions the zellij
        // generator writes into text_unselected/exit_code/frame (those are floored
        // for readability as text ON the dark surfaces). The generator keeps the
        // raw palette in multiplayer_user_colors, so read the accents from there.
        let m = c.multiplayer_user_colors;
        let green = m.player_3;   // #75783a
        let orange = m.player_4;  // #cc7942
        let yellow = m.player_8;  // #af9e6b
        let teal = m.player_6;    // #357b6d
        let bar_bg = c.text_unselected.background;
        let bar_fg = c.text_unselected.base;
        let acc_bg = green;                         // clock/online pill
        let acc_fg = c.text_unselected.background;  // dark text on accent pills
        let edit_bg = teal;                         // edit-field colour
        let session_editing = self.session_edit.is_some();
        let tab_editing = self.mode_info.mode == InputMode::RenameTab;
        let mode_bg = if session_editing {
            edit_bg
        } else {
            mode_colour(&self.mode_info.mode, &c)
        };

        let session = self
            .mode_info
            .session_name
            .clone()
            .unwrap_or_else(|| "zellij".into());
        let mode = if session_editing {
            "RENAME"
        } else {
            match self.mode_info.mode {
                InputMode::Locked => "LOCKED",
                InputMode::Tmux => "PREFIX",
                InputMode::Pane => "PANE",
                InputMode::Tab => "TAB",
                InputMode::Resize => "RESIZE",
                InputMode::Move => "MOVE",
                InputMode::Scroll => "SCROLL",
                InputMode::EnterSearch | InputMode::Search => "SEARCH",
                InputMode::RenameTab | InputMode::RenamePane => "RENAME",
                InputMode::Session => "SESSION",
                _ => "NORMAL",
            }
        };

        // ── left: mode pill · session pill · tabs ───────────────────────────
        let mut left = String::new();
        left.push_str(&pill(&format!(" {} ", mode), acc_fg, mode_bg)); // mode
        if let Some(buf) = &self.session_edit {
            // Inline edit field: pre-filled buffer + cursor, teal bg.
            left.push_str(&pill(&format!(" {}_ ", buf), acc_fg, edit_bg));
        } else {
            left.push_str(&pill(&format!(" {} ", session), acc_fg, yellow)); // session (theme yellow)
        }
        left.push_str(&fmt(bar_fg, bar_bg));
        for tab in &self.tabs {
            if tab.active && tab_editing {
                // Native RenameTab mode: zellij core edits the name and streams
                // it here via TabUpdate — render it as an inline edit field
                // (raw name: it IS the edit buffer), with a cursor. Until our
                // normalising rename above lands, an auto name would flash as
                // "Tab #N": show the display form for that brief moment.
                left.push_str(&pill(&format!(" {}_ ", tab_display_name(tab)), acc_fg, edit_bg));
                left.push_str(&fmt(bar_fg, bar_bg));
                continue;
            }
            let label = format!(" {} ", tab_display_name(tab));
            if tab.active {
                // yellow bold on a slightly-lighter dark bg (zjstatus style).
                left.push_str(&pill(&label, yellow, c.text_selected.background));
                left.push_str(&fmt(bar_fg, bar_bg));
            } else {
                // yellow, but not bold.
                left.push_str(&fmt(yellow, bar_bg));
                left.push_str(&label);
                left.push_str(&fmt(bar_fg, bar_bg));
            }
        }

        // ── the notice zone: a transient system message next to the tabs ────
        if let Some(notice) = &self.notice {
            left.push(' ');
            left.push_str(&pill(&format!(" {} ", notice), acc_fg, teal));
            left.push_str(&fmt(bar_fg, bar_bg));
        }

        // ── right: CPU · MEM · online · clock ───────────────────────────────
        let mut right = String::new();
        if !self.cpu.is_empty() {
            right.push_str(&fmt(orange, bar_bg));
            right.push_str(&format!(" CPU {} ", self.cpu));
        }
        if !self.mem.is_empty() {
            right.push_str(&fmt(bar_fg, bar_bg));
            right.push_str(&format!("MEM {} ", self.mem));
        }
        if !self.online.is_empty() {
            right.push_str(&fmt(acc_bg, bar_bg));
            right.push_str(&format!(" {} ", self.online));
        }
        if !self.clock.is_empty() {
            right.push_str(&pill(&format!(" {} ", self.clock), acc_fg, acc_bg));
        }

        // ── compose on bar_bg ───────────────────────────────────────────────
        let pad = cols.saturating_sub(visible_width(&left) + visible_width(&right));
        let mut out = String::new();
        out.push_str(&fmt(bar_fg, bar_bg));
        out.push_str(&left);
        out.push_str(&" ".repeat(pad));
        out.push_str(&right);
        out.push_str("\u{1b}[0m");
        print!("{}", out);
    }
}

impl State {
    fn sample(&mut self) {
        run(&["sh", "-c", &format!("{}/cpu.sh", SCRIPTS)], "cpu");
        run(&["sh", "-c", &format!("{}/mem.sh", SCRIPTS)], "mem");
        run(&["sh", "-c", &format!("{}/online.sh", SCRIPTS)], "online");
        run(&["sh", "-c", "date +'%a %d %b  %H:%M'"], "clock");
        set_timeout(TICK);
    }

    // Handles one intercepted key on the instance zellij routed it to. Only
    // that instance gets keys, so it is the buffer's owner: it edits and then
    // mirrors the result to all sibling instances.
    fn handle_session_edit_key(&mut self, key: KeyWithModifier) -> bool {
        let Some(buf) = self.session_edit.as_mut() else {
            // Keys while not editing means a stray intercept: disarm at once.
            clear_key_presses_intercepts();
            return false;
        };
        let ctrl = key.key_modifiers.contains(&KeyModifier::Ctrl);
        let alt = key.key_modifiers.contains(&KeyModifier::Alt);
        match key.bare_key {
            BareKey::Enter => {
                let name = buf.trim().to_string();
                if name.is_empty() {
                    self.end_session_edit(None);
                } else {
                    rename_session(&name);
                    self.end_session_edit(Some(name));
                }
                true
            }
            BareKey::Esc => {
                self.end_session_edit(None);
                true
            }
            BareKey::Char('c') if ctrl => {
                self.end_session_edit(None);
                true
            }
            BareKey::Char('u') if ctrl => {
                buf.clear();
                self.broadcast_buf();
                true
            }
            BareKey::Backspace => {
                buf.pop();
                self.broadcast_buf();
                true
            }
            BareKey::Char(ch) if !ctrl && !alt => {
                if buf.chars().count() < MAX_NAME_LEN && !ch.is_control() {
                    buf.push(ch);
                    self.broadcast_buf();
                }
                true
            }
            // Swallow everything else while editing (arrows, F-keys, ...).
            _ => false,
        }
    }

    // Finish the rename on this instance and tell every sibling to finish too.
    // The broadcast also reaches this instance again — by then the state is
    // already clear and the extra clear_key_presses_intercepts() is harmless.
    //
    // NOTE: deliberately NO .with_plugin_url() here. A url-addressed message
    // goes through zellij's get_or_load_plugins() and — when the url fails to
    // match the running instances — LAUNCHES a fresh floating instance instead
    // of reaching the bars (observed in testing). A url-less MessageToPlugin
    // takes the pipe_to_all_plugins() path: it broadcasts to every RUNNING
    // plugin instance and can never launch anything. Foreign plugins simply
    // ignore our message names.
    // `applied` is the session name just set via rename_session() (None on
    // cancel); it rides in the close broadcast so every instance learns the
    // new name immediately — see the note in pipe() on "rename_close".
    fn end_session_edit(&mut self, applied: Option<String>) {
        clear_key_presses_intercepts();
        self.session_edit = None;
        if let Some(name) = &applied {
            self.mode_info.session_name = Some(name.clone());
        }
        let mut msg = MessageToPlugin::new("rename_close");
        if let Some(name) = applied {
            msg = msg.with_payload(name);
        }
        pipe_message_to_plugin(msg);
    }

    // Mirror the authoritative buffer to all instances so the visible one
    // (wherever it is) renders what the user types. Url-less broadcast — see
    // the note on end_session_edit().
    fn broadcast_buf(&self) {
        if let Some(buf) = &self.session_edit {
            pipe_message_to_plugin(
                MessageToPlugin::new("rename_buf").with_payload(buf.clone()),
            );
        }
    }
}

// ── helpers ─────────────────────────────────────────────────────────────────

fn run(cmd: &[&str], id: &str) {
    let mut ctx = BTreeMap::new();
    ctx.insert("id".to_string(), id.to_string());
    run_command(cmd, ctx);
}

// zellij auto-names tabs "Tab #<n>" (n is a running counter, not necessarily
// position+1). Any such name counts as unnamed.
fn is_auto_tab_name(name: &str) -> bool {
    match name.strip_prefix("Tab #") {
        Some(rest) => !rest.is_empty() && rest.bytes().all(|b| b.is_ascii_digit()),
        None => false,
    }
}

// The name to show for a tab: our "Tab N" style until the user renames it.
fn tab_display_name(tab: &TabInfo) -> String {
    if is_auto_tab_name(&tab.name) {
        format!("Tab {}", tab.position + 1)
    } else {
        tab.name.clone()
    }
}

// A distinct theme colour per mode, so the leftmost pill signals the mode by bg.
// All from the RAW palette (multiplayer_user_colors) — see the note in render().
fn mode_colour(mode: &InputMode, c: &Styling) -> PaletteColor {
    let m = c.multiplayer_user_colors;
    match mode {
        InputMode::Locked => m.player_9,                              // red
        InputMode::Tmux => m.player_5,                                // purple (prefix)
        InputMode::Pane => m.player_2,                                // blue
        InputMode::Tab => m.player_4,                                 // orange
        InputMode::Resize => m.player_6,                             // teal
        InputMode::Move => m.player_1,                                // magenta/brown
        InputMode::Scroll => m.player_8,                              // yellow
        InputMode::Search | InputMode::EnterSearch => m.player_4,     // orange
        InputMode::RenameTab | InputMode::RenamePane => m.player_6,   // teal
        InputMode::Session => m.player_5,                             // purple
        _ => m.player_3,                                              // NORMAL: green
    }
}

fn ansi(color: PaletteColor, layer: u8) -> String {
    match color {
        PaletteColor::Rgb((r, g, b)) => format!("\u{1b}[{};2;{};{};{}m", layer, r, g, b),
        PaletteColor::EightBit(n) => format!("\u{1b}[{};5;{}m", layer, n),
    }
}

fn fmt(fg: PaletteColor, bg: PaletteColor) -> String {
    format!("{}{}", ansi(fg, 38), ansi(bg, 48))
}

fn pill(text: &str, fg: PaletteColor, bg: PaletteColor) -> String {
    format!("{}\u{1b}[1m{}\u{1b}[22m", fmt(fg, bg), text)
}

fn visible_width(s: &str) -> usize {
    let mut w = 0usize;
    let mut in_esc = false;
    for ch in s.chars() {
        if in_esc {
            if ch == 'm' {
                in_esc = false;
            }
        } else if ch == '\u{1b}' {
            in_esc = true;
        } else {
            w += 1;
        }
    }
    w
}
