# Paneru Architecture

This document records the current architecture of Paneru as observed from the
codebase, with notes on places where the structure can be simplified,
deduplicated, or made faster. It is intentionally implementation-oriented: the
goal is to make future changes less error-prone, especially around focus,
retile, scrolling, and macOS event ordering.

## 1. Runtime Shape

Paneru is a macOS window manager built around Bevy ECS. The daemon process owns
one Bevy app, receives events from macOS and IPC clients, mutates ECS state, then
commits resulting window position/size changes back through AppKit,
Accessibility, and SkyLight wrappers.

The high-level loop is:

```text
macOS callbacks / IPC / config watcher
  -> EventSender channel
  -> ecs::systems::pump_events
  -> Bevy Event messages
  -> commands, triggers, and ECS systems
  -> marker components such as RepositionMarker / ResizeMarker / ReshuffleAroundMarker
  -> PostUpdate animation systems
  -> WindowApi::reposition / WindowApi::resize
```

Main entry points:

- `src/main.rs`
  - Defines CLI subcommands: `launch`, service commands, `send-cmd`, `query`,
    and `subscribe`.
  - Starts `CommandReader` for IPC.
  - Creates the `EventSender` channel and calls `ecs::setup_bevy_app`.
- `src/ecs.rs`
  - Defines shared marker/components.
  - Registers core schedules and observers.
  - Builds the Bevy app with manager/config/platform resources.
- `src/events.rs`
  - Defines the central `Event` enum used by platform callbacks, IPC, and ECS.

## 2. Major Subsystems

### 2.1 Platform and macOS Bridge

The code intentionally isolates AppKit, Accessibility, CoreGraphics, and
SkyLight access behind wrapper types.

Important files:

- `src/platform/input.rs`
  - Owns the `CGEventTap`.
  - Translates raw keyboard, mouse, scroll, and gesture events into Paneru
    `Event`s.
  - Handles keybinding interception and per-focused-window passthrough keys via
    `FOCUSED_PASSTHROUGH`.
- `src/platform/notify.rs`, `src/platform/workspace.rs`,
  `src/platform/mission_control.rs`
  - Bridge lower-level workspace and system notifications into `Event`s.
- `src/manager.rs`
  - Defines `WindowManagerApi`, the boundary between ECS logic and macOS.
  - `WindowManagerOS` implements display discovery, space discovery, window
    hit-testing, workspace contents, config watching, quit, cursor position,
    and dimming.
- `src/manager/app.rs`
  - Wraps application-level AX observation and focused-window tracking.
- `src/manager/windows.rs`
  - Wraps window-level AX operations: id, role/subrole, title, frame,
    reposition, resize, focus, padding, fullscreen state, etc.
- `src/manager/display.rs`
  - Normalizes display bounds and visible viewport with menubar/dock/padding.

Design constraint:

- All AppKit/CoreGraphics calls must remain on the main thread or inside the
  existing platform wrappers that already enforce that assumption.
- ECS systems should call `WindowManager`, `Window`, `Application`, and
  `Display` abstractions, not raw `objc2` or `accessibility_sys`.

### 2.2 Bevy ECS Core

Paneru uses ECS as the source of truth for process/app/window/display/workspace
state. The key components live in `src/ecs.rs`.

Important entity types:

- Process entity
  - `BProcess`
  - `ExistingMarker` or `FreshMarker` during startup/lifecycle handling
- Application entity
  - `Application`
  - child of process
- Window entity
  - `Window`
  - child of application
  - `Position`, `Bounds`, `LayoutPosition`, `WidthRatio`
  - optional state markers such as `FocusedMarker`, `Unmanaged`,
    `FullWidthMarker`, `NativeFullscreenMarker`, `RetileMarker`
- Display entity
  - `Display`
  - `ActiveDisplayMarker`
  - optional `DockPosition`
- Workspace / virtual workspace entity
  - `LayoutStrip`
  - `Position`
  - `ActiveWorkspaceMarker`
  - `SelectedVirtualMarker`
  - optional `Scrolling`, `PreviousStripPosition`

Important marker components:

- `FocusedMarker`
  - Exactly one focused window is maintained by `focus::maintain_focus_singleton`.
- `Unmanaged`
  - `Floating`, `Minimized`, or `Hidden`.
  - Floating windows are removed from `LayoutStrip` and excluded from tiling.
- `RepositionMarker`
  - Request to animate/update a position.
- `ResizeMarker`
  - Request to animate/update size.
- `ReshuffleAroundMarker`
  - Request to move a strip so a given window is visible.
- `RetileMarker`
  - Tracks a floating window being explicitly returned to the layout.
  - Stores `previous_center_x`, the screen-space x center before re-entering the
    tiled strip.
- `Scrolling`
  - Tracks in-progress strip scrolling/swiping, velocity, and target position.

Custom `SystemParam`s in `src/ecs/params.rs` are important for keeping systems
readable:

- `Windows`
  - Central window query helper.
  - Provides `focused`, `find`, `find_parent`, `get_managed`, `moving_frame`,
    `layout_position`, etc.
- `ActiveDisplay`
  - Immutable active display/strip access.
- `ActiveDisplayMut`
  - Mutable active display/strip access.
- `GlobalState`
  - Focus-follows-mouse and skip-reshuffle flags.

## 3. Scheduling Model

Core system registration is in `src/ecs.rs`.

### Startup

```text
gather_displays
gather_initial_processes
```

This creates display/workspace entities and loads initial process/config data.

### PreUpdate

```text
dispatch_toplevel_triggers
pump_events
command systems
workspace bind systems
```

`pump_events` drains the external MPSC channel into Bevy messages. Command
systems read `Event::Command` and mutate ECS state.

### Update

Main lifecycle and reactive systems:

- Add existing/launched processes and apps.
- Finish initial setup once windows are loaded.
- Watch display/workspace changes.
- Process focus, app, window, config, and mission-control events.
- Layout plugin recalculates logical positions when strips or sizes change.
- Scroll plugin integrates swipe/scroll movement.
- Workspace plugin handles virtual workspaces and native space changes.

### PostUpdate

```text
animate_entities -> commit_window_position
animate_resize_entities -> commit_window_size
update_overlays
update_flash_messages
menubar status item update
focus autocenter/recover systems
```

`RepositionMarker` and `ResizeMarker` are animated into `Position` and `Bounds`;
then committed through the `Window` wrapper.

## 4. Event and Command Flow

### External Input

`platform/input.rs` transforms low-level events into:

- `Event::MouseMoved`, `MouseDown`, `MouseDragged`, `MouseUp`
- `Event::Swipe`, `Scroll`, `TouchpadDown`, `TouchpadUp`
- `Event::VerticalSwipe`, `VerticalScrollTick`
- `Event::Command { command }` for matching keybindings

### IPC

`reader.rs` uses `/tmp/paneru.socket`.

Supported request types:

- `send-cmd`: parsed by `config::parse_command`, emitted as `Event::Command`.
- `query`: creates `Event::StateQuery` with a response channel.
- `subscribe`: creates `Event::StateSubscribe` with a cloned stream.

### Command Handling

`src/commands.rs` owns the user-facing command model:

- `Command`
- `Operation`
- `Direction`
- `MoveFocus`
- `ResizeDirection`

Registered command systems include:

- focus, swap, stack/unstack
- center, snap, resize, fullwidth
- move to display
- virtual workspace switching/moving
- manage/floating toggle
- print state and quit

`manage_window` has special focus handling:

- It first asks the frontmost app for the real AX focused window.
- It falls back to ECS `FocusedMarker`.
- This avoids acting on the tiled window underneath a focused floating window.

## 5. Layout Model

The layout core is `src/ecs/layout.rs`.

### `LayoutStrip`

A `LayoutStrip` is a horizontal strip of columns for one native workspace and
one virtual workspace index.

Columns are:

- `Single(Entity)`
- `Stack(Vec<StackItem>)`
- `Tabs(Vec<Entity>)`
- `Fullscreen(Entity)`

Stack items are:

- `Single(Entity)`
- `Tabs(Vec<Entity>)`

