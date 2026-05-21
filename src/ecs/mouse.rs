use bevy::app::{App, Plugin, Update};
use bevy::ecs::entity::Entity;
use bevy::ecs::message::MessageReader;
use bevy::ecs::query::With;
use bevy::ecs::schedule::IntoScheduleConfigs as _;
use bevy::ecs::system::{Commands, Local, Query, Res, ResMut, Single};
use std::time::Duration;
use tracing::{debug, trace, warn};

use super::MouseHeld;
use crate::config::Config;
use crate::ecs::layout::LayoutStrip;
use crate::ecs::params::{GlobalState, Windows};
use crate::ecs::{
    ActiveWorkspaceMarker, MissionControlActive, Position, Scrolling, focus_entity,
    reposition_entity, reshuffle_around, resize_entity,
};
use crate::events::Event;
use crate::manager::{Origin, WindowManager, origin_from};
use crate::platform::WinID;

const MOUSE_HELD_TIMEOUT: Duration = Duration::from_secs(60);

pub struct MouseEventsPlugin;

impl Plugin for MouseEventsPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<MouseHeld>();
        let mission_control_inactive = |mission_control: Option<Res<MissionControlActive>>| {
            mission_control.is_none_or(|active| !active.0)
        };

        app.add_systems(
            Update,
            (
                (
                    mouse_moved_trigger,
                    mouse_resize_trigger,
                    mouse_down_trigger,
                )
                    .chain()
                    .run_if(mission_control_inactive),
                mouse_up_trigger.after(mouse_down_trigger),
                mouse_held_timeout,
            ),
        );
    }
}

/// Handles mouse moved events.
///
/// If "focus follows mouse" is enabled, this function finds the window under the cursor and
/// focuses it. It also handles child windows like sheets and drawers to ensure the correct
/// window receives focus.
///
/// # Arguments
///
/// * `trigger` - The Bevy event trigger containing the mouse moved event.
/// * `windows` - A query for all windows.
/// * `focused_window` - A query for the currently focused window.
/// * `main_cid` - The main connection ID resource.
/// * `config` - The optional configuration resource.
#[allow(clippy::needless_pass_by_value)]
fn mouse_moved_trigger(
    mut messages: MessageReader<Event>,
    windows: Windows,
    window_manager: Res<WindowManager>,
    config: Res<Config>,
    mut global_state: GlobalState,
    mut commands: Commands,
) {
    for event in messages.read() {
        let Event::MouseMoved { point, modifiers } = event else {
            continue;
        };

        if config
            .mouse_resize_modifier()
            .is_some_and(|modifier| modifier.matches(*modifiers))
        {
            // Resizing is handled by a separate trigger or logic.
            // For now, let's just intercept it here to prevent focus changes during resize.
            continue;
        }

        if !config.focus_follows_mouse() {
            continue;
        }
        if global_state.ffm_flag().is_some() {
            trace!("ffm_window_id > 0");
            continue;
        }
        let Ok(window_id) = window_manager.find_window_at_point(point) else {
            debug!("can not find window at point {point:?}");
            continue;
        };
        if windows
            .focused()
            .is_some_and(|(window, _)| window.id() == window_id)
        {
            trace!("allready focused {window_id}");
            continue;
        }
        let Some((window, entity)) = windows.find(window_id) else {
            trace!("can not find focused window: {window_id}");
            continue;
        };

        let child_window = window_manager
            .get_associated_windows(window_id)
            .into_iter()
            .find_map(|child_wid| {
                windows.find(child_wid).and_then(|(window, _)| {
                    window
                        .child_role()
                        .inspect_err(|err| {
                            warn!("getting role {window_id}: {err}");
                        })
                        .is_ok_and(|child| child)
                        .then_some(window)
                })
            });
        if let Some(child) = child_window {
            debug!("found child of {}: {}", child.id(), window.id());
        }

        // Do not reshuffle windows due to moved mouse focus.
        global_state.set_skip_reshuffle(true);
        global_state.set_ffm_flag(Some(window.id()));
        focus_entity(entity, false, &mut commands);
    }
}

