# lvim-zbar

A theme-following status bar plugin for [zellij](https://zellij.dev), with inline rename.

## What it shows

```
 v  Split down   h  Split right   +  Fullscreen   c  New tab   ...   (the hints bar, optional)
 mode · session · tabs                                    CPU · MEM · online · clock
```

zellij hands the plugin the **active theme's colours** in every `ModeUpdate` event (which fires on
a theme switch too), so the bar repaints live and follows whatever theme the session runs — there
is no per-theme regeneration of anything, unlike a bar whose colours are baked into the layout at
session start.

## Inline rename

- **Tab rename** is fully native: the `r` keybind (in zellij's tmux mode here) enters the RenameTab
  input mode — zellij core owns the keystrokes, the bar renders the active tab pill as a live edit
  field. The plugin never touches the keyboard for this, so it can never lock it.
- **Session rename** has no native input mode, so the `R` keybind pipes `rename_session_start` to
  the bar; it intercepts keys only while the edit field is open. Enter applies, Esc cancels, and
  both broadcast `rename_close`, which unconditionally clears the intercept in every instance. The
  same `rename_close` is accepted from the CLI as an external emergency unlock:

  ```sh
  zellij pipe --name rename_close
  ```

## Install

With [lvim-zpm](https://github.com/lvim-tech/lvim-zpm), add the repo to `~/.config/zellij/zpm.kdl`:

```kdl
plugins {
    "lvim-tech/lvim-zbar"
}
```

The shipped `zpm.kdl` here tells the manager everything else: it installs the built wasm to
`~/.config/zellij/plugins/lvim-zbar.wasm` (the path the layouts reference) and pre-grants the
permissions. Then put the bar in your layout (`~/.config/zellij/layouts/default.kdl`):

```kdl
layout {
    pane

    pane size=1 borderless=true {
        plugin location="file:~/.config/zellij/plugins/lvim-zbar.wasm"
    }
}
```

Manual install: copy `lvim-zbar.wasm` to `~/.config/zellij/plugins/` yourself and approve the
permission prompt on first load.

Keybinds used above (`r`, `R`) live in your own config; the `R` bind is:

```kdl
bind "R" { MessagePlugin "file:~/.config/zellij/plugins/lvim-zbar.wasm" { name "rename_session_start"; }; SwitchToMode "normal"; }
```

## The hints bar

The same wasm loaded a **second time**, with `bar "hints"` in its plugin configuration, is a bar on
**top** that lists the keybinds of the current mode — like zellij's own status bar, but following
the same theme as the bottom one and toggleable at runtime:

```kdl
layout {
    pane size=1 borderless=true {
        plugin location="file:~/.config/zellij/plugins/lvim-zbar.wasm" {
            bar "hints"
        }
    }

    pane

    pane size=1 borderless=true {
        plugin location="file:~/.config/zellij/plugins/lvim-zbar.wasm"
    }
}
```

The hints come out of `ModeInfo`, which the plugin already receives on every mode and theme change,
so the top bar needs no extra permission and no polling.

### The same hints as zellij, drawn our way

**What it says** is a port of zellij's own one-line status bar. That bar sends a one-row pane to
`one_line_ui()` (`default-plugins/status-bar/src/one_line_ui.rs`), and this one reimplements it:

- **`get_keys_and_hints`** — the curated per-mode table: which hints, in what order, each with a
  long and a short wording. Not "everything we can name".
- **`action_key`** — which key a hint gets, by an *exact* match on the action sequence. This is why
  a hint can be missing: zellij's "Rename tab" wants `SwitchToMode(RenameTab)` **and**
  `TabNameInput`, so `bind "r" { SwitchToMode "renametab"; }` earns no hint — in zellij's bar
  either.
- **the width ladder** — long wordings, then short ones, then as many short ones as fit followed by
  `" ... "`.

**How it looks** is not zellij's. Its hints are powerline ribbons, arrows on both ends, opened by a
separator arrow that divides the mode indicators from the hints. This bar draws no mode indicators,
so that separator would divide nothing; and the boxes are square:

```
 v  Split down   h  Split right   +  Fullscreen   c  New tab   ...
```

The **key** is the theme's yellow — `multiplayer_user_colors.player_8`, which is where the lvim
colorschemes put it — on the bar's own background. The **label** is `ribbon_unselected.base` on
`ribbon_unselected.background`, the shade of the active tab. Both bold, one space between hints.
Accents come from the raw `multiplayer_user_colors` for the same reason the bottom bar's do: the
`text_unselected` slots are contrast-flattened by the theme exporter.

The one mode zellij's table does not cover is **Normal** (and Locked): there its one-line bar shows
*mode indicators* instead of hints. An empty top bar in the mode you spend your time in is useless
and the mode indicators duplicate the bottom bar's mode pill, so those two keep this plugin's own
list — every bind it can name — drawn and cut the same way.

### Toggling it

The toggle is a keybind of your own — the plugin never sees which one, it only knows the pipe:

```kdl
keybinds {
    tmux {
        bind "o" {
            MessagePlugin "file:~/.config/zellij/plugins/lvim-zbar.wasm" { name "hints_toggle"; };
            SwitchToMode "normal";
        }
    }
}
```

**The toggle never appears in the hints.** zellij compiles `MessagePlugin` into a `KeybindPipe`
action, and the bar skips the one named `hints_toggle` — every other plugin keybind (the `R` above,
for instance) is still listed, under its pipe name.

The bind above lives in a mode, so it is reached as a prefix chord (`Ctrl s` then `o` with the
keybinds in this README's config). Any free binding works — a `shared_except "locked"` one too, if
your terminal really delivers it; the pipe name is the only part the plugin knows about.

From the CLI the state is set rather than flipped:

```sh
zellij pipe --name hints_set -- off   # or: on
```

`hints_toggle` is deliberately accepted from a **keybind only**: a CLI pipe ends with an empty
payload — a second delivery that would toggle straight back.

### Why a state and not a flip

The bar is loaded once per tab, and a url-addressed keybind pipe reaches every instance of it. If
the message meant "flip", it would flip once per tab. So each recipient computes the same next
state and broadcasts `hints_set on|off`, which is idempotent: N deliveries do the work of one. A
newly loaded instance (a new tab) asks its siblings with `hints_query` and they answer with the
same `hints_set`, so a new tab opens with the hints bar in the state the rest of the session is in.

Off means an **empty line**, not a hidden pane. `hide_pane_with_id` / `show_pane_with_id` looked
like the tidier answer, but it never handed the row back — the terminal pane below reports the same
height either way — and it did shift the panes around it on the way back. The row was going to stay
reserved regardless, so the bar simply stops drawing.

## The right-side widgets

CPU / MEM / online come from the small POSIX scripts in `scripts/` (the clock is internal), run via
the `RunCommands` permission and refreshed on a timer. Copy them to `~/.config/zellij/scripts/`.
Note: the scripts directory is currently a compile-time constant (`SCRIPTS` in `src/main.rs`) —
adjust it to your `$HOME` before building.

## Building

```sh
cargo build --release --target wasm32-wasip1
```

The built `lvim-zbar.wasm` is shipped in the repo, so building is only needed after changes.

## One instance per tab

A layout plugin is instantiated once per tab — that shapes the rename flows above (the instances
mirror the edit buffer to each other over plugin pipes) and the hints toggle below it, and it is
why the bar must stay a layout plugin: it is a visible pane, not a background service.
