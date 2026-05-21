use std::time::Duration;

use bevy::app::{App, Plugin, PostUpdate};
use bevy::ecs::component::Component;
use bevy::ecs::entity::Entity;
use bevy::ecs::lifecycle::{Add, Remove};
use bevy::ecs::observer::On;
use bevy::ecs::query::{Added, Has, With};
use bevy::ecs::resource::Resource;
use bevy::ecs::schedule::IntoScheduleConfigs as _;
use bevy::ecs::system::{Commands, Populated, Query, Res, ResMut, Single};
use bevy::prelude::Event as BevyEvent;
use bevy::time::common_conditions::on_timer;
use tracing::{Level, debug, error, instrument, warn};

use super::{FocusedMarker, MouseHeld, SystemTheme};
use crate::config::Config;
use crate::ecs::layout::LayoutStrip;
use crate::ecs::params::{ActiveDisplay, GlobalState, Windows};
use crate::ecs::{
    ActiveWorkspaceMarker, RetileMarker, SelectedVirtualMarker, SendMessageTrigger,
    StrayFocusEvent, default_floating_subrole, focus_entity, reposition_entity, reshuffle_around,
};
use crate::events::Event;
use crate::manager::{Application, Window, WindowManager};

const REFRESH_WINDOW_CHECK_FREQ_MS: u64 = 1000;
pub struct FocusEventsPlugin;

impl Plugin for FocusEventsPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<CachedTiledFocus>();
        app.add_systems(
            PostUpdate,
            (
                autocenter_window_on_focus.after(super::systems::animate_resize_entities),
                autocenter_stable_retiled_window.after(autocenter_window_on_focus),
                recover_lost_focus.run_if(on_timer(Duration::from_millis(
                    REFRESH_WINDOW_CHECK_FREQ_MS,
                ))),
            ),
        );
        app.add_observer(dim_remove_window_trigger)
            .add_observer(dim_window_trigger)
            .add_observer(maintain_focus_singleton)
            .add_observer(virtual_strip_activated)
            .add_observer(stray_focus_observer)
            .add_observer(focus_window_trigger);
    }
}

#[derive(BevyEvent)]
pub(super) struct FocusWindow {
    pub entity: Entity,
    pub raise: bool,
}

#[derive(Default, Resource)]
struct CachedTiledFocus(Option<Entity>);

#[derive(Component)]
struct SuppressFocusView;

fn cache_tiled_focus(entity: Entity, windows: &Windows, cache: &mut CachedTiledFocus) {
    if is_tiled_focus_candidate(entity, windows) {
        cache.0 = Some(entity);
    }
}

fn cached_tiled_focus_entity(
    windows: &Windows,
    active_strip: &LayoutStrip,
    cache: &mut CachedTiledFocus,
) -> Option<Entity> {
    let entity = cache.0?;
    if active_strip.contains(entity) && is_tiled_focus_candidate(entity, windows) {
        Some(entity)
    } else {
        cache.0 = None;
        None
    }
}

fn is_tiled_focus_candidate(entity: Entity, windows: &Windows) -> bool {
    let Some((window, _, unmanaged)) = windows.get_managed(entity) else {
        return false;
    };
    unmanaged.is_none() && !default_floating_subrole(window)
}

#[allow(clippy::needless_pass_by_value)]
#[instrument(level = Level::DEBUG, skip_all, fields(trigger))]
fn maintain_focus_singleton(
    trigger: On<Add, FocusedMarker>,
    windows: Query<(Entity, Has<FocusedMarker>), With<Window>>,
    all_windows: Windows,
    mut cached_tiled_focus: ResMut<CachedTiledFocus>,
    mut config: GlobalState,
    mut commands: Commands,
) {
    let focused_entity = trigger.event().entity;
    cache_tiled_focus(focused_entity, &all_windows, &mut cached_tiled_focus);

    for (entity, focused) in windows {
        if focused
            && entity != focused_entity
            && let Ok(mut entity_commands) = commands.get_entity(entity)
        {
            debug!("window {entity} lost focus.");
            entity_commands.try_remove::<FocusedMarker>();
        }
    }

    // Check if the reshuffle was caused by a keyboard switch or mouse move.
    // Skip reshuffle if caused by mouse - because then it won't center.
    if config.ffm_flag().is_none() {
        config.set_skip_reshuffle(false);
    }
    config.set_ffm_flag(None);
}

#[allow(clippy::needless_pass_by_value)]
#[instrument(level = Level::DEBUG, skip_all, fields(trigger))]
fn autocenter_window_on_focus(
    focused: Single<(Entity, Option<&SuppressFocusView>), Added<FocusedMarker>>,
    mouse_held: Res<MouseHeld>,
    windows: Windows,
    global_state: GlobalState,
    active_display: ActiveDisplay,
    config: Res<Config>,
    mut commands: Commands,
) {
    let (entity, suppress_focus_view) = *focused;

    if suppress_focus_view.is_some() {
        commands.entity(entity).try_remove::<SuppressFocusView>();
        return;
    }

    if global_state.skip_reshuffle() || global_state.initializing() || mouse_held.is_held() {
        return;
    }
    bring_window_into_focus_view(
        entity,
        FocusViewMode::RespectConfig,
        &windows,
        &active_display,
        &config,
        &mut commands,
    );
}

#[allow(clippy::needless_pass_by_value)]
#[instrument(level = Level::DEBUG, skip_all)]
fn autocenter_stable_retiled_window(
    retiled: Populated<Entity, With<RetileMarker>>,
    windows: Windows,
    active_display: ActiveDisplay,
    config: Res<Config>,
    mut commands: Commands,
) {
    for entity in retiled {
        bring_window_into_focus_view(
            entity,
            FocusViewMode::Center,
            &windows,
            &active_display,
            &config,
            &mut commands,
        );
        commands.entity(entity).try_remove::<RetileMarker>();
    }
}

