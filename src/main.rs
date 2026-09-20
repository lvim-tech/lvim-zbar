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
// mode and can be toggled away with a keybind that is itself never drawn.
//
// WHAT it says is a port of zellij's own one-line status bar: the same curated
// per-mode table, the same wording long and short, the same rule for which key a
// hint gets, the same width ladder. See get_keys_and_hints().
//
// HOW it looks is ours: square boxes rather than zellij's powerline ribbons, no
// separator arrow in front (it divides the mode indicators from the hints, and we
// draw no mode indicators), and the key in the theme's yellow.
//
// The toggle has to survive one-instance-per-tab: a url-addressed keybind pipe
// reaches EVERY status instance, so "flip it" would flip N times. What travels
// between the instances is therefore the RESULTING state, never a flip — each
// recipient computes the same next value and broadcasts the idempotent
// "hints_set on|off", which every instance (both roles) applies to its shadow
// of the state. See the pipe() arms.
//
// Off means an EMPTY line, not a hidden pane. hide_pane_with_id never handed the
// row back (the terminal pane's height is the same either way, measured) and it
// did shift the panes around it, so the bar simply stops drawing.

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
    // The fleet of Claude Code agents, as lvim-zclaude counts it: the prose line it broadcasts
    // (`3 agents · 1 busy · 1 waiting`) and whether any of them is waiting for a person. It
    // arrives as the `lvim-zclaude-summary` pipe; nothing is polled and nothing is read from
    // disk, so a machine without that plugin simply never shows the field.
    claude: String,
    claude_waiting: bool,
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
    // Where each tab landed on the last paint, so a click can be turned back
    // into a tab. Written by render, read by the click handler.
    tab_hits: Vec<TabHit>,
}

