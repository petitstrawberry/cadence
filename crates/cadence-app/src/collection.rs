//! Library collections and their bounded asynchronous artwork cache.
//!
//! Layout follows KanadeApp's album grid and artist list. Collection, detail,
//! and artwork state stay here; playback ticks do not rebuild the gallery.
use crate::{
    Cadence,
    extension::{Tab, Workspace},
    state,
    ui::{
        AnyView, LIBRARY_HEADER_HEIGHT, PLAYER_CLEARANCE, back_header, button, caption, clock,
        counted, library_header, selected_color, text, truncate,
    },
};
use cadence_library::{ArtworkRef, LibraryTrack, MediaLocation};
use cadence_sources::{SourceConfig, SourceRegistry};
use scarlet_ui::{
    hstack,
    prelude::*,
    views::{BitmapImage, ImageFit},
    vstack, zstack,
};
use std::{
    any::Any,
    cell::{Cell, RefCell},
    collections::{BTreeMap, HashMap, VecDeque},
    path::PathBuf,
    rc::Rc,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
        mpsc,
    },
    time::Duration,
};

const COVER_EDGE: u32 = 256;
const COVER_BUDGET: usize = 32 * 1024 * 1024;
const PENDING_COVERS: usize = 128;
const COVER_SLOTS: usize = 512;
const COVERS_PER_POLL: usize = 4;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum CollectionKind {
    Albums,
    Artists,
}

#[derive(Clone)]
struct Group {
    key: String,
    kind: CollectionKind,
    title: String,
    subtitle: String,
    tracks: Arc<Vec<LibraryTrack>>,
    artwork: Option<(String, ArtworkRef)>,
    all_songs: bool,
}

struct LibraryWatch {
    library: State<Vec<LibraryTrack>>,
    library_subscription: scarlet_ui::SubscriptionId,
    sources: State<Vec<SourceConfig>>,
    source_subscription: scarlet_ui::SubscriptionId,
}
impl Drop for LibraryWatch {
    fn drop(&mut self) {
        self.library.unsubscribe(self.library_subscription);
        self.sources.unsubscribe(self.source_subscription);
    }
}

#[derive(Clone)]
pub struct Collections {
    albums: State<Vec<Group>>,
    artists: State<Vec<Group>>,
    album_results: State<Vec<Group>>,
    artist_results: State<Vec<Group>>,
    album_query: State<String>,
    artist_query: State<String>,
    last_queries: Rc<RefCell<(String, String)>>,
    detail: State<Option<Group>>,
    selected_artist: State<Option<Group>>,
    artist_albums: State<Vec<Group>>,
    detail_tracks: State<Vec<LibraryTrack>>,
    detail_selection: State<Option<usize>>,
    gallery_selection: State<Option<usize>>,
    watch: Rc<RefCell<Option<LibraryWatch>>>,
    dirty: Arc<AtomicBool>,
    covers: CoverCache,
    all_songs_art: BitmapImage,
}
impl Collections {
    pub fn new() -> Self {
        Self {
            albums: state(vec![]),
            artists: state(vec![]),
            album_results: state(vec![]),
            artist_results: state(vec![]),
            album_query: state(String::new()),
            artist_query: state(String::new()),
            last_queries: Rc::new(RefCell::new((String::new(), String::new()))),
            detail: state(None),
            selected_artist: state(None),
            artist_albums: state(vec![]),
            detail_tracks: state(vec![]),
            detail_selection: state(None),
            gallery_selection: state(None),
            watch: Rc::new(RefCell::new(None)),
            dirty: Arc::new(AtomicBool::new(true)),
            covers: CoverCache::new(),
            all_songs_art: all_songs_artwork(),
        }
    }

    pub fn poll(&self, workspace: &Workspace) {
        let needs_watch = self
            .watch
            .borrow()
            .as_ref()
            .is_none_or(|watch| watch.library.id() != workspace.library.id());
        if needs_watch {
            let library_dirty = self.dirty.clone();
            let source_dirty = self.dirty.clone();
            *self.watch.borrow_mut() = Some(LibraryWatch {
                library: workspace.library.clone(),
                library_subscription: workspace.library.subscribe_any(Arc::new(move || {
                    library_dirty.store(true, Ordering::Relaxed);
                })),
                sources: workspace.sources.clone(),
                source_subscription: workspace.sources.subscribe_any(Arc::new(move || {
                    source_dirty.store(true, Ordering::Relaxed);
                })),
            });
            self.dirty.store(true, Ordering::Relaxed);
        }
        let changed = self.dirty.swap(false, Ordering::Relaxed);
        if changed {
            let tracks = workspace.library.get();
            let (albums, artists) = group_library(tracks);
            if let Some(previous) = self.detail.get() {
                let groups = if previous.kind == CollectionKind::Albums {
                    &albums
                } else {
                    &artists
                };
                let replacement = groups
                    .iter()
                    .find(|group| group.key == previous.key)
                    .cloned();
                self.detail_tracks.set(
                    replacement
                        .as_ref()
                        .map(|group| group.tracks.as_ref().clone())
                        .unwrap_or_default(),
                );
                self.detail_selection.set(None);
                self.detail.set(replacement);
            }
            if let Some(previous) = self.selected_artist.get() {
                let replacement = artists
                    .iter()
                    .find(|artist| artist.key == previous.key)
                    .cloned();
                self.artist_albums.set(
                    replacement
                        .as_ref()
                        .map(|artist| albums_for_artist(&albums, artist))
                        .unwrap_or_default(),
                );
                self.selected_artist.set(replacement);
            }
            self.albums.set(albums);
            self.artists.set(artists);
            self.covers
                .set_context(workspace.sources.get(), workspace.media_cache_directory());
        }
        let queries = (self.album_query.get(), self.artist_query.get());
        let previous_queries = self.last_queries.borrow().clone();
        if changed || previous_queries.0 != queries.0 {
            self.album_results
                .set(filter_groups(self.albums.get(), &queries.0));
        }
        if changed || previous_queries.1 != queries.1 {
            self.artist_results
                .set(filter_groups(self.artists.get(), &queries.1));
        }
        *self.last_queries.borrow_mut() = queries;
        self.covers.poll();
    }

    pub fn view(&self, app: &Cadence, kind: CollectionKind, width: f32, height: f32) -> AnyView {
        AnyView::new(CollectionPane {
            app: app.clone(),
            collections: self.clone(),
            kind,
            width,
            height,
        })
    }

    pub fn return_to_root(&self) {
        self.detail.set(None);
        self.selected_artist.set(None);
        self.artist_albums.set(vec![]);
    }
    fn open_group(&self, group: Group) {
        if group.kind == CollectionKind::Artists {
            self.artist_albums
                .set(albums_for_artist(&self.albums.get(), &group));
            self.detail.set(None);
            self.selected_artist.set(Some(group));
            return;
        }
        self.detail_tracks.set(group.tracks.as_ref().clone());
        self.detail_selection.set(None);
        self.detail.set(Some(group));
    }
    fn results(&self, kind: CollectionKind) -> &State<Vec<Group>> {
        match kind {
            CollectionKind::Albums => &self.album_results,
            CollectionKind::Artists => &self.artist_results,
        }
    }
    fn query(&self, kind: CollectionKind) -> &State<String> {
        match kind {
            CollectionKind::Albums => &self.album_query,
            CollectionKind::Artists => &self.artist_query,
        }
    }
}

#[derive(Clone, PartialEq, Eq, PartialOrd, Ord)]
enum AlbumIdentity {
    /// Kanade's album identity is its parent directory, regardless of tags.
    Local(PathBuf),
    /// A provider's opaque album ID only has meaning within that source.
    Provider(String, String),
    /// Without an album identity, do not invent one from title/artist tags.
    Unassigned(String, String),
}
impl AlbumIdentity {
    fn for_track(track: &LibraryTrack) -> Self {
        if let MediaLocation::Local(path) = &track.location
            && let Some(parent) = path.parent()
        {
            return Self::Local(parent.to_owned());
        }
        if let Some(album_id) = &track.album_id {
            Self::Provider(track.source_id.clone(), album_id.clone())
        } else {
            Self::Unassigned(track.source_id.clone(), track.item_id.clone())
        }
    }

