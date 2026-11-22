//! Linux の実ウィンドウでスクリーンショット用のシナリオを実行する。

mod capture;

use std::{
    cell::Cell,
    error::Error,
    fs,
    path::{Path, PathBuf},
    rc::Rc,
    time::{Duration, Instant},
};

use gpui::{
    App, AppContext as _, AsyncApp, Bounds, Keystroke, MouseButton, MouseDownEvent, MouseMoveEvent,
    MouseUpEvent, PlatformInput, Window, WindowBounds, WindowHandle, point, px, size,
};
use serde::Deserialize;

use crate::{AppIntent, ViewLayout, persistence::InitialWindow, shell::FilerShell};

type ShotResult<T> = Result<T, Box<dyn Error + Send + Sync>>;

const STEP_TIMEOUT: Duration = Duration::from_secs(15);
const POLL_INTERVAL: Duration = Duration::from_millis(16);
const DEFAULT_SIZE: (u32, u32) = (1280, 800);

#[derive(Deserialize)]
struct Scenario {
    steps: Vec<Step>,
}

#[derive(Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
enum Step {
    Size {
        width: u32,
        height: u32,
    },
    Theme {
        theme: Theme,
    },
    Wait {
        ms: u64,
    },
    WaitIdle,
    Navigate {
        path: PathBuf,
    },
    Key {
        #[serde(alias = "key", deserialize_with = "parse_keys")]
        keys: Vec<Keystroke>,
    },
    Click {
        x: u32,
        y: u32,
    },
    Select {
        name: String,
    },
    View {
        mode: ViewMode,
    },
    Screenshot {
        name: String,
    },
}

#[derive(Deserialize)]
#[serde(rename_all = "lowercase")]
enum Theme {
    Light,
    Dark,
}

#[derive(Deserialize)]
#[serde(rename_all = "lowercase")]
enum ViewMode {
    Rows,
    Tiles,
    Icons,
}

fn parse_keys<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Vec<Keystroke>, D::Error> {
    let input = String::deserialize(deserializer)?;
    let keys = input
        .split_whitespace()
        .map(|key| Keystroke::parse(&key.to_ascii_lowercase().replace('+', "-")))
        .collect::<Result<Vec<_>, _>>()
        .map_err(serde::de::Error::custom)?;
    if keys.is_empty() {
        return Err(serde::de::Error::custom("keys must contain a keystroke"));
    }
    Ok(keys)
}

pub(super) struct Plan {
    scenario: Scenario,
    output: PathBuf,
    fixture: PathBuf,
    initial_size: (u32, u32),
}

impl Plan {
    pub(super) fn from_process_args() -> ShotResult<Self> {
        let args = std::env::args_os().skip(2).collect::<Vec<_>>();
        let [scenario, output, fixture] = args.as_slice() else {
            return Err(
                "usage: expos --headless-shot <scenario.json> <out-dir> <fixture-dir>".into(),
            );
        };
        let scenario: Scenario = serde_json::from_reader(fs::File::open(scenario)?)?;
        let mut initial_size = None;
        for (index, step) in scenario.steps.iter().enumerate() {
            match step {
                Step::Size { width, height } => {
                    if !(1..=3840).contains(width) || !(1..=2160).contains(height) {
                        return Err(format!("step {}: size must fit 3840×2160", index + 1).into());
                    }
                    initial_size.get_or_insert((*width, *height));
                }
                Step::Navigate { path } if path.is_absolute() => {
                    return Err(
                        format!("step {}: navigate requires a relative path", index + 1).into(),
                    );
                }
                Step::Screenshot { name } if !valid_name(name) => {
                    return Err(
                        format!("step {}: invalid screenshot name {name:?}", index + 1).into(),
                    );
                }
                _ => {}
            }
        }
        if std::env::var_os("DISPLAY").is_none_or(|display| display.is_empty()) {
            return Err("an X11 display is required; use scripts/shot.sh".into());
        }
        let fixture = fs::canonicalize(fixture)?;
        if !fixture.is_dir() {
            return Err(format!("fixture is not a directory: {}", fixture.display()).into());
        }
        fs::create_dir_all(output)?;
        Ok(Self {
            scenario,
            output: fs::canonicalize(output)?,
            fixture,
            initial_size: initial_size.unwrap_or(DEFAULT_SIZE),
        })
    }