The strip stores logical order only. It does not directly move AppKit windows.
Movement is derived through ECS layout systems.

### Logical Layout Pipeline

1. `LayoutStrip` changes when a window is appended, inserted, removed, swapped,
   stacked, or restored.
2. `layout_strip_changed` recalculates each window's `LayoutPosition` and
   `Bounds`.
3. `position_layout_strips` marks contained window layout positions as changed
   when the strip moves.
4. `position_layout_windows` combines:
   - window `LayoutPosition`
   - strip `Position`
   - display viewport
   - sliver rules
   - stack/tabs/fullwidth state
   and writes final window `Position`/`Bounds`.
5. `animate_entities` and `animate_resize_entities` move `Position`/`Bounds`
   toward requested markers.
6. `commit_window_position` and `commit_window_size` call `WindowApi`.

### Reshuffling

`ReshuffleAroundMarker` is used to move the strip so a target window is visible.

`reshuffle_layout_strip`:

- Finds the strip containing the marked window.
- Uses `windows.moving_frame(entity)` and the display viewport.
- Computes the strip offset needed to expose the window.
- Skips movement if `window_hidden_ratio` allows the current hidden fraction.

This is separate from direct `window center`, which repositions the strip around
the target layout position.

## 6. Focus Model

Focus is represented by `FocusedMarker`.

Important paths:

- macOS focus event:
  - `Event::WindowFocused`
  - `triggers::window_focused_trigger`
  - validates frontmost app/current AX focus
  - inserts `FocusedMarker`
- internal focus request:
  - `focus_entity(entity, raise, commands)`
  - inserts `FocusedMarker`
  - triggers `focus::FocusWindow`
  - calls `Window::focus_with_raise` or `focus_without_raise`
- singleton maintenance:
  - `focus::maintain_focus_singleton`
  - removes `FocusedMarker` from other windows

`focus::autocenter_window_on_focus` is the current “make focus visible” hook. It
runs on `Added<FocusedMarker>`, respects skip flags and mouse-held state, and
delegates to `bring_window_into_focus_view`.

Floating-window close handoff should be conservative. The runtime should not
keep a floating-window stack. A single tiled-focus cache may live in
`ecs/focus.rs`, but it must be updated only from low-frequency ECS focus marker
changes and consumed only as a fallback for lost focus recovery. It must not hook
into every `AXFocusedUIElementChanged` notification from terminal/text-entry
apps, because those events can fire while typing and should not move or re-raise
neighboring tiled windows.

Retiling a focused floating window is a special case because `FocusedMarker` is
already present and therefore not “Added” again. The current approach uses
`RetileMarker` so `autocenter_retiled_window_after_layout` can call the same
focus-view logic after the layout systems have updated tiled position/width.

## 7. Floating, Retiling, and Unmanaged State

`Unmanaged` is overloaded for three states:

- `Floating`
- `Minimized`
- `Hidden`

### Floating

When a window becomes floating:

- `window_unmanaged_trigger` handles `On<Add, Unmanaged>`.
- If the variant is `Floating`, it may resize/reposition the window according to
  config/grid/default floating behavior.
- It removes the entity from all `LayoutStrip`s.

### Returning to Tiled

When a floating window is toggled back to tiled:

1. `manage_window` records `RetileMarker { previous_center_x }`.
2. `Unmanaged` is removed.
3. `window_managed_trigger` handles `On<Remove, Unmanaged>`.
4. If window config defines `index`, that wins.
5. Otherwise, retile insertion is based on the saved floating screen center:
   the window is inserted before the first visible tiled column whose screen
   center is to its right.
6. No immediate generic reshuffle is triggered for retile, because that would
   use stale floating geometry.
7. Focus post-update logic consumes `RetileMarker` after layout has produced a
   tiled `LayoutPosition` and `Bounds`.

This area is behaviorally sensitive. Regressions tend to show up as strip
position jumps, off-screen slivers, or drawer-like overlap when the retiled
window is inserted at the wrong logical index.

## 8. Scrolling and Swipe Model

