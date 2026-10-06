use super::{FolderPickerError, FolderPickerOptions, FolderPickerOutcome, FolderPickerResult};
use block2::RcBlock;
use objc2::{MainThreadMarker, rc::Retained};
use objc2_app_kit::{NSApplication, NSModalResponseCancel, NSModalResponseOK, NSOpenPanel};
use objc2_foundation::{NSString, NSURL};
use std::{
    cell::{Cell, RefCell},
    ffi::{CStr, CString, OsStr},
    os::unix::ffi::OsStrExt,
    path::PathBuf,
    ptr::NonNull,
    rc::Rc,
};

pub(super) struct Session {
    panel: Rc<Retained<NSOpenPanel>>,
    result: Rc<RefCell<Option<FolderPickerResult>>>,
    finished: Rc<Cell<bool>>,
}

impl Session {
    pub(super) fn begin(options: FolderPickerOptions) -> Result<Self, FolderPickerError> {
        let mtm = MainThreadMarker::new().ok_or_else(|| {
            FolderPickerError::Platform("Native folder selection requires the UI thread".into())
        })?;
        let application = NSApplication::sharedApplication(mtm);
        let owner = application
            .keyWindow()
            .or_else(|| application.mainWindow())
            .ok_or(FolderPickerError::OwnerClosed)?;
        if owner.attachedSheet().is_some() {
            return Err(FolderPickerError::Busy);
        }

        let panel = Rc::new(NSOpenPanel::openPanel(mtm));
        panel.setCanChooseDirectories(true);
        panel.setCanChooseFiles(false);
        panel.setAllowsMultipleSelection(false);
        panel.setCanCreateDirectories(false);
        if !options.title.is_empty() {
            panel.setTitle(Some(&NSString::from_str(&options.title)));
        }
        if let Some(directory) = options.initial_directory {
            let path = CString::new(directory.as_os_str().as_bytes()).map_err(|_| {
                FolderPickerError::InvalidOptions("initial directory contains NUL".into())
            })?;
            // SAFETY: `path` is a live NUL-terminated filesystem representation.
            // NSURL copies it, and all AppKit operations run on the main thread.
            let url = unsafe {
                NSURL::fileURLWithFileSystemRepresentation_isDirectory_relativeToURL(
                    NonNull::new(path.as_ptr().cast_mut()).unwrap(),
                    true,
                    None,
                )
            };
            panel.setDirectoryURL(Some(&url));
        }

        let result = Rc::new(RefCell::new(None));
        let finished = Rc::new(Cell::new(false));
        let callback_result = result.clone();
        let callback_finished = finished.clone();
        // The panel retains its completion block. Keeping only a weak panel
        // reference avoids retaining it through a panel -> block -> panel cycle.
        let weak_panel = Rc::downgrade(&panel);
        let completion = RcBlock::new(move |response| {
            if callback_finished.replace(true) {
                return;
            }
            let selection = if response == NSModalResponseCancel {
                Ok(FolderPickerOutcome::Cancelled)
            } else if response == NSModalResponseOK {
                weak_panel
                    .upgrade()
                    .ok_or(FolderPickerError::OwnerClosed)
                    .and_then(|panel| {
                        let urls = panel.URLs();
                        if urls.count() != 1 {
                            return Err(FolderPickerError::Platform(
                                "Folder dialog returned an invalid selection".into(),
                            ));
                        }
                        let url = urls.objectAtIndex(0);
                        // SAFETY: the URL owns its NUL-terminated representation
                        // for the duration of this copy. Preserve raw path bytes.
                        let bytes = unsafe {
                            CStr::from_ptr(url.fileSystemRepresentation().as_ptr()).to_bytes()
                        };
                        Ok(FolderPickerOutcome::Selected(PathBuf::from(
                            OsStr::from_bytes(bytes),
                        )))
                    })
            } else {
                Err(FolderPickerError::Platform(format!(
                    "Folder dialog could not complete (response {response})"
                )))
            };
            *callback_result.borrow_mut() = Some(selection);
        });
        panel.beginSheetModalForWindow_completionHandler(&owner, &completion);
        Ok(Self {
            panel,
            result,
            finished,
        })
    }

    pub(super) fn take_result(&mut self) -> Option<FolderPickerResult> {
        self.result.borrow_mut().take()
    }

    pub(super) fn cancel(&mut self) {
        if !self.finished.replace(true) {
            // SAFETY: this UI-thread-only session owns the pending AppKit panel.
            unsafe { self.panel.cancel(None) };
            *self.result.borrow_mut() = Some(Ok(FolderPickerOutcome::Cancelled));
        }
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.cancel();
    }
}
