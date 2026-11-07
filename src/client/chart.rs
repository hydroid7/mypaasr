//! Text charts for the terminal: multi-row bar charts, sparklines and an
//! uptime strip. Pure functions over plain values, so they are easy to test.

use super::{DIM, GREEN, Output, RED, YELLOW, paint};

const EIGHTHS: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
/// Width of the y-axis labels left of a chart.
pub const AXIS: usize = 8;

/// How several buckets are combined into one column when there are more
/// buckets than columns.
#[derive(Debug, Clone, Copy)]
pub enum Agg {
    Mean,
    Max,
    Min,
}

/// Groups `values` into at most `cols` columns. Missing values are skipped; a
/// column with no value stays missing.
pub fn resample(values: &[Option<f64>], cols: usize, agg: Agg) -> Vec<Option<f64>> {
    if cols == 0 || values.len() <= cols {
        return values.to_vec();
    }
    let group = values.len().div_ceil(cols);
    values
        .chunks(group)
        .map(|chunk| {
            let present: Vec<f64> = chunk.iter().flatten().copied().collect();
            if present.is_empty() {
                return None;
            }
            Some(match agg {
                Agg::Mean => present.iter().sum::<f64>() / present.len() as f64,
                Agg::Max => present.iter().copied().fold(f64::MIN, f64::max),
                Agg::Min => present.iter().copied().fold(f64::MAX, f64::min),
            })
        })
        .collect()
}

/// A bar chart `height` rows high, each value `col_width` characters wide, with
/// the top of the scale and zero labelled on the left by `label`. Rows are
/// returned top first.
pub fn bars(values: &[Option<f64>], height: usize, col_width: usize, label: impl Fn(f64) -> String) -> Vec<String> {
    let height = height.max(1);
    let max = values.iter().flatten().copied().fold(0.0, f64::max);
    let levels: Vec<Option<usize>> = values
        .iter()
        .map(|v| v.map(|x| if max > 0.0 { ((x / max) * (height * 8) as f64).round() as usize } else { 0 }))
        .collect();
    (0..height)
        .map(|row| {
            let base = (height - 1 - row) * 8;
            let axis = if row == 0 {
                format!("{:>w$} ┤", label(max), w = AXIS - 2)
            } else if row == height - 1 {
                format!("{:>w$} ┤", label(0.0), w = AXIS - 2)
            } else {
                format!("{:>w$}┤", "", w = AXIS - 1)
            };
            let body: String = levels
                .iter()
                .map(|l| {
                    let c = match l {
                        // Nothing measured: a faint dot on the baseline.
                        None => {
                            if row == height - 1 {
                                '·'
                            } else {
                                ' '
                            }
                        }
                        Some(l) => match l.saturating_sub(base).min(8) {
                            0 => ' ',
                            n => EIGHTHS[n - 1],
                        },
                    };
                    c.to_string().repeat(col_width)
                })
                .collect();
            format!("{axis}{body}")
        })
        .collect()
}

/// One row of time labels under a chart, placed where they fit without overlapping.
pub fn time_axis(times: &[u64], col_width: usize, label: impl Fn(u64) -> String) -> String {
    let width = times.len() * col_width;
    let mut line: Vec<char> = vec![' '; width];
    let mut next_free = 0;
    for (i, t) in times.iter().enumerate() {
        let pos = i * col_width;
        let text: Vec<char> = label(*t).chars().collect();
        if pos >= next_free && pos + text.len() <= width {
            line[pos..pos + text.len()].copy_from_slice(&text);
            next_free = pos + text.len() + 2;
        }
    }
    format!("{:>w$}└{}", "", line.into_iter().collect::<String>(), w = AXIS - 1)
}

/// `▁▂▃▅▇` in one row, scaled to the largest value; missing values are blank.
pub fn sparkline(values: &[Option<f64>]) -> String {
    let max = values.iter().flatten().copied().fold(0.0, f64::max);
    values
        .iter()
        .map(|v| match v {
            None => ' ',
            Some(x) if max <= 0.0 || *x <= 0.0 => '▁',
            Some(x) => EIGHTHS[(((x / max) * 7.0).round() as usize).min(7)],
        })
        .collect()
}

