//! Facts about this PC's input and display that change what is worth recommending.

/// Simultaneous touch points the touch screen supports (0 = no touch screen).
pub fn touch_points() -> u32 {
    use windows::Win32::UI::WindowsAndMessaging::{GetSystemMetrics, SM_MAXIMUMTOUCHES};
    unsafe { GetSystemMetrics(SM_MAXIMUMTOUCHES) }.max(0) as u32
}

/// Display scaling of the primary display, in percent (100 = 96 DPI).
pub fn display_scale() -> u32 {
    use windows::Win32::UI::HiDpi::GetDpiForSystem;
    let dpi = unsafe { GetDpiForSystem() };
    dpi * 100 / 96
}

#[cfg(test)]
mod tests {
    #[test]
    fn detection_runs() {
        let scale = super::display_scale();
        assert!((100..=500).contains(&scale), "{scale}");
        let _ = super::touch_points();
        for d in crate::drives::list() {
            // Must not fail, whatever the answer; printed for the test log.
            println!("{}: ssd = {:?}", d.letter, crate::drives::is_ssd(d.letter));
        }
        assert_eq!(crate::drives::is_ssd('#'), None);
    }
}