    fn key(&self) -> String {
        match self {
            Self::Local(directory) => format!("local-album:{}", directory.to_string_lossy()),
            Self::Provider(source, album) => {
                format!(
                    "provider-album:{}",
                    cadence_library::track_id(source, album)
                )
            }
            Self::Unassigned(source, item) => {
                format!(
                    "unassigned-album:{}",
                    cadence_library::track_id(source, item)
                )
            }
        }
    }
}

fn album_track_order(left: &LibraryTrack, right: &LibraryTrack) -> std::cmp::Ordering {
    // SQLite's ascending ORDER BY places NULL before numbered tracks.
    (&left.disc_number, &left.track_number, &left.title).cmp(&(
        &right.disc_number,
        &right.track_number,
        &right.title,
    ))
}

fn artist_track_order(left: &LibraryTrack, right: &LibraryTrack) -> std::cmp::Ordering {
    left.album
        .cmp(&right.album)
        .then_with(|| album_track_order(left, right))
}

fn group_library(tracks: Vec<LibraryTrack>) -> (Vec<Group>, Vec<Group>) {
    let mut albums: BTreeMap<AlbumIdentity, (usize, Vec<LibraryTrack>)> = BTreeMap::new();
    let mut artists: BTreeMap<String, Vec<LibraryTrack>> = BTreeMap::new();
    for (index, track) in tracks.iter().enumerate() {
        albums
            .entry(AlbumIdentity::for_track(track))
            .or_insert_with(|| (index, vec![]))
            .1
            .push(track.clone());
        // Kanade exposes the distinct union of artist and album_artist; a track
        // matches either field, and equal fields must not duplicate that track.
        if let Some(artist) = &track.artist {
            artists
                .entry(artist.clone())
                .or_default()
                .push(track.clone());
        }
        if let Some(album_artist) = &track.album_artist
            && track.artist.as_ref() != Some(album_artist)
        {
            artists
                .entry(album_artist.clone())
                .or_default()
                .push(track.clone());
        }
    }
    let mut ordered_albums: Vec<_> = albums.into_iter().collect();
    ordered_albums.sort_by(|(left, (left_index, _)), (right, (right_index, _))| {
        match (left, right) {
            (AlbumIdentity::Local(left), AlbumIdentity::Local(right)) => left
                .as_os_str()
                .to_string_lossy()
                .cmp(&right.as_os_str().to_string_lossy()),
            (AlbumIdentity::Local(_), _) => std::cmp::Ordering::Less,
            (_, AlbumIdentity::Local(_)) => std::cmp::Ordering::Greater,
            // Server catalog order is preserved; tags cannot recover dir_path.
            _ => left_index.cmp(right_index),
        }
    });
    let mut album_groups: Vec<_> = ordered_albums
        .into_iter()
        .map(|(identity, (_, mut tracks))| {
            // Kanade upserts the album title when each directory track is
            // scanned. It derives the artist with COALESCE(album_artist, artist)
            // from an associated track, without tag normalization or guessing.
            let title = tracks
                .last()
                .and_then(|track| track.album.clone())
                .unwrap_or_else(|| "Untitled".into());
            let artist = tracks
                .iter()
                .find_map(|track| track.album_artist.clone().or_else(|| track.artist.clone()))
                .unwrap_or_default();
            tracks.sort_by(album_track_order);
            make_group(
                identity.key(),
                CollectionKind::Albums,
                title,
                artist,
                tracks,
            )
        })
        .collect();
    if !tracks.is_empty() {
        album_groups.insert(
            0,
            Group {
                key: "all-songs".into(),
                kind: CollectionKind::Albums,
                title: "All Songs".into(),
                subtitle: format!("{} songs", tracks.len()),
                tracks: Arc::new(tracks),
                artwork: None,
                all_songs: true,
            },
        );
    }
    let artist_groups: Vec<_> = artists
        .into_iter()
        .map(|(title, mut tracks)| {
            tracks.sort_by(artist_track_order);
            let subtitle = format!("{} songs", tracks.len());
            make_group(
                title.clone(),
                CollectionKind::Artists,
                title,
                subtitle,
                tracks,
            )
        })
        .collect();
    (album_groups, artist_groups)
}
fn make_group(
    key: String,
    kind: CollectionKind,
    title: String,
    subtitle: String,
    tracks: Vec<LibraryTrack>,
) -> Group {
    let artwork = tracks.iter().find_map(|track| {
        track
            .artwork
            .clone()
            .map(|art| (track.source_id.clone(), art))
    });
    Group {
        key,
        kind,
        title,
        subtitle,
        tracks: Arc::new(tracks),
        artwork,
        all_songs: false,
    }
}
fn albums_for_artist(albums: &[Group], artist: &Group) -> Vec<Group> {
    albums
        .iter()
        .filter(|album| {
            !album.all_songs
                && album.tracks.iter().any(|track| {
                    track.artist.as_deref() == Some(&artist.title)
                        || track.album_artist.as_deref() == Some(&artist.title)
                })
        })
        .cloned()
        .collect()
}

fn filter_groups(groups: Vec<Group>, query: &str) -> Vec<Group> {
    let query = query.trim().to_lowercase();
    groups
        .into_iter()
        .filter(|group| {
            query.is_empty()
                || (!group.all_songs
                    && format!("{} {}", group.title, group.subtitle)
                        .to_lowercase()
                        .contains(&query))
        })
        .collect()
}

#[derive(Clone)]
struct CollectionPane {
    app: Cadence,
    collections: Collections,
    kind: CollectionKind,
    width: f32,
    height: f32,
}
impl CollectionPane {
    fn body(&self) -> AnyView {
        if let Some(group) = self.collections.detail.get().filter(|_| {
            self.kind == CollectionKind::Albums || self.collections.selected_artist.get().is_some()
        }) {
            return self.detail_view(group);
        }
        if self.kind == CollectionKind::Artists {
            if let Some(artist) = self.collections.selected_artist.get() {
                let back = self.collections.clone();
                let groups = self.collections.artist_albums.clone();
                return AnyView::new(Surface::new(vstack! {
                    back_header(&artist.title, AnyView::new(caption(counted(groups.get().len(),"album")).alignment(Alignment::Trailing).frame(76.,32.)),76.,self.width,move || back.return_to_root()),
                    self.album_grid(groups, self.height - LIBRARY_HEADER_HEIGHT),
                }.spacing(0.), SurfaceRole::Canvas).frame(self.width, self.height));
            }
        }
        let groups = self.collections.results(self.kind).clone();
        let count = groups.get().iter().filter(|group| !group.all_songs).count();
        let label = if self.kind == CollectionKind::Albums {
            "albums"
        } else {
            "artists"
        };
        let content_height = (self.height - LIBRARY_HEADER_HEIGHT).max(120.);
        let content = if groups.get().is_empty() {
            let empty_library = self.collections.albums.get().is_empty();
            let title = if empty_library {
                "Build your music library"
            } else {
                "No matches"
            };
            let detail = if empty_library {
                "Add a music source to browse its albums and artists."
            } else {
                "Try another album or artist name."
            };
            let open = self.app.clone();
            AnyView::new(vstack! {
                Spacer::new(),
                IconView::new(Icon::Music).size(IconSize::Pixels(40)).color(ColorPalette::default().text_secondary()),
                text(title).font_size(18.),
                caption(detail),
                button("Add source").icon(Icon::Plus).on_click(move || { open.workspace.source_page.set(crate::extension::SourcePage::Connect); open.workspace.tab.set(Tab::Sources); }).frame(142., 32.),
                Spacer::new(),
            }.spacing(12.).frame(self.width, content_height))
        } else if self.kind == CollectionKind::Artists {
            self.artist_list(groups, content_height)
        } else {
            self.album_grid(groups, content_height)
        };
        AnyView::new(Surface::new(vstack! {
            library_header(&self.app, if self.kind == CollectionKind::Albums { "Albums" } else { "Artists" },
                crate::ui::counted(count, if self.kind == CollectionKind::Albums { "album" } else { "artist" }), self.collections.query(self.kind).clone(), &format!("Search {label}"), self.width),
            content,
        }.spacing(0.), SurfaceRole::Canvas).frame(self.width, self.height))
    }

