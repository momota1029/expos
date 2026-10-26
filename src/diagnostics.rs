//! 任意のCPU描画準備計測。GPU表示時間やFPSとは区別して記録する。
use std::time::{Duration, Instant};

pub struct FrameProfile {
    enabled: bool,
    live_output: bool,
    samples: Vec<FrameSample>,
    reports: Vec<String>,
    max_rows: usize,
    last_finish: Option<Instant>,
}

/// render 内で CPU を使う主要な段階。GPU submit / compositor の時間は含めない。
#[derive(Clone, Copy, Default)]
pub struct FramePhases {
    pub layout: Duration,
    pub measure_rows: Duration,
    pub measured_rows: usize,
    pub previews: Duration,
    pub rows: Duration,
    pub scroll_surface: Duration,
}

#[derive(Clone, Copy)]
struct FrameSample {
    total: f64,
    view_render_interval: Option<f64>,
    layout: f64,
    measure_rows: f64,
    measured_rows: usize,
    previews: f64,
    rows: f64,
    scroll_surface: f64,
}

fn milliseconds(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1000.0
}

fn percentiles(values: impl Iterator<Item = f64>) -> (f64, f64, f64) {
    let mut values: Vec<_> = values.collect();
    values.sort_by(f64::total_cmp);
    (
        values[values.len() / 2],
        values[values.len() * 95 / 100],
        values[values.len() - 1],
    )
}
impl Default for FrameProfile {
    fn default() -> Self {
        Self {
            enabled: std::env::var_os("GPUI_FILER_PROFILE").is_some(),
            // terminal への同期書込みは Windows で frame を詰まらせ得る。通常の計測は
            // view が閉じるまで貯め、必要なときだけ明示的に live 出力を使う。
            live_output: std::env::var_os("GPUI_FILER_PROFILE_LIVE").is_some(),
            samples: Vec::new(),
            reports: Vec::new(),
            max_rows: 0,
            last_finish: None,
        }
    }
}
impl FrameProfile {
    pub fn start(&self) -> Option<Instant> {
        self.enabled.then(Instant::now)
    }
    pub fn finish(&mut self, started: Option<Instant>, rows: usize, phases: FramePhases) {
        let Some(started) = started else {
            return;
        };
        self.finish_at(started, rows, phases, Instant::now());
    }

    fn finish_at(&mut self, started: Instant, rows: usize, phases: FramePhases, finished: Instant) {
        let view_render_interval = self
            .last_finish
            .replace(finished)
            .map(|last| milliseconds(finished.duration_since(last)));
        self.samples.push(FrameSample {
            total: milliseconds(started.elapsed()),
            view_render_interval,
            layout: milliseconds(phases.layout),
            measure_rows: milliseconds(phases.measure_rows),
            measured_rows: phases.measured_rows,
            previews: milliseconds(phases.previews),
            rows: milliseconds(phases.rows),
            scroll_surface: milliseconds(phases.scroll_surface),
        });
        self.max_rows = self.max_rows.max(rows);
        if self.samples.len() == 60 {
            let total = percentiles(self.samples.iter().map(|sample| sample.total));
            let view_render_interval = percentiles(
                self.samples
                    .iter()
                    .filter_map(|sample| sample.view_render_interval),
            );
            let layout = percentiles(self.samples.iter().map(|sample| sample.layout));
            let measure_rows = percentiles(self.samples.iter().map(|sample| sample.measure_rows));
            let previews = percentiles(self.samples.iter().map(|sample| sample.previews));
            let rows = percentiles(self.samples.iter().map(|sample| sample.rows));
            let scroll_surface =
                percentiles(self.samples.iter().map(|sample| sample.scroll_surface));
            let report = format!(
                "[v6-render-cpu] samples=60 rows_max={} total_ms={:.3}/{:.3}/{:.3} view_render_interval_ms={:.3}/{:.3}/{:.3} layout_ms={:.3}/{:.3}/{:.3} measure_rows_ms={:.3}/{:.3}/{:.3} measured_rows={} previews_ms={:.3}/{:.3}/{:.3} rows_ms={:.3}/{:.3}/{:.3} scroll_ui_ms={:.3}/{:.3}/{:.3}",
                self.max_rows,
                total.0,
                total.1,
                total.2,
                view_render_interval.0,
                view_render_interval.1,
                view_render_interval.2,
                layout.0,
                layout.1,
                layout.2,
                measure_rows.0,
                measure_rows.1,
                measure_rows.2,
                self.samples
                    .iter()
                    .map(|sample| sample.measured_rows)
                    .sum::<usize>(),
                previews.0,
                previews.1,
                previews.2,
                rows.0,
                rows.1,
                rows.2,
                scroll_surface.0,
                scroll_surface.1,
                scroll_surface.2,
            );
            if self.live_output {
                eprintln!("{report}");
            } else {
                self.reports.push(report);
            }
            self.samples.clear();
            self.max_rows = 0;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn view_render_interval_excludes_first_callback_and_reports_percentiles() {
        let mut profile = FrameProfile::default();
        profile.enabled = true;
        let base = Instant::now();

        for frame in 0..60 {
            let finished = base
                + Duration::from_millis(if frame == 59 {
                    58 * 16 + 100
                } else {
                    frame * 16
                });
            profile.finish_at(finished, 0, FramePhases::default(), finished);

            if frame == 0 {
                assert_eq!(profile.samples[0].view_render_interval, None);
            } else if frame == 1 {
                assert_eq!(profile.samples[1].view_render_interval, Some(16.0));
            }
        }

        assert!(profile.reports[0].contains("view_render_interval_ms=16.000/16.000/100.000"));
    }
}

impl Drop for FrameProfile {
    fn drop(&mut self) {
        if self.enabled && !self.live_output {
            for report in self.reports.drain(..) {
                eprintln!("{report}");
            }
        }
    }
}
