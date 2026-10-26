use crate::Icon;
use gpui::{
    App, IntoElement, ParentElement as _, RenderImage, RenderOnce, Styled as _, Window, div, img,
    px,
};
use gpui_filer_resources::IconId;
use std::sync::Arc;

/// 探索や画像読み込みは呼び出し元が行い、この部品はv5の前後レイヤーと画像を合成する。
#[derive(IntoElement)]
pub struct FolderPreview {
    images: Arc<Vec<Arc<RenderImage>>>,
    size: f32,
}

impl FolderPreview {
    pub fn new(images: Arc<Vec<Arc<RenderImage>>>, size: f32) -> Self {
        Self { images, size }
    }
}

impl RenderOnce for FolderPreview {
    fn render(self, window: &mut Window, _cx: &mut App) -> impl IntoElement {
        let mut container = div().relative().size(px(self.size)).flex_shrink_0();
        if self.images.is_empty() {
            return container.child(Icon::new(IconId::Folder).size(px(self.size)));
        }
        container = container.child(
            div()
                .absolute()
                .inset_0()
                .child(Icon::new(IconId::FolderBack).size(px(self.size))),
        );
        let thumbnail_size = self.size * 22.0 / 32.0;
        let step = self.size / 32.0;
        let count = self.images.len().min(3);
        // RenderImageの寸法はdevice px。レイアウトは論理pxで組むので換算する。
        let scale_factor = window.scale_factor();
        let mut placements = Vec::new();
        for (i, image) in self.images.iter().take(3).enumerate().rev() {
            let dimensions = image.size(0);
            let source_width = dimensions.width.0 as f32 / scale_factor;
            let source_height = dimensions.height.0 as f32 / scale_factor;
            let scale = thumbnail_size / source_width.max(source_height).max(1.0);
            let width = source_width * scale;
            let height = source_height * scale;
            let x = self.size * 0.5 + (i as f32 - (count - 1) as f32 * 0.5) * step - width * 0.5;
            let y = self.size * 6.0 / 32.0 + thumbnail_size * 0.5 + i as f32 * step - height * 0.5;
            placements.push((image.clone(), width, height, x, y));
        }
        for (image, width, height, x, y) in placements {
            container = container.child(
                div().absolute().left(px(x)).top(px(y)).child(
                    img(image)
                        .w(px(width))
                        .h(px(height))
                        .rounded(px(self.size / 32.0)),
                ),
            );
        }
        container.child(
            div()
                .absolute()
                .inset_0()
                .child(Icon::new(IconId::FolderFront).size(px(self.size))),
        )
    }
}
