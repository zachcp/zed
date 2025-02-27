#[cfg(test)]
mod tab_switcher_tests;

use collections::HashMap;
use editor::items::entry_git_aware_label_color;
use gpui::{
    actions, impl_actions, rems, Action, AnyElement, AppContext, DismissEvent, EntityId,
    EventEmitter, FocusHandle, FocusableView, Model, Modifiers, ModifiersChangedEvent, MouseButton,
    MouseUpEvent, ParentElement, Render, Styled, Task, View, ViewContext, VisualContext, WeakView,
};
use picker::{Picker, PickerDelegate};
use project::Project;
use schemars::JsonSchema;
use serde::Deserialize;
use settings::Settings;
use std::sync::Arc;
use ui::{prelude::*, ListItem, ListItemSpacing, Tooltip};
use util::ResultExt;
use workspace::{
    item::{ItemHandle, ItemSettings, TabContentParams},
    pane::{render_item_indicator, tab_details, Event as PaneEvent},
    ModalView, Pane, SaveIntent, Workspace,
};

const PANEL_WIDTH_REMS: f32 = 28.;

#[derive(PartialEq, Clone, Deserialize, JsonSchema, Default)]
pub struct Toggle {
    #[serde(default)]
    pub select_last: bool,
}

impl_actions!(tab_switcher, [Toggle]);
actions!(tab_switcher, [CloseSelectedItem]);

pub struct TabSwitcher {
    picker: View<Picker<TabSwitcherDelegate>>,
    init_modifiers: Option<Modifiers>,
}

impl ModalView for TabSwitcher {}

pub fn init(cx: &mut AppContext) {
    cx.observe_new_views(TabSwitcher::register).detach();
}

impl TabSwitcher {
    fn register(workspace: &mut Workspace, _: &mut ViewContext<Workspace>) {
        workspace.register_action(|workspace, action: &Toggle, cx| {
            let Some(tab_switcher) = workspace.active_modal::<Self>(cx) else {
                Self::open(action, workspace, cx);
                return;
            };

            tab_switcher.update(cx, |tab_switcher, cx| {
                tab_switcher
                    .picker
                    .update(cx, |picker, cx| picker.cycle_selection(cx))
            });
        });
    }

    fn open(action: &Toggle, workspace: &mut Workspace, cx: &mut ViewContext<Workspace>) {
        let mut weak_pane = workspace.active_pane().downgrade();
        for dock in [
            workspace.left_dock(),
            workspace.bottom_dock(),
            workspace.right_dock(),
        ] {
            dock.update(cx, |this, cx| {
                let Some(panel) = this
                    .active_panel()
                    .filter(|panel| panel.focus_handle(cx).contains_focused(cx))
                else {
                    return;
                };
                if let Some(pane) = panel.pane(cx) {
                    weak_pane = pane.downgrade();
                }
            })
        }

        let project = workspace.project().clone();
        workspace.toggle_modal(cx, |cx| {
            let delegate =
                TabSwitcherDelegate::new(project, action, cx.view().downgrade(), weak_pane, cx);
            TabSwitcher::new(delegate, cx)
        });
    }

    fn new(delegate: TabSwitcherDelegate, cx: &mut ViewContext<Self>) -> Self {
        Self {
            picker: cx.new_view(|cx| Picker::nonsearchable_uniform_list(delegate, cx)),
            init_modifiers: cx.modifiers().modified().then_some(cx.modifiers()),
        }
    }

    fn handle_modifiers_changed(
        &mut self,
        event: &ModifiersChangedEvent,
        cx: &mut ViewContext<Self>,
    ) {
        let Some(init_modifiers) = self.init_modifiers else {
            return;
        };
        if !event.modified() || !init_modifiers.is_subset_of(event) {
            self.init_modifiers = None;
            if self.picker.read(cx).delegate.matches.is_empty() {
                cx.emit(DismissEvent)
            } else {
                cx.dispatch_action(menu::Confirm.boxed_clone());
            }
        }
    }

    fn handle_close_selected_item(&mut self, _: &CloseSelectedItem, cx: &mut ViewContext<Self>) {
        self.picker.update(cx, |picker, cx| {
            picker
                .delegate
                .close_item_at(picker.delegate.selected_index(), cx)
        });
    }
}

impl EventEmitter<DismissEvent> for TabSwitcher {}

impl FocusableView for TabSwitcher {
    fn focus_handle(&self, cx: &AppContext) -> FocusHandle {
        self.picker.focus_handle(cx)
    }
}