    fn album_grid(&self, groups: State<Vec<Group>>, height: f32) -> AnyView {
        let available = (self.width - 40.).max(160.);
        let columns = ((available + 16.) / 176.).floor().max(1.) as usize;
        let side = (available - 16. * columns.saturating_sub(1) as f32) / columns as f32;
        let app = self.app.clone();
        let collections = self.collections.clone();
        AnyView::new(
            GridView::new(
                groups,
                self.collections.gallery_selection.clone(),
                columns,
                side + 50.,
                move |_, group, _| AlbumTile {
                    app: app.clone(),
                    collections: collections.clone(),
                    group,
                    side,
                },
            )
            .item_key(|group| group.key.clone().into())
            .minimum_cell_width(160.)
            .spacing(16.)
            .row_spacing(16.)
            .content_bottom_inset(PLAYER_CLEARANCE)
            .padding_insets(EdgeInsets::new(20., 16., 20., 0.))
            .frame(self.width, height),
        )
    }

    fn artist_list(&self, groups: State<Vec<Group>>, height: f32) -> AnyView {
        let collections = self.collections.clone();
        let width = self.width - 40.;
        AnyView::new(ListView::new(groups, self.collections.gallery_selection.clone(), 56.,
            move |_, group, _| {
                let collections = collections.clone();
                Surface::section(hstack! {
                    IconView::new(Icon::Microphone).size(IconSize::Medium).color(ColorPalette::default().primary()),
                    text(truncate(&group.title, 70)).font_size(14.)
                        .alignment(Alignment::Leading).frame(f32::INFINITY, 44.),
                    caption(counted(group.tracks.len(),"song")),
                    IconView::new(Icon::ChevronRight).size(IconSize::Small).color(ColorPalette::default().text_secondary()),
                }.spacing(12.).padding_insets(EdgeInsets::horizontal(14.))
                    .frame(width, 44.)
                    ).frame(width,44.).padding_insets(EdgeInsets::new(0.,6.,0.,6.))
                    .on_click(move || collections.open_group(group.clone()))
            }).item_key(|group| group.key.clone().into()).content_bottom_inset(PLAYER_CLEARANCE).padding_insets(EdgeInsets::new(20.,16.,20.,0.)).frame(self.width, height))
    }

    fn detail_view(&self, group: Group) -> AnyView {
        let back = self.collections.clone();
        let play_all = self.app.clone();
        let queue_all = self.app.clone();
        let play_tracks = group.tracks.clone();
        let queue_tracks = group.tracks.clone();
        let bulk_actions = AnyView::new(hstack! {
            button("Play all").icon(Icon::PlayerPlay).on_click(move || play_all.workspace.queue_collection(&play_all, &play_tracks, Some(0))).frame(100., 30.),
            button("Add to queue").icon(Icon::Plus).on_click(move || queue_all.workspace.queue_collection(&queue_all, &queue_tracks, None)).frame(126., 30.),
        }.spacing(8.));
        let duration: f64 = group
            .tracks
            .iter()
            .filter_map(|track| track.duration_seconds)
            .filter(|value| value.is_finite() && *value >= 0.)
            .sum();
        let duration = Duration::try_from_secs_f64(duration)
            .map(clock)
            .unwrap_or_default();
        let cover = self
            .collections
            .covers
            .view(group.artwork.clone(), 112., Icon::Music);
        let selected = self.collections.detail_selection.clone();
        let width = self.width;
        let list_height = (self.height - LIBRARY_HEADER_HEIGHT).max(80.);
        let row_tracks = group.tracks.clone();
        let key_tracks = row_tracks.clone();
        let content_height = 164. + group.tracks.len() as f32 * 52.;
        let play_app = self.app.clone();
        let queue_tracks = group.tracks.clone();
        let parent = self
            .collections
            .selected_artist
            .get()
            .map(|artist| artist.title)
            .unwrap_or_else(|| "Albums".into());
        let multi_disc = group
            .tracks
            .iter()
            .map(|track| track.disc_number)
            .collect::<std::collections::BTreeSet<_>>()
            .len()
            > 1;
        AnyView::new(Surface::new(vstack! {
            back_header(&parent,AnyView::new(Spacer::new().frame(0.,32.)),0.,width,move || back.detail.set(None)),
            ScrollView::new(vstack! {
            hstack! {
                cover,
                vstack! {
                    text(truncate(&group.title, ((width - 172.) / 11.).max(12.) as usize)).font_size(22.)
                        .alignment(Alignment::Leading).frame_height(30.),
                    caption(&group.subtitle).alignment(Alignment::Leading).frame_height(20.),
                    caption(format!("{} songs · {duration}", group.tracks.len())).alignment(Alignment::Leading).frame_height(20.),
                    bulk_actions.alignment(Alignment::Leading).frame_height(30.),
                }.spacing(3.).frame(f32::INFINITY, 112.),
            }.spacing(20.).padding_insets(EdgeInsets::new(20.,20.,20.,32.)).frame(width, 164.),
            LazyVStack::new(group.tracks.len(), 52., move |index| {
                    let track = row_tracks[index].clone();
                    let selection = selected.get();
                    let selected = selected.clone();
                    let play = play_app.clone();
                    let add = play_app.clone();
                    let tracks = queue_tracks.clone();
                    let added = track.clone();
                    let duration = track.duration_seconds.and_then(|value| Duration::try_from_secs_f64(value).ok()).map(clock).unwrap_or_else(|| "—".into());
                    hstack! {
                        hstack! {
                            caption(if multi_disc { track.disc_number.map(|disc| format!("{disc} · {}", track.track_number.unwrap_or(0))).unwrap_or_else(|| "—".into()) }
                                else { track.track_number.map(|number| number.to_string()).unwrap_or_else(|| "—".into()) })
                                .alignment(Alignment::Trailing).frame(44., 44.),
                            vstack! {
                                text(truncate(&track.title, ((width - 204.) / 7.2) as usize)).font_size(14.),
                                caption(truncate(track.artist.as_deref().unwrap_or("Unknown artist"), ((width - 204.) / 6.5) as usize)),
                            }.spacing(3.).alignment(Alignment::Leading).frame_width(width - 204.),
                            caption(duration).alignment(Alignment::Trailing).frame(44., 44.),
                        }.spacing(12.).frame(width - 84., 52.).on_click(move || {
                            selected.set(Some(index));
                            play.workspace.queue_collection(&play, &tracks, Some(index));
                        }),
                        Button::icon_only(Icon::Plus).header_style().on_click(move || add.workspace.queue_track(&add, added.clone(), false)).frame(28.,28.),
                    }.spacing(12.).padding_insets(EdgeInsets::horizontal(20.)).frame(width, 52.)
                        .background(if selection == Some(index) { selected_color() } else { ColorPalette::default().background() })
                }).item_key(move |index| key_tracks[index].id.clone().into()).frame(width, content_height - 164.),
            }.spacing(0.).frame(width, content_height)).vertical().content_bottom_inset(PLAYER_CLEARANCE).frame(width, list_height),
        }.spacing(0.), SurfaceRole::Canvas).frame(width, self.height))
    }
}
impl View for CollectionPane {
    fn create_element(&self) -> Box<dyn Element> {
        Box::new(scarlet_ui::ComponentElement::new_with_builder(
            self.clone(),
            |pane| Box::new(pane.body()),
        ))
    }
    fn listenables(&self) -> Vec<&dyn Listenable> {
        vec![
            &self.collections.detail,
            &self.collections.detail_selection,
            &self.collections.selected_artist,
            &self.collections.artist_albums,
            self.collections.results(self.kind),
        ]
    }
    fn as_any(&self) -> &dyn Any {
        self
    }
}