/// One tab's columns on the drawn bar: `start..end`, and which tab they are.
///
/// Columns, not bytes: the bar is one ANSI string in which a tab's label is
/// surrounded by zero-width escapes, and a click arrives as a column number.
struct TabHit {
    start: usize,
    /// One past the last column the tab occupies.
    end: usize,
    /// `TabInfo::position` — 0-based, unlike `switch_tab_to`.
    position: usize,
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
        let mut events = vec![
            EventType::ModeUpdate,
            EventType::TabUpdate,
            EventType::Timer,
            EventType::RunCommandResult,
            EventType::PermissionRequestResult,
            EventType::InterceptedKeyPress,
        ];
        // Only the bar that draws tabs has anything to click. The subscription
        // is narrowed rather than filtered in the handler because Mouse covers
        // Hover too, which fires on every pointer movement across the pane: the
        // hints bar would be woken for nothing, once per mouse move.
        if self.role == Role::Status {
            events.push(EventType::Mouse);
        }
        subscribe(&events);
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
            // A click on a tab switches to it. Only LeftClick is answered: the
            // rest of the Mouse family — Hover above all — arrives for every
            // pointer movement over the bar and must cost nothing.
            //
            // No repaint from here. Switching produces a TabUpdate, which is
            // what redraws the bar; saying "render" now would paint the old
            // active tab a second time.
            Event::Mouse(Mouse::LeftClick(line, col)) => {
                self.click_tab(line, col);
                false
            }
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
            // The fleet of agents, broadcast by lvim-zclaude whenever its count changes (it sends
            // `zellij pipe --name lvim-zclaude-summary` with two lines: the prose for a bar, and
            // `agents=N busy=N waiting=N done=N idle=N` for anything that would rather parse).
            // The bar is TOLD; it never polls, never reads `~/.claude`, and knows nothing when the
            // board is not running — which is exactly what an absent field should mean.
            "lvim-zclaude-summary" => {
                if self.role != Role::Status {
                    return false;
                }
                let Some(text) = message.payload else { return false };
                let prose = text.lines().next().unwrap_or("").trim().to_string();
                let pairs = text.lines().nth(1).unwrap_or("");
                let waiting = field_count(pairs, "waiting") > 0 || field_count(pairs, "done") > 0;
                // "0 agents" is nothing to show: the field disappears rather than reading zero.
                let prose = if prose.starts_with("0 agent") { String::new() } else { prose };
                let changed = prose != self.claude || waiting != self.claude_waiting;
                if changed {
                    eprintln!("lvim-zbar: claude {} (waiting {})", prose, waiting);
                }
                self.claude = prose;
                self.claude_waiting = waiting;
                changed
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
                // Only a repaint: the pane stays where the layout put it. Hiding
                // it with hide_pane_with_id/show_pane_with_id never gave the row
                // back (measured: the terminal pane's height does not change) and
                // did disturb the geometry around it, so what "off" means here is
                // an empty line.
                self.role == Role::Hints
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
    /// Turn a click on the bar into a tab switch.
    ///
    /// The columns come from the last paint, which is the only thing that knows
    /// where a tab ended up: names change length, a rename turns one into an
    /// edit field, and the notice zone moves nothing but sits right after them.
    ///
    /// Deliberately silent for anything that is not squarely a tab. A status bar
    /// is not a place where a stray click should do something surprising, and
    /// there is no visual feedback here to explain what it did.
    fn click_tab(&self, line: isize, col: usize) {
        // The bar draws a single row, so anything below it is empty space that
        // happens to belong to the pane.
        if line != 0 {
            return;
        }
        // Mid-rename the bar is showing an edit field. zellij's RenameTab mode
        // edits whichever tab is active, so switching underneath it would leave
        // the rename pointing at a tab the user never meant to rename — and the
        // session rename has the keyboard intercepted on top of that. Neither is
        // worth a click that looks like navigation.
        if self.session_edit.is_some() || self.mode_info.mode == InputMode::RenameTab {
            return;
        }
        let Some(hit) = self
            .tab_hits
            .iter()
            .find(|hit| col >= hit.start && col < hit.end)
        else {
            return;
        };
        // `switch_tab_to` counts from 1 (it takes the number in the default tab
        // names); `TabInfo::position` counts from 0.
        switch_tab_to(hit.position as u32 + 1);
    }

    // The bar at the bottom: mode pill · session pill · tabs · notice, then the
    // widgets on the right.
    fn render_status(&mut self, cols: usize) {
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
        // Where each tab lands, in columns, so a click can find it again.
        // Measured off the string being drawn, with the same `visible_width`
        // that computes this row's padding — so the hit zone and the paint
        // agree by construction rather than by two calculations happening to
        // match. Collected locally: `self.tabs` is borrowed by the loop.
        let mut hits: Vec<TabHit> = Vec::with_capacity(self.tabs.len());
        for tab in &self.tabs {
            let start = visible_width(&left);
            if tab.active && tab_editing {
                // Native RenameTab mode: zellij core edits the name and streams
                // it here via TabUpdate — render it as an inline edit field
                // (raw name: it IS the edit buffer), with a cursor. Until our
                // normalising rename above lands, an auto name would flash as
                // "Tab #N": show the display form for that brief moment.
                left.push_str(&pill(&format!(" {}_ ", tab_display_name(tab)), acc_fg, edit_bg));
                left.push_str(&fmt(bar_fg, bar_bg));
                hits.push(TabHit {
                    start,
                    end: visible_width(&left),
                    position: tab.position,
                });
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
            hits.push(TabHit {
                start,
                end: visible_width(&left),
                position: tab.position,
            });
        }
        self.tab_hits = hits;

        // ── the notice zone: a transient system message next to the tabs ────
        // Plain coloured text (the theme's cyan) on the bar background — a pill would shout; a
        // notice only speaks.
        if let Some(notice) = &self.notice {
            left.push_str(&fmt(teal, bar_bg));
            left.push_str(&format!("  {} ", notice));
            left.push_str(&fmt(bar_fg, bar_bg));
        }

        // ── right: agents · CPU · MEM · online · clock ──────────────────────
        let mut right = String::new();
        // The agents lead the right side: a machine's load is background, but an agent waiting
        // for an answer is the one thing on this bar that is waiting for THIS person. Red is
        // that wait; otherwise the field is as quiet as CPU.
        if !self.claude.is_empty() {
            right.push_str(&fmt(if self.claude_waiting { red } else { teal }, bar_bg));
            right.push_str(&format!(" ✳ {} ", self.claude));
        }
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
    // The hints bar draws what zellij's own one-line bar draws, through zellij's
    // own UI components: the key is serialized text, the label a ribbon, and the
    // HOST turns both into arrows, colours and bold with the very code that
    // paints its native bar. Nothing here knows the theme.
    fn render_hints(&self, cols: usize) {
        // Exactly zellij's fill: set the bar background and erase to the end of
        // the line, so the row is uniform however long the content came out.
        let fill = match self.mode_info.style.colors.text_unselected.background {
            PaletteColor::Rgb((r, g, b)) => format!("\u{1b}[48;2;{};{};{}m\u{1b}[0K", r, g, b),
            PaletteColor::EightBit(n) => format!("\u{1b}[48;5;{}m\u{1b}[0K", n),
        };
        if self.hints_hidden {
            print!("{}", fill);
            return;
        }

        let mut line = String::new();
        let mut width = 0usize;
        // zellij opens the line with add_keygroup_separator(): an orange arrow
        // dividing the mode indicators from the hints. We do not draw the mode
        // indicators, so the arrow would divide nothing — only the word for what
        // is being typed survives, as a box like every other.
        let (help, help_len) = mode_help_box(&self.mode_info);
        if help_len <= cols {
            line.push_str(&help);
            width += help_len;
        }

        let hints = get_keys_and_hints(&self.mode_info);
        // zellij's table is empty for Normal and Locked — there its one-line bar
        // shows mode indicators instead of hints. An empty bar in the mode you
        // live in would be useless, so those two keep our own derived list; the
        // rendering and the width ladder are the native ones either way.
        let hints = if hints.is_empty() { derived_hints(&self.mode_info) } else { hints };
        let (body, _) = keybinds(&self.mode_info, &hints, cols.saturating_sub(width));
        line.push_str(&body);
        print!("{}{}", line, fill);
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

// ─── the native hint line, ported ───────────────────────────────────────────
//
// zellij's status-bar plugin sends a one-row pane to one_line_ui(), NOT to the
// two-line first_line/second_line pair. Everything from here to derived_hints()
// is a port of that module (default-plugins/status-bar/src/one_line_ui.rs), so
// the two bars agree hint for hint, word for word and colour for colour:
//
//   get_keys_and_hints   the curated per-mode table — WHICH hints, in what
//                        order, with a long and a short wording for each
//   action_key & co.     which key each hint gets: an EXACT match on the
//                        action sequence, so a bind that does not do precisely
//                        what the table asks for shows no hint at all
//   keybinds             the width ladder: long names, short names, then as
//                        many short ones as fit followed by " ... "
//   add_shortcut         one hint = serialized key text + a ribbon
//
// The one segment deliberately left out is render_mode_key_indicators(), the
// left-hand list of MODE names: it is empty in Tmux mode anyway and the status
// bar below already carries the mode.

/// What zellij appends when the hints do not fit — its own MORE_MSG, spaces and
/// all, not an ellipsis.
const MORE_MSG: &str = " ... ";
/// Shorthand for the action every mode uses to get back out.
const TO_NORMAL: Action = Action::SwitchToMode { input_mode: InputMode::Normal };

/// Every key bound to EXACTLY this sequence of actions.
///
/// Exact is the operative word, and it is why the native bar hides hints one
/// might expect: its "Rename tab" wants `SwitchToMode(RenameTab)` followed by
/// `TabNameInput`, so a bind that only switches the mode matches nothing and
/// the hint is dropped. We drop it too.
fn action_key(keymap: &[(KeyWithModifier, Vec<Action>)], action: &[Action]) -> Vec<KeyWithModifier> {
    keymap
        .iter()
        .filter_map(|(key, acvec)| {
            let matching = acvec.iter().zip(action).filter(|(a, b)| a.shallow_eq(b)).count();
            (matching == acvec.len() && matching == action.len()).then(|| key.clone())
        })
        .collect()
}

/// The keys for several action sequences at once — how one hint comes to carry
/// a whole group, as in `h|j|k|l` for "Move focus".
fn action_key_group(
    keymap: &[(KeyWithModifier, Vec<Action>)],
    actions: &[&[Action]],
) -> Vec<KeyWithModifier> {
    actions.iter().flat_map(|action| action_key(keymap, action)).collect()
}

/// The first key whose FIRST action is this one — looser than action_key, and
/// used where zellij wants one key rather than the whole matching set.
fn single_action_key(
    keymap: &[(KeyWithModifier, Vec<Action>)],
    action: &[Action],
) -> Vec<KeyWithModifier> {
    keymap
        .iter()
        .find(|(_, acvec)| acvec.first() == action.first())
        .map(|(key, _)| vec![key.clone()])
        .unwrap_or_default()
}

/// The first key that launches the named plugin (the Session mode entries).
fn plugin_key(keymap: &[(KeyWithModifier, Vec<Action>)], url: &str) -> Vec<KeyWithModifier> {
    keymap
        .iter()
        .find(|(_, acvec)| acvec.iter().any(|a| a.launches_plugin(url)))
        .map(|(key, _)| vec![key.clone()])
        .unwrap_or_default()
}

/// The current mode's binds, sorted by key and deduplicated by what they DO, so
/// that when several keys run the same actions the first one wins. zellij does
/// this before consulting the table; the order of the surviving keys is what
/// decides which key each hint ends up showing.
fn deduped_keymap(mode_info: &ModeInfo) -> Vec<(KeyWithModifier, Vec<Action>)> {
    let mut keymap = mode_info.get_mode_keybinds();
    keymap.sort_by(|(a, _), (b, _)| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let mut seen: Vec<Vec<Action>> = Vec::new();
    let mut out = Vec::new();
    for (key, actions) in keymap {
        if seen.contains(&actions) {
            continue;
        }
        seen.push(actions.clone());
        out.push((key, actions));
    }
    out
}

/// The way back to the base mode, preferring Enter — computed BEFORE the dedupe
/// above, exactly as zellij does, because the dedupe would otherwise collapse
/// the several keys that all just leave the mode.
fn to_base_mode_key(mode_info: &ModeInfo) -> Vec<KeyWithModifier> {
    let keymap = mode_info.get_mode_keybinds();
    let target = mode_info.base_mode.unwrap_or(InputMode::Normal);
    let keys = action_key(&keymap, &[Action::SwitchToMode { input_mode: target }]);
    if keys.contains(&KeyWithModifier::new(BareKey::Enter)) {
        vec![KeyWithModifier::new(BareKey::Enter)]
    } else {
        keys.into_iter().take(1).collect()
    }
}

/// A NewTab action to match against. Every field is ignored — Action::shallow_eq
/// treats any two NewTab as equal — so the values here are only ballast.
fn any_new_tab() -> Action {
    Action::NewTab {
        tiled_layout: None,
        floating_layouts: vec![],
        swap_tiled_layouts: None,
        swap_floating_layouts: None,
        tab_name: None,
        should_change_focus_to_new_tab: true,
        cwd: None,
        initial_panes: None,
        first_pane_unblock_condition: None,
    }
}

/// zellij's own table: for the current mode, which hints to show, in which
/// order, with the long and the short wording, and the keys that earn each one.
/// Empty for Normal and Locked — see render_hints().
#[rustfmt::skip]
fn get_keys_and_hints(mi: &ModeInfo) -> Vec<(String, String, Vec<KeyWithModifier>)> {
    use Action as A;
    use Direction as Dir;
    use InputMode as IM;

    let km = deduped_keymap(mi);
    let to_base = to_base_mode_key(mi);
    let s = |text: &str| text.to_string();

    if mi.mode == IM::Pane { vec![
        (s("New"), s("New"), single_action_key(&km, &[A::NewPane { direction: None, pane_name: None, start_suppressed: false }, TO_NORMAL])),
        (s("Change Focus"), s("Move"), action_key_group(&km, &[
            &[A::MoveFocus { direction: Dir::Left }], &[A::MoveFocus { direction: Dir::Down }],
            &[A::MoveFocus { direction: Dir::Up }], &[A::MoveFocus { direction: Dir::Right }]])),
        (s("Close"), s("Close"), single_action_key(&km, &[A::CloseFocus, TO_NORMAL])),
        (s("Rename"), s("Rename"), single_action_key(&km, &[A::SwitchToMode { input_mode: IM::RenamePane }, A::PaneNameInput { input: vec![0] }])),
        (s("Toggle Fullscreen"), s("Fullscreen"), single_action_key(&km, &[A::ToggleFocusFullscreen, TO_NORMAL])),
        (s("Toggle Floating"), s("Floating"), single_action_key(&km, &[A::ToggleFloatingPanes, TO_NORMAL])),
        (s("Toggle Embed"), s("Embed"), single_action_key(&km, &[A::TogglePaneEmbedOrFloating, TO_NORMAL])),
        (s("Split Right"), s("Right"), single_action_key(&km, &[A::NewPane { direction: Some(Dir::Right), pane_name: None, start_suppressed: false }, TO_NORMAL])),
        (s("Split Down"), s("Down"), single_action_key(&km, &[A::NewPane { direction: Some(Dir::Down), pane_name: None, start_suppressed: false }, TO_NORMAL])),
        (s("Stack"), s("Stack"), single_action_key(&km, &[A::NewStackedPane { command: None, pane_name: None, near_current_pane: false, tab_id: None }, TO_NORMAL])),
        (s("Select pane"), s("Select"), to_base),
    ]} else if mi.mode == IM::Tab {
        // The default bindings put all four arrows on tab focus, and the dedupe
        // above would leave Left and Down. zellij special-cases it back to the
        // pair a reader expects; so do we.
        let raw = mi.get_mode_keybinds();
        let full = action_key_group(&raw, &[&[A::GoToPreviousTab], &[A::GoToNextTab]]);
        let focus_keys = if full.contains(&KeyWithModifier::new(BareKey::Left))
            && full.contains(&KeyWithModifier::new(BareKey::Right)) {
            vec![KeyWithModifier::new(BareKey::Left), KeyWithModifier::new(BareKey::Right)]
        } else {
            action_key_group(&km, &[&[A::GoToPreviousTab], &[A::GoToNextTab]])
        };
        vec![
        (s("New"), s("New"), single_action_key(&km, &[any_new_tab(), TO_NORMAL])),
        (s("Change focus"), s("Move"), focus_keys),
        (s("Close"), s("Close"), single_action_key(&km, &[A::CloseTab, TO_NORMAL])),
        (s("Rename"), s("Rename"), single_action_key(&km, &[A::SwitchToMode { input_mode: IM::RenameTab }, A::TabNameInput { input: vec![0] }])),
        (s("Sync"), s("Sync"), single_action_key(&km, &[A::ToggleActiveSyncTab, TO_NORMAL])),
        (s("Break pane to new tab"), s("Break out"), single_action_key(&km, &[A::BreakPane, TO_NORMAL])),
        (s("Break pane left/right"), s("Break"), action_key_group(&km, &[
            &[A::BreakPaneLeft, TO_NORMAL], &[A::BreakPaneRight, TO_NORMAL]])),
        (s("Toggle"), s("Toggle"), single_action_key(&km, &[A::ToggleTab])),
        (s("Select pane"), s("Select"), to_base),
    ]} else if mi.mode == IM::Resize { vec![
        (s("Increase/Decrease size"), s("Increase/Decrease"), action_key_group(&km, &[
            &[A::Resize { resize: Resize::Increase, direction: None }],
            &[A::Resize { resize: Resize::Decrease, direction: None }]])),
        (s("Increase to"), s("Increase"), action_key_group(&km, &[
            &[A::Resize { resize: Resize::Increase, direction: Some(Dir::Left) }],
            &[A::Resize { resize: Resize::Increase, direction: Some(Dir::Down) }],
            &[A::Resize { resize: Resize::Increase, direction: Some(Dir::Up) }],
            &[A::Resize { resize: Resize::Increase, direction: Some(Dir::Right) }]])),
        (s("Decrease from"), s("Decrease"), action_key_group(&km, &[
            &[A::Resize { resize: Resize::Decrease, direction: Some(Dir::Left) }],
            &[A::Resize { resize: Resize::Decrease, direction: Some(Dir::Down) }],
            &[A::Resize { resize: Resize::Decrease, direction: Some(Dir::Up) }],
            &[A::Resize { resize: Resize::Decrease, direction: Some(Dir::Right) }]])),
        (s("Select pane"), s("Select"), to_base),
    ]} else if mi.mode == IM::Move { vec![
        (s("Switch Location"), s("Move"), action_key_group(&km, &[
            &[A::MovePane { direction: Some(Dir::Left) }], &[A::MovePane { direction: Some(Dir::Down) }],
            &[A::MovePane { direction: Some(Dir::Up) }], &[A::MovePane { direction: Some(Dir::Right) }]])),
        (s("When done"), s("Back"), to_base),
    ]} else if mi.mode == IM::Scroll { vec![
        (s("Enter search term"), s("Search"), action_key(&km, &[A::SwitchToMode { input_mode: IM::EnterSearch }, A::SearchInput { input: vec![0] }])),
        (s("Scroll"), s("Scroll"), action_key_group(&km, &[
            &[A::ScrollDown], &[A::ScrollUp],
            &[A::PageScrollDown], &[A::PageScrollUp],
            &[A::HalfPageScrollDown], &[A::HalfPageScrollUp]])),
        (s("Edit scrollback in default editor"), s("Edit"), single_action_key(&km, &[A::EditScrollback { ansi: false }, TO_NORMAL])),
        (s("Select pane"), s("Select"), to_base),
        // zellij's table ends with a "Scroll commands" entry for
        // ScrollToPreviousPrompt/ScrollToNextPrompt; zellij-tile 0.44.3 has no
        // such actions, so we cannot express it. It only shows when those are
        // bound, and then it would come last anyway.
    ]} else if mi.mode == IM::EnterSearch { vec![
        (s("When done"), s("Done"), action_key(&km, &[A::SwitchToMode { input_mode: IM::Search }])),
        (s("Cancel"), s("Cancel"), action_key(&km, &[A::SearchInput { input: vec![27] }, A::SwitchToMode { input_mode: IM::Scroll }])),
    ]} else if mi.mode == IM::Search { vec![
        (s("Enter Search term"), s("Search"), action_key(&km, &[A::SwitchToMode { input_mode: IM::EnterSearch }, A::SearchInput { input: vec![0] }])),
        (s("Scroll"), s("Scroll"), action_key_group(&km, &[&[A::ScrollDown], &[A::ScrollUp]])),
        (s("Scroll page"), s("Scroll"), action_key_group(&km, &[&[A::PageScrollDown], &[A::PageScrollUp]])),
        (s("Scroll half page"), s("Scroll"), action_key_group(&km, &[&[A::HalfPageScrollDown], &[A::HalfPageScrollUp]])),
        (s("Search down"), s("Down"), action_key(&km, &[A::Search { direction: SearchDirection::Down }])),
        (s("Search up"), s("Up"), action_key(&km, &[A::Search { direction: SearchDirection::Up }])),
        (s("Case sensitive"), s("Case"), action_key(&km, &[A::SearchToggleOption { option: SearchOption::CaseSensitivity }])),
        (s("Wrap"), s("Wrap"), action_key(&km, &[A::SearchToggleOption { option: SearchOption::Wrap }])),
        (s("Whole words"), s("Whole"), action_key(&km, &[A::SearchToggleOption { option: SearchOption::WholeWord }])),
    ]} else if mi.mode == IM::Session { vec![
        (s("Detach"), s("Detach"), action_key(&km, &[A::Detach])),
        (s("Session Manager"), s("Manager"), plugin_key(&km, "session-manager")),
        (s("Share"), s("Share"), plugin_key(&km, "zellij:share")),
        (s("Configure"), s("Config"), plugin_key(&km, "configuration")),
        (s("Layout Manager"), s("Layouts"), plugin_key(&km, "zellij:layout-manager")),
        (s("Plugin Manager"), s("Plugins"), plugin_key(&km, "plugin-manager")),
        (s("About"), s("About"), plugin_key(&km, "zellij:about")),
        (s("Select pane"), s("Select"), to_base),
    ]} else if mi.mode == IM::Tmux { vec![
        (s("Move focus"), s("Move"), action_key_group(&km, &[
            &[A::MoveFocus { direction: Dir::Left }], &[A::MoveFocus { direction: Dir::Down }],
            &[A::MoveFocus { direction: Dir::Up }], &[A::MoveFocus { direction: Dir::Right }]])),
        (s("Split down"), s("Down"), action_key(&km, &[A::NewPane { direction: Some(Dir::Down), pane_name: None, start_suppressed: false }, TO_NORMAL])),
        (s("Split right"), s("Right"), action_key(&km, &[A::NewPane { direction: Some(Dir::Right), pane_name: None, start_suppressed: false }, TO_NORMAL])),
        (s("Fullscreen"), s("Fullscreen"), action_key(&km, &[A::ToggleFocusFullscreen, TO_NORMAL])),
        (s("New tab"), s("New"), action_key(&km, &[any_new_tab(), TO_NORMAL])),
        (s("Rename tab"), s("Rename"), action_key(&km, &[A::SwitchToMode { input_mode: IM::RenameTab }, A::TabNameInput { input: vec![0] }])),
        (s("Previous Tab"), s("Previous"), action_key(&km, &[A::GoToPreviousTab, TO_NORMAL])),
        (s("Next Tab"), s("Next"), action_key(&km, &[A::GoToNextTab, TO_NORMAL])),
        (s("Select pane"), s("Select"), to_base),
    ]} else if matches!(mi.mode, IM::RenamePane | IM::RenameTab) { vec![
        (s("When done"), s("Done"), to_base),
    ]} else { vec![] }
}

/// Normal and Locked, where zellij's table is empty: every bind we can name,
/// grouped by what it does, in the order the configuration lists them. The
/// wording does not shrink here — there is no second, shorter name to fall back
/// to — so the ladder below goes straight to cutting.
fn derived_hints(mode_info: &ModeInfo) -> Vec<(String, String, Vec<KeyWithModifier>)> {
    let mut hints: Vec<(String, String, Vec<KeyWithModifier>)> = Vec::new();
    for (key, actions) in mode_info.get_mode_keybinds() {
        let Some(label) = action_label(&actions, mode_info.mode) else {
            continue;
        };
        match hints.iter_mut().find(|(long, _, _)| *long == label) {
            Some((_, _, keys)) => {
                if !keys.contains(&key) {
                    keys.push(key);
                }
            }
            None => hints.push((label.clone(), label, vec![key])),
        }
    }
    hints
}

/// The three rungs zellij climbs down when the line will not fit: every hint
/// under its full name, then every hint under its short name, then as many
/// short ones as fit with " ... " standing in for the rest.
fn keybinds(
    mode_info: &ModeInfo,
    hints: &[(String, String, Vec<KeyWithModifier>)],
    max_width: usize,
) -> (String, usize) {
    let full = shortcut_list(mode_info, hints, false);
    if full.1 <= max_width {
        return full;
    }
    let short = shortcut_list(mode_info, hints, true);
    if short.1 <= max_width {
        return short;
    }
    best_effort_shortcut_list(mode_info, hints, max_width)
}

fn shortcut_list(
    mode_info: &ModeInfo,
    hints: &[(String, String, Vec<KeyWithModifier>)],
    short: bool,
) -> (String, usize) {
    let mut part = String::new();
    let mut len = 0;
    for (long, brief, keys) in hints {
        let (piece, piece_len) = add_shortcut(mode_info, if short { brief } else { long }, keys);
        part.push_str(&piece);
        len += piece_len;
    }
    (part, len)
}

fn best_effort_shortcut_list(
    mode_info: &ModeInfo,
    hints: &[(String, String, Vec<KeyWithModifier>)],
    max_len: usize,
) -> (String, usize) {
    let mut part = String::new();
    let mut len = 0;
    for (_, brief, keys) in hints {
        let (piece, piece_len) = add_shortcut(mode_info, brief, keys);
        if len + piece_len + MORE_MSG.chars().count() > max_len {
            part.push_str(MORE_MSG);
            len += MORE_MSG.chars().count();
            break;
        }
        part.push_str(&piece);
        len += piece_len;
    }
    (part, len)
}

/// One hint: a KEY box in the theme's yellow, then — square edge to square edge
/// — a box naming what the key does, then a gap.
///
/// zellij draws these as ribbons, which arrive with powerline arrows on both
/// ends. We paint them by hand instead: straight edges, and the key in
/// `multiplayer_user_colors.player_8`, which is where the lvim colorschemes put
/// their yellow (see mode_colour() on why the accents come from the RAW palette
/// and not from the contrast-flattened text_unselected fields).
fn add_shortcut(mode_info: &ModeInfo, text: &str, keys: &[KeyWithModifier]) -> (String, usize) {
    if keys.is_empty() {
        return (String::new(), 0);
    }
    let colors = mode_info.style.colors;
    let bar_bg = colors.text_unselected.background;
    let (key_text, key_len) = key_label(keys);

    let mut part = pill(&format!(" {} ", key_text), colors.multiplayer_user_colors.player_8, bar_bg);
    part.push_str(&pill(
        &format!(" {} ", text),
        colors.ribbon_unselected.base,
        colors.ribbon_unselected.background,
    ));
    part.push_str(&fmt(colors.text_unselected.base, bar_bg));
    part.push(' ');
    // both paddings, both boxes and the gap after them
    (part, key_len + text.chars().count() + 5)
}

/// The keys of one hint, in zellij's own wording: `h|j|k|l` normally, `hjkl`
/// (and the other run-on groups) without separators, and a shared modifier
/// lifted out in front as `Ctrl <p|n>`.
fn key_label(keys: &[KeyWithModifier]) -> (String, usize) {
    if keys.is_empty() {
        return (String::new(), 0);
    }
    let common = common_modifiers(keys);
    let no_common_modifier = common.is_empty();
    let modifier_str = common.iter().map(|m| m.to_string()).collect::<Vec<_>>().join("-");

    let rendered: Vec<String> = keys
        .iter()
        .map(|key| {
            if no_common_modifier || keys.len() == 1 {
                key.to_string()
            } else {
                key.strip_common_modifiers(&common).to_string()
            }
        })
        .collect();

    // The groups zellij runs together, because "hjkl" reads as one thing and
    // "h|j|k|l" does not.
    let separator = match rendered.join("").as_str() {
        "HJKL" | "hjkl" | "←↓↑→" | "←→" | "↓↑" | "[]" => "",
        _ => "|",
    };

    let text = if no_common_modifier || rendered.len() == 1 {
        rendered.join(separator)
    } else {
        format!("{} <{}>", modifier_str, rendered.join(separator))
    };
    let len = text.chars().count();
    (text, len)
}

/// The modifiers every key in the group shares — what gets lifted out in front.
fn common_modifiers(keys: &[KeyWithModifier]) -> Vec<KeyModifier> {
    let Some((last, rest)) = keys.split_last() else {
        return vec![];
    };
    let mut common = last.key_modifiers.clone();
    for key in rest {
        common = common.intersection(&key.key_modifiers).cloned().collect();
    }
    common.into_iter().collect()
}

/// The word for what is being typed, in the modes that swallow your typing —
/// zellij carries it on the front of its keygroup separator; we draw it as a box
/// like the rest, since the separator itself is gone.
fn mode_help_box(mode_info: &ModeInfo) -> (String, usize) {
    let text = match mode_info.mode {
        InputMode::RenamePane => "RENAMING PANE",
        InputMode::RenameTab => "RENAMING TAB",
        InputMode::EnterSearch => "ENTERING SEARCH TERM",
        InputMode::Search => "SEARCHING",
        _ => return (String::new(), 0),
    };
    let colors = mode_info.style.colors;
    let mut part = pill(
        &format!(" {} ", text),
        colors.text_unselected.emphasis_0,
        colors.text_unselected.background,
    );
    part.push(' ');
    (part, text.chars().count() + 3)
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

/// `agents=3 busy=1 waiting=1 done=0 idle=1` → the number after one of those names, or 0.
///
/// A line of `key=value` pairs rather than JSON, so this stays a `split_whitespace` and the bar
/// carries no parser. An unknown name, a missing line or a value that is not a number all read as
/// nothing to show — a broadcast from a newer lvim-zclaude must never blank the bar.
fn field_count(pairs: &str, name: &str) -> u32 {
    pairs
        .split_whitespace()
        .filter_map(|pair| pair.split_once('='))
        .find(|(key, _)| *key == name)
        .and_then(|(_, value)| value.parse().ok())
        .unwrap_or(0)
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