impl Render for TabSwitcher {
    fn render(&mut self, cx: &mut ViewContext<Self>) -> impl IntoElement {
        v_flex()
            .key_context("TabSwitcher")
            .w(rems(PANEL_WIDTH_REMS))
            .on_modifiers_changed(cx.listener(Self::handle_modifiers_changed))
            .on_action(cx.listener(Self::handle_close_selected_item))
            .child(self.picker.clone())
    }
}

struct TabMatch {
    item_index: usize,
    item: Box<dyn ItemHandle>,
    detail: usize,
    preview: bool,
}

pub struct TabSwitcherDelegate {
    select_last: bool,
    tab_switcher: WeakView<TabSwitcher>,
    selected_index: usize,
    pane: WeakView<Pane>,
    project: Model<Project>,
    matches: Vec<TabMatch>,
}

impl TabSwitcherDelegate {
    fn new(
        project: Model<Project>,
        action: &Toggle,
        tab_switcher: WeakView<TabSwitcher>,
        pane: WeakView<Pane>,
        cx: &mut ViewContext<TabSwitcher>,
    ) -> Self {
        Self::subscribe_to_updates(&pane, cx);
        Self {
            select_last: action.select_last,
            tab_switcher,
            selected_index: 0,
            pane,
            project,
            matches: Vec::new(),
        }
    }

    fn subscribe_to_updates(pane: &WeakView<Pane>, cx: &mut ViewContext<TabSwitcher>) {
        let Some(pane) = pane.upgrade() else {
            return;
        };
        cx.subscribe(&pane, |tab_switcher, _, event, cx| {
            match event {
                PaneEvent::AddItem { .. }
                | PaneEvent::RemovedItem { .. }
                | PaneEvent::Remove { .. } => tab_switcher.picker.update(cx, |picker, cx| {
                    picker.delegate.update_matches(cx);
                    cx.notify();
                }),
                _ => {}
            };
        })
        .detach();
    }

    fn update_matches(&mut self, cx: &mut WindowContext) {
        let selected_item_id = self.selected_item_id();
        self.matches.clear();
        let Some(pane) = self.pane.upgrade() else {
            return;
        };

        let pane = pane.read(cx);
        let mut history_indices = HashMap::default();
        pane.activation_history().iter().rev().enumerate().for_each(
            |(history_index, history_entry)| {
                history_indices.insert(history_entry.entity_id, history_index);
            },
        );

        let items: Vec<Box<dyn ItemHandle>> = pane.items().map(|item| item.boxed_clone()).collect();
        items
            .iter()
            .enumerate()
            .zip(tab_details(&items, cx))
            .map(|((item_index, item), detail)| TabMatch {
                item_index,
                item: item.boxed_clone(),
                detail,
                preview: pane.is_active_preview_item(item.item_id()),
            })
            .for_each(|tab_match| self.matches.push(tab_match));

        let non_history_base = history_indices.len();
        self.matches.sort_by(move |a, b| {
            let a_score = *history_indices
                .get(&a.item.item_id())
                .unwrap_or(&(a.item_index + non_history_base));
            let b_score = *history_indices
                .get(&b.item.item_id())
                .unwrap_or(&(b.item_index + non_history_base));
            a_score.cmp(&b_score)
        });

        self.selected_index = self.compute_selected_index(selected_item_id);
    }

    fn selected_item_id(&self) -> Option<EntityId> {
        self.matches
            .get(self.selected_index())
            .map(|tab_match| tab_match.item.item_id())
    }

    fn compute_selected_index(&mut self, prev_selected_item_id: Option<EntityId>) -> usize {
        if self.matches.is_empty() {
            return 0;
        }

        if let Some(selected_item_id) = prev_selected_item_id {
            // If the previously selected item is still in the list, select its new position.
            if let Some(item_index) = self
                .matches
                .iter()
                .position(|tab_match| tab_match.item.item_id() == selected_item_id)
            {
                return item_index;
            }
            // Otherwise, try to preserve the previously selected index.
            return self.selected_index.min(self.matches.len() - 1);
        }

        if self.select_last {
            return self.matches.len() - 1;
        }

        if self.matches.len() > 1 {
            // Index 0 is active, so don't preselect it for switching.
            return 1;
        }

        0
    }

    fn close_item_at(&mut self, ix: usize, cx: &mut ViewContext<Picker<TabSwitcherDelegate>>) {
        let Some(tab_match) = self.matches.get(ix) else {
            return;
        };
        let Some(pane) = self.pane.upgrade() else {
            return;
        };
        pane.update(cx, |pane, cx| {
            pane.close_item_by_id(tab_match.item.item_id(), SaveIntent::Close, cx)
                .detach_and_log_err(cx);
        });
    }
}

