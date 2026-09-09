//! Native password entry for mail provisioning.
//!
//! Non-secret endpoint details may cross the Tauri command boundary, but
//! passwords never enter the WebView or IPC payload. On macOS they are typed
//! into secure AppKit controls, copied once into zeroizing Rust storage, and
//! the controls are cleared before the dialog is released.

use tauri::AppHandle;
use zeroize::{Zeroize, Zeroizing};

#[derive(Zeroize)]
pub(crate) struct MailPasswords {
    pub imap: String,
    pub smtp: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PromptError {
    Unavailable,
}

#[cfg(target_os = "macos")]
pub(crate) fn prompt_mail_passwords(
    app: &AppHandle,
) -> Result<Option<Zeroizing<MailPasswords>>, PromptError> {
    use objc2::MainThreadMarker;
    use std::sync::mpsc::sync_channel;

    if let Some(mtm) = MainThreadMarker::new() {
        return Ok(show_mail_password_prompt(mtm));
    }
    let (sender, receiver) = sync_channel(1);
    app.run_on_main_thread(move || {
        let result = MainThreadMarker::new().map(show_mail_password_prompt);
        let _ = sender.send(result);
    })
    .map_err(|_| PromptError::Unavailable)?;
    receiver
        .recv()
        .map_err(|_| PromptError::Unavailable)?
        .ok_or(PromptError::Unavailable)
}

#[cfg(target_os = "macos")]
fn show_mail_password_prompt(mtm: objc2::MainThreadMarker) -> Option<Zeroizing<MailPasswords>> {
    use objc2::MainThreadOnly;
    use objc2_app_kit::{
        NSAccessibility, NSAlert, NSAlertFirstButtonReturn, NSAlertStyle, NSSecureTextField,
        NSTextField, NSView,
    };
    use objc2_foundation::{NSPoint, NSRect, NSSize, NSString};

    fn frame(x: f64, y: f64, width: f64, height: f64) -> NSRect {
        NSRect::new(NSPoint::new(x, y), NSSize::new(width, height))
    }

    let alert = NSAlert::new(mtm);
    alert.setAlertStyle(NSAlertStyle::Informational);
    alert.setMessageText(&NSString::from_str("Enter mail passwords"));
    alert.setInformativeText(&NSString::from_str(
        "Native Mux uses these only to verify the configured servers, then stores them in the macOS Keychain. They never enter the WebView.",
    ));
    alert.addButtonWithTitle(&NSString::from_str("Connect"));
    alert.addButtonWithTitle(&NSString::from_str("Cancel"));

    let accessory = NSView::initWithFrame(NSView::alloc(mtm), frame(0.0, 0.0, 420.0, 112.0));
    let imap_label = NSTextField::labelWithString(&NSString::from_str("IMAP password"), mtm);
    imap_label.setFrame(frame(0.0, 88.0, 420.0, 18.0));
    let imap = NSSecureTextField::initWithFrame(
        NSSecureTextField::alloc(mtm),
        frame(0.0, 58.0, 420.0, 24.0),
    );
    imap.setPlaceholderString(Some(&NSString::from_str("Incoming mail password")));
    imap.setAccessibilityLabel(Some(&NSString::from_str("IMAP password")));

    let smtp_label = NSTextField::labelWithString(&NSString::from_str("SMTP password"), mtm);
    smtp_label.setFrame(frame(0.0, 30.0, 420.0, 18.0));
    let smtp = NSSecureTextField::initWithFrame(
        NSSecureTextField::alloc(mtm),
        frame(0.0, 0.0, 420.0, 24.0),
    );
    smtp.setPlaceholderString(Some(&NSString::from_str("Outgoing mail password")));
    smtp.setAccessibilityLabel(Some(&NSString::from_str("SMTP password")));

    accessory.addSubview(&imap_label);
    accessory.addSubview(&imap);
    accessory.addSubview(&smtp_label);
    accessory.addSubview(&smtp);
    alert.setAccessoryView(Some(&accessory));
    alert.window().setInitialFirstResponder(Some(&imap));
    let accepted = alert.runModal() == NSAlertFirstButtonReturn;
    let values = accepted.then(|| {
        Zeroizing::new(MailPasswords {
            imap: imap.stringValue().to_string(),
            smtp: smtp.stringValue().to_string(),
        })
    });
    let empty = NSString::from_str("");
    imap.setStringValue(&empty);
    smtp.setStringValue(&empty);
    values
}

#[cfg(not(target_os = "macos"))]
pub(crate) fn prompt_mail_passwords(
    _app: &AppHandle,
) -> Result<Option<Zeroizing<MailPasswords>>, PromptError> {
    Err(PromptError::Unavailable)
}
