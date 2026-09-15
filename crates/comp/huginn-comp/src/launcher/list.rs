//! The list layout: a search bar that opens into a grid.
//!
//! Closed, the launcher is only its field — a bar hanging from near the top
//! of the screen (see [`super::placement`]), with a chevron that says there
//! is more. Opened, a grid hangs below the field. Before anything is typed
//! that is one row of suggestions and a foot of the pinned and recently used
//! applications; once something is, tabs that narrow the search by kind, the
//! sort, and the results as tiles, applications before files, with the
//! arithmetic result above them and the offer to run the query below.

use super::paint::*;
use super::*;
use crate::text::Weight;
use crate::theme::{TEXT, TEXT_DIM};

/// The panel's width at a 1080p output, in logical pixels.
const WIDTH: f32 = 760.0;

/// Lay out and paint the list. See [`super::compose`].
pub(super) fn compose(
    launcher: &Launcher,
    apps: &[Entry],
    text: &mut Text,
    icons: &Icons,
    pixmaps: &mut Pixmaps,
    output: Rect,
    density: u32,
) -> (Canvas, Layout) {
    let m = Metrics::for_output(output, density);
    let scale = m.scale;
    let px = |v: f32| v * scale;
    let width = px(WIDTH)
        .min(output.w() as f32 * m.density as f32 - px(32.0))
        .max(px(360.0))
        .floor();
    let pad = px(9.0);
    let field_h = px(50.0);
    // The grid sits a little further in than the field, as the field's own
    // rounding does, so the tiles line up with the text in it.
    let inset = pad + px(6.0);
    let inner = width - inset * 2.0;
    let gap = px(4.0);
    let tile_w = (inner - gap * (COLUMNS - 1) as f32) / COLUMNS as f32;
    let tile_h = px(108.0);
    let tools_h = px(34.0);
    let heading_h = px(18.0);
    let section = px(12.0);
    let row_h = m.row;
    let strip_h = px(54.0);
    let strip_head = px(28.0);
    let hint_size = px(11.5);
    let keys_h = hint_size * 1.75;

    let grid = launcher.is_grid();
    let collapsed = launcher.is_collapsed();
    let target = launcher.target();
    let visible = launcher.visible();
    let has_result = visible.first() == Some(&Target::Result);
    let has_command = visible.last() == Some(&Target::Command);
    let tiles: Vec<Target> = if grid {
        launcher
            .suggested()
            .iter()
            .map(|i| Target::App(*i))
            .collect()
    } else {
        launcher
            .results()
            .iter()
            .map(|i| Target::App(*i))
            .chain(launcher.file_hits().iter().map(|i| Target::File(*i)))
            .collect()
    };
    let tile_start = usize::from(has_result);
    let total_rows = tiles.len().div_ceil(COLUMNS);
    let first_row = if grid { 0 } else { launcher.first_row() };
    let drawn_rows = if grid {
        total_rows.min(1)
    } else {
        total_rows.saturating_sub(first_row).min(GRID_ROWS)
    };
    let strip = if grid {
        launcher.pins_shown().len() + launcher.recent().len()
    } else {
        0
    };
    let nothing = tiles.is_empty() && !has_result && !has_command;
    let menu = launcher
        .menu()
        .and(launcher.selection())
        .and_then(|i| apps.get(i))
        .map(|entry| launcher.menu_items(entry));
    let menu_metrics = m.with_width(width as usize);

    // How tall it is, from what it will show.
    let mut body = 0.0;
    if !collapsed {
        body += section;
        body += if grid { heading_h } else { tools_h };
        body += section;
        if has_result {
            body += row_h + section / 2.0;
        }
        if drawn_rows > 0 {
            body += drawn_rows as f32 * tile_h + (drawn_rows - 1) as f32 * gap;
        }
        if nothing {
            body += row_h;
        }
        if has_command {
            body += section / 2.0 + row_h;
        }
        if strip > 0 {
            body += section * 2.0 + 1.0 + strip_head + strip_h;
        }
        if let Some(items) = &menu {
            body = body.max(menu_metrics.menu_height(items.len()) + section);
        }
        body += section + keys_h + px(4.0);
    }
    let height = (pad * 2.0 + field_h + body).ceil().max(1.0) as usize;
    let width_px = width as usize;

    let mut canvas = Canvas::new(width_px, height);
    let mut layout = Layout {
        size: (width_px as i32, height as i32),
        ..Layout::default()
    };
    let rect =
        |x: f32, y: f32, w: f32, h: f32| Rect::from_xywh(x as i32, y as i32, w as i32, h as i32);
    let accent = crate::theme::accent();
    canvas.material(0, 0, width_px, height, px(22.0), ALPHA);

    // The field: a well with the accent's edge, since it always has focus.
    let (fx, fy, fw) = (pad, pad, width - pad * 2.0);
    canvas.fill_rounded(
        fx as usize,
        fy as usize,
        fw as usize,
        field_h as usize,
        px(14.0),
        rgba(255, 255, 255, 0.05),
    );
    canvas.stroke_rounded(
        fx as usize,
        fy as usize,
        fw as usize,
        field_h as usize,
        px(14.0),
        1.0_f32.max(scale * 0.75),
        faded(accent, 0.5),
    );
    let glyph_x = fx + px(24.0);
    draw_search_glyph(
        &mut canvas,
        glyph_x,
        fy + field_h / 2.0 - px(1.5),
        px(6.0),
        px(1.6),
        TEXT_DIM,
    );
    // The chevron: down on the bar, up once it has opened.
    let chevron = px(34.0);
    let (chevron_x, chevron_y) = (fx + fw - px(8.0) - chevron, fy + (field_h - chevron) / 2.0);
    draw_chevron(
        &mut canvas,
        chevron_x + chevron / 2.0,
        chevron_y + chevron / 2.0,
        px(5.0),
        px(1.8),
        !collapsed,
        TEXT_DIM,
    );
    layout
        .buttons
        .push((rect(chevron_x, chevron_y, chevron, chevron), Button::Expand));
    let query_size = px(16.5);
    let text_x = glyph_x + px(20.0);
    let text_y = fy + (field_h - query_size * 1.35) / 2.0;
    let room = chevron_x - px(10.0) - text_x;
    let caret_w = px(2.0).max(1.0);
    if launcher.query().is_empty() {
        // The caret before the hint, not through its first letter.
        canvas.fill(
            text_x as usize,
            (text_y + px(2.0)) as usize,
            caret_w as usize,
            (query_size * 1.2) as usize,
            accent.to_rgba_bytes(),
        );
        let hint = fit(text, PLACEHOLDER, query_size, room - px(8.0));
        text.draw(
            &mut canvas,
            &hint,
            query_size,
            (text_x + caret_w + px(6.0)) as i32,
            text_y as i32,
            TEXT_DIM,
        );
    } else {
        let shown = fit_tail(text, launcher.query(), query_size, room - px(6.0));
        text.draw(
            &mut canvas,
            &shown,
            query_size,
            text_x as i32,
            text_y as i32,
            TEXT,
        );
        let caret_x = text_x + text.measure(&shown, query_size).0 + px(2.0);
        canvas.fill(
            caret_x as usize,
            (text_y + px(2.0)) as usize,
            caret_w as usize,
            (query_size * 1.2) as usize,
            accent.to_rgba_bytes(),
        );
    }

    if collapsed {
        return (canvas, layout);
    }

    let mut y = pad + field_h + section;
    if grid {
        text.draw_weighted(
            &mut canvas,
            "Suggested",
            px(12.5),
            (inset + px(4.0)) as i32,
            y as i32,
            TEXT,
            EMPHASIS,
        );
        let note = "From what you opened recently";
        let note_w = text.measure(note, px(11.5)).0;
        text.draw(
            &mut canvas,
            note,
            px(11.5),
            (inset + inner - px(4.0) - note_w) as i32,
            (y + px(1.0)) as i32,
            TEXT_DIM,
        );
        y += heading_h + section;
    } else {
        // Tabs by kind, with how many of each the search found; the sort at
        // the far end. A hairline under both.
        let (apps_found, files_found) = launcher.found();
        let tab_size = px(13.0);
        let count_size = px(10.5);
        let mut tx = inset;
        for filter in Filter::ALL {
            let count = match filter {
                Filter::All => apps_found + files_found,
                Filter::Apps => apps_found,
                Filter::Files => files_found,
            }
            .to_string();
            let label_w = text.measure(filter.label(), tab_size).0;
            let count_w = text.measure(&count, count_size).0;
            let tab_w = px(12.0) + label_w + px(7.0) + count_w + px(12.0);
            let on = launcher.filter() == filter;
            let label_y = y + px(6.0);
            text.draw(
                &mut canvas,
                filter.label(),
                tab_size,
                (tx + px(12.0)) as i32,
                label_y as i32,
                if on { TEXT } else { TEXT_DIM },
            );
            text.draw(
                &mut canvas,
                &count,
                count_size,
                (tx + px(12.0) + label_w + px(7.0)) as i32,
                (label_y + (tab_size - count_size) * 1.1) as i32,
                TEXT_DIM,
            );
            if on {
                canvas.fill_rounded(
                    (tx + px(12.0)) as usize,
                    (y + tools_h - px(2.0)) as usize,
                    (label_w + px(7.0) + count_w) as usize,
                    px(2.0).max(1.0) as usize,
                    px(1.0),
                    accent,
                );
            }
            layout
                .buttons
                .push((rect(tx, y, tab_w, tools_h), Button::Filter(filter)));
            tx += tab_w + px(2.0);
        }
        let sort_size = px(12.5);
        let lead_w = text.measure("Sort", sort_size).0;
        let label = launcher.sort().label();
        let label_w = text.measure_weighted(label, sort_size, EMPHASIS).0;
        let sort_w = lead_w + px(5.0) + label_w + px(16.0);
        let sort_x = inset + inner - sort_w - px(6.0);
        let sort_y = y + px(6.0);
        text.draw(
            &mut canvas,
            "Sort",
            sort_size,
            sort_x as i32,
            sort_y as i32,
            TEXT_DIM,
        );
        text.draw_weighted(
            &mut canvas,
            label,
            sort_size,
            (sort_x + lead_w + px(5.0)) as i32,
            sort_y as i32,
            TEXT,
            EMPHASIS,
        );
        draw_chevron(
            &mut canvas,
            sort_x + sort_w - px(5.0),
            sort_y + sort_size * 0.7,
            px(3.5),
            px(1.5),
            false,
            TEXT_DIM,
        );
        layout.buttons.push((
            rect(sort_x - px(6.0), y, sort_w + px(12.0), tools_h),
            Button::Sort,
        ));
        canvas.tint(
            inset as usize,
            (y + tools_h) as usize,
            inner as usize,
            1,
            WHITE,
            0x12,
        );
        y += tools_h + section;
    }

    let style = RowStyle {
        pad: inset,
        inner,
        row: row_h,
        size: m.size,
        scale,
    };
    if has_result && let Some(value) = launcher.result() {
        layout.hits.push((rect(inset, y, inner, row_h), 0));
        glyph_row(
            &mut canvas,
            text,
            &style,
            y,
            RESULT_GLYPH,
            value,
            target == Some(Target::Result),
        );
        y += row_h + section / 2.0;
    }

    let icon_size = px(44.0) as u32;
    for (n, tile) in tiles
        .iter()
        .enumerate()
        .skip(first_row * COLUMNS)
        .take(drawn_rows * COLUMNS)
    {
        let (column, row) = (n % COLUMNS, n / COLUMNS - first_row);
        let x = inset + (tile_w + gap) * column as f32;
        let ty = y + (tile_h + gap) * row as f32;
        let position = tile_start + n;
        layout.hits.push((rect(x, ty, tile_w, tile_h), position));
        let selected = launcher.selected() == position;
        match *tile {
            Target::App(index) => {
                let Some(entry) = apps.get(index) else {
                    continue;
                };
                let icon = app_icon(icons, pixmaps, entry, icon_size, density);
                let sub = if grid {
                    launcher
                        .last_used(index)
                        .map(|at| ago(launcher.now().saturating_sub(at)))
                } else {
                    kind_of(entry).map(str::to_owned)
                };
                draw_tile(
                    &mut canvas,
                    text,
                    (x, ty, tile_w, tile_h),
                    scale,
                    icon.as_ref(),
                    &entry.name,
                    sub.as_deref(),
                    selected,
                    launcher.is_pinned(entry),
                );
            }
            Target::File(index) => {
                let Some(file) = launcher.files().get(index) else {
                    continue;
                };
                let icon = file_icon(icons, pixmaps, icon_size, density);
                let location = launcher.files().location(index);
                draw_tile(
                    &mut canvas,
                    text,
                    (x, ty, tile_w, tile_h),
                    scale,
                    icon.as_ref(),
                    &file.name,
                    Some(&location),
                    selected,
                    false,
                );
            }
            Target::Command | Target::Result => {}
        }
    }
    if drawn_rows > 0 {
        y += drawn_rows as f32 * tile_h + (drawn_rows - 1) as f32 * gap;
    }
    if nothing {
        let note = if grid {
            "Open something and it will be suggested here."
        } else if launcher.filter() != Filter::All {
            "Nothing of this kind. Ctrl ← shows everything."
        } else {
            "No matches. Try a shorter word."
        };
        text.draw(
            &mut canvas,
            note,
            m.size * 0.95,
            (inset + px(10.0)) as i32,
            (y + (row_h - m.size * 1.3) / 2.0) as i32,
            TEXT_DIM,
        );
        y += row_h;
    }
    if has_command {
        y += section / 2.0;
        let label = fit(
            text,
            &format!("Run \"{}\"", launcher.query()),
            m.size,
            inner - px(60.0),
        );
        layout
            .hits
            .push((rect(inset, y, inner, row_h), visible.len() - 1));
        glyph_row(
            &mut canvas,
            text,
            &style,
            y,
            COMMAND_GLYPH,
            &label,
            target == Some(Target::Command),
        );
        y += row_h;
    }

    if strip > 0 {
        y += section;
        canvas.tint(inset as usize, y as usize, inner as usize, 1, WHITE, 0x12);
        y += 1.0 + section;
        draw_strip(
            &mut canvas,
            text,
            icons,
            pixmaps,
            &mut layout,
            launcher,
            apps,
            (inset, y, inner),
            scale,
            density,
        );
        y += strip_head + strip_h;
    }

    // The actions menu, over the bottom of the body against the right edge.
    let body_bottom = y;
    let mut keys_top = body_bottom;
    if let (Some(item), Some(entry), Some(items)) = (
        launcher.menu(),
        launcher.selection().and_then(|i| apps.get(i)),
        &menu,
    ) {
        // A menu taller than the body is pushed down past it, and the hints
        // go under the menu rather than through it.
        let menu_h = menu_metrics.menu_height(items.len());
        let menu_top = (body_bottom - menu_h).max(pad + field_h + section);
        keys_top = keys_top.max(menu_top + menu_h);
        draw_menu(
            &mut canvas,
            text,
            &mut layout,
            &menu_metrics,
            &entry.name,
            items,
            item,
            body_bottom,
            pad + field_h + section,
        );
    }

    let hints = hints_for(target, launcher.menu().is_some(), grid);
    draw_hints(
        &mut canvas,
        text,
        hints,
        Align::Right(inset + inner - px(4.0)),
        keys_top + section,
        hint_size,
        scale,
        crate::theme::WELL_RAISED,
        None,
    );

    (canvas, layout)
}