    pub(super) fn fixture(&self) -> &Path {
        &self.fixture
    }

    pub(super) fn initial_window(&self) -> InitialWindow {
        let (width, height) = self.initial_size;
        InitialWindow {
            bounds: WindowBounds::Windowed(Bounds::new(
                point(px(0.), px(0.)),
                size(px(width as f32), px(height as f32)),
            )),
            display: None,
        }
    }

    pub(super) fn start(self, handle: WindowHandle<FilerShell>, exit: Rc<Cell<i32>>, cx: &App) {
        cx.spawn(async move |cx| {
            match self.run(handle, cx).await {
                Ok(()) => exit.set(0),
                Err(error) => eprintln!("--headless-shot: {error}"),
            }
            cx.update(|app| app.quit());
        })
        .detach();
    }

    async fn run(self, handle: WindowHandle<FilerShell>, cx: &mut AsyncApp) -> ShotResult<()> {
        let runner = Runner {
            handle,
            fixture: self.fixture,
            output: self.output,
        };
        runner.frames(cx, deadline()).await?;
        for (index, step) in self.scenario.steps.into_iter().enumerate() {
            runner
                .step(step, cx)
                .await
                .map_err(|error| format!("step {}: {error}", index + 1))?;
        }
        Ok(())
    }
}

struct Runner {
    handle: WindowHandle<FilerShell>,
    fixture: PathBuf,
    output: PathBuf,
}

impl Runner {
    async fn step(&self, step: Step, cx: &mut AsyncApp) -> ShotResult<()> {
        let until = deadline();
        match step {
            Step::Size { width, height } => {
                let wanted = size(px(width as f32), px(height as f32));
                if self.window(cx, |window, _| window.viewport_size())? != wanted {
                    self.window(cx, |window, _| window.resize(wanted))?;
                }
                wait_until(cx, until, "window resize", |cx| {
                    self.window(cx, |window, _| window.viewport_size() == wanted)
                })
                .await?;
                self.frames(cx, until).await?;
            }
            Step::Theme { theme } => {
                let name = match theme {
                    Theme::Light => "light",
                    Theme::Dark => "default",
                };
                let palette = cx
                    .background_executor()
                    .spawn(async move { crate::theme::load(name) })
                    .await;
                self.handle.update(cx, |shell, _, cx| {
                    shell.apply_shot_theme(name, palette, cx);
                })?;
                self.frames(cx, until).await?;
            }
            Step::Wait { ms } => {
                let duration = Duration::from_millis(ms);
                if duration > STEP_TIMEOUT {
                    return Err(
                        format!("wait {ms} ms exceeds the {STEP_TIMEOUT:?} deadline").into(),
                    );
                }
                cx.background_executor().timer(duration).await;
            }
            Step::WaitIdle => self.wait_idle(cx, until).await?,
            Step::Navigate { path } => {
                let destination = fs::canonicalize(self.fixture.join(&path))?;
                if !destination.starts_with(&self.fixture) || !destination.is_dir() {
                    return Err(
                        format!("not a directory inside the fixture: {}", path.display()).into(),
                    );
                }
                self.with_active_view(cx, |view, _, cx| view.navigate(destination, cx))?;
            }
            Step::Key { keys } => {
                for key in keys {
                    self.window(cx, |window, cx| window.dispatch_keystroke(key, cx))?;
                    self.frames(cx, until).await?;
                }
            }
            Step::Click { x, y } => {
                self.frames(cx, until).await?;
                self.window(cx, |window, cx| click(window, cx, x, y))??;
            }
            Step::Select { name } => {
                self.wait_idle(cx, until).await?;
                let found = self.with_active_view(cx, |view, window, cx| {
                    let Some(index) = view.entries.iter().position(|entry| entry.name == name)
                    else {
                        return false;
                    };
                    view.focus_handle.focus(window, cx);
                    view.apply_item(
                        AppIntent::PickItem {
                            toggle: false,
                            range: false,
                        },
                        index,
                        window,
                        cx,
                    );
                    view.reveal_focus(index);
                    true
                })?;
                if !found {
                    return Err(format!("entry not found: {name:?}").into());
                }
            }
            Step::View { mode } => {
                let layout = match mode {
                    ViewMode::Rows => ViewLayout::Rows,
                    ViewMode::Tiles => ViewLayout::Tiles,
                    ViewMode::Icons => ViewLayout::Icons,
                };
                self.with_active_view(cx, |view, _, cx| view.set_layout(layout, cx))?;
                self.frames(cx, until).await?;
            }
            Step::Screenshot { name } => {
                self.wait_idle(cx, until).await?;
                let target =
                    self.window(cx, |window, _| capture::Target::from_window(window))??;
                let output = self.output.join(format!("{name}.png"));
                let remaining = until.saturating_duration_since(Instant::now());
                let saved = cx
                    .background_executor()
                    .spawn(async move { target.save_png(output, remaining) })
                    .await?;
                println!("{}", saved.display());
            }
        }
        Ok(())
    }

