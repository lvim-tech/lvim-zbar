# lvim-zbar

A theme-following status bar plugin for [zellij](https://zellij.dev), with inline rename.

## What it shows

```
 v  Split down   h  Split right   +  Fullscreen   c  New tab   …    (the hints bar, optional)
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
so the top bar needs no extra permission and no polling. Keys that do the same thing share one hint
(`h/j/k/l` `Focus`).

### Same colours, same words as zellij's own bar

The palette is zellij's status bar's, sampled off it: the **key** is `text_unselected.emphasis_2`
straight on the bar, the **label** is a ribbon — `ribbon_unselected.base` on
`ribbon_unselected.background`. The wording is zellij's too (`Split down`, `Previous Tab`,
`Increase/Decrease size`, …), so the two bars read identically. The one deliberate difference: the
boxes are square, with no arrow separators, and there is no mode pill (the bar at the bottom names
the mode).

### When it does not fit

Like zellij's own bar, the line is rendered at more than one level of detail and the fullest one
that fits wins, rather than amputating the only rendering there is:

1. every key that does the job — `h/j/k/l` `Move focus`
2. one key per hint — `h` `Move focus`

The wording never shrinks: `Down` in place of `Split down` reads as a different bar. Width buys
fewer keys and then fewer hints, so if even one key per hint overflows, the tail is cut with a
trailing `…`. What goes first is what matters least — the hints are ordered by `HINT_ORDER`, whose
head is zellij's own tmux-mode line, hint for hint.

Bear in mind zellij's bar shows a *curated* handful per mode while this one shows everything it can
label, so a mode with twenty-odd binds will end in `…` where the native bar does not.

### Toggling it

The toggle is a keybind of your own — the plugin never sees which one, it only knows the pipe:

```kdl
keybinds {
    shared_except "locked" {
        bind "Alt H" {
            MessagePlugin "file:~/.config/zellij/plugins/lvim-zbar.wasm" { name "hints_toggle"; };
        }
    }
}
```

**The toggle never appears in the hints.** zellij compiles `MessagePlugin` into a `KeybindPipe`
action, and the bar skips the one named `hints_toggle` — every other plugin keybind (the `R` above,
for instance) is still listed, under its pipe name.

`Alt H` is only a suggestion; any free binding works, and the pipe name is what matters.

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

Hiding uses `hide_pane_with_id` / `show_pane_with_id(..., should_focus_pane: false)` rather than
`hide_self` / `show_self`: the latter is documented to focus the pane and switch to its tab, and a
bar must never take focus. While toggled off the bar also draws an empty line, so a host that does
not reclaim the row leaves a blank bar rather than a stale one.

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
