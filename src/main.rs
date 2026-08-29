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
//
// THE HINTS BAR — the same wasm loaded a SECOND time, with `bar "hints"` in its
// layout configuration, is the bar on TOP: it draws the keybinds of the current
// mode (straight out of ModeInfo, which we already hold — no extra
// subscription) and can be toggled away with a keybind that is itself never
// drawn.
//
// The toggle has to survive one-instance-per-tab: a url-addressed keybind pipe
// reaches EVERY status instance, so "flip it" would flip N times. What travels
// between the instances is therefore the RESULTING state, never a flip — each
// recipient computes the same next value and broadcasts the idempotent
// "hints_set on|off", which every instance (both roles) applies to its shadow
// of the state. See the pipe() arms.

use std::collections::BTreeMap;
use zellij_tile::prelude::actions::{Action, SearchDirection, SearchOption};
use zellij_tile::prelude::*;

const TICK: f64 = 2.0;
// Resolved by the host shell (run via `sh -c`), so it is portable and carries no
// hardcoded username. The widget scripts live under the zellij config dir.
const SCRIPTS: &str = "${ZELLIJ_CONFIG_DIR:-$HOME/.config/zellij}/scripts";
const MAX_NAME_LEN: usize = 60;

/// The pipe name the toggle keybind sends — and the ONE bind the hints bar
/// never draws, so the combination that summons it stays invisible.
const HINTS_TOGGLE: &str = "hints_toggle";
/// Which of the two bars this instance is. One wasm, two roles, told apart by
/// the layout's plugin configuration (`bar "hints"`).
#[derive(Default, PartialEq, Clone, Copy)]
enum Role {
    /// The bar at the bottom: mode, session, tabs, notice, widgets.
    #[default]
    Status,
    /// The bar on top: the current mode's keybinding hints.
    Hints,
}

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
    // Which bar this instance draws.
    role: Role,
    // Our own pane id, so we can hide/show THIS pane without touching focus.
    own_id: u32,
    // Shadow of the hints bar's state, kept by BOTH roles: every instance has
    // to compute the same next value for the idempotent toggle broadcast.
    // Hidden rather than visible, so the derived Default (false) means "shown".
    hints_hidden: bool,
}

/// How many TICK periods a notice stays on the bar (TICK = 2s → ~6 seconds).
const NOTICE_TICKS: u8 = 3;

/// The load figures turn from blue to red at these levels.
const CPU_WARN_PERCENT: u32 = 80;
const MEM_WARN_PERCENT: u32 = 85;

register_plugin!(State);