`src/ecs/scroll.rs` manages the PaperWM/Niri-like scrollable strip behavior.

Input sources:

- Trackpad gestures -> `Event::Swipe`
- Scroll wheel with modifier -> `Event::Scroll`
- Touch lifecycle -> `TouchpadDown` / `TouchpadUp`
- Vertical swipe/scroll -> virtual workspace switching

State:

- `Scrolling { velocity, position, is_user_swiping, last_event }` on the active
  `LayoutStrip` entity.

Pipeline:

```text
swipe_gesture
  -> updates Scrolling.position and Scrolling.velocity
apply_inertia
  -> decays velocity after user releases
apply_snap_force
  -> magnetic center snap when auto_center is enabled
scrolling_integrator
  -> integrates velocity into position
apply_scrolling_constraints
  -> clamps position to legal strip bounds
swiping_timeout
  -> removes Scrolling when movement stops
```

Optimization note: any programmatic strip movement that wants to behave like
scrolling should ideally go through `Scrolling` and `apply_scrolling_constraints`
or share its clamp logic. Direct `reposition_entity(strip_entity, ...)` can bypass
scroll constraints and cause invalid viewport positions.

## 9. Workspace and Virtual Workspace Model

`src/ecs/workspace.rs` maps Paneru virtual workspaces onto native macOS spaces.

Concepts:

- Native workspace / Space ID is the `LayoutStrip::id`.
- Virtual workspace is `LayoutStrip::virtual_index`.
- One strip per native workspace + virtual index.
- `ActiveWorkspaceMarker` means the native space/display is currently active.
- `SelectedVirtualMarker` means which virtual strip is selected for that native
  workspace.
- `PreviousStripPosition` preserves scroll position and focus when switching
  virtual strips.

Key paths:

- `workspace_change_trigger`
  - reacts to native `SpaceChanged`
  - activates matching strip
  - handles native fullscreen spaces
- `detect_moved_windows`
  - discovers windows moved by macOS into an active workspace but not present in
    any local strip
- virtual bind systems
  - switch/move windows between virtual strips
- orphan handling
  - rescues windows when display/workspace entities disappear

## 10. State Persistence and Query

`src/ecs/state.rs` has two related responsibilities:

1. Persisting/restoring layout state.
2. Producing query/subscription state documents.

Persistence:

- State file path: `/tmp/paneru-state.json`.
- `PaneruState::extract` serializes workspace/strip/column/window layout.
- `restore_window_state` in `triggers.rs` reorders strips after startup.
- Matching primarily uses window id, pid, and bundle id, with additional
  heuristic fields stored for future matching.

Query:

- `PaneruQueryState` gives active display/workspace/focus plus virtual workspace
  window lists.
- `commands/query.rs` registers query handlers and subscription notification.

Optimization note: persistence currently writes to `/tmp`; that makes state
volatile across reboot and can be surprising for a layout manager. If permanence
is desired, this should move to an XDG/state directory.

## 11. Configuration

`src/config.rs` handles:

- config discovery
- TOML parsing
- deprecated option detection
- keybinding parsing
- command parsing
- window rule matching
- persistence of per-bundle floating rules
- accessors for defaults and migrated sections

Submodules:

- `config/padding.rs`
- `config/decorations.rs`
- `config/swipe.rs`

Configuration is stored behind `ArcSwap`, which allows input callback code to
read current config without locking. `refresh_configuration_trigger` reloads the
config and updates display/window-dependent behavior.

Keybinding names map to commands by splitting binding table keys on `_`.
Examples:

- `window_focus_west`
- `window_swap_east`
- `window_virtualnum_3`
- `quit`

## 12. Overlay and Menubar

`src/overlay.rs` creates AppKit overlay windows for:

- dimming inactive windows
- drawing focused-window border
- flash/status messages

`src/menubar.rs` updates the macOS menu bar status item with workspace state.

Overlay update is scheduled after position/size animation so it can use the most
recent window geometry.

## 13. Testing Structure

Tests live under `src/tests/` with a mock Bevy world.

Important files:

- `harness.rs`
  - builds a Bevy app with mock manager, process, app, and windows.