/// Handles mouse down events.
///
/// This function finds the window at the click point. If the window is not fully visible,
/// it triggers a reshuffle to expose it.
///
/// # Arguments
///
/// * `trigger` - The Bevy event trigger containing the mouse down event.
/// * `windows` - A query for all windows.
/// * `active_display` - A query for the active display.
/// * `main_cid` - The main connection ID resource.
/// * `commands` - Bevy commands to trigger a reshuffle.
#[allow(clippy::needless_pass_by_value, clippy::too_many_arguments)]
fn mouse_down_trigger(
    mut messages: MessageReader<Event>,
    windows: Windows,
    active_workspace: Query<(Entity, Option<&Scrolling>), With<ActiveWorkspaceMarker>>,
    window_manager: Res<WindowManager>,
    config: Res<Config>,
    mut mouse_held: ResMut<MouseHeld>,
    mut commands: Commands,
) {
    for event in messages.read() {
        let Event::MouseDown { point, .. } = event else {
            continue;
        };
        trace!("{point:?}");

        let Some((_, entity)) = window_manager
            .find_window_at_point(point)
            .ok()
            .and_then(|window_id| windows.find(window_id))
        else {
            continue;
        };

        // Stop any ongoing scroll.
        for (entity, scroll) in active_workspace {
            if scroll.is_some() {
                commands.entity(entity).try_remove::<Scrolling>();
            }
        }

        if config.window_hidden_ratio() >= 1.0 {
            // At max hidden ratio, never reshuffle on click.
        } else {
            // Defer reshuffle until mouse-up so the window doesn't shift
            // mid-click. This is a resource, not a spawned component, so a
            // short down/up pair in the same frame still reveals reliably.
            mouse_held.hold(entity);
        }
    }
}

/// Handles mouse-up events. Triggers the deferred reshuffle so the clicked
/// window slides into view after the user releases the button.
#[allow(clippy::needless_pass_by_value)]
fn mouse_up_trigger(
    mut messages: MessageReader<Event>,
    mut mouse_held: ResMut<MouseHeld>,
    mut commands: Commands,
) {
    for event in messages.read() {
        if !matches!(event, Event::MouseUp { .. }) {
            continue;
        }

        if let Some(entity) = mouse_held.take() {
            reshuffle_around(entity, &mut commands);
        }
    }
}

fn mouse_held_timeout(mut mouse_held: ResMut<MouseHeld>) {
    mouse_held.clear_if_expired(MOUSE_HELD_TIMEOUT);
}

#[derive(Default)]
pub(super) struct MouseResizeState {
    last_point: Option<Origin>,
    window_id: Option<WinID>,
}

#[allow(clippy::needless_pass_by_value)]
fn mouse_resize_trigger(
    mut messages: MessageReader<Event>,
    windows: Windows,
    active_workspace: Single<(Entity, &LayoutStrip, &Position), With<ActiveWorkspaceMarker>>,
    window_manager: Res<WindowManager>,
    config: Res<Config>,
    mut state: Local<MouseResizeState>,
    mut commands: Commands,
) {
    for event in messages.read() {
        let Event::MouseMoved { point, modifiers } = event else {
            continue;
        };

        if config
            .mouse_resize_modifier()
            .is_none_or(|modifier| !modifier.matches(*modifiers))
        {
            state.last_point = None;
            state.window_id = None;
            continue;
        }
        let pointer = origin_from(*point);

        let Some(last_point) = state.last_point else {
            state.last_point = Some(pointer);
            continue;
        };
        state.last_point = Some(pointer);

        let dx = (pointer.x - last_point.x) * 5;
        if dx.abs() < 1 {
            continue;
        }

        let Ok(window_id) = window_manager.find_window_at_point(point) else {
            continue;
        };
        if state.window_id.is_some_and(|id| window_id != id) {
            continue;
        }
        state.window_id = Some(window_id);

        let Some((window, entity)) = windows.find(window_id) else {
            continue;
        };
        let (strip_entity, strip, strip_position) = *active_workspace;
        if !strip.contains(entity) {
            continue;
        }

        let mut frame = window.frame();
        let center = frame.center();

        if pointer.x < center.x {
            // Resize Left Edge: increase/decrease width AND shift the strip so the right edge stays
            // anchored.
            let mut origin = strip_position.0;
            origin.x += dx;
            reposition_entity(strip_entity, origin, &mut commands);

            frame.min.x += dx;
        } else {
            frame.max.x += dx;
        }
        resize_entity(entity, frame.size(), &mut commands);
    }
}
