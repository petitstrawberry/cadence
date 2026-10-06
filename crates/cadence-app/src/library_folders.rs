//! Registered library folders and scan status, independent of playback state.
use crate::{
    extension::Workspace,
    jobs::LibraryWorker,
    state,
    ui::{AnyView, button, caption, truncate},
};
use cadence_library::{ScanError, ScanProgress, ScanReport};
use scarlet_ui::{hstack, prelude::*, vstack};
use std::{
    any::Any,
    cell::{Cell, RefCell},
    rc::Rc,
};

#[derive(Clone)]
pub struct FolderState {
    pub active_root: State<Option<std::path::PathBuf>>,
    pub details_expanded: State<bool>,
    pub busy: State<bool>,
    pub saving: State<bool>,
    pub failure: State<String>,
    pub current_path: State<String>,
    pub progress_text: State<String>,
    pub summary: State<String>,
    pub errors: State<Vec<ScanError>>,
    error_selection: State<Option<usize>>,
    progress: Rc<RefCell<Option<ScanProgress>>>,
    report: Rc<RefCell<Option<ScanReport>>>,
    report_checked: Rc<Cell<bool>>,
}
impl FolderState {
    pub fn new() -> Self {
        Self {
            active_root: state(None),
            details_expanded: state(false),
            busy: state(false),
            saving: state(false),
            failure: state(String::new()),
            current_path: state(String::new()),
            progress_text: state("Scans include subfolders and update tags and artwork.".into()),
            summary: state("No scan in this session".into()),
            errors: state(vec![]),
            error_selection: state(None),
            progress: Rc::new(RefCell::new(None)),
            report: Rc::new(RefCell::new(None)),
            report_checked: Rc::new(Cell::new(false)),
        }
    }
    pub fn scan_started(&self) {
        if !self.busy.get() {
            self.busy.set(true);
        }
        self.report_checked.set(false);
        if self.saving.get() {
            self.saving.set(false);
        }
        set_changed(&self.failure, String::new());
        *self.progress.borrow_mut() = None;
        *self.report.borrow_mut() = None;
        self.errors.set(vec![]);
        self.error_selection.set(None);
        set_changed(&self.summary, "Scan queued…".into());
        set_changed(&self.current_path, String::new());
        set_changed(
            &self.progress_text,
            "Preparing to scan the selected folders…".into(),
        );
    }
    pub fn poll(&self, worker: &LibraryWorker) {
        let busy = worker.is_scanning();
        if self.busy.get() != busy {
            if busy {
                self.report_checked.set(false);
            }
            self.busy.set(busy);
        }
        let progress = worker.scan_progress();
        self.update_phase(busy, progress.as_ref());
        if *self.progress.borrow() != progress {
            if let Some(progress) = &progress {
                set_changed(
                    &self.current_path,
                    progress
                        .current_path
                        .as_ref()
                        .map(|path| path.display().to_string())
                        .unwrap_or_default(),
                );
                set_changed(
                    &self.progress_text,
                    format!(
                        "{} entries checked · {} audio files · {} added · {} updated · {} skipped · {} errors",
                        progress.entries_seen,
                        progress.audio_files_processed,
                        progress.added,
                        progress.updated,
                        progress.skipped,
                        progress.error_count
                    ),
                );
                if !progress.finished {
                    set_changed(&self.summary, "Scanning folders…".into());
                }
            }
            *self.progress.borrow_mut() = progress;
        }
        // Reports can include up to 1024 errors. Read them only after a scan,
        // rather than cloning the error list during every progress tick.
        if !busy && !self.report_checked.replace(true) {
            let failure = worker.scan_failure();
            set_changed(&self.failure, failure.clone().unwrap_or_default());
            let report = worker.scan_report();
            if let Some(error) = failure {
                set_changed(&self.summary, "Scan not saved · Library unchanged".into());
                set_changed(&self.progress_text, report.as_ref().map(|report| format!(
                    "{} entries checked · {} audio files · {} metadata errors · No changes saved",
                    report.entries_seen, report.audio_files_processed, report.error_count
                )).unwrap_or_else(|| "The library rejected this scan; no changes were made.".into()));
                set_changed(&self.failure, error);
                self.errors.set(
                    report
                        .as_ref()
                        .map(|report| report.errors.clone())
                        .unwrap_or_default(),
                );
                self.error_selection.set(None);
                *self.report.borrow_mut() = report;
            } else if *self.report.borrow() != report {
                if let Some(report) = &report {
                    let summary = if report.cancelled {
                        set_changed(
                            &self.progress_text,
                            format!(
                                "{} entries checked · {} audio files · No changes saved",
                                report.entries_seen, report.audio_files_processed
                            ),
                        );
                        format!(
                            "Scan cancelled · Library unchanged · {} errors",
                            report.error_count
                        )
                    } else {
                        format!(
                            "Scan finished · {} added · {} updated · {} skipped · {} errors{}",
                            report.added,
                            report.updated,
                            report.skipped,
                            report.error_count,
                            if report.limit_reached {
                                " · Scan limit reached"
                            } else {
                                ""
                            }
                        )
                    };
                    set_changed(&self.summary, summary);
                    self.errors.set(report.errors.clone());
                    self.error_selection.set(None);
                }
                *self.report.borrow_mut() = report;
            }
        }
    }
    fn update_phase(&self, busy: bool, progress: Option<&ScanProgress>) {
        let saving =
            busy && progress.is_some_and(|progress| progress.finished && !progress.cancelled);
        if self.saving.get() != saving {
            self.saving.set(saving);
        }
        if saving {
            set_changed(&self.summary, "Saving library…".into());
        }
    }
    pub fn can_cancel(&self) -> bool {
        self.busy.get() && !self.saving.get()
    }
}
fn set_changed(state: &State<String>, value: String) {
    if state.get() != value {
        state.set(value);
    }
}