#[derive(Clone)]
struct AlbumTile {
    app: Cadence,
    collections: Collections,
    group: Group,
    side: f32,
}
impl AlbumTile {
    fn body(&self) -> AnyView {
        let app = self.app.clone();
        let collections = self.collections.clone();
        let group = self.group.clone();
        let cover = if group.all_songs {
            AnyView::new(Surface::section(zstack! {
                Image::from_bitmap(self.collections.all_songs_art.clone()).fit_mode(ImageFit::Cover).frame(self.side, self.side),
                IconView::new(Icon::Music).size(IconSize::Pixels(44)).color(Color::WHITE)
                    .frame(44., 44.).alignment(Alignment::Center).frame(self.side, self.side),
            }).corner_radius(10.).frame(self.side, self.side))
        } else {
            self.collections
                .covers
                .view(group.artwork.clone(), self.side, Icon::Music)
        };
        AnyView::new(vstack! {
            cover,
            text(truncate(&group.title, (self.side / 7.3).max(10.) as usize)).font_size(13.)
                .alignment(Alignment::Leading).frame(self.side, 19.),
            caption(truncate(&group.subtitle, (self.side / 7.).max(10.) as usize)).font_size(11.)
                .alignment(Alignment::Leading).frame(self.side, 17.),
        }.spacing(5.).frame(self.side, self.side + 50.)
            .background(ColorPalette::default().background())
            .on_click(move || {
                if group.all_songs { app.workspace.tab.set(Tab::Library); }
                else { collections.open_group(group.clone()); }
            }))
    }
}
impl View for AlbumTile {
    fn create_element(&self) -> Box<dyn Element> {
        Box::new(scarlet_ui::ComponentElement::new_with_builder(
            self.clone(),
            |tile| Box::new(tile.body()),
        ))
    }
    fn listenables(&self) -> Vec<&dyn Listenable> {
        vec![]
    }
    fn as_any(&self) -> &dyn Any {
        self
    }
}

#[derive(Clone)]
struct CoverSlot {
    image: State<Option<BitmapImage>>,
    requested: Rc<Cell<bool>>,
    pins: Arc<AtomicUsize>,
    cancel: Rc<RefCell<Arc<AtomicBool>>>,
}

// A mounted cover owns one lease regardless of view/element cloning. Eviction
// can release retained offscreen images without invalidating visible covers.
struct CoverLease {
    pins: Arc<AtomicUsize>,
    cancel: Rc<RefCell<Arc<AtomicBool>>>,
}
impl CoverLease {
    fn new(slot: &CoverSlot) -> Self {
        slot.pins.fetch_add(1, Ordering::Relaxed);
        Self {
            pins: slot.pins.clone(),
            cancel: slot.cancel.clone(),
        }
    }
}
impl Drop for CoverLease {
    fn drop(&mut self) {
        if self.pins.fetch_sub(1, Ordering::Relaxed) == 1 {
            self.cancel.borrow().store(true, Ordering::Relaxed);
        }
    }
}
struct CoverRequest {
    key: String,
    generation: u32,
    context: u64,
    cancel: Arc<AtomicBool>,
    context_cancel: Arc<AtomicBool>,
    pins: Arc<AtomicUsize>,
    artwork: ArtworkRef,
    config: Option<SourceConfig>,
    cache: PathBuf,
}
impl CoverRequest {
    fn abandoned(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)
            || self.context_cancel.load(Ordering::Relaxed)
            || self.pins.load(Ordering::Relaxed) == 0
    }
}
struct CoverResult {
    key: String,
    generation: u32,
    context: u64,
    cancel: Arc<AtomicBool>,
    bitmap: Option<BitmapImage>,
}
struct CacheInner {
    slots: RefCell<HashMap<String, CoverSlot>>,
    slot_order: RefCell<VecDeque<String>>,
    pending: RefCell<VecDeque<CoverRequest>>,
    requests: mpsc::SyncSender<CoverRequest>,
    results: RefCell<mpsc::Receiver<CoverResult>>,
    resident: RefCell<VecDeque<(String, usize)>>,
    bytes: Cell<usize>,
    cancel: Arc<AtomicBool>,
    context: Cell<u64>,
    context_cancel: RefCell<Arc<AtomicBool>>,
    sources: RefCell<Vec<SourceConfig>>,
    directory: RefCell<PathBuf>,
}
impl Drop for CacheInner {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Relaxed);
        self.context_cancel.borrow().store(true, Ordering::Relaxed);
        for slot in self.slots.get_mut().values() {
            slot.cancel.borrow().store(true, Ordering::Relaxed);
        }
    }
}
#[derive(Clone)]
struct CoverCache(Rc<CacheInner>);
impl CoverCache {
    fn new() -> Self {
        let (send, requests) = mpsc::sync_channel::<CoverRequest>(8);
        let (results, receive) = mpsc::sync_channel::<CoverResult>(8);
        let cancel = Arc::new(AtomicBool::new(false));
        let worker_cancel = cancel.clone();
        let _ = std::thread::Builder::new()
            .name("cadence-covers".into())
            .spawn(move || {
                while let Ok(request) = requests.recv() {
                    if worker_cancel.load(Ordering::Relaxed) {
                        break;
                    }
                    if request.abandoned() {
                        continue;
                    }
                    let data = match &request.artwork {
                        ArtworkRef::File(_) | ArtworkRef::Embedded { .. } => {
                            cadence_library::read_artwork(
                                &request.artwork,
                                cadence_library::MAX_ARTWORK_BYTES,
                            )
                        }
                        _ => request
                            .config
                            .as_ref()
                            .ok_or_else(|| "Artwork source is unavailable".to_owned())
                            .and_then(|config| {
                                SourceRegistry::builtin().fetch_artwork(
                                    config,
                                    &request.artwork,
                                    &request.cache,
                                    &request.cancel,
                                )
                            }),
                    };
                    // Reading embedded artwork can take longer than scrolling
                    // past a tile. Do not decode it if its last lease is gone.
                    if request.abandoned() {
                        continue;
                    }
                    let bitmap = data
                        .and_then(|data| cadence_library::decode_artwork(&data, COVER_EDGE))
                        .ok()
                        .map(|bitmap| {
                            // Vec -> Arc storage also allocates and copies; keep
                            // that work beside decoding, away from the UI thread.
                            BitmapImage::from_bgra(bitmap.pixels, bitmap.width, bitmap.height)
                        });
                    if worker_cancel.load(Ordering::Relaxed) {
                        break;
                    }
                    if request.abandoned() {
                        continue;
                    }
                    if results
                        .send(CoverResult {
                            key: request.key,
                            generation: request.generation,
                            context: request.context,
                            cancel: request.cancel,
                            bitmap,
                        })
                        .is_err()
                    {
                        break;
                    }
                }
            });
        Self(Rc::new(CacheInner {
            slots: RefCell::new(HashMap::new()),
            slot_order: RefCell::new(VecDeque::new()),
            pending: RefCell::new(VecDeque::new()),
            requests: send,
            results: RefCell::new(receive),
            resident: RefCell::new(VecDeque::new()),
            bytes: Cell::new(0),
            cancel,
            context: Cell::new(0),
            context_cancel: RefCell::new(Arc::new(AtomicBool::new(false))),
            sources: RefCell::new(vec![]),
            directory: RefCell::new(PathBuf::new()),
        }))
    }
    fn set_context(&self, sources: Vec<SourceConfig>, directory: PathBuf) {
        if *self.0.sources.borrow() == sources && *self.0.directory.borrow() == directory {
            return;
        }
        *self.0.sources.borrow_mut() = sources;
        *self.0.directory.borrow_mut() = directory;
        self.0.context.set(self.0.context.get().wrapping_add(1));
        self.0
            .context_cancel
            .borrow()
            .store(true, Ordering::Relaxed);
        *self.0.context_cancel.borrow_mut() = Arc::new(AtomicBool::new(false));
        self.0.pending.borrow_mut().clear();
        // A failed/unavailable source is retryable after reconnecting. Notify
        // only on an actual source/cache-context change, never on an idle poll.
        for slot in self.0.slots.borrow().values() {
            if slot.image.get().is_none() {
                slot.cancel.borrow().store(true, Ordering::Relaxed);
                slot.requested.set(false);
                slot.image.set(None);
            }
        }
    }
    fn view(&self, artwork: Option<(String, ArtworkRef)>, side: f32, fallback: Icon) -> AnyView {
        let key = artwork.as_ref().map(|(source, art)| {
            format!(
                "{source}:{}",
                serde_json::to_string(art).unwrap_or_default()
            )
        });
        let (image, lease) = if let Some(key) = &key {
            let slot = self.touch_slot(key);
            let lease = Rc::new(CoverLease::new(&slot));
            self.trim_slots();
            (slot.image, Some(lease))
        } else {
            (state(None), None)
        };
        // The boundary surrounds the component that subscribes to image state.
        // Placing it only inside body() would leave that subscriber outside it
        // and route each completed cover to the gallery's much larger cache.
        AnyView::new(
            CoverView {
                cache: self.clone(),
                key,
                artwork,
                image,
                side,
                fallback,
                _lease: lease,
            }
            .repaint_boundary(),
        )
    }
    fn touch_slot(&self, key: &str) -> CoverSlot {
        let mut order = self.0.slot_order.borrow_mut();
        if let Some(index) = order.iter().position(|entry| entry == key) {
            order.remove(index);
        }
        order.push_back(key.to_owned());
        self.0
            .slots
            .borrow_mut()
            .entry(key.to_owned())
            .or_insert_with(|| CoverSlot {
                image: state(None),
                requested: Rc::new(Cell::new(false)),
                pins: Arc::new(AtomicUsize::new(0)),
                cancel: Rc::new(RefCell::new(Arc::new(AtomicBool::new(false)))),
            })
            .clone()
    }
    fn trim_slots(&self) {
        let mut order = self.0.slot_order.borrow_mut();
        while order.len() > COVER_SLOTS {
            let candidate = order.iter().position(|key| {
                self.0
                    .slots
                    .borrow()
                    .get(key)
                    .is_none_or(|slot| slot.pins.load(Ordering::Relaxed) == 0)
            });
            let Some(index) = candidate else {
                break;
            };
            let Some(old) = order.remove(index) else {
                break;
            };
            if let Some(slot) = self.0.slots.borrow_mut().remove(&old) {
                slot.cancel.borrow().store(true, Ordering::Relaxed);
                slot.image.set(None);
            }
            let mut resident = self.0.resident.borrow_mut();
            if let Some(index) = resident.iter().position(|(entry, _)| entry == &old)
                && let Some((_, bytes)) = resident.remove(index)
            {
                self.0.bytes.set(self.0.bytes.get().saturating_sub(bytes));
            }
        }
    }
    fn trim_resident_to(&self, budget: usize) {
        // Only retained, unmounted covers are candidates. Mounted GridView
        // cells (including its finite overscan) keep their decoded image.
        while self.0.bytes.get() > budget {
            let candidate = self.0.resident.borrow().iter().position(|(key, _)| {
                self.0
                    .slots
                    .borrow()
                    .get(key)
                    .is_none_or(|slot| slot.pins.load(Ordering::Relaxed) == 0)
            });
            let Some(index) = candidate else {
                break;
            };
            let Some((key, bytes)) = self.0.resident.borrow_mut().remove(index) else {
                break;
            };
            self.0.bytes.set(self.0.bytes.get().saturating_sub(bytes));
            if let Some(slot) = self.0.slots.borrow().get(&key) {
                slot.requested.set(false);
                slot.image.set(None);
            }
        }
    }
    fn request(&self, key: &str, source: &str, artwork: &ArtworkRef) {
        let Some(slot) = self.0.slots.borrow().get(key).cloned() else {
            return;
        };
        if slot.image.get().is_some() {
            let mut resident = self.0.resident.borrow_mut();
            if let Some(index) = resident.iter().position(|(entry, _)| entry == key)
                && let Some(entry) = resident.remove(index)
            {
                resident.push_back(entry);
            }
            return;
        }
        if slot.cancel.borrow().load(Ordering::Relaxed) {
            *slot.cancel.borrow_mut() = Arc::new(AtomicBool::new(false));
            slot.requested.set(false);
        }
        if slot.requested.replace(true) {
            return;
        }
        let config = self
            .0
            .sources
            .borrow()
            .iter()
            .find(|config| config.id == source)
            .cloned();
        let mut pending = self.0.pending.borrow_mut();
        if pending.len() >= PENDING_COVERS
            && let Some(old) = pending.pop_front()
            && let Some(old_slot) = self.0.slots.borrow().get(&old.key)
            && old_slot.image.id().get() == old.generation
            && Arc::ptr_eq(&old_slot.cancel.borrow(), &old.cancel)
        {
            old.cancel.store(true, Ordering::Relaxed);
            old_slot.requested.set(false);
        }
        pending.push_back(CoverRequest {
            key: key.into(),
            generation: slot.image.id().get(),
            context: self.0.context.get(),
            cancel: slot.cancel.borrow().clone(),
            context_cancel: self.0.context_cancel.borrow().clone(),
            pins: slot.pins.clone(),
            artwork: artwork.clone(),
            config,
            cache: self.0.directory.borrow().clone(),
        });
    }
    fn poll(&self) {
        for _ in 0..COVERS_PER_POLL {
            let Ok(result) = self.0.results.borrow().try_recv() else {
                break;
            };
            if let Some(bitmap) = result.bitmap {
                let bytes = std::mem::size_of_val(bitmap.pixels());
                if let Some(slot) = self.0.slots.borrow().get(&result.key).cloned()
                    && slot.image.id().get() == result.generation
                    && self.0.context.get() == result.context
                    && !result.cancel.load(Ordering::Relaxed)
                    && Arc::ptr_eq(&slot.cancel.borrow(), &result.cancel)
                {
                    slot.image.set(Some(bitmap));
                    self.0.bytes.set(self.0.bytes.get() + bytes);
                    self.0.resident.borrow_mut().push_back((result.key, bytes));
                }
            }
        }
        self.trim_resident_to(COVER_BUDGET);
        self.trim_slots();
        loop {
            let Some(request) = self.0.pending.borrow_mut().pop_front() else {
                break;
            };
            if request.abandoned() {
                if let Some(slot) = self.0.slots.borrow().get(&request.key)
                    && slot.image.id().get() == request.generation
                    && Arc::ptr_eq(&slot.cancel.borrow(), &request.cancel)
                {
                    slot.requested.set(false);
                }
                continue;
            }
            match self.0.requests.try_send(request) {
                Ok(()) => {}
                Err(mpsc::TrySendError::Full(request)) => {
                    self.0.pending.borrow_mut().push_front(request);
                    break;
                }
                Err(mpsc::TrySendError::Disconnected(_)) => {
                    self.0.pending.borrow_mut().clear();
                    break;
                }
            }
        }
    }
}

