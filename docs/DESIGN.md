# termy — Design spec (from reference screenshot)

Reference: 1280×765 screenshot of a multi-session AI-agent terminal workspace.
Background colors below were **sampled from pixels** (region mode); accent
colors are approximate (anti-aliased glyphs/icons).

## Built-in theme "Dark OLED" — chrome palette

| Token                | Hex       | Where                                         |
|----------------------|-----------|-----------------------------------------------|
| `terminal_bg`        | `#000000` | Pane content, active tab, footer              |
| `chrome_bg`          | `#0d0d0d` | Sidebar, title/tab strip, window edge         |
| `input_bg`           | `#080808` | Sidebar search field                          |
| `button_bg`          | `#181818` | "New session" button                          |
| `pill_selected_bg`   | `#202020` | Selected filter pill ("All")                  |
| `border_subtle`      | `#1b1b1b` | Rules, pane separators (1 px)                 |
| `text_primary`       | `#b8b8b8` | Card titles, tab titles                       |
| `text_dim`           | `#606060` | Group headers, subtitles, elapsed time        |
| `diff_del_bg`        | `#350400` | Removed lines (line numbers ≈ `#8a3e40`)      |
| `diff_add_bg`        | `#062400` | Added lines (line numbers ≈ `#5ba061`)        |
| `status_done`        | ≈`#4caf6a`| "Done" label, unread dot on the card icon     |
| `status_working`     | `text_dim`| "Working" label (+ spinner in the pill)       |
| `accent_claude`      | ≈`#d97757`| Claude Code starburst icon                    |
| `accent_warn`        | ≈`#e0a040`| "auto mode on" in the status line             |

Contrast is high: pure black content against near-black chrome, with no
visible borders. Panes are separated by tone, not by lines.

## Layout

```
┌●●● ▢ ──────────┬─ [✳ Active tab ×] Claude Code  Claude Code  midulive  midterm  + ─── ∿ ⊕ ┐
│ ✎ New session ⇧⌘T│📁│                                    │                                │
│ 🔍 Search agents   / │  pane A (terminal, #000)           │  pane B (terminal, #000)       │
│ [All 6] ◷Waiting ◌Working 5                              │                                │
│ bid-model-detector │                                    │                                │
│  ✳• Title      3m  │                                    │                                │
│     Done (green)   │            [Jump to bottom ↓]      │                                │
│ midterm            │ ─────────────────────────────────  │ ─────────────────────────────  │
│  ✳ Claude Code 21m │ ❯ _                                │ ❯ _                            │
│     subtitle/task  │ ─────────────────────────────────  │ ─────────────────────────────  │
│ …                  │ ▸▸ status · model   week ━━─ 15% ↻5d 11h │  (same per pane)          │
│ ✳ ◎ ⟳ ⚗ ◎ ⬡ ▢ ◆   │                                    │                                │
│ 🗄 Archived     19 │                                    │                                │
└────────────────────┴────────────────────────────────────┴────────────────────────────────┘
```

- **Sidebar** ≈ 200 logical px (≈ 16% at 1280). Collapsible with Cmd/Ctrl+B
  via the toggle next to the traffic lights (macOS: transparent titlebar
  + `fullsize_content_view`, chrome drawn by egui).
- **Session card**: 16 px agent icon (unread/done dot on top) · title
  (1 line, ellipsized) · elapsed time right-aligned, dim · second line =
  current task or status (`Done` green, `Working` dim). Grouped under
  lowercase project headers (Git root name).
- **Filter pills**: `All N` · `Waiting` (clock icon) · `Working N` (spinner).
- **Sidebar footer**: row of agent-profile icons (launch a new session with
  that profile) + `Archived  N`.
- **Tab strip**: tabs have icon + title (ellipsized, ~140 px), the active one
  is `#000` blending into its pane, inactive ones `#0d0d0d` with dim text,
  `+` at the end, global actions on the right (activity monitor, …).
  The screenshot shows one strip on top of the left pane. We implement **one
  strip per pane**, all at the same height, so the 50/50 split reads as a
  single continuous strip.
- **"Jump to bottom ↓" pill**: floating at the bottom-center of a pane when
  its viewport is scrolled up in the scrollback. Click → `scroll_to_bottom`.
- **Pane footer** (our own, egui): agent status · model · token/quota bar
  (`week ━━─ 15% ↻ 5d 11h` = usage vs. budget + time to reset).
  ⚠ In the screenshot this line is drawn by Claude Code itself (its status
  line), inside the terminal. Our footer must be compact and toggleable
  (`[window] pane_footer = true`) so it doesn't duplicate the CLI's status line.

## Typography

Monospace ≈ 12–13 pt (logical), tight line height (~1.2). UI chrome uses the
same mono family at ~12 pt, so everything reads as one terminal-native surface.