    fn window<R>(
        &self,
        cx: &mut AsyncApp,
        update: impl FnOnce(&mut Window, &mut App) -> R,
    ) -> ShotResult<R> {
        Ok(cx.update_window(self.handle.into(), |_, window, cx| update(window, cx))?)
    }

    fn with_active_view<R>(
        &self,
        cx: &mut AsyncApp,
        update: impl FnOnce(
            &mut crate::DirectoryListDemo,
            &mut Window,
            &mut gpui::Context<crate::DirectoryListDemo>,
        ) -> R,
    ) -> ShotResult<R> {
        let view = self
            .handle
            .read_with(cx, |shell, _| shell.active_view())?
            .ok_or("no active directory view")?;
        self.window(cx, |window, cx| {
            view.update(cx, |view, cx| update(view, window, cx))
        })
    }

    async fn wait_idle(&self, cx: &mut AsyncApp, until: Instant) -> ShotResult<()> {
        wait_until(cx, until, "directory enumeration", |cx| {
            self.with_active_view(cx, |view, _, _| {
                let status = view.session.status();
                if let Some(error) = status.error() {
                    return Err(format!("directory enumeration failed: {error}").into());
                }
                Ok(!status.is_loading())
            })?
        })
        .await?;
        self.frames(cx, until).await
    }

    async fn frames(&self, cx: &mut AsyncApp, until: Instant) -> ShotResult<()> {
        // prepaint で viewport が確定してから発生する再描画まで待つ。
        for _ in 0..3 {
            let arrived = Rc::new(Cell::new(false));
            let signal = arrived.clone();
            self.window(cx, |window, _| {
                window.refresh();
                window.on_next_frame(move |_, _| signal.set(true));
            })?;
            wait_until(cx, until, "presenting a frame", |_| Ok(arrived.get())).await?;
        }
        Ok(())
    }
}

fn click(window: &mut Window, cx: &mut App, x: u32, y: u32) -> ShotResult<()> {
    let position = point(px(x as f32), px(y as f32));
    let viewport = window.viewport_size();
    if position.x >= viewport.width || position.y >= viewport.height {
        return Err(format!("click ({x}, {y}) is outside the viewport").into());
    }
    window.dispatch_event(
        PlatformInput::MouseMove(MouseMoveEvent {
            position,
            ..Default::default()
        }),
        cx,
    );
    window.dispatch_event(
        PlatformInput::MouseDown(MouseDownEvent {
            button: MouseButton::Left,
            position,
            click_count: 1,
            ..Default::default()
        }),
        cx,
    );
    window.dispatch_event(
        PlatformInput::MouseUp(MouseUpEvent {
            button: MouseButton::Left,
            position,
            click_count: 1,
            ..Default::default()
        }),
        cx,
    );
    Ok(())
}

fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}

fn deadline() -> Instant {
    Instant::now() + STEP_TIMEOUT
}

async fn wait_until(
    cx: &mut AsyncApp,
    until: Instant,
    reason: &str,
    mut ready: impl FnMut(&mut AsyncApp) -> ShotResult<bool>,
) -> ShotResult<()> {
    loop {
        let remaining = until.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(format!("timed out waiting for {reason}").into());
        }
        if ready(cx)? {
            return Ok(());
        }
        cx.background_executor()
            .timer(POLL_INTERVAL.min(remaining))
            .await;
    }
}