/// Inline activity belongs to the source being refreshed, never a separate page.
pub fn source_activity(workspace: &Workspace, width: f32, height: f32) -> AnyView {
    if height == 0. {
        return AnyView::new(Spacer::new().frame(width, 0.));
    }
    AnyView::new(SourceActivity {
        folders: workspace.folders.clone(),
        width,
        height,
    })
}
#[derive(Clone)]
struct SourceActivity {
    folders: FolderState,
    width: f32,
    height: f32,
}
impl SourceActivity {
    fn body(&self) -> AnyView {
        let folders = &self.folders;
        let toggle = folders.details_expanded.clone();
        let has_errors = !folders.errors.get().is_empty() || !folders.failure.get().is_empty();
        let details = if has_errors {
            AnyView::new(
                button(if folders.details_expanded.get() {
                    "Hide details"
                } else {
                    "Details"
                })
                .header_style()
                .on_click(move || toggle.set(!toggle.get()))
                .frame(100., 28.),
            )
        } else {
            AnyView::new(Spacer::new().frame(0., 28.))
        };
        let errors = folders.errors.get();
        let expanded = folders.details_expanded.get();
        let content_width = self.width - 64.;
        let error_rows = if expanded {
            AnyView::new(vstack! {
                caption(truncate(&folders.failure.get(), (content_width / 6.5) as usize)).alignment(Alignment::Leading).frame(content_width, if folders.failure.get().is_empty() {0.} else {24.}),
                ListView::new(state(errors),state(None),48.,move |_,error,_| {
                    hstack! { vstack! {
                        caption(truncate(&error.path.display().to_string(), (content_width / 6.5) as usize)),
                        caption(truncate(&error.message, (content_width / 6.5) as usize)),
                    }.spacing(2.).alignment(Alignment::Leading).frame_width(content_width - 12.)
                    }.frame(content_width,48.)
                }).frame(content_width,(folders.errors.get().len() as f32 * 48.).min(144.)),
            }.spacing(0.))
        } else {
            AnyView::new(Spacer::new().frame(content_width, 0.))
        };
        AnyView::new(Surface::section(vstack! {
            hstack! {
                vstack! {
                    Text::from_state(folders.summary.clone()).font_size(12.).alignment(Alignment::Leading).frame(content_width - 112.,22.),
                    Text::from_state(folders.progress_text.clone()).font_size(11.).color(ColorPalette::default().text_secondary()).alignment(Alignment::Leading).frame(content_width - 112.,22.),
                }.spacing(0.),
                details,
            }.spacing(12.).frame(content_width,48.),
            error_rows,
        }.spacing(0.).frame(content_width,self.height - 16.).padding_insets(EdgeInsets::new(12.,4.,12.,4.))).frame(self.width - 40., self.height - 8.)
            .padding_insets(EdgeInsets::new(20.,4.,20.,4.)).frame(self.width,self.height))
    }
}
impl View for SourceActivity {
    fn create_element(&self) -> Box<dyn Element> {
        Box::new(scarlet_ui::ComponentElement::new_with_builder(
            self.clone(),
            |view| Box::new(view.body()),
        ))
    }
    fn listenables(&self) -> Vec<&dyn Listenable> {
        vec![
            &self.folders.summary,
            &self.folders.errors,
            &self.folders.failure,
            &self.folders.details_expanded,
        ]
    }
    fn as_any(&self) -> &dyn Any {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::jobs::LibraryCommand;
    use std::{
        path::PathBuf,
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        },
        time::{Duration, Instant},
    };

