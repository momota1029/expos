use std::rc::Rc;

pub fn current_platform() -> Rc<dyn gpui::Platform> {
    #[cfg(windows)]
    {
        Rc::new(
            gpui_windows::WindowsPlatform::new(false)
                .expect("failed to initialize Windows platform"),
        )
    }

    #[cfg(target_os = "linux")]
    {
        gpui_linux::current_platform(false)
    }

    #[cfg(not(any(windows, target_os = "linux")))]
    unimplemented!("expos supports Windows and Linux");
}
