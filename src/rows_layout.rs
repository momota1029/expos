use gpui::{
    AppContext as _, Context, Hsla, InteractiveElement as _, IntoElement as _, ParentElement as _,
    SharedString, StatefulInteractiveElement as _, Styled as _, div, prelude::FluentBuilder as _,
    px,
};
use gpui_filer_ui::BORDER;

use crate::{
    CELL_PADDING_X, CELL_PADDING_Y, CHROME_FONT_SIZE, DirectoryListDemo, HEADER_HEIGHT,
    ROW_PADDING_X, RowColumnReorderDrag, RowColumnResizeDrag,
    shell::{Message, Translator},
    sorting::{Column, SortState},
};

const ROW_CHECKBOX_WIDTH: f32 = 44.0;
const MIN_NAME_CELL_WIDTH: f32 = 144.0;

/// Rows header と body が共有する、view 幅から導く列 projection。
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct RowColumns {
    pub(super) checkbox_visible: bool,
    pub(super) kind: bool,
    pub(super) size: bool,
    pub(super) modified: bool,
    pub(super) name_width: f32,
    pub(super) order: [Column; 4],
}

pub(super) fn project_columns(
    view_width: f32,
    enabled: crate::folder_view_preferences::ColumnPrefs,
) -> RowColumns {
    project_columns_with_checkbox(view_width, enabled, true)
}

pub(super) fn project_columns_with_checkbox(
    view_width: f32,
    enabled: crate::folder_view_preferences::ColumnPrefs,
    checkbox_visible: bool,
) -> RowColumns {
    let checkbox_width = if checkbox_visible {
        ROW_CHECKBOX_WIDTH
    } else {
        0.0
    };
    let content_width = (view_width - ROW_PADDING_X * 2.0 - checkbox_width).max(0.0);
    let mut columns = RowColumns {
        checkbox_visible,
        kind: enabled.kind,
        size: enabled.size,
        modified: enabled.modified,
        name_width: 0.0,
        order: enabled.order,
    };
    let required_width = |columns: RowColumns| {
        MIN_NAME_CELL_WIDTH
            + if columns.kind {
                enabled.kind_width + 1.0
            } else {
                0.0
            }
            + if columns.size {
                enabled.size_width + 1.0
            } else {
                0.0
            }
            + if columns.modified {
                enabled.modified_width + 1.0
            } else {
                0.0
            }
    };
    if content_width < required_width(columns) {
        columns.modified = false;
    }
    if content_width < required_width(columns) {
        columns.size = false;
    }
    if content_width < required_width(columns) {
        columns.kind = false;
    }
    let metadata_width = if columns.kind {
        enabled.kind_width + 1.0
    } else {
        0.0
    } + if columns.size {
        enabled.size_width + 1.0
    } else {
        0.0
    } + if columns.modified {
        enabled.modified_width + 1.0
    } else {
        0.0
    };
    // 狭い pane では列を縮めて一文字折り返しにせず、Name cell を通常幅のまま
    // viewport の clip に任せる。header/body とも同じ projection を使う。
    columns.name_width = (content_width - metadata_width).max(MIN_NAME_CELL_WIDTH);
    columns
}

pub(super) fn detail_cell(text: impl Into<SharedString>, width: f32) -> gpui::Div {
    div()
        .w(px(width))
        .h_full()
        .px(px(CELL_PADDING_X))
        .py(px(CELL_PADDING_Y))
        .flex()
        .items_center()
        .flex_shrink_0()
        .overflow_hidden()
        .child(div().flex_1().min_w_0().truncate().child(text.into()))
}

