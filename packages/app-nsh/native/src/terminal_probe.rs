//! Fixed-grid drawing experiments, independent of VT parsing and daemon connections.

use nsh_base::theme;
use nsh_core::{
    canvas::CanvasTextBaseline,
    color::{Hsla, rgb, rgba},
    prelude::*,
};
use serde::Deserialize;

const FONT: &str = "18px monospace";
const LABEL_FONT: &str = "12px sans-serif";
const COLUMNS: usize = 24;
const GRAPHEME_FIXTURES: &str = include_str!(
    "../../../app/modules/paseo-terminal/android/src/androidTest/assets/grapheme-rows.json"
);

#[derive(Debug, PartialEq)]
struct Cell {
    text: String,
    column: usize,
    span: usize,
}

#[derive(Debug, PartialEq)]
struct StyleSpan {
    column: usize,
    span: usize,
    foreground: u32,
    background: u32,
}

#[derive(Debug, PartialEq)]
struct Row {
    text: String,
    columns: usize,
    cells: Vec<Cell>,
    styles: Vec<StyleSpan>,
}

/// Decodes Paseo's native row contract; offsets count UTF-16 units, never UTF-8 bytes.
fn decode_row(source: &str) -> Result<Row, &'static str> {
    let mut sections = source.splitn(3, ';');
    let numbers = |section: &str| -> Result<Vec<u32>, &'static str> {
        section
            .split(',')
            .map(|part| part.parse().map_err(|_| "invalid row number"))
            .collect()
    };
    let cell_data = numbers(sections.next().ok_or("missing cells")?)?;
    let style_data = numbers(sections.next().ok_or("missing styles")?)?;
    let text = sections.next().ok_or("missing text")?;
    if cell_data.len() % 2 != 0 || style_data.len() % 4 != 0 {
        return Err("incomplete cell or style tuple");
    }
    let units: Vec<u16> = text.encode_utf16().collect();
    let mut offset = 0usize;
    let mut column = 0usize;
    let mut cells = Vec::new();
    for pair in cell_data.chunks_exact(2) {
        let length = pair[0] as usize;
        let span = pair[1] as usize;
        if length == 0 || span == 0 || span > COLUMNS {
            return Err("invalid cell length or span");
        }
        let end = offset.checked_add(length).ok_or("cell offset overflow")?;
        let encoded = units.get(offset..end).ok_or("cell exceeds row text")?;
        let cell_text =
            String::from_utf16(encoded).map_err(|_| "cell splits a UTF-16 surrogate pair")?;
        cells.push(Cell {
            text: cell_text,
            column,
            span,
        });
        offset = end;
        column += span;
        if column > COLUMNS {
            return Err("row exceeds probe columns");
        }
    }
    if offset != units.len() {
        return Err("unclaimed row text");
    }
    let mut style_column = 0usize;
    let mut styles = Vec::new();
    for tuple in style_data.chunks_exact(4) {
        let span = tuple[0] as usize;
        if span == 0 || span > column.saturating_sub(style_column) {
            return Err("style exceeds row columns");
        }
        if tuple[3] != 0 {
            return Err("style flags are outside this drawing probe");
        }
        styles.push(StyleSpan {
            column: style_column,
            span,
            foreground: tuple[1],
            background: tuple[2],
        });
        style_column += span;
    }
    if style_column != column {
        return Err("styles do not cover row columns");
    }
    Ok(Row {
        text: text.to_owned(),
        columns: column,
        cells,
        styles,
    })
}

#[derive(Deserialize)]
struct GraphemeFixture {
    output: String,
    row: String,
}

fn row_source(cells: &[(&str, usize)], styles: &[(usize, u32, u32, u32)]) -> String {
    let lengths = cells
        .iter()
        .flat_map(|(text, span)| [text.encode_utf16().count().to_string(), span.to_string()])
        .collect::<Vec<_>>()
        .join(",");
    let styles = styles
        .iter()
        .flat_map(|&(span, fg, bg, flags)| [span as u32, fg, bg, flags])
        .map(|value| value.to_string())
        .collect::<Vec<_>>()
        .join(",");
    let text: String = cells.iter().map(|(text, _)| *text).collect();
    format!("{lengths};{styles};{text}")
}