- `mocks.rs`
  - mock `WindowManagerApi`, `ApplicationApi`, `WindowApi`, `ProcessApi`.
- `interaction.rs`
  - focus, scrolling, stale focus, hidden ratio, floating interactions.
- `tiling.rs`
  - stack/center/focus/resize layout behavior.
- `display.rs`
  - multi-display movement/active display behavior.
- `state.rs`
  - persistence/query state behavior.

## 14. Current Architectural Risks

### 14.1 Focus and Visibility Are Still Too Coupled to Timing

Focus visibility currently depends on schedule ordering:

- focus marker added
- resize/layout systems update
- focus post-update autocenter/reshuffle
- animation and commit

Retile had to add `RetileMarker` because `FocusedMarker` was already present and
did not re-trigger `Added<FocusedMarker>`.

Potential improvement:

- Introduce an explicit `EnsureVisible { entity, mode }` component or event.
- Have exactly one system turn that into strip scrolling/repositioning after
  layout is up to date.
- Use it for focus changes, center command, retile, and possibly virtual
  workspace switches.
- Keep focus history in the focus layer. Do not reuse workspace restoration
  components such as `PreviousStripPosition` for floating-window focus fallback.

### 14.2 Programmatic Strip Movement Has Multiple Paths

Current movement paths include:

- `ReshuffleAroundMarker`
- command center directly repositioning the strip
- scroll gesture through `Scrolling`
- focus autocenter

These can disagree about clamp rules and timing.

Potential improvement:

- Centralize strip target calculation and clamping.
- Make “move strip so entity is visible/centered” a shared pure function.
- Reuse `scroll::clamp_viewport_offset` or expose a higher-level API rather than
  bypassing it with raw `reposition_entity(strip_entity, ...)`.

### 14.3 `Unmanaged` Overloads Independent States

`Unmanaged` has variants for `Floating`, `Minimized`, and `Hidden`. That is easy
to query but semantically overloaded:

- Floating is user/layout state.
- Minimized is OS visibility state.
- Hidden is app visibility state.

Risks:

- Adding/removing `Unmanaged` triggers broad observers even when only one
  variant is relevant.
- A window cannot be both floating and hidden/minimized without losing one state.

Potential improvement:

- Split into separate components:
  - `Floating`
  - `Minimized`
  - `Hidden`
- Or use one state component plus separate persistent user preference for
  floating.

### 14.4 Layout Uses Fresh `Vec` Allocation in Hot Paths

Remaining examples:

- `LayoutStrip::all_windows() -> Vec<Entity>` is still useful for restore/query
  paths, but should not be used in animation/layout hot paths.
- `binpack_heights` still allocates a height vector per stacked column.

Recently addressed:

- `LayoutStrip::all_columns() -> Vec<Entity>` was replaced in hot paths with
  `column_tops()` to avoid collecting top-level column entities just to iterate
  over them.
- `relative_positions` now borrows column/stack/tab items instead of cloning
  `StackItem` and tab vectors, and expands tabs directly into the output frame
  buffer instead of allocating a small `Vec` per stack item.

Potential improvement:

- Add iterator-returning APIs where possible.
- Replace repeated `Vec` allocation in layout/scroll hot paths with iterators or
  small preallocated buffers.
- Keep allocation-based helpers for tests/debug only if convenient.

### 14.5 Query Helpers Are Linear Scans

Many layout methods still scan strip/window query results. This is probably fine
for dozens of windows, but the same searches appear in many systems.

If profiling shows lookup cost matters, an index should be added only after the
ownership and invalidation rules are explicit.

Potential improvement:

- Add a `WinID -> Entity` index maintained from spawn/despawn/change observers.
- Add strip-local indexes for `Entity -> column index` if layout mutation/search
  becomes measurable.
- Use the index in lower-level systems that still manually scan window queries
  for `WindowMoved` and `WindowResized`.

### 14.6 State Restore Focus Ordering Is Fragile

Startup currently:

1. discovers windows
2. focuses a first active strip window in `finish_setup`
3. triggers `RestoreWindowState`
4. restore reorders strips and then corrects focus