fn resize_divider(left: Column, right: Column, cx: &mut Context<DirectoryListDemo>) -> gpui::Div {
    div()
        .relative()
        .w(px(1.0))
        .bg(BORDER)
        .opacity(0.6)
        .flex_shrink_0()
        .child(
            div()
                .id(("row-column-resize", right as usize))
                .absolute()
                .top_0()
                .bottom_0()
                .left(px(-4.5))
                .w(px(10.0))
                .cursor_col_resize()
                .on_mouse_down(
                    gpui::MouseButton::Left,
                    cx.listener(move |this, event: &gpui::MouseDownEvent, _, cx| {
                        cx.stop_propagation();
                        if event.source == gpui::MouseInputSource::Mouse && event.click_count == 1 {
                            this.begin_row_column_resize(left, right, event.position.x.as_f32());
                        }
                    }),
                )
                .on_drag_with_threshold_when(
                    RowColumnResizeDrag,
                    px(2.0),
                    |down| {
                        down.button == gpui::MouseButton::Left
                            && down.source == gpui::MouseInputSource::Mouse
                    },
                    |_, _, _, cx| cx.new(|_| gpui::Empty),
                )
                .on_drag_move(cx.listener(
                    |this, event: &gpui::DragMoveEvent<RowColumnResizeDrag>, _, cx| {
                        this.update_row_column_resize(event.event.position.x.as_f32(), cx);
                    },
                ))
                .on_drop(cx.listener(|this, _: &RowColumnResizeDrag, _, cx| {
                    this.finish_row_column_resize(cx);
                }))
                .on_mouse_up(
                    gpui::MouseButton::Left,
                    cx.listener(|this, _, _, cx| {
                        this.finish_row_column_resize(cx);
                    }),
                )
                .on_mouse_up_out(
                    gpui::MouseButton::Left,
                    cx.listener(|this, _, _, cx| {
                        this.finish_row_column_resize(cx);
                    }),
                )
                .on_click(cx.listener(move |this, event: &gpui::ClickEvent, _, cx| {
                    if matches!(event, gpui::ClickEvent::Mouse(mouse)
                        if mouse.down.button == gpui::MouseButton::Left
                            && mouse.down.click_count == 1
                            && mouse.down.source == gpui::MouseInputSource::Mouse)
                    {
                        cx.stop_propagation();
                    } else if event.standard_click() {
                        this.touch_item_tap_series = None;
                        this.last_click_was_blank = false;
                        this.toggle_sort(right, cx);
                        cx.stop_propagation();
                    }
                })),
        )
}

pub(super) fn column_divider() -> gpui::Div {
    div()
        .w(px(1.0))
        .h_full()
        .bg(BORDER)
        .opacity(0.6)
        .flex_shrink_0()
}

/// 列ヘッダの1セル。クリックで並べ替えを切り替える。
///
/// 並べ替え中の列には v5 と同じく向きを示す印を付ける。
pub(super) fn header_cell(
    column: Column,
    label: &str,
    width: f32,
    sort: SortState,
    cx: &mut Context<DirectoryListDemo>,
) -> gpui::Stateful<gpui::Div> {
    let active = sort.column() == Some(column);
    let marker = match (active, sort.is_descending()) {
        (true, false) => " ▲",
        (true, true) => " ▼",
        _ => "",
    };
    let reorder_entity = cx.entity();
    div()
        .id(("sort", column as usize))
        .w(px(width))
        .min_h(px(HEADER_HEIGHT))
        .px(px(CELL_PADDING_X))
        .py(px(CELL_PADDING_Y))
        .flex()
        .items_center()
        .flex_shrink_0()
        .overflow_hidden()
        .when(active, |cell| {
            cell.text_color(gpui_filer_ui::theme_palette(cx).text)
        })
        .hover(|cell| cell.bg(gpui_filer_ui::theme_palette(cx).border))
        .child(
            div()
                .w_full()
                .min_w_0()
                .whitespace_normal()
                .child(format!("{label}{marker}")),
        )
        .on_mouse_down(
            gpui::MouseButton::Left,
            cx.listener(move |this, event: &gpui::MouseDownEvent, _, _| {
                this.capture_row_column_reorder_down(column, event);
            }),
        )
        .on_drag_with_threshold_when(
            RowColumnReorderDrag { source: column },
            px(8.0),
            |event| {
                event.button == gpui::MouseButton::Left
                    && event.source == gpui::MouseInputSource::Mouse
                    && event.click_count == 1
                    && crate::input::press_chord(event).is_some_and(|chord| {
                        chord.modifiers == gpui_filer_input::Modifiers::default()
                    })
            },
            move |_, _, _, cx| {
                reorder_entity.update(cx, |this, _| this.begin_row_column_reorder(column));
                cx.new(|_| gpui::Empty)
            },
        )
        .on_drag_move(cx.listener(
            |this, _: &gpui::DragMoveEvent<RowColumnReorderDrag>, _, _| {
                let stale = this
                    .row_column_reorder
                    .is_some_and(|drag| !this.row_column_reorder_is_current(drag.down));
                if stale {
                    this.cancel_row_column_reorder();
                }
            },
        ))
        .on_drop(
            cx.listener(move |this, event: &RowColumnReorderDrag, _, cx| {
                this.finish_row_column_reorder(event.source, column, cx);
            }),
        )
        .on_click(
            cx.listener(move |this, event: &gpui::ClickEvent, _window, cx| {
                this.row_column_reorder_down = None;
                if this.row_column_reorder_suppress_click {
                    this.row_column_reorder_suppress_click = false;
                    cx.stop_propagation();
                    return;
                }
                if event.standard_click() {
                    this.touch_item_tap_series = None;
                    this.last_click_was_blank = false;
                    this.toggle_sort(column, cx);
                }
            }),
        )
}

