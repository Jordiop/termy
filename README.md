# termy

A native, GPU-rendered terminal workspace for AI-agent workflows: multiple
sessions grouped by project, tabs and splits, and a **100% local** token
monitor for Claude Code, Codex and Gemini CLI. Zero telemetry: there isn't a
single networking crate in the dependency tree.

Runs on macOS (Metal), Windows (DirectX 12 + ConPTY) and Linux (Vulkan, Wayland/X11).

## Build

Requires Rust ≥ 1.95.

```sh
cargo run --release
```

## Shortcuts (defaults)

| Action | macOS | Windows / Linux |
|---|---|---|
| New session | ⌘T | Ctrl+Shift+T |
| Split vertically / horizontally | ⌘D / ⇧⌘D | Ctrl+Shift+D / Ctrl+Shift+E |
| Close tab | ⌘W | Ctrl+Shift+W |
| Next / previous tab | ⇧⌘] / ⇧⌘[ | Ctrl+Tab / Ctrl+Shift+Tab |
| Next pane | ⌘] | Ctrl+Shift+] |
| Toggle sidebar | ⌘B | Ctrl+Shift+B |
| Search sessions | ⌘K | Ctrl+Shift+K |
| Command palette | ⇧⌘P | Ctrl+Shift+P |
| Settings | ⌘, | Ctrl+, |
| Usage dashboard | ⇧⌘U | Ctrl+Shift+U |
| Copy / paste | ⌘C / ⌘V | Ctrl+Shift+C / Ctrl+Shift+V |
| Font size + / − / reset | ⌘= / ⌘- / ⌘0 | Ctrl+= / Ctrl+- / Ctrl+0 |
| Reload config | ⇧⌘R | Ctrl+Shift+R |
| Previous / next prompt | ⌘↑ / ⌘↓ | Ctrl+Shift+↑ / Ctrl+Shift+↓ |
| Copy last command output | ⇧⌘O | Ctrl+Shift+O |

On Windows/Linux, letter shortcuts use Ctrl+Shift so plain Ctrl+letter keeps
reaching the shell. Everything is rebindable in Settings → Keybindings (press
the new combination) or in `[keybindings]`.

**Command palette**: every action, new agent sessions, open and archived
sessions and themes in one list. Type to fuzzy-filter, ↑/↓ and Enter to run,
Escape to close; commands you ran recently come first.

**Tabs**: double-click a tab (or a sidebar card) to rename it; Enter saves,
Escape cancels, an empty name restores the automatic one. Right-click for
Rename / Reset name / Close.

**Shell integration** (zsh): termy loads a small script into zsh sessions
that marks each prompt (OSC 133) and reports the working directory (OSC 7).
Prompts get a mark in the left margin, red when the command failed; you can
jump between prompts and copy the last command's output, and new tabs open in
the directory you `cd`'d to. Your own `.zshenv`/`.zshrc` load as usual. Other
shells that emit OSC 133 themselves get the same features. Turn it off with
`shell_integration = false` in `[terminal]`.

**Session state**: each session is *Working* (recent output), *Waiting*
(quiet) or *Needs you*: the program rang the bell, sent a notification
(OSC 9 / 777, whose message becomes the card's title), or Claude Code ended
its turn. It stays there until you type in the session; the sidebar's ⓘ pill
lists them. Claude Code only sends notifications to terminals it knows, so
set `"preferredNotifChannel": "iterm2"` in its settings to also get its
permission prompts.

**Images**: paste copies an image as a PNG in the temp folder and pastes its
path, which Claude Code and Codex attach as an image. Copied files paste their
paths, and so do files dropped on the window. On Windows/Linux plain Ctrl+V
does the same when the clipboard holds an image or files (with text it is
still sent to the program as ^V).

## Configuration

`~/.config/termy/config.toml` (or `$XDG_CONFIG_HOME/termy`, or
`$TERMY_CONFIG_DIR`) is created on first run. Changes apply instantly, whether
you edit the file or use the settings panel. Saving from the panel keeps your
comments and formatting.

* `[window]` opacity, padding, decorations, initial size, sidebar, pane footer,
  tabs: `tab_style` (`rounded` / `pill` / `flat` / `underline`), `tab_radius`,
  `tab_max_width`
* `[font]` family, size, line height, ligatures
* `[cursor]` shape (`block` / `beam` / `underline`), blink
* `[terminal]` shell and args, working directory, scrollback, scroll speed,
  `option_as_alt` (macOS), `shell_integration`, extra environment variables
* `[appearance]` `theme`: one of `dark-oled`, `ghostty-dark`, `catppuccin-mocha`,
  `tokyo-night`, `gruvbox-dark`, `solarized-light`, or your own
  `~/.config/termy/themes/<name>.toml` (same format as `assets/themes/`)
* `[monitor]` and `[[agents]]`: agent profiles and token budgets
* `[resources]`: per-tab CPU / RAM / disk I/O in the pane footer (`enabled`,
  `show_cpu`, `show_memory`, `show_disk`, `interval_ms`). It counts the tab's
  whole process tree, so an agent's subprocesses are included.

```toml
[[agents]]
name = "Claude Code"
command = "claude"
icon = "claude"
color = "#d97757"
metrics = { kind = "claude_code", path = "~/.claude/projects" }
budget = { weekly_tokens = 5000000, weekly_reset = "mon 09:00" }  # UTC; omit for rolling 7 days
```

Metrics sources: `claude_code`, `codex`, `gemini_cli`, `jsonl` (any log, with
fields mapped via JSON pointers) and `command` (a local program that prints
`{"used": N, "limit": N, "resets_at": "…"}`).

### Usage dashboard

Ctrl+Shift+U (⇧⌘U), or the chart button next to Settings, opens a **Usage**
tab with tokens, tasks and agent time over 7 / 30 / 90 days or all history,
tokens by day, an hour-of-week heatmap, and per-model and per-agent tables
(click an agent to see only its usage). A *task* is one agent session and a
*turn* one model call; agent time merges each session's calls into active
spans (gaps over 5 min end a span). Older logs are read on demand the first
time a longer range is picked.

## Privacy

The token monitor only reads files the agents already write on your machine.
termy never opens a network connection.

## Development

```sh
cargo test     # 113 tests: config, themes, PTY, VT emulation, prompt marks, session state, command palette, token parsing, usage stats, layout, input
cargo clippy --all-targets
```

`TERMY_CAPTURE=frame.bmp` saves a rendered frame after
`TERMY_CAPTURE_DELAY_MS` (default 2000) and exits. `TERMY_DEMO_INPUT` types into
the first session and `TERMY_DEMO_ACTIONS=split_vertical,toggle_settings` runs
keybinding actions at startup. These are useful for checking rendering without
screen-recording permissions.

Design notes and the sampled palette: [`docs/DESIGN.md`](docs/DESIGN.md).

## License

Termy is licensed under the [PolyForm Noncommercial License 1.0.0](LICENSE.md). You may use, modify and share it for any noncommercial purpose. Commercial use is not permitted without a separate license from the author.
