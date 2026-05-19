use bevy::ecs::entity::Entity;
use bevy::ecs::query::With;
use bevy::ecs::system::{Local, NonSendMut, Query};
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, NSObject};
use objc2::{DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send, sel};
use objc2_app_kit::{
    NSColor, NSMenu, NSMenuItem, NSStatusBar, NSStatusItem, NSVariableStatusItemLength, NSWorkspace,
};
use objc2_core_foundation::CGFloat;
use objc2_foundation::{NSObjectProtocol, NSString, NSURL};
use tracing::{error, warn};

use crate::config::CONFIGURATION_FILE;
use crate::ecs::ActiveWorkspaceMarker;
use crate::ecs::layout::LayoutStrip;
use crate::events::{Event, EventSender};

pub struct MenuBarManager {
    mtm: MainThreadMarker,
    status_bar: Retained<NSStatusBar>,
    status_item: Retained<NSStatusItem>,
    _menu: Retained<NSMenu>,
    _menu_target: Retained<MenuBarActionTarget>,
    current_label: Option<String>,
}

const STATUS_ITEM_BACKGROUND_ALPHA: CGFloat = 0.18;
const STATUS_ITEM_CORNER_RADIUS: CGFloat = 5.0;

impl MenuBarManager {
    pub fn new(mtm: MainThreadMarker, events: EventSender) -> Self {
        let status_bar = NSStatusBar::systemStatusBar();
        let status_item = status_bar.statusItemWithLength(NSVariableStatusItemLength);
        status_item.setVisible(true);
        let menu_target = MenuBarActionTarget::new(mtm, events);
        let menu = build_menu(mtm, &menu_target);
        status_item.setMenu(Some(&menu));

        let mut manager = Self {
            mtm,
            status_bar,
            status_item,
            _menu: menu,
            _menu_target: menu_target,
            current_label: None,
        };
        manager.show_virtual_workspace(0);
        manager
    }

    pub fn show_virtual_workspace(&mut self, virtual_index: u32) {
        let label = format_virtual_workspace_label(virtual_index);
        if self.current_label.as_deref() == Some(label.as_str()) {
            return;
        }

        let title = NSString::from_str(&label);
        let tooltip = NSString::from_str("Paneru virtual workspace");
        let Some(button) = self.status_item.button(self.mtm) else {
            warn!("unable to update menu bar: status item has no button");
            return;
        };

        button.setWantsLayer(true);
        if let Some(layer) = button.layer() {
            let background = NSColor::controlAccentColor()
                .colorWithAlphaComponent(STATUS_ITEM_BACKGROUND_ALPHA)
                .CGColor();
            layer.setBackgroundColor(Some(&background));
            layer.setCornerRadius(STATUS_ITEM_CORNER_RADIUS);
            layer.setMasksToBounds(true);
        }
        button.setTitle(&title);
        button.setToolTip(Some(&tooltip));
        self.current_label = Some(label);
    }
}

impl Drop for MenuBarManager {
    fn drop(&mut self) {
        self.status_bar.removeStatusItem(&self.status_item);
    }
}

pub fn update_virtual_workspace_status_item(
    active_workspace: Query<(Entity, &LayoutStrip), With<ActiveWorkspaceMarker>>,
    menu_bar: Option<NonSendMut<MenuBarManager>>,
    mut displayed_workspace: Local<Option<(Entity, u32)>>,
) {
    let Some(mut menu_bar) = menu_bar else {
        return;
    };
    let Some((entity, strip)) = active_workspace.iter().next() else {
        return;
    };

    let next = (entity, strip.virtual_index);
    if displayed_workspace
        .as_ref()
        .is_some_and(|displayed| *displayed == next)
    {
        return;
    }

    menu_bar.show_virtual_workspace(strip.virtual_index);
    *displayed_workspace = Some(next);
}

pub(crate) fn format_virtual_workspace_label(virtual_index: u32) -> String {
    format!("VW {}", virtual_index + 1)
}

#[derive(Debug)]
struct MenuBarActionTargetIvars {
    events: EventSender,
}

define_class!(
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "PaneruMenuBarActionTarget"]
    #[ivars = MenuBarActionTargetIvars]
    #[derive(Debug)]
    struct MenuBarActionTarget;

    unsafe impl NSObjectProtocol for MenuBarActionTarget {}

    impl MenuBarActionTarget {
        #[unsafe(method(openConfig:))]
        fn open_config(&self, _sender: &AnyObject) {
            open_configuration_file();
        }

        #[unsafe(method(quitPaneru:))]
        fn quit_paneru(&self, _sender: &AnyObject) {
            if let Err(err) = self.ivars().events.send(Event::Exit) {
                error!("sending quit event from menu bar: {err}");
            }
        }
    }
);

impl MenuBarActionTarget {
    fn new(mtm: MainThreadMarker, events: EventSender) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(MenuBarActionTargetIvars { events });
        unsafe { msg_send![super(this), init] }
    }
}

fn build_menu(mtm: MainThreadMarker, target: &MenuBarActionTarget) -> Retained<NSMenu> {
    let menu = NSMenu::new(mtm);
    let open_config = menu_item(mtm, "打开配置文件", sel!(openConfig:), target);
    let quit = menu_item(mtm, "退出", sel!(quitPaneru:), target);

    menu.addItem(&open_config);
    menu.addItem(&quit);
    menu
}

fn menu_item(
    mtm: MainThreadMarker,
    title: &str,
    action: objc2::runtime::Sel,
    target: &MenuBarActionTarget,
) -> Retained<NSMenuItem> {
    let title = NSString::from_str(title);
    let key_equivalent = NSString::from_str("");
    let item = unsafe {
        NSMenuItem::initWithTitle_action_keyEquivalent(
            NSMenuItem::alloc(mtm),
            &title,
            Some(action),
            &key_equivalent,
        )
    };
    unsafe {
        item.setTarget(Some(target.as_ref()));
    }
    item
}

fn open_configuration_file() {
    let path = NSString::from_str(&CONFIGURATION_FILE.to_string_lossy());
    let url = NSURL::fileURLWithPath(&path);
    if !NSWorkspace::sharedWorkspace().openURL(&url) {
        warn!(
            "unable to open configuration file '{}'",
            CONFIGURATION_FILE.display()
        );
    }
}

#[cfg(test)]
mod tests {
    use super::format_virtual_workspace_label;

    #[test]
    fn label_is_one_based() {
        assert_eq!(format_virtual_workspace_label(0), "VW 1");
        assert_eq!(format_virtual_workspace_label(4), "VW 5");
    }
}
