//! The list layout: one sheet of glass, front and centre.
//!
//! The search field across the top, a row of chips under it, and the grid
//! below that — every application before anything is typed, narrowed by the
//! chips to a category; the results as tiles, applications before files,
//! once something is. Under a hairline at the foot, the pinned and recently
//! used applications. The panel is the same size whatever it shows: a sheet
//! that grew and shrank with every keystroke would jump about under the eye,
//! and the grid keeps its three rows of room even when a search fills one.

use super::paint::*;
use super::*;
use crate::text::Weight;

/// The panel's width at a 1080p output, in logical pixels.
const WIDTH: f32 = 900.0;

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
    let text_color = crate::theme::text();
    let text_dim = crate::theme::text_dim();

    // The design's measurements, in logical pixels at 1080p. On a screen too
    // short for them the whole sheet is drawn smaller rather than cut off.
    let natural = 26.0 * 2.0 + 56.0 + 20.0 + 34.0 + 20.0 + 3.0 * 124.0 + 2.0 * 6.0
        + 30.0 + 16.0 + 1.0 + 16.0 + 28.0 + 58.0;
    let room = output.h() as f32 * m.density as f32 * 0.92;
    let squeeze = (room / (natural * m.scale)).min(1.0);
    let scale = m.scale * squeeze;
    let px = |v: f32| v * scale;

    let width = px(WIDTH)
        .min(output.w() as f32 * m.density as f32 - px(32.0))
        .max(px(420.0))
        .floor();
    let pad = px(26.0);
    let field_h = px(56.0);
    let chips_h = px(34.0);
    let inner = width - pad * 2.0;
    let gap = px(6.0);
    let tile_w = (inner - gap * (COLUMNS - 1) as f32) / COLUMNS as f32;
    let tile_h = px(124.0);
    let grid_h = GRID_ROWS as f32 * tile_h + (GRID_ROWS - 1) as f32 * gap;
    let pager_h = px(30.0);
    let section = px(16.0);
    let strip_head = px(28.0);
    let strip_h = px(58.0);
    let row_h = m.row * squeeze;
    let hint_size = px(11.5);

    let grid = launcher.is_grid();
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
    let first_row = launcher.first_row();
    // The result and run rows share the grid's room rather than adding to
    // the sheet: each takes the place of a row of tiles.
    let row_room = GRID_ROWS - usize::from(has_result) - usize::from(has_command);
    let drawn_rows = total_rows.saturating_sub(first_row).min(row_room.max(1));
    let nothing = tiles.is_empty() && !has_result && !has_command;
    let menu = launcher
        .menu()
        .and(launcher.selection())
        .and_then(|i| apps.get(i))
        .map(|entry| launcher.menu_items(entry));
    let menu_metrics = m.with_width(width as usize);

    let height = (pad * 2.0
        + field_h
        + px(20.0)
        + chips_h
        + px(20.0)
        + grid_h
        + pager_h
        + section
        + 1.0
        + section
        + strip_head
        + strip_h)
        .ceil() as usize;
    let width_px = width as usize;

    let mut canvas = Canvas::new(width_px, height);
    let mut layout = Layout {
        size: (width_px as i32, height as i32),
        ..Layout::default()
    };
    let rect =
        |x: f32, y: f32, w: f32, h: f32| Rect::from_xywh(x as i32, y as i32, w as i32, h as i32);
    let accent = crate::theme::accent();
    // Bare, only the bar is glass. The canvas keeps the full sheet's size so
    // the bar sits exactly where the sheet's field will be once it opens, and
    // [`placement`] needs no second idea of where the launcher goes.
    let compact = launcher.is_compact();
    let glass_h = if compact {
        (pad * 2.0 + field_h).ceil() as usize
    } else {
        height
    };
    canvas.material(
        0,
        0,
        width_px,
        glass_h,
        px(RADIUS + 6.0),
        crate::theme::panel_alpha(),
    );
    if compact {
        layout.surfaces.push(rect(0.0, 0.0, width, glass_h as f32));
    }

    // The field: a pill of lighter glass, ringed faintly. The caret is the
    // accent, since the field always has the keyboard.
    let (fx, fy, fw) = (pad, pad, inner);
    let field_radius = px(18.0);
    canvas.fill_rounded(
        fx as usize,
        fy as usize,
        fw as usize,
        field_h as usize,
        field_radius,
        crate::theme::well(),
    );
    canvas.stroke_rounded(
        fx as usize,
        fy as usize,
        fw as usize,
        field_h as usize,
        field_radius,
        1.0_f32.max(scale * 0.75),
        crate::theme::hairline(),
    );
    let glyph_x = fx + px(26.0);
    draw_search_glyph(
        &mut canvas,
        glyph_x,
        fy + field_h / 2.0 - px(1.5),
        px(7.0),
        px(1.8),
        text_dim,
    );
    // A keycap at the far end says how to leave.
    let cap_size = px(12.0);
    let cap = "Esc";
    let cap_w = text.measure(cap, cap_size).0 + px(18.0);
    let cap_h = px(26.0);
    let cap_x = fx + fw - px(14.0) - cap_w;
    let cap_y = fy + (field_h - cap_h) / 2.0;
    canvas.fill_rounded(
        cap_x as usize,
        cap_y as usize,
        cap_w as usize,
        cap_h as usize,
        px(8.0),
        crate::theme::well_raised(),
    );
    draw_centred(
        text,
        &mut canvas,
        cap,
        cap_size,
        cap_x + cap_w / 2.0,
        cap_y + (cap_h - cap_size * 1.35) / 2.0,
        text_dim,
        Weight::NORMAL,
    );
    // With nothing typed, a pill beside it opens the grid, or folds it back.
    let mut field_end = cap_x;
    if launcher.is_grid() {
        let pill_size = px(12.5);
        let pill_label = if compact { "All apps" } else { "Less" };
        let pill_w = text.measure(pill_label, pill_size).0 + px(38.0);
        let pill_x = cap_x - px(8.0) - pill_w;
        canvas.fill_rounded(
            pill_x as usize,
            cap_y as usize,
            pill_w as usize,
            cap_h as usize,
            cap_h / 2.0,
            crate::theme::well_raised(),
        );
        text.draw(
            &mut canvas,
            pill_label,
            pill_size,
            (pill_x + px(12.0)) as i32,
            (cap_y + (cap_h - pill_size * 1.35) / 2.0) as i32,
            text_color,
        );
        draw_chevron(
            &mut canvas,
            pill_x + pill_w - px(14.0),
            cap_y + cap_h / 2.0,
            px(3.5),
            px(1.5),
            !compact,
            text_dim,
        );
        layout
            .buttons
            .push((rect(pill_x, cap_y, pill_w, cap_h), Button::Expand));
        field_end = pill_x;
    }
    let query_size = px(18.0);
    let text_x = glyph_x + px(22.0);
    let text_y = fy + (field_h - query_size * 1.35) / 2.0;
    let text_room = field_end - px(12.0) - text_x;
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
        let hint = fit(text, PLACEHOLDER, query_size, text_room - px(8.0));
        text.draw(
            &mut canvas,
            &hint,
            query_size,
            (text_x + caret_w + px(6.0)) as i32,
            text_y as i32,
            text_dim,
        );
    } else {
        let shown = fit_tail(text, launcher.query(), query_size, text_room - px(6.0));
        text.draw(
            &mut canvas,
            &shown,
            query_size,
            text_x as i32,
            text_y as i32,
            text_color,
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

    if compact {
        return (canvas, layout);
    }

    // The chips: categories before anything is typed, kinds of result
    // after, each with how many the search found. The sort at the far end.
    let mut y = pad + field_h + px(20.0);
    let chip_size = px(13.5);
    let count_size = px(11.0);
    let mut cx = pad;
    let chips: Vec<(String, Option<String>, bool, Button)> = if grid {
        launcher
            .categories()
            .iter()
            .enumerate()
            .map(|(i, category)| {
                let label = match category {
                    Category::All => "All",
                    other => other.label(),
                };
                (
                    label.to_owned(),
                    None,
                    i == launcher.category(),
                    Button::Category(i),
                )
            })
            .collect()
    } else {
        let (apps_found, files_found) = launcher.found();
        Filter::ALL
            .into_iter()
            .map(|filter| {
                let count = match filter {
                    Filter::All => apps_found + files_found,
                    Filter::Apps => apps_found,
                    Filter::Files => files_found,
                };
                (
                    filter.label().to_owned(),
                    Some(count.to_string()),
                    launcher.filter() == filter,
                    Button::Filter(filter),
                )
            })
            .collect()
    };
    let sort_size = px(13.0);
    let sort_label = launcher.sort().label();
    let lead_w = text.measure("Sort", sort_size).0;
    let label_w = text.measure_weighted(sort_label, sort_size, EMPHASIS).0;
    let sort_w = lead_w + px(6.0) + label_w + px(18.0);
    let sort_x = pad + inner - sort_w - px(4.0);
    for (label, count, on, button) in &chips {
        let label_w = text.measure(label, chip_size).0;
        let count_w = count
            .as_deref()
            .map_or(0.0, |c| text.measure(c, count_size).0 + px(7.0));
        let chip_w = px(18.0) + label_w + count_w + px(18.0);
        if cx + chip_w > sort_x - px(12.0) {
            break;
        }
        let (fill, edge) = if *on {
            let ink = crate::theme::ink();
            (faded(ink, 0.26), Some(faded(ink, 0.34)))
        } else {
            (crate::theme::well(), None)
        };
        canvas.fill_rounded(
            cx as usize,
            y as usize,
            chip_w as usize,
            chips_h as usize,
            chips_h / 2.0,
            fill,
        );
        if let Some(edge) = edge {
            canvas.stroke_rounded(
                cx as usize,
                y as usize,
                chip_w as usize,
                chips_h as usize,
                chips_h / 2.0,
                1.0_f32.max(scale * 0.75),
                edge,
            );
        }
        let label_y = y + (chips_h - chip_size * 1.35) / 2.0;
        text.draw(
            &mut canvas,
            label,
            chip_size,
            (cx + px(18.0)) as i32,
            label_y as i32,
            if *on { text_color } else { text_dim },
        );
        if let Some(count) = count {
            text.draw(
                &mut canvas,
                count,
                count_size,
                (cx + px(18.0) + label_w + px(7.0)) as i32,
                (label_y + (chip_size - count_size) * 1.1) as i32,
                text_dim,
            );
        }
        layout
            .buttons
            .push((rect(cx, y, chip_w, chips_h), *button));
        cx += chip_w + px(8.0);
    }
    let sort_y = y + (chips_h - sort_size * 1.35) / 2.0;
    text.draw(
        &mut canvas,
        "Sort",
        sort_size,
        sort_x as i32,
        sort_y as i32,
        text_dim,
    );
    text.draw_weighted(
        &mut canvas,
        sort_label,
        sort_size,
        (sort_x + lead_w + px(6.0)) as i32,
        sort_y as i32,
        text_color,
        EMPHASIS,
    );
    draw_chevron(
        &mut canvas,
        sort_x + sort_w - px(5.0),
        sort_y + sort_size * 0.72,
        px(3.5),
        px(1.5),
        false,
        text_dim,
    );
    layout.buttons.push((
        rect(sort_x - px(6.0), y, sort_w + px(12.0), chips_h),
        Button::Sort,
    ));
    y += chips_h + px(20.0);

    // The grid's room, whatever fills it.
    let grid_top = y;
    let style = RowStyle {
        pad,
        inner,
        row: row_h,
        size: m.size * squeeze,
        scale,
    };
    if has_result && let Some(value) = launcher.result() {
        let ry = y + (tile_h - row_h) / 2.0;
        layout.hits.push((rect(pad, ry, inner, row_h), 0));
        glyph_row(
            &mut canvas,
            text,
            &style,
            ry,
            RESULT_GLYPH,
            value,
            target == Some(Target::Result),
        );
        y += tile_h + gap;
    }

    // The highlight under the tiles, drawn once, where its slide has got to,
    // rather than by the tile it belongs to — mid-slide it is between two.
    let selected_tile = launcher
        .selected()
        .checked_sub(tile_start)
        .filter(|n| *n < tiles.len())
        .map(|n| (n % COLUMNS, n / COLUMNS))
        .filter(|(_, row)| (first_row..first_row + drawn_rows).contains(row))
        .map(|(column, row)| {
            rect(
                pad + (tile_w + gap) * column as f32,
                y + (tile_h + gap) * (row - first_row) as f32,
                tile_w,
                tile_h,
            )
        });
    if let Some(target) = selected_tile {
        let at = launcher.highlight_rect(target);
        draw_wash(&mut canvas, at, px(20.0), scale);
        layout.highlight = Some(at);
    }

    let icon_size = px(64.0) as u32;
    for (n, tile) in tiles
        .iter()
        .enumerate()
        .skip(first_row * COLUMNS)
        .take(drawn_rows * COLUMNS)
    {
        let (column, row) = (n % COLUMNS, n / COLUMNS - first_row);
        let x = pad + (tile_w + gap) * column as f32;
        let ty = y + (tile_h + gap) * row as f32;
        let position = tile_start + n;
        layout.hits.push((rect(x, ty, tile_w, tile_h), position));
        match *tile {
            Target::App(index) => {
                let Some(entry) = apps.get(index) else {
                    continue;
                };
                let icon = app_icon(icons, pixmaps, entry, icon_size, density);
                let sub = (!grid).then(|| kind_of(entry).map(str::to_owned)).flatten();
                draw_tile(
                    &mut canvas,
                    text,
                    (x, ty, tile_w, tile_h),
                    scale,
                    icon.as_ref(),
                    &entry.name,
                    sub.as_deref(),
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
                    false,
                );
            }
            Target::Command | Target::Result => {}
        }
    }
    if drawn_rows > 0 && !tiles.is_empty() {
        y += drawn_rows as f32 * (tile_h + gap);
    }
    if nothing {
        let note = if grid {
            "Nothing installed in this category."
        } else if launcher.filter() != Filter::All {
            "Nothing of this kind. Ctrl ← shows everything."
        } else {
            "No matches. Try a shorter word."
        };
        draw_centred(
            text,
            &mut canvas,
            note,
            px(14.0),
            width / 2.0,
            grid_top + grid_h / 2.0 - px(10.0),
            text_dim,
            Weight::NORMAL,
        );
    }
    if has_command {
        let label = fit(
            text,
            &format!("Run \"{}\"", launcher.query()),
            style.size,
            inner - px(60.0),
        );
        let ry = y + (tile_h - row_h) / 2.0;
        layout
            .hits
            .push((rect(pad, ry, inner, row_h), visible.len() - 1));
        glyph_row(
            &mut canvas,
            text,
            &style,
            ry,
            COMMAND_GLYPH,
            &label,
            target == Some(Target::Command),
        );
    }

    // The pager, centred under the grid, when there is more than fits.
    let pager_top = grid_top + grid_h;
    if total_rows > row_room && launcher.menu().is_none() {
        let label = format!(
            "{}–{} of {}",
            first_row * COLUMNS + 1,
            ((first_row + drawn_rows) * COLUMNS).min(tiles.len()),
            tiles.len()
        );
        let label_w = text.measure(&label, hint_size).0;
        let label_y = pager_top + (pager_h - hint_size * 1.35) / 2.0;
        draw_centred(
            text,
            &mut canvas,
            &label,
            hint_size,
            width / 2.0,
            label_y,
            text_dim,
            Weight::NORMAL,
        );
        let arrow = px(26.0);
        for (direction, ax) in [
            (-1, width / 2.0 - label_w / 2.0 - px(14.0) - arrow),
            (1, width / 2.0 + label_w / 2.0 + px(14.0)),
        ] {
            let can = if direction < 0 {
                first_row > 0
            } else {
                first_row + drawn_rows < total_rows
            };
            let cy = pager_top + pager_h / 2.0;
            canvas.fill_rounded(
                ax as usize,
                (cy - arrow / 2.0) as usize,
                arrow as usize,
                arrow as usize,
                arrow / 2.0,
                crate::theme::well(),
            );
            draw_chevron_sideways(
                &mut canvas,
                ax + arrow / 2.0,
                cy,
                px(4.0),
                px(1.6),
                direction < 0,
                if can { text_color } else { faded(text_dim, 0.4) },
            );
            layout.buttons.push((
                rect(ax, cy - arrow / 2.0, arrow, arrow),
                Button::Page(direction as isize),
            ));
        }
    }

    // The foot, under a hairline.
    let foot = pager_top + pager_h + section;
    let rule = crate::theme::rule();
    canvas.tint(
        pad as usize,
        foot as usize,
        inner as usize,
        1,
        rule,
        rule.to_rgba_bytes()[3].saturating_mul(2),
    );
    let foot = foot + 1.0 + section;
    if grid {
        draw_strip(
            &mut canvas,
            text,
            icons,
            pixmaps,
            &mut layout,
            launcher,
            apps,
            (pad, foot, inner),
            scale,
            density,
        );
    } else {
        let hints = hints_for(target, launcher.menu().is_some(), grid);
        draw_hints(
            &mut canvas,
            text,
            hints,
            Align::Right(pad + inner - px(4.0)),
            foot + (strip_head + strip_h - hint_size * 1.75) / 2.0,
            hint_size,
            scale,
            crate::theme::well_raised(),
            None,
        );
    }

    // The actions menu, over the bottom of the grid against the right edge.
    if let (Some(item), Some(entry), Some(items)) = (
        launcher.menu(),
        launcher.selection().and_then(|i| apps.get(i)),
        &menu,
    ) {
        draw_menu(
            &mut canvas,
            text,
            &mut layout,
            &menu_metrics,
            &entry.name,
            items,
            item,
            pager_top + pager_h,
            pad + field_h + section,
        );
    }

    (canvas, layout)
}

/// A chevron pointing left or right, centred on (`cx`, `cy`): the pager's.
fn draw_chevron_sideways(
    canvas: &mut Canvas,
    cx: f32,
    cy: f32,
    size: f32,
    thickness: f32,
    left: bool,
    color: crate::theme::Color,
) {
    let tip = if left { -size * 0.5 } else { size * 0.5 };
    let a = (cx - tip, cy - size);
    let b = (cx + tip, cy);
    let c = (cx - tip, cy + size);
    let reach = size + thickness * 2.0;
    canvas.paint(
        (cx - reach) as i32,
        (cy - reach) as i32,
        (reach * 2.0) as i32 + 1,
        (reach * 2.0) as i32 + 1,
        |x, y| {
            let d = segment_distance(x, y, a, b).min(segment_distance(x, y, b, c));
            let coverage = edge(d - thickness / 2.0);
            (coverage > 0.0).then_some((color, coverage))
        },
    );
}

/// The highlight: a well of lighter glass edged with the accent.
fn draw_wash(canvas: &mut Canvas, at: Rect, radius: f32, scale: f32) {
    let (x, y) = (at.x().max(0) as usize, at.y().max(0) as usize);
    let (w, h) = (at.w().max(0) as usize, at.h().max(0) as usize);
    canvas.fill_rounded(x, y, w, h, radius, crate::theme::well_raised());
    canvas.stroke_rounded(
        x,
        y,
        w,
        h,
        radius,
        1.0_f32.max(scale),
        faded(crate::theme::accent(), 0.55),
    );
}

/// A tile of the grid: a large icon with its name under it, and in a search
/// a quieter line under that — what it is, or where a file lives. No box
/// around it until it is highlighted: a grid of boxes reads as a form to be
/// filled in, a grid of icons as a place to pick from. The highlight is drawn
/// under it separately (see [`draw_wash`]), and a dot sits on the icon when
/// the application is pinned.
#[allow(clippy::too_many_arguments)]
fn draw_tile(
    canvas: &mut Canvas,
    text: &mut Text,
    (x, y, w, _): (f32, f32, f32, f32),
    scale: f32,
    icon: Option<&raven_desktop::Pixmap>,
    title: &str,
    sub: Option<&str>,
    pinned: bool,
) {
    let px = |v: f32| v * scale;
    let accent = crate::theme::accent();
    let cx = x + w / 2.0;
    let icon_box = px(64.0);
    let top = y + px(if sub.is_some() { 10.0 } else { 16.0 });
    if let Some(icon) = icon {
        blit_centred(canvas, icon, cx, top + icon_box / 2.0);
    }
    if pinned {
        glow_dot(
            canvas,
            cx + icon_box / 2.0 - px(3.0),
            top + px(3.0),
            px(3.5),
            px(4.5),
            accent,
            faded(crate::theme::background(), 0.9),
        );
    }
    let title_size = px(13.5);
    let title = fit(text, title, title_size, w - px(14.0));
    let title_y = top + icon_box + px(8.0);
    draw_centred(
        text,
        canvas,
        &title,
        title_size,
        cx,
        title_y,
        crate::theme::text(),
        Weight::NORMAL,
    );
    if let Some(sub) = sub {
        let sub_size = px(11.0);
        let sub = fit(text, sub, sub_size, w - px(14.0));
        draw_centred(
            text,
            canvas,
            &sub,
            sub_size,
            cx,
            title_y + title_size * 1.35,
            crate::theme::text_dim(),
            Weight::NORMAL,
        );
    }
}

/// The foot of the opened list: "Pinned & Recent" with a link to the pin
/// bar, then the pinned applications as icons and, past a divider, the
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
    let start = launcher.suggested().len();
    let (pins, recent) = (launcher.pins_shown(), launcher.recent());
    let selected = launcher.selected();
    let on_strip = (start..start + pins.len() + recent.len()).contains(&selected);

    // The heading names what the highlight is on, since the icons have no
    // labels of their own.
    let head_size = px(13.5);
    let head = "Pinned & Recent";
    text.draw_weighted(
        canvas,
        head,
        head_size,
        (x + px(4.0)) as i32,
        y as i32,
        crate::theme::text(),
        EMPHASIS,
    );
    // A shortcut to the pin bar — drawn only when something is pinned,
    // because the bar itself only exists then: with nothing on it there is
    // nothing to link to, and a link that opens nothing is worse than none.
    let link = "Pin bar →";
    let link_size = px(12.0);
    let link_w = if pins.is_empty() {
        0.0
    } else {
        text.measure(link, link_size).0
    };
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
        text.draw(canvas, &name, head_size, name_x as i32, y as i32, crate::theme::text_dim());
    }
    if !pins.is_empty() {
        text.draw(canvas, link, link_size, link_x as i32, y as i32, crate::theme::text_dim());
        layout.buttons.push((
            rect(link_x - px(6.0), y - px(4.0), link_w + px(12.0), px(22.0)),
            Button::PinnedPanel,
        ));
    }

    let row_y = y + px(28.0);
    let box_size = px(56.0);
    let gap = px(4.0);
    let wash = |canvas: &mut Canvas,
                layout: &mut Layout,
                (bx, by, bw, bh): (f32, f32, f32, f32),
                radius: f32| {
        let at = launcher.highlight_rect(rect(bx, by, bw, bh));
        draw_wash(canvas, at, radius, scale);
        layout.highlight = Some(at);
    };
    let mut sx = x + px(2.0);
    for (n, index) in pins.iter().enumerate() {
        let position = start + n;
        if position == selected {
            wash(canvas, layout, (sx, row_y, box_size, box_size), px(14.0));
        }
        if let Some(icon) = apps
            .get(*index)
            .and_then(|entry| app_icon(icons, pixmaps, entry, px(40.0) as u32, density))
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
            crate::theme::rule(),
            crate::theme::rule().to_rgba_bytes()[3].saturating_mul(2),
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
            wash(canvas, layout, (sx, row_y, card_w, box_size), px(12.0));
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
        text.draw(canvas, &name, name_size, label_x as i32, top as i32, crate::theme::text());
        let when = ago(launcher.now().saturating_sub(*at));
        text.draw(
            canvas,
            &when,
            when_size,
            label_x as i32,
            (top + name_size * 1.35) as i32,
            crate::theme::text_dim(),
        );
        layout
            .hits
            .push((rect(sx, row_y, card_w, box_size), position));
        sx += card_w + gap;
    }
}
