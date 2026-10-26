mod verify;

#[cfg(windows)]
mod source;

/// Create7z execution is deliberately separate from ZIP's path-based adapter.
/// The worker receives the provider owner captured by the application pump.
#[cfg(windows)]
pub(super) fn execute(
    token: super::super::model::AttemptToken,
    request: &super::super::model::OperationRequest,
    provider: Option<crate::archive_provider::ArchiveProviderOwner>,
    cancelled: &std::sync::atomic::AtomicBool,
    controls: &std::sync::mpsc::Receiver<super::WorkerControl>,
    emit: &mut impl FnMut(super::ExecutorFact),
) {
    source::execute(token, request, provider, cancelled, controls, emit);
}

#[cfg(all(test, windows))]
pub(super) fn execute_with_image_for_test(
    token: super::super::model::AttemptToken,
    request: &super::super::model::OperationRequest,
    provider: crate::archive_provider::ArchiveProviderOwner,
    image: crate::archive_provider::application_image::ApplicationImageLease,
    cancelled: &std::sync::atomic::AtomicBool,
    controls: &std::sync::mpsc::Receiver<super::WorkerControl>,
    emit: &mut impl FnMut(super::ExecutorFact),
) {
    source::execute_with_image_for_test(token, request, provider, image, cancelled, controls, emit);
}

#[cfg(all(test, windows))]
mod custody_tests;
