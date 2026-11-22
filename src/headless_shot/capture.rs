use std::{
    fs::{self, OpenOptions},
    io::Write,
    path::PathBuf,
    process::Command,
    time::{Duration, Instant},
};

use gpui::Window;
use raw_window_handle::{HasWindowHandle, RawWindowHandle};

use super::ShotResult;

pub(super) struct Target {
    xid: u64,
    width: u32,
    height: u32,
}

impl Target {
    pub(super) fn from_window(window: &Window) -> ShotResult<Self> {
        let xid = match HasWindowHandle::window_handle(window)?.as_raw() {
            RawWindowHandle::Xcb(handle) => u64::from(handle.window.get()),
            RawWindowHandle::Xlib(handle) => handle.window as u64,
            _ => return Err("capture needs an X11 window".into()),
        };
        let viewport = window
            .viewport_size()
            .to_device_pixels(window.scale_factor());
        Ok(Self {
            xid,
            width: viewport.width.0.try_into()?,
            height: viewport.height.0.try_into()?,
        })
    }

    pub(super) fn save_png(self, destination: PathBuf, timeout: Duration) -> ShotResult<PathBuf> {
        if timeout.is_zero() {
            return Err("timed out before X11 capture".into());
        }
        let until = Instant::now() + timeout;
        let output = Command::new("timeout")
            .args([
                "--foreground",
                "--kill-after=1s",
                &format!("{:.6}s", timeout.as_secs_f64().max(0.001)),
                "import",
                "-window",
                &format!("0x{:x}", self.xid),
                "png:-",
            ])
            .output()?;
        if matches!(output.status.code(), Some(124 | 137)) {
            return Err("timed out capturing the X11 window".into());
        }
        if !output.status.success() {
            return Err(format!(
                "ImageMagick import failed ({}): {}",
                output.status,
                String::from_utf8_lossy(&output.stderr).trim()
            )
            .into());
        }
        let pixels = image::load_from_memory_with_format(&output.stdout, image::ImageFormat::Png)?
            .into_rgba8();
        if pixels.dimensions() != (self.width, self.height) {
            return Err(format!(
                "PNG size {:?} does not match viewport {}×{}",
                pixels.dimensions(),
                self.width,
                self.height
            )
            .into());
        }
        let mut colors = pixels.pixels();
        let first = colors.next().ok_or("PNG contains no pixels")?;
        if !colors.any(|pixel| pixel != first) {
            return Err("PNG contains only one color".into());
        }
        let mut nonce = [0u8; 8];
        getrandom::fill(&mut nonce).map_err(|error| format!("temporary PNG name: {error}"))?;
        let temporary =
            destination.with_extension(format!("{:016x}.tmp", u64::from_ne_bytes(nonce)));
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)?;
        let result = (|| -> ShotResult<()> {
            file.write_all(&output.stdout)?;
            drop(file);
            if Instant::now() >= until {
                return Err("timed out validating or writing the PNG".into());
            }
            fs::rename(&temporary, &destination)?;
            Ok(())
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temporary);
        }
        result?;
        Ok(destination)
    }
}