#[derive(Clone, Copy)]
enum FocusViewMode {
    RespectConfig,
    Center,
}

fn bring_window_into_focus_view(
    entity: Entity,
    mode: FocusViewMode,
    windows: &Windows,
    active_display: &ActiveDisplay,
    config: &Config,
    commands: &mut Commands,
) {
    let should_center = match mode {
        FocusViewMode::RespectConfig => config.auto_center(),
        FocusViewMode::Center => true,
    };

    if should_center
        && let Some((_, _, None)) = windows.get_managed(entity)
        && let Some(size) = windows.size(entity)
        && let Some(layout_position) = windows.layout_position(entity)
    {
        let viewport = active_display.bounds();
        let x = viewport.center().x - layout_position.0.x - size.x / 2;
        reposition_entity(
            active_display.active_strip_entity(),
            viewport.min.with_x(x),
            commands,
        );
        return;
    }
    reshuffle_around(entity, commands);
}

#[allow(clippy::needless_pass_by_value)]
fn dim_window_trigger(
    trigger: On<Add, FocusedMarker>,
    windows: Windows,
    window_manager: Res<WindowManager>,
    config: Res<Config>,
    theme: Option<Res<SystemTheme>>,
) {
    let Some(window) = windows.get(trigger.event().entity) else {
        return;
    };

    let dark = theme.is_some_and(|theme| theme.is_dark);
    if config.window_dim_ratio(dark).is_some() {
        window_manager.dim_windows(&[window.id()], 0.0);
    }
}

#[allow(clippy::needless_pass_by_value)]
fn dim_remove_window_trigger(
    trigger: On<Remove, FocusedMarker>,
    windows: Windows,
    active_display: ActiveDisplay,
    window_manager: Res<WindowManager>,
    config: Res<Config>,
    theme: Option<Res<SystemTheme>>,
) {
    let Some((window, _, None)) = windows.get_managed(trigger.event().entity) else {
        return;
    };

    let same_display = active_display
        .active_strip()
        .contains(trigger.event().entity);
    if !same_display {
        // Do not dim the window loosing focus on another display.
        return;
    }

    let dark = theme.is_some_and(|theme| theme.is_dark);
    if let Some(dim_ratio) = config.window_dim_ratio(dark) {
        window_manager.dim_windows(&[window.id()], dim_ratio);
    }
}

#[allow(clippy::needless_pass_by_value)]
#[instrument(level = Level::DEBUG, skip_all, fields(trigger))]
fn virtual_strip_activated(
    trigger: On<Add, FocusedMarker>,
    workspaces: Query<(Entity, &LayoutStrip, Has<ActiveWorkspaceMarker>)>,
    mut commands: Commands,
) {
    let owner_strip = workspaces.into_iter().find_map(|(entity, strip, active)| {
        (strip.contains(trigger.entity) && !active).then_some(entity)
    });
    if let Some(entity) = owner_strip
        && let Ok(mut entity_commands) = commands.get_entity(entity)
    {
        entity_commands
            .try_insert(ActiveWorkspaceMarker)
            .try_insert(SelectedVirtualMarker);
    }
}

#[allow(clippy::needless_pass_by_value)]
fn focus_window_trigger(trigger: On<FocusWindow>, windows: Windows, apps: Query<&Application>) {
    let FocusWindow { entity, raise } = *trigger.event();
    let Some(window) = windows.get(entity) else {
        return;
    };
    let Some(psn) = windows.psn(window.id(), &apps) else {
        return;
    };
    if !raise
        && let Some((focused_window, _)) = windows.focused()
        && let Some(focused_psn) = windows.psn(focused_window.id(), &apps)
    {
        window.focus_without_raise(psn, focused_window, focused_psn);
    } else {
        window.focus_with_raise(psn);
    }
}

#[allow(clippy::needless_pass_by_value)]
#[instrument(level = Level::DEBUG, skip_all)]
fn recover_lost_focus(
    windows: Windows,
    active_workspace: Query<&LayoutStrip, With<ActiveWorkspaceMarker>>,
    mut cached_tiled_focus: ResMut<CachedTiledFocus>,
    mut commands: Commands,
) {
    if windows.focused().is_some() {
        return;
    }
    error!("Lost focus marker, recovering!");
    if let Ok(strip) = active_workspace
        .single()
        .inspect_err(|err| error!("Unable to get current workspace: {err}"))
    {
        if let Some(entity) = cached_tiled_focus_entity(&windows, strip, &mut cached_tiled_focus) {
            commands.entity(entity).try_insert(SuppressFocusView);
            focus_entity(entity, true, &mut commands);
            return;
        }
        let Some(entity) = strip.first().ok().and_then(|col| col.top()) else {
            return;
        };
        focus_entity(entity, false, &mut commands);
    }
}

#[allow(clippy::needless_pass_by_value)]
pub(super) fn stray_focus_observer(
    trigger: On<Add, Window>,
    focus_events: Populated<(Entity, &StrayFocusEvent)>,
    windows: Windows,
    mut commands: Commands,
) {
    let entity = trigger.event().entity;
    let Some(window_id) = windows.get(entity).map(|window| window.id()) else {
        return;
    };

    focus_events
        .iter()
        .filter(|(_, stray_focus)| stray_focus.0 == window_id)
        .for_each(|(timeout_entity, _)| {
            debug!("Re-queueing lost focus event for window id {window_id}.");
            commands.trigger(SendMessageTrigger(Event::WindowFocused { window_id }));
            commands.entity(timeout_entity).despawn();
        });
}