    static NEXT_DIRECTORY: AtomicUsize = AtomicUsize::new(0);
    struct Directory(PathBuf);
    impl Directory {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "cadence-folder-ui-{}-{}",
                std::process::id(),
                NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir_all(path.join("music")).unwrap();
            Self(path)
        }
    }
    impl Drop for Directory {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    fn wait_for_scan(state: &FolderState, worker: &LibraryWorker) {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            while worker.try_recv().is_some() {}
            state.poll(worker);
            if !state.busy.get() && state.report_checked.get() {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "scan state did not finish: {}",
                state.summary.get()
            );
            std::thread::sleep(Duration::from_millis(2));
        }
    }
    #[test]
    fn cancellation_is_available_during_scanning_but_not_atomic_save() {
        let folders = FolderState::new();
        folders.scan_started();
        folders.update_phase(true, Some(&ScanProgress::default()));
        assert!(folders.can_cancel());
        folders.update_phase(
            true,
            Some(&ScanProgress {
                finished: true,
                ..ScanProgress::default()
            }),
        );
        assert!(folders.saving.get());
        assert!(!folders.can_cancel());
        assert_eq!(folders.summary.get(), "Saving library…");
        folders.busy.set(false);
        folders.update_phase(
            false,
            Some(&ScanProgress {
                finished: true,
                ..ScanProgress::default()
            }),
        );
        assert!(!folders.saving.get());
        assert!(!folders.can_cancel());
    }

    #[test]
    fn rejected_read_only_scan_shows_failure_instead_of_queued() {
        let directory = Directory::new();
        let data = directory.0.join("data");
        std::fs::create_dir_all(&data).unwrap();
        std::fs::write(data.join("library.json"), b"corrupt catalog").unwrap();
        let worker = LibraryWorker::new(data);
        let folders = FolderState::new();
        worker
            .send(LibraryCommand::Scan(directory.0.join("music")))
            .unwrap();
        folders.scan_started();
        wait_for_scan(&folders, &worker);
        assert_eq!(folders.summary.get(), "Scan not saved · Library unchanged");
        assert!(!folders.failure.get().is_empty());
        assert!(!folders.summary.get().contains("added"));
        assert!(folders.progress_text.get().contains("no changes"));
    }

    #[test]
    fn persistence_failure_does_not_present_discovered_tracks_as_saved() {
        let directory = Directory::new();
        let data = directory.0.join("data");
        let worker = LibraryWorker::new(data.clone());
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if worker.try_recv().is_some() {
                break;
            }
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(2));
        }
        std::fs::create_dir_all(data.join("library.json")).unwrap();
        let samples = vec![0u8; 9_600];
        let mut wav = Vec::new();
        wav.extend_from_slice(b"RIFF");
        wav.extend_from_slice(&(36u32 + samples.len() as u32).to_le_bytes());
        wav.extend_from_slice(b"WAVEfmt ");
        wav.extend_from_slice(&16u32.to_le_bytes());
        wav.extend_from_slice(&1u16.to_le_bytes());
        wav.extend_from_slice(&1u16.to_le_bytes());
        wav.extend_from_slice(&48_000u32.to_le_bytes());
        wav.extend_from_slice(&96_000u32.to_le_bytes());
        wav.extend_from_slice(&2u16.to_le_bytes());
        wav.extend_from_slice(&16u16.to_le_bytes());
        wav.extend_from_slice(b"data");
        wav.extend_from_slice(&(samples.len() as u32).to_le_bytes());
        wav.extend_from_slice(&samples);
        std::fs::write(directory.0.join("music/song.wav"), wav).unwrap();
        let folders = FolderState::new();
        worker
            .send(LibraryCommand::Scan(directory.0.join("music")))
            .unwrap();
        folders.scan_started();
        wait_for_scan(&folders, &worker);
        assert_eq!(folders.report.borrow().as_ref().unwrap().added, 1);
        assert_eq!(folders.summary.get(), "Scan not saved · Library unchanged");
        assert!(!folders.failure.get().is_empty());
        assert!(!folders.summary.get().contains("1 added"));
        assert!(folders.progress_text.get().contains("No changes saved"));
    }

    #[test]
    fn identical_rescan_results_complete_and_idle_polling_does_not_rebuild_labels() {
        let directory = Directory::new();
        let worker = LibraryWorker::new(directory.0.join("data"));
        let folders = FolderState::new();
        for _ in 0..2 {
            worker
                .send(LibraryCommand::Scan(directory.0.join("music")))
                .unwrap();
            folders.scan_started();
            assert!(folders.busy.get());
            wait_for_scan(&folders, &worker);
            assert!(folders.summary.get().contains("0 errors"));
        }
        let calls = Arc::new(AtomicUsize::new(0));
        let summary_calls = calls.clone();
        folders.summary.subscribe_any(Arc::new(move || {
            summary_calls.fetch_add(1, Ordering::Relaxed);
        }));
        let progress_calls = calls.clone();
        folders.progress_text.subscribe_any(Arc::new(move || {
            progress_calls.fetch_add(1, Ordering::Relaxed);
        }));
        let path_calls = calls.clone();
        folders.current_path.subscribe_any(Arc::new(move || {
            path_calls.fetch_add(1, Ordering::Relaxed);
        }));
        for _ in 0..25 {
            folders.poll(&worker);
        }
        assert_eq!(calls.load(Ordering::Relaxed), 0);
    }
}