/// A tile of the grid: icon, name, and a line under it — when it was used,
/// what it is, or where a file lives — washed and ringed in the accent when
/// highlighted, with a dot on the icon when the application is pinned.
#[allow(clippy::too_many_arguments)]
fn draw_tile(
    canvas: &mut Canvas,
    text: &mut Text,
    (x, y, w, h): (f32, f32, f32, f32),
    scale: f32,
    icon: Option<&raven_desktop::Pixmap>,
    title: &str,
    sub: Option<&str>,
    selected: bool,
    pinned: bool,
) {
    let px = |v: f32| v * scale;
    let accent = crate::theme::accent();
    if selected {
        let (xu, yu, wu, hu) = (x as usize, y as usize, w as usize, h as usize);
        canvas.fill_rounded(xu, yu, wu, hu, px(16.0), faded(accent, 0.13));
        canvas.stroke_rounded(
            xu,
            yu,
            wu,
            hu,
            px(16.0),
            1.0_f32.max(scale),
            faded(accent, 0.6),
        );
    }
    let cx = x + w / 2.0;
    let icon_box = px(52.0);
    let top = y + px(12.0);
    if let Some(icon) = icon {
        blit_centred(canvas, icon, cx, top + icon_box / 2.0);
    }
    if pinned {
        glow_dot(
            canvas,
            cx + icon_box / 2.0 - px(4.0),
            top + px(4.0),
            px(3.5),
            px(4.5),
            accent,
            rgba(16, 16, 24, 0.9),
        );
    }
    let title_size = px(12.5);
    let title = fit(text, title, title_size, w - px(12.0));
    let title_y = top + icon_box + px(4.0);
    draw_centred(
        text,
        canvas,
        &title,
        title_size,
        cx,
        title_y,
        TEXT,
        Weight::NORMAL,
    );
    if let Some(sub) = sub {
        let sub_size = px(11.0);
        let sub = fit(text, sub, sub_size, w - px(12.0));
        draw_centred(
            text,
            canvas,
            &sub,
            sub_size,
            cx,
            title_y + title_size * 1.35,
            TEXT_DIM,
            Weight::NORMAL,
        );
    }
}