impl PickerDelegate for TabSwitcherDelegate {
    type ListItem = ListItem;

    fn placeholder_text(&self, _cx: &mut WindowContext) -> Arc<str> {
        Arc::default()
    }

    fn no_matches_text(&self, _cx: &mut WindowContext) -> SharedString {
        "No tabs".into()
    }

    fn match_count(&self) -> usize {
        self.matches.len()
    }

    fn selected_index(&self) -> usize {
        self.selected_index
    }

    fn set_selected_index(&mut self, ix: usize, cx: &mut ViewContext<Picker<Self>>) {
        self.selected_index = ix;
        cx.notify();
    }

    fn separators_after_indices(&self) -> Vec<usize> {
        Vec::new()
    }

    fn update_matches(
        &mut self,
        _raw_query: String,
        cx: &mut ViewContext<Picker<Self>>,
    ) -> Task<()> {
        self.update_matches(cx);
        Task::ready(())
    }

    fn confirm(&mut self, _secondary: bool, cx: &mut ViewContext<Picker<TabSwitcherDelegate>>) {
        let Some(pane) = self.pane.upgrade() else {
            return;
        };
        let Some(selected_match) = self.matches.get(self.selected_index()) else {
            return;
        };
        pane.update(cx, |pane, cx| {
            pane.activate_item(selected_match.item_index, true, true, cx);
        });
    }

    fn dismissed(&mut self, cx: &mut ViewContext<Picker<TabSwitcherDelegate>>) {
        self.tab_switcher
            .update(cx, |_, cx| cx.emit(DismissEvent))
            .log_err();
    }

    fn render_match(
        &self,
        ix: usize,
        selected: bool,
        cx: &mut ViewContext<Picker<Self>>,
    ) -> Option<Self::ListItem> {
        let tab_match = self
            .matches
            .get(ix)
            .expect("Invalid matches state: no element for index {ix}");

        let params = TabContentParams {
            detail: Some(tab_match.detail),
            selected: true,
            preview: tab_match.preview,
        };
        let label = tab_match.item.tab_content(params, cx);

        let icon = tab_match.item.tab_icon(cx).map(|icon| {
            let git_status_color = ItemSettings::get_global(cx)
                .git_status
                .then(|| {
                    tab_match
                        .item
                        .project_path(cx)
                        .as_ref()
                        .and_then(|path| {
                            let project = self.project.read(cx);
                            let entry = project.entry_for_path(path, cx)?;
                            let git_status = project
                                .project_path_git_status(path, cx)
                                .map(|status| status.summary())
                                .unwrap_or_default();
                            Some((entry, git_status))
                        })
                        .map(|(entry, git_status)| {
                            entry_git_aware_label_color(git_status, entry.is_ignored, selected)
                        })
                })
                .flatten();

            icon.color(git_status_color.unwrap_or_default())
        });

        let indicator = render_item_indicator(tab_match.item.boxed_clone(), cx);
        let indicator_color = if let Some(ref indicator) = indicator {
            indicator.color
        } else {
            Color::default()
        };
        let indicator = h_flex()
            .flex_shrink_0()
            .children(indicator)
            .child(div().w_2())
            .into_any_element();
        let close_button = div()
            // We need this on_mouse_up here instead of on_click on the close
            // button because Picker intercepts the same events and handles them
            // as click's on list items.
            // See the same handler in Picker for more details.
            .on_mouse_up(
                MouseButton::Right,
                cx.listener(move |picker, _: &MouseUpEvent, cx| {
                    cx.stop_propagation();
                    picker.delegate.close_item_at(ix, cx);
                }),
            )
            .child(
                IconButton::new("close_tab", IconName::Close)
                    .icon_size(IconSize::Small)
                    .icon_color(indicator_color)
                    .tooltip(|cx| Tooltip::text("Close", cx)),
            )
            .into_any_element();

        Some(
            ListItem::new(ix)
                .spacing(ListItemSpacing::Sparse)
                .inset(true)
                .toggle_state(selected)
                .child(h_flex().w_full().child(label))
                .start_slot::<Icon>(icon)
                .map(|el| {
                    if self.selected_index == ix {
                        el.end_slot::<AnyElement>(close_button)
                    } else {
                        el.end_slot::<AnyElement>(indicator)
                            .end_hover_slot::<AnyElement>(close_button)
                    }
                }),
        )
    }
}