#[derive(Clone)]
struct CoverView {
    cache: CoverCache,
    key: Option<String>,
    artwork: Option<(String, ArtworkRef)>,
    image: State<Option<BitmapImage>>,
    side: f32,
    fallback: Icon,
    _lease: Option<Rc<CoverLease>>,
}
impl CoverView {
    fn body(&self) -> AnyView {
        if let (Some(key), Some((source, artwork))) = (&self.key, &self.artwork) {
            self.cache.request(key, source, artwork);
        }
        let content = if let Some(bitmap) = self.image.get() {
            AnyView::new(
                Image::from_bitmap(bitmap)
                    .fit_mode(ImageFit::Cover)
                    .frame(self.side, self.side),
            )
        } else {
            AnyView::new(
                IconView::new(self.fallback)
                    .size(IconSize::Pixels((self.side * 0.24).max(24.) as u16))
                    .color(ColorPalette::default().text_secondary())
                    .frame((self.side * 0.24).max(24.), (self.side * 0.24).max(24.))
                    .alignment(Alignment::Center)
                    .frame(self.side, self.side),
            )
        };
        AnyView::new(
            Surface::section(content)
                .corner_radius(10.)
                .frame(self.side, self.side),
        )
    }
}
impl View for CoverView {
    fn create_element(&self) -> Box<dyn Element> {
        Box::new(scarlet_ui::ComponentElement::new_with_builder(
            self.clone(),
            |cover| Box::new(cover.body()),
        ))
    }
    fn listenables(&self) -> Vec<&dyn Listenable> {
        vec![&self.image]
    }
    fn as_any(&self) -> &dyn Any {
        self
    }
}

