//! Native folder selection without blocking the application's UI event loop.
//!
//! Construct, poll, and drop a picker on the UI thread. Platforms without an
//! adapter return [`FolderPickerError::Unsupported`] so callers can offer an
//! in-app browser. Other errors describe an actual native-dialog failure.

use std::{fmt, marker::PhantomData, path::PathBuf, rc::Rc};

#[cfg(target_os = "macos")]
#[path = "folder_picker_macos.rs"]
mod macos;

#[derive(Clone, Debug)]
pub struct FolderPickerOptions {
    pub title: String,
    pub initial_directory: Option<PathBuf>,
}

impl Default for FolderPickerOptions {
    fn default() -> Self {
        Self {
            title: "Add music folder".into(),
            initial_directory: None,
        }
    }
}

impl FolderPickerOptions {
    fn validate(&self) -> Result<(), FolderPickerError> {
        if let Some(directory) = &self.initial_directory {
            if !directory.is_absolute() {
                return Err(FolderPickerError::InvalidOptions(
                    "initial directory must be an absolute path".into(),
                ));
            }
            #[cfg(unix)]
            {
                use std::os::unix::ffi::OsStrExt;
                if directory.as_os_str().as_bytes().contains(&0) {
                    return Err(FolderPickerError::InvalidOptions(
                        "initial directory contains NUL".into(),
                    ));
                }
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FolderPickerOutcome {
    Selected(PathBuf),
    Cancelled,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FolderPickerError {
    Unsupported,
    Busy,
    OwnerClosed,
    InvalidOptions(String),
    Platform(String),
}

impl fmt::Display for FolderPickerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unsupported => f.write_str("Native folder selection is unavailable"),
            Self::Busy => f.write_str("Another dialog is already open for this window"),
            Self::OwnerClosed => f.write_str("The folder dialog's window is unavailable"),
            Self::InvalidOptions(message) | Self::Platform(message) => f.write_str(message),
        }
    }
}

impl std::error::Error for FolderPickerError {}

pub type FolderPickerResult = Result<FolderPickerOutcome, FolderPickerError>;

/// A pending native dialog. Dropping it cancels an unfinished selection.
///
/// The handle is deliberately neither `Send` nor `Sync`, including on platforms
/// without an adapter, so adding another backend cannot change its ownership.
pub struct FolderPicker {
    #[cfg(target_os = "macos")]
    session: macos::Session,
    _ui_thread: PhantomData<Rc<()>>,
}

impl FolderPicker {
    /// Show a single-folder sheet on the application's key or main window.
    pub fn show(options: FolderPickerOptions) -> Result<Self, FolderPickerError> {
        options.validate()?;
        #[cfg(target_os = "macos")]
        {
            Ok(Self {
                session: macos::Session::begin(options)?,
                _ui_thread: PhantomData,
            })
        }
        #[cfg(not(target_os = "macos"))]
        {
            Err(FolderPickerError::Unsupported)
        }
    }

    /// Retrieve the selection once; pending and consumed results return `None`.
    pub fn take_result(&mut self) -> Option<FolderPickerResult> {
        #[cfg(target_os = "macos")]
        {
            self.session.take_result()
        }
        #[cfg(not(target_os = "macos"))]
        {
            None
        }
    }

    /// Cancel a pending dialog. The next poll returns `Cancelled` once.
    pub fn cancel(&mut self) {
        #[cfg(target_os = "macos")]
        self.session.cancel();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn invalid_directory_is_rejected_before_opening_a_dialog() {
        let result = FolderPicker::show(FolderPickerOptions {
            initial_directory: Some("relative/music".into()),
            ..FolderPickerOptions::default()
        });
        assert!(matches!(result, Err(FolderPickerError::InvalidOptions(_))));
    }

    #[cfg(unix)]
    #[test]
    fn nul_directory_is_rejected_before_opening_a_dialog() {
        use std::os::unix::ffi::OsStrExt;
        let result = FolderPicker::show(FolderPickerOptions {
            initial_directory: Some(PathBuf::from(std::ffi::OsStr::from_bytes(b"/music/\0"))),
            ..FolderPickerOptions::default()
        });
        assert!(matches!(result, Err(FolderPickerError::InvalidOptions(_))));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn appkit_is_not_called_from_a_worker_thread() {
        let error =
            std::thread::spawn(
                || match FolderPicker::show(FolderPickerOptions::default()) {
                    Err(error) => error,
                    Ok(_) => panic!("worker thread unexpectedly opened a native dialog"),
                },
            )
            .join()
            .unwrap();
        assert!(matches!(error, FolderPickerError::Platform(_)));
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn unavailable_adapter_has_an_explicit_fallback_error() {
        assert!(matches!(
            FolderPicker::show(FolderPickerOptions::default()),
            Err(FolderPickerError::Unsupported)
        ));
    }
}
