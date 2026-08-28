# lvim-zbar

A theme-following status bar plugin for [zellij](https://zellij.dev), with inline rename.

## What it shows

```
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
mirror the edit buffer to each other over plugin pipes), and it is why the bar must stay a layout
plugin: it is a visible pane, not a background service.