fn fixtures() -> Result<Vec<(String, Row)>, String> {
    let fixture = |title: &str, cells: &[(&str, usize)], styles: &[(usize, u32, u32, u32)]| {
        decode_row(&row_source(cells, styles))
            .map(|row| (title.to_owned(), row))
            .map_err(str::to_owned)
    };
    let ruler: Vec<_> = "012345678901234567890123"
        .char_indices()
        .map(|(offset, _)| (&"012345678901234567890123"[offset..offset + 1], 1))
        .collect();
    let white = 0xffeeeeee;
    let mut rows = vec![
        fixture("ASCII 列标尺", &ruler, &[(COLUMNS, white, 0, 0)])?,
        fixture(
            "CJK / ZWJ / 旗帜（跨度来自 fixture）",
            &[
                ("A", 1),
                ("中", 2),
                ("文", 2),
                ("👨‍👩‍👧", 2),
                ("🇰🇷", 2),
                ("B", 1),
            ],
            &[(10, white, 0, 0)],
        )?,
        fixture(
            "组合音标（一个 cell 的多个码点）",
            &[("e\u{301}", 1), ("A\u{308}\u{301}", 1), ("Z", 1)],
            &[(3, white, 0, 0)],
        )?,
        fixture(
            "跨 cell 颜色的 conjunct：क् + ष",
            &[("क्", 1), ("ष", 1), (" ", 1), ("X", 1)],
            &[
                (1, 0xffff9966, 0xff31231e, 0),
                (1, 0xff66ddff, 0xff152c33, 0),
                (2, white, 0, 0),
            ],
        )?,
    ];
    let real: Vec<GraphemeFixture> =
        serde_json::from_str(GRAPHEME_FIXTURES).map_err(|error| error.to_string())?;
    for fixture in real {
        let row = decode_row(&fixture.row).map_err(str::to_owned)?;
        rows.push((format!("Paseo fixture: {}", fixture.output), row));
    }
    Ok(rows)
}

fn argb(color: u32) -> Hsla {
    rgba(color.rotate_left(8)).to_hsla()
}

fn rectangle(x: f32, y: f32, width: f32, height: f32) -> Bounds<Points> {
    Bounds::new(Point::new(px(x), px(y)), Size::new(px(width), px(height)))
}

fn paint_row(
    mut painter: Painter,
    row: &Row,
    x: f32,
    y: f32,
    cell: f32,
    height: f32,
    baseline: f32,
    whole_row: bool,
) -> Painter {
    for style in &row.styles {
        if style.background != 0 {
            painter = painter.fill_color(argb(style.background)).fill_rect(
                px(x + style.column as f32 * cell),
                px(y),
                px(style.span as f32 * cell),
                px(height),
            );
        }
    }
    // Fixed columns make selection geometry comparable even when glyph advances differ.
    painter = painter.fill_color(rgba(0x2684ff55).to_hsla()).fill_rect(
        px(x + cell),
        px(y),
        px(3.0 * cell),
        px(height),
    );
    if whole_row {
        for style in &row.styles {
            painter = painter
                .save()
                .begin_path()
                .rect(rectangle(
                    x + style.column as f32 * cell,
                    y,
                    style.span as f32 * cell,
                    height,
                ))
                .clip()
                .font(FONT)
                .fill_color(argb(style.foreground))
                .fill_text(&row.text, px(x), px(y + baseline))
                .restore();
        }
    } else {
        // Deliberately independent cell shaping: this is the comparison, not cluster equivalence.
        for item in &row.cells {
            let style = row
                .styles
                .iter()
                .find(|style| {
                    item.column >= style.column && item.column < style.column + style.span
                })
                .expect("validated style coverage");
            painter = painter
                .save()
                .begin_path()
                .rect(rectangle(
                    x + item.column as f32 * cell,
                    y,
                    item.span as f32 * cell,
                    height,
                ))
                .clip()
                .font(FONT)
                .fill_color(argb(style.foreground))
                .fill_text(
                    &item.text,
                    px(x + item.column as f32 * cell),
                    px(y + baseline),
                )
                .restore();
        }
    }
    for column in 0..=COLUMNS {
        painter = painter.fill_color(rgb(0x33404c).to_hsla()).fill_rect(
            px(x + column as f32 * cell),
            px(y),
            px(0.5),
            px(height),
        );
    }
    // Cursor is painted last, as a bar at the authoritative fixture column.
    painter.fill_color(rgb(0xffdd88).to_hsla()).fill_rect(
        px(x + 8.0 * cell),
        px(y),
        px(2.0),
        px(height),
    )
}

