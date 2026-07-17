//! macOS: draw the connected-tunnel count as a **native** two-line title on the
//! tray's status-bar button — "sshoal" over "<count> ●" (white text, green dot).
//! Native text isn't capped at 18pt like the tray-icon image, so it fills the
//! menu-bar height, stays crisp, and needs no fork of tray-icon. It does reach
//! the `NSStatusBarButton` via the app's windows (private-ish), so every call is
//! best-effort: if the button can't be found it simply does nothing.
#![cfg(target_os = "macos")]

use objc2::rc::Retained;
use objc2_app_kit::{
    NSApplication, NSBaselineOffsetAttributeName, NSColor, NSFont, NSFontAttributeName,
    NSFontWeightSemibold, NSForegroundColorAttributeName, NSMutableParagraphStyle,
    NSParagraphStyleAttributeName, NSStatusBarButton, NSTextAlignment, NSView,
};
use objc2_foundation::{MainThreadMarker, NSMutableAttributedString, NSNumber, NSRange, NSString};

/// Green dot colour — aimonitor's dark-appearance "safe" green (0.46,0.95,0.58).
const DOT: (f64, f64, f64) = (0.46, 0.95, 0.58);

/// Recursively find an `NSStatusBarButton` in a view tree.
fn find_button(view: Retained<NSView>) -> Option<Retained<NSStatusBarButton>> {
    match view.downcast::<NSStatusBarButton>() {
        Ok(btn) => Some(btn),
        Err(view) => view.subviews().iter().find_map(find_button),
    }
}

fn status_button(mtm: MainThreadMarker) -> Option<Retained<NSStatusBarButton>> {
    let app = NSApplication::sharedApplication(mtm);
    app.windows()
        .iter()
        .filter_map(|w| w.contentView())
        .find_map(find_button)
}

/// Set the tray button's native title to "sshoal" / "<count> ●". Returns whether
/// the button was found (so the caller can fall back to the image icon).
pub fn set_count(count: usize) -> bool {
    let Some(mtm) = MainThreadMarker::new() else {
        return false;
    };
    let Some(btn) = status_button(mtm) else {
        return false;
    };

    // Show the green ● only when something is connected; at 0 it's just "0".
    let has_dot = count > 0;
    let text = if has_dot {
        format!("sshoal\n{count} ●")
    } else {
        format!("sshoal\n{count}")
    };
    let ns = NSString::from_str(&text);
    let len = ns.length();
    let attr = NSMutableAttributedString::from_nsstring(&ns);
    let full = NSRange::new(0, len);
    let name_len = "sshoal\n".len(); // 7 (ASCII + \n → UTF-16 units match)
    let name = NSRange::new(0, name_len);
    let num = NSRange::new(name_len, len - name_len);

    // aimonitor's exact recipe: per-line paragraph styles (name line 11pt, number
    // line 12pt, 1px gap) so the bigger number line isn't clamped to the name's
    // height, plus a -4 baseline offset over the whole string that centres the
    // pair vertically (the status button top-anchors the title otherwise).
    let para_name = NSMutableParagraphStyle::new();
    para_name.setAlignment(NSTextAlignment::Center);
    para_name.setMinimumLineHeight(11.0);
    para_name.setMaximumLineHeight(11.0);
    para_name.setParagraphSpacing(1.0);
    let para_num = NSMutableParagraphStyle::new();
    para_num.setAlignment(NSTextAlignment::Center);
    para_num.setMinimumLineHeight(12.0);
    para_num.setMaximumLineHeight(12.0);

    let name_font = NSFont::systemFontOfSize(10.0); // line 1, regular
    let semibold = unsafe { NSFontWeightSemibold };
    let num_font = NSFont::monospacedDigitSystemFontOfSize_weight(12.0, semibold);
    let white = NSColor::whiteColor();
    let green = NSColor::colorWithSRGBRed_green_blue_alpha(DOT.0, DOT.1, DOT.2, 1.0);
    let offset = NSNumber::new_f64(-4.0);

    unsafe {
        attr.addAttribute_value_range(NSForegroundColorAttributeName, &white, full);
        attr.addAttribute_value_range(NSBaselineOffsetAttributeName, &offset, full);
        attr.addAttribute_value_range(NSFontAttributeName, &name_font, name);
        attr.addAttribute_value_range(NSParagraphStyleAttributeName, &para_name, name);
        attr.addAttribute_value_range(NSFontAttributeName, &num_font, num);
        attr.addAttribute_value_range(NSParagraphStyleAttributeName, &para_num, num);
        if has_dot {
            let dot = NSRange::new(len - 1, 1); // the "●" (1 UTF-16 unit)
            attr.addAttribute_value_range(NSForegroundColorAttributeName, &green, dot);
        }
        btn.setImage(None);
        btn.setAttributedTitle(&attr);
    }
    true
}