/// The foot of the opened list: "Pinned & Recent" with a link to the pinned
/// panel, then the pinned applications as icons and, past a divider, the
/// recently used ones as cards saying when.
#[allow(clippy::too_many_arguments)]
fn draw_strip(
    canvas: &mut Canvas,
    text: &mut Text,
    icons: &Icons,
    pixmaps: &mut Pixmaps,
    layout: &mut Layout,
    launcher: &Launcher,
    apps: &[Entry],
    (x, y, inner): (f32, f32, f32),
    scale: f32,
    density: u32,
) {
    let px = |v: f32| v * scale;
    let rect =
        |x: f32, y: f32, w: f32, h: f32| Rect::from_xywh(x as i32, y as i32, w as i32, h as i32);
    let accent = crate::theme::accent();
    let start = launcher.suggested().len();
    let (pins, recent) = (launcher.pins_shown(), launcher.recent());
    let selected = launcher.selected();
    let on_strip = (start..start + pins.len() + recent.len()).contains(&selected);

    // The heading names what the highlight is on, since the icons have no
    // labels of their own.
    let head_size = px(12.5);
    let head = "Pinned & Recent";
    text.draw_weighted(
        canvas,
        head,
        head_size,
        (x + px(4.0)) as i32,
        y as i32,
        TEXT,
        EMPHASIS,
    );
    let link = "Pinned panel →";
    let link_size = px(12.0);
    let link_w = text.measure(link, link_size).0;
    let link_x = x + inner - link_w - px(6.0);
    if on_strip
        && let Some(Target::App(index)) = launcher.visible().get(selected)
        && let Some(entry) = apps.get(*index)
    {
        let head_w = text.measure_weighted(head, head_size, EMPHASIS).0;
        let name_x = x + px(4.0) + head_w + px(6.0);
        let name = fit(
            text,
            &format!("· {}", entry.name),
            head_size,
            link_x - px(12.0) - name_x,
        );
        text.draw(canvas, &name, head_size, name_x as i32, y as i32, TEXT_DIM);
    }
    text.draw(canvas, link, link_size, link_x as i32, y as i32, TEXT_DIM);
    layout.buttons.push((
        rect(link_x - px(6.0), y - px(4.0), link_w + px(12.0), px(22.0)),
        Button::PinnedPanel,
    ));

    let row_y = y + px(28.0);
    let box_size = px(54.0);
    let gap = px(4.0);
    let wash = |canvas: &mut Canvas, (bx, by, bw, bh): (f32, f32, f32, f32), radius: f32| {
        let (xu, yu, wu, hu) = (bx as usize, by as usize, bw as usize, bh as usize);
        canvas.fill_rounded(xu, yu, wu, hu, radius, faded(accent, 0.13));
        canvas.stroke_rounded(
            xu,
            yu,
            wu,
            hu,
            radius,
            1.0_f32.max(scale),
            faded(accent, 0.6),
        );
    };
    let mut sx = x + px(2.0);
    for (n, index) in pins.iter().enumerate() {
        let position = start + n;
        if position == selected {
            wash(canvas, (sx, row_y, box_size, box_size), px(14.0));
        }
        if let Some(icon) = apps
            .get(*index)
            .and_then(|entry| app_icon(icons, pixmaps, entry, px(34.0) as u32, density))
        {
            blit_centred(canvas, &icon, sx + box_size / 2.0, row_y + box_size / 2.0);
        }
        layout
            .hits
            .push((rect(sx, row_y, box_size, box_size), position));
        sx += box_size + gap;
    }
    if !pins.is_empty() && !recent.is_empty() {
        canvas.tint(
            (sx + px(8.0)) as usize,
            (row_y + px(10.0)) as usize,
            1,
            px(34.0) as usize,
            WHITE,
            0x1F,
        );
        sx += px(20.0);
    }
    if recent.is_empty() {
        return;
    }
    let right = x + inner;
    let card_w =
        ((right - sx - gap * (recent.len() - 1) as f32) / recent.len() as f32).max(px(120.0));
    for (n, (index, at)) in recent.iter().enumerate() {
        if sx + card_w > right + 1.0 {
            break;
        }
        let position = start + pins.len() + n;
        let Some(entry) = apps.get(*index) else {
            continue;
        };
        if position == selected {
            wash(canvas, (sx, row_y, card_w, box_size), px(12.0));
        }
        let icon_x = sx + px(10.0);
        if let Some(icon) = app_icon(icons, pixmaps, entry, px(30.0) as u32, density) {
            blit_centred(canvas, &icon, icon_x + px(15.0), row_y + box_size / 2.0);
        }
        let label_x = icon_x + px(30.0) + px(10.0);
        let (name_size, when_size) = (px(12.5), px(11.0));
        let block = name_size * 1.35 + when_size * 1.35;
        let top = row_y + (box_size - block) / 2.0;
        let name = fit(
            text,
            &entry.name,
            name_size,
            sx + card_w - px(8.0) - label_x,
        );
        text.draw(canvas, &name, name_size, label_x as i32, top as i32, TEXT);
        let when = ago(launcher.now().saturating_sub(*at));
        text.draw(
            canvas,
            &when,
            when_size,
            label_x as i32,
            (top + name_size * 1.35) as i32,
            TEXT_DIM,
        );
        layout
            .hits
            .push((rect(sx, row_y, card_w, box_size), position));
        sx += card_w + gap;
    }
}