/// One cell of the uptime strip, from the share of samples with any replica
/// running (`up`) and with all of them running (`full`).
pub fn uptime_cell(out: Output, up: Option<f64>, full: Option<f64>, width: usize) -> String {
    let (c, color) = match (up, full) {
        (None, _) => ('·', DIM),
        (Some(u), Some(f)) if u >= 0.999 && f >= 0.999 => ('█', GREEN),
        (Some(u), _) if u >= 0.999 => ('▆', YELLOW),
        (Some(u), _) if u > 0.0 => ('▃', RED),
        _ => ('▁', RED),
    };
    paint(out, color, &c.to_string().repeat(width))
}

pub fn uptime_legend(out: Output) -> String {
    format!(
        "{} all replicas up  {} some replicas down  {} partly down  {} down  {} no data",
        paint(out, GREEN, "█"),
        paint(out, YELLOW, "▆"),
        paint(out, RED, "▃"),
        paint(out, RED, "▁"),
        paint(out, DIM, "·"),
    )
}

/// `950`, `1.2k`, `3.4M`.
pub fn compact(v: f64) -> String {
    match v {
        v if v >= 1e6 => format!("{:.1}M", v / 1e6),
        v if v >= 1e4 => format!("{:.0}k", v / 1e3),
        v if v >= 1e3 => format!("{:.1}k", v / 1e3),
        v if v >= 10.0 || v == 0.0 => format!("{v:.0}"),
        v => format!("{v:.1}"),
    }
}

/// `99.98%`, or `-` when unknown.
pub fn percent(v: Option<f64>) -> String {
    match v {
        None => "-".into(),
        Some(x) if x >= 1.0 => "100%".into(),
        Some(x) => format!("{:.2}%", x * 100.0),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resampling() {
        let v = [Some(1.0), Some(3.0), None, None, Some(5.0)];
        assert_eq!(resample(&v, 10, Agg::Mean), v.to_vec(), "fits: unchanged");
        assert_eq!(resample(&v, 3, Agg::Mean), vec![Some(2.0), None, Some(5.0)]);
        assert_eq!(resample(&v, 3, Agg::Max), vec![Some(3.0), None, Some(5.0)]);
        assert_eq!(resample(&v, 3, Agg::Min), vec![Some(1.0), None, Some(5.0)]);
    }

    #[test]
    fn bar_chart_scales_to_the_largest_value() {
        let rows = bars(&[Some(0.0), Some(4.0), Some(8.0), None], 2, 1, |v| format!("{v}"));
        assert_eq!(rows.len(), 2);
        // 8 is full height (two full rows), 4 is half (one full bottom row).
        assert!(rows[0].ends_with("  █ "), "{:?}", rows[0]);
        assert!(rows[1].ends_with(" ██·"), "{:?}", rows[1]);
        assert!(rows[0].starts_with("     8 ┤"));
        assert!(rows[1].starts_with("     0 ┤"));
        let wide = bars(&[Some(1.0)], 1, 3, |_| String::new());
        assert!(wide[0].ends_with("███"));
    }

    #[test]
    fn axis_labels_do_not_overlap() {
        let line = time_axis(&[0, 1, 2, 3, 4, 5, 6, 7], 1, |t| format!("t{t}"));
        assert_eq!(line, format!("{}└t0  t4  ", " ".repeat(AXIS - 1)));
    }

    #[test]
    fn sparkline_and_numbers() {
        assert_eq!(sparkline(&[Some(0.0), Some(1.0), None, Some(2.0)]), "▁▅ █");
        assert_eq!(compact(950.0), "950");
        assert_eq!(compact(1234.0), "1.2k");
        assert_eq!(compact(56_000.0), "56k");
        assert_eq!(compact(2.5), "2.5");
        assert_eq!(percent(Some(0.99987)), "99.99%");
        assert_eq!(percent(Some(1.0)), "100%");
        assert_eq!(percent(None), "-");
    }

    #[test]
    fn uptime_cells() {
        let o = Output::default();
        assert_eq!(uptime_cell(o, Some(1.0), Some(1.0), 2), "██");
        assert_eq!(uptime_cell(o, Some(1.0), Some(0.5), 1), "▆");
        assert_eq!(uptime_cell(o, Some(0.5), Some(0.5), 1), "▃");
        assert_eq!(uptime_cell(o, Some(0.0), Some(0.0), 1), "▁");
        assert_eq!(uptime_cell(o, None, None, 1), "·");
    }
}