This is better than before but still split across two lifecycle phases.

Potential improvement:

- Move initial focus decision after restore entirely.
- Let `finish_setup` only build strips and trigger restore.
- Let restore or a final initialization system choose focus exactly once.

### 14.7 `commands.rs` Is Large and Mixed Responsibility

`commands.rs` owns:

- command data model
- parser-facing operation enums
- command systems
- layout mutation helpers
- state printing/debug output
- focus/retile special handling

Potential improvement:

- Split command systems by concern:
  - `commands/focus.rs`
  - `commands/layout.rs`
  - `commands/workspace.rs`
  - `commands/manage.rs`
  - `commands/debug.rs`
- Keep `Command`, `Operation`, and parsing-facing enums in `commands/mod.rs`.

### 14.8 Some Names and Comments Are Stale or Misspelled

Examples:

- Several doc comments describe older trigger-based APIs or stale arguments.
- Some comments use “pane”, “strip”, “panel”, and “column” interchangeably.

Potential improvement:

- Standardize terms:
  - strip: horizontal virtual workspace strip
  - column: horizontal layout slot
  - stack: vertical group inside a column
  - tabs: native tab group sharing a frame
- Prune argument-heavy doc comments on private systems; keep high-signal module
  and function comments.

### 14.9 Direct Config Persistence During Commands

`manage_window` persists floating rules to the config file and immediately
reloads config.

Risks:

- Command path has file I/O.
- Config watcher may also observe the same edit.
- Persisting per-bundle rules as side effect of a keybinding may surprise users.

Potential improvement:

- Put config persistence behind a dedicated event/command.
- Batch config writes.
- Optionally separate “temporary toggle floating” from “persist floating rule”.

### 14.10 IPC Protocol Is Simple but Rigid

`reader.rs` uses a length-prefixed NUL-separated argv protocol.

Potential improvement:

- Keep current protocol for `send-cmd`.
- Add JSON request/response support for query/subscribe if more clients appear.
- Consider per-client timeout/error response instead of logging only.

## 15. Low-Risk Cleanup Candidates

These are relatively contained changes.

- Rename `StableRetileMarker` references if any remain; the current marker should
  consistently be `RetileMarker`.
- Remove stale doc comments on private systems where signatures no longer match.
- Move print-state debug formatting out of `commands.rs`.
- Consolidate “center strip on entity” and “reshuffle around entity” into one
  focus/viewport module.
- Replace `/tmp/paneru-state.json` and `/tmp/paneru.socket` with XDG runtime/state
  paths, with `/tmp` fallback if needed.

## 16. Higher-Risk Refactors

These need more tests before changing.

- Split `Unmanaged` into separate components.
- Move initial focus selection entirely after state restore.
- Make all programmatic strip movement go through `Scrolling` and shared clamp
  logic.
- Replace linear window lookups with indexed resources.
- Refactor `commands.rs` into focused submodules.
- Separate layout calculation from ECS mutation into pure functions so retile,
  insert, stack, and restore cases can be unit-tested without Bevy scheduling.

## 17. Practical Mental Model for Future Changes

When changing behavior, identify which layer owns the decision:

- macOS fact: `manager/*`, `platform/*`
- event translation: `events.rs`, `platform/input.rs`, `reader.rs`
- user command: `commands.rs`
- state mutation: ECS systems/triggers
- layout order: `LayoutStrip`
- logical layout: `LayoutPosition`/`Bounds`
- viewport/strip movement: `Position` on `LayoutStrip`, `Scrolling`,
  `ReshuffleAroundMarker`
- physical commit: `commit_window_position`, `commit_window_size`

Avoid mixing layers. In particular:

- Do not calculate layout order from final AppKit frame after layout markers have
  been applied unless that is explicitly intended.
- Do not directly move AppKit windows from commands; write ECS markers or state.
- Do not add new focus visibility behavior without deciding whether it should
  share `ReshuffleAroundMarker`, command center, or scroll constraints.
- Do not rely on `Added<FocusedMarker>` for cases where focus is already on the
  window and only its managed/floating state changes.
