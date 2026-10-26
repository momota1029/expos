use std::collections::HashSet;
use std::path::PathBuf;
use std::time::Duration;

const DROP_SOURCE_START_TIMEOUT_MS: u64 = 500;
const COPY_EFFECT: u32 = 1;
const MOVE_EFFECT: u32 = 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PerformedEffectReport {
    Unreported,
    Value(u32),
    Ambiguous,
}

impl PerformedEffectReport {
    fn record(&mut self, next: Option<u32>) {
        *self = match (*self, next) {
            (Self::Ambiguous, _) | (_, None) => Self::Ambiguous,
            (Self::Unreported, Some(value)) => Self::Value(value),
            (Self::Value(current), Some(value)) if current == value => Self::Value(current),
            (Self::Value(_), Some(_)) => Self::Ambiguous,
        };
    }

    fn value(self) -> Option<u32> {
        match self {
            Self::Value(value) => Some(value),
            Self::Unreported | Self::Ambiguous => None,
        }
    }
}

fn parse_performed_effect_dword(bytes: &[u8]) -> Option<u32> {
    let bytes: [u8; 4] = bytes.get(..4)?.try_into().ok()?;
    Some(u32::from_le_bytes(bytes))
}

fn parse_shell_performed_effect(
    format: u16,
    registered_format: u16,
    valid_formatetc: bool,
    is_hglobal: bool,
    bytes: &[u8],
) -> Option<u32> {
    if format != registered_format || !valid_formatetc || !is_hglobal {
        return None;
    }
    parse_performed_effect_dword(bytes)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DropSourceDecision {
    Continue,
    Drop,
    Cancel,
}

fn drop_source_decision(
    saw_left_down: &mut bool,
    left_down: bool,
    elapsed: Duration,
) -> DropSourceDecision {
    if *saw_left_down {
        return if left_down {
            DropSourceDecision::Continue
        } else {
            DropSourceDecision::Drop
        };
    }
    // QueryContinueDrag は入力状態が変わったときに呼ばれる。初回が release なら
    // S_OK を返して待たず、Cancel して DoDragDrop を終端させる。
    if !left_down || elapsed >= Duration::from_millis(DROP_SOURCE_START_TIMEOUT_MS) {
        return DropSourceDecision::Cancel;
    }
    *saw_left_down = true;
    DropSourceDecision::Continue
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DragResult {
    Dropped,
    Cancelled,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DragExecution {
    pub result: DragResult,
    /// キャンセル時はOLE仕様上返らない。値はOLEのeffectで、アプリ操作受理の証明ではない。
    pub performed_effect: Option<u32>,
    /// IDataObject::SetData で Shell が報告した CFSTR_PERFORMEDDROPEFFECT。
    /// DoDragDrop の戻り値とは独立したプラットフォーム報告値。
    pub shell_performed_effect: Option<u32>,
    pub visual_warning: Option<String>,
}

fn drag_execution_with_shell_report(
    result: DragResult,
    performed_effect: Option<u32>,
    performed_report: PerformedEffectReport,
    visual_warning: Option<String>,
) -> DragExecution {
    DragExecution {
        result,
        performed_effect,
        shell_performed_effect: performed_report.value(),
        visual_warning,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DragError {
    UnsupportedPlatform,
    InvalidInput(String),
    Com(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DragVisual {
    pub width: u32,
    pub height: u32,
    pub hotspot_x: i32,
    pub hotspot_y: i32,
    pub rgba: Vec<u8>,
    pub title: Option<String>,
    pub subtitle: Option<String>,
}

impl DragVisual {
    pub fn new(
        width: u32,
        height: u32,
        hotspot_x: i32,
        hotspot_y: i32,
        rgba: Vec<u8>,
    ) -> Result<Self, DragError> {
        if width == 0 || height == 0 {
            return Err(DragError::InvalidInput(
                "drag visual width/height must be > 0".to_string(),
            ));
        }
        if hotspot_x < 0 || hotspot_x >= width as i32 || hotspot_y < 0 || hotspot_y >= height as i32
        {
            return Err(DragError::InvalidInput(
                "drag visual hotspot is outside image bounds".to_string(),
            ));
        }
        let expected_len = width as usize * height as usize * 4;
        if rgba.len() != expected_len {
            return Err(DragError::InvalidInput(format!(
                "drag visual rgba length mismatch: expected {expected_len}, got {}",
                rgba.len()
            )));
        }
        Ok(Self {
            width,
            height,
            hotspot_x,
            hotspot_y,
            rgba,
            title: None,
            subtitle: None,
        })
    }

    pub fn with_title(mut self, title: impl Into<String>) -> Self {
        self.title = Some(title.into());
        self
    }

    pub fn with_subtitle(mut self, subtitle: impl Into<String>) -> Self {
        self.subtitle = Some(subtitle.into());
        self
    }

    /// デフォルトのドラッグ画像（複数選択時はスタック + 件数バッジ）。
    ///
    /// なぜ crate 側で自動生成するか:
    /// - UI 層を Windows/GDI 依存から切り離し、内部 D&D でも再利用できるようにするため。
    pub fn stacked_file_cards(item_count: usize) -> Self {
        let count = item_count.max(1);
        let width = 104u32;
        let height = 74u32;
        let mut rgba = vec![0u8; width as usize * height as usize * 4];

        let layers = count.clamp(1, 3);
        for layer in (0..layers).rev() {
            let offset_x = 6 * layer as i32;
            let offset_y = 4 * layer as i32;
            let alpha = if layer == 0 { 236 } else { 160 };
            draw_card(&mut rgba, width, height, offset_x, offset_y, alpha);
        }

        if count > 1 {
            let label = if count > 99 {
                "99+".to_string()
            } else {
                count.to_string()
            };
            draw_count_badge(&mut rgba, width, height, &label);
        }

        Self {
            width,
            height,
            hotspot_x: 16,
            hotspot_y: 12,
            rgba,
            title: None,
            subtitle: None,
        }
    }

    /// Creates the native drag image with a bounded, opaque title panel.
    pub fn labelled_stacked_file_cards(item_count: usize, title: impl Into<String>) -> Self {
        Self::labelled_stacked_file_cards_scaled(item_count, title, 1.0)
    }

    /// Creates the native drag image at the source window's device scale.
    fn labelled_stacked_file_cards_scaled(
        item_count: usize,
        title: impl Into<String>,
        scale_factor: f32,
    ) -> Self {
        let count = item_count.max(1);
        let scale_milli = normalized_drag_scale_milli(scale_factor);
        let scale = scale_milli as f32 / 1000.0;
        let logical_width = 320u32;
        let logical_height = 88u32;
        let mut logical_rgba = vec![0u8; logical_width as usize * logical_height as usize * 4];

        for layer in (0..count.clamp(1, 3)).rev() {
            draw_labelled_card(
                &mut logical_rgba,
                logical_width,
                logical_height,
                6 * layer as i32,
                4 * layer as i32,
                255,
            );
        }

        // GDI の DrawText は32bit DIBのalphaを更新しないため、文字の背面を
        // 不透明にして、OSのalpha blendでも文字と背景が一緒に見えるようにする。
        fill_rect(
            &mut logical_rgba,
            logical_width,
            logical_height,
            111,
            10,
            201,
            72,
            [0, 0, 0, 60],
        );
        fill_round_rect(
            &mut logical_rgba,
            logical_width,
            logical_height,
            108,
            6,
            204,
            76,
            11,
            [221, 233, 250, 255],
        );
        fill_round_rect(
            &mut logical_rgba,
            logical_width,
            logical_height,
            109,
            7,
            202,
            74,
            10,
            [244, 248, 255, 255],
        );
        fill_round_rect(
            &mut logical_rgba,
            logical_width,
            logical_height,
            112,
            10,
            3,
            68,
            1,
            [65, 132, 230, 255],
        );
        let width = (logical_width as f32 * scale).round() as u32;
        let height = (logical_height as f32 * scale).round() as u32;
        let rgba = scale_rgba_nearest(&logical_rgba, logical_width, logical_height, width, height);
        Self {
            width,
            height,
            hotspot_x: (16.0 * scale).round() as i32,
            hotspot_y: (12.0 * scale).round() as i32,
            rgba,
            title: Some(title.into()),
            subtitle: None,
        }
    }
}

fn normalized_drag_scale_milli(scale_factor: f32) -> u32 {
    if !scale_factor.is_finite() || scale_factor <= 0.0 {
        return 1000;
    }
    (scale_factor.clamp(0.25, 1.5) * 1000.0).round() as u32
}

fn clamp_drag_scale_milli(scale_milli: u32) -> u32 {
    scale_milli.clamp(250, 1500)
}

fn scaled_logical_px(px: i32, scale_milli: u32) -> i32 {
    ((px as i64 * scale_milli as i64 + 500) / 1000) as i32
}

fn text_rects_for_scale(scale_milli: u32) -> ((i32, i32, i32, i32), (i32, i32, i32, i32)) {
    // 320×88時の既存レイアウト値を基準にし、DPI変更で余白も同率にする。
    let scale = |v| scaled_logical_px(v, scale_milli);
    (
        (scale(128), scale(19), scale(308), scale(43)),
        (scale(128), scale(48), scale(308), scale(63)),
    )
}

fn scale_rgba_nearest(
    src: &[u8],
    src_width: u32,
    src_height: u32,
    width: u32,
    height: u32,
) -> Vec<u8> {
    let mut dst = vec![0; width as usize * height as usize * 4];
    for y in 0..height {
        let source_y = y * src_height / height;
        for x in 0..width {
            let source_x = x * src_width / width;
            let source = ((source_y * src_width + source_x) * 4) as usize;
            let target = ((y * width + x) * 4) as usize;
            dst[target..target + 4].copy_from_slice(&src[source..source + 4]);
        }
    }
    dst
}

fn title_ink_rgb(rgba: &[u8], width: u32, height: u32, rect: (i32, i32, i32, i32)) -> [u8; 3] {
    let (left, top, right, bottom) = rect;
    let mut luminance_sum = 0u64;
    let mut opaque_count = 0u64;
    let mut pixel_count = 0u64;
    for y in top.max(0)..bottom.min(height as i32) {
        for x in left.max(0)..right.min(width as i32) {
            pixel_count += 1;
            let offset = (y as usize * width as usize + x as usize) * 4;
            if rgba[offset + 3] != 255 {
                continue;
            }
            let [r, g, b] = [rgba[offset], rgba[offset + 1], rgba[offset + 2]];
            luminance_sum += 299 * r as u64 + 587 * g as u64 + 114 * b as u64;
            opaque_count += 1;
        }
    }

    // 透明画素を含む領域では、RGBではなく安全な明色を選ぶ。
    if pixel_count == 0 || opaque_count != pixel_count {
        [245, 249, 255]
    } else if luminance_sum / opaque_count > 128_000 {
        [24, 31, 43]
    } else {
        [245, 249, 255]
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DragVisualSource {
    /// crate 内のデフォルト画像を使う
    Auto,
    /// 呼び出し側が生成した画像を使う
    Custom(DragVisual),
    /// 生成時に指定したデバイス倍率で文字も描画する画像。
    /// bitmap寸法・hotspotはscale適用済みで、ここでは文字だけに倍率を伝える。
    CustomScaled {
        visual: DragVisual,
        scale_milli: u32,
    },
    /// 画像を設定せず、OS 既定カーソルのみでドラッグする
    None,
}

impl DragVisualSource {
    pub fn labelled_stacked_file_cards_scaled(
        item_count: usize,
        title: impl Into<String>,
        scale_factor: f32,
    ) -> Self {
        let scale_milli = normalized_drag_scale_milli(scale_factor);
        Self::CustomScaled {
            visual: DragVisual::labelled_stacked_file_cards_scaled(item_count, title, scale_factor),
            scale_milli,
        }
    }
}

impl Default for DragVisualSource {
    fn default() -> Self {
        Self::Auto
    }
}

#[derive(Debug, Clone, Default)]
pub struct DragOptions {
    pub visual: DragVisualSource,
    pub ghost_window: Option<DragGhostWindow>,
    /// Opaque provenance token for a drag session owned by the caller.
    pub local_drag_session_id: Option<u64>,
    /// Shared cancellation signal for a primary Windows touch contact handed to OLE.
    pub touch_drag_cancelled: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
}

impl PartialEq for DragOptions {
    fn eq(&self, other: &Self) -> bool {
        let same_touch_cancellation =
            match (&self.touch_drag_cancelled, &other.touch_drag_cancelled) {
                (None, None) => true,
                (Some(left), Some(right)) => std::sync::Arc::ptr_eq(left, right),
                _ => false,
            };
        self.visual == other.visual
            && self.ghost_window == other.ghost_window
            && self.local_drag_session_id == other.local_drag_session_id
            && same_touch_cancellation
    }
}

impl Eq for DragOptions {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DragGhostWindow {
    /// 外部（GPUI）で作成したゴーストウィンドウの HWND
    pub hwnd: isize,
    /// カーソル位置に対する表示オフセット
    pub offset_x: i32,
    pub offset_y: i32,
}

/// A filesystem source authorized by an already captured object identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileDragSource {
    pub path: PathBuf,
    pub volume_serial: u32,
    pub file_index: u64,
    pub is_directory: bool,
    /// Directory snapshot facts used only as a best-effort Move stale-content guard.
    pub captured_size: Option<u64>,
    pub captured_modified: Option<std::time::SystemTime>,
}

/// Stable identity of one directory in the path-resolution route for a source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileDragParentIdentity {
    pub path: PathBuf,
    pub volume_serial: u32,
    pub file_index: u64,
}

/// OLE中の同一fileへのhardlink差し戻しをcleanupで拒否するためのmetadata署名。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileDragLeafSignature {
    pub number_of_links: u32,
    pub change_time: i64,
}

/// Paths exposed through CF_HDROP and handles that pin the captured objects and
/// parent routes until OLE returns. A Move-capable leaf path may change meanwhile.
pub struct PinnedFileDragSources {
    paths: Vec<PathBuf>,
    _parent_handles: Vec<std::fs::File>,
    parent_routes: Vec<Vec<FileDragParentIdentity>>,
    leaf_handles: Vec<std::fs::File>,
    leaf_signatures: Vec<FileDragLeafSignature>,
    policy: FileDragPinPolicy,
}

/// Holds the original regular-file identities until the operation worker has
/// opened and compared the current source paths.
pub struct MoveFileIdentityLease {
    sources: Vec<MoveFileIdentityHandle>,
}

struct MoveFileIdentityHandle {
    path: PathBuf,
    parent_route: Vec<FileDragParentIdentity>,
    signature: FileDragLeafSignature,
    _file: std::fs::File,
}

impl MoveFileIdentityLease {
    pub fn len(&self) -> usize {
        self.sources.len()
    }

    pub fn is_empty(&self) -> bool {
        self.sources.is_empty()
    }

    /// Original source order captured when the drag was prepared.
    pub fn source_paths(&self) -> impl ExactSizeIterator<Item = &PathBuf> {
        self.sources.iter().map(|source| &source.path)
    }

    pub fn parent_routes(&self) -> impl ExactSizeIterator<Item = &[FileDragParentIdentity]> {
        self.sources
            .iter()
            .map(|source| source.parent_route.as_slice())
    }

    pub fn signatures(&self) -> impl ExactSizeIterator<Item = FileDragLeafSignature> + '_ {
        self.sources.iter().map(|source| source.signature)
    }

    /// Transfers source paths and keep-alive handles in the same request order.
    pub fn into_parts(
        self,
    ) -> (
        Vec<PathBuf>,
        Vec<std::fs::File>,
        Vec<Vec<FileDragParentIdentity>>,
        Vec<FileDragLeafSignature>,
    ) {
        let (paths, files, routes, signatures): (Vec<_>, Vec<_>, Vec<_>, Vec<_>) = self
            .sources
            .into_iter()
            .map(|source| {
                (
                    source.path,
                    source._file,
                    source.parent_route,
                    source.signature,
                )
            })
            .fold(
                (Vec::new(), Vec::new(), Vec::new(), Vec::new()),
                |mut acc, (path, file, route, signature)| {
                    acc.0.push(path);
                    acc.1.push(file);
                    acc.2.push(route);
                    acc.3.push(signature);
                    acc
                },
            );
        (paths, files, routes, signatures)
    }
}

/// OLE execution facts and, for a dropped Move-capable drag, the original leaf
/// identity lease. The caller must retain the lease only when its complete OLE
/// MOVE validation authorizes source cleanup; dropping it releases all leaves.
pub struct PinnedFileDragExecution {
    pub execution: DragExecution,
    move_lease: Option<MoveFileIdentityLease>,
}

impl PinnedFileDragExecution {
    /// Returns a cleanup lease only when both independent OLE channels report exact MOVE.
    /// Any lease not authorized by that evidence is dropped with this result.
    pub fn into_authorized_parts(mut self) -> (DragExecution, Option<MoveFileIdentityLease>) {
        let authorized = self.execution.result == DragResult::Dropped
            && self.execution.performed_effect == Some(MOVE_EFFECT)
            && self.execution.shell_performed_effect == Some(MOVE_EFFECT);
        let move_lease = authorized.then(|| self.move_lease.take()).flatten();
        (self.execution, move_lease)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FileDragPinPolicy {
    #[default]
    CopyOnly,
    MoveCapable,
}

impl FileDragPinPolicy {
    fn permits_move(self) -> bool {
        matches!(self, Self::MoveCapable)
    }
}

impl PinnedFileDragSources {
    pub fn len(&self) -> usize {
        self.paths.len()
    }

    pub fn is_empty(&self) -> bool {
        self.paths.is_empty()
    }

    fn allowed_effects(&self) -> u32 {
        COPY_EFFECT
            | if self.policy.permits_move() {
                MOVE_EFFECT
            } else {
                0
            }
    }
}

pub trait ExternalDragService: Send + Sync {
    fn begin_file_drag_with_options_detailed(
        &self,
        paths: &[PathBuf],
        options: &DragOptions,
    ) -> Result<DragExecution, DragError>;

    fn begin_file_drag_with_options(
        &self,
        paths: &[PathBuf],
        options: &DragOptions,
    ) -> Result<DragResult, DragError> {
        self.begin_file_drag_with_options_detailed(paths, options)
            .map(|execution| execution.result)
    }

    fn begin_file_drag(&self, paths: &[PathBuf]) -> Result<DragResult, DragError> {
        self.begin_file_drag_with_options(paths, &DragOptions::default())
    }
}

#[derive(Debug, Default)]
pub struct WinShellDragService;

impl ExternalDragService for WinShellDragService {
    fn begin_file_drag_with_options_detailed(
        &self,
        paths: &[PathBuf],
        options: &DragOptions,
    ) -> Result<DragExecution, DragError> {
        let normalized = normalize_paths(paths)?;
        let mut resolved_options = options.clone();
        resolved_options.visual = resolve_drag_visual(options, normalized.len());
        platform::begin_file_drag_impl(&normalized, &resolved_options, COPY_EFFECT)
    }

    fn begin_file_drag_with_options(
        &self,
        paths: &[PathBuf],
        options: &DragOptions,
    ) -> Result<DragResult, DragError> {
        self.begin_file_drag_with_options_detailed(paths, options)
            .map(|execution| execution.result)
    }
}

pub fn begin_file_drag(paths: &[PathBuf]) -> Result<DragResult, DragError> {
    begin_file_drag_with_options(paths, &DragOptions::default())
}

pub fn begin_file_drag_with_options(
    paths: &[PathBuf],
    options: &DragOptions,
) -> Result<DragResult, DragError> {
    WinShellDragService.begin_file_drag_with_options(paths, options)
}

pub fn begin_file_drag_with_options_detailed(
    paths: &[PathBuf],
    options: &DragOptions,
) -> Result<DragExecution, DragError> {
    WinShellDragService.begin_file_drag_with_options_detailed(paths, options)
}

/// Performs blocking no-follow probes; call this away from GPUI input/render paths.
/// The returned lease is Send and keeps each validated source and its parent path
/// chain from being renamed or replaced until it is dropped. Only absolute local
/// drive paths are supported; UNC and other path forms are rejected.
pub fn pin_file_drag_sources(
    sources: &[FileDragSource],
) -> Result<PinnedFileDragSources, DragError> {
    pin_file_drag_sources_with_policy(sources, FileDragPinPolicy::CopyOnly)
}

/// Explicitly permits a regular-file drag to advertise Move to OLE.
///
/// Since CF_HDROP exposes paths, a Move-capable leaf can be renamed or replaced
/// while the drag is active. Any later source cleanup must still revalidate the
/// captured identity; the target may resolve a replacement at that path.
/// Move-capable pins reject any captured directory source.
pub fn pin_file_drag_sources_with_policy(
    sources: &[FileDragSource],
    policy: FileDragPinPolicy,
) -> Result<PinnedFileDragSources, DragError> {
    platform::pin_file_drag_sources(sources, policy)
}

/// Starts the OLE loop on the caller's initialized STA and retains source pins until
/// that loop returns. Do not await between the last contact/session check and this call.
pub fn begin_pinned_file_drag_with_options_detailed(
    sources: PinnedFileDragSources,
    options: &DragOptions,
) -> Result<DragExecution, DragError> {
    begin_pinned_file_drag_with_move_lease(sources, options).map(|result| result.execution)
}

/// Performs OLE and releases parent-route handles as soon as OLE returns.
/// Successful dropped Move-capable drags transfer only their leaf identity
/// handles to the caller; cancelled and failed drags release every handle.
pub fn begin_pinned_file_drag_with_move_lease(
    sources: PinnedFileDragSources,
    options: &DragOptions,
) -> Result<PinnedFileDragExecution, DragError> {
    let mut resolved_options = options.clone();
    resolved_options.visual = resolve_drag_visual(options, sources.len());
    let effects = sources.allowed_effects();
    let PinnedFileDragSources {
        paths,
        _parent_handles,
        parent_routes,
        leaf_handles,
        leaf_signatures,
        policy,
    } = sources;
    let execution = platform::begin_file_drag_impl(&paths, &resolved_options, effects)?;
    Ok(finish_pinned_file_drag(
        PinnedFileDragSources {
            paths,
            _parent_handles,
            parent_routes,
            leaf_handles,
            leaf_signatures,
            policy,
        },
        execution,
    ))
}

fn finish_pinned_file_drag(
    sources: PinnedFileDragSources,
    execution: DragExecution,
) -> PinnedFileDragExecution {
    let PinnedFileDragSources {
        paths,
        _parent_handles,
        parent_routes,
        leaf_handles,
        leaf_signatures,
        policy,
    } = sources;
    drop(_parent_handles);
    let move_lease = if execution.result == DragResult::Dropped && policy.permits_move() {
        Some(MoveFileIdentityLease {
            sources: paths
                .into_iter()
                .zip(leaf_handles)
                .zip(parent_routes)
                .zip(leaf_signatures)
                .map(
                    |(((path, file), parent_route), signature)| MoveFileIdentityHandle {
                        path,
                        parent_route,
                        signature,
                        _file: file,
                    },
                )
                .collect(),
        })
    } else {
        drop(leaf_handles);
        None
    };
    PinnedFileDragExecution {
        execution,
        move_lease,
    }
}

/// Release the source window's capture after verifying exact ownership.
pub fn release_source_capture(source_hwnd: isize) -> Result<(), DragError> {
    platform::release_source_capture(source_hwnd)
}

fn resolve_drag_visual(options: &DragOptions, count: usize) -> DragVisualSource {
    match &options.visual {
        DragVisualSource::Auto => DragVisualSource::Custom(DragVisual::stacked_file_cards(count)),
        DragVisualSource::Custom(visual) => DragVisualSource::Custom(visual.clone()),
        DragVisualSource::CustomScaled {
            visual,
            scale_milli,
        } => DragVisualSource::CustomScaled {
            visual: visual.clone(),
            scale_milli: clamp_drag_scale_milli(*scale_milli),
        },
        DragVisualSource::None => DragVisualSource::None,
    }
}

fn normalize_paths(paths: &[PathBuf]) -> Result<Vec<PathBuf>, DragError> {
    if paths.is_empty() {
        return Err(DragError::InvalidInput("paths is empty".to_string()));
    }

    // なぜ重複除去するか:
    // 同じパスを複数回 DataObject に積むと、コピー先の挙動がシェル実装依存になるため。
    let mut unique = Vec::with_capacity(paths.len());
    let mut seen = HashSet::with_capacity(paths.len());
    for path in paths {
        if path.as_os_str().is_empty() {
            return Err(DragError::InvalidInput("path is empty".to_string()));
        }
        if !path.exists() {
            return Err(DragError::InvalidInput(format!(
                "path does not exist: {}",
                path.display()
            )));
        }
        if seen.insert(path.clone()) {
            unique.push(path.clone());
        }
    }
    Ok(unique)
}

fn draw_card(
    canvas: &mut [u8],
    canvas_w: u32,
    canvas_h: u32,
    offset_x: i32,
    offset_y: i32,
    alpha: u8,
) {
    let card_x = 8 + offset_x;
    let card_y = 8 + offset_y;
    let card_w = 72;
    let card_h = 52;

    fill_rect(
        canvas,
        canvas_w,
        canvas_h,
        card_x,
        card_y,
        card_w,
        card_h,
        [255, 255, 255, alpha],
    );
    stroke_rect(
        canvas,
        canvas_w,
        canvas_h,
        card_x,
        card_y,
        card_w,
        card_h,
        [120, 130, 150, alpha],
    );

    fill_rect(
        canvas,
        canvas_w,
        canvas_h,
        card_x + 8,
        card_y + 10,
        16,
        20,
        [90, 138, 214, alpha],
    );
    fill_rect(
        canvas,
        canvas_w,
        canvas_h,
        card_x + 30,
        card_y + 14,
        30,
        4,
        [146, 153, 168, alpha],
    );
    fill_rect(
        canvas,
        canvas_w,
        canvas_h,
        card_x + 30,
        card_y + 22,
        24,
        4,
        [182, 188, 200, alpha],
    );

    draw_folded_corner(canvas, canvas_w, canvas_h, card_x, card_y, card_w, alpha);
}

fn draw_labelled_card(
    canvas: &mut [u8],
    canvas_w: u32,
    canvas_h: u32,
    offset_x: i32,
    offset_y: i32,
    alpha: u8,
) {
    let card_x = 8 + offset_x;
    let card_y = 8 + offset_y;
    let card_w = 72;
    let card_h = 52;

    fill_round_rect(
        canvas,
        canvas_w,
        canvas_h,
        card_x + 2,
        card_y + 3,
        card_w,
        card_h,
        5,
        [10, 17, 30, alpha / 2],
    );
    fill_round_rect(
        canvas,
        canvas_w,
        canvas_h,
        card_x,
        card_y,
        card_w,
        card_h,
        5,
        [226, 234, 246, alpha],
    );
    fill_round_rect(
        canvas,
        canvas_w,
        canvas_h,
        card_x + 1,
        card_y + 1,
        card_w - 2,
        card_h - 2,
        4,
        [250, 252, 255, alpha],
    );

    // 小さなサムネイルに写真風の山形と、本文を示す罫線を入れる。
    fill_round_rect(
        canvas,
        canvas_w,
        canvas_h,
        card_x + 8,
        card_y + 10,
        19,
        25,
        3,
        [89, 151, 224, alpha],
    );
    fill_rect(
        canvas,
        canvas_w,
        canvas_h,
        card_x + 9,
        card_y + 27,
        17,
        7,
        [76, 133, 198, alpha],
    );
    for (x, y, color) in [
        (card_x + 11, card_y + 24, [191, 225, 255, alpha]),
        (card_x + 12, card_y + 23, [191, 225, 255, alpha]),
        (card_x + 13, card_y + 22, [191, 225, 255, alpha]),
        (card_x + 14, card_y + 23, [191, 225, 255, alpha]),
        (card_x + 15, card_y + 24, [191, 225, 255, alpha]),
        (card_x + 17, card_y + 25, [222, 241, 255, alpha]),
        (card_x + 18, card_y + 24, [222, 241, 255, alpha]),
        (card_x + 19, card_y + 23, [222, 241, 255, alpha]),
        (card_x + 20, card_y + 24, [222, 241, 255, alpha]),
        (card_x + 21, card_y + 25, [222, 241, 255, alpha]),
    ] {
        blend_pixel(canvas, canvas_w, canvas_h, x, y, color);
    }
    fill_round_rect(
        canvas,
        canvas_w,
        canvas_h,
        card_x + 13,
        card_y + 13,
        5,
        5,
        2,
        [255, 220, 132, alpha],
    );

    fill_round_rect(
        canvas,
        canvas_w,
        canvas_h,
        card_x + 33,
        card_y + 13,
        27,
        4,
        2,
        [105, 124, 150, alpha],
    );
    fill_round_rect(
        canvas,
        canvas_w,
        canvas_h,
        card_x + 33,
        card_y + 21,
        22,
        3,
        1,
        [163, 177, 197, alpha],
    );
    fill_round_rect(
        canvas,
        canvas_w,
        canvas_h,
        card_x + 8,
        card_y + 41,
        51,
        3,
        1,
        [207, 217, 231, alpha],
    );
    stroke_rect(
        canvas,
        canvas_w,
        canvas_h,
        card_x,
        card_y,
        card_w,
        card_h,
        [138, 157, 183, alpha],
    );
}

fn fill_round_rect(
    canvas: &mut [u8],
    canvas_w: u32,
    canvas_h: u32,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    radius: i32,
    color: [u8; 4],
) {
    let radius = radius.clamp(0, w.min(h) / 2);
    let radius_sq = radius * radius;
    for yy in 0..h {
        for xx in 0..w {
            let corner_x = if xx < radius {
                radius - xx - 1
            } else if xx >= w - radius {
                xx - (w - radius)
            } else {
                0
            };
            let corner_y = if yy < radius {
                radius - yy - 1
            } else if yy >= h - radius {
                yy - (h - radius)
            } else {
                0
            };
            if corner_x == 0
                || corner_y == 0
                || corner_x * corner_x + corner_y * corner_y <= radius_sq
            {
                blend_pixel(canvas, canvas_w, canvas_h, x + xx, y + yy, color);
            }
        }
    }
}

fn draw_folded_corner(
    canvas: &mut [u8],
    canvas_w: u32,
    canvas_h: u32,
    card_x: i32,
    card_y: i32,
    card_w: i32,
    alpha: u8,
) {
    let fold = 10;
    for row in 0..fold {
        for col in 0..=row {
            let x = card_x + card_w - 1 - col;
            let y = card_y + row;
            blend_pixel(canvas, canvas_w, canvas_h, x, y, [232, 236, 246, alpha]);
        }
    }
    for d in 0..fold {
        blend_pixel(
            canvas,
            canvas_w,
            canvas_h,
            card_x + card_w - 1 - d,
            card_y + d,
            [173, 181, 196, alpha],
        );
    }
}

fn draw_count_badge(canvas: &mut [u8], canvas_w: u32, canvas_h: u32, label: &str) {
    let text_width = tiny_text_width(label);
    let badge_w = (text_width + 8).max(16);
    let badge_h = 14;
    let badge_x = canvas_w as i32 - badge_w - 10;
    let badge_y = canvas_h as i32 - badge_h - 10;

    fill_rect(
        canvas,
        canvas_w,
        canvas_h,
        badge_x + 1,
        badge_y + 1,
        badge_w,
        badge_h,
        [0, 0, 0, 70],
    );
    fill_rect(
        canvas,
        canvas_w,
        canvas_h,
        badge_x,
        badge_y,
        badge_w,
        badge_h,
        [23, 92, 210, 240],
    );
    stroke_rect(
        canvas,
        canvas_w,
        canvas_h,
        badge_x,
        badge_y,
        badge_w,
        badge_h,
        [210, 224, 255, 255],
    );

    let text_x = badge_x + (badge_w - text_width) / 2;
    let text_y = badge_y + 4;
    draw_tiny_text(
        canvas,
        canvas_w,
        canvas_h,
        text_x,
        text_y,
        label,
        [255, 255, 255, 255],
    );
}

fn tiny_text_width(text: &str) -> i32 {
    let glyph_w = 3;
    let spacing = 1;
    let len = text.chars().count() as i32;
    if len == 0 {
        0
    } else {
        len * glyph_w + (len - 1) * spacing
    }
}

fn draw_tiny_text(
    canvas: &mut [u8],
    canvas_w: u32,
    canvas_h: u32,
    x: i32,
    y: i32,
    text: &str,
    color: [u8; 4],
) {
    let mut cursor_x = x;
    for ch in text.chars() {
        if let Some(glyph) = tiny_glyph(ch) {
            draw_tiny_glyph(canvas, canvas_w, canvas_h, cursor_x, y, glyph, color);
            cursor_x += 4;
        }
    }
}

fn draw_tiny_glyph(
    canvas: &mut [u8],
    canvas_w: u32,
    canvas_h: u32,
    x: i32,
    y: i32,
    glyph: [u8; 15],
    color: [u8; 4],
) {
    for gy in 0..5 {
        for gx in 0..3 {
            if glyph[(gy * 3 + gx) as usize] != 0 {
                blend_pixel(canvas, canvas_w, canvas_h, x + gx, y + gy, color);
            }
        }
    }
}

fn tiny_glyph(ch: char) -> Option<[u8; 15]> {
    Some(match ch {
        '0' => [1, 1, 1, 1, 0, 1, 1, 0, 1, 1, 0, 1, 1, 1, 1],
        '1' => [0, 1, 0, 1, 1, 0, 0, 1, 0, 0, 1, 0, 1, 1, 1],
        '2' => [1, 1, 1, 0, 0, 1, 1, 1, 1, 1, 0, 0, 1, 1, 1],
        '3' => [1, 1, 1, 0, 0, 1, 0, 1, 1, 0, 0, 1, 1, 1, 1],
        '4' => [1, 0, 1, 1, 0, 1, 1, 1, 1, 0, 0, 1, 0, 0, 1],
        '5' => [1, 1, 1, 1, 0, 0, 1, 1, 1, 0, 0, 1, 1, 1, 1],
        '6' => [1, 1, 1, 1, 0, 0, 1, 1, 1, 1, 0, 1, 1, 1, 1],
        '7' => [1, 1, 1, 0, 0, 1, 0, 1, 0, 0, 1, 0, 0, 1, 0],
        '8' => [1, 1, 1, 1, 0, 1, 1, 1, 1, 1, 0, 1, 1, 1, 1],
        '9' => [1, 1, 1, 1, 0, 1, 1, 1, 1, 0, 0, 1, 1, 1, 1],
        '+' => [0, 1, 0, 0, 1, 0, 1, 1, 1, 0, 1, 0, 0, 1, 0],
        _ => return None,
    })
}

fn fill_rect(
    canvas: &mut [u8],
    canvas_w: u32,
    canvas_h: u32,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    color: [u8; 4],
) {
    for yy in 0..h {
        for xx in 0..w {
            blend_pixel(canvas, canvas_w, canvas_h, x + xx, y + yy, color);
        }
    }
}

fn stroke_rect(
    canvas: &mut [u8],
    canvas_w: u32,
    canvas_h: u32,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    color: [u8; 4],
) {
    fill_rect(canvas, canvas_w, canvas_h, x, y, w, 1, color);
    fill_rect(canvas, canvas_w, canvas_h, x, y + h - 1, w, 1, color);
    fill_rect(canvas, canvas_w, canvas_h, x, y, 1, h, color);
    fill_rect(canvas, canvas_w, canvas_h, x + w - 1, y, 1, h, color);
}

fn blend_pixel(canvas: &mut [u8], canvas_w: u32, canvas_h: u32, x: i32, y: i32, src: [u8; 4]) {
    if x < 0 || y < 0 || x >= canvas_w as i32 || y >= canvas_h as i32 {
        return;
    }
    let idx = ((y as u32 * canvas_w + x as u32) * 4) as usize;
    let dst = &mut canvas[idx..idx + 4];
    alpha_blend(dst, src);
}

fn alpha_blend(dst: &mut [u8], src: [u8; 4]) {
    let src_a = src[3] as u32;
    if src_a == 0 {
        return;
    }

    let dst_a = dst[3] as u32;
    let out_a = src_a + ((dst_a * (255 - src_a) + 127) / 255);

    for channel in 0..3 {
        let src_c = src[channel] as u32;
        let dst_c = dst[channel] as u32;
        let src_pre = src_c * src_a;
        let dst_pre = (dst_c * dst_a * (255 - src_a) + 127) / 255;
        let out_pre = src_pre + dst_pre;
        dst[channel] = if out_a == 0 {
            0
        } else {
            ((out_pre + out_a / 2) / out_a).min(255) as u8
        };
    }
    dst[3] = out_a.min(255) as u8;
}

#[cfg(target_os = "windows")]
mod platform {
    use std::cell::{Cell, RefCell};
    use std::collections::{HashMap, HashSet};
    use std::ffi::c_void;
    use std::mem::{self, ManuallyDrop};
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::io::{AsRawHandle, FromRawHandle, RawHandle};
    use std::path::{Component, Path, PathBuf, Prefix};
    use std::rc::Rc;
    use std::time::Instant;

    use crate::title_ink_rgb;
    use windows::Win32::Foundation::{
        BOOL, COLORREF, DATA_S_SAMEFORMATETC, DRAGDROP_S_CANCEL, DRAGDROP_S_DROP,
        DRAGDROP_S_USEDEFAULTCURSORS, DV_E_FORMATETC, DV_E_TYMED, E_NOTIMPL, HANDLE, HWND,
        OLE_E_ADVISENOTSUPPORTED, POINT, RECT, S_FALSE, S_OK, SIZE,
    };
    use windows::Win32::Graphics::Gdi::{
        BI_RGB, BITMAPINFO, BITMAPINFOHEADER, CreateCompatibleDC, CreateDIBSection, CreateFontW,
        DIB_RGB_COLORS, DT_END_ELLIPSIS, DT_LEFT, DT_NOPREFIX, DT_SINGLELINE, DT_VCENTER, DeleteDC,
        DeleteObject, DrawTextW, HBITMAP, HDC, HGDIOBJ, SelectObject, SetBkMode, SetTextColor,
        TRANSPARENT,
    };
    use windows::Win32::Storage::FileSystem::{
        CreateFileW, FILE_BASIC_INFO, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
        FILE_LIST_DIRECTORY, FILE_READ_ATTRIBUTES, FILE_READ_DATA, FILE_SHARE_DELETE,
        FILE_SHARE_READ, FILE_SHARE_WRITE, FILE_STANDARD_INFO, FileBasicInfo, FileStandardInfo,
        GetFileInformationByHandle, GetFileInformationByHandleEx, OPEN_EXISTING,
    };
    use windows::Win32::System::Com::Urlmon::CopyStgMedium;
    use windows::Win32::System::Com::{
        CLSCTX_INPROC_SERVER, CoCreateInstance, DATADIR_GET, DVASPECT_CONTENT, FORMATETC,
        IAdviseSink, IDataObject, IDataObject_Impl, IEnumFORMATETC, IEnumFORMATETC_Impl,
        IEnumSTATDATA, STGMEDIUM, STGMEDIUM_0, STREAM_SEEK, TYMED_HGLOBAL, TYMED_ISTREAM,
    };
    use windows::Win32::System::DataExchange::RegisterClipboardFormatW;
    use windows::Win32::System::Memory::{
        GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock,
    };
    use windows::Win32::System::Ole::{
        CF_HDROP, DROPEFFECT, DROPEFFECT_COPY, DoDragDrop, IDropSource, IDropSource_Impl,
        OleInitialize, OleUninitialize, ReleaseStgMedium,
    };
    use windows::Win32::System::SystemServices::MK_LBUTTON;
    use windows::Win32::UI::Input::KeyboardAndMouse::{GetAsyncKeyState, VK_LBUTTON};
    use windows::Win32::UI::Input::KeyboardAndMouse::{GetCapture, ReleaseCapture};
    use windows::Win32::UI::Shell::{
        CLSID_DragDropHelper, DROPFILES, IDragSourceHelper, SHDRAGIMAGE,
    };
    use windows::Win32::UI::WindowsAndMessaging::{
        GetCursorPos, HWND_TOPMOST, IsWindow, SW_HIDE, SWP_NOACTIVATE, SWP_NOSIZE, SWP_NOZORDER,
        SWP_SHOWWINDOW, SetWindowPos, ShowWindow,
    };
    use windows::core::{Error, PCWSTR, implement};

    use crate::{
        DragError, DragExecution, DragGhostWindow, DragOptions, DragResult, DragVisual,
        FileDragLeafSignature, FileDragParentIdentity, FileDragSource, PerformedEffectReport,
        PinnedFileDragSources, parse_shell_performed_effect,
    };

    #[cfg(test)]
    pub(super) fn file_drag_identity_for_tests(path: &std::path::Path) -> (u32, u64) {
        use windows::Win32::Storage::FileSystem::FILE_SHARE_DELETE;

        let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
        let handle = unsafe {
            CreateFileW(
                PCWSTR(wide.as_ptr()),
                FILE_READ_ATTRIBUTES.0,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                None,
                OPEN_EXISTING,
                FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
                None,
            )
        }
        .expect("open test source");
        let file = unsafe { std::fs::File::from_raw_handle(handle.0 as RawHandle) };
        let mut info = Default::default();
        unsafe {
            GetFileInformationByHandle(HANDLE(file.as_raw_handle() as *mut c_void), &mut info)
        }
        .expect("read identity from test handle");
        (
            info.dwVolumeSerialNumber,
            (u64::from(info.nFileIndexHigh) << 32) | u64::from(info.nFileIndexLow),
        )
    }

    #[cfg(test)]
    pub(super) fn file_drag_leaf_signature_for_tests(
        path: &std::path::Path,
    ) -> crate::FileDragLeafSignature {
        let file = std::fs::File::open(path).expect("open test source for signature");
        leaf_signature(&file).expect("read test source signature")
    }

    #[cfg(test)]
    pub(super) fn file_drag_identity_from_handle_for_tests(file: &std::fs::File) -> (u32, u64) {
        use std::os::windows::io::AsRawHandle;
        use windows::Win32::Foundation::HANDLE;
        use windows::Win32::Storage::FileSystem::GetFileInformationByHandle;

        let mut info = Default::default();
        unsafe {
            GetFileInformationByHandle(HANDLE(file.as_raw_handle() as *mut c_void), &mut info)
        }
        .expect("read identity from retained test handle");
        (
            info.dwVolumeSerialNumber,
            (u64::from(info.nFileIndexHigh) << 32) | u64::from(info.nFileIndexLow),
        )
    }

    #[cfg(test)]
    pub(super) fn can_open_source_for_delete_for_tests(path: &std::path::Path) -> Result<(), i32> {
        use windows::Win32::Foundation::CloseHandle;
        use windows::Win32::Storage::FileSystem::{
            FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
        };

        let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
        let handle = unsafe {
            CreateFileW(
                PCWSTR(wide.as_ptr()),
                0x0001_0000,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                None,
                OPEN_EXISTING,
                FILE_FLAG_BACKUP_SEMANTICS,
                None,
            )
        }
        .map_err(|error| error.code().0)?;
        unsafe {
            let _ = CloseHandle(handle);
        }
        Ok(())
    }

    #[cfg(test)]
    pub(super) fn can_open_source_for_write_for_tests(path: &std::path::Path) -> Result<(), i32> {
        use windows::Win32::Foundation::CloseHandle;
        let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
        let handle = unsafe {
            CreateFileW(
                PCWSTR(wide.as_ptr()),
                windows::Win32::Storage::FileSystem::FILE_WRITE_DATA.0,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                None,
                OPEN_EXISTING,
                FILE_FLAG_BACKUP_SEMANTICS,
                None,
            )
        }
        .map_err(|error| error.code().0)?;
        unsafe {
            let _ = CloseHandle(handle);
        }
        Ok(())
    }

    #[cfg(test)]
    pub(super) fn open_existing_writer_for_tests(path: &std::path::Path) -> std::fs::File {
        use std::os::windows::io::FromRawHandle;

        let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
        let handle = unsafe {
            CreateFileW(
                PCWSTR(wide.as_ptr()),
                windows::Win32::Storage::FileSystem::FILE_WRITE_DATA.0,
                FILE_SHARE_READ | FILE_SHARE_DELETE,
                None,
                OPEN_EXISTING,
                FILE_FLAG_BACKUP_SEMANTICS,
                None,
            )
        }
        .expect("open test writer");
        unsafe { std::fs::File::from_raw_handle(handle.0 as RawHandle) }
    }

    #[cfg(test)]
    pub(super) fn can_open_source_read_without_sharing_for_tests(
        path: &std::path::Path,
    ) -> Result<(), i32> {
        use windows::Win32::Foundation::CloseHandle;

        let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
        let handle = unsafe {
            CreateFileW(
                PCWSTR(wide.as_ptr()),
                FILE_READ_DATA.0,
                windows::Win32::Storage::FileSystem::FILE_SHARE_MODE(0),
                None,
                OPEN_EXISTING,
                FILE_FLAG_BACKUP_SEMANTICS,
                None,
            )
        }
        .map_err(|error| error.code().0)?;
        unsafe {
            let _ = CloseHandle(handle);
        }
        Ok(())
    }

    pub(super) fn pin_file_drag_sources(
        sources: &[FileDragSource],
        policy: crate::FileDragPinPolicy,
    ) -> Result<PinnedFileDragSources, DragError> {
        if policy.permits_move() && sources.iter().any(|source| source.is_directory) {
            return Err(DragError::InvalidInput(
                "Move-capable drag supports regular files only".to_string(),
            ));
        }
        let input_paths = sources
            .iter()
            .map(|source| source.path.clone())
            .collect::<Vec<_>>();
        let paths = super::normalize_paths(&input_paths)?;
        if paths.len() != sources.len() {
            return Err(DragError::InvalidInput("duplicate source path".to_string()));
        }

        // CF_HDROP は path text を渡すため、解決に使う全 directory を
        // root から保持して、ancestor の rename/replacement を拒否する。
        let mut parent_chains = Vec::with_capacity(sources.len());
        for source in sources {
            parent_chains.push(source_parent_chain(&source.path)?);
        }

        let mut parent_handles = Vec::new();
        let mut pinned_directory_paths = HashSet::new();
        let mut parent_identities = HashMap::new();
        for chain in &parent_chains {
            // Each chain is ordered root-to-leaf. Reopening shared ancestors is
            // unnecessary when the exact path is already held without delete share.
            for directory in chain {
                if !pinned_directory_paths.insert(directory.clone()) {
                    continue;
                }
                let (handle, info) = open_drag_pin(
                    directory,
                    FILE_LIST_DIRECTORY.0,
                    FILE_SHARE_READ | FILE_SHARE_WRITE,
                )?;
                if info.dwFileAttributes & 0x400 != 0 || info.dwFileAttributes & 0x10 == 0 {
                    return Err(DragError::InvalidInput(
                        "source parent chain contains a reparse point or non-directory".to_string(),
                    ));
                }
                parent_identities.insert(
                    directory.clone(),
                    FileDragParentIdentity {
                        path: directory.clone(),
                        volume_serial: info.dwVolumeSerialNumber,
                        file_index: file_index(&info),
                    },
                );
                parent_handles.push(handle);
            }
        }

        // Pin the captured leaves after their paths are protected by the parent
        // chains. Keep the existing leaf identity and reparse-point checks.
        let mut leaf_handles = Vec::with_capacity(sources.len());
        let mut leaf_signatures = Vec::with_capacity(sources.len());
        for source in sources {
            let access = if source.is_directory {
                FILE_LIST_DIRECTORY.0
            } else {
                FILE_READ_DATA.0
            };
            let leaf_share_mode = if policy.permits_move() {
                // Move permits replacement but prevents in-place mutation.
                FILE_SHARE_READ | FILE_SHARE_DELETE
            } else {
                FILE_SHARE_READ | FILE_SHARE_WRITE
            };
            let (validated_file, info) = open_drag_pin(&source.path, access, leaf_share_mode)?;
            if info.dwFileAttributes & 0x400 != 0
                || (info.dwFileAttributes & 0x10 != 0) != source.is_directory
                || info.dwVolumeSerialNumber != source.volume_serial
                || file_index(&info) != source.file_index
            {
                return Err(DragError::InvalidInput(
                    "source identity changed or source is a reparse point".to_string(),
                ));
            }
            if policy.permits_move() {
                let metadata = validated_file.metadata().map_err(|_| {
                    DragError::InvalidInput("could not read pinned source metadata".to_string())
                })?;
                let modified = metadata.modified().ok();
                // Size and timestamp are a best-effort signature; same-size rewrites with an
                // equal or preserved timestamp can evade this check.
                if source.captured_size != Some(metadata.len())
                    || source.captured_modified.is_none()
                    || modified != source.captured_modified
                {
                    return Err(DragError::InvalidInput(
                        "source changed since drag capture".to_string(),
                    ));
                }
                let signature = leaf_signature(&validated_file)?;
                leaf_signatures.push(signature);
            }
            leaf_handles.push(validated_file);
        }

        // A path can have changed while its parent chain was being acquired.
        // Confirm the captured identity at OLE handoff; Move-capable CF_HDROP
        // still permits a later leaf replacement while the drag is in progress.
        for source in sources {
            let share_mode = if policy.permits_move() {
                FILE_SHARE_READ | FILE_SHARE_DELETE
            } else {
                FILE_SHARE_READ | FILE_SHARE_WRITE
            };
            let (probe, info) = open_drag_pin(&source.path, FILE_READ_ATTRIBUTES.0, share_mode)?;
            let matches_source = info.dwFileAttributes & 0x400 == 0
                && (info.dwFileAttributes & 0x10 != 0) == source.is_directory
                && info.dwVolumeSerialNumber == source.volume_serial
                && file_index(&info) == source.file_index;
            drop(probe);
            if !matches_source {
                return Err(DragError::InvalidInput(
                    "source path changed while pinning its parent chain".to_string(),
                ));
            }
        }

        Ok(PinnedFileDragSources {
            paths,
            _parent_handles: parent_handles,
            parent_routes: parent_chains
                .iter()
                .map(|chain| {
                    chain
                        .iter()
                        .map(|directory| {
                            parent_identities
                                .get(directory)
                                .cloned()
                                .expect("every pinned parent has an identity")
                        })
                        .collect()
                })
                .collect(),
            leaf_handles,
            leaf_signatures,
            policy,
        })
    }

    fn source_parent_chain(path: &Path) -> Result<Vec<PathBuf>, DragError> {
        let supported = path.is_absolute()
            && matches!(
                path.components().next(),
                Some(Component::Prefix(prefix)) if matches!(prefix.kind(), Prefix::Disk(_))
            )
            && path
                .components()
                .all(|component| !matches!(component, Component::ParentDir));
        if !supported {
            return Err(DragError::InvalidInput(
                "source path must be an absolute drive path without parent traversal".to_string(),
            ));
        }
        let parent = path.parent().ok_or_else(|| {
            DragError::InvalidInput("source path has no parent directory".to_string())
        })?;
        let mut chain = parent
            .ancestors()
            .map(Path::to_path_buf)
            .collect::<Vec<_>>();
        chain.reverse();
        if chain.is_empty() || !chain[0].is_absolute() {
            return Err(DragError::InvalidInput(
                "source parent chain is not rooted on a local volume".to_string(),
            ));
        }
        Ok(chain)
    }

    fn open_drag_pin(
        path: &Path,
        desired_access: u32,
        share_mode: windows::Win32::Storage::FileSystem::FILE_SHARE_MODE,
    ) -> Result<
        (
            std::fs::File,
            windows::Win32::Storage::FileSystem::BY_HANDLE_FILE_INFORMATION,
        ),
        DragError,
    > {
        let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
        let handle = unsafe {
            CreateFileW(
                PCWSTR(wide.as_ptr()),
                desired_access | FILE_READ_ATTRIBUTES.0,
                share_mode,
                None,
                OPEN_EXISTING,
                FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
                None,
            )
        }
        .map_err(|error| {
            DragError::InvalidInput(format!(
                "could not open source path without following reparse points: {error}"
            ))
        })?;
        let file = unsafe { std::fs::File::from_raw_handle(handle.0 as RawHandle) };
        let mut info = Default::default();
        unsafe {
            GetFileInformationByHandle(HANDLE(file.as_raw_handle() as *mut c_void), &mut info)
        }
        .map_err(|error| {
            DragError::InvalidInput(format!(
                "could not inspect source handle without following reparse points: {error}"
            ))
        })?;
        Ok((file, info))
    }

    fn leaf_signature(file: &std::fs::File) -> Result<FileDragLeafSignature, DragError> {
        use std::os::windows::io::AsRawHandle;

        let handle = windows::Win32::Foundation::HANDLE(file.as_raw_handle() as *mut _);
        let mut standard = FILE_STANDARD_INFO::default();
        let mut basic = FILE_BASIC_INFO::default();
        unsafe {
            GetFileInformationByHandleEx(
                handle,
                FileStandardInfo,
                &mut standard as *mut _ as *mut _,
                std::mem::size_of::<FILE_STANDARD_INFO>() as u32,
            )
            .map_err(|_| DragError::InvalidInput("could not read pinned link count".into()))?;
            GetFileInformationByHandleEx(
                handle,
                FileBasicInfo,
                &mut basic as *mut _ as *mut _,
                std::mem::size_of::<FILE_BASIC_INFO>() as u32,
            )
            .map_err(|_| DragError::InvalidInput("could not read pinned change time".into()))?;
        }
        if standard.NumberOfLinks == 0 {
            return Err(DragError::InvalidInput(
                "pinned source has no usable link count".into(),
            ));
        }
        Ok(FileDragLeafSignature {
            number_of_links: standard.NumberOfLinks,
            change_time: basic.ChangeTime,
        })
    }

    fn file_index(info: &windows::Win32::Storage::FileSystem::BY_HANDLE_FILE_INFORMATION) -> u64 {
        (u64::from(info.nFileIndexHigh) << 32) | u64::from(info.nFileIndexLow)
    }

    pub(super) fn release_source_capture(source_hwnd: isize) -> Result<(), DragError> {
        use windows::Win32::Foundation::HWND;

        let expected = HWND(source_hwnd as *mut std::ffi::c_void);
        let captured = unsafe { GetCapture() };
        if !source_owns_capture(expected, captured) {
            return Err(DragError::InvalidInput(
                "mouse capture is no longer owned by the source window".to_string(),
            ));
        }
        unsafe { ReleaseCapture() }
            .map_err(|error| DragError::Com(format!("ReleaseCapture failed: {error}")))?;
        Ok(())
    }

    fn source_owns_capture(expected: HWND, captured: HWND) -> bool {
        !expected.0.is_null() && captured == expected
    }

    #[cfg(test)]
    mod capture_tests {
        use super::source_owns_capture;
        use windows::Win32::Foundation::HWND;

        #[test]
        fn capture_handoff_requires_the_exact_non_null_source_window() {
            let expected = HWND(1usize as *mut std::ffi::c_void);
            assert!(source_owns_capture(expected, expected));
            assert!(!source_owns_capture(
                expected,
                HWND(2usize as *mut std::ffi::c_void)
            ));
            assert!(!source_owns_capture(
                HWND(std::ptr::null_mut()),
                HWND(std::ptr::null_mut())
            ));
        }
    }

    #[allow(unexpected_cfgs)]
    #[cfg(not(rust_analyzer))]
    fn not_impl_error(message: impl AsRef<str>) -> Error {
        Error::new(E_NOTIMPL, message)
    }

    #[allow(unexpected_cfgs)]
    #[cfg(rust_analyzer)]
    fn not_impl_error(_message: impl AsRef<str>) -> Error {
        // rust-analyzer が Error::new の型を誤推論するケース向けの回避。
        Error::from(E_NOTIMPL)
    }

    pub(super) fn begin_file_drag_impl(
        paths: &[PathBuf],
        options: &DragOptions,
        allowed_effects: u32,
    ) -> Result<DragExecution, DragError> {
        let visual = options_visual(options);
        let ghost_window = options
            .ghost_window
            .map(GhostWindowFollower::from_options)
            .transpose()?;

        unsafe {
            OleInitialize(None)
                .map_err(|err| DragError::Com(format!("OleInitialize failed: {err}")))?;
        }
        let _ole_guard = OleGuard;

        let data_object_impl = FileDataObject::new(paths, options.local_drag_session_id)?;
        let performed_report = data_object_impl.performed_drop_effect.clone();
        let data_object: IDataObject = data_object_impl.into();
        let (mut visual_warning, _drag_image) = if let Some((visual, scale_milli)) = visual {
            match attach_drag_image(&data_object, visual, scale_milli) {
                Ok(guard) => (None, Some(guard)),
                Err(err) => (Some(format_drag_image_error(&err)), None),
            }
        } else {
            (None, None)
        };
        let _ghost_guard = ghost_window.map(DragGhostGuard);
        if let Some(ghost) = ghost_window {
            ghost.show_at_cursor();
        }
        let touch_drag_cancelled = options.touch_drag_cancelled.clone();
        let drop_source: IDropSource =
            FileDropSource::new(ghost_window, touch_drag_cancelled.clone()).into();

        // DoDragDrop はボタン状態の変化がないと QueryContinueDrag を呼ばないことがある。
        // Mouse は OLE の drag loop 前に release 済みなら loop 自体を開始しない。
        // Touch は合成 mouse 状態の反映時刻に依存せず、共有 contact signal と OLE 入力で終端する。
        let touch_was_cancelled = touch_drag_cancelled
            .as_ref()
            .is_some_and(|cancelled| cancelled.load(std::sync::atomic::Ordering::Acquire));
        let mouse_was_released =
            touch_drag_cancelled.is_none() && unsafe { GetAsyncKeyState(VK_LBUTTON.0 as i32) >= 0 };
        if touch_was_cancelled || mouse_was_released {
            return Ok(crate::drag_execution_with_shell_report(
                DragResult::Cancelled,
                None,
                performed_report.get(),
                visual_warning.take(),
            ));
        }

        let mut performed = DROPEFFECT(0);
        if file_drop_trace_enabled() {
            eprintln!(
                "[file-drop] t_us={} DoDragDrop begin",
                file_drop_trace_time_micros()
            );
        }
        let hr = unsafe {
            DoDragDrop(
                &data_object,
                &drop_source,
                DROPEFFECT(allowed_effects),
                &mut performed,
            )
        };

        if hr == DRAGDROP_S_DROP {
            return Ok(crate::drag_execution_with_shell_report(
                DragResult::Dropped,
                Some(performed.0),
                performed_report.get(),
                visual_warning.take(),
            ));
        }
        if hr == DRAGDROP_S_CANCEL {
            return Ok(crate::drag_execution_with_shell_report(
                DragResult::Cancelled,
                None,
                performed_report.get(),
                visual_warning.take(),
            ));
        }
        Err(DragError::Com(format!(
            "DoDragDrop failed: 0x{:08X}",
            hr.0 as u32
        )))
    }

    fn options_visual(options: &DragOptions) -> Option<(&DragVisual, Option<u32>)> {
        match &options.visual {
            crate::DragVisualSource::Auto => None,
            crate::DragVisualSource::Custom(visual) => Some((visual, None)),
            crate::DragVisualSource::CustomScaled {
                visual,
                scale_milli,
            } => Some((visual, Some(crate::clamp_drag_scale_milli(*scale_milli)))),
            crate::DragVisualSource::None => None,
        }
    }

    fn format_drag_image_error(err: &DragError) -> String {
        match err {
            DragError::Com(msg) | DragError::InvalidInput(msg) => msg.clone(),
            DragError::UnsupportedPlatform => "unsupported platform".to_string(),
        }
    }

    struct OleGuard;
    impl Drop for OleGuard {
        fn drop(&mut self) {
            unsafe {
                OleUninitialize();
            }
        }
    }

    struct DragBitmapGuard(HBITMAP);
    impl Drop for DragBitmapGuard {
        fn drop(&mut self) {
            let _ = unsafe { DeleteObject(self.0) };
        }
    }

    struct DcGuard(HDC);
    impl Drop for DcGuard {
        fn drop(&mut self) {
            let _ = unsafe { DeleteDC(self.0) };
        }
    }

    struct SelectedObjectGuard {
        dc: HDC,
        old: HGDIOBJ,
    }
    impl Drop for SelectedObjectGuard {
        fn drop(&mut self) {
            let _ = unsafe { SelectObject(self.dc, self.old) };
        }
    }

    struct SelectedFontGuard {
        dc: HDC,
        old: HGDIOBJ,
        font: HGDIOBJ,
    }
    impl Drop for SelectedFontGuard {
        fn drop(&mut self) {
            unsafe {
                let _ = SelectObject(self.dc, self.old);
                let _ = DeleteObject(self.font);
            }
        }
    }

    #[derive(Clone, Copy)]
    struct GhostWindowFollower {
        hwnd: HWND,
        offset_x: i32,
        offset_y: i32,
    }

    impl GhostWindowFollower {
        fn from_options(options: DragGhostWindow) -> Result<Self, DragError> {
            let follower = Self {
                hwnd: HWND(options.hwnd as *mut c_void),
                offset_x: options.offset_x,
                offset_y: options.offset_y,
            };
            if !unsafe { IsWindow(follower.hwnd).as_bool() } {
                return Err(DragError::InvalidInput(
                    "ghost window hwnd is not valid".to_string(),
                ));
            }
            Ok(follower)
        }

        fn show_at_cursor(self) {
            self.move_to_cursor(true);
        }

        fn hide(self) {
            unsafe {
                let _ = ShowWindow(self.hwnd, SW_HIDE);
            }
        }

        fn move_to_cursor(self, show: bool) {
            let mut cursor = POINT::default();
            if unsafe { GetCursorPos(&mut cursor) }.is_err() {
                return;
            }
            let mut flags = SWP_NOSIZE | SWP_NOACTIVATE;
            let z_order = if show {
                flags |= SWP_SHOWWINDOW;
                HWND_TOPMOST
            } else {
                flags |= SWP_NOZORDER;
                HWND(std::ptr::null_mut())
            };
            unsafe {
                let _ = SetWindowPos(
                    self.hwnd,
                    z_order,
                    cursor.x + self.offset_x,
                    cursor.y + self.offset_y,
                    0,
                    0,
                    flags,
                );
            }
        }
    }

    struct DragGhostGuard(GhostWindowFollower);
    impl Drop for DragGhostGuard {
        fn drop(&mut self) {
            self.0.hide();
        }
    }

    fn attach_drag_image(
        data_object: &IDataObject,
        visual: &DragVisual,
        scale_milli: Option<u32>,
    ) -> Result<DragBitmapGuard, DragError> {
        let hbitmap = create_drag_bitmap(visual, scale_milli)
            .map_err(|err| DragError::Com(format!("Create drag bitmap failed: {err}")))?;
        let guard = DragBitmapGuard(hbitmap);

        let drag_source_helper: IDragSourceHelper = unsafe {
            CoCreateInstance(&CLSID_DragDropHelper, None, CLSCTX_INPROC_SERVER).map_err(|err| {
                DragError::Com(format!("CoCreateInstance(IDragSourceHelper) failed: {err}"))
            })?
        };

        let drag_image = SHDRAGIMAGE {
            sizeDragImage: SIZE {
                cx: visual.width as i32,
                cy: visual.height as i32,
            },
            ptOffset: POINT {
                x: visual.hotspot_x,
                y: visual.hotspot_y,
            },
            hbmpDragImage: hbitmap,
            crColorKey: COLORREF(0),
        };

        if file_drop_trace_enabled() {
            eprintln!(
                "[file-drop] t_us={} drag-image prepare scale_milli={} bitmap={}x{} shdragimage={}x{} hotspot={},{}",
                file_drop_trace_time_micros(),
                scale_milli.unwrap_or(1000),
                visual.width,
                visual.height,
                drag_image.sizeDragImage.cx,
                drag_image.sizeDragImage.cy,
                visual.hotspot_x,
                visual.hotspot_y,
            );
        }

        unsafe {
            drag_source_helper
                .InitializeFromBitmap(&drag_image, data_object)
                .map_err(|err| {
                    DragError::Com(format!(
                        "IDragSourceHelper::InitializeFromBitmap failed: {err}"
                    ))
                })?;
        }

        if file_drop_trace_enabled() {
            eprintln!(
                "[file-drop] t_us={} drag-image attached=true",
                file_drop_trace_time_micros()
            );
        }

        Ok(guard)
    }

    fn create_drag_bitmap(
        visual: &DragVisual,
        scale_milli: Option<u32>,
    ) -> Result<HBITMAP, String> {
        let mut info = BITMAPINFO::default();
        info.bmiHeader = BITMAPINFOHEADER {
            biSize: mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: visual.width as i32,
            // top-down DIB にしてメモリ上の行順序をそのまま使う
            biHeight: -(visual.height as i32),
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            biSizeImage: (visual.width * visual.height * 4) as u32,
            ..Default::default()
        };

        let mut bits_ptr: *mut c_void = std::ptr::null_mut();
        let hbitmap = unsafe {
            CreateDIBSection(None, &info, DIB_RGB_COLORS, &mut bits_ptr, None, 0)
                .map_err(|err| format!("CreateDIBSection failed: {err}"))?
        };
        if bits_ptr.is_null() {
            return Err("CreateDIBSection returned null bits pointer".to_string());
        }

        let pixel_bytes = (visual.width * visual.height * 4) as usize;
        let dst = unsafe { std::slice::from_raw_parts_mut(bits_ptr as *mut u8, pixel_bytes) };
        rgba_to_bgra(&visual.rgba, dst);
        draw_visual_text_on_bitmap(hbitmap, visual, scale_milli)?;

        Ok(hbitmap)
    }

    fn rgba_to_bgra(src: &[u8], dst: &mut [u8]) {
        for (src_px, dst_px) in src.chunks_exact(4).zip(dst.chunks_exact_mut(4)) {
            dst_px[0] = src_px[2];
            dst_px[1] = src_px[1];
            dst_px[2] = src_px[0];
            dst_px[3] = src_px[3];
        }
    }

    fn draw_visual_text_on_bitmap(
        hbitmap: HBITMAP,
        visual: &DragVisual,
        scale_milli: Option<u32>,
    ) -> Result<(), String> {
        if visual.title.is_none() && visual.subtitle.is_none() {
            return Ok(());
        }

        let dc = unsafe { CreateCompatibleDC(None) };
        if dc.is_invalid() {
            return Err("CreateCompatibleDC returned invalid handle".to_string());
        }
        let _dc_guard = DcGuard(dc);

        let old = unsafe { SelectObject(dc, hbitmap) };
        if old.is_invalid() {
            return Err("SelectObject(hbitmap) returned invalid handle".to_string());
        }
        let _select_guard = SelectedObjectGuard { dc, old };

        unsafe {
            let _ = SetBkMode(dc, TRANSPARENT);
        }
        let (title_rect, subtitle_rect) = compute_text_rects(visual, scale_milli);

        if let Some(title) = visual.title.as_deref() {
            let mut text = encode_wide_null(title);
            let mut rect = title_rect;
            let title_font = unsafe {
                let font_name = encode_wide_null("Segoe UI");
                let font = CreateFontW(
                    -scale_milli.map_or(20, |scale| super::scaled_logical_px(20, scale)),
                    0,
                    0,
                    0,
                    600,
                    0,
                    0,
                    0,
                    1,
                    0,
                    0,
                    0,
                    0,
                    windows::core::PCWSTR(font_name.as_ptr()),
                );
                if font.is_invalid() {
                    None
                } else {
                    let font_object = HGDIOBJ(font.0);
                    let old_font = SelectObject(dc, font_object);
                    if old_font.is_invalid() {
                        let _ = DeleteObject(font_object);
                        None
                    } else {
                        Some(SelectedFontGuard {
                            dc,
                            old: old_font,
                            font: font_object,
                        })
                    }
                }
            };
            unsafe {
                let _ = SetTextColor(dc, {
                    let [r, g, b] = title_ink_rgb(
                        &visual.rgba,
                        visual.width,
                        visual.height,
                        (
                            title_rect.left,
                            title_rect.top,
                            title_rect.right,
                            title_rect.bottom,
                        ),
                    );
                    rgb(r, g, b)
                });
                let _ = DrawTextW(
                    dc,
                    &mut text,
                    &mut rect,
                    DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_END_ELLIPSIS | DT_NOPREFIX,
                );
            }
            drop(title_font);
        }

        if let Some(subtitle) = visual.subtitle.as_deref() {
            let mut text = encode_wide_null(subtitle);
            let mut rect = subtitle_rect;
            unsafe {
                let _ = SetTextColor(dc, rgb(161, 179, 212));
                let _ = DrawTextW(
                    dc,
                    &mut text,
                    &mut rect,
                    DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_END_ELLIPSIS | DT_NOPREFIX,
                );
            }
        }

        Ok(())
    }

    pub(super) fn compute_text_rects(
        visual: &DragVisual,
        scale_milli: Option<u32>,
    ) -> (RECT, RECT) {
        let (title, subtitle) = if let Some(scale) = scale_milli {
            super::text_rects_for_scale(scale)
        } else {
            let width = visual.width as i32;
            let height = visual.height as i32;

            // 任意サイズのcustom visualでは従来どおりcanvas寸法に合わせる。
            let min_left = 52;
            let max_left = (width - 24).max(min_left);
            let left = ((width * 40) / 100).clamp(min_left, max_left);
            let right = (width - 12).max(left + 12);

            let min_top = 12;
            let max_top = (height - 30).max(min_top);
            let title_top = ((height * 22) / 100).clamp(min_top, max_top);
            let title_height = ((height * 28) / 100).clamp(14, 28);
            let subtitle_height = ((height * 18) / 100).clamp(14, 28);
            let line_gap = ((height * 6) / 100).clamp(4, 10);

            let title_bottom = (title_top + title_height).min(height - 4);
            let subtitle_top = (title_bottom + line_gap).min(height - 6);
            let subtitle_bottom = (subtitle_top + subtitle_height).min(height - 2);
            (
                (left, title_top, right, title_bottom.max(title_top + 1)),
                (
                    left,
                    subtitle_top,
                    right,
                    subtitle_bottom.max(subtitle_top + 1),
                ),
            )
        };

        (
            RECT {
                left: title.0,
                top: title.1,
                right: title.2,
                bottom: title.3,
            },
            RECT {
                left: subtitle.0,
                top: subtitle.1,
                right: subtitle.2,
                bottom: subtitle.3,
            },
        )
    }

    fn encode_wide_null(text: &str) -> Vec<u16> {
        text.encode_utf16().chain(std::iter::once(0)).collect()
    }

    fn rgb(r: u8, g: u8, b: u8) -> COLORREF {
        COLORREF((r as u32) | ((g as u32) << 8) | ((b as u32) << 16))
    }

    #[derive(Clone, Copy)]
    enum FormatKind {
        FileDrop,
        PreferredDropEffect,
    }

    #[derive(Clone)]
    struct StoredFormat {
        format: FORMATETC,
        bytes: Vec<u8>,
    }

    struct OwnedStgMedium(STGMEDIUM);

    impl Drop for OwnedStgMedium {
        fn drop(&mut self) {
            unsafe { ReleaseStgMedium(&mut self.0) };
        }
    }

    #[implement(IDataObject)]
    struct FileDataObject {
        wide_paths: Vec<u16>,
        preferred_drop_effect_format: u16,
        performed_drop_effect_format: u16,
        drag_context_format: u16,
        performed_drop_effect: Rc<Cell<PerformedEffectReport>>,
        extra_formats: RefCell<Vec<StoredFormat>>,
        drag_context: RefCell<Option<OwnedStgMedium>>,
    }

    fn file_drop_trace_enabled() -> bool {
        std::env::var_os("GPUI_FILE_DROP_TRACE").is_some()
    }

    fn file_drop_trace_time_micros() -> u128 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |duration| duration.as_micros())
    }

    fn trace_data_object_format(
        operation: &str,
        format: Option<&FORMATETC>,
        result: windows::core::HRESULT,
    ) {
        if !file_drop_trace_enabled() {
            return;
        }
        if let Some(format) = format {
            eprintln!(
                "[file-drop] t_us={} IDataObject::{operation} cfFormat={} dwAspect={} lindex={} tymed={} hr=0x{:08x}",
                file_drop_trace_time_micros(),
                format.cfFormat,
                format.dwAspect,
                format.lindex,
                format.tymed,
                result.0 as u32,
            );
        } else {
            eprintln!(
                "[file-drop] t_us={} IDataObject::{operation} format=null hr=0x{:08x}",
                file_drop_trace_time_micros(),
                result.0 as u32,
            );
        }
    }

    fn trace_get_data_result(format: &FORMATETC, result: &windows::core::Result<STGMEDIUM>) {
        if !file_drop_trace_enabled() {
            return;
        }
        match result {
            Ok(medium) => eprintln!(
                "[file-drop] t_us={} IDataObject::GetData cfFormat={} dwAspect={} lindex={} format_tymed={} medium_tymed={} pUnkForRelease={} hr=0x00000000",
                file_drop_trace_time_micros(),
                format.cfFormat,
                format.dwAspect,
                format.lindex,
                format.tymed,
                medium.tymed,
                medium.pUnkForRelease.is_some(),
            ),
            Err(error) => eprintln!(
                "[file-drop] t_us={} IDataObject::GetData cfFormat={} dwAspect={} lindex={} tymed={} hr=0x{:08x}",
                file_drop_trace_time_micros(),
                format.cfFormat,
                format.dwAspect,
                format.lindex,
                format.tymed,
                error.code().0 as u32,
            ),
        }
    }

    fn trace_set_data(
        format: Option<&FORMATETC>,
        medium_tymed: Option<u32>,
        frelease: bool,
        stored: bool,
        result: windows::core::HRESULT,
    ) {
        if !file_drop_trace_enabled() {
            return;
        }
        if let (Some(format), Some(medium_tymed)) = (format, medium_tymed) {
            eprintln!(
                "[file-drop] t_us={} IDataObject::SetData cfFormat={} dwAspect={} lindex={} format_tymed={} medium_tymed={} frelease={} stored={} hr=0x{:08x}",
                file_drop_trace_time_micros(),
                format.cfFormat,
                format.dwAspect,
                format.lindex,
                format.tymed,
                medium_tymed,
                frelease,
                stored,
                result.0 as u32,
            );
        } else {
            eprintln!(
                "[file-drop] t_us={} IDataObject::SetData format_or_medium=null frelease={} stored={} hr=0x{:08x}",
                file_drop_trace_time_micros(),
                frelease,
                stored,
                result.0 as u32,
            );
        }
    }

    impl FileDataObject {
        fn new(paths: &[PathBuf], local_drag_session_id: Option<u64>) -> Result<Self, DragError> {
            let wide_paths = build_wide_paths(paths);
            let format_name: Vec<u16> = "Preferred DropEffect\0".encode_utf16().collect();
            let format = unsafe { RegisterClipboardFormatW(PCWSTR(format_name.as_ptr())) };
            if format == 0 {
                return Err(DragError::Com(
                    "RegisterClipboardFormatW failed".to_string(),
                ));
            }
            let performed_name: Vec<u16> = "Performed DropEffect\0".encode_utf16().collect();
            let performed_format =
                unsafe { RegisterClipboardFormatW(PCWSTR(performed_name.as_ptr())) };
            if performed_format == 0 {
                return Err(DragError::Com(
                    "RegisterClipboardFormatW failed for performed drop effect".to_string(),
                ));
            }
            let drag_context_name: Vec<u16> = "DragContext\0".encode_utf16().collect();
            let drag_context_format =
                unsafe { RegisterClipboardFormatW(PCWSTR(drag_context_name.as_ptr())) };
            if drag_context_format == 0 {
                return Err(DragError::Com(
                    "RegisterClipboardFormatW failed for DragContext".to_string(),
                ));
            }
            let extra_formats = if let Some(session_id) = local_drag_session_id {
                let session_name: Vec<u16> =
                    "GPUI.Filer.InternalDragSession\0".encode_utf16().collect();
                let session_format =
                    unsafe { RegisterClipboardFormatW(PCWSTR(session_name.as_ptr())) };
                if session_format == 0 {
                    return Err(DragError::Com(
                        "RegisterClipboardFormatW failed for local drag session".to_string(),
                    ));
                }
                vec![StoredFormat {
                    format: FORMATETC {
                        cfFormat: session_format as u16,
                        ptd: std::ptr::null_mut(),
                        dwAspect: DVASPECT_CONTENT.0,
                        lindex: -1,
                        tymed: TYMED_HGLOBAL.0 as u32,
                    },
                    bytes: session_id.to_le_bytes().to_vec(),
                }]
            } else {
                Vec::new()
            };
            Ok(Self {
                wide_paths,
                preferred_drop_effect_format: format as u16,
                performed_drop_effect_format: performed_format as u16,
                drag_context_format: drag_context_format as u16,
                performed_drop_effect: Rc::new(Cell::new(PerformedEffectReport::Unreported)),
                extra_formats: RefCell::new(extra_formats),
                drag_context: RefCell::new(None),
            })
        }

        fn format_kind(&self, fmt: &FORMATETC) -> Result<FormatKind, Error> {
            if fmt.dwAspect != DVASPECT_CONTENT.0 || fmt.lindex != -1 {
                return Err(Error::from(DV_E_FORMATETC));
            }
            let has_hglobal = (fmt.tymed & TYMED_HGLOBAL.0 as u32) != 0;
            if !has_hglobal {
                return Err(Error::from(DV_E_TYMED));
            }
            if fmt.cfFormat == CF_HDROP.0 {
                return Ok(FormatKind::FileDrop);
            }
            if fmt.cfFormat == self.preferred_drop_effect_format {
                return Ok(FormatKind::PreferredDropEffect);
            }
            Err(Error::from(DV_E_FORMATETC))
        }

        fn query_extra_format(&self, fmt: &FORMATETC) -> Option<Vec<u8>> {
            self.extra_formats
                .borrow()
                .iter()
                .find(|stored| format_matches(fmt, &stored.format))
                .map(|stored| stored.bytes.clone())
        }

        fn drag_context_matches(&self, fmt: &FORMATETC) -> bool {
            fmt.cfFormat == self.drag_context_format
                && fmt.dwAspect == DVASPECT_CONTENT.0
                && fmt.lindex == -1
                && fmt.tymed & TYMED_ISTREAM.0 as u32 != 0
                && self.drag_context.borrow().is_some()
        }

        fn store_extra_format(&self, fmt: &FORMATETC, bytes: Vec<u8>) {
            let normalized = normalize_format(fmt);
            let mut extra_formats = self.extra_formats.borrow_mut();
            if let Some(existing) = extra_formats
                .iter_mut()
                .find(|stored| stored.format.cfFormat == normalized.cfFormat)
            {
                existing.format = normalized;
                existing.bytes = bytes;
                return;
            }
            extra_formats.push(StoredFormat {
                format: normalized,
                bytes,
            });
        }

        fn enumerated_formats(&self) -> Vec<FORMATETC> {
            let mut formats = vec![
                FORMATETC {
                    cfFormat: CF_HDROP.0,
                    ptd: std::ptr::null_mut(),
                    dwAspect: DVASPECT_CONTENT.0,
                    lindex: -1,
                    tymed: TYMED_HGLOBAL.0 as u32,
                },
                FORMATETC {
                    cfFormat: self.preferred_drop_effect_format,
                    ptd: std::ptr::null_mut(),
                    dwAspect: DVASPECT_CONTENT.0,
                    lindex: -1,
                    tymed: TYMED_HGLOBAL.0 as u32,
                },
            ];
            formats.extend(
                self.extra_formats
                    .borrow()
                    .iter()
                    .map(|stored| stored.format),
            );
            if self.drag_context.borrow().is_some() {
                formats.push(FORMATETC {
                    cfFormat: self.drag_context_format,
                    ptd: std::ptr::null_mut(),
                    dwAspect: DVASPECT_CONTENT.0,
                    lindex: -1,
                    tymed: TYMED_ISTREAM.0 as u32,
                });
            }
            formats
        }
    }

    impl IDataObject_Impl for FileDataObject_Impl {
        fn GetData(&self, pformatetcin: *const FORMATETC) -> windows::core::Result<STGMEDIUM> {
            if pformatetcin.is_null() {
                trace_data_object_format("GetData", None, DV_E_FORMATETC);
                return Err(Error::from(DV_E_FORMATETC));
            }
            let fmt = unsafe { &*pformatetcin };

            let result = (|| {
                if let Ok(kind) = self.format_kind(fmt) {
                    return match kind {
                        FormatKind::FileDrop => {
                            let hglobal =
                                alloc_hdrop_hglobal(&self.wide_paths).map_err(not_impl_error)?;
                            Ok(STGMEDIUM {
                                tymed: TYMED_HGLOBAL.0 as u32,
                                u: STGMEDIUM_0 { hGlobal: hglobal },
                                pUnkForRelease: ManuallyDrop::new(None),
                            })
                        }
                        FormatKind::PreferredDropEffect => {
                            let hglobal =
                                alloc_u32_hglobal(DROPEFFECT_COPY.0).map_err(not_impl_error)?;
                            Ok(STGMEDIUM {
                                tymed: TYMED_HGLOBAL.0 as u32,
                                u: STGMEDIUM_0 { hGlobal: hglobal },
                                pUnkForRelease: ManuallyDrop::new(None),
                            })
                        }
                    };
                }

                if self.drag_context_matches(fmt) {
                    let copied = {
                        let stored = self.drag_context.borrow();
                        let source = stored.as_ref().expect("matched stored DragContext");
                        OwnedStgMedium(unsafe { CopyStgMedium(&source.0)? })
                    };
                    let source_stream =
                        unsafe { copied.0.u.pstm.as_ref() }.expect("copied DragContext IStream");
                    let stream = unsafe { source_stream.Clone()? };
                    unsafe { stream.Seek(0, STREAM_SEEK(0), None)? };
                    drop(copied);
                    return Ok(STGMEDIUM {
                        tymed: TYMED_ISTREAM.0 as u32,
                        u: STGMEDIUM_0 {
                            pstm: ManuallyDrop::new(Some(stream)),
                        },
                        pUnkForRelease: ManuallyDrop::new(None),
                    });
                }

                if let Some(bytes) = self.query_extra_format(fmt) {
                    let hglobal = alloc_bytes_hglobal(&bytes).map_err(not_impl_error)?;
                    return Ok(STGMEDIUM {
                        tymed: TYMED_HGLOBAL.0 as u32,
                        u: STGMEDIUM_0 { hGlobal: hglobal },
                        pUnkForRelease: ManuallyDrop::new(None),
                    });
                }

                Err(Error::from(DV_E_FORMATETC))
            })();
            trace_get_data_result(fmt, &result);
            result
        }

        fn GetDataHere(
            &self,
            _pformatetc: *const FORMATETC,
            _pmedium: *mut STGMEDIUM,
        ) -> windows::core::Result<()> {
            Err(Error::from(E_NOTIMPL))
        }

        fn QueryGetData(&self, pformatetc: *const FORMATETC) -> windows::core::HRESULT {
            if pformatetc.is_null() {
                trace_data_object_format("QueryGetData", None, DV_E_FORMATETC);
                return DV_E_FORMATETC;
            }
            let fmt = unsafe { &*pformatetc };
            let result = if self.format_kind(fmt).is_ok()
                || self.drag_context_matches(fmt)
                || self.query_extra_format(fmt).is_some()
            {
                S_OK
            } else {
                DV_E_FORMATETC
            };
            trace_data_object_format("QueryGetData", Some(fmt), result);
            result
        }

        fn GetCanonicalFormatEtc(
            &self,
            _pformatectin: *const FORMATETC,
            pformatetcout: *mut FORMATETC,
        ) -> windows::core::HRESULT {
            if !pformatetcout.is_null() {
                unsafe {
                    (*pformatetcout).ptd = std::ptr::null_mut();
                }
            }
            DATA_S_SAMEFORMATETC
        }

        fn SetData(
            &self,
            pformatetc: *const FORMATETC,
            pmedium: *const STGMEDIUM,
            frelease: BOOL,
        ) -> windows::core::Result<()> {
            if pformatetc.is_null() || pmedium.is_null() {
                trace_set_data(None, None, false, false, DV_E_FORMATETC);
                return Err(Error::from(DV_E_FORMATETC));
            }

            let format = unsafe { &*pformatetc };
            let medium = unsafe { &*pmedium };
            let medium_tymed = medium.tymed;
            let mut stored = false;
            if format.cfFormat == self.drag_context_format {
                if format.dwAspect != DVASPECT_CONTENT.0
                    || format.lindex != -1
                    || format.tymed & TYMED_ISTREAM.0 as u32 == 0
                    || medium.tymed != TYMED_ISTREAM.0 as u32
                {
                    return Err(Error::from(DV_E_FORMATETC));
                }
                let copied = OwnedStgMedium(unsafe { CopyStgMedium(medium)? });
                let old = {
                    let mut stored = self.drag_context.borrow_mut();
                    std::mem::replace(&mut *stored, Some(copied))
                };
                drop(old);
                stored = true;
            } else if format.cfFormat == self.performed_drop_effect_format {
                let bytes = if medium.tymed == TYMED_HGLOBAL.0 as u32 {
                    clone_hglobal_bytes(unsafe { medium.u.hGlobal }).ok()
                } else {
                    None
                };
                let report = parse_shell_performed_effect(
                    format.cfFormat,
                    self.performed_drop_effect_format,
                    format.dwAspect == DVASPECT_CONTENT.0
                        && format.lindex == -1
                        && format.tymed & TYMED_HGLOBAL.0 as u32 != 0,
                    medium.tymed == TYMED_HGLOBAL.0 as u32,
                    bytes.as_deref().unwrap_or_default(),
                );
                let mut state = self.performed_drop_effect.get();
                state.record(report);
                self.performed_drop_effect.set(state);
            } else if medium.tymed == TYMED_HGLOBAL.0 as u32 {
                let bytes = match clone_hglobal_bytes(unsafe { medium.u.hGlobal }) {
                    Ok(bytes) => bytes,
                    Err(error) => {
                        let mapped = not_impl_error(&error);
                        trace_set_data(
                            Some(format),
                            Some(medium_tymed),
                            frelease.as_bool(),
                            false,
                            mapped.code(),
                        );
                        return Err(mapped);
                    }
                };
                self.store_extra_format(format, bytes);
                stored = true;
            } else {
                // なぜ非 HGLOBAL も受理するか:
                // IDragSourceHelper は環境により別 tymed を渡すことがあり、
                // ここで弾くとドラッグ画像適用全体が失敗するため。
                // 非 HGLOBAL は現時点で再提供しないが、drag 自体は継続させる。
            }

            // なぜ ReleaseStgMedium するか:
            // fRelease=true は呼び出し元から所有権を移譲されたことを意味するため。
            if frelease.as_bool() {
                unsafe {
                    ReleaseStgMedium(pmedium as *mut STGMEDIUM);
                }
            }
            trace_set_data(
                Some(format),
                Some(medium_tymed),
                frelease.as_bool(),
                stored,
                S_OK,
            );
            Ok(())
        }

        fn EnumFormatEtc(&self, dwdirection: u32) -> windows::core::Result<IEnumFORMATETC> {
            if dwdirection != DATADIR_GET.0 as u32 {
                return Err(Error::from(E_NOTIMPL));
            }
            Ok(FormatEtcEnumerator::new(self.enumerated_formats(), 0).into())
        }

        fn DAdvise(
            &self,
            _pformatetc: *const FORMATETC,
            _advf: u32,
            _padvsink: Option<&IAdviseSink>,
        ) -> windows::core::Result<u32> {
            Err(Error::from(OLE_E_ADVISENOTSUPPORTED))
        }

        fn DUnadvise(&self, _dwconnection: u32) -> windows::core::Result<()> {
            Err(Error::from(OLE_E_ADVISENOTSUPPORTED))
        }

        fn EnumDAdvise(&self) -> windows::core::Result<IEnumSTATDATA> {
            Err(Error::from(OLE_E_ADVISENOTSUPPORTED))
        }
    }

    #[cfg(test)]
    mod local_drag_session_format_tests {
        use super::{FORMATETC, FileDataObject, format_matches};
        use crate::DragOptions;
        use std::path::PathBuf;
        use windows::Win32::System::Com::{DVASPECT_CONTENT, TYMED_HGLOBAL};
        use windows::Win32::System::DataExchange::RegisterClipboardFormatW;
        use windows::core::PCWSTR;

        fn local_session_format() -> u16 {
            let name: Vec<u16> = "GPUI.Filer.InternalDragSession\0".encode_utf16().collect();
            unsafe { RegisterClipboardFormatW(PCWSTR(name.as_ptr())) as u16 }
        }

        #[test]
        fn local_session_format_is_optional_and_serialized_as_little_endian_u64() {
            let without_token = FileDataObject::new(&[PathBuf::from(r"C:\source.txt")], None)
                .expect("construct default data object");
            let with_token = FileDataObject::new(
                &[PathBuf::from(r"C:\source.txt")],
                Some(0x0123_4567_89ab_cdef),
            )
            .expect("construct session data object");
            let session_format = local_session_format();
            assert_ne!(session_format, 0);
            let request = FORMATETC {
                cfFormat: session_format,
                ptd: std::ptr::null_mut(),
                dwAspect: DVASPECT_CONTENT.0,
                lindex: -1,
                tymed: TYMED_HGLOBAL.0 as u32,
            };

            assert!(
                !without_token
                    .enumerated_formats()
                    .iter()
                    .any(|format| format.cfFormat == session_format)
            );
            assert!(without_token.query_extra_format(&request).is_none());

            let formats = with_token.enumerated_formats();
            let advertised = formats
                .iter()
                .find(|format| format.cfFormat == session_format)
                .expect("session format is enumerated");
            assert!(format_matches(&request, advertised));
            assert_eq!(
                with_token.query_extra_format(&request),
                Some(0x0123_4567_89ab_cdef_u64.to_le_bytes().to_vec())
            );
            assert_eq!(
                with_token
                    .extra_formats
                    .borrow()
                    .iter()
                    .find(|stored| stored.format.cfFormat == session_format)
                    .expect("serialized session format")
                    .bytes,
                0x0123_4567_89ab_cdef_u64.to_le_bytes()
            );

            // DragOptions::default() keeps all external callers token-free.
            assert_eq!(DragOptions::default().local_drag_session_id, None);
        }
    }

    #[cfg(test)]
    mod drag_context_set_data_tests {
        use super::{
            BOOL, DV_E_FORMATETC, DVASPECT_CONTENT, FORMATETC, FileDataObject, IDataObject, S_OK,
            STGMEDIUM, STGMEDIUM_0, TYMED_HGLOBAL, TYMED_ISTREAM,
        };
        use std::mem::ManuallyDrop;
        use std::path::PathBuf;
        use windows::Win32::Foundation::HGLOBAL;
        use windows::Win32::System::Com::StructuredStorage::CreateStreamOnHGlobal;
        use windows::Win32::System::Com::{IStream, STREAM_SEEK};
        use windows::Win32::System::DataExchange::RegisterClipboardFormatW;
        use windows::Win32::System::Ole::ReleaseStgMedium;
        use windows::core::PCWSTR;

        fn drag_context_format() -> u16 {
            let name: Vec<u16> = "DragContext\0".encode_utf16().collect();
            unsafe { RegisterClipboardFormatW(PCWSTR(name.as_ptr())) as u16 }
        }

        fn stream_medium() -> (IStream, STGMEDIUM) {
            let stream = unsafe { CreateStreamOnHGlobal(HGLOBAL::default(), BOOL(1)) }
                .expect("create in-memory IStream");
            let mut data = *b"drag-context";
            let mut written = 0;
            unsafe {
                stream
                    .Write(
                        data.as_mut_ptr().cast(),
                        data.len() as u32,
                        Some(&mut written),
                    )
                    .ok()
                    .expect("write test stream");
                stream
                    .Seek(0, STREAM_SEEK(0), None)
                    .expect("rewind source stream");
            }
            assert_eq!(written as usize, data.len());
            let medium = STGMEDIUM {
                tymed: TYMED_ISTREAM.0 as u32,
                u: STGMEDIUM_0 {
                    pstm: ManuallyDrop::new(Some(stream.clone())),
                },
                pUnkForRelease: ManuallyDrop::new(None),
            };
            (stream, medium)
        }

        fn read_stream(medium: &STGMEDIUM) -> Vec<u8> {
            assert_eq!(medium.tymed, TYMED_ISTREAM.0 as u32);
            let stream = unsafe { medium.u.pstm.as_ref() }.expect("returned IStream");
            let mut bytes = vec![0; 12];
            let mut read = 0;
            unsafe {
                stream
                    .Read(
                        bytes.as_mut_ptr().cast(),
                        bytes.len() as u32,
                        Some(&mut read),
                    )
                    .ok()
                    .expect("read test stream");
            }
            bytes.truncate(read as usize);
            bytes
        }

        fn data_object() -> IDataObject {
            FileDataObject::new(&[PathBuf::from(r"C:\source.txt")], None)
                .expect("construct FileDataObject")
                .into()
        }

        fn format(tymed: u32) -> FORMATETC {
            FORMATETC {
                cfFormat: drag_context_format(),
                ptd: std::ptr::null_mut(),
                dwAspect: DVASPECT_CONTENT.0,
                lindex: -1,
                tymed,
            }
        }

        #[test]
        fn drag_context_is_reprovided_as_owned_media_for_both_release_modes() {
            for frelease in [false, true] {
                let data_object = data_object();
                let (_source, mut input) = stream_medium();
                let format = format(TYMED_ISTREAM.0 as u32);

                unsafe {
                    data_object
                        .SetData(&format, &input, BOOL(i32::from(frelease)))
                        .expect("store DragContext");
                }
                assert_eq!(unsafe { data_object.QueryGetData(&format) }, S_OK);

                let first = unsafe { data_object.GetData(&format) }.expect("first GetData");
                assert_eq!(read_stream(&first), b"drag-context");
                let second = unsafe { data_object.GetData(&format) }.expect("second GetData");
                assert_eq!(read_stream(&second), b"drag-context");

                unsafe {
                    ReleaseStgMedium(&mut (first as STGMEDIUM));
                    ReleaseStgMedium(&mut (second as STGMEDIUM));
                    if !frelease {
                        ReleaseStgMedium(&mut input);
                    }
                }
            }
        }

        #[test]
        fn drag_context_rejects_mismatched_medium_and_format_fields() {
            let data_object = data_object();
            let (_source, mut input) = stream_medium();
            let valid = format(TYMED_ISTREAM.0 as u32);
            unsafe {
                data_object
                    .SetData(&valid, &input, BOOL(0))
                    .expect("store DragContext")
            };

            for invalid in [
                format(TYMED_HGLOBAL.0 as u32),
                FORMATETC {
                    dwAspect: 2,
                    ..valid
                },
                FORMATETC { lindex: 0, ..valid },
            ] {
                assert_eq!(
                    unsafe { data_object.QueryGetData(&invalid) },
                    DV_E_FORMATETC
                );
                assert!(unsafe { data_object.GetData(&invalid) }.is_err());
            }

            let wrong_medium = STGMEDIUM {
                tymed: TYMED_HGLOBAL.0 as u32,
                u: STGMEDIUM_0 {
                    hGlobal: HGLOBAL::default(),
                },
                pUnkForRelease: ManuallyDrop::new(None),
            };
            assert!(unsafe { data_object.SetData(&valid, &wrong_medium, BOOL(0)) }.is_err());
            unsafe { ReleaseStgMedium(&mut input) };
        }
    }

    #[cfg(test)]
    mod performed_drop_effect_set_data_tests {
        use super::BOOL;
        use super::{DVASPECT_CONTENT, FORMATETC, FileDataObject, IDataObject, TYMED_HGLOBAL};
        use crate::{MOVE_EFFECT, PerformedEffectReport};
        use std::path::PathBuf;
        use windows::Win32::System::Com::{STGMEDIUM, STGMEDIUM_0};
        use windows::Win32::System::Ole::ReleaseStgMedium;

        fn report_after_set_data(
            data_object: &IDataObject,
            report_format: u16,
            aspect: u32,
            format_tymed: u32,
            effect: u32,
        ) -> windows::core::Result<()> {
            let format = FORMATETC {
                cfFormat: report_format,
                ptd: std::ptr::null_mut(),
                dwAspect: aspect,
                lindex: -1,
                tymed: format_tymed,
            };
            let hglobal =
                super::alloc_u32_hglobal(effect).expect("allocate performed-effect HGLOBAL");
            let mut medium = STGMEDIUM {
                tymed: TYMED_HGLOBAL.0 as u32,
                u: STGMEDIUM_0 { hGlobal: hglobal },
                pUnkForRelease: std::mem::ManuallyDrop::new(None),
            };

            // frelease=false keeps this test's STGMEDIUM owned by the caller.
            let result = unsafe { data_object.SetData(&format, &medium, BOOL(0)) };
            unsafe { ReleaseStgMedium(&mut medium) };
            result
        }

        fn make_data_object() -> (
            IDataObject,
            std::rc::Rc<std::cell::Cell<PerformedEffectReport>>,
            u16,
        ) {
            let implementation = FileDataObject::new(&[PathBuf::from(r"C:\source.txt")], None)
                .expect("construct FileDataObject");
            let report = implementation.performed_drop_effect.clone();
            let format = implementation.performed_drop_effect_format;
            (implementation.into(), report, format)
        }

        #[test]
        fn com_set_data_captures_valid_and_ambiguous_performed_effect_reports() {
            let (data_object, report, format) = make_data_object();
            report_after_set_data(
                &data_object,
                format,
                DVASPECT_CONTENT.0,
                TYMED_HGLOBAL.0 as u32,
                MOVE_EFFECT,
            )
            .expect("COM SetData accepts performed MOVE report");
            let execution = crate::drag_execution_with_shell_report(
                crate::DragResult::Dropped,
                Some(MOVE_EFFECT),
                report.get(),
                None,
            );
            assert_eq!(execution.performed_effect, Some(MOVE_EFFECT));
            assert_eq!(execution.shell_performed_effect, Some(MOVE_EFFECT));
            report_after_set_data(&data_object, format, 0, 0, MOVE_EFFECT)
                .expect("COM SetData accepts malformed report as evidence");
            let execution = crate::drag_execution_with_shell_report(
                crate::DragResult::Dropped,
                Some(MOVE_EFFECT),
                report.get(),
                None,
            );
            assert_eq!(execution.shell_performed_effect, None);

            let (data_object, report, format) = make_data_object();
            report_after_set_data(
                &data_object,
                format,
                DVASPECT_CONTENT.0,
                TYMED_HGLOBAL.0 as u32,
                MOVE_EFFECT,
            )
            .expect("COM SetData accepts first MOVE report");
            report_after_set_data(
                &data_object,
                format,
                DVASPECT_CONTENT.0,
                TYMED_HGLOBAL.0 as u32,
                MOVE_EFFECT,
            )
            .expect("COM SetData accepts repeated MOVE report");
            let execution = crate::drag_execution_with_shell_report(
                crate::DragResult::Dropped,
                Some(MOVE_EFFECT),
                report.get(),
                None,
            );
            assert_eq!(execution.shell_performed_effect, Some(MOVE_EFFECT));
            report_after_set_data(
                &data_object,
                format,
                DVASPECT_CONTENT.0,
                TYMED_HGLOBAL.0 as u32,
                crate::COPY_EFFECT,
            )
            .expect("COM SetData accepts conflicting COPY report");
            let execution = crate::drag_execution_with_shell_report(
                crate::DragResult::Dropped,
                Some(MOVE_EFFECT),
                report.get(),
                None,
            );
            assert_eq!(execution.shell_performed_effect, None);
        }
    }

    fn normalize_format(format: &FORMATETC) -> FORMATETC {
        FORMATETC {
            cfFormat: format.cfFormat,
            ptd: std::ptr::null_mut(),
            dwAspect: format.dwAspect,
            lindex: format.lindex,
            // なぜ HGLOBAL 固定にするか:
            // 現状、SetData で保持する実体は HGLOBAL のみだから。
            tymed: TYMED_HGLOBAL.0 as u32,
        }
    }

    fn format_matches(request: &FORMATETC, stored: &FORMATETC) -> bool {
        request.cfFormat == stored.cfFormat
            && request.dwAspect == stored.dwAspect
            && request.lindex == stored.lindex
            && (request.tymed & stored.tymed) != 0
    }

    fn clone_hglobal_bytes(
        hglobal: windows::Win32::Foundation::HGLOBAL,
    ) -> Result<Vec<u8>, String> {
        if hglobal.is_invalid() {
            return Err("SetData called with invalid HGLOBAL".to_string());
        }

        let size = unsafe { GlobalSize(hglobal) };
        if size == 0 {
            return Ok(Vec::new());
        }

        let ptr = unsafe { GlobalLock(hglobal) as *const u8 };
        if ptr.is_null() {
            return Err("GlobalLock returned null".to_string());
        }

        let bytes = unsafe { std::slice::from_raw_parts(ptr, size) }.to_vec();
        let _ = unsafe { GlobalUnlock(hglobal) };
        Ok(bytes)
    }

    #[implement(IEnumFORMATETC)]
    struct FormatEtcEnumerator {
        formats: Vec<FORMATETC>,
        index: Cell<usize>,
    }

    impl FormatEtcEnumerator {
        fn new(formats: Vec<FORMATETC>, index: usize) -> Self {
            Self {
                formats,
                index: Cell::new(index),
            }
        }
    }

    impl IEnumFORMATETC_Impl for FormatEtcEnumerator_Impl {
        fn Next(
            &self,
            celt: u32,
            rgelt: *mut FORMATETC,
            pceltfetched: *mut u32,
        ) -> windows::core::HRESULT {
            if celt == 0 {
                return S_OK;
            }
            if rgelt.is_null() {
                return DV_E_FORMATETC;
            }

            let start = self.index.get();
            let mut fetched = 0usize;
            let max = celt as usize;
            while start + fetched < self.formats.len() && fetched < max {
                unsafe {
                    *rgelt.add(fetched) = self.formats[start + fetched];
                }
                fetched += 1;
            }
            self.index.set(start + fetched);

            if !pceltfetched.is_null() {
                unsafe {
                    *pceltfetched = fetched as u32;
                }
            }

            if fetched == max { S_OK } else { S_FALSE }
        }

        fn Skip(&self, celt: u32) -> windows::core::Result<()> {
            let start = self.index.get();
            let end = start.saturating_add(celt as usize).min(self.formats.len());
            self.index.set(end);
            if end - start == celt as usize {
                Ok(())
            } else {
                Err(Error::from(S_FALSE))
            }
        }

        fn Reset(&self) -> windows::core::Result<()> {
            self.index.set(0);
            Ok(())
        }

        fn Clone(&self) -> windows::core::Result<IEnumFORMATETC> {
            Ok(FormatEtcEnumerator::new(self.formats.clone(), self.index.get()).into())
        }
    }

    #[implement(IDropSource)]
    struct FileDropSource {
        saw_left_down: Cell<bool>,
        started_at: Instant,
        ghost_window: Option<GhostWindowFollower>,
        touch_drag_cancelled: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
    }

    impl FileDropSource {
        fn new(
            ghost_window: Option<GhostWindowFollower>,
            touch_drag_cancelled: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
        ) -> Self {
            Self {
                saw_left_down: Cell::new(false),
                started_at: Instant::now(),
                ghost_window,
                touch_drag_cancelled,
            }
        }
    }

    impl IDropSource_Impl for FileDropSource_Impl {
        fn QueryContinueDrag(
            &self,
            fescapepressed: BOOL,
            grfkeystate: windows::Win32::System::SystemServices::MODIFIERKEYS_FLAGS,
        ) -> windows::core::HRESULT {
            if self
                .touch_drag_cancelled
                .as_ref()
                .is_some_and(|cancelled| cancelled.load(std::sync::atomic::Ordering::Acquire))
            {
                return DRAGDROP_S_CANCEL;
            }
            if fescapepressed.as_bool() {
                return DRAGDROP_S_CANCEL;
            }
            // なぜ GetAsyncKeyState を併用するか:
            // ワーカースレッド起点だと grfkeystate が 0 のまま来るケースがあり、
            // MK_LBUTTON 判定だけでは drag が開始/終了できずに詰まるため。
            let left_down = (grfkeystate.0 & MK_LBUTTON.0) != 0
                || unsafe { GetAsyncKeyState(VK_LBUTTON.0 as i32) < 0 };
            let mut saw_left_down = self.saw_left_down.get();
            let decision = super::drop_source_decision(
                &mut saw_left_down,
                left_down,
                self.started_at.elapsed(),
            );
            self.saw_left_down.set(saw_left_down);
            match decision {
                super::DropSourceDecision::Continue => S_OK,
                super::DropSourceDecision::Drop => DRAGDROP_S_DROP,
                super::DropSourceDecision::Cancel => DRAGDROP_S_CANCEL,
            }
        }

        fn GiveFeedback(&self, _dweffect: DROPEFFECT) -> windows::core::HRESULT {
            if let Some(ghost_window) = self.ghost_window {
                ghost_window.move_to_cursor(false);
            }
            DRAGDROP_S_USEDEFAULTCURSORS
        }
    }

    fn build_wide_paths(paths: &[PathBuf]) -> Vec<u16> {
        use std::os::windows::ffi::OsStrExt;

        let mut wide_paths = Vec::new();
        for path in paths {
            wide_paths.extend(path.as_os_str().encode_wide());
            wide_paths.push(0);
        }
        wide_paths.push(0);
        wide_paths
    }

    fn alloc_hdrop_hglobal(
        wide_paths: &[u16],
    ) -> Result<windows::Win32::Foundation::HGLOBAL, String> {
        let dropfiles_size = std::mem::size_of::<DROPFILES>();
        let bytes_len = std::mem::size_of_val(wide_paths);
        let total_size = dropfiles_size + bytes_len;

        unsafe {
            let hglobal = GlobalAlloc(GMEM_MOVEABLE, total_size)
                .map_err(|e| format!("GlobalAlloc failed: {e}"))?;
            let ptr = GlobalLock(hglobal) as *mut u8;
            if ptr.is_null() {
                return Err("GlobalLock returned null".to_string());
            }

            // なぜ DROPFILES を先頭に置くか:
            // CF_HDROP の既定バイナリレイアウトを Explorer が期待しているため。
            let dropfiles = ptr as *mut DROPFILES;
            (*dropfiles).pFiles = dropfiles_size as u32;
            (*dropfiles).pt.x = 0;
            (*dropfiles).pt.y = 0;
            (*dropfiles).fNC = false.into();
            (*dropfiles).fWide = true.into();

            let dst = ptr.add(dropfiles_size) as *mut u16;
            std::ptr::copy_nonoverlapping(wide_paths.as_ptr(), dst, wide_paths.len());
            let _ = GlobalUnlock(hglobal);
            Ok(hglobal)
        }
    }

    fn alloc_u32_hglobal(value: u32) -> Result<windows::Win32::Foundation::HGLOBAL, String> {
        unsafe {
            let hglobal = GlobalAlloc(GMEM_MOVEABLE, std::mem::size_of::<u32>())
                .map_err(|e| format!("GlobalAlloc failed: {e}"))?;
            let ptr = GlobalLock(hglobal) as *mut u32;
            if ptr.is_null() {
                return Err("GlobalLock returned null".to_string());
            }
            *ptr = value;
            let _ = GlobalUnlock(hglobal);
            Ok(hglobal)
        }
    }

    fn alloc_bytes_hglobal(bytes: &[u8]) -> Result<windows::Win32::Foundation::HGLOBAL, String> {
        unsafe {
            let alloc_size = bytes.len().max(1);
            let hglobal = GlobalAlloc(GMEM_MOVEABLE, alloc_size)
                .map_err(|e| format!("GlobalAlloc failed: {e}"))?;
            let ptr = GlobalLock(hglobal) as *mut u8;
            if ptr.is_null() {
                return Err("GlobalLock returned null".to_string());
            }
            if !bytes.is_empty() {
                std::ptr::copy_nonoverlapping(bytes.as_ptr(), ptr, bytes.len());
            } else {
                *ptr = 0;
            }
            let _ = GlobalUnlock(hglobal);
            Ok(hglobal)
        }
    }
}

#[cfg(not(target_os = "windows"))]
mod platform {
    use std::path::PathBuf;

    use crate::{DragError, DragExecution, DragVisual, FileDragSource, PinnedFileDragSources};

    pub(super) fn pin_file_drag_sources(
        _sources: &[FileDragSource],
        _policy: crate::FileDragPinPolicy,
    ) -> Result<PinnedFileDragSources, DragError> {
        Err(DragError::UnsupportedPlatform)
    }

    pub(super) fn begin_file_drag_impl(
        _paths: &[PathBuf],
        _options: &crate::DragOptions,
        _allowed_effects: u32,
    ) -> Result<DragExecution, DragError> {
        Err(DragError::UnsupportedPlatform)
    }

    pub(super) fn release_source_capture(_source_hwnd: isize) -> Result<(), DragError> {
        Err(DragError::UnsupportedPlatform)
    }
}

#[cfg(test)]
mod tests {
    #[cfg(target_os = "windows")]
    use std::fs;
    use std::path::PathBuf;
    use std::time::Duration;

    #[cfg(target_os = "windows")]
    use super::platform::compute_text_rects;
    use super::{
        COPY_EFFECT, DROP_SOURCE_START_TIMEOUT_MS, DragError, DragExecution, DragResult,
        DragVisual, DragVisualSource, DropSourceDecision, MOVE_EFFECT, PerformedEffectReport,
        PinnedFileDragSources, drop_source_decision, finish_pinned_file_drag, normalize_paths,
        parse_shell_performed_effect, scaled_logical_px, text_rects_for_scale, title_ink_rgb,
    };
    #[cfg(target_os = "windows")]
    use super::{
        FileDragPinPolicy, FileDragSource, pin_file_drag_sources, pin_file_drag_sources_with_policy,
    };
    #[test]
    fn performed_effect_requires_exact_registered_format_and_dword_hglobal() {
        let move_value = MOVE_EFFECT.to_le_bytes();
        assert_eq!(
            parse_shell_performed_effect(0xC123, 0xC123, true, true, &move_value),
            Some(MOVE_EFFECT)
        );
        assert_eq!(
            parse_shell_performed_effect(
                0xC123,
                0xC123,
                true,
                true,
                &[move_value.as_slice(), &[0xAA, 0xBB]].concat(),
            ),
            Some(MOVE_EFFECT)
        );
        assert_eq!(
            parse_shell_performed_effect(0xC124, 0xC123, true, true, &move_value),
            None
        );
        assert_eq!(
            parse_shell_performed_effect(0xC123, 0xC123, true, false, &move_value),
            None
        );
        assert_eq!(
            parse_shell_performed_effect(0xC123, 0xC123, false, true, &move_value),
            None
        );
    }

    #[test]
    fn malformed_or_conflicting_performed_effect_reports_are_ambiguous() {
        assert_eq!(
            parse_shell_performed_effect(7, 7, true, true, &[2, 0, 0]),
            None
        );

        let mut report = PerformedEffectReport::Unreported;
        report.record(Some(MOVE_EFFECT));
        assert_eq!(report.value(), Some(MOVE_EFFECT));
        report.record(Some(MOVE_EFFECT));
        assert_eq!(report.value(), Some(MOVE_EFFECT));
        report.record(Some(COPY_EFFECT));
        assert_eq!(report, PerformedEffectReport::Ambiguous);
        assert_eq!(report.value(), None);

        let mut malformed_after_move = PerformedEffectReport::Unreported;
        malformed_after_move.record(Some(MOVE_EFFECT));
        malformed_after_move.record(None);
        assert_eq!(malformed_after_move.value(), None);
    }

    #[cfg(target_os = "windows")]
    const ERROR_SHARING_VIOLATION_HRESULT: i32 = 0x8007_0020_u32 as i32;
    #[cfg(target_os = "windows")]
    const ERROR_SHARING_VIOLATION_OS_CODE: i32 = 32;

    #[cfg(target_os = "windows")]
    fn test_dir() -> PathBuf {
        use std::time::{SystemTime, UNIX_EPOCH};

        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock after epoch")
            .as_nanos();
        let path =
            std::env::temp_dir().join(format!("win-shell-dnd-{}-{nonce}", std::process::id()));
        fs::create_dir(&path).expect("create unique test directory");
        path
    }

    #[cfg(target_os = "windows")]
    fn source_for_path(path: PathBuf, is_directory: bool) -> FileDragSource {
        let (volume_serial, file_index) = super::platform::file_drag_identity_for_tests(&path);
        FileDragSource {
            captured_size: if is_directory {
                None
            } else {
                fs::metadata(&path).ok().map(|m| m.len())
            },
            captured_modified: if is_directory {
                None
            } else {
                fs::metadata(&path).ok().and_then(|m| m.modified().ok())
            },
            path,
            volume_serial,
            file_index,
            is_directory,
        }
    }

    #[cfg(target_os = "windows")]
    fn source_for_file(path: PathBuf) -> FileDragSource {
        source_for_path(path, false)
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn pinned_sources_hold_the_validated_file_against_rename_or_delete() {
        let dir = test_dir();
        let path = dir.join("source.txt");
        fs::write(&path, b"source").expect("write test file");
        let pinned = pin_file_drag_sources(&[source_for_file(path.clone())])
            .expect("pin the current filesystem object");
        assert_eq!(pinned.len(), 1);
        assert_eq!(pinned.allowed_effects(), COPY_EFFECT);
        assert!(
            pinned._parent_handles.len() + pinned.leaf_handles.len() > 1,
            "parent chain and leaf are pinned"
        );
        assert_eq!(
            fs::read(&path).expect("read while source is pinned"),
            b"source"
        );
        let renamed_path = dir.join("renamed.txt");
        let no_share_open = super::platform::can_open_source_read_without_sharing_for_tests(&path);
        let delete_open = super::platform::can_open_source_for_delete_for_tests(&path);
        let rename_error = fs::rename(&path, &renamed_path).err();
        let delete_error = fs::remove_file(&path).err();
        let original_path_unchanged = path.exists() && !renamed_path.exists();
        drop(pinned);
        let release_rename_succeeded = fs::rename(&path, &renamed_path).is_ok();
        let release_delete_succeeded = fs::remove_file(&renamed_path).is_ok();
        for cleanup_path in [&path, &renamed_path] {
            if cleanup_path.exists() {
                fs::remove_file(cleanup_path).expect("clean up test source");
            }
        }
        fs::remove_dir(&dir).expect("remove test directory");
        assert!(
            no_share_open.err() == Some(ERROR_SHARING_VIOLATION_HRESULT),
            "non-shared read open must fail with ERROR_SHARING_VIOLATION"
        );
        assert_eq!(
            delete_open.err(),
            Some(ERROR_SHARING_VIOLATION_HRESULT),
            "delete-access open must fail with ERROR_SHARING_VIOLATION"
        );
        assert_eq!(
            rename_error.and_then(|error| error.raw_os_error()),
            Some(ERROR_SHARING_VIOLATION_OS_CODE),
            "rename must fail with ERROR_SHARING_VIOLATION"
        );
        assert_eq!(
            delete_error.and_then(|error| error.raw_os_error()),
            Some(ERROR_SHARING_VIOLATION_OS_CODE),
            "delete must fail with ERROR_SHARING_VIOLATION"
        );
        assert!(original_path_unchanged);
        assert!(release_rename_succeeded);
        assert!(release_delete_succeeded);
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn move_capable_pin_shares_delete_only_for_leaf_and_holds_parent_route() {
        let dir = test_dir();
        let path = dir.join("source.txt");
        let renamed_path = dir.join("renamed-source.txt");
        let replacement_path = path.clone();
        let renamed_dir = dir.with_file_name(format!(
            "{}-renamed",
            dir.file_name()
                .expect("test directory name")
                .to_string_lossy()
        ));
        fs::write(&path, b"source").expect("write test file");
        let original_identity = super::platform::file_drag_identity_for_tests(&path);
        let pinned = pin_file_drag_sources_with_policy(
            &[source_for_file(path.clone())],
            FileDragPinPolicy::MoveCapable,
        )
        .expect("pin a regular file with Move capability");

        let allowed_effects = pinned.allowed_effects();
        let leaf_delete_open = super::platform::can_open_source_for_delete_for_tests(&path);
        let leaf_write_open = super::platform::can_open_source_for_write_for_tests(&path);
        let leaf_read = fs::read(&path);
        let leaf_rename = fs::rename(&path, &renamed_path);
        let replacement_create = leaf_rename
            .as_ref()
            .ok()
            .map(|()| fs::write(&replacement_path, b"replacement"));
        let renamed_identity = if renamed_path.exists() {
            Some(super::platform::file_drag_identity_for_tests(&renamed_path))
        } else {
            None
        };
        let replacement_identity = if replacement_path.exists() {
            Some(super::platform::file_drag_identity_for_tests(
                &replacement_path,
            ))
        } else {
            None
        };
        let parent_rename_error = fs::rename(&dir, &renamed_dir).err();
        let parent_delete_open = super::platform::can_open_source_for_delete_for_tests(&dir);
        let original_parent_path_unchanged = dir.exists() && !renamed_dir.exists();
        drop(pinned);
        let release_parent_rename_succeeded = fs::rename(&dir, &renamed_dir).is_ok();
        let cleanup_dir = if renamed_dir.exists() {
            &renamed_dir
        } else {
            &dir
        };
        let _ = fs::remove_file(cleanup_dir.join("source.txt"));
        let _ = fs::remove_file(cleanup_dir.join("renamed-source.txt"));
        let cleanup_result = fs::remove_dir(cleanup_dir);

        assert_eq!(allowed_effects, COPY_EFFECT | MOVE_EFFECT);
        assert_eq!(
            leaf_delete_open,
            Ok(()),
            "Move-capable leaf pin permits DELETE access"
        );
        assert!(matches!(leaf_read, Ok(ref bytes) if bytes == b"source"));
        assert_eq!(
            leaf_write_open.err(),
            Some(ERROR_SHARING_VIOLATION_HRESULT),
            "Move-capable leaf pin rejects in-place write access"
        );
        assert!(leaf_rename.is_ok(), "pinned CF_HDROP leaf can be renamed");
        assert!(
            matches!(replacement_create, Some(Ok(()))),
            "original leaf path can be replaced"
        );
        assert_eq!(renamed_identity, Some(original_identity));
        assert!(
            replacement_identity.is_some_and(|identity| identity != original_identity),
            "the original path now identifies a different object; CF_HDROP resolves paths"
        );
        assert_eq!(
            parent_rename_error.and_then(|error| error.raw_os_error()),
            Some(ERROR_SHARING_VIOLATION_OS_CODE),
            "Move-capable parent pin must deny renaming the source route"
        );
        assert_eq!(
            parent_delete_open.err(),
            Some(ERROR_SHARING_VIOLATION_HRESULT),
            "Move-capable parent pin must deny DELETE access"
        );
        assert!(original_parent_path_unchanged);
        assert!(release_parent_rename_succeeded);
        assert!(cleanup_result.is_ok(), "remove test fixture directory");
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn move_capable_leaf_pin_records_a_hardlink_added_during_the_lease() {
        let dir = test_dir();
        let path = dir.join("source.txt");
        let moved_path = dir.join("moved-source.txt");
        fs::write(&path, b"source").expect("write test source");
        let pinned = pin_file_drag_sources_with_policy(
            &[source_for_file(path.clone())],
            FileDragPinPolicy::MoveCapable,
        )
        .expect("pin regular-file source for Move");
        let captured_signature = pinned.leaf_signatures[0];

        fs::rename(&path, &moved_path).expect("Move-capable source permits leaf rename");
        let hardlink_result = fs::hard_link(&moved_path, &path);
        let current_signature = hardlink_result
            .as_ref()
            .ok()
            .map(|()| super::platform::file_drag_leaf_signature_for_tests(&path));
        drop(pinned);

        let _ = fs::remove_file(&path);
        let _ = fs::remove_file(&moved_path);
        let cleanup_result = fs::remove_dir(&dir);
        assert!(
            hardlink_result.is_ok(),
            "CF_HDROP lease permits the link race"
        );
        let current_signature = current_signature.expect("read signature after creating link");
        assert_eq!(
            captured_signature.number_of_links + 1,
            current_signature.number_of_links
        );
        assert_ne!(
            captured_signature.change_time,
            current_signature.change_time
        );
        assert!(cleanup_result.is_ok(), "remove test fixture directory");
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn move_capable_pin_fails_when_a_writer_is_already_open() {
        let dir = test_dir();
        let path = dir.join("source.txt");
        fs::write(&path, b"source").expect("write test file");
        let writer = super::platform::open_existing_writer_for_tests(&path);

        let result = pin_file_drag_sources_with_policy(
            &[source_for_file(path.clone())],
            FileDragPinPolicy::MoveCapable,
        );
        drop(writer);

        assert!(
            result.is_err(),
            "a live writer prevents Move pin acquisition"
        );
        fs::remove_file(path).expect("remove test source");
        fs::remove_dir(dir).expect("remove test directory");
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn move_capable_pin_rejects_in_place_content_change_since_capture() {
        let dir = test_dir();
        let path = dir.join("source.txt");
        fs::write(&path, b"captured content").expect("write test source");
        let captured = source_for_file(path.clone());
        let identity = (captured.volume_serial, captured.file_index);
        fs::write(&path, b"changed content with another size")
            .expect("rewrite source without replacing the file");
        assert_eq!(
            super::platform::file_drag_identity_for_tests(&path),
            identity,
            "the content rewrite keeps the same filesystem identity"
        );

        let result = pin_file_drag_sources_with_policy(&[captured], FileDragPinPolicy::MoveCapable);
        assert!(matches!(result, Err(DragError::InvalidInput(_))));
        fs::remove_file(path).expect("remove test source");
        fs::remove_dir(dir).expect("remove test directory");
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn move_capable_pin_rejects_timestamp_only_change_since_capture() {
        let dir = test_dir();
        let path = dir.join("source.txt");
        fs::write(&path, b"captured").expect("write test source");
        let captured = source_for_file(path.clone());
        let identity = (captured.volume_serial, captured.file_index);
        fs::write(&path, b"modified").expect("rewrite source with the same byte length");
        let modified = captured
            .captured_modified
            .expect("capture source modification time")
            + std::time::Duration::from_secs(10);
        let writer = std::fs::OpenOptions::new()
            .write(true)
            .open(&path)
            .expect("open source to set its modification time");
        writer
            .set_times(std::fs::FileTimes::new().set_modified(modified))
            .expect("change only the source modification time");
        drop(writer);
        let current_metadata = fs::metadata(&path).expect("read rewritten source metadata");
        assert_eq!(current_metadata.len(), captured.captured_size.unwrap());
        assert_ne!(
            current_metadata.modified().unwrap(),
            captured.captured_modified.unwrap()
        );
        assert_eq!(
            super::platform::file_drag_identity_for_tests(&path),
            identity,
            "the content rewrite keeps the same filesystem identity"
        );

        let result = pin_file_drag_sources_with_policy(&[captured], FileDragPinPolicy::MoveCapable);
        assert!(matches!(result, Err(DragError::InvalidInput(_))));
        fs::remove_file(path).expect("remove test source");
        fs::remove_dir(dir).expect("remove test directory");
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn move_capable_pin_requires_both_captured_signature_facts() {
        let dir = test_dir();
        let path = dir.join("source.txt");
        fs::write(&path, b"source").expect("write test source");
        let mut source = source_for_file(path.clone());
        source.captured_size = None;
        let missing_size =
            pin_file_drag_sources_with_policy(&[source], FileDragPinPolicy::MoveCapable);
        assert!(matches!(missing_size, Err(DragError::InvalidInput(_))));

        let mut source = source_for_file(path.clone());
        source.captured_modified = None;
        let missing_modified =
            pin_file_drag_sources_with_policy(&[source], FileDragPinPolicy::MoveCapable);
        assert!(matches!(missing_modified, Err(DragError::InvalidInput(_))));

        let mut copy_only_source = source_for_file(path.clone());
        copy_only_source.captured_size = None;
        copy_only_source.captured_modified = None;
        assert!(pin_file_drag_sources(&[copy_only_source]).is_ok());
        fs::remove_file(path).expect("remove test source");
        fs::remove_dir(dir).expect("remove test directory");
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn move_capable_pin_rejects_captured_directories() {
        let dir = test_dir();
        let path = dir.join("source-dir");
        fs::create_dir(&path).expect("create source directory");
        let result = pin_file_drag_sources_with_policy(
            &[source_for_path(path.clone(), true)],
            FileDragPinPolicy::MoveCapable,
        );
        assert!(matches!(result, Err(DragError::InvalidInput(_))));
        fs::remove_dir(&path).expect("remove test source directory");
        fs::remove_dir(&dir).expect("remove test directory");
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn pinned_sources_hold_the_validated_directory_against_rename_or_delete() {
        let dir = test_dir();
        let source_dir = dir.join("source-dir");
        let renamed_dir = dir.join("renamed-dir");
        fs::create_dir(&source_dir).expect("create source directory");
        let pinned = pin_file_drag_sources(&[source_for_path(source_dir.clone(), true)])
            .expect("pin the current directory object");

        assert_eq!(
            fs::read_dir(&source_dir)
                .expect("list while source directory is pinned")
                .count(),
            0
        );
        let no_share_open =
            super::platform::can_open_source_read_without_sharing_for_tests(&source_dir);
        let delete_open = super::platform::can_open_source_for_delete_for_tests(&source_dir);
        let rename_error = fs::rename(&source_dir, &renamed_dir).err();
        let delete_error = fs::remove_dir(&source_dir).err();
        let original_path_unchanged = source_dir.exists() && !renamed_dir.exists();

        drop(pinned);
        let release_rename_succeeded = fs::rename(&source_dir, &renamed_dir).is_ok();
        let release_delete_succeeded = fs::remove_dir(&renamed_dir).is_ok();
        fs::remove_dir(&dir).expect("remove test directory");

        assert_eq!(
            no_share_open.err(),
            Some(ERROR_SHARING_VIOLATION_HRESULT),
            "non-shared directory-list open must fail with ERROR_SHARING_VIOLATION"
        );
        assert_eq!(
            delete_open.err(),
            Some(ERROR_SHARING_VIOLATION_HRESULT),
            "directory delete-access open must fail with ERROR_SHARING_VIOLATION"
        );
        assert_eq!(
            rename_error.and_then(|error| error.raw_os_error()),
            Some(ERROR_SHARING_VIOLATION_OS_CODE),
            "directory rename must fail with ERROR_SHARING_VIOLATION"
        );
        assert_eq!(
            delete_error.and_then(|error| error.raw_os_error()),
            Some(ERROR_SHARING_VIOLATION_OS_CODE),
            "directory delete must fail with ERROR_SHARING_VIOLATION"
        );
        assert!(original_path_unchanged);
        assert!(release_rename_succeeded);
        assert!(release_delete_succeeded);
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn pinned_sources_hold_parent_ancestors_until_the_lease_drops() {
        let dir = test_dir();
        let ancestor = dir.join("ancestor");
        let parent = ancestor.join("parent");
        fs::create_dir(&ancestor).expect("create source ancestor");
        fs::create_dir(&parent).expect("create source parent");
        let path = parent.join("source.txt");
        fs::write(&path, b"source").expect("write test source");

        let pinned =
            pin_file_drag_sources(&[source_for_file(path)]).expect("pin source parent chain");
        let renamed = dir.join("renamed-ancestor");
        let rename_error = fs::rename(&ancestor, &renamed).err();
        let original_path_unchanged = ancestor.exists() && !renamed.exists();

        drop(pinned);
        let release_rename_succeeded = fs::rename(&ancestor, &renamed).is_ok();
        fs::remove_file(renamed.join("parent").join("source.txt"))
            .expect("remove source after release");
        fs::remove_dir(renamed.join("parent")).expect("remove source parent after release");
        fs::remove_dir(renamed).expect("remove ancestor after release");
        fs::remove_dir(dir).expect("remove test directory");

        assert_eq!(
            rename_error.and_then(|error| error.raw_os_error()),
            Some(ERROR_SHARING_VIOLATION_OS_CODE),
            "renaming a source ancestor must fail with ERROR_SHARING_VIOLATION"
        );
        assert!(original_path_unchanged);
        assert!(release_rename_succeeded);
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn pinned_sources_share_exact_parent_chain_handles() {
        let dir = test_dir();
        let first = dir.join("first.txt");
        let second = dir.join("second.txt");
        fs::write(&first, b"first").expect("write first source");
        fs::write(&second, b"second").expect("write second source");
        let ancestor_count = first.parent().unwrap().ancestors().count();

        let pinned = pin_file_drag_sources(&[
            source_for_file(first.clone()),
            source_for_file(second.clone()),
        ])
        .expect("pin both sources and their shared parents");
        assert_eq!(pinned._parent_handles.len(), ancestor_count);
        assert_eq!(pinned.leaf_handles.len(), 2);
        drop(pinned);

        fs::remove_file(first).expect("remove first source");
        fs::remove_file(second).expect("remove second source");
        fs::remove_dir(dir).expect("remove test directory");
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn pinned_sources_reject_a_different_object_at_the_captured_path() {
        let dir = test_dir();
        let captured_path = dir.join("captured.txt");
        let current_path = dir.join("current.txt");
        fs::write(&captured_path, b"captured object").expect("write captured file");
        fs::write(&current_path, b"replacement object").expect("write replacement file");
        let mut stale_source = source_for_file(captured_path);
        stale_source.path = current_path.clone();

        assert!(pin_file_drag_sources(&[stale_source]).is_err());
        fs::remove_file(current_path).expect("remove replacement file");
        fs::remove_file(dir.join("captured.txt")).expect("remove captured file");
        fs::remove_dir(&dir).expect("remove test directory");
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn pinned_sources_reject_relative_paths() {
        use std::time::{SystemTime, UNIX_EPOCH};

        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock after epoch")
            .as_nanos();
        let relative_path = PathBuf::from(format!(
            "win-shell-dnd-relative-{}-{nonce}.txt",
            std::process::id()
        ));
        let absolute_path = std::env::current_dir()
            .expect("read current directory")
            .join(&relative_path);
        fs::write(&absolute_path, b"source").expect("write test source");
        let mut source = source_for_file(absolute_path);
        source.path = relative_path.clone();

        assert!(pin_file_drag_sources(&[source]).is_err());
        fs::remove_file(relative_path).expect("remove test source");
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn pinned_sources_reject_a_reparse_point() {
        use std::os::windows::fs::symlink_file;

        let dir = test_dir();
        let target = dir.join("target.txt");
        let link = dir.join("link.txt");
        fs::write(&target, b"target").expect("write target file");
        let source = source_for_file(target.clone());
        if symlink_file(&target, &link).is_err() {
            fs::remove_file(target).expect("remove target file");
            fs::remove_dir(&dir).expect("remove test directory");
            return;
        }
        let mut reparse_source = source;
        reparse_source.path = link.clone();
        assert!(pin_file_drag_sources(&[reparse_source]).is_err());
        fs::remove_file(link).expect("remove symlink");
        fs::remove_file(target).expect("remove target file");
        fs::remove_dir(&dir).expect("remove test directory");
    }

    #[test]
    fn pinned_drag_sources_can_cross_from_background_validation_to_foreground() {
        fn assert_send<T: Send>() {}
        assert_send::<PinnedFileDragSources>();
        assert_send::<super::MoveFileIdentityLease>();
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn move_lease_keeps_original_leaf_and_releases_parent_route_after_ole() {
        let dir = test_dir();
        let first = dir.join("first.txt");
        let second = dir.join("second.txt");
        let moved_first = dir.join("moved-first.txt");
        let original_dir = dir.clone();
        let renamed_dir = dir.with_file_name(format!(
            "{}-renamed",
            dir.file_name()
                .expect("test directory name")
                .to_string_lossy()
        ));
        fs::write(&first, b"original first").expect("write first source");
        fs::write(&second, b"second").expect("write second source");
        let first_identity = super::platform::file_drag_identity_for_tests(&first);
        let sources = pin_file_drag_sources_with_policy(
            &[
                source_for_file(first.clone()),
                source_for_file(second.clone()),
            ],
            FileDragPinPolicy::MoveCapable,
        )
        .expect("pin ordered Move-capable files");
        let result = finish_pinned_file_drag(
            sources,
            DragExecution {
                result: DragResult::Dropped,
                performed_effect: Some(MOVE_EFFECT),
                shell_performed_effect: Some(MOVE_EFFECT),
                visual_warning: None,
            },
        );
        let (_, lease) = result.into_authorized_parts();
        let lease = lease.expect("both OLE channels authorize the dropped MOVE");
        assert_eq!(lease.len(), 2);
        assert_eq!(
            lease.source_paths().cloned().collect::<Vec<_>>(),
            [first.clone(), second.clone()]
        );
        let routes = lease.parent_routes().collect::<Vec<_>>();
        assert_eq!(routes.len(), 2);
        assert_eq!(
            routes[0].last().map(|parent| parent.path.as_path()),
            first.parent()
        );
        assert_eq!(
            routes[1].last().map(|parent| parent.path.as_path()),
            second.parent()
        );
        assert!(
            routes
                .iter()
                .flat_map(|route| route.iter())
                .all(|parent| parent.file_index != 0)
        );

        fs::rename(&first, &moved_first)
            .expect("move original leaf while its lease permits deletion");
        fs::remove_file(&moved_first).expect("delete moved original while its lease is held");
        fs::write(&first, b"replacement").expect("recreate the old source path");
        assert_eq!(
            super::platform::file_drag_identity_from_handle_for_tests(&lease.sources[0]._file),
            first_identity,
            "leaf lease retains the deleted original object identity"
        );
        let replacement_identity = super::platform::file_drag_identity_for_tests(&first);
        assert_ne!(replacement_identity, first_identity);
        assert_eq!(
            super::platform::can_open_source_for_delete_for_tests(&original_dir),
            Ok(()),
            "parent route handles were released when the lease was extracted"
        );
        drop(lease);
        fs::remove_file(first).expect("remove replacement");
        fs::remove_file(second).expect("remove second source");
        fs::remove_dir(original_dir).expect("remove test directory");
        let _ = renamed_dir;
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn copy_only_pin_cannot_produce_a_move_identity_lease() {
        let dir = test_dir();
        let path = dir.join("source.txt");
        fs::write(&path, b"copy only").expect("write test source");
        let pinned =
            pin_file_drag_sources(&[source_for_file(path.clone())]).expect("pin CopyOnly source");
        let result = finish_pinned_file_drag(
            pinned,
            DragExecution {
                result: DragResult::Dropped,
                performed_effect: Some(COPY_EFFECT),
                shell_performed_effect: Some(COPY_EFFECT),
                visual_warning: None,
            },
        );
        assert!(result.move_lease.is_none());
        fs::remove_file(path).expect("remove test source");
        fs::remove_dir(dir).expect("remove test directory");
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn move_identity_lease_requires_drop_and_exact_agreement_from_both_effects() {
        let dir = test_dir();
        for (index, execution) in [
            DragExecution {
                result: DragResult::Dropped,
                performed_effect: Some(COPY_EFFECT),
                shell_performed_effect: Some(MOVE_EFFECT),
                visual_warning: None,
            },
            DragExecution {
                result: DragResult::Dropped,
                performed_effect: Some(MOVE_EFFECT),
                shell_performed_effect: Some(COPY_EFFECT),
                visual_warning: None,
            },
            DragExecution {
                result: DragResult::Cancelled,
                performed_effect: Some(MOVE_EFFECT),
                shell_performed_effect: Some(MOVE_EFFECT),
                visual_warning: None,
            },
        ]
        .into_iter()
        .enumerate()
        {
            let path = dir.join(format!("source-{index}.txt"));
            fs::write(&path, b"source").expect("write test source");
            let pinned = pin_file_drag_sources_with_policy(
                &[source_for_file(path.clone())],
                FileDragPinPolicy::MoveCapable,
            )
            .expect("pin Move-capable source");
            let result = finish_pinned_file_drag(pinned, execution);
            let (_, lease) = result.into_authorized_parts();
            assert!(
                lease.is_none(),
                "partial or cancelled MOVE evidence must not authorize cleanup"
            );
            fs::remove_file(path).expect("remove test source");
        }
        fs::remove_dir(dir).expect("remove test directory");
    }

    #[test]
    fn drop_source_finishes_after_observing_press_then_release() {
        let mut saw_left_down = false;
        assert_eq!(
            drop_source_decision(&mut saw_left_down, true, Duration::from_millis(0)),
            DropSourceDecision::Continue
        );
        assert_eq!(
            drop_source_decision(&mut saw_left_down, false, Duration::from_millis(0)),
            DropSourceDecision::Drop
        );
    }

    #[test]
    fn drop_source_cancels_immediately_if_first_state_is_released() {
        let mut saw_left_down = false;
        assert_eq!(
            drop_source_decision(&mut saw_left_down, false, Duration::from_millis(0),),
            DropSourceDecision::Cancel
        );
        assert!(!saw_left_down);
    }

    #[test]
    fn drop_source_rejects_a_press_that_arrives_after_the_start_deadline() {
        let mut saw_left_down = false;
        assert_eq!(
            drop_source_decision(
                &mut saw_left_down,
                true,
                Duration::from_millis(DROP_SOURCE_START_TIMEOUT_MS),
            ),
            DropSourceDecision::Cancel
        );
        assert!(!saw_left_down);
    }

    #[test]
    fn normalize_paths_rejects_empty() {
        let err = normalize_paths(&[]).expect_err("empty paths should fail");
        assert_eq!(err, DragError::InvalidInput("paths is empty".to_string()));
    }

    #[test]
    fn normalize_paths_rejects_missing_path() {
        let err = normalize_paths(&[PathBuf::from("__definitely_missing__")])
            .expect_err("missing path should fail");
        match err {
            DragError::InvalidInput(message) => assert!(message.contains("path does not exist")),
            _ => panic!("unexpected error type"),
        }
    }

    #[test]
    fn drag_visual_new_validates_hotspot_and_size() {
        let err =
            DragVisual::new(16, 16, 99, 0, vec![0; 16 * 16 * 4]).expect_err("invalid hotspot");
        match err {
            DragError::InvalidInput(message) => assert!(message.contains("hotspot")),
            _ => panic!("unexpected error type"),
        }
    }

    #[test]
    fn stacked_file_cards_generates_rgba() {
        let visual = DragVisual::stacked_file_cards(5);
        assert_eq!(
            visual.rgba.len(),
            visual.width as usize * visual.height as usize * 4
        );
        assert!(visual.hotspot_x >= 0 && visual.hotspot_x < visual.width as i32);
        assert!(visual.hotspot_y >= 0 && visual.hotspot_y < visual.height as i32);
        assert!(visual.title.is_none());
    }

    #[test]
    fn labelled_drag_visual_has_a_visible_backing_panel() {
        let visual = DragVisual::labelled_stacked_file_cards(2, "two files");
        assert_eq!(visual.title.as_deref(), Some("two files"));
        assert_eq!(visual.subtitle, None);
        assert_eq!(visual.width, 320);
        assert_eq!(visual.height, 88);
        assert_eq!(
            visual.rgba.len(),
            visual.width as usize * visual.height as usize * 4
        );
        assert_eq!((visual.hotspot_x, visual.hotspot_y), (16, 12));

        let panel_pixel = (20 * visual.width as usize + 120) * 4;
        assert_eq!(visual.rgba[panel_pixel + 3], 255);

        let text_backing_pixel = (20 * visual.width as usize + 130) * 4;
        assert_eq!(visual.rgba[text_backing_pixel + 3], 255);
        let card_pixel = (22 * visual.width as usize + 50) * 4;
        assert_ne!(
            &visual.rgba[text_backing_pixel..text_backing_pixel + 3],
            &visual.rgba[card_pixel..card_pixel + 3]
        );

        // 右下に重なったカードの端が残り、複数アイテムのスタックを示す。
        let rear_card_edge = (60 * visual.width as usize + 84) * 4;
        assert_ne!(visual.rgba[rear_card_edge + 3], 0);
    }

    #[test]
    fn labelled_drag_visual_scales_canvas_hotspot_and_text_layout() {
        let DragVisualSource::CustomScaled {
            visual: one,
            scale_milli: one_scale,
        } = DragVisualSource::labelled_stacked_file_cards_scaled(2, "two files", 1.0)
        else {
            panic!("expected scaled visual");
        };
        assert_eq!((one.width, one.height), (320, 88));
        assert_eq!((one.hotspot_x, one.hotspot_y), (16, 12));
        assert_eq!(one.rgba.len(), 320 * 88 * 4);
        assert_eq!(one_scale, 1000);
        assert_eq!(
            text_rects_for_scale(one_scale),
            ((128, 19, 308, 43), (128, 48, 308, 63))
        );
        assert_eq!(scaled_logical_px(20, one_scale), 20);

        let DragVisualSource::CustomScaled {
            visual: two,
            scale_milli: two_scale,
        } = DragVisualSource::labelled_stacked_file_cards_scaled(2, "two files", 2.0)
        else {
            panic!("expected scaled visual");
        };
        assert_eq!((two.width, two.height), (480, 132));
        assert_eq!((two.hotspot_x, two.hotspot_y), (24, 18));
        assert_eq!(two.rgba.len(), 480 * 132 * 4);
        assert_eq!(two_scale, 1500);
        assert_eq!(
            text_rects_for_scale(two_scale),
            ((192, 29, 462, 65), (192, 72, 462, 95))
        );
        assert_eq!(scaled_logical_px(20, two_scale), 30);

        #[cfg(target_os = "windows")]
        {
            let (title, subtitle) = compute_text_rects(&two, Some(two_scale));
            assert_eq!(
                (title.left, title.top, title.right, title.bottom),
                (192, 29, 462, 65)
            );
            assert_eq!(
                (subtitle.left, subtitle.top, subtitle.right, subtitle.bottom),
                (192, 72, 462, 95)
            );
        }
    }

    #[test]
    fn labelled_drag_visual_invalid_scale_falls_back_to_one_x() {
        for scale in [0.0, -1.0, f32::NAN, f32::INFINITY] {
            let DragVisualSource::CustomScaled {
                visual,
                scale_milli,
            } = DragVisualSource::labelled_stacked_file_cards_scaled(1, "one file", scale)
            else {
                panic!("expected scaled visual");
            };
            assert_eq!((visual.width, visual.height), (320, 88));
            assert_eq!((visual.hotspot_x, visual.hotspot_y), (16, 12));
            assert_eq!(scale_milli, 1000);
        }
    }

    #[test]
    fn title_ink_uses_only_opaque_backing_pixels() {
        let rect = (0, 0, 2, 1);
        let light = [240, 240, 240, 255, 220, 230, 240, 255];
        assert_eq!(title_ink_rgb(&light, 2, 1, rect), [24, 31, 43]);

        let dark = [20, 20, 20, 255, 40, 40, 40, 255];
        assert_eq!(title_ink_rgb(&dark, 2, 1, rect), [245, 249, 255]);

        // 透明画素のRGBは任意値なので、背景判定に混ぜない。
        let transparent = [255, 255, 255, 0, 1, 2, 3, 0];
        assert_eq!(title_ink_rgb(&transparent, 2, 1, rect), [245, 249, 255]);
        let mixed = [240, 240, 240, 255, 0, 0, 0, 0];
        assert_eq!(title_ink_rgb(&mixed, 2, 1, rect), [245, 249, 255]);
        assert_eq!(title_ink_rgb(&light, 2, 1, (1, 0, 1, 1)), [245, 249, 255]);
    }

    #[test]
    fn labelled_title_rect_is_fully_opaque_through_ellipsis_bounds() {
        let visual = DragVisual::labelled_stacked_file_cards(1, "a very long title");
        let title_rect = (128, 19, 308, 43);
        // Checking every pixel also covers the rightmost ellipsis area.
        assert_eq!(
            title_ink_rgb(&visual.rgba, visual.width, visual.height, title_rect),
            [24, 31, 43]
        );
        for y in title_rect.1..title_rect.3 {
            for x in title_rect.0..title_rect.2 {
                let offset = (y as usize * visual.width as usize + x as usize) * 4;
                assert_eq!(
                    visual.rgba[offset + 3],
                    255,
                    "transparent title backing at ({x}, {y})"
                );
            }
        }
    }

    #[test]
    fn labelled_drag_cards_are_opaque_but_keep_transparent_outer_canvas() {
        let visual = DragVisual::labelled_stacked_file_cards(2, "two files");
        let alpha_at = |x: u32, y: u32| visual.rgba[((y * visual.width + x) * 4 + 3) as usize];

        // 前面カードの面と、後ろのカードが覗く端は不透明に保つ。
        assert_eq!(alpha_at(50, 40), 255);
        assert_eq!(alpha_at(84, 40), 255);
        // ドラッグ画像の輪郭外は透過したままにする。
        assert_eq!(alpha_at(0, 0), 0);

        let single = DragVisual::labelled_stacked_file_cards(1, "one file");
        let shadow_alpha = single.rgba[((30 * single.width + 81) * 4 + 3) as usize];
        assert!((1..255).contains(&shadow_alpha));
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn title_and_subtitle_rects_keep_fixed_bounds_and_clearance() {
        let visual = DragVisual::new(320, 88, 16, 12, vec![0; 320 * 88 * 4])
            .unwrap()
            .with_title("one item")
            .with_subtitle("2 items");
        let (title, subtitle) = compute_text_rects(&visual, None);

        assert_eq!((title.left, title.right), (128, 308));
        assert_eq!(title.bottom - title.top, 24);
        assert!(subtitle.top - title.bottom >= 4);
        assert!(subtitle.top >= title.bottom);
        assert!(subtitle.right <= visual.width as i32);
        assert!(subtitle.bottom <= visual.height as i32);
        assert_eq!(subtitle.bottom - subtitle.top, 15);

        let compact_visual = DragVisual::new(104, 88, 16, 12, vec![0; 104 * 88 * 4])
            .unwrap()
            .with_title("compact")
            .with_subtitle("custom image");
        let (compact_title, compact_subtitle) = compute_text_rects(&compact_visual, None);
        assert_eq!((compact_title.left, compact_title.right), (52, 92));
        assert!(compact_subtitle.right <= compact_visual.width as i32);
    }
}