impl ZellijPlugin for State {
    fn load(&mut self, configuration: BTreeMap<String, String>) {
        // A status bar must never take focus, or keys (splits, etc.) would land
        // on it instead of the shell.
        set_selectable(false);
        // Two bars out of one wasm: `bar "hints"` in the layout makes this
        // instance the keybinding-hints bar on top. Anything else — including no
        // configuration at all, which is how the status bar is loaded and what
        // the url-addressed keybind pipes match — is the status bar.
        if configuration.get("bar").map(String::as_str) == Some("hints") {
            self.role = Role::Hints;
        }
        self.own_id = get_plugin_ids().plugin_id;
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
                    // Only the status bar has widgets; sampling on the hints bar
                    // would run every widget script twice per tick.
                    if self.role == Role::Status {
                        self.sample();
                    }
                    // MessageAndLaunchOtherPlugins is granted only now, so this
                    // is the earliest we can ask the instances in the other tabs
                    // whether the hints bar is currently on. In the first tab
                    // nobody answers and the default (shown) stands.
                    pipe_message_to_plugin(
                        MessageToPlugin::new("hints_query")
                            .with_payload(self.own_id.to_string()),
                    );
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
                // The hints bar has no notice zone — and no timer to age one
                // out, so it must not keep the state either.
                if self.role != Role::Status {
                    return false;
                }
                match message.payload {
                    // A CLI pipe ends with a payload of None — the END of the stream, not a
                    // message. Ignore it, or the notice would vanish the instant the CLI closes.
                    None => false,
                    // An explicitly EMPTY payload clears an existing notice early.
                    Some(text) if text.trim().is_empty() => {
                        self.notice = None;
                        self.notice_ticks = 0;
                        true
                    }
                    Some(text) => {
                        self.notice = Some(text.trim().to_string());
                        self.notice_ticks = NOTICE_TICKS;
                        true
                    }
                }
            }
            // The toggle keybind (`MessagePlugin ... { name "hints_toggle"; }`).
            // It is url-addressed, so it lands on every STATUS instance — one
            // per tab. Nobody flips anything here: each recipient computes the
            // same next state from its own shadow and broadcasts it as the
            // idempotent "hints_set", so N deliveries do the work of one.
            //
            // Keybind source only. A CLI pipe ends with a payload of None — a
            // second delivery that would toggle straight back; from the CLI use
            // "hints_set" instead, which says what it means.
            HINTS_TOGGLE => {
                if message.source == PipeSource::Keybind {
                    self.broadcast_hints_state(!self.hints_hidden);
                }
                false
            }
            // The one authoritative message: the hints bar is on, or it is off.
            // Sent by the toggle above, as the answer to a newcomer's
            // "hints_query", and usable from the CLI:
            //   zellij pipe --name hints_set -- off
            "hints_set" => {
                // A CLI pipe's terminating None is the end of the stream, not a
                // message (the same gotcha as "notice").
                let Some(state) = message.payload else {
                    return false;
                };
                let hidden = state.trim() == "off";
                if hidden == self.hints_hidden {
                    return false;
                }
                self.hints_hidden = hidden;
                if self.role == Role::Hints {
                    self.apply_hints_visibility();
                    return true;
                }
                false
            }
            // A freshly loaded instance (a new tab) asking what the state is.
            // The payload is the asker's own plugin id, because the broadcast
            // comes back to the sender too and it must not answer itself.
            "hints_query" => {
                if message.payload.as_deref() != Some(self.own_id.to_string().as_str()) {
                    self.broadcast_hints_state(self.hints_hidden);
                }
                false
            }
            _ => false,
        }
    }

    fn render(&mut self, _rows: usize, cols: usize) {
        match self.role {
            Role::Status => self.render_status(cols),
            Role::Hints => self.render_hints(cols),
        }
    }
}