pub(super) fn rows_header(
    foreground: Hsla,
    translator: &Translator,
    sort: SortState,
    columns: RowColumns,
    enabled: crate::folder_view_preferences::ColumnPrefs,
    cx: &mut Context<DirectoryListDemo>,
) -> gpui::Div {
    let name = header_cell(
        Column::Name,
        &translator.text(Message::ColumnName),
        // Name 列は残り幅を使うので、幅は flex 側で決める。
        0.0,
        sort,
        cx,
    )
    .w_auto()
    .min_w(px(columns.name_width))
    .w(px(columns.name_width));
    let kind = header_cell(
        Column::Kind,
        &translator.text(Message::ColumnKind),
        enabled.kind_width,
        sort,
        cx,
    );
    let size = header_cell(
        Column::Size,
        &translator.text(Message::ColumnSize),
        enabled.size_width,
        sort,
        cx,
    )
    .text_right();
    let modified = header_cell(
        Column::Modified,
        &translator.text(Message::ColumnModified),
        enabled.modified_width,
        sort,
        cx,
    );

    let mut name = Some(name.into_any_element());
    let mut kind = Some(kind.into_any_element());
    let mut size = Some(size.into_any_element());
    let mut modified = Some(modified.into_any_element());
    let mut ordered_cells = Vec::with_capacity(7);
    let mut previous = None;
    for column in enabled.order {
        let visible = match column {
            Column::Name => true,
            Column::Kind => columns.kind,
            Column::Size => columns.size,
            Column::Modified => columns.modified,
        };
        if !visible {
            continue;
        }
        if !ordered_cells.is_empty() {
            ordered_cells.push(resize_divider(previous.unwrap(), column, cx).into_any_element());
        }
        ordered_cells.push(match column {
            Column::Name => name.take().unwrap(),
            Column::Kind => kind.take().unwrap(),
            Column::Size => size.take().unwrap(),
            Column::Modified => modified.take().unwrap(),
        });
        previous = Some(column);
    }

    div()
        .w_full()
        .min_h(px(HEADER_HEIGHT))
        .px(px(ROW_PADDING_X))
        .flex_none()
        .flex()
        .items_stretch()
        .bg(gpui_filer_ui::theme_palette(cx).surface_header)
        .border_b_1()
        .border_color(gpui_filer_ui::theme_palette(cx).border)
        .text_color(foreground)
        .text_size(px(CHROME_FONT_SIZE))
        .child(
            div()
                .w(px(if columns.checkbox_visible {
                    ROW_CHECKBOX_WIDTH
                } else {
                    0.0
                }))
                .flex_shrink_0(),
        )
        .children(ordered_cells)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checkbox_visibility_keeps_header_and_body_column_geometry_in_sync() {
        let mut prefs = crate::folder_view_preferences::ColumnPrefs::default();
        prefs.order = [Column::Kind, Column::Name, Column::Size, Column::Modified];
        let with_checkbox = project_columns_with_checkbox(1100.0, prefs, true);
        let without_checkbox = project_columns_with_checkbox(1100.0, prefs, false);

        assert_eq!(with_checkbox.order, without_checkbox.order);
        assert_eq!(with_checkbox.kind, without_checkbox.kind);
        assert_eq!(with_checkbox.size, without_checkbox.size);
        assert_eq!(with_checkbox.modified, without_checkbox.modified);
        assert_eq!(with_checkbox.checkbox_visible, true);
        assert_eq!(without_checkbox.checkbox_visible, false);
        assert_eq!(
            without_checkbox.name_width - with_checkbox.name_width,
            ROW_CHECKBOX_WIDTH
        );

        // Header cells and body cells both consume RowColumns; confirm reordered fixed
        // columns keep the same divider coordinates on either side of the Name cell.
        let divider_positions = |columns: RowColumns| {
            let mut x = if columns.checkbox_visible {
                ROW_CHECKBOX_WIDTH
            } else {
                0.0
            };
            let mut positions = Vec::new();
            for column in columns.order {
                let (visible, width) = match column {
                    Column::Name => (true, columns.name_width),
                    Column::Kind => (columns.kind, prefs.kind_width),
                    Column::Size => (columns.size, prefs.size_width),
                    Column::Modified => (columns.modified, prefs.modified_width),
                };
                if visible {
                    x += width;
                    positions.push(x);
                    x += 1.0;
                }
            }
            positions
        };
        let header_with = divider_positions(with_checkbox);
        let body_with = divider_positions(with_checkbox);
        let header_without = divider_positions(without_checkbox);
        let body_without = divider_positions(without_checkbox);
        assert_eq!(header_with, body_with);
        assert_eq!(header_without, body_without);
        assert_eq!(header_with.len(), 4);
        assert_eq!(header_without.len(), 4);
    }

    #[test]
    fn columns_hide_metadata_in_priority_order_and_restore_at_thresholds() {
        let all_columns = 487.0;
        let no_modified = 487.0;
        let no_size = 366.0;
        let name_only = 285.0;

        let enabled = crate::folder_view_preferences::ColumnPrefs::default();
        let all = project_columns(all_columns, enabled);
        assert!(all.kind && all.size && all.modified);
        assert_eq!(all.name_width, MIN_NAME_CELL_WIDTH);

        let modified_hidden = project_columns(no_modified - 0.1, enabled);
        assert!(modified_hidden.kind && modified_hidden.size);
        assert!(!modified_hidden.modified);
        assert!(modified_hidden.name_width >= MIN_NAME_CELL_WIDTH);

        let size_hidden = project_columns(no_size - 0.1, enabled);
        assert!(size_hidden.kind);
        assert!(!size_hidden.size && !size_hidden.modified);
        assert!(size_hidden.name_width >= MIN_NAME_CELL_WIDTH);

        let name_only_columns = project_columns(name_only - 0.1, enabled);
        assert!(!name_only_columns.kind && !name_only_columns.size && !name_only_columns.modified);
        assert!(name_only_columns.name_width >= MIN_NAME_CELL_WIDTH);

        let narrow = project_columns(180.0, enabled);
        assert!(!narrow.kind && !narrow.size && !narrow.modified);
        assert_eq!(narrow.name_width, MIN_NAME_CELL_WIDTH);

        let wide = project_columns(2500.0, enabled);
        assert!(wide.kind && wide.size && wide.modified);
        assert!(wide.name_width > all.name_width);
    }
}