// The All Songs illustration is an app asset, composed with ScarletUI Image
// and IconView. It is generated once, never decoded during a frame.
fn all_songs_artwork() -> BitmapImage {
    let side = 128u32;
    let mut pixels = Vec::with_capacity((side * side) as usize);
    for y in 0..side {
        for x in 0..side {
            let t = (x + y) as f32 / (2 * (side - 1)) as f32;
            let (start, end, amount): ([f32; 3], [f32; 3], f32) = if t < 0.5 {
                ([198., 52., 219.], [255., 45., 94.], t * 2.)
            } else {
                ([255., 45., 94.], [255., 143., 43.], (t - 0.5) * 2.)
            };
            let component = |index| (start[index] + (end[index] - start[index]) * amount) as u32;
            pixels.push(0xff00_0000 | component(0) << 16 | component(1) << 8 | component(2));
        }
    }
    BitmapImage::from_bgra(pixels, side, side)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    fn song(id: &str, album: &str, artist: &str) -> LibraryTrack {
        LibraryTrack {
            id: id.into(),
            source_id: "local".into(),
            item_id: id.into(),
            title: id.into(),
            artist: Some(artist.into()),
            album: Some(album.into()),
            album_id: None,
            album_artist: None,
            track_number: None,
            disc_number: None,
            duration_seconds: Some(60.),
            location: MediaLocation::Local(PathBuf::from(format!(
                "/test/{artist}/{album}/{id}.flac"
            ))),
            artwork: None,
        }
    }

    #[test]
    fn groups_keep_same_title_albums_by_different_artists_distinct() {
        let mut first = song("one", "Night", "Alice");
        first.artwork = Some(ArtworkRef::File(PathBuf::from("/test/cover.png")));
        let second = song("two", "Night", "Alice");
        let third = song("three", "Night", "Bob");
        let (albums, artists) = group_library(vec![first, second, third]);
        assert_eq!(albums.len(), 3);
        assert!(albums[0].all_songs);
        assert_eq!(albums[0].tracks.len(), 3);
        assert_eq!(albums[1].tracks.len(), 2);
        assert_eq!(albums[2].tracks.len(), 1);
        assert_ne!(albums[1].key, albums[2].key);
        assert!(albums[1].artwork.is_some());
        assert_eq!(artists.len(), 2);
    }

    #[test]
    fn directory_keeps_compilation_tracks_together_without_new_metadata() {
        let mut first = song("one", "Theme songs", "北宇治高校吹奏楽部");
        let mut second = song("two", "Theme songs", "北宇治高校吹奏楽部, TRUE");
        first.location = MediaLocation::Local("/music/Euphonium/01.flac".into());
        second.location = MediaLocation::Local("/music/Euphonium/02.flac".into());
        let (albums, artists) = group_library(vec![first, second]);
        assert_eq!(albums.len(), 2);
        assert_eq!(albums[1].tracks.len(), 2);
        assert_eq!(albums[1].subtitle, "北宇治高校吹奏楽部");
        assert_eq!(artists.len(), 2);
        assert_eq!(artists[1].title, "北宇治高校吹奏楽部, TRUE");
    }

    #[test]
    fn directory_identity_and_order_ignore_album_and_artist_tags() {
        let mut first = song("one", "Zulu", "Same artist");
        let mut second = song("two", "Zulu", "Same artist");
        let mut third = song("three", "Alpha", "Different artist");
        first.location = MediaLocation::Local("/music/A/01.flac".into());
        second.location = MediaLocation::Local("/music/B/01.flac".into());
        third.location = MediaLocation::Local("/music/A/02.flac".into());
        let (albums, _) = group_library(vec![second, first, third]);
        assert_eq!(albums.len(), 3);
        assert_eq!(albums[1].title, "Alpha");
        assert_eq!(albums[1].tracks.len(), 2);
        assert_eq!(albums[2].title, "Zulu");
        assert_ne!(albums[1].key, albums[2].key);
    }

    #[test]
    fn disc_subdirectories_are_separate_albums_like_kanade() {
        let mut first = song("one", "Same release", "Artist");
        let mut second = song("two", "Same release", "Artist");
        first.location = MediaLocation::Local("/music/Release/CD1/01.flac".into());
        second.location = MediaLocation::Local("/music/Release/CD2/01.flac".into());
        let (albums, _) = group_library(vec![second, first]);
        assert_eq!(albums.len(), 3);
        assert_eq!(albums[1].tracks[0].id, "one");
        assert_eq!(albums[2].tracks[0].id, "two");
    }

    #[test]
    fn directory_sort_matches_sqlite_text_not_path_component_order() {
        let mut nested = song("nested", "Release", "Artist");
        let mut edition = song("edition", "Release", "Artist");
        nested.location = MediaLocation::Local("/music/A/Disc/01.flac".into());
        edition.location = MediaLocation::Local("/music/A-Edition/01.flac".into());
        let (albums, _) = group_library(vec![nested, edition]);
        assert_eq!(albums[1].tracks[0].id, "edition");
        assert_eq!(albums[2].tracks[0].id, "nested");
    }

    #[test]
    fn album_artist_is_literal_and_also_part_of_artist_union() {
        let mut first = song("one", "Release", "Soloist");
        first.album_artist = Some(" Ensemble ".into());
        let mut second = song("two", "Release", " Ensemble ");
        second.album_artist = Some(" Ensemble ".into());
        second.location = first.location.clone();
        let mut unknown = song("three", "Other", "ignored");
        unknown.artist = None;
        unknown.album_artist = None;
        let (albums, artists) = group_library(vec![first, second, unknown]);
        assert_eq!(albums[1].subtitle, " Ensemble ");
        assert_eq!(artists.len(), 2);
        assert_eq!(artists[0].title, " Ensemble ");
        assert_eq!(artists[0].tracks.len(), 2);
        assert_eq!(artists[1].title, "Soloist");
        assert_eq!(artists[1].tracks.len(), 1);
    }

    #[test]
    fn album_details_follow_disc_track_title_with_missing_numbers_first() {
        let mut tracks: Vec<_> = ["Zulu", "Beta", "Alpha", "Disc two", "Unnumbered"]
            .into_iter()
            .map(|title| song(title, "Release", "Artist"))
            .collect();
        tracks[0].disc_number = Some(1);
        tracks[0].track_number = Some(2);
        tracks[1].disc_number = Some(1);
        tracks[1].track_number = Some(1);
        tracks[2].disc_number = Some(1);
        tracks[2].track_number = Some(1);
        tracks[3].disc_number = Some(2);
        tracks[3].track_number = Some(1);
        tracks[4].track_number = Some(99);
        let (albums, _) = group_library(tracks);
        assert_eq!(
            albums[1]
                .tracks
                .iter()
                .map(|track| track.title.as_str())
                .collect::<Vec<_>>(),
            ["Unnumbered", "Alpha", "Beta", "Zulu", "Disc two"]
        );
    }

    #[test]
    fn artist_details_use_album_then_disc_track_title_order() {
        let mut first = song("later", "Zulu", "Artist");
        first.track_number = Some(1);
        let mut second = song("second", "Alpha", "Artist");
        second.track_number = Some(2);
        let mut third = song("first", "Alpha", "Artist");
        third.track_number = Some(1);
        let (_, artists) = group_library(vec![first, second, third]);
        assert_eq!(
            artists[0]
                .tracks
                .iter()
                .map(|track| track.id.as_str())
                .collect::<Vec<_>>(),
            ["first", "second", "later"]
        );
    }

    #[test]
    fn missing_album_title_uses_untitled_and_empty_tags_stay_literal() {
        let mut missing = song("one", "ignored", "ignored");
        missing.album = None;
        missing.artist = None;
        missing.album_artist = None;
        missing.location = MediaLocation::Local("/music/A/01.flac".into());
        let mut empty = song("two", "", "");
        empty.location = MediaLocation::Local("/music/B/01.flac".into());
        let (albums, artists) = group_library(vec![missing, empty]);
        assert_eq!(albums[1].title, "Untitled");
        assert_eq!(albums[1].subtitle, "");
        assert_eq!(albums[2].title, "");
        assert_eq!(artists.len(), 1);
        assert_eq!(artists[0].title, "");
    }

    #[test]
    fn provider_albums_use_opaque_ids_with_source_namespaces() {
        let mut first = song("one", "Release", "Soloist one");
        first.location = MediaLocation::Provider;
        first.source_id = "server A".into();
        first.album_id = Some("opaque album".into());
        first.album_artist = Some("Ensemble".into());
        let mut second = first.clone();
        second.id = "two".into();
        second.item_id = "two".into();
        second.artist = Some("Soloist two".into());
        let mut other_server = first.clone();
        other_server.id = "three".into();
        other_server.item_id = "three".into();
        other_server.source_id = "server B".into();
        let (albums, _) = group_library(vec![first, second, other_server]);
        assert_eq!(albums.len(), 3);
        assert_eq!(albums[1].tracks.len(), 2);
        assert_eq!(albums[2].tracks.len(), 1);
        assert_ne!(albums[1].key, albums[2].key);
    }

    #[test]
    fn unidentified_provider_items_do_not_guess_a_shared_album() {
        let mut first = song("one", "Same title", "Same artist");
        first.location = MediaLocation::Provider;
        let mut second = song("two", "Same title", "Same artist");
        second.location = MediaLocation::Provider;
        let (albums, _) = group_library(vec![first, second]);
        assert_eq!(albums.len(), 3);
        assert_ne!(albums[1].key, albums[2].key);
    }

    #[test]
    fn idle_polls_do_not_notify_collection_views() {
        let workspace = Workspace::memory();
        workspace.library.set(vec![song("one", "Night", "Alice")]);
        let collections = Collections::new();
        collections.poll(&workspace);
        let calls = Arc::new(AtomicUsize::new(0));
        let album_calls = calls.clone();
        let artist_calls = calls.clone();
        collections.album_results.subscribe_any(Arc::new(move || {
            album_calls.fetch_add(1, Ordering::Relaxed);
        }));
        collections.artist_results.subscribe_any(Arc::new(move || {
            artist_calls.fetch_add(1, Ordering::Relaxed);
        }));
        for _ in 0..100 {
            collections.poll(&workspace);
        }
        assert_eq!(calls.load(Ordering::Relaxed), 0);
        workspace.library.set(vec![
            song("one", "Night", "Alice"),
            song("two", "Dawn", "Bob"),
        ]);
        collections.poll(&workspace);
        assert_eq!(calls.load(Ordering::Relaxed), 2);
        assert_eq!(collections.album_results.get().len(), 3);
    }

    #[test]
    fn album_search_does_not_invalidate_artist_results() {
        let workspace = Workspace::memory();
        workspace.library.set(vec![
            song("one", "Night", "Alice"),
            song("two", "Dawn", "Bob"),
        ]);
        let collections = Collections::new();
        collections.poll(&workspace);
        let calls = Arc::new(AtomicUsize::new(0));
        let artist_calls = calls.clone();
        collections.artist_results.subscribe_any(Arc::new(move || {
            artist_calls.fetch_add(1, Ordering::Relaxed);
        }));
        collections.album_query.set(" aLIce ".into());
        collections.poll(&workspace);
        assert_eq!(collections.album_results.get().len(), 1);
        assert_eq!(collections.album_results.get()[0].title, "Night");
        assert_eq!(collections.artist_results.get().len(), 2);
        assert_eq!(calls.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn removing_the_open_album_returns_to_the_collection() {
        let workspace = Workspace::memory();
        workspace.library.set(vec![song("one", "Night", "Alice")]);
        let collections = Collections::new();
        collections.poll(&workspace);
        collections.open_group(collections.albums.get()[1].clone());
        collections.detail_selection.set(Some(0));
        workspace.library.set(vec![]);
        collections.poll(&workspace);
        assert!(collections.detail.get().is_none());
        assert!(collections.detail_tracks.get().is_empty());
        assert_eq!(collections.detail_selection.get(), None);
    }

    #[test]
    fn artist_albums_include_full_compilations_and_update_without_losing_parent() {
        let workspace = Workspace::memory();
        let mut first = song("one", "Compilation", "Alice");
        first.location = MediaLocation::Local(PathBuf::from("/compilation/one.wav"));
        let mut second = song("two", "Compilation", "Bob");
        second.location = MediaLocation::Local(PathBuf::from("/compilation/two.wav"));
        workspace.library.set(vec![first.clone(), second.clone()]);
        let collections = Collections::new();
        collections.poll(&workspace);
        collections.open_group(collections.artists.get()[0].clone());
        assert!(collections.detail.get().is_none());
        assert_eq!(collections.artist_albums.get().len(), 1);
        assert_eq!(collections.artist_albums.get()[0].tracks.len(), 2);
        collections.open_group(collections.artist_albums.get()[0].clone());
        assert_eq!(collections.detail_tracks.get().len(), 2);
        workspace.library.set(vec![first]);
        collections.poll(&workspace);
        assert_eq!(collections.detail_tracks.get().len(), 1);
        assert_eq!(collections.selected_artist.get().unwrap().title, "Alice");
        collections.detail.set(None);
        assert_eq!(collections.artist_albums.get().len(), 1);
        workspace.library.set(vec![]);
        collections.poll(&workspace);
        assert!(collections.selected_artist.get().is_none());
        assert!(collections.artist_albums.get().is_empty());
    }

    #[test]
    fn failed_cover_retries_only_after_its_context_changes() {
        let cache = CoverCache::new();
        let directory = PathBuf::from("/test/cache");
        cache.set_context(vec![], directory.clone());
        let slot = cache.touch_slot("remote-cover");
        slot.requested.set(true);
        let notifications = Arc::new(AtomicUsize::new(0));
        let calls = notifications.clone();
        slot.image.subscribe_any(Arc::new(move || {
            calls.fetch_add(1, Ordering::Relaxed);
        }));
        for _ in 0..10 {
            cache.set_context(vec![], directory.clone());
        }
        assert!(slot.requested.get());
        assert_eq!(notifications.load(Ordering::Relaxed), 0);
        let old_cancel = cache.0.context_cancel.borrow().clone();
        let source = SourceConfig {
            id: "server".into(),
            name: "Server".into(),
            plugin_id: "kanade".into(),
            settings: serde_json::json!({"url":"http://unused.invalid"}),
        };
        cache.set_context(vec![source.clone()], directory.clone());
        assert!(!slot.requested.get());
        assert_eq!(notifications.load(Ordering::Relaxed), 1);
        assert!(old_cancel.load(Ordering::Relaxed));
        for _ in 0..10 {
            cache.set_context(vec![source.clone()], directory.clone());
        }
        assert_eq!(notifications.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn visible_cover_lease_prevents_eviction_and_decode_feedback() {
        let cache = CoverCache::new();
        let visible = cache.touch_slot("visible");
        let offscreen = cache.touch_slot("offscreen");
        visible
            .image
            .set(Some(BitmapImage::from_bgra(vec![0xff000000], 1, 1)));
        offscreen
            .image
            .set(Some(BitmapImage::from_bgra(vec![0xff000000], 1, 1)));
        visible.requested.set(true);
        offscreen.requested.set(true);
        cache.0.bytes.set(8);
        cache
            .0
            .resident
            .borrow_mut()
            .extend([("visible".into(), 4), ("offscreen".into(), 4)]);
        let lease = Rc::new(CoverLease::new(&visible));
        let element_clone = lease.clone();
        let notifications = Arc::new(AtomicUsize::new(0));
        let calls = notifications.clone();
        visible.image.subscribe_any(Arc::new(move || {
            calls.fetch_add(1, Ordering::Relaxed);
        }));
        for _ in 0..10 {
            cache.trim_resident_to(0);
        }
        assert!(visible.image.get().is_some());
        assert!(offscreen.image.get().is_none());
        assert_eq!(cache.0.bytes.get(), 4);
        assert_eq!(notifications.load(Ordering::Relaxed), 0);
        assert!(cache.0.pending.borrow().is_empty());
        drop(lease);
        cache.trim_resident_to(0);
        assert!(visible.image.get().is_some());
        drop(element_clone);
        cache.trim_resident_to(0);
        assert!(visible.image.get().is_none());
        assert_eq!(cache.0.bytes.get(), 0);
    }

    #[test]
    fn visiting_many_covers_bounds_metadata_and_pending_io() {
        let cache = CoverCache::new();
        for index in 0..1_000 {
            let key = format!("cover-{index}");
            cache.touch_slot(&key);
            cache.trim_slots();
            cache.request(
                &key,
                "local",
                &ArtworkRef::File(PathBuf::from(format!("/test/{index}.png"))),
            );
        }
        assert_eq!(cache.0.slots.borrow().len(), COVER_SLOTS);
        assert_eq!(cache.0.pending.borrow().len(), PENDING_COVERS);
        assert_eq!(cache.0.bytes.get(), 0);
    }

    #[test]
    fn scrolling_away_cancels_pending_and_running_cover_requests() {
        let cache = CoverCache::new();
        let slot = cache.touch_slot("first");
        let lease = Rc::new(CoverLease::new(&slot));
        cache.request("first", "local", &ArtworkRef::File("/unused.png".into()));
        let old_cancel = cache.0.pending.borrow()[0].cancel.clone();
        let element_clone = lease.clone();
        drop(lease);
        assert!(!old_cancel.load(Ordering::Relaxed));
        drop(element_clone);
        assert!(old_cancel.load(Ordering::Relaxed));
        cache.poll();
        assert!(cache.0.pending.borrow().is_empty());
        assert!(!slot.requested.get());

        // Remounting a cover permits a fresh request without reviving a canceled
        // provider operation or waiting for an abandoned image to finish.
        let _new_lease = CoverLease::new(&slot);
        cache.request("first", "local", &ArtworkRef::File("/unused.png".into()));
        let pending = cache.0.pending.borrow();
        assert_eq!(pending.len(), 1);
        assert!(!pending[0].abandoned());
        assert!(!Arc::ptr_eq(&old_cancel, &pending[0].cancel));
    }

    #[test]
    fn completed_covers_have_a_bounded_ui_publish_batch() {
        let cache = CoverCache::new();
        let (send, results) = mpsc::channel();
        *cache.0.results.borrow_mut() = results;
        let mut leases = vec![];
        for index in 0..COVERS_PER_POLL + 2 {
            let key = format!("cover-{index}");
            let slot = cache.touch_slot(&key);
            leases.push(CoverLease::new(&slot));
            assert!(
                send.send(CoverResult {
                    key,
                    generation: slot.image.id().get(),
                    context: cache.0.context.get(),
                    cancel: slot.cancel.borrow().clone(),
                    bitmap: Some(BitmapImage::from_bgra(vec![0xff12_3456], 1, 1)),
                })
                .is_ok()
            );
        }
        cache.poll();
        assert_eq!(
            cache
                .0
                .slots
                .borrow()
                .values()
                .filter(|slot| slot.image.get().is_some())
                .count(),
            COVERS_PER_POLL
        );
        assert_eq!(cache.0.bytes.get(), COVERS_PER_POLL * 4);
        cache.poll();
        assert_eq!(cache.0.bytes.get(), (COVERS_PER_POLL + 2) * 4);
    }

    #[test]
    fn late_canceled_cover_cannot_overwrite_its_replacement() {
        let cache = CoverCache::new();
        let (send, results) = mpsc::channel();
        *cache.0.results.borrow_mut() = results;
        let slot = cache.touch_slot("cover");
        let lease = CoverLease::new(&slot);
        let old_cancel = slot.cancel.borrow().clone();
        drop(lease);
        let _replacement = CoverLease::new(&slot);
        cache.request("cover", "local", &ArtworkRef::File("/unused.png".into()));
        assert!(
            send.send(CoverResult {
                key: "cover".into(),
                generation: slot.image.id().get(),
                context: cache.0.context.get(),
                cancel: old_cancel,
                bitmap: Some(BitmapImage::from_bgra(vec![0xff12_3456], 1, 1)),
            })
            .is_ok()
        );
        cache.poll();
        assert!(slot.image.get().is_none());
        assert_eq!(cache.0.bytes.get(), 0);
    }

    type RasterChunks = Rc<RefCell<Vec<(Rect, Arc<scarlet_ui::buffer::Buffer>)>>>;
    struct CoverRasterBackend {
        cpu: scarlet_ui::renderer::CpuPaintBackend,
        chunks: RasterChunks,
    }
    impl scarlet_ui::renderer::PaintBackend for CoverRasterBackend {
        fn uses_cpu_picture_cache(&self) -> bool {
            true
        }
        fn resize(&mut self, size: Size, scale_milli: u32) {
            scarlet_ui::renderer::PaintBackend::resize(&mut self.cpu, size, scale_milli);
        }
        fn render<'a>(
            &'a mut self,
            context: &scarlet_ui::renderer::PaintContext<'_>,
            background: Color,
            logical_damage: Option<&[Rect]>,
            physical_damage: Option<&[scarlet_ui::compositor::DamageRect]>,
        ) -> scarlet_ui::error::Result<scarlet_ui::renderer::BackendFrame<'a>> {
            use scarlet_ui::renderer::{PaintBuffer, PaintCommand};
            for command in context.commands() {
                let (dst, index) = match command {
                    PaintCommand::DrawBuffer { dst, buffer_idx }
                    | PaintCommand::DrawBufferRect {
                        dst, buffer_idx, ..
                    } => (*dst, *buffer_idx),
                    _ => continue,
                };
                if let PaintBuffer::Shared(buffer) = &context.buffers()[index] {
                    self.chunks.borrow_mut().push((dst, buffer.clone()));
                }
            }
            scarlet_ui::renderer::PaintBackend::render(
                &mut self.cpu,
                context,
                background,
                logical_damage,
                physical_damage,
            )
        }
    }

    #[test]
    fn completing_one_cover_reuses_sibling_and_gallery_rasters() {
        let cache = CoverCache::new();
        let covers: Vec<_> = (0..3)
            .map(|index| {
                cache.view(
                    Some((
                        "local".into(),
                        ArtworkRef::File(format!("/test/{index}.png").into()),
                    )),
                    80.,
                    Icon::Music,
                )
            })
            .collect();
        let slots: Vec<_> = cache.0.slots.borrow().values().cloned().collect();
        for slot in &slots {
            slot.image
                .set(Some(BitmapImage::from_bgra(vec![0xff12_3456; 64], 8, 8)));
        }
        // Derive the key through the actual provider reference serialization so
        // this test remains independent of ArtworkRef's wire representation.
        let key = format!(
            "local:{}",
            serde_json::to_string(&ArtworkRef::File("/test/0.png".into())).unwrap()
        );
        let first = cache.0.slots.borrow().get(&key).unwrap().clone();
        let content = hstack! {
            vstack! { covers[0].clone(), text("First").frame(80., 20.) }.spacing(4.),
            vstack! { covers[1].clone(), text("Second").frame(80., 20.) }.spacing(4.),
            vstack! { covers[2].clone(), text("Third").frame(80., 20.) }.spacing(4.),
        }
        .spacing(16.)
        .padding(8.)
        .repaint_boundary();
        let size = Size::new(300., 144.);
        let observed = Rc::new(RefCell::new(vec![]));
        let mut pipeline = scarlet_ui::RenderingPipeline::new();
        pipeline.set_paint_backend(Box::new(CoverRasterBackend {
            cpu: scarlet_ui::renderer::CpuPaintBackend::new(size, 1000, Color::BLACK),
            chunks: observed.clone(),
        }));
        pipeline.set_root(
            Window::new("Cover test", content)
                .size(size)
                .create_element(),
        );
        pipeline.layout_initial();
        let settle = |pipeline: &mut scarlet_ui::RenderingPipeline| {
            let mut pixels = vec![];
            for _ in 0..12 {
                if let Some(buffer) = pipeline.render() {
                    pixels = buffer.data().to_vec();
                }
                if !pipeline.has_dirty() {
                    return pixels;
                }
            }
            panic!("Cover UI did not settle");
        };
        let before_pixels = settle(&mut pipeline);
        let before = observed.borrow_mut().drain(..).collect::<Vec<_>>();
        assert!(
            !before.is_empty(),
            "No retained gallery raster was observed"
        );
        first
            .image
            .set(Some(BitmapImage::from_bgra(vec![0xffed_cba9; 64], 8, 8)));
        let after_pixels = settle(&mut pipeline);
        assert_ne!(
            before_pixels, after_pixels,
            "Completed cover did not update pixels"
        );
        let after = observed.borrow();
        assert!(!after.is_empty(), "Cover completion produced no present");
        let mut changed = 0;
        for (rect, buffer) in after.iter() {
            if !before
                .iter()
                .any(|(old_rect, old)| old_rect == rect && Arc::ptr_eq(old, buffer))
            {
                changed += 1;
                assert!(
                    rect.size.width <= 80.5 && rect.size.height <= 80.5 && rect.origin.x < 96.,
                    "Cover completion rerasterized a sibling or gallery chunk: {rect:?}"
                );
            }
        }
        assert!(changed > 0, "No changed cover raster was observed");
    }
}