/// Compares fixed cell placement with whole-row shaping; no VT state or live terminal is involved.
#[component]
pub fn TerminalProbe() -> impl IntoElement {
    let rows = match fixtures() {
        Ok(rows) => rows,
        Err(error) => return view().child(text(format!("终端绘图 fixture 错误：{error}"))),
    };
    let metrics = text_metrics(FONT, "0");
    let cell = metrics.width.0.max(1.0);
    let height = metrics.height.0.max(18.0) + 8.0;
    let baseline = metrics.ascent.0 + 4.0;
    let gap = 28.0;
    let left = 12.0;
    let right = left + COLUMNS as f32 * cell + gap;
    let width = right + COLUMNS as f32 * cell + left;
    let step = height + 22.0;
    let total_height = 34.0 + rows.len() as f32 * step;
    let mut painter = Painter::new()
        .fill_color(rgb(0x0b0b0b).to_hsla())
        .fill_rect(px(0.0), px(0.0), px(width), px(total_height))
        .text_baseline(CanvasTextBaseline::Alphabetic)
        .font(LABEL_FONT)
        .fill_color(rgb(0xcccccc).to_hsla())
        .fill_text("按 cell 定位：独立塑形", px(left), px(20.0))
        .fill_text("整行塑形：自然字距，非网格布局", px(right), px(20.0));
    for (index, (title, row)) in rows.iter().enumerate() {
        let y = 34.0 + index as f32 * step;
        painter = painter
            .font(LABEL_FONT)
            .fill_color(rgb(0xcccccc).to_hsla())
            .fill_text(title, px(left), px(y + 12.0));
        painter = paint_row(painter, row, left, y + 18.0, cell, height, baseline, false);
        painter = paint_row(painter, row, right, y + 18.0, cell, height, baseline, true);
    }
    view().flex_1().bg(move || theme().background).p_3()
        .child(text("终端网格绘图 probe").text_color(move || theme().foreground))
        .child(text("固定数据；蓝色为列 1–3 的选择，黄色为列 8 光标。左右均不是完整终端渲染器。").text_color(move || theme().muted_foreground))
        .child(text("现有 Canvas 不提供整行 cluster 到终端列的重定位；此处对比两种局限。字重与斜体未纳入本 probe。").text_color(move || theme().muted_foreground))
        .child(scroll().flex_1().child(scroll().horizontal().child(canvas(painter).w(px(width)).h(px(total_height)).test_id("terminal-grid-probe"))))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn real_paseo_rows_keep_utf16_cell_boundaries_and_spans() {
        let real: Vec<GraphemeFixture> = serde_json::from_str(GRAPHEME_FIXTURES).unwrap();
        assert!(!real.is_empty());
        for fixture in real {
            let row = decode_row(&fixture.row).unwrap();
            assert_eq!(
                row.cells
                    .iter()
                    .map(|cell| cell.text.as_str())
                    .collect::<String>(),
                row.text
            );
            assert_eq!(row.columns, 8);
            assert!(row.text.starts_with(&fixture.output));
            assert_eq!(
                row.styles.iter().map(|style| style.span).sum::<usize>(),
                row.columns
            );
        }
    }

    #[test]
    fn utf16_surrogates_combining_text_and_wide_columns_are_independent() {
        let row = decode_row(&row_source(
            &[("A", 1), ("👨‍👩‍👧", 2), ("中", 2), ("e\u{301}", 1)],
            &[(6, 0xffaabbcc, 0, 0)],
        ))
        .unwrap();
        assert_eq!(
            row.cells
                .iter()
                .map(|cell| (cell.column, cell.span))
                .collect::<Vec<_>>(),
            vec![(0, 1), (1, 2), (3, 2), (5, 1)]
        );
        assert_eq!(row.cells[1].text, "👨‍👩‍👧");
        assert_eq!(row.cells[3].text, "e\u{301}");
        assert!(decode_row("1,1,1,1;2,4294967295,0,0;😀").is_err());
    }

    #[test]
    fn styles_can_change_inside_a_shaping_cluster_without_merging_cells() {
        let row = decode_row(&row_source(
            &[("क्", 1), ("ष", 1)],
            &[(1, 0xffff0000, 0, 0), (1, 0xff00ff00, 0, 0)],
        ))
        .unwrap();
        assert_eq!(row.text, "क्ष");
        assert_eq!(row.cells.len(), 2);
        assert_eq!(row.styles.len(), 2);
        assert_eq!(row.cells[1].column, 1);
        assert_eq!(argb(0x80204060), rgba(0x20406080).to_hsla());
    }

    #[test]
    fn invalid_tuple_boundaries_and_style_coverage_are_rejected() {
        for source in [
            "1;1,0,0,0;a",
            "1,0;1,0,0,0;a",
            "3,1;1,0,0,0;a",
            "1,1;1,0,0,0;ab",
            "1,1;2,0,0,0;a",
            "1,1;0,0,0,0;a",
            "1,1;1,0,0;a",
            "1,1;1,0,0,1;a",
        ] {
            assert!(decode_row(source).is_err(), "{source}");
        }
        assert_eq!(decode_row("1,1;1,4294967295,0,0;;").unwrap().text, ";");
        assert_eq!(fixtures().unwrap().len(), 10);
    }
}
