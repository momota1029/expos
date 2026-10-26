use std::sync::Arc;

use gpui::{
    App, Asset, AssetSource as _, DevicePixels, ImageCacheError, RenderImage, SvgSize, size,
};
use gpui_filer_resources::{IconId, IconSource, ResourceAssets};

/// 色付きSVGは単色マスクではなく画像として描く。GPUIのasset cacheを使い、
/// 同じicon・物理サイズの描画を共有する。生成画像は静的リソースには保存しない。
pub(super) struct ColorIcon;

impl Asset for ColorIcon {
    type Source = (IconId, u32);
    type Output = Result<Arc<RenderImage>, ImageCacheError>;

    fn load(
        (id, pixels): Self::Source,
        cx: &mut App,
    ) -> impl Future<Output = Self::Output> + Send + 'static {
        let renderer = cx.svg_renderer();
        async move {
            let IconSource::ColorSvg(path) = id.definition().source else {
                return Err(ImageCacheError::Asset("expected a color SVG".into()));
            };
            let bytes = ResourceAssets
                .load(path)
                .map_err(|error| ImageCacheError::Other(Arc::new(error)))?
                .ok_or_else(|| ImageCacheError::Asset(path.into()))?;
            let parsed = renderer.parse_svg(&bytes)?;
            Ok(renderer.render_parsed(
                &parsed,
                SvgSize::Size(size(
                    DevicePixels(pixels as i32),
                    DevicePixels(pixels as i32),
                )),
            )?)
        }
    }
}