impl State {
    // The bar at the bottom: mode pill · session pill · tabs · notice, then the
    // widgets on the right.
    fn render_status(&self, cols: usize) {
        let c = self.mode_info.style.colors;
        // Accent pills carry dark text on a coloured background, so they want the
        // RAW palette accents — not the contrast-floored versions the zellij
        // generator writes into text_unselected/exit_code/frame (those are floored
        // for readability as text ON the dark surfaces). The generator keeps the
        // raw palette in multiplayer_user_colors, so read the accents from there.
        let m = c.multiplayer_user_colors;
        let green = m.player_3;   // #75783a
        let yellow = m.player_8;  // #af9e6b
        let teal = m.player_6;    // #357b6d — the palette's cyan
        let blue = m.player_2;    // #42728b
        let red = m.player_9;     // #cb4f4f — warnings
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
            mode_label(&self.mode_info.mode)
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
        // Plain coloured text (the theme's cyan) on the bar background — a pill would shout; a
        // notice only speaks.
        if let Some(notice) = &self.notice {
            left.push_str(&fmt(teal, bar_bg));
            left.push_str(&format!("  {} ", notice));
            left.push_str(&fmt(bar_fg, bar_bg));
        }

        // ── right: CPU · MEM · online · clock ───────────────────────────────
        let mut right = String::new();
        if !self.cpu.is_empty() {
            // Blue while healthy, red past the warning threshold.
            let warn = percent(&self.cpu).map_or(false, |p| p >= CPU_WARN_PERCENT);
            right.push_str(&fmt(if warn { red } else { blue }, bar_bg));
            right.push_str(&format!(" CPU {} ", self.cpu));
        }
        if !self.mem.is_empty() {
            let warn = used_of_total_percent(&self.mem).map_or(false, |p| p >= MEM_WARN_PERCENT);
            right.push_str(&fmt(if warn { red } else { blue }, bar_bg));
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

    // The bar on top: the current mode's keybinds. While it is toggled off it
    // draws an EMPTY line — whether or not the host actually reclaimed the row,
    // the worst case is then a blank bar and not a stale one.
    //
    // No mode pill here: the key boxes are painted in the mode's own colour (and
    // the bar at the bottom names the mode anyway), which buys back the width.
    fn render_hints(&self, cols: usize) {
        let c = self.mode_info.style.colors;
        let blank = format!(
            "{}{}\u{1b}[0m",
            fmt(c.text_unselected.base, c.text_unselected.background),
            " ".repeat(cols)
        );
        if self.hints_hidden {
            print!("{}", blank);
            return;
        }
        // The native bar's answer to a narrow terminal: render the hints at
        // several levels of detail and show the fullest one that fits, rather
        // than cutting the tail off the only rendering there is.
        let hints = hint_groups(&self.mode_info);
        let line = self
            .compose_hints(&hints, Level::AllKeys, cols, false)
            .or_else(|| self.compose_hints(&hints, Level::OneKey, cols, false))
            // Still too long: cut the tail and say so with an ellipsis. Bare
            // keys would fit far more of them, but a key with no name teaches
            // nothing — better fewer hints that still mean something, and
            // hint_groups has already put the important ones first.
            // (`cut` always yields a line, so the blank is unreachable.)
            .or_else(|| self.compose_hints(&hints, Level::OneKey, cols, true))
            .unwrap_or(blank);
        print!("{}", line);
    }

    // One rendering of the hints at a given level of detail: each hint is a KEY
    // box in the mode's own colour, then — square edge to square edge, no arrow
    // separators — a NEUTRAL label box saying what the key does.
    //
    // Returns None when the line does not fit and `cut` is off; that is the
    // caller's signal to try a shorter level instead of amputating this one.
    fn compose_hints(
        &self,
        hints: &[Hint],
        level: Level,
        cols: usize,
        cut: bool,
    ) -> Option<String> {
        let c = self.mode_info.style.colors;
        let bar_bg = c.text_unselected.background;
        let bar_fg = c.text_unselected.base;
        // Taken from zellij's own status bar (sampled off it, pixel for pixel):
        // the key is emphasis_2 straight on the bar, the label is a ribbon.
        let key_bg = bar_bg;
        let key_fg = c.text_unselected.emphasis_2;
        let label_bg = c.ribbon_unselected.background;
        let label_fg = c.ribbon_unselected.base;

        let mut out = String::new();
        out.push_str(&fmt(bar_fg, bar_bg));
        let mut width = 0usize;
        for hint in hints {
            let keys = match level {
                Level::AllKeys => hint.keys.join("|"),
                _ => hint.best_key().to_string(),
            };
            let label = hint.label.as_str();
            // the key and its padding + the gap after the hint, plus the label
            // box with a space either side.
            let need = keys.chars().count() + 3 + label.chars().count() + 2;
            if width + need > cols {
                if !cut {
                    return None;
                }
                if width + 2 <= cols {
                    out.push_str(&fmt(key_bg, bar_bg));
                    out.push_str(" \u{2026}");
                    width += 2;
                }
                break;
            }
            width += need;
            out.push_str(&pill(&format!(" {} ", keys), key_fg, key_bg));
            // Deliberately not bold: the key shouts, the label only names.
            out.push_str(&fmt(label_fg, label_bg));
            out.push_str(&format!(" {} ", label));
            out.push_str(&fmt(bar_fg, bar_bg));
            out.push(' ');
        }
        out.push_str(&" ".repeat(cols.saturating_sub(width)));
        out.push_str("\u{1b}[0m");
        Some(out)
    }

    // Tell every instance — both roles — what the hints bar's state IS. The
    // message carries the result and never "flip", so the N copies a
    // url-addressed keybind produces collapse into a single effect. Url-less
    // broadcast, for the reason spelled out on end_session_edit().
    fn broadcast_hints_state(&self, hidden: bool) {
        pipe_message_to_plugin(
            MessageToPlugin::new("hints_set").with_payload(if hidden { "off" } else { "on" }),
        );
    }

    // Hide or show THIS pane. Deliberately not hide_self()/show_self(): the
    // latter is documented to focus the pane and switch to its tab, and a bar
    // must never take focus — the pane-id variants take an explicit
    // "do not focus" flag instead.
    fn apply_hints_visibility(&self) {
        let id = PaneId::Plugin(self.own_id);
        if self.hints_hidden {
            hide_pane_with_id(id);
        } else {
            show_pane_with_id(id, false, false);
        }
    }

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

// The bar's name for a mode — shared by both bars, so a mode switch hint on top
// reads exactly like the pill that appears at the bottom once you take it.
fn mode_label(mode: &InputMode) -> &'static str {
    match mode {
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
}

/// One hint: the keys that do a thing, and zellij's own name for the thing.
struct Hint {
    keys: Vec<String>,
    label: String,
}

impl Hint {
    // The key to show when there is room for only one: the shortest, which
    // picks the plain letter over "Ctrl+..." and over an arrow key.
    fn best_key(&self) -> &str {
        self.keys
            .iter()
            .min_by_key(|key| key.chars().count())
            .map(String::as_str)
            .unwrap_or("")
    }
}

/// How much of a hint survives — the rungs the hints bar climbs down when the
/// terminal is too narrow for the fullest rendering, the way the native bar
/// does it rather than amputating the tail of a single rendering.
#[derive(Clone, Copy, PartialEq)]
enum Level {
    /// Every key that does the job: `h/j/k/l` `Move focus`.
    AllKeys,
    /// One key per hint: `h` `Move focus`.
    OneKey,
}

// The wording never shrinks — zellij's own words are the point of the bar, and
// "Down" in place of "Split down" reads as a different bar. Width buys fewer
// keys and then fewer hints, never abbreviations.

/// The order zellij's own bar puts its hints in, extended with the rest of what
/// we can label. Anything not listed keeps its place in the config, after these
/// — so when the line has to be cut, what goes is what matters least.
const HINT_ORDER: &[&str] = &[
    // The head follows zellij's own tmux-mode line, hint for hint.
    "Move focus",
    "Split down",
    "Split right",
    "Split left",
    "Split up",
    "New Pane",
    "Fullscreen",
    "New tab",
    "Close",
    "Rename tab",
    "Rename pane",
    "Previous Tab",
    "Next Tab",
    "Select pane",
    // Then the rest of what we can label, most-reached-for first.
    "Toggle Floating",
    "Toggle Embed",
    "New Floating",
    "Stack",
    "Increase/Decrease size",
    "Move pane",
    "Move tab",
    "Toggle",
    "Detach",
    "Quit",
    "Scroll",
    "Enter search term",
    "Search down",
    "Search up",
    "Case sensitive",
    "Whole words",
    "Edit scrollback in default editor",
    "Break pane to new tab",
    "Break pane left/right",
    "Switch Location",
    "Sync",
    "Frames",
    "Pin",
    "Copy",
    "Clear",
    "Run",
    "Plugin Manager",
];

// The current mode's keybinds, grouped by what they DO: one hint per label — so
// the six keys that scroll become a single "j|k|Ctrl f|Ctrl b|d|u Scroll", the
// way zellij groups them — then ordered by HINT_ORDER.
fn hint_groups(mode_info: &ModeInfo) -> Vec<Hint> {
    let mut hints: Vec<Hint> = Vec::new();
    for (key, actions) in mode_info.get_mode_keybinds() {
        let Some(label) = action_label(&actions, mode_info.mode) else {
            continue;
        };
        let key = key.to_string();
        match hints.iter_mut().find(|hint| hint.label == label) {
            Some(hint) => {
                // Every key, however many: zellij prints all six of
                // "j|k|Ctrl f|Ctrl b|d|u" and the width ladder handles the rest.
                if !hint.keys.contains(&key) {
                    hint.keys.push(key);
                }
            }
            None => hints.push(Hint { label, keys: vec![key] }),
        }
    }
    // Stable, so hints sharing a rank (and everything unlisted) keep the order
    // they have in the config.
    hints.sort_by_key(|hint| {
        HINT_ORDER
            .iter()
            .position(|known| *known == hint.label)
            .unwrap_or(HINT_ORDER.len())
    });
    hints
}

// One bind may carry several actions; the first one that has a name gives the
// hint its label.
fn action_label(actions: &[Action], mode: InputMode) -> Option<String> {
    actions.iter().find_map(|action| single_action_label(action, mode))
}

// What an action is called, or None for the ones that would only add noise
// (per-tab jumps, raw writes to the terminal) — and for OUR OWN toggle, the one
// bind the hints bar must never advertise.
//
// The words are zellij's own, lifted from its status bar so the two bars read
// identically.
fn single_action_label(action: &Action, mode: InputMode) -> Option<String> {
    let label = match action {
        Action::SwitchToMode { input_mode } | Action::SwitchModeForAllClients { input_mode } => {
            return Some(switch_label(*input_mode, mode))
        }
        Action::SkipConfirm { action } => return single_action_label(action, mode),
        Action::KeybindPipe { name, .. } => {
            let name = name.as_deref()?;
            if name == HINTS_TOGGLE {
                return None;
            }
            return Some(humanise(name));
        }
        Action::NewPane { direction: Some(Direction::Down), .. }
        | Action::NewTiledPane { direction: Some(Direction::Down), .. } => "Split down",
        Action::NewPane { direction: Some(Direction::Right), .. }
        | Action::NewTiledPane { direction: Some(Direction::Right), .. } => "Split right",
        Action::NewPane { direction: Some(Direction::Left), .. }
        | Action::NewTiledPane { direction: Some(Direction::Left), .. } => "Split left",
        Action::NewPane { direction: Some(Direction::Up), .. }
        | Action::NewTiledPane { direction: Some(Direction::Up), .. } => "Split up",
        Action::NewPane { .. } | Action::NewTiledPane { .. } | Action::NewInPlacePane { .. } => {
            "New Pane"
        }
        Action::NewStackedPane { .. } => "Stack",
        Action::NewFloatingPane { .. } => "New Floating",
        Action::NewTab { .. } => "New tab",
        Action::CloseFocus | Action::CloseTab => "Close",
        Action::ToggleFocusFullscreen => "Fullscreen",
        Action::TogglePaneFrames => "Frames",
        Action::ToggleFloatingPanes => "Toggle Floating",
        Action::TogglePaneEmbedOrFloating => "Toggle Embed",
        Action::TogglePanePinned => "Pin",
        Action::ToggleActiveSyncTab => "Sync",
        Action::MoveFocus { .. } | Action::MoveFocusOrTab { .. } => "Move focus",
        Action::SwitchFocus | Action::FocusNextPane | Action::FocusPreviousPane => {
            "Select pane"
        }
        Action::MovePane { .. } | Action::MovePaneBackwards => "Move pane",
        Action::MoveTab { .. } => "Move tab",
        Action::GoToNextTab => "Next Tab",
        Action::GoToPreviousTab => "Previous Tab",
        Action::ToggleTab => "Toggle",
        Action::Resize { .. } => "Increase/Decrease size",
        // One hint for every kind of scrolling — zellij groups them, so a scroll
        // mode reads "j|k|Ctrl f|Ctrl b|d|u Scroll" and not three separate hints.
        Action::ScrollUp
        | Action::ScrollDown
        | Action::ScrollToTop
        | Action::ScrollToBottom
        | Action::PageScrollUp
        | Action::PageScrollDown
        | Action::HalfPageScrollUp
        | Action::HalfPageScrollDown => "Scroll",
        Action::EditScrollback { .. } => "Edit scrollback in default editor",
        Action::Search { direction: SearchDirection::Down } => "Search down",
        Action::Search { direction: SearchDirection::Up } => "Search up",
        Action::SearchToggleOption { option: SearchOption::CaseSensitivity } => {
            "Case sensitive"
        }
        Action::SearchToggleOption { option: SearchOption::WholeWord } => "Whole words",
        Action::SearchToggleOption { option: SearchOption::Wrap } => "Wrap",
        Action::BreakPane => "Break pane to new tab",
        Action::BreakPaneLeft | Action::BreakPaneRight => "Break pane left/right",
        Action::PreviousSwapLayout | Action::NextSwapLayout => "Switch Location",
        Action::ClearScreen => "Clear",
        Action::Copy => "Copy",
        Action::Confirm => "Yes",
        Action::Deny => "No",
        Action::Run { .. } => "Run",
        Action::LaunchOrFocusPlugin { .. }
        | Action::LaunchPlugin { .. }
        | Action::NewTiledPluginPane { .. }
        | Action::NewFloatingPluginPane { .. }
        | Action::NewInPlacePluginPane { .. } => "Plugin Manager",
        Action::Detach => "Detach",
        Action::Quit => "Quit",
        _ => return None,
    };
    Some(label.to_string())
}

// Where a mode switch goes. Leaving a mode is "Back" (that is what Esc does
// from inside one), leaving Locked is "Unlock"; the rename modes get zellij's
// own wording, and everything else names the mode it enters, in the same words
// as the pill on the status bar.
fn switch_label(target: InputMode, current: InputMode) -> String {
    match (current, target) {
        (InputMode::Locked, InputMode::Normal) => "Unlock",
        (_, InputMode::Locked) => "Lock",
        (InputMode::RenameTab, InputMode::Normal) | (InputMode::RenamePane, InputMode::Normal) => {
            "When done"
        }
        // zellij calls the way out of a mode "Select pane" — you go back to using
        // the panes. (It drops the hint entirely when the mode IS the base mode.)
        (current, InputMode::Normal) if current != InputMode::Normal => "Select pane",
        (_, InputMode::RenameTab) => "Rename tab",
        (_, InputMode::RenamePane) => "Rename pane",
        (_, InputMode::EnterSearch) => "Enter search term",
        (_, target) => mode_label(&target),
    }
    .to_string()
}

// A pipe-bind has no action to name it — a foreign plugin's keybind is labelled
// by its pipe name, made readable: "rename_session_start" -> "Rename session start".
fn humanise(name: &str) -> String {
    let spaced = name.replace('_', " ");
    let mut chars = spaced.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => spaced,
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

/// "37%" → 37 (what cpu.sh prints).
fn percent(s: &str) -> Option<u32> {
    s.trim().trim_end_matches('%').parse().ok()
}

/// "20G/67G" → the used share as a percent (what mem.sh prints).
fn used_of_total_percent(s: &str) -> Option<u32> {
    let (used, total) = s.trim().split_once('/')?;
    let number = |x: &str| -> Option<f32> {
        x.trim()
            .trim_end_matches(|c: char| c.is_ascii_alphabetic())
            .parse()
            .ok()
    };
    let (used, total) = (number(used)?, number(total)?);
    if total > 0.0 {
        Some(((used / total) * 100.0).round() as u32)
    } else {
        None
    }
}
