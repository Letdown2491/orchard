//! The store view: catalog list, search, full-page app details, install
//! actions.
//!
//! MVP interaction model: type to search, arrow through the list, Enter
//! or click opens the app page. All flatpak work runs on background
//! threads through the single CLI seam (`orchard-flatpak`); the UI polls
//! nothing, results land via `cx.spawn` updates.

use std::collections::{HashMap, HashSet};
use std::ops::Range;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use gpui::{
    div, prelude::*, px, uniform_list, AnyElement, AnimationExt, ClickEvent, ClipboardItem,
    Context, FocusHandle, Hsla, KeyDownEvent, MouseButton, MouseDownEvent, ParentElement, Point,
    Render, ScrollHandle, ScrollStrategy, ScrollWheelEvent, SharedString, Styled, StyledText,
    TextLayout, TextRun, UniformListScrollHandle, Window,
};

use orchard_core::{oci, permissions, CatalogFile, Risk, Source, StoreApp};

use crate::ui::{self, app_icon, badge, button, Tone};

/// Port the in-process indexd listens on. The client is the remote.
pub const INDEXD_PORT: u16 = 7780;

const FLATHUB_REMOTE: &str = "flathub";

/// How long a status-bar line (error or note) lingers before the
/// tick pump retracts it: readable, not permanent. The detail that
/// matters is the outcome, which the bar also shows.
const STATUS_HOLD: Duration = Duration::from_secs(10);

/// Lifecycle of a per-app operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JobState {
    Verifying,
    Installing,
    Updating,
    Removing,
}

impl JobState {
    fn label(&self) -> &'static str {
        match self {
            JobState::Verifying => "Verifying digest…",
            JobState::Installing => "Installing…",
            JobState::Updating => "Updating…",
            JobState::Removing => "Removing…",
        }
    }

    /// The noun for the quiet notes: what the running job is called
    /// when something else presses its button.
    fn noun(&self) -> &'static str {
        match self {
            JobState::Verifying | JobState::Installing => "install",
            JobState::Updating => "update",
            JobState::Removing => "removal",
        }
    }
}

/// A live line from the runner: the stage the flatpak child reports
/// and the percent it announced, if any.
#[derive(Debug, Clone, PartialEq, Eq)]
struct JobProgress {
    stage: String,
    percent: Option<u32>,
}

/// One running operation: the phase behind the static labels, the
/// moment it began (the elapsed counter), the live progress from the
/// child's stdout, and the child pid a cancel press interrupts.
#[derive(Debug, Clone)]
struct Job {
    state: JobState,
    started: std::time::Instant,
    progress: Option<JobProgress>,
    cancel_pid: Option<u32>,
    cancel_requested: bool,
}

impl Job {
    fn new(state: JobState) -> Self {
        Job {
            state,
            started: Instant::now(),
            progress: None,
            cancel_pid: None,
            cancel_requested: false,
        }
    }

    fn label(&self) -> &'static str {
        self.state.label()
    }

    /// The status bar's running line, right-aligned: the most
    /// specific truth available, the static phase only until the
    /// child says something finer.
    fn status_text(&self) -> String {
        let elapsed = elapsed_text(self.started.elapsed());
        match &self.progress {
            Some(p) => match p.percent {
                Some(n) => format!("{} · {n}% · {elapsed}", p.stage),
                None => format!("{} · {elapsed}", p.stage),
            },
            None => match self.state {
                JobState::Verifying => format!("Verifying digest · {elapsed}"),
                JobState::Installing => format!("Installing · {elapsed}"),
                JobState::Updating => format!("Updating · {elapsed}"),
                JobState::Removing => format!("Removing · {elapsed}"),
            },
        }
    }
}

/// Which section of the catalog list is shown.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Filter {
    All,
    Reviewed,
    Publisher,
    Installed,
}

impl Filter {
    fn label(&self) -> &'static str {
        match self {
            Filter::All => "All",
            Filter::Reviewed => "Flathub",
            Filter::Publisher => "Orchard",
            Filter::Installed => "Installed",
        }
    }

    /// The filter a settings name selects, if it names one.
    fn from_setting(name: &str) -> Option<Filter> {
        match name {
            "all" => Some(Filter::All),
            "flathub" => Some(Filter::Reviewed),
            "orchard" => Some(Filter::Publisher),
            "installed" => Some(Filter::Installed),
            _ => None,
        }
    }
}

/// Which relay group the settings page's draft input edits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RelayTarget {
    Events,
    Social,
}

/// What `flatpak list` reported for an installed app.
#[derive(Debug, Clone)]
/// An installed app, as `flatpak list` reports it.
pub struct InstalledInfo {
    /// Deploy version from `flatpak list`, e.g. `1.2.95.453`.
    pub version: Option<String>,
    /// Human size from `flatpak info`, e.g. `1.0 GB`.
    pub size: Option<String>,
    /// Sandbox metadata ini read from the deploy, for the permissions view.
    pub metadata: Option<String>,
    /// True when the deploy lives in the user installation rather than
    /// the system one. Publisher-channel apps install user-scope.
    pub user_scope: bool,
}

pub struct StoreRoot {
    pub(crate) focus_handle: FocusHandle,
    apps: Vec<StoreApp>,
    installed: HashMap<String, InstalledInfo>,
    search: String,
    filter: Filter,
    selected: Option<String>,
    /// The publisher of the row the selection came from, when it came
    /// from a row click: two publishers of one app id are two rows,
    /// and only the clicked twin is the page's subject. Keyboard
    /// selection carries None (the first twin).
    selected_publisher: Option<String>,
    jobs: HashMap<String, Job>,
    /// The app awaiting an install confirmation: its button reads
    /// "Sure?" until the second press starts the job, or Escape, page
    /// close, or another install dismisses it.
    confirm_install: Option<String>,
    /// The app awaiting a remove confirmation: its Remove reads
    /// "Sure?" until the second press starts the job, or Escape,
    /// a page close, or another action dismisses it. Mirrors
    /// `confirm_install` and dies in the same places.
    confirm_remove: Option<String>,
    /// The app awaiting an update confirmation, the third arm of the
    /// same idiom: starting any action disarms all of them.
    confirm_update: Option<String>,
    /// The running job's armed cancel: its pill reads "Cancel?" until
    /// the second press cancels, or Escape or a page close disarms.
    confirm_cancel: Option<String>,
    /// The armed settings clear ("Sure?" on a Clear button): the
    /// same two-press grammar, keyed by the button id.
    confirm_clear: Option<String>,
    /// A quiet status-bar note that is not an error: the cancel
    /// receipt, the one-operation-at-a-time refusal. Cleared by the
    /// next job or a page close.
    job_note: Option<String>,
    /// When the current error line was raised: the tick pump
    /// retracts it once the hold lapses.
    error_at: Option<Instant>,
    /// When the current note line was raised, same clock.
    note_at: Option<Instant>,
    error: Option<String>,
    flathub_loading: bool,
    /// Remote images resolved through the disk cache: url -> path, or
    /// None when the fetch failed.
    fetched: HashMap<String, Option<PathBuf>>,
    /// Fetches in flight, keyed by url.
    fetching: HashSet<String>,
    /// Disk usage for the settings page, measured on open and after a
    /// clear, never per frame.
    cache_sizes: (u64, u64),
    /// Sandbox metadata read from the remote for apps that are not
    /// installed (reviewed channel), fetched on page open and cached
    /// for the session.
    remote_metadata: HashMap<String, String>,
    /// Sizes of the remote commit for apps that are not installed:
    /// app id -> (download, installed), humanized by flatpak.
    remote_sizes: HashMap<String, (Option<String>, Option<String>)>,
    /// Remote fetches in flight (metadata + sizes), keyed by app id.
    remote_fetching: HashSet<String>,
    /// Publisher-channel page-open artifact checks in flight.
    publisher_fetching: HashSet<String>,
    /// Failed pre-install digest checks: app key -> why. Security
    /// relevant, so it shows on the page, not just in the log.
    verify_errors: HashMap<String, String>,
    /// app id -> verified manifest digest (publisher channel,
    /// post-verify).
    verified: HashMap<String, String>,
    /// Verified digest pins on disk (location|pin -> served digest): a
    /// checked pin is a fact that cannot rot, so verdicts survive
    /// restarts and never re-contact the registry.
    verified_pins: std::collections::BTreeMap<String, String>,
    /// Which publisher keys this client has seen per app id: the
    /// continuity record behind the new-key disclosure.
    continuity: orchard_core::ContinuityFile,
    /// Reviews from indexd (kind 30065): app id -> newest first.
    reviews: HashMap<String, Vec<orchard_core::Review>>,
    /// The earliest event per reviewer pubkey, from the reviews
    /// document: the new-key signal for the reviews aggregate.
    first_seen: std::collections::BTreeMap<String, u64>,
    /// A reviews fetch in flight (whole document; small at this scale).
    reviews_fetching: bool,
    /// The signed-in identity's own follows (kind 3 `p` tags): the
    /// personal web of trust. Review rows from these keys carry the
    /// circle glyph; empty when signed out or not yet fetched.
    follows: std::collections::HashSet<String>,
    /// A follows fetch in flight.
    follows_fetching: bool,
    /// The reviewer contact-list graph from indexd: reviewer pubkey
    /// -> the pubkeys their kind 3 follows. Public data; the
    /// distance-2 set is computed from it locally.
    graph: std::collections::BTreeMap<String, Vec<String>>,
    /// A graph fetch in flight.
    graph_fetching: bool,
    /// Every profile indexd holds, parsed once: display names and
    /// picture urls for the account trigger, review rows, and the
    /// publisher line.
    profiles: std::collections::BTreeMap<String, ProfileBits>,
    /// A profiles fetch in flight.
    profiles_fetching: bool,
    /// NIP-05 verdicts per publisher pubkey: the claim that was
    /// checked and whether the domain backed it. Absent means
    /// unchecked, not unverified.
    nip05: std::collections::BTreeMap<String, Nip05Verdict>,
    /// Publisher keys with a NIP-05 check in flight.
    nip05_checking: Vec<String>,
    /// Repository proofs: repository url -> the publisher keys the
    /// forge-hosted proof file named, or None when the fetch failed
    /// (one attempt per repository per session, never a guess).
    repo_proofs: std::collections::BTreeMap<String, Option<Vec<String>>>,
    /// Repositories with a proof fetch in flight.
    repo_proof_fetching: Vec<String>,
    /// The sort dropdown's open state: the click point to anchor at.
    sort_menu: Option<gpui::Point<gpui::Pixels>>,
    /// The signed-in identity's own social-relay profile fetch in
    /// flight (one per session; stops once the profile lands).
    own_profile_fetching: bool,
    /// The reviews list is collapsed by default; the header toggles it.
    reviews_expanded: bool,
    /// The review lens: false shows the circle first (the signed-in
    /// default), true shows everyone as one recency list. Reset when
    /// a different app opens, so every page starts close to home.
    reviews_everyone: bool,
    /// The user's settings, shared with the embedded indexd: relay
    /// sets, channel visibility, update cadence, privacy toggles.
    /// Edits persist and apply live.
    settings: orchard_core::SettingsHandle,
    /// The settings page is open (replaces the list; Escape backs out).
    settings_open: bool,
    /// The npub just copied from a publisher header: that header's
    /// line reads "Copied." for a beat, then reverts to the key. The
    /// revert only fires while the copy is still this key, so copying
    /// two publishers in quick succession leaves the second's word up.
    npub_copied: Option<String>,
    /// The relay editor's draft text, shared by both relay groups.
    relay_draft: String,
    /// Which relay group the draft edits: the input the keyboard is
    /// wired to. None when the page just opened (nothing focused).
    relay_target: Option<RelayTarget>,
    relay_cursor: usize,
    relay_anchor: Option<usize>,
    /// Last painted text layouts for the two relay inputs (events,
    /// social): the caret queries last frame's measured layout.
    relay_layouts: [Option<TextLayout>; 2],
    /// The settings page's vertical scroll.
    settings_scroll: gpui::ScrollHandle,
    /// The signed-in reviewer: a NIP-46 session (the remote signer's
    /// key is the identity). None until the user pairs a signer; the
    /// pairing file survives restarts and restores the session.
    reviewer: Option<Reviewer>,
    /// The sign-in modal is open (a QR pairing, then an optional name).
    signin_modal: bool,
    /// The live pairing session while the QR shows: None once paired
    /// (the session becomes a pending reviewer) or on close.
    connect: Option<ConnectSession>,
    /// The reviewer built from a successful pairing, waiting through
    /// the optional name question.
    pending_reviewer: Option<Reviewer>,
    /// The modal opened from the reviews flow: drop into the composer
    /// after sign-in. The header entry point does not.
    signin_for_review: bool,
    /// The channel dropdown is open, anchored at the click point.
    channel_menu: Option<gpui::Point<gpui::Pixels>>,
    /// The account dropdown is open, anchored at the click point.
    account_menu: Option<gpui::Point<gpui::Pixels>>,
    about_modal: bool,
    /// Compose mode: keystrokes go to the review draft, not the search
    /// box or the page's type-to-close. Entered by clicking the compose
    /// box or pressing r on a page; left by Escape or a successful post.
    composing: bool,
    /// The draft review (or, while the sign-in form is open, the name):
    /// (rating 1.0 to 5.0, text). The two forms never show at once, so
    /// one input serves both. Kept across accidental Escapes, cleared on
    /// post and on page switch.
    review_draft: (Option<f32>, String),
    /// Draft text cursor (byte index) and selection anchor, mirroring
    /// the search box fields.
    review_cursor: usize,
    review_anchor: Option<usize>,
    /// Layout handle for the draft text (caret mapping). None until the
    /// first frame with text has been laid out.
    review_layout: Option<TextLayout>,
    /// A review post in flight.
    review_posting: bool,
    /// Review compose/post error, shown next to the compose box.
    review_error: Option<String>,
    /// Detail page open? Selection survives closing, so keyboard
    /// navigation resumes where it left off.
    page_open: bool,
    /// Active browse category (None = all).
    category: Option<String>,
    /// Text cursor position, byte index into `search`.
    cursor: usize,
    /// Selection anchor (byte index); None = plain caret, no selection.
    anchor: Option<usize>,
    /// Layout handle for the search text: maps byte indices to pixels
    /// for the caret and mouse positioning. None until the first
    /// frame's text has been laid out.
    search_layout: Option<TextLayout>,
    /// Scroll handle for the screenshot strip (wheel capture).
    shot_handle: ScrollHandle,
    /// Scroll handle for the detail page (reset on app switch).
    page_scroll: ScrollHandle,
    /// The open collection page: the subject whose page shows. The
    /// app page underneath stays mounted; Escape pops back to it.
    collection_view: Option<CollectionPage>,
    /// The collection page's scroll.
    collection_scroll: ScrollHandle,
    /// The enlarged screenshot (its url), over a dimmed page.
    lightbox: Option<String>,
    /// Scroll handle for the virtualized list (keyboard reveal).
    list_handle: UniformListScrollHandle,
}

impl StoreRoot {
    /// The channel to open on: the settings' default channel, unless
    /// that channel's source is hidden (then All).
    fn initial_filter(settings: &orchard_core::SettingsHandle) -> Filter {
        let settings = settings.get();
        match Filter::from_setting(&settings.default_channel) {
            Some(Filter::Reviewed) if settings.flathub_source => Filter::Reviewed,
            Some(Filter::Publisher) if settings.orchard_source => Filter::Publisher,
            Some(Filter::Installed) | Some(Filter::All) | None => Filter::All,
            _ => Filter::All,
        }
    }

    pub fn new(cx: &mut Context<Self>, settings: orchard_core::SettingsHandle) -> Self {
        let mut this = StoreRoot {
            focus_handle: cx.focus_handle(),
            apps: Vec::new(),
            installed: HashMap::new(),
            search: String::new(),
            filter: Self::initial_filter(&settings),
            selected: None,
            selected_publisher: None,
            jobs: HashMap::new(),
            confirm_install: None,
            confirm_remove: None,
            confirm_update: None,
            confirm_cancel: None,
            confirm_clear: None,
            job_note: None,
            error_at: None,
            note_at: None,
            verified: HashMap::new(),
            verified_pins: load_digest_cache_at(&digest_cache_path()),
            continuity: load_continuity(),
            error: None,
            flathub_loading: true,
            fetched: HashMap::new(),
            fetching: HashSet::new(),
            cache_sizes: (0, 0),
            remote_metadata: HashMap::new(),
            remote_sizes: HashMap::new(),
            remote_fetching: HashSet::new(),
            publisher_fetching: HashSet::new(),
            verify_errors: HashMap::new(),
            reviews: HashMap::new(),
            first_seen: Default::default(),
            reviews_fetching: false,
            follows: Default::default(),
            follows_fetching: false,
            graph: Default::default(),
            graph_fetching: false,
            profiles: Default::default(),
            profiles_fetching: false,
            nip05: Default::default(),
            nip05_checking: Vec::new(),
            repo_proofs: Default::default(),
            repo_proof_fetching: Vec::new(),
            sort_menu: None,
            own_profile_fetching: false,
            reviews_expanded: false,
            reviews_everyone: false,
            settings: settings.clone(),
            settings_open: false,
            npub_copied: None,
            relay_draft: String::new(),
            relay_target: None,
            relay_cursor: 0,
            relay_anchor: None,
            relay_layouts: [None, None],
            settings_scroll: ScrollHandle::new(),
            reviewer: load_reviewer(),
            signin_modal: false,
            connect: None,
            pending_reviewer: None,
            signin_for_review: false,
            channel_menu: None,
            account_menu: None,
            about_modal: false,
            composing: false,
            review_draft: (None, String::new()),
            review_cursor: 0,
            review_anchor: None,
            review_layout: None,
            review_posting: false,
            review_error: None,
            page_open: false,
            category: None,
            cursor: 0,
            anchor: None,
            search_layout: None,
            shot_handle: ScrollHandle::new(),
            page_scroll: ScrollHandle::new(),
            collection_view: None,
            collection_scroll: ScrollHandle::new(),
            lightbox: None,
            list_handle: UniformListScrollHandle::new(),
        };

        // a saved reviewer signs back in automatically; their profile
        // name resolves in the background when the local record is
        // missing one
        if this.reviewer.is_some() {
            this.resolve_reviewer_name(cx);
        }

        // the job tick: the elapsed counter and the indeterminate
        // sweep repaint while an operation runs, and the status
        // bar's lines are retracted once their hold lapses
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(Duration::from_millis(500)).await;
                if this
                    .update(cx, |this, cx| {
                        let now = Instant::now();
                        let mut changed = false;
                        if this
                            .error_at
                            .is_some_and(|at| now.duration_since(at) >= STATUS_HOLD)
                        {
                            this.error = None;
                            this.error_at = None;
                            changed = true;
                        }
                        if this
                            .note_at
                            .is_some_and(|at| now.duration_since(at) >= STATUS_HOLD)
                        {
                            this.job_note = None;
                            this.note_at = None;
                            changed = true;
                        }
                        if changed || !this.jobs.is_empty() {
                            cx.notify();
                        }
                    })
                    .is_err()
                {
                    break;
                }
            }
        })
        .detach();

        // Orchard channel: read the catalog over HTTP from the in-process
        // indexd (the client is the remote), fall back to the file. Blocking
        // calls on the background pool keep startup snappy.
        cx.spawn(async move |this, cx| {
            let orchard_apps = cx
                .background_executor()
                .spawn(async move { load_orchard_catalog() })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.apply_orchard_apps(orchard_apps, cx);
                // the reviewer graph and the profiles are public data,
                // no session needed
                this.ensure_graph(cx);
                this.ensure_profiles(cx);
                // a session restored from identity.json learns its
                // circle right away, not on the first tick
                if this.reviewer.is_some() {
                    this.ensure_follows(cx);
                    this.ensure_own_profile(cx);
                }
            });
        })
        .detach();

        // Orchard channel refresh: indexd merges relay events as they
        // arrive, so a publisher's new release appears here without a
        // restart. Cheap no-op when the document has not changed.
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(std::time::Duration::from_secs(30))
                    .await;
                let orchard_apps = cx
                    .background_executor()
                    .spawn(async move { load_orchard_catalog() })
                    .await;
                let _ = this.update(cx, |this, cx| {
                    this.apply_orchard_apps(orchard_apps, cx);
                    // reviews only move while somebody is looking
                    if this.page_open {
                        this.ensure_reviews(cx);
                        // the graph feeds the same pages' two-hop marks
                        this.ensure_graph(cx);
                    }
                    // the account trigger wears the profile picture
                    this.ensure_profiles(cx);
                    // a restored session picks its circle up here
                    if this.reviewer.is_some() && this.follows.is_empty() {
                        this.ensure_follows(cx);
                    }
                    if this.reviewer.is_some() {
                        this.ensure_own_profile(cx);
                    }
                });
            }
        })
        .detach();

        // The update scan: how often to re-compare installed apps
        // against their channels' newest versions. The launch check is
        // the orchard loop above plus refresh_installed here; the
        // cadence setting adds periodic repeats, read live from the
        // settings (a cadence edit applies on the next 30s poll).
        let update_settings = settings.clone();
        cx.spawn(async move |this, cx| {
            let mut since_check: u32 = 0;
            loop {
                cx.background_executor()
                    .timer(std::time::Duration::from_secs(30))
                    .await;
                since_check += 30;
                let due = match update_settings.get().update_checks.as_str() {
                    "hourly" => since_check >= 3600,
                    "daily" => since_check >= 86400,
                    _ => false,
                };
                if !due {
                    continue;
                }
                since_check = 0;
                let orchard_apps = cx
                    .background_executor()
                    .spawn(async move { load_orchard_catalog() })
                    .await;
                let _ = this.update(cx, |this, cx| {
                    this.apply_orchard_apps(orchard_apps, cx);
                    this.refresh_installed(cx);
                });
            }
        })
        .detach();

        // Reviewed channel: parse flathub's AppStream in the background.
        cx.spawn(async move |this, cx| {
            let apps = cx
                .background_executor()
                .spawn(async move { load_flathub() })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.apps.extend(apps);
                this.flathub_loading = false;
                cx.notify();
            });
        })
        .detach();

        // Installed state.
        this.refresh_installed(cx);
        this
    }

    // ----- catalog helpers -----

    /// Indices into `apps` matching the search, source filter, and
    /// category, in display order. Returns indices (not clones) so
    /// renders stay cheap.
    fn filtered(&self) -> Vec<usize> {
        let needle = self.search.as_str();
        let settings = self.settings.get();
        let mut out = self
            .apps
            .iter()
            .enumerate()
            // a hidden source vanishes from every channel, list and
            // filter alike: visibility is the source's own switch
            .filter(|(_, app)| match app.source {
                Source::Reviewed => settings.flathub_source,
                Source::Publisher => settings.orchard_source,
            })
            .filter(|(_, app)| match self.filter {
                Filter::All => true,
                Filter::Reviewed => app.source == Source::Reviewed,
                Filter::Publisher => app.source == Source::Publisher,
                Filter::Installed => self.installed.contains_key(&app.app_id),
            })
            .filter(|(_, app)| match &self.category {
                Some(want) => app.categories.iter().any(|c| c == want),
                None => true,
            })
            .filter(|(_, app)| {
                needle.is_empty()
                    || ci_contains(&app.name, needle)
                    || ci_contains(&app.summary, needle)
                    || ci_contains(&app.app_id, needle)
            })
            .map(|(ix, _)| ix)
            .collect::<Vec<_>>();
        // most recently updated first by default; the file order is
        // alphabetical by app id, which reads as noise. Undated apps
        // sink, name breaks ties. The user's chosen sort replaces
        // recency explicitly: a hidden reordering is never ours to
        // make.
        let sort = ListSort::from_settings(&settings.sort);
        let today = orchard_core::appstream::today_iso();
        out.sort_by(|&a, &b| list_sort(&self.apps[a], &self.apps[b], sort, &today));
        out
    }

    fn selected_app(&self) -> Option<&StoreApp> {
        let id = self.selected.as_ref()?;
        self.find_app(id)
    }

    /// The app a selection points at: the id, and when the selection
    /// came from a specific publisher's row, that row's twin only.
    /// None publisher matches either (keyboard selection, reviewed
    /// apps).
    fn find_app(&self, app_id: &str) -> Option<&StoreApp> {
        self.apps.iter().find(|app| {
            app.app_id == app_id
                && self.selected_publisher.as_deref() == app.publisher.as_deref()
        })
    }

    fn refresh_installed(&mut self, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            let installed = cx
                .background_executor()
                .spawn(async move { list_installed() })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.installed = installed;
                cx.notify();
            });
        })
        .detach();
    }

    /// Select an app (keyboard) without opening the page. Images are
    /// fetched when the page opens, not per arrow press.
    fn set_selected(&mut self, app_id: &str, cx: &mut Context<Self>) {
        if self.selected.as_deref() == Some(app_id) {
            return;
        }
        self.selected = Some(app_id.to_string());
        // keyboard selection aims at the first twin; only a row click
        // names its publisher
        self.selected_publisher = None;
        cx.notify();
    }

    /// Open a collection page. The app page (if any) stays mounted
    /// underneath; Escape returns to it.
    fn open_collection(&mut self, page: CollectionPage, cx: &mut Context<Self>) {
        self.collection_view = Some(page);
        self.collection_scroll.set_offset(Point::new(px(0.), px(0.)));
        cx.notify();
    }

    /// Open the detail page for an app and queue its remote images
    /// (icon fallback and screenshots) for the disk cache.
    fn open_page(&mut self, app_id: &str, cx: &mut Context<Self>) {        let is_new = self.selected.as_deref() != Some(app_id);
        self.selected = Some(app_id.to_string());
        if is_new {
            // the strip's horizontal scroll and the page's vertical
            // scroll must not leak between apps, nor a lightbox
            self.shot_handle.set_offset(Point::new(px(0.), px(0.)));
            self.page_scroll.set_offset(Point::new(px(0.), px(0.)));
            self.lightbox = None;
            // a half-written review belongs to the app it was for
            self.composing = false;
            self.reviews_expanded = false;
            self.reviews_everyone = false;
            self.close_signin();
        }
        if self.page_open {
            return;
        }
        self.page_open = true;
        let urls: Vec<String> = self
            .find_app(app_id)
            .map(|app| {
                let mut urls = Vec::new();
                if app.icon_path.is_none()
                    && let Some(url) = &app.icon_url
                {
                    urls.push(url.clone());
                }
                urls.extend(app.screenshots.iter().take(6).cloned());
                urls
            })
            .unwrap_or_default();
        // failed fetches get one retry per page open
        for url in &urls {
            if self.fetched.get(url) == Some(&None) {
                self.fetched.remove(url);
            }
        }
        self.ensure_fetched(urls, cx);
        // pre-install data: for reviewed apps the remote serves the
        // manifest and sizes of the commit that would install; for
        // publisher apps the artifact itself is read (digest check plus
        // sandbox labels in one manifest fetch)
        let source = self.find_app(app_id).map(|a| a.source);
        let not_installed = !self.installed.contains_key(app_id);
        match source {
            Some(Source::Reviewed) if not_installed => self.ensure_remote_data(app_id, cx),
            Some(Source::Publisher) if not_installed => self.ensure_publisher_data(app_id, cx),
            _ => {}
        }
        self.ensure_reviews(cx);
        cx.notify();
    }

    /// Fetch an app's remote commit summary and sandbox manifest
    /// (once per session; failures retry on the next page open).
    fn ensure_remote_data(&mut self, app_id: &str, cx: &mut Context<Self>) {
        let key = app_key(app_id, self.selected_publisher.as_deref());
        if self.remote_metadata.contains_key(&key) && self.remote_sizes.contains_key(&key)
            || !self.remote_fetching.insert(key.clone())
        {
            return;
        }
        let app_id = app_id.to_string();
        let remote = FLATHUB_REMOTE.to_string();
        cx.spawn(async move |this, cx| {
            let id = app_id.clone();
            let remote_meta = remote.clone();
            let metadata = cx
                .background_executor()
                .spawn(async move {
                    orchard_flatpak::remote_metadata(&remote_meta, &id)
                        .output()
                        .ok()
                        .filter(|out| out.status.success())
                        .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_string())
                        .filter(|text| !text.is_empty())
                })
                .await;
            let id = app_id.clone();
            let sizes = cx
                .background_executor()
                .spawn(async move {
                    orchard_flatpak::remote_info(&remote, &id)
                        .output()
                        .ok()
                        .filter(|out| out.status.success())
                        .map(|out| {
                            orchard_flatpak::parse_remote_info(&String::from_utf8_lossy(
                                &out.stdout,
                            ))
                        })
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                if let Some(metadata) = metadata {
                    this.remote_metadata.insert(key.clone(), metadata);
                }
                if let Some(sizes) = sizes {
                    this.remote_sizes.insert(key.clone(), sizes);
                }
                this.remote_fetching.remove(&key);
                cx.notify();
            });
        })
        .detach();
    }

    /// Replace the publisher-channel rows with a fresh read of the
    /// merged catalog (indexd). A no-op when nothing changed, so the
    /// periodic refresh never disturbs list state. Sandbox metadata
    /// read earlier in the session (from the artifact) is transplanted:
    /// that fetch already happened and the pin has not moved.
    fn apply_orchard_apps(&mut self, incoming: Vec<StoreApp>, cx: &mut Context<Self>) {
        let before = orchard_fingerprint(
            self.apps
                .iter()
                .filter(|app| app.source == Source::Publisher),
        );
        let after = orchard_fingerprint(incoming.iter());
        if before == after {
            return;
        }
        let old_rows: Vec<StoreApp> = self
            .apps
            .extract_if(.., |app: &mut StoreApp| app.source == Source::Publisher)
            .collect();
        let mut old_metadata: HashMap<String, Option<String>> = old_rows
            .into_iter()
            .map(|app| (app.app_id.clone(), app.metadata))
            .collect();
        let mut incoming = incoming;
        for app in &mut incoming {
            if app.metadata.is_none()
                && let Some(Some(metadata)) = old_metadata.get(&app.app_id)
            {
                app.metadata = Some(metadata.clone());
            }
            old_metadata.remove(&app.app_id);
        }
        // an app that vanished (unpublished, key revoked) closes the
        // page; so does the selected publisher vanishing while another
        // keeps the same app id alive
        // publisher-key continuity: fold the fresh rows in (they are
        // the whole publisher catalog); a key new for a known app id
        // is the change the page will disclose
        let batch: Vec<(String, String)> = incoming
            .iter()
            .filter_map(|app| {
                app.publisher
                    .as_ref()
                    .map(|pubkey| (app.app_id.clone(), pubkey.clone()))
            })
            .collect();
        self.apps.extend(incoming);
        if !batch.is_empty() && absorb_continuity(&mut self.continuity, &batch) {
            save_continuity(&self.continuity);
        }
        if self.page_open && self.selected_app().is_none() {
            self.selected = None;
            self.selected_publisher = None;
            self.page_open = false;
        }
        self.ensure_nip05_checks(cx);
        self.ensure_repo_proofs(cx);
        cx.notify();
    }

    /// The key's verified NIP-05 claim: the domain backed it AND the
    /// profile still carries the same claim (a changed claim reads as
    /// unverified until rechecked).
    fn verified_nip05(&self, pubkey: &str) -> Option<String> {
        let verdict = self.nip05.get(pubkey)?;
        if !verdict.verified {
            return None;
        }
        match self
            .profiles
            .get(pubkey)
            .and_then(|p| p.nip05.as_deref())
        {
            Some(claim) if claim == verdict.claim => Some(verdict.claim.clone()),
            _ => None,
        }
    }

    /// Kick a NIP-05 check for every publisher whose profile claims an
    /// address and whose claim has no verdict yet: the domain behind
    /// the claim is the only party that can confirm it, so the client
    /// asks, once per claim per session, and renders the address only
    /// when the answer is yes. A claim on its own shows nothing. The
    /// signed-in identity's own claim joins the scan (the account
    /// menu wears the same check), and so do comment authors: the
    /// review rows play by the same rule.
    /// Repository proofs for publisher rows: the forge must host the
    /// publisher's key. One fetch per repository per session; any
    /// failure is unverified, never a guess.
    fn ensure_repo_proofs(&mut self, cx: &mut Context<Self>) {
        let repos: std::collections::BTreeSet<String> = self
            .apps
            .iter()
            .filter(|app| app.source == Source::Publisher)
            .filter_map(|app| app.repository.clone())
            .collect();
        for repo in repos {
            if self.repo_proofs.contains_key(&repo)
                || self.repo_proof_fetching.contains(&repo)
            {
                continue;
            }
            let Some(url) = proof_url(&repo) else {
                // a forge we cannot derive a proof URL for is a
                // recorded miss: unverified, not retried per frame
                self.repo_proofs.insert(repo, None);
                continue;
            };
            self.repo_proof_fetching.push(repo.clone());
            cx.spawn(async move |this, cx| {
                let keys = cx
                    .background_executor()
                    .spawn(async move { fetch_proof_keys(&url) })
                    .await;
                let _ = this.update(cx, |this, cx| {
                    this.repo_proof_fetching.retain(|r| r != &repo);
                    this.repo_proofs.insert(repo, keys);
                    cx.notify();
                });
            })
            .detach();
        }
    }

    /// True when the repository's proof file names this publisher
    /// key: the repo-verified fact.
    fn repo_verified(&self, repository: &str, pubkey: Option<&str>) -> bool {
        let Some(Some(keys)) = self.repo_proofs.get(repository) else {
            return false;
        };
        pubkey.is_some_and(|key| keys.iter().any(|k| k == key))
    }

    /// Does this row's listing carry a verified repository proof?
    fn row_verified(&self, app: &StoreApp) -> bool {
        self.repo_verified(
            app.repository.as_deref().unwrap_or(""),
            app.publisher.as_deref(),
        )
    }

    fn ensure_nip05_checks(&mut self, cx: &mut Context<Self>) {
        let mut candidates: std::collections::BTreeSet<String> = self
            .apps
            .iter()
            .filter(|app| app.source == Source::Publisher)
            .filter_map(|app| app.publisher.clone())
            // the signed-in identity's own claim: the account menu
            .chain(self.reviewer.as_ref().map(|r| r.signer_pubkey.clone()))
            .collect();
        // comment authors too: their rows get the same treatment
        for review in self.reviews.values().flatten() {
            candidates.insert(review.pubkey.clone());
        }
        let pending: Vec<(String, String)> = candidates
            .into_iter()
            .filter(|pubkey| {
                !self.nip05.contains_key(pubkey) && !self.nip05_checking.contains(pubkey)
            })
            .filter_map(|pubkey| {
                let claim = self.profiles.get(&pubkey)?.nip05.clone()?;
                Some((pubkey, claim))
            })
            .collect();
        for (pubkey, claim) in pending {
            self.nip05_checking.push(pubkey.clone());
            cx.spawn(async move |this, cx| {
                let check_key = pubkey.clone();
                let check_claim = claim.clone();
                let verified = cx
                    .background_executor()
                    .spawn(async move { verify_nip05(&check_claim, &check_key) })
                    .await;
                let _ = this.update(cx, |this, cx| {
                    this.nip05_checking.retain(|key| key != &pubkey);
                    this.nip05.insert(pubkey, Nip05Verdict { claim, verified });
                    cx.notify();
                });
            })
            .detach();
        }
    }

    /// Pre-install artifact check for publisher apps: one manifest fetch
    /// verifies the pinned digest and reads the sandbox labels, so the
    /// page shows exactly what install will pin. Same fetch the install
    /// button and the list rows run; failures surface on the page and
    /// the next session tries again.
    fn ensure_publisher_data(&mut self, app_id: &str, cx: &mut Context<Self>) {
        let key = app_key(app_id, self.selected_publisher.as_deref());
        let Some(app) = self.find_app(app_id) else {
            return;
        };
        let (location, pinned, found_id, publisher) = (
            app.location.clone(),
            app.pinned_digest.clone(),
            app.app_id.clone(),
            app.publisher.clone(),
        );
        self.run_digest_check(key, location, pinned, found_id, publisher, cx);
    }

    /// The list's viewport trigger: a publisher row entering the list
    /// earns its row badge the way a page open does, adopting a disk
    /// verdict before any network. Skipped in the Orchard channel view:
    /// its rows hide the badge, so checking them is registry contact
    /// with nothing to show for it.
    fn ensure_row_digest(
        &mut self,
        key: &str,
        location: Option<String>,
        pinned: Option<String>,
        app_id: &str,
        publisher: Option<String>,
        cx: &mut Context<Self>,
    ) {
        if self.verified.contains_key(key)
            || self.verify_errors.contains_key(key)
            || self.publisher_fetching.contains(key)
        {
            return;
        }
        let cached = match (location.as_deref(), pinned.as_deref()) {
            (Some(location_text), Some(pinned)) => self
                .verified_pins
                .get(&digest_cache_key(location_text, pinned))
                .cloned(),
            _ => None,
        };
        if let Some(digest) = cached {
            self.verified.insert(key.to_string(), digest);
            cx.notify();
            return;
        }
        self.run_digest_check(key.to_string(), location, pinned, app_id.to_string(), publisher, cx);
    }

    /// The one digest check: fetch the registry manifest, compare the
    /// pin, read the sandbox labels. Ok records the verdict in memory
    /// and, with a location and pin present, on disk (a checked pin is
    /// a fact that cannot rot, so the cache never expires); Err records
    /// the failure for the page.
    fn run_digest_check(
        &mut self,
        key: String,
        location_text: Option<String>,
        pinned: Option<String>,
        app_id: String,
        publisher: Option<String>,
        cx: &mut Context<Self>,
    ) {
        if !self.publisher_fetching.insert(key.clone()) {
            return;
        }
        let location = match location_text
            .as_deref()
            .and_then(|raw| oci::Location::parse(raw).ok())
        {
            Some(location) => location,
            None => {
                self.publisher_fetching.remove(&key);
                return;
            }
        };
        let cache_key = pinned
            .as_deref()
            .map(|pin| digest_cache_key(location_text.as_deref().unwrap_or(""), pin));
        cx.spawn(async move |this, cx| {
            let report = cx
                .background_executor()
                .spawn(async move { oci::verify(&location, pinned.as_deref()) })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.publisher_fetching.remove(&key);
                match report {
                    Ok(report) => {
                        this.verified.insert(key.clone(), report.digest.clone());
                        if let Some(cache_key) = cache_key {
                            this.verified_pins.insert(cache_key, report.digest.clone());
                            save_digest_cache_at(&digest_cache_path(), &this.verified_pins);
                        }
                        if let Some(app) = this.apps.iter_mut().find(|a| {
                            a.app_id == app_id && a.publisher == publisher
                        }) {
                            if app.metadata.is_none() {
                                app.metadata = report.metadata.clone();
                            }
                            this.remote_sizes.insert(
                                key.clone(),
                                (
                                    Some(human_size(report.download_size)),
                                    report.installed_size.map(human_size),
                                ),
                            );
                        }
                    }
                    Err(err) => {
                        // the pin did not match or the artifact is
                        // unreadable: show it on the page, and the next
                        // session tries again
                        this.verify_errors
                            .insert(key.clone(), format!("{err:#}"));
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    // ----- reviews (kind 30065) -----

    /// Fetch every profile indexd holds: display names and picture
    /// urls for the account trigger, review rows, and the publisher
    /// line. Public data; parsed once on arrival.
    fn ensure_profiles(&mut self, cx: &mut Context<Self>) {
        if self.profiles_fetching {
            return;
        }
        self.profiles_fetching = true;
        cx.spawn(async move |this, cx| {
            let fetched = cx
                .background_executor()
                .spawn(async move { load_profiles() })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.profiles_fetching = false;
                match fetched {
                    Ok(profiles) => {
                        // merge, not replace: the signed-in identity's
                        // own profile (fetched from the social relays,
                        // which indexd need not share) must survive
                        for (pubkey, bits) in profiles {
                            this.profiles.insert(pubkey, bits);
                        }
                        this.ensure_nip05_checks(cx);
                        cx.notify();
                    }
                    Err(err) => {
                        // keep the old profiles; the next tick retries
                        eprintln!("profiles fetch failed: {err:#}");
                    }
                }
            });
        })
        .detach();
    }

    /// The signed-in identity's own profile, fetched read-only and
    /// anonymously from the social relays: indexd only holds profiles
    /// its catalog relays carried, and the dev relay never sees the
    /// user's kind 0. Lands in the same profiles map the account
    /// trigger and "Signed in as" read.
    fn ensure_own_profile(&mut self, cx: &mut Context<Self>) {
        let Some(reviewer) = self.reviewer.as_ref() else {
            return;
        };
        let pubkey = reviewer.signer_pubkey.clone();
        if self.own_profile_fetching || self.profiles.contains_key(&pubkey) {
            return;
        }
        self.own_profile_fetching = true;
        let key = pubkey.clone();
        cx.spawn(async move |this, cx| {
            let bits = cx
                .background_executor()
                .spawn(async move { load_own_profile(&key) })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.own_profile_fetching = false;
                if let Some(bits) = bits {
                    // the name shows up in "Signed in as" even when
                    // indexd's feed never carried the profile
                    if let Some(reviewer) = this.reviewer.as_mut()
                        && reviewer.signer_pubkey == pubkey
                        && reviewer.name.is_none()
                    {
                        reviewer.name = bits.name.clone();
                    }
                    this.profiles.insert(pubkey, bits);
                    cx.notify();
                }
            });
        })
        .detach();
    }

    /// Fetch the reviewer contact-list graph from indexd (startup and
    /// the refresh tick; the crawler over there refreshes on its own
    /// cadence). Page-independent: any open reviews section renders
    /// from it.
    fn ensure_graph(&mut self, cx: &mut Context<Self>) {
        if self.graph_fetching {
            return;
        }
        self.graph_fetching = true;
        cx.spawn(async move |this, cx| {
            let fetched = cx
                .background_executor()
                .spawn(async move { load_graph() })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.graph_fetching = false;
                match fetched {
                    Ok(graph) => {
                        this.graph = graph;
                        cx.notify();
                    }
                    Err(err) => {
                        // keep the old graph; the next tick retries
                        eprintln!("graph fetch failed: {err:#}");
                    }
                }
            });
        })
        .detach();
    }

    /// Fetch the reviews document from indexd (once per page open; the
    /// refresh loop repeats the call while a page is open). The whole
    /// document is small at this scale; per-app filtering happens here.
    fn ensure_reviews(&mut self, cx: &mut Context<Self>) {
        if self.reviews_fetching {
            return;
        }
        self.reviews_fetching = true;
        cx.spawn(async move |this, cx| {
            let fetched = cx
                .background_executor()
                .spawn(async move { load_reviews() })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.reviews_fetching = false;
                match fetched {
                    Ok((grouped, first_seen)) => {
                        this.reviews = grouped;
                        this.first_seen = first_seen;
                        // fresh comment authors may carry claims; the
                        // scan covers them on arrival, not next tick
                        this.ensure_nip05_checks(cx);
                        cx.notify();
                    }
                    Err(err) => {
                        // keep the old reviews; the next page open or
                        // refresh tick retries
                        eprintln!("reviews fetch failed: {err:#}");
                    }
                }
            });
        })
        .detach();
    }

    /// Fetch the signed-in identity's contact list from the social
    /// relays: one kind 3 per relay, read-only and anonymous. Guarded
    /// like the other fetches. An empty union keeps the old set: a
    /// relay outage must not erase a known circle. Kind 3 edits mid
    /// session are picked up on the next launch.
    fn ensure_follows(&mut self, cx: &mut Context<Self>) {
        let Some(reviewer) = self.reviewer.as_ref() else {
            return;
        };
        if self.follows_fetching {
            return;
        }
        self.follows_fetching = true;
        let pubkey = reviewer.signer_pubkey.clone();
        cx.spawn(async move |this, cx| {
            let fetched = cx
                .background_executor()
                .spawn(async move { load_follows(&pubkey) })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.follows_fetching = false;
                if !fetched.is_empty() {
                    this.follows = fetched;
                    cx.notify();
                }
            });
        })
        .detach();
    }

    /// Post the draft review for the open page: sign with the local
    /// identity, send to the relays, refresh the section.
    fn post_review(&mut self, app_id: &str, cx: &mut Context<Self>) {
        if self.review_posting {
            return;
        }
        if self.reviewer.is_none() {
            self.review_error = Some("Sign in to post.".into());
            cx.notify();
            return;
        };
        let Some(rating) = self.review_draft.0 else {
            self.review_error = Some("Pick a rating first (keys 1 to 5).".into());
            cx.notify();
            return;
        };
        let reviewer = self.reviewer.clone().unwrap();
        let content = self.review_draft.1.trim().to_string();
        self.review_posting = true;
        self.review_error = None;
        let app_id = app_id.to_string();
        let relays = review_relays(&self.settings);
        cx.spawn(async move |this, cx| {
            let posted = cx
                .background_executor()
                .spawn(async move { post_review(&reviewer, &app_id, rating, content, &relays) })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.review_posting = false;
                match posted {
                    Ok(()) => {
                        this.review_draft = (None, String::new());
                        this.review_cursor = 0;
                        this.review_anchor = None;
                        this.review_layout = None;
                        this.composing = false;
                        this.ensure_reviews(cx);
                    }
                    Err(err) => {
                        this.review_error = Some(format!("{err:#}"));
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Enter the review flow from the page: compose when signed in,
    /// the sign-in modal when not. Both expand the section.
    fn start_review(&mut self, cx: &mut Context<Self>) {
        self.reviews_expanded = true;
        self.review_error = None;
        if self.reviewer.is_some() {
            self.composing = true;
        } else {
            self.open_signin_modal(true, cx);
        }
        cx.notify();
    }

    /// Open the sign-in modal and start a fresh pairing session: an
    /// ephemeral client key, the pairing URI with its anti-spoofing
    /// secret, and the QR image on disk. The connect relay is the
    /// first review relay; it has to be one the signer app can reach.
    fn open_signin_modal(&mut self, for_review: bool, cx: &mut Context<Self>) {
        self.close_signin();
        self.signin_modal = true;
        self.signin_for_review = for_review;
        self.start_pairing(cx);
    }

    /// Build one pairing session and start its poll loop. Runs again
    /// for "new pairing code".
    fn start_pairing(&mut self, cx: &mut Context<Self>) {
        use nostr::key::GetPublicKey as _;
        use nostr::prelude::*;
        self.review_error = None;
        let relay = connect_relay();
        let keys = Keys::generate();
        // the anti-spoofing secret: the signer echoes it back in the
        // connect response, a pairing answer without it is noise. A
        // fresh key's secret hex is 64 random characters, enough for
        // an echo token.
        let secret = Keys::generate().secret_key().to_secret_hex();
        let client_pubkey = keys.get_public_key().expect("generated key has a pubkey");
        let uri = match orchard_nostr::connect::connect_uri(
            &client_pubkey,
            &relay,
            "Orchard",
            "https://github.com/Letdown2491/orchard",
            // the whole request surface: reviews and profile metadata,
            // nothing else
            "sign_event:30065,sign_event:0",
            &secret,
        ) {
            Ok(uri) => uri,
            Err(err) => {
                self.review_error = Some(format!("pairing URI failed: {err:#}"));
                return;
            }
        };
        let qr_path = match crate::qr::write_qr_png(&uri, &qr_cache_dir()) {
            Ok(path) => path,
            Err(err) => {
                self.review_error = Some(format!("QR image failed: {err:#}"));
                return;
            }
        };
        self.connect = Some(ConnectSession {
            keys: keys.clone(),
            qr_path,
            uri,
            relay: relay.clone(),
        });
        cx.notify();

        // the poll loop: while the modal shows the QR, listen for the
        // signer's connect response; pairing completes from here
        cx.spawn(async move |this, cx| loop {
            cx.background_executor()
                .timer(std::time::Duration::from_millis(1200))
                .await;
            let waiting = this
                .update(cx, |this, _| {
                    this.signin_modal && this.connect.is_some() && this.pending_reviewer.is_none()
                })
                .unwrap_or(false);
            if !waiting {
                break;
            }
            let answer = cx
                .background_executor()
                .spawn({
                    let relay = relay.clone();
                    let keys = keys.clone();
                    let secret = secret.clone();
                    async move {
                        orchard_nostr::connect::poll_connect(
                            &relay,
                            &keys,
                            &secret,
                            std::time::Duration::from_millis(1500),
                        )
                    }
                })
                .await;
            // a signer's answer, or nothing yet: poll again; relay
            // hiccups retry the same way until the modal closes
            if let Ok(Some((signer_pubkey, secret))) = answer {
                let _ = this.update(cx, |this, cx| {
                    this.pairing_connected(signer_pubkey, secret, cx);
                });
                break;
            }
        })
        .detach();
    }

    /// A signer answered the pairing: keep the session as a pending
    /// reviewer and ask the optional name question.
    fn pairing_connected(&mut self, signer_pubkey: String, secret: String, cx: &mut Context<Self>) {
        let Some(session) = self.connect.take() else {
            return;
        };
        self.pending_reviewer = Some(Reviewer {
            keys: session.keys,
            relay: session.relay,
            signer_pubkey,
            secret,
            name: None,
        });
        self.review_draft = (None, String::new());
        self.review_cursor = 0;
        self.review_anchor = None;
        self.review_layout = None;
        self.review_error = None;
        cx.notify();
    }

    /// Finish sign-in from the name question: persist the pairing
    /// (plus the optional published name), close the modal, drop into
    /// compose when it opened from the reviews flow.
    fn finish_sign_in(&mut self, cx: &mut Context<Self>) {
        let Some(pending) = self.pending_reviewer.take() else {
            self.close_signin();
            cx.notify();
            return;
        };
        let name = self.review_draft.1.trim().to_string();
        let name = (!name.is_empty()).then_some(name);
        let from_reviews = self.signin_for_review;
        let mut reviewer = pending;
        reviewer.name = name.clone();
        let relays = review_relays(&self.settings);
        cx.spawn(async move |this, cx| {
            let done = cx
                .background_executor()
                .spawn(async move {
                    let publish = (|| -> anyhow::Result<()> {
                        save_reviewer(&reviewer)?;
                        if let Some(published) = &name {
                            // proper profile management: merge against
                            // the author's latest kind 0 so every field
                            // they ever published survives; when indexd
                            // cannot answer, publish nothing rather
                            // than guess
                            match fetch_profile_raws() {
                                Ok(raws) => {
                                    let merged = merge_profile_raw(
                                        raws.get(&reviewer.signer_pubkey).map(String::as_str),
                                        published,
                                    );
                                    if let Err(err) = send_signed(
                                        &reviewer,
                                        orchard_nostr::KIND_PROFILE as u16,
                                        Vec::new(),
                                        &merged,
                                        &relays,
                                    ) {
                                        return Err(anyhow::anyhow!(
                                            "profile publish failed: {err:#}"
                                        ));
                                    }
                                }
                                Err(err) => {
                                    return Err(anyhow::anyhow!(
                                        "could not read your existing profile ({err:#}); \
                                         the name is kept on this device only"
                                    ));
                                }
                            }
                        }
                        Ok(())
                    })();
                    (reviewer, publish)
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                let (reviewer, publish) = done;
                match publish {
                    Ok(()) => {
                        let named_locally = reviewer.name.is_some();
                        this.reviewer = Some(reviewer);
                        this.signin_modal = false;
                        this.signin_for_review = false;
                        this.composing = from_reviews;
                        this.review_draft = (None, String::new());
                        this.review_cursor = 0;
                        this.review_anchor = None;
                        this.review_layout = None;
                        this.review_error = None;
                        if !named_locally {
                            // an established key: their name comes from
                            // the profile they already published
                            this.resolve_reviewer_name(cx);
                        }
                        // past reviews by this key pick up the new
                        // name once the profile propagates
                        this.ensure_reviews(cx);
                        this.ensure_follows(cx);
                        this.ensure_own_profile(cx);
                    }
                    // keep the pairing: the name question stays up with
                    // the error under it, so the user can retry
                    Err(err) => {
                        this.pending_reviewer = Some(reviewer);
                        this.review_error = Some(format!("{err:#}"));
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Fill the signed-in reviewer's display name from their kind 0
    /// profile when it was not chosen locally. Runs in the background;
    /// a missing or unreachable profile leaves the pubkey fragment.
    fn resolve_reviewer_name(&mut self, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            let raws = cx
                .background_executor()
                .spawn(async move { fetch_profile_raws() })
                .await;
            let _ = this.update(cx, |this, cx| {
                if let Ok(raws) = raws
                    && let Some(reviewer) = this.reviewer.as_mut()
                    && reviewer.name.is_none()
                    && let Some(raw) = raws.get(&reviewer.signer_pubkey)
                {
                    reviewer.name = ProfileBits::parse(raw).name;
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// End the session: the pairing stays in identity.json on this
    /// device and restores the session on the next launch. The secret
    /// key itself never lives here at all.
    fn sign_out(&mut self, cx: &mut Context<Self>) {
        self.reviewer = None;
        self.composing = false;
        cx.notify();
    }

    /// Close the sign-in modal, dropping the pairing session and any
    /// half-paired signer. The reviews draft resets with it: the
    /// shared input belongs to whatever form opens next.
    fn close_signin(&mut self) {
        self.signin_modal = false;
        self.signin_for_review = false;
        self.connect = None;
        self.pending_reviewer = None;
        self.review_draft = (None, String::new());
        self.review_cursor = 0;
        self.review_anchor = None;
        self.review_layout = None;
        self.review_error = None;
    }

    /// Close the detail page. Selection survives so keyboard
    /// navigation resumes where it left off.
    fn close_page(&mut self, cx: &mut Context<Self>) {
        self.page_open = false;
        self.lightbox = None;
        self.composing = false;
        self.reviews_expanded = false;
        self.disarm_confirms();
        self.job_note = None;
        self.close_signin();
        cx.notify();
    }

    // ----- settings -----

    /// Open the settings page from the account menu. It replaces the
    /// list and the detail page alike; Escape backs out.
    fn open_settings(&mut self, cx: &mut Context<Self>) {
        self.account_menu = None;
        self.about_modal = false;
        self.channel_menu = None;
        self.sort_menu = None;
        self.confirm_clear = None;
        self.collection_view = None;
        self.close_signin();
        if self.page_open {
            self.close_page(cx);
        }
        self.lightbox = None;
        self.settings_open = true;
        self.settings_scroll.set_offset(Point::new(px(0.), px(0.)));
        self.relay_draft.clear();
        self.relay_cursor = 0;
        self.relay_anchor = None;
        // keyboard-first: the catalog relay input is the page's text
        // surface from the first keystroke
        self.relay_target = Some(RelayTarget::Events);
        self.refresh_cache_sizes();
        cx.notify();
    }

    fn close_settings(&mut self, cx: &mut Context<Self>) {
        self.settings_open = false;
        self.confirm_clear = None;
        self.relay_target = None;
        self.relay_draft.clear();
        self.relay_anchor = None;
        cx.notify();
    }

    /// Edit the settings and repaint. Persistence happens inside the
    /// handle (atomic write); consumers, indexd included, read the
    /// shared state on their next tick or frame.
    fn edit_settings(
        &mut self,
        edit: impl FnOnce(&mut orchard_core::Settings),
        cx: &mut Context<Self>,
    ) {
        self.settings.update(edit);
        cx.notify();
    }

    /// Show or hide a channel's source. Hiding the source of the
    /// active channel filter resets the filter: the hidden channel no
    /// longer appears in the dropdown, so an empty-looking list would
    /// only confuse.
    fn set_source_visible(&mut self, reviewed: bool, visible: bool, cx: &mut Context<Self>) {
        self.edit_settings(
            |settings| {
                if reviewed {
                    settings.flathub_source = visible;
                } else {
                    settings.orchard_source = visible;
                }
            },
            cx,
        );
        let hidden = match self.filter {
            Filter::Reviewed => reviewed,
            Filter::Publisher => !reviewed,
            _ => false,
        };
        if hidden && !visible {
            self.filter = Filter::All;
        }
    }

    /// The relay group the settings page edits, for the draft input.
    /// The required relay is pinned: a file list that omits it still
    /// shows it first, so the display matches what the client reads.
    /// An empty list stays empty: it means the built-in set.
    fn relay_urls_for(&self, target: RelayTarget, settings: &orchard_core::Settings) -> Vec<String> {
        let list = match target {
            RelayTarget::Events => settings.event_relays.clone(),
            RelayTarget::Social => settings.social_relays.clone(),
        };
        if list.is_empty() {
            list
        } else {
            orchard_nostr::relay::with_required_relay(list)
        }
    }

    /// Validate and add the draft: a ws:// or wss:// URL, not already
    /// in the group's effective set. Adding while the built-in set is
    /// showing starts the file's list as the built-in set plus the
    /// new URL, so adding never silently drops the built-ins. Empty
    /// or bad drafts are ignored silently: the input keeps its text
    /// so the user can fix it.
    fn add_relay_from_draft(&mut self, cx: &mut Context<Self>) {
        let Some(target) = self.relay_target else {
            return;
        };
        let url = self.relay_draft.trim().to_string();
        if !url.starts_with("ws://") && !url.starts_with("wss://") {
            return;
        }
        // a URL needs somewhere to connect to after the scheme
        if url.len() <= "ws://".len() {
            return;
        }
        let custom = self.relay_urls_for(target, &self.settings.get());
        let effective = if custom.is_empty() {
            orchard_nostr::relay::relay_urls(None)
        } else {
            custom
        };
        if effective.iter().any(|listed| listed == &url) {
            self.relay_draft.clear();
            self.relay_cursor = 0;
            self.relay_anchor = None;
            cx.notify();
            return;
        }
        self.edit_settings(
            |settings| {
                let list = match target {
                    RelayTarget::Events => &mut settings.event_relays,
                    RelayTarget::Social => &mut settings.social_relays,
                };
                if list.is_empty() {
                    *list = orchard_nostr::relay::relay_urls(None);
                }
                list.push(url.clone());
            },
            cx,
        );
        self.relay_draft.clear();
        self.relay_cursor = 0;
        self.relay_anchor = None;
        cx.notify();
    }

    /// Remove one row. While the built-in set is showing (an empty
    /// file list), removal switches the file to an explicit list: the
    /// built-ins minus the removed one. The supervisor applies the
    /// change within a second. The required relay cannot be removed:
    /// its row wears no removal button, and a removal that lands on it
    /// anyway is a no-op.
    fn remove_relay(&mut self, target: RelayTarget, index: usize, cx: &mut Context<Self>) {
        // the list the buttons index into: the file's list with the
        // required relay pinned in, or the built-in set for an empty one
        let mut display = self.relay_urls_for(target, &self.settings.get());
        if display.is_empty() {
            display = orchard_nostr::relay::relay_urls(None);
        }
        if display.get(index).map(String::as_str)
            == Some(orchard_nostr::relay::REQUIRED_RELAY)
        {
            return;
        }
        self.edit_settings(
            |settings| {
                let list = match target {
                    RelayTarget::Events => &mut settings.event_relays,
                    RelayTarget::Social => &mut settings.social_relays,
                };
                if list.is_empty() {
                    *list = display
                        .iter()
                        .enumerate()
                        .filter(|(position, _)| *position != index)
                        .map(|(_, url)| url.clone())
                        .collect();
                } else {
                    // the display sits one row above the file's list
                    // when the pin is a display-only prepend
                    let offset = display.len() - list.len();
                    if let Some(position) = index.checked_sub(offset)
                        && position < list.len()
                    {
                        list.remove(position);
                    }
                }
            },
            cx,
        );
    }

    /// Back to the built-in set: an empty list means the rendezvous
    /// default (or the environment override, which always wins).
    fn reset_relays(&mut self, target: RelayTarget, cx: &mut Context<Self>) {
        self.edit_settings(
            |settings| match target {
                RelayTarget::Events => settings.event_relays.clear(),
                RelayTarget::Social => settings.social_relays.clear(),
            },
            cx,
        );
        if self.relay_target == Some(target) {
            self.relay_draft.clear();
            self.relay_cursor = 0;
            self.relay_anchor = None;
        }
    }

    /// Keyboard editing for the relay draft: one line, like the
    /// search box but self-contained. Byte indices like the search's;
    /// backspace and delete walk to a char boundary first so a
    /// multi-byte paste cannot split.
    fn move_relay_cursor(&mut self, key: &str, _extend: bool) {
        let len = self.relay_draft.len();
        match key {
            "left" => {
                self.relay_cursor = self.prev_boundary(self.relay_cursor);
            }
            "right" => {
                let mut cursor = (self.relay_cursor + 1).min(len);
                while cursor < len && !self.relay_draft.is_char_boundary(cursor) {
                    cursor += 1;
                }
                self.relay_cursor = cursor;
            }
            "home" => self.relay_cursor = 0,
            "end" => self.relay_cursor = len,
            "backspace" => {
                if self.relay_cursor > 0 {
                    let cursor = self.prev_boundary(self.relay_cursor);
                    self.relay_draft.remove(cursor);
                    self.relay_cursor = cursor;
                }
            }
            "delete"
                if self.relay_cursor < len => {
                    let mut end = self.relay_cursor + 1;
                    while end < len && !self.relay_draft.is_char_boundary(end) {
                        end += 1;
                    }
                    self.relay_draft.replace_range(self.relay_cursor..end, "");
                }
            _ => {}
        }
        self.relay_anchor = None;
    }

    /// The previous char boundary at or before `from`.
    fn prev_boundary(&self, from: usize) -> usize {
        let mut cursor = from.saturating_sub(1);
        while cursor > 0 && !self.relay_draft.is_char_boundary(cursor) {
            cursor -= 1;
        }
        cursor
    }

    /// The draft's selection range, ordered.
    fn relay_selection(&self) -> (usize, usize) {
        match self.relay_anchor {
            Some(anchor) if anchor <= self.relay_cursor => (anchor, self.relay_cursor),
            Some(anchor) => (self.relay_cursor, anchor),
            None => (self.relay_cursor, self.relay_cursor),
        }
    }

    fn insert_relay_text(&mut self, text: &str) {
        // a selection collapses into the insert
        let (start, end) = match self.relay_anchor {
            Some(anchor) => {
                let (start, end) = if anchor <= self.relay_cursor {
                    (anchor, self.relay_cursor)
                } else {
                    (self.relay_cursor, anchor)
                };
                self.relay_anchor = None;
                (start.min(self.relay_draft.len()), end.min(self.relay_draft.len()))
            }
            None => (self.relay_cursor, self.relay_cursor),
        };
        self.relay_draft.replace_range(start..end, text);
        self.relay_cursor = start + text.len();
    }

    /// Drop every cached image and forget the fetched map: pages fall
    /// back to placeholders until (and unless) images are re-enabled.
    fn clear_image_cache(&mut self, cx: &mut Context<Self>) {
        self.fetched.clear();
        self.fetching.clear();
        cx.spawn(async move |this, cx| {
            crate::cache::clear_images();
            let _ = this.update(cx, |this, cx| {
                this.refresh_cache_sizes();
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    /// Drop absorbed events through indexd: the feed empties, the
    /// disk cache empties on the writer's next tick, and the relay
    /// threads re-subscribe and re-deliver. The Orchard channel
    /// refills from the relays within seconds.
    fn clear_event_cache(&mut self, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            let agent = ureq::AgentBuilder::new()
                .timeout(std::time::Duration::from_secs(5))
                .build();
            let _ = agent
                .post(&format!("http://127.0.0.1:{INDEXD_PORT}/v1/cache"))
                .call();
            // the writer's next tick (3s) does the actual emptying;
            // one extra beat keeps the reading honest
            cx.background_executor()
                .timer(std::time::Duration::from_secs(4))
                .await;
            let _ = this.update(cx, |this, cx| {
                this.refresh_cache_sizes();
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    /// The settings page's disk-usage lines. Measured once per open
    /// (and after a clear), not per frame: the walk stats every cache
    /// file, and the page repaints on every scroll and hover.
    fn refresh_cache_sizes(&mut self) {
        self.cache_sizes = (
            crate::cache::image_cache_bytes(),
            crate::cache::event_cache_bytes(),
        );
    }


    /// Start a background fetch for each not-yet-cached url. Results
    /// land in `fetched` and notify.
    fn ensure_fetched(&mut self, urls: Vec<String>, cx: &mut Context<Self>) {
        // the remote-images setting is consulted per call, so turning
        // it on applies to the next render pass with no restart
        if !self.settings.get().remote_images {
            return;
        }
        let mut todo = Vec::new();
        for url in urls {
            if self.fetched.contains_key(&url) || self.fetching.contains(&url) {
                continue;
            }
            self.fetching.insert(url.clone());
            todo.push(url);
        }
        if todo.is_empty() {
            return;
        }
        cx.spawn(async move |this, cx| {
            for url in todo {
                let target = url.clone();
                let path = cx
                    .background_executor()
                    .spawn(async move { crate::cache::fetch(&target) })
                    .await;
                let _ = this.update(cx, |this, cx| {
                    this.fetching.remove(&url);
                    this.fetched.insert(url, path);
                    cx.notify();
                });
            }
        })
        .detach();
    }

    // ----- actions -----

    fn install(&mut self, app_id: &str, cx: &mut Context<Self>) {
        self.disarm_confirms();
        // the selection's publisher names the twin: two publishers of
        // one app id install from different artifacts
        let Some(app) = self
            .apps
            .iter()
            .find(|a| {
                a.app_id == app_id
                    && a.publisher.as_deref() == self.selected_publisher.as_deref()
            })
            .cloned()
        else {
            return;
        };
        if self.jobs.contains_key(app_id) || self.refuse_if_busy() {
            return;
        }
        match app.source {
            Source::Reviewed => {
                let cmd = orchard_flatpak::install_ref(&app.app_id, FLATHUB_REMOTE);
                self.jobs
                    .insert(app.app_id.clone(), Job::new(JobState::Installing));
                self.run_job(cmd, &app.app_id, "Install", "flathub", cx);
            }
            Source::Publisher => {
                self.verify_then_install(app, cx);
            }
        }
        cx.notify();
    }

    /// Update an installed app. A Flathub app updates from its origin
    /// remote; a publisher-channel app re-runs the verified install
    /// against the newest release's digest, because the deployed ref
    /// is an immutable digest pin only a fresh verify-and-install can
    /// move. Detection is version-based either way.
    fn update(&mut self, app_id: &str, cx: &mut Context<Self>) {
        self.disarm_confirms();
        if self.jobs.contains_key(app_id) || self.refuse_if_busy() {
            return;
        }
        // the row names the twin: two publishers of one app id update
        // from different artifacts
        let Some(app) = self
            .apps
            .iter()
            .find(|a| {
                a.app_id == app_id
                    && a.publisher.as_deref() == self.selected_publisher.as_deref()
            })
            .cloned()
        else {
            return;
        };
        match app.source {
            Source::Reviewed => {
                self.jobs.insert(app.app_id.clone(), Job::new(JobState::Updating));
                let cmd = orchard_flatpak::update_ref(&app.app_id);
                self.run_job(cmd, &app.app_id, "Update", "flathub", cx);
            }
            Source::Publisher => self.verify_then_install(app, cx),
        }
        cx.notify();
    }

    /// Publisher channel: verify the registry manifest against the pinned
    /// digest, then hand flatpak the image location. The install refuses to
    /// start on digest mismatch.
    fn verify_then_install(&mut self, app: StoreApp, cx: &mut Context<Self>) {
        let Some(location_text) = app.location.clone() else {
            return;
        };
        let location = match oci::Location::parse(&location_text) {
            Ok(location) => location,
            Err(err) => {
                self.fail(format!("bad artifact location: {err:#}"));
                return;
            }
        };
        // the registry details ride along for the artifact download
        let download_source = location.clone();
        // the status line's "from": the registry host the parsed
        // location carries, or nothing for a local layout
        let place = match &location {
            oci::Location::Registry { registry, .. } => registry
                .split('/')
                .next()
                .unwrap_or_default()
                .to_string(),
            oci::Location::OciDir(_) => String::new(),
        };
        self.jobs
            .insert(app.app_id.clone(), Job::new(JobState::Verifying));
        cx.notify();

        cx.spawn(async move |this, cx| {
            let pinned = app.pinned_digest.clone();
            let mut app = app;
            let report = cx
                .background_executor()
                .spawn(async move { oci::verify(&location, pinned.as_deref()) })
                .await;
            // The registry fetch and the install handoff: flatpak's
            // docker:// image path never authenticates (it fetches
            // bare, so token registries like ghcr answer 401), so the
            // client itself downloads the verified artifact into a
            // local OCI layout and hands flatpak that: the zero-setup
            // oci: path. Layouts are content-addressed by manifest
            // digest and land by rename, so an existing directory is
            // a complete one and a retry skips the download.
            let install_arg = match (&report, &download_source) {
                (Ok(report), oci::Location::Registry { .. }) => {
                    let dir = crate::cache::artifact_dir(&app.app_id, &report.digest);
                    if dir.is_dir() {
                        format!("oci:{}", dir.display())
                    } else {
                        let source = download_source.clone();
                        let dir_arg = dir.clone();
                        let digest = report.digest.clone();
                        let downloaded = cx
                            .background_executor()
                            .spawn(async move {
                                oci::download_layout(&source, Some(&digest), &dir_arg)
                            })
                            .await;
                        if let Err(err) = downloaded {
                            let _ = this.update(cx, |this, cx| {
                                this.jobs.remove(&app.app_id);
                                this.fail(format!("download failed for {}: {err:#}", app.name));
                                cx.notify();
                            });
                            return;
                        }
                        format!("oci:{}", dir.display())
                    }
                }
                (Ok(_), oci::Location::OciDir(path)) => format!("oci:{}", path.display()),
                // verify failed: the update below reports it and never
                // reaches an install
                (Err(_), _) => String::new(),
            };
            let verified = report.map(|report| {
                (
                    report.flatpak_ref.clone(),
                    report.digest.clone(),
                    report.metadata.clone(),
                )
            });
            let _ = this.update(cx, |this, cx| {
                match verified {
                    Ok((flatpak_ref, digest, metadata)) => {
                        // a cancel pressed during the verify or the
                        // download: the receipt, no install
                        if this
                            .jobs
                            .get(&app.app_id)
                            .is_some_and(|job| job.cancel_requested)
                        {
                            this.jobs.remove(&app.app_id);
                            this.note("Install cancelled.".to_string());
                            cx.notify();
                            return;
                        }
                        this.verified
                            .insert(app_key(&app.app_id, app.publisher.as_deref()), digest.clone());
                        // Prefer the image's own metadata for the viewer.
                        if app.metadata.is_none() {
                            app.metadata = metadata;
                        }
                        // The catalog ref must match what the image labels
                        // claim; flatpak enforces this again at pull time.
                        let _ = flatpak_ref;
                        let cmd = orchard_flatpak::install_image(&install_arg);
                        // the install phase continues the same job: the
                        // elapsed clock and a cancel pressed mid-verify
                        // carry over
                        let next = this
                            .jobs
                            .get(&app.app_id)
                            .cloned()
                            .unwrap_or_else(|| Job::new(JobState::Installing));
                        this.jobs
                            .insert(app.app_id.clone(), Job { state: JobState::Installing, ..next });
                        // the deployment's key baseline: updates are
                        // compared against this from now on
                        this.record_installed_from(
                            &app.app_id,
                            app.publisher.as_deref(),
                        );
                        this.run_job(cmd, &app.app_id, "Install", &place, cx);
                    }
                    Err(err) => {
                        this.jobs.remove(&app.app_id);
                        this.fail(format!(
                            "verification failed for {}: {err:#}",
                            app.name
                        ));
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Record the publisher key whose artifact is deployed for an
    /// installed app (None clears the record, at uninstall).
    fn record_installed_from(&mut self, app_id: &str, publisher: Option<&str>) {
        match publisher {
            Some(key) => {
                self.continuity
                    .installed_from
                    .insert(app_id.to_string(), key.to_string());
            }
            None => {
                self.continuity.installed_from.remove(app_id);
            }
        }
        save_continuity(&self.continuity);
    }

    /// The publisher key whose artifact is deployed for an installed
    /// app, recorded at install time: the update path compares the
    /// row's key against it to warn about updates from a new key.
    fn installed_from(&self, app_id: &str) -> Option<&str> {
        self.continuity
            .installed_from
            .get(app_id)
            .map(|key| key.as_str())
    }

    fn uninstall(&mut self, app_id: &str, cx: &mut Context<Self>) {
        self.disarm_confirms();
        if self.jobs.contains_key(app_id) || self.refuse_if_busy() {
            return;
        }
        self.record_installed_from(app_id, None);
        // remove from the scope the app deployed to: publisher-channel
        // apps are user installs, reviewed (Flathub) apps system installs
        let scope = if self
            .installed
            .get(app_id)
            .is_some_and(|info| info.user_scope)
        {
            orchard_flatpak::Scope::User
        } else {
            orchard_flatpak::Scope::System
        };
        let cmd = orchard_flatpak::uninstall(scope, app_id);
        self.jobs
            .insert(app_id.to_string(), Job::new(JobState::Removing));
        self.run_job(cmd, app_id, "Remove", "", cx);
    }

    fn open(&mut self, app_id: &str, cx: &mut Context<Self>) {
        if orchard_flatpak::run(app_id).spawn().is_err() {
            self.fail(format!("failed to launch {app_id}"));
            cx.notify();
        }
    }

    /// Run a flatpak command in the background, then refresh installed state.
    /// The one choke point for the status bar's error line: every
    /// message is stamped, and the tick pump retracts it when the
    /// hold lapses. A quiet bar is the resting state.
    fn fail(&mut self, msg: String) {
        self.error = Some(msg);
        self.error_at = Some(Instant::now());
    }

    /// The quiet sibling of [Self::fail]: receipts and refusals,
    /// retracted on the same clock.
    fn note(&mut self, msg: String) {
        self.job_note = Some(msg);
        self.note_at = Some(Instant::now());
    }

    /// Every armed confirm disarms together: starting any action,
    /// closing a page, or Escape lets go of all of them at once.
    fn disarm_confirms(&mut self) {
        self.confirm_install = None;
        self.confirm_remove = None;
        self.confirm_update = None;
        self.confirm_cancel = None;
        self.confirm_clear = None;
    }

    /// The one-operation-at-a-time rule: flatpak serializes the
    /// system installation behind a lock anyway, and a second racer
    /// reads as our failure. Leaves a quiet note naming the running
    /// job instead. True when the press was refused.
    fn refuse_if_busy(&mut self) -> bool {
        if self.jobs.is_empty() {
            return false;
        }
        if let Some((id, job)) = self.jobs.iter().next() {
            let name = self
                .apps
                .iter()
                .find(|a| a.app_id == *id)
                .map(|a| a.name.clone())
                .unwrap_or_else(|| id.clone());
            self.note(format!(
                "One operation at a time: finishing the {} of {name} first.",
                job.state.noun()
            ));
        }
        true
    }

    /// The second press of "Cancel?": flag the job and interrupt the
    /// child the way a terminal's Ctrl-C would (SIGINT, so flatpak
    /// unwinds its own transaction). The runner's reap turns the
    /// flag into the quiet receipt; mid-verify, the flag alone
    /// suffices, the verify task checks it before installing.
    fn cancel_job(&mut self, app_id: &str, cx: &mut Context<Self>) {
        self.confirm_cancel = None;
        let Some(job) = self.jobs.get_mut(app_id) else {
            return;
        };
        job.cancel_requested = true;
        if let Some(pid) = job.cancel_pid {
            // the terminal's own grammar: Ctrl-C is SIGINT, and
            // flatpak unwinds its transaction on it
            unsafe { libc::kill(pid as i32, libc::SIGINT) };
        }
        cx.notify();
    }

    /// Run a flatpak child with live progress. The probe's finding
    /// drives the shape: flatpak prints its percent lines to stdout
    /// when piped and its errors to stderr, so the runner streams
    /// both, parses the stdout lines into the job's progress, and
    /// keeps the child pid where a cancel press can find it. The
    /// pipes close when the child exits, so reaping after both
    /// readers reach EOF is order-safe.
    fn run_job(
        &mut self,
        mut cmd: std::process::Command,
        app_id: &str,
        label: &'static str,
        place: &str,
        cx: &mut Context<Self>,
    ) {
        let app_id = app_id.to_string();
        let label = label.to_string();
        let place = place.to_string();
        self.job_note = None;
        cx.spawn(async move |this, cx| {
            use std::io::{BufRead, Read};
            use std::process::Stdio;
            let prepared = cx
                .background_executor()
                .spawn(async move {
                    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
                    cmd.spawn().map(|mut child| {
                        let pid = child.id();
                        let stdout = child.stdout.take();
                        let stderr = child.stderr.take();
                        (child, pid, stdout, stderr)
                    })
                })
                .await;
            let (mut child, pid, stdout, stderr) = match prepared {
                Ok(ok) => ok,
                Err(err) => {
                    let _ = this.update(cx, |this, cx| {
                        this.fail(format!("{label}: {err}"));
                        this.jobs.remove(&app_id);
                        this.refresh_installed(cx);
                        cx.notify();
                    });
                    return;
                }
            };
            let _ = this.update(cx, |this, _| {
                if let Some(job) = this.jobs.get_mut(&app_id) {
                    job.cancel_pid = Some(pid);
                }
            });

            // two plain reader threads into one channel: stdout lines
            // (the progress), one final stderr text (the failure)
            let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Result<String, String>>();
            let out_tx = tx.clone();
            std::thread::spawn(move || {
                if let Some(mut out) = stdout {
                    let mut reader = std::io::BufReader::new(&mut out);
                    let mut line = String::new();
                    loop {
                        line.clear();
                        match reader.read_line(&mut line) {
                            Ok(0) | Err(_) => break,
                            Ok(_) => {
                                if out_tx
                                    .send(Ok(line.trim_end().to_string()))
                                    .is_err()
                                {
                                    break;
                                }
                            }
                        }
                    }
                }
            });
            std::thread::spawn(move || {
                let mut text = String::new();
                if let Some(mut err) = stderr {
                    let _ = err.read_to_string(&mut text);
                }
                let _ = tx.send(Err(text));
            });

            let mut stderr_text = String::new();
            while let Some(event) = rx.recv().await {
                match event {
                    Ok(line) => {
                        if let Some((stage_label, percent)) = parse_progress_line(&line) {
                            let stage = stage_text(&stage_label, &place);
                            let _ = this.update(cx, |this, cx| {
                                if let Some(job) = this.jobs.get_mut(&app_id) {
                                    job.progress = Some(JobProgress { stage, percent });
                                    cx.notify();
                                }
                            });
                        }
                    }
                    Err(text) => stderr_text = text,
                }
            }
            let status = cx
                .background_executor()
                .spawn(async move { child.wait() })
                .await;
            // the outcome, not the exit code, is the truth: a fresh
            // installation list is fetched before a failed job is
            // believed
            let installed = cx
                .background_executor()
                .spawn(async move { list_installed() })
                .await;
            let _ = this.update(cx, |this, cx| {
                let cancelled = this
                    .jobs
                    .get(&app_id)
                    .is_some_and(|job| job.cancel_requested);
                this.jobs.remove(&app_id);
                if cancelled {
                    // the receipt, not a failure: the interrupt was
                    // asked for
                    this.note(format!("{label} cancelled."));
                    this.error = None;
                    this.error_at = None;
                } else {
                    match status.map(|s| s.success()) {
                        Ok(true) => {
                            this.error = None;
                            this.error_at = None;
                        }
                        Ok(false) => {
                            // flatpak's cleanup can error after the
                            // job's work is already done (observed:
                            // "Uninstalling: directory not empty" on a
                            // deployment already gone), so the fresh
                            // list decides whether the job did its work
                            let done = match label.as_str() {
                                "Remove" => !installed.contains_key(&app_id),
                                "Update" => this
                                    .apps
                                    .iter()
                                    .find(|a| a.app_id == app_id)
                                    .is_some_and(|app| {
                                        installed
                                            .get(&app_id)
                                            .is_some_and(|info| {
                                                !update_available(app, info)
                                            })
                                    }),
                                _ => false,
                            };
                            if done {
                                this.note(format!(
                                    "{label}d, though flatpak reported a cleanup warning."
                                ));
                                this.error = None;
                                this.error_at = None;
                            } else {
                                this.fail(format!(
                                    "{label} failed: {}",
                                    stderr_text.trim()
                                ));
                            }
                        }
                        Err(err) => this.fail(format!("{label}: {err}")),
                    }
                }
                this.installed = installed;
                cx.notify();
            });
        })
        .detach();
    }

    // ----- keyboard -----

    /// The character a keystroke produces: gpui reports the key
    /// label printed on the key (unshifted, so shift-a's label is
    /// "a"), while key_char carries the character the layout and
    /// modifiers actually produce. Text insertion wants the latter;
    /// the label is only the fallback for keys that make none.
    fn produced_char(keystroke: &gpui::Keystroke) -> String {
        keystroke
            .key_char
            .as_ref()
            .filter(|char| char.chars().count() == 1)
            .cloned()
            .unwrap_or_else(|| keystroke.key.clone())
    }

    fn key_down(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let keystroke = &event.keystroke;
        let key = keystroke.key.as_str();
        let shift = keystroke.modifiers.shift;
        let ctrl = keystroke.modifiers.control || keystroke.modifiers.platform;

        // ----- modals outrank menus: an open modal swallows
        // everything but Escape; a click away closes it through the
        // backdrop -----
        if self.about_modal {
            if key == "escape" {
                self.about_modal = false;
                cx.notify();
            }
            return;
        }

        // ----- sign-in modal: QR phase swallows keys (Escape closes);
        // the name question takes the shared input like the composer -----
        if self.signin_modal {
            if self.pending_reviewer.is_none() {
                if key == "escape" {
                    self.close_signin();
                    cx.notify();
                }
                return;
            }
            match key {
                "escape" => {
                    self.close_signin();
                }
                "enter" => self.finish_sign_in(cx),
                "left" | "right" | "home" | "end" | "backspace" | "delete" => {
                    self.move_draft_cursor(key, shift)
                }
                "a" if ctrl => {
                    self.review_anchor = Some(0);
                    self.review_cursor = self.review_draft.1.len();
                }
                "v" if ctrl => {
                    if let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) {
                        let pasted: String = text.split_whitespace().collect();
                        self.insert_draft_text(&pasted);
                    }
                }
                "space" => self.insert_draft_text(" "),
                other if other.chars().count() == 1 && !ctrl => {
                    self.insert_draft_text(&Self::produced_char(keystroke));
                }
                _ => {}
            }
            cx.notify();
            return;
        }

        // ----- review compose: keystrokes go to the draft, not the
        // search box, and typing does not close the page -----
        if self.composing {
            match key {
                "escape" => {
                    // compose first, page second: a stray Escape does
                    // not throw the reader out of the app
                    self.composing = false;
                }"enter" => {
                    if !self.review_posting {
                        let app_id = self.selected.clone().unwrap_or_default();
                        self.post_review(&app_id, cx);
                    }
                }
                // digits 1 to 5 set the star rating, discoverable and
                // one keypress each; shifted they are text (!, @)
                "1" | "2" | "3" | "4" | "5" if !shift => {
                    self.review_draft.0 = Some(key.parse::<u8>().unwrap() as f32);
                    self.review_error = None;
                }
                "left" | "right" | "home" | "end" | "backspace" | "delete" => {
                    self.move_draft_cursor(key, shift);
                }
                "space" => self.insert_draft_text(" "),
                other if other.chars().count() == 1 && !ctrl => {
                    self.insert_draft_text(&Self::produced_char(keystroke));
                }
                _ => {}
            }
            cx.notify();
            return;
        }

        // r starts a review from the page, before the type-to-close
        // rule eats the keystroke
        if self.page_open && key == "r" && !ctrl {
            self.start_review(cx);
            cx.notify();
            return;
        }

        // v flips the review lens while the section is expanded
        if self.page_open && key == "v" && !ctrl && self.reviews_expanded {
            self.reviews_everyone = !self.reviews_everyone;
            cx.notify();
            return;
        }

        // p opens the open app's publisher page, like the "Published
        // by" row: everything one key publishes. Falls through to the
        // type-to-close rule when the page has no publisher key (a
        // reviewed app), and yields to it when a collection page is
        // already up.
        if self.page_open && key == "p" && !ctrl && self.collection_view.is_none()
            && let Some(pubkey) = self.selected_publisher.clone().filter(|k| !k.is_empty())
        {
            self.open_collection(CollectionPage::Publisher(pubkey), cx);
            cx.notify();
            return;
        }

        // ----- dropdown menus: an open menu swallows everything but
        // Escape; a click away closes it through the overlay -----
        if self.channel_menu.is_some()
            || self.account_menu.is_some()
            || self.sort_menu.is_some()
        {
            if key == "escape" {
                self.channel_menu = None;
                self.account_menu = None;
                self.sort_menu = None;
                cx.notify();
            }
            return;
        }

        // ----- settings page: the relay draft is the text surface;
        // Escape backs out to the list -----
        if self.settings_open {
            match key {
                "escape" => self.close_settings(cx),
                "left" | "right" | "home" | "end" | "backspace" | "delete" => {
                    self.move_relay_cursor(key, shift);
                }
                "a" if ctrl => {
                    self.relay_anchor = Some(0);
                    self.relay_cursor = self.relay_draft.len();
                }
                "c" if ctrl => {
                    let (start, end) = self.relay_selection();
                    if start != end {
                        cx.write_to_clipboard(ClipboardItem::new_string(
                            self.relay_draft[start..end].to_string(),
                        ));
                    }
                }
                "x" if ctrl => {
                    let (start, end) = self.relay_selection();
                    if start != end {
                        cx.write_to_clipboard(ClipboardItem::new_string(
                            self.relay_draft[start..end].to_string(),
                        ));
                        self.insert_relay_text("");
                    }
                }
                "v" if ctrl => {
                    if let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) {
                        // a pasted URL may carry whitespace or a newline
                        let pasted: String = text.split_whitespace().collect();
                        self.insert_relay_text(&pasted);
                    }
                }
                "enter" => self.add_relay_from_draft(cx),
                "space" => self.insert_relay_text(" "),
                other if other.chars().count() == 1 && !ctrl => {
                    self.insert_relay_text(&Self::produced_char(keystroke));
                }
                _ => {}
            }
            cx.notify();
            return;
        }

        match key {
            // ----- search editing: the input is always live -----
            "left" => self.move_cursor(false, shift),
            "right" => self.move_cursor(true, shift),
            "home" => {
                self.cursor = 0;
                if !shift {
                    self.anchor = None;
                }
            }
            "end" => {
                self.cursor = self.search.len();
                if !shift {
                    self.anchor = None;
                }
            }
            "a" if ctrl => {
                self.anchor = Some(0);
                self.cursor = self.search.len();
            }
            "c" if ctrl => {
                let (start, end) = self.selection();
                if start != end {
                    cx.write_to_clipboard(ClipboardItem::new_string(
                        self.search[start..end].to_string(),
                    ));
                }
            }
            "x" if ctrl => {
                let (start, end) = self.selection();
                if start != end {
                    cx.write_to_clipboard(ClipboardItem::new_string(
                        self.search[start..end].to_string(),
                    ));
                    self.delete_text(false);
                }
            }
            "v" if ctrl => {
                if let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) {
                    if self.page_open {
                        self.close_page(cx);
                    }
                    self.insert_text(&text.replace(['\n', '\r'], " "));
                }
            }
            "backspace" => {
                // the browser idiom: backspace backs out of a page
                // only when the field is empty; with text present it
                // is editing, not navigation
                if self.page_open && self.search.is_empty() {
                    self.close_page(cx);
                }
                self.delete_text(true);
            }
            "delete" => {
                // forward-delete edits; it never navigates
                self.delete_text(false);
            }
            "escape" => {
                // back out one level: lightbox, page, then selection,
                // then text, then row selection
                self.disarm_confirms();
                if self.lightbox.is_some() {
                    self.lightbox = None;
                } else if self.collection_view.is_some() {
                    // one level back: the app page underneath (if any)
                    // is still mounted
                    self.collection_view = None;
                } else if self.page_open {
                    self.close_page(cx);
                } else if self.anchor.is_some() {
                    self.anchor = None;
                } else if !self.search.is_empty() {
                    self.search.clear();
                    self.cursor = 0;
                } else if self.selected.is_some() {
                    self.selected = None;
                    self.selected_publisher = None;
                }
            }
            "up" | "down" => {
                if self.page_open {
                    self.close_page(cx);
                }
                let rows = self.filtered();
                if rows.is_empty() {
                    return;
                }
                let current = self
                    .selected
                    .as_deref()
                    .and_then(|id| {
                        // prefer the selected twin's own row; a publisher
                        // may be filtered out entirely, so fall back to
                        // the first row of the id
                        rows.iter()
                            .position(|&ix| {
                                let app = &self.apps[ix];
                                app.app_id == id
                                    && self.selected_publisher.as_deref()
                                        == app.publisher.as_deref()
                            })
                            .or_else(|| {
                                rows.iter()
                                    .position(|&ix| self.apps[ix].app_id == id)
                            })
                    });
                let next = match (current, key) {
                    (Some(ix), "down") => (ix + 1).min(rows.len() - 1),
                    (Some(ix), "up") => ix.saturating_sub(1),
                    (None, "down") => 0,
                    (None, "up") => rows.len() - 1,
                    _ => return,
                };
                let app_id = self.apps[rows[next]].app_id.clone();
                self.set_selected(&app_id, cx);
                self.reveal_row(next, window);
            }
            "enter" => {
                // install stays a click action; Enter only opens pages
                if self.page_open {
                    return;
                }
                let target = match &self.selected {
                    Some(id) => Some(id.clone()),
                    None => self
                        .filtered()
                        .first()
                        .map(|&first| self.apps[first].app_id.clone()),
                };
                if let Some(app_id) = target {
                    self.open_page(&app_id, cx);
                }
            }
            "space" => {
                // typing means back to the list, from either level
                self.collection_view = None;
                if self.page_open {
                    self.close_page(cx);
                }
                self.insert_text(" ");
            }
            other if other.chars().count() == 1 && !ctrl => {
                self.collection_view = None;
                if self.page_open {
                    self.close_page(cx);
                }
                self.insert_text(&Self::produced_char(keystroke));
            }
            _ => return,
        }
        cx.notify();
    }

    /// Insert text at the cursor, replacing any selection.
    fn insert_text(&mut self, text: &str) {
        let (start, end) = self.selection();
        self.search.replace_range(start..end, text);
        self.cursor = start + text.len();
        self.anchor = None;
    }

    /// Delete the selection, or one char before the cursor
    /// (`backspace`) / after it (`delete`).
    fn delete_text(&mut self, backspace: bool) {
        let (start, end) = self.selection();
        if start != end {
            self.search.replace_range(start..end, "");
            self.cursor = start;
            self.anchor = None;
            return;
        }
        if backspace && start > 0 {
            let prev = self.search[..start]
                .chars()
                .next_back()
                .map_or(0, |c| c.len_utf8());
            self.search.replace_range(start - prev..start, "");
            self.cursor = start - prev;
        } else if !backspace && end < self.search.len() {
            let next = self.search[end..].chars().next().map_or(0, |c| c.len_utf8());
            self.search.replace_range(end..end + next, "");
        }
        self.anchor = None;
    }

    /// Move the cursor one char, extending the selection with shift.
    fn move_cursor(&mut self, right: bool, extend: bool) {
        self.cursor = if right {
            self.search[self.cursor..]
                .chars()
                .next()
                .map_or(self.cursor, |c| self.cursor + c.len_utf8())
        } else {
            self.search[..self.cursor]
                .chars()
                .next_back()
                .map_or(0, |c| self.cursor - c.len_utf8())
        };
        if !extend {
            self.anchor = None;
        }
    }

    /// Draft-review text edits: the same model as the search box,
    /// pointed at the review draft instead. `key` is a gpui key name.
    fn insert_draft_text(&mut self, text: &str) {
        let (start, end) = self.draft_selection();
        self.review_draft.1.replace_range(start..end, text);
        self.review_cursor = start + text.len();
        self.review_anchor = None;
    }

    fn delete_draft_text(&mut self, backspace: bool) {
        let (start, end) = self.draft_selection();
        let body = &mut self.review_draft.1;
        if start != end {
            body.replace_range(start..end, "");
            self.review_cursor = start;
            self.review_anchor = None;
            return;
        }
        if backspace && start > 0 {
            let prev = body[..start]
                .chars()
                .next_back()
                .map_or(0, |c| c.len_utf8());
            body.replace_range(start - prev..start, "");
            self.review_cursor = start - prev;
        } else if !backspace && end < body.len() {
            let next = body[end..].chars().next().map_or(0, |c| c.len_utf8());
            body.replace_range(end..end + next, "");
        }
        self.review_anchor = None;
    }

    fn move_draft_cursor(&mut self, key: &str, extend: bool) {
        if key == "backspace" || key == "delete" {
            self.delete_draft_text(key == "backspace");
            return;
        }
        let body = &self.review_draft.1;
        self.review_cursor = match key {
            "left" => body[..self.review_cursor]
                .chars()
                .next_back()
                .map_or(0, |c| self.review_cursor - c.len_utf8()),
            "right" => body[self.review_cursor..]
                .chars()
                .next()
                .map_or(self.review_cursor, |c| self.review_cursor + c.len_utf8()),
            "home" => 0,
            "end" => body.len(),
            _ => return,
        };
        if !extend {
            self.review_anchor = None;
        }
    }

    /// The draft's selected byte range, ordered; `(cursor, cursor)`
    /// when empty.
    fn draft_selection(&self) -> (usize, usize) {
        match self.review_anchor {
            Some(anchor) if anchor != self.review_cursor => {
                (anchor.min(self.review_cursor), anchor.max(self.review_cursor))
            }
            _ => (self.review_cursor, self.review_cursor),
        }
    }

    /// The selected byte range, ordered. `(cursor, cursor)` when empty.
    fn selection(&self) -> (usize, usize) {
        match self.anchor {
            Some(anchor) => (anchor.min(self.cursor), anchor.max(self.cursor)),
            None => (self.cursor, self.cursor),
        }
    }

    /// Scroll the virtualized list so row `ix` is visible (keyboard
    /// navigation). uniform_list 0.2.2 has no nearest strategy, so
    /// pick top or bottom from the current offset.
    fn reveal_row(&self, ix: usize, window: &Window) {
        let state = self.list_handle.0.borrow();
        let item_h = state
            .last_item_size
            .map(|size| f32::from(size.item.height))
            .unwrap_or(ROW_HEIGHT);
        let first_visible = -f32::from(state.base_handle.offset().y) / item_h;
        drop(state);
        let visible = (f32::from(window.viewport_size().height) / item_h).max(1.0);
        if (ix as f32) < first_visible {
            self.list_handle.scroll_to_item(ix, ScrollStrategy::Top);
        } else if (ix as f32) >= first_visible + visible {
            self.list_handle.scroll_to_item(ix, ScrollStrategy::Bottom);
        }
    }

    // ----- rendering -----

    /// The browse surface: category chip row over the virtualized
    /// list, with skeleton rows while the catalog loads and an
    /// actionable empty state otherwise.
    fn render_browse(&self, cx: &mut Context<Self>) -> AnyElement {
        let rows = self.filtered();
        // the composition line: what this view is made of, quiet
        // facts only, between the chips and the rows. Computed before
        // the list closure moves the rows.
        let composition = (!rows.is_empty()).then(|| {
            let mut publishers = std::collections::HashSet::new();
            let mut verified = 0;
            let mut circle = 0;
            for &i in &rows {
                let app = &self.apps[i];
                if let Some(key) = app.publisher.as_deref() {
                    publishers.insert(key);
                    if self.follows.contains(key) {
                        circle += 1;
                    }
                }
                if self.row_verified(app) {
                    verified += 1;
                }
            }
            div()
                .px_4()
                .py_1()
                .bg(ui::inset())
                .text_size(px(ui::TEXT_SIZE_LABEL))
                .text_color(ui::text_dim())
                .child(composition_line(
                    rows.len(),
                    publishers.len(),
                    verified,
                    circle,
                ))
        });
        let body: AnyElement = if !rows.is_empty() {
            let view = cx.entity();
            uniform_list("app-list", rows.len(), move |range, window, cx| {
                view.update(cx, |root, cx| root.render_rows(&rows, range, window, cx))
            })
            .track_scroll(self.list_handle.clone())
            .flex_1()
            .min_w_0()
            .bg(ui::inset())
            .into_any_element()
        } else if self.flathub_loading {
            self.render_skeleton()
        } else if self.filter == Filter::Installed {
            div()
                .flex_1()
                .min_w_0()
                .flex()
                .flex_col()
                .items_center()
                .justify_center()
                .gap_2()
                .bg(ui::inset())
                .child(ui::icon("icons/search.svg", 20., ui::text_dim()))
                .child(
                    div()
                        .text_size(px(ui::TEXT_SIZE_EMPHASIS))
                        .text_color(ui::text())
                        .child("Nothing installed yet"),
                )
                .child(
                    div()
                        .text_size(px(ui::TEXT_SIZE))
                        .text_color(ui::text_dim())
                        .child("Apps you install will show up here."),
                )
                .into_any_element()
        } else {
            div()
                .flex_1()
                .min_w_0()
                .flex()
                .flex_col()
                .items_center()
                .justify_center()
                .gap_2()
                .bg(ui::inset())
                .child(ui::icon("icons/search.svg", 20., ui::text_dim()))
                .child(
                    div()
                        .text_size(px(ui::TEXT_SIZE_EMPHASIS))
                        .text_color(ui::text())
                        .child("No apps match"),
                )
                .child(
                    div()
                        .text_size(px(ui::TEXT_SIZE))
                        .text_color(ui::text_dim())
                        .child(format!(
                            "Nothing matches {:?}. Escape clears the search.",
                            self.search
                        )),
                )
                .into_any_element()
        };

        div()
            .flex_1()
            .min_h_0()
            .flex()
            .flex_col()
            .child(self.render_category_bar(cx))
            .children(composition)
            .child(body)
            .into_any_element()
    }

    /// The category chip row: the browse dimension. Only categories
    /// present in the catalog get a chip.
    fn render_category_bar(&self, cx: &mut Context<Self>) -> AnyElement {
        div()
            .flex()
            .flex_wrap()
            .items_center()
            .gap_1()
            .px_4()
            .py_1p5()
            .border_b_1()
            .border_color(ui::divider())
            .bg(ui::panel())
            .child(self.category_chip("All categories", None, cx))
            .children(self.category_counts().into_iter().map(|(name, _)| {
                self.category_chip(name, Some(name.to_string()), cx)
            }))
            // the sort picker: the list order is a chosen option,
            // persisted like the other view preferences
            .child(div().flex_1().min_w(px(8.)))
            .child(
                div()
                    .id("sort-trigger")
                    .flex()
                    .items_center()
                    .gap_1()
                    .px_2()
                    .py_1()
                    .rounded_sm()
                    .cursor_pointer()
                    .text_size(px(ui::TEXT_SIZE))
                    .text_color(ui::text())
                    .hover(|style| style.bg(ui::surface()))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, event: &MouseDownEvent, _w, cx| {
                            let open = this.sort_menu.is_some();
                            this.channel_menu = None;
                            this.account_menu = None;
                            this.about_modal = false;
                            this.sort_menu = if open { None } else { Some(event.position) };
                            cx.stop_propagation();
                            cx.notify();
                        }),
                    )
                    .child(format!(
                        "Sort: {}",
                        ListSort::from_settings(&self.settings.get().sort).label()
                    ))
                    .child(ui::icon("icons/chevron_down.svg", 11., ui::text_dim())),
            )
            .into_any_element()
    }

    fn category_chip(&self, label: &str, value: Option<String>, cx: &mut Context<Self>) -> AnyElement {
        let active = self.category == value;
        div()
            .id(SharedString::from(format!("cat-{label}")))
            .px_2()
            .py_0p5()
            .rounded_sm()
            .text_size(px(ui::TEXT_SIZE_SMALL))
            .cursor_pointer()
            .when(active, |chip| {
                chip.bg(ui::accent()).text_color(ui::accent_fg())
            })
            .when(!active, |chip| {
                chip.text_color(ui::text())
                    .hover(|style| style.bg(ui::surface()).text_color(ui::text()))
            })
            .on_click(cx.listener(move |this, _e: &ClickEvent, _w, cx| {
                this.category = value.clone();
                cx.notify();
            }))
            .child(label.to_string())
            .into_any_element()
    }

    /// Counts per browse category across the whole catalog, in chip
    /// order.
    fn category_counts(&self) -> Vec<(&'static str, usize)> {
        BROWSE_CATEGORIES
            .into_iter()
            .map(|name| {
                let count = self
                    .apps
                    .iter()
                    .filter(|app| app.categories.iter().any(|c| c == name))
                    .count();
                (name, count)
            })
            .filter(|(_, count)| *count > 0)
            .collect()
    }

    /// Dim placeholder rows while the catalog loads: same rhythm as
    /// real rows so the swap does not jump.
    fn render_skeleton(&self) -> AnyElement {
        div()
            .flex_1()
            .min_w_0()
            .flex()
            .flex_col()
            .bg(ui::inset())
            .children((0..12).map(|_| {
                div()
                    .h(px(ROW_HEIGHT))
                    .w_full()
                    .flex()
                    .items_center()
                    .gap_3()
                    .px_4()
                    .border_b_1()
                    .border_color(ui::divider_soft())
                    .child(div().w(px(40.)).h(px(40.)).rounded_full().bg(ui::surface()))
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap_1p5()
                            .child(div().w(px(180.)).h(px(10.)).rounded_sm().bg(ui::surface()))
                            .child(div().w(px(280.)).h(px(8.)).rounded_sm().bg(ui::surface())),
                    )
            }))
            .with_animation(
                "skeleton",
                gpui::Animation::new(Duration::from_millis(1400)).repeat(),
                |rows, delta| rows.opacity(0.55 + 0.35 * delta),
            )
            .into_any_element()
    }

    /// Build one screenful of rows for the virtualized list.
    fn render_rows(
        &mut self,
        rows: &[usize],
        range: Range<usize>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        // the visible rows' remote icons ride the shared cache: the
        // list never fetched them (only open_page did), so publisher
        // rows wore initials until their page was opened once
        let row_icon_urls: Vec<String> = range
            .clone()
            .filter_map(|ix| {
                rows.get(ix)
                    .and_then(|&app_ix| self.apps[app_ix].icon_url.clone())
            })
            .filter(|url| !self.fetched.contains_key(url))
            .collect();
        self.ensure_fetched(row_icon_urls, cx);
        // the visible rows' digest checks ride the same lazy pattern:
        // rows entering the list earn the row badge the way a page
        // open does, so the badge is not a page-visit souvenir
        if self.filter != Filter::Publisher {
            let digest_jobs: Vec<(String, Option<String>, Option<String>, String, Option<String>)> =
                range
                    .clone()
                    .filter_map(|ix| {
                        let app = &self.apps[*rows.get(ix)?];
                        if app.source != Source::Publisher {
                            return None;
                        }
                        Some((
                            app_key(&app.app_id, app.publisher.as_deref()),
                            app.location.clone(),
                            app.pinned_digest.clone(),
                            app.app_id.clone(),
                            app.publisher.clone(),
                        ))
                    })
                    .collect();
            for (key, location, pinned, app_id, publisher) in digest_jobs {
                self.ensure_row_digest(&key, location, pinned, &app_id, publisher, cx);
            }
        }
        let selected = self.selected.as_deref();
        range
            .filter_map(|ix| rows.get(ix))
            .map(|&app_ix| {
                let app = &self.apps[app_ix];
                let app_id = app.app_id.clone();
                let action_id = app_id.clone();
                let row_publisher = app.publisher.clone();
                // the row's bookkeeping key: two publishers of one app
                // id keep separate digests
                let row_key = app_key(&app.app_id, row_publisher.as_deref());
                // unique element ids: twin rows share the app id, and
                // duplicate ids collapse hover and state
                let row_id = format!(
                    "app-row-{app_id}-{}",
                    row_publisher.as_deref().unwrap_or("")
                );
                let digest_id = format!("{row_id}-digest");
                let is_selected = selected == Some(app.app_id.as_str())
                    && self.selected_publisher.as_deref() == app.publisher.as_deref();
                let is_installed = self.installed.contains_key(&app.app_id);
                let outdated = self
                    .installed
                    .get(app.app_id.as_str())
                    .is_some_and(|info| update_available(app, info));
                let job = self.jobs.get(&app.app_id).map(|job| job.label().to_string());
                let verified_digest = self.verified.get(&row_key).cloned();
                let row_update_id = app_id.clone();
                // one publisher clone per closure: two move closures
                // cannot share the original
                let click_publisher = row_publisher.clone();
                let install_publisher = row_publisher.clone();
                let row_repo_ok = app.source == Source::Publisher
                    && self.repo_verified(
                        app.repository.as_deref().unwrap_or(""),
                        app.publisher.as_deref(),
                    );

                div()
                    .id(SharedString::from(row_id))
                    .h(px(ROW_HEIGHT))
                    .w_full()
                    .flex()
                    .items_center()
                    .gap_3()
                    .px_4()
                    .border_b_1()
                    .border_color(ui::divider_soft())
                    .cursor_pointer()
                    .hover(|style| style.bg(ui::surface()))
                    .when(is_selected, |row| row.bg(ui::row_selected()))
                    .on_click(cx.listener(move |this, _event: &ClickEvent, _window, cx| {
                        // a twin row names its publisher: the page is
                        // about THIS row, not the first same-id row
                        this.selected_publisher = click_publisher.clone();
                        this.open_page(&app_id, cx);
                    }))                    .child(app_icon(
                        app.icon_path
                            .as_deref()
                            .or_else(|| {
                                app.icon_url.as_deref().and_then(|url| {
                                    self.fetched.get(url).and_then(|f| f.as_deref())
                                })
                            }),
                        &app.name,
                        40.,
                    ))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .gap_0p5()
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap_2()
                                    .child(
                                        div()
                                            .text_size(px(ui::TEXT_SIZE_EMPHASIS))
                                            .font_weight(gpui::FontWeight::MEDIUM)
                                            .text_color(ui::text())
                                            .overflow_hidden()
                                            .whitespace_nowrap()
                                            .child(app.name.clone()),
                                    )
                                    .child(match app.source {
                                        Source::Reviewed => badge("Flathub", ui::text_dim()),
                                        Source::Publisher => badge("Orchard", ui::accent()),
                                    })
                                    // digest ok: this client has checked
                                    // the artifact against the signed
                                    // pin. Hidden in the Orchard view:
                                    // every row there is digest-pinned
                                    // by construction, so a badge on the
                                    // visited subset implies the rest
                                    // are something less. In All and
                                    // Installed the presence is real
                                    // information. The shield is quiet
                                    // like the repo check; the words
                                    // live on hover and on the page.
                                    .when(
                                        verified_digest.is_some()
                                            && self.filter != Filter::Publisher,
                                        |row| {
                                            row.child(
                                                div()
                                                    .id(SharedString::from(digest_id))
                                                    .hover(|style| {
                                                        style.bg(gpui::transparent_black())
                                                    })
                                                    .tooltip(|_window, cx| {
                                                        cx.new(|_| {
                                                            TextTooltip(
                                                                "Artifact matches the signed digest"
                                                                    .into(),
                                                            )
                                                        })
                                                        .into()
                                                    })
                                                    .child(ui::icon(
                                                        "icons/shield.svg",
                                                        13.,
                                                        ui::risk_color(orchard_core::Risk::Low),
                                                    )),
                                            )
                                        },
                                    )
                                    // repo-verified: the forge hosts
                                    // this publisher key (quiet by
                                    // default, details on the page)
                                    .when(row_repo_ok, |row| {
                                        row.child(ui::icon(
                                            "icons/check.svg",
                                            13.,
                                            ui::risk_color(orchard_core::Risk::Low),
                                        ))
                                    }),
                            )
                            .child(
                                div()
                                    .text_size(px(ui::TEXT_SIZE))
                                    .text_color(ui::text_dim())
                                    .overflow_hidden()
                                    .whitespace_nowrap()
                                    // a second publisher of the same app
                                    // id is a second row: name them in
                                    // the list, quiet by default
                                    .child(match app.source {
                                        Source::Publisher => {
                                            let who = app
                                                .publisher
                                                .as_deref()
                                                .and_then(|pubkey| {
                                                    self.profiles.get(pubkey)
                                                })
                                                .and_then(|p| p.name.clone())
                                                .unwrap_or_else(|| {
                                                    short_npub(
                                                        app.publisher.as_deref().unwrap_or(""),
                                                    )
                                                });
                                            // the name links to the
                                            // publisher page straight
                                            // from the list; the stop
                                            // keeps the row's own open
                                            // from firing, same rules
                                            // as the row buttons
                                            let key = app.publisher.clone().unwrap_or_default();
                                            div()
                                                .flex()
                                                .gap_1()
                                                .min_w_0()
                                                .overflow_hidden()
                                                .whitespace_nowrap()
                                                .child(
                                                    div()
                                                        .id(SharedString::from(format!(
                                                            "row-by-{action_id}-{key}"
                                                        )))
                                                        .cursor_pointer()
                                                        .hover(|style| {
                                                            style.text_color(ui::text())
                                                        })
                                                        .on_click(cx.listener(
                                                            move |this, _e: &ClickEvent, _w, cx| {
                                                                cx.stop_propagation();
                                                                this.open_collection(
                                                                    CollectionPage::Publisher(
                                                                        key.clone(),
                                                                    ),
                                                                    cx,
                                                                );
                                                            },
                                                        ))
                                                        .child(format!("by {who}")),
                                                )
                                                .child("\u{b7}")
                                                .child(
                                                    div()
                                                        .overflow_hidden()
                                                        .whitespace_nowrap()
                                                        .child(app.summary.clone()),
                                                )
                                        }
                                        Source::Reviewed => div()
                                            .overflow_hidden()
                                            .whitespace_nowrap()
                                            .child(app.summary.clone()),
                                    }),
                            ),
                    )
                    .child(match job {
                        Some(label) => badge(
                            &label,
                            ui::risk_color(orchard_core::Risk::Medium),
                        )
                        .into_any_element(),
                        None if is_installed => {
                            // remove is not on the card: a destructive
                            // action belongs on the app page, not
                            // beside the Open it can mis-arm
                            // the newest release is signed by a key
                            // other than the deployed artifact's: the
                            // update arms amber and the badge says so
                            let key_changed = self
                                .installed_from(action_id.as_str())
                                .is_some_and(|from| {
                                    Some(from) != row_publisher.as_deref()
                                });
                            let update_confirming = self.confirm_update.as_deref()
                                == Some(action_id.as_str());
                            div()
                                .flex()
                                .items_center()
                                .gap_2()
                                // no "Installed" badge: the actions
                                // (Open, Remove) already say it, and
                                // the Installed page says it again;
                                // only something to do earns a badge
                                .when(outdated, |slot| {
                                    slot.child(if key_changed {
                                        badge(
                                            "Update from a new publisher key",
                                            ui::risk_color(orchard_core::Risk::Medium),
                                        )
                                    } else {
                                        badge(
                                            "Update available",
                                            ui::risk_color(orchard_core::Risk::Medium),
                                        )
                                    })
                                })
                                .child(ui::small_button(
                                    format!("row-open-{action_id}-{}", row_publisher.as_deref().unwrap_or("")),
                                    Some("icons/play.svg"),
                                    "Open",
                                    Tone::Accent,
                                )
                                .on_click(cx.listener(move |this, _e: &ClickEvent, _w, cx| {
                                    cx.stop_propagation();
                                    this.open(action_id.as_str(), cx);
                                })))
                                .when(outdated, |slot| {
                                    let update_id = row_update_id.clone();
                                    let update_publisher = row_publisher.clone();
                                    slot.child(
                                        ui::small_button(
                                            format!(
                                                "row-update-{update_id}-{}",
                                                row_publisher.as_deref().unwrap_or("")
                                            ),
                                            Some("icons/update.svg"),
                                            if update_confirming { "Sure?" } else { "Update" },
                                            if key_changed { Tone::Warn } else { Tone::Quiet },
                                        )
                                        .on_click(cx.listener(
                                            move |this, _e: &ClickEvent, _w, cx| {
                                                cx.stop_propagation();
                                                this.selected_publisher =
                                                    update_publisher.clone();
                                                if update_confirming {
                                                    this.update(update_id.as_str(), cx);
                                                } else {
                                                    this.confirm_update =
                                                        Some(update_id.clone());
                                                    cx.notify();
                                                }
                                            },
                                        )),
                                    )
                                })
                                .into_any_element()
                        }
                        None if app.version.is_none() && app.source == Source::Publisher => {
                            // the chain exists but nothing built for
                            // this machine: the honest note replaces
                            // an install that cannot work
                            div()
                                .flex_none()
                                .child(badge(&no_build_note(app), ui::text_dim()))
                                .into_any_element()
                        }
                        None => {
                            // two-press install: the first press arms
                            // the button, the second starts the job
                            let confirming = self.confirm_install.as_deref()
                                == Some(action_id.as_str());
                            // no version badge: the raw string is a
                            // publisher-asserted free-text thing the
                            // card cannot compare, and the page's
                            // details line carries it on demand
                            div()
                                .flex()
                                .items_center()
                                .gap_2()
                                .child(
                                    ui::small_button(
                                        format!(
                                            "row-install-{action_id}-{}",
                                            row_publisher.as_deref().unwrap_or("")
                                        ),
                                        Some("icons/plus.svg"),
                                        if confirming { "Sure?" } else { "Install" },
                                        Tone::Accent,
                                    )
                                    .on_click(cx.listener(move |this, _e: &ClickEvent, _w, cx| {
                                        cx.stop_propagation();
                                        // the twin this row stands for
                                        this.selected_publisher = install_publisher.clone();
                                        if confirming {
                                            this.install(action_id.as_str(), cx);
                                        } else {
                                            this.confirm_install = Some(action_id.clone());
                                            cx.notify();
                                        }
                                    })),
                                )
                                .into_any_element()
                        }
                    })
                    .into_any_element()
            })
            .collect()
    }

    /// The full app page: identity, screenshots, provenance,
    /// permissions, and the install action in one scrollable view.
    /// The page is where the install decision happens, so the action
    /// bar stays pinned while the content scrolls.
    fn render_page(&mut self, app: StoreApp, font: gpui::Font, cx: &mut Context<Self>) -> AnyElement {
        let app_id = app.app_id.clone();
        // the page's bookkeeping key: the selected twin's publisher
        // names which digest, sizes, and verify error belong here
        let page_key = app_key(&app_id, app.publisher.as_deref());
        let installed = self.installed.contains_key(&app_id);
        let job = self.jobs.get(&app_id).cloned();
        let verified = self.verified.get(&page_key).cloned();

        // the permissions source, in order: catalog metadata (Orchard
        // channel), the deploy's ini (installed apps), the remote's
        // manifest (pre-install check on the reviewed channel)
        let sandbox_metadata: Option<String> = app
            .metadata
            .clone()
            .or_else(|| {
                self.installed
                    .get(&app_id)
                    .and_then(|info| info.metadata.clone())
            })
            .or_else(|| self.remote_metadata.get(&page_key).cloned());

        // the header icon: cached AppStream PNG, or a disk-cache fetch
        // of the remote icon, or the initials tile
        let header_icon = match (&app.icon_path, &app.icon_url) {
            (Some(path), _) => app_icon(Some(path), &app.name, 56.),
            (None, Some(url)) => match self.fetched.get(url) {
                Some(Some(path)) => app_icon(Some(path), &app.name, 56.),
                _ => app_icon(None, &app.name, 56.),
            },
            (None, None) => app_icon(None, &app.name, 56.),
        };

        // the quiet provenance line: what the user is relying on,
        // without cryptographic jargon by default
        let digest_failed = self.verify_errors.get(&page_key).cloned();
        let digest_badge = if digest_failed.is_none() {
            verified.clone().map(|digest| {
                let short = &digest[..digest.len().min(12)];
                // the pin itself is the fact; the mechanism lives on
                // hover. The wrapper carries the hover style (the
                // repaint trigger) and a unique id (tooltip state
                // lives under the element's global id), same rules as
                // the review glyphs.
                div()
                    .id("digest-pin")
                    .flex_none()
                    .hover(|style| style.bg(gpui::transparent_black()))
                    .tooltip(|_window, cx| {
                        cx.new(|_| {
                            TextTooltip(
                                "Install requires an exact match to this publisher-signed digest".into(),
                            )
                        })
                        .into()
                    })
                    .child(badge(
                        &format!("digest {short}"),
                        ui::risk_color(Risk::Low),
                    ))
            })
        } else {
            None
        };
        let provenance = match app.source {
            Source::Reviewed => {
                // the developer claim is navigation, not identity: a
                // name anyone can assert, so it clicks through to the
                // developer page and carries no checks
                let developer = app.developer.clone().map(|name| {
                    let label = format!("Developed by {name}.");
                    div()
                        .id("developed-by")
                        .flex()
                        .gap_1()
                        .items_center()
                        .text_size(px(ui::TEXT_SIZE))
                        .text_color(ui::text())
                        .cursor_pointer()
                        .hover(|style| style.text_color(ui::accent()))
                        .on_click(cx.listener(move |this, _e: &ClickEvent, _w, cx| {
                            this.open_collection(CollectionPage::Developer(name.clone()), cx);
                        }))
                        .child(label)
                });
                div()
                .flex()
                .flex_wrap()
                .items_center()
                .gap_2()
                .child(badge("Reviewed by Flathub", ui::text()))
                .child(
                    div()
                        .text_size(px(ui::TEXT_SIZE))
                        .text_color(ui::text())
                        .child("Moderator-reviewed listing; installs from your flathub remote."),
                )
                .children(developer)
                .into_any_element()
            }
            Source::Publisher => div()
                .flex()
                .flex_wrap()
                .items_center()
                .gap_2()
                // a verified check, not a text badge: the words live
                // on hover, same rules as the review glyphs
                .child(
                    div()
                        .id("publisher-verified")
                        .flex_none()
                        .hover(|style| style.bg(gpui::transparent_black()))
                        .tooltip(|_window, cx| {
                            cx.new(|_| {
                                TextTooltip(
                                    "Publisher-verified: the release events carry this publisher's signature".into(),
                                )
                            })
                            .into()
                        })
                        .child(ui::icon("icons/check.svg", 12., ui::accent())),
                )
                .children(app.publisher.as_ref().map(|pubkey| {
                    let profile = self.profiles.get(pubkey);
                    // the identity: the domain-backed address when the
                    // claim checked out, else the profile name, else
                    // the truncated npub. The verified check earlier in
                    // this row is about signatures; the address earns
                    // its place by being one. The row clicks through to
                    // the publisher page: everything this key publishes.
                    let label = match self.verified_nip05(pubkey) {
                        Some(claim) => format!("Published by {claim}."),
                        None => {
                            match profile.and_then(|p| p.name.clone()) {
                                Some(name) => format!("Published by {name}."),
                                None => format!("Published by {}.", short_npub(pubkey)),
                            }
                        }
                    };
                    let key = pubkey.clone();
                    div()
                        .id("published-by")
                        .flex()
                        .gap_1()
                        .items_center()
                        .text_size(px(ui::TEXT_SIZE))
                        .text_color(ui::text())
                        .cursor_pointer()
                        .hover(|style| style.text_color(ui::accent()))
                        .on_click(cx.listener(move |this, _e: &ClickEvent, _w, cx| {
                            this.open_collection(CollectionPage::Publisher(key.clone()), cx);
                        }))
                        .child(label)
                }))
                // continuity: this app id was published by another key
                // before this client ever saw this one; a signed
                // handover from that key reads calmer than an
                // unexplained new key
                .children(app.publisher.as_ref().filter(|pubkey| {
                    self.continuity
                        .apps
                        .get(&app.app_id)
                        .is_some_and(|entry| entry.flagged.contains(pubkey))
                }).map(|pubkey| {
                    let authorized = app.successor_to.is_some();
                    let (label, note) = if authorized {
                        (
                            "Publisher migrated",
                            "The previous key signed a statement handing this app id to this key. The digest pin still verifies the artifact.",
                        )
                    } else {
                        (
                            "New publisher key",
                            "This app id was published by another key before; this key is new since then. The digest pin is what your install verifies.",
                        )
                    };
                    div()
                        .id(SharedString::from(format!(
                            "key-changed-{}-{pubkey}",
                            app.app_id
                        )))
                        .flex_none()
                        .hover(|style| style.bg(gpui::transparent_black()))
                        .tooltip(|_window, cx| {
                            cx.new(|_| TextTooltip(note.into())).into()
                        })
                        .child(badge(
                            label,
                            ui::risk_color(if authorized {
                                Risk::Low
                            } else {
                                Risk::Medium
                            }),
                        ))
                }))
                .children(digest_badge)
                .children(digest_failed.clone().map(|err| {
                    div()
                        .flex()
                        .flex_wrap()
                        .items_center()
                        .gap_2()
                        .child(badge("Digest check failed", ui::risk_color(Risk::High)))
                        .child(
                            div()
                                .text_size(px(ui::TEXT_SIZE))
                                .text_color(ui::risk_color(Risk::High))
                                .child(err),
                        )
                }))
                .into_any_element(),
        };

        let mut content = div()
            .w_full()
            .min_w_0()
            .max_w(px(760.))
            .px_8()
            .py_6()
            .flex()
            .flex_col()
            .gap_5()
            .child(
                div()
                    .id("back")
                    .flex()
                    .items_center()
                    .gap_1p5()
                    .cursor_pointer()
                    .text_color(ui::accent())
                    .hover(|style| style.text_color(ui::text()))
                    .on_click(cx.listener(|this, _e: &ClickEvent, _w, cx| {
                        // a mouse close hands the list back clean; the
                        // keyboard anchor only survives Esc
                        this.selected = None;
                        this.close_page(cx);
                    }))
                    .child(ui::icon("icons/arrow_left.svg", 13., ui::accent()))
                    .child("All apps"),
            )
            .child(
                // identity header
                div().flex().items_start().gap_4().child(header_icon).child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .flex()
                        .flex_col()
                        .gap_1()
                        .child(
                            div()
                                .text_size(px(ui::TEXT_SIZE_EMPHASIS + 4.))
                                .font_weight(gpui::FontWeight::SEMIBOLD)
                                .text_color(ui::text())
                                .child(app.name.clone()),
                        )
                        .child(
                            div()
                                .flex()
                                .flex_wrap()
                                .items_center()
                                .gap_2()
                                .text_size(px(ui::TEXT_SIZE))
                                .text_color(ui::text())
                                .child(div().child(app.app_id.clone()))
                                .children(app.version.clone().map(|v| div().child(format!("· {v}"))))
                                .children(app.release_date.clone().map(|date| {
                                    // the claim reads as "updated" only
                                    // once it is true: a pre-listed
                                    // future release says "dated" until
                                    // the day arrives
                                    if &*date > orchard_core::appstream::today_iso().as_str() {
                                        div().child(format!("· dated {date}"))
                                    } else {
                                        div().child(format!("· updated {date}"))
                                    }
                                }))
                                .children(match self.installed.get(&app_id) {
                                    // installed: the size on disk; not
                                    // installed: what the remote would
                                    // download (or occupy once it lands)
                                    Some(info) => info
                                        .size
                                        .clone()
                                        .map(|size| div().child(format!("· {size}"))),
                                    None => self
                                        .remote_sizes
                                        .get(&page_key)
                                        .and_then(|(download, installed)| {
                                            download
                                                .clone()
                                                .map(|size| div().child(format!("· {size}")))
                                                .or_else(|| {
                                                    installed.clone().map(|size| {
                                                        div().child(format!("· {size}"))
                                                    })
                                                })
                                        }),
                                })
                                .children(
                                    app.license.clone().map(|l| div().child(format!("· {l}"))),
                                ),
                        )
                        .child(provenance),
                ),
            );

        if !app.screenshots.is_empty() {
            content = content.child(
                div()
                    .id("screenshots")
                    .flex()
                    .gap_2()
                    .overflow_x_scroll()
                    .track_scroll(&self.shot_handle)
                    // gpui hands every wheel axis to every scrollable
                    // under the pointer, so a horizontal swipe's small
                    // vertical drift would scroll the page too. Own
                    // x-dominant gestures here and stop the event before
                    // the page's scroll handler runs.
                    .on_scroll_wheel(cx.listener(|this, event: &ScrollWheelEvent, window, cx| {
                        let delta = event.delta.pixel_delta(window.line_height());
                        if delta.x.abs() > delta.y.abs() {
                            let mut offset = this.shot_handle.offset();
                            offset.x += delta.x;
                            this.shot_handle.set_offset(offset);
                            cx.stop_propagation();
                            cx.notify();
                        }
                    }))
                    .children(app.screenshots.iter().take(6).map(|url| {
                        match self.fetched.get(url) {
                            Some(Some(path)) => div()
                                .id(SharedString::from(format!("shot-{url}")))
                                .flex_none()
                                .cursor_pointer()
                                .rounded_sm()
                                .border_1()
                                .border_color(ui::divider())
                                .overflow_hidden()
                                .hover(|style| style.border_color(ui::accent()))
                                .on_click(cx.listener({
                                    let url = url.clone();
                                    move |this, _e: &ClickEvent, _w, cx| {
                                        this.lightbox = Some(url.clone());
                                        cx.notify();
                                    }
                                }))
                                .child(
                                    gpui::img(path.clone())
                                        .w(px(280.))
                                        .h(px(158.))
                                        .object_fit(gpui::ObjectFit::Cover),
                                )
                                .into_any_element(),
                            slot => image_placeholder(slot.is_none()),
                        }
                    })),
            );
        }

        let has_sandbox = sandbox_metadata.is_some();
        // the update's key differs from the deployed artifact's: the
        // fact rides above the action bar, amber, a fact not a
        // verdict
        let update_changed = self
            .installed
            .get(&app_id)
            .is_some_and(|info| update_available(&app, info))
            && self
                .installed_from(app_id.as_str())
                .is_some_and(|from| Some(from) != app.publisher.as_deref());
        // at a confirm, medium-and-high grants repeat here: quiet by
        // default, loud exactly once
        let risk_note = (self.confirm_install.as_deref() == Some(app_id.as_str())
            || self.confirm_update.as_deref() == Some(app_id.as_str()))
        .then(|| risk_echo(sandbox_metadata.as_deref()))
        .flatten();
        // the repo-verified fact for the page's repository link
        let page_repo_ok = app.repository.as_deref().is_some_and(|url| {
            self.repo_verified(url, app.publisher.as_deref())
        });
        content = content
            .child(
                div()
                    .text_size(px(ui::TEXT_SIZE_EMPHASIS))
                    .text_color(ui::text())
                    .child(app.summary.clone()),
            )
            .children(app.description.clone().map(|description| {
                // block text: one line per div so paragraphs and bullet
                // lists keep their structure; full contrast for reading
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .w_full()
                    .text_size(px(ui::TEXT_SIZE))
                    .text_color(ui::text())
                    .children(description.lines().map(|line| div().w_full().child(line.to_string())))
            }))
            .child(
                // the project links share one tight group: they are
                // one block of text, not two sections
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .children(
                        app.homepage
                            .clone()
                            .map(|url| {
                                project_link("homepage", "icons/globe.svg", url, false)
                            }),
                    )
                    .children(
                        app.repository
                            .clone()
                            .map(|url| {
                                project_link(
                                    "repository",
                                    "icons/git.svg",
                                    url,
                                    page_repo_ok,
                                )
                            }),
                    ),
            )
            .child(self.render_reviews(&app_id, app.publisher.as_deref(), font.clone(), cx))
            .when_some(sandbox_metadata, |section, metadata| {
                section.child(
                    div()
                        .flex()
                        .flex_col()
                        .gap_2()
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .gap_1p5()
                                .text_size(px(ui::TEXT_SIZE_EMPHASIS - 1.))
                                .font_weight(gpui::FontWeight::MEDIUM)
                                .text_color(ui::text())
                                .child(ui::icon("icons/shield.svg", 13., ui::text()))
                                .child("What it can access"),
                        )
                        .child(render_permissions_static(&metadata)),
                )
            })
            .when(!has_sandbox, |section| {
                // no sandbox data yet: it is being fetched from the
                // artifact or the remote, or flatpak could not produce it
                let note = if self.verify_errors.contains_key(&page_key) {
                    "Sandbox info unavailable: the artifact did not pass its digest check."
                } else if self.publisher_fetching.contains(&page_key) {
                    "Reading the sandbox manifest from the artifact…"
                } else if self.remote_fetching.contains(&page_key) {
                    "Reading the sandbox manifest from flathub…"
                } else {
                    "Flatpak apps run in a sandbox; this app's permissions appear here once its manifest is available."
                };
                section.child(
                    div()
                        .flex()
                        .items_center()
                        .gap_1p5()
                        .text_size(px(ui::TEXT_SIZE))
                        .text_color(ui::text())
                        .child(ui::icon("icons/shield.svg", 13., ui::text_dim()))
                        .child(note),
                )
            });

        div()
            .flex_1()
            .min_h_0()
            .flex()
            .flex_col()
            .bg(ui::panel())
            .child(
                div()
                    .id("page-scroll")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .track_scroll(&self.page_scroll)
                    .child(div().flex().justify_center().child(content)),
            )
            .child(
                // action bar: pinned under the scrolling content
                div().flex().items_center().gap_2().px_8().py_3().border_t_1()
                    .border_color(ui::divider()).bg(ui::panel())
                    .children(update_changed.then(|| {
                        div()
                            .flex()
                            .items_center()
                            .gap_1p5()
                            .text_size(px(ui::TEXT_SIZE))
                            .text_color(ui::risk_color(Risk::Medium))
                            .child(ui::icon(
                                "icons/shield.svg",
                                13.,
                                ui::risk_color(Risk::Medium),
                            ))
                            .child("This update is signed by a new publisher key.")
                    }))
                    .children(risk_note.map(|(color, text)| {
                        div()
                            .flex()
                            .items_center()
                            .gap_1p5()
                            .text_size(px(ui::TEXT_SIZE))
                            .text_color(color)
                            .child(ui::icon("icons/shield.svg", 13., color))
                            .child(text)
                    }))
                    .children(match job {
                        Some(state) => {
                            // the running pill is the cancel surface:
                            // first press arms "Cancel?", the second
                            // interrupts; the bar inside stays honest
                            // either way
                            let confirming =
                                self.confirm_cancel.as_deref() == Some(page_key.as_str());
                            let sweep = (state.started.elapsed().as_millis() % 2400) as f32
                                / 2400.;
                            let cancel_key = page_key.clone();
                            vec![ui::job_pill(
                                format!("job-pill-{page_key}"),
                                if confirming { "Cancel?" } else { state.label() },
                                state.progress.as_ref().and_then(|p| p.percent),
                                sweep,
                                confirming,
                            )
                            .on_click(cx.listener(move |this, _e: &ClickEvent, _w, cx| {
                                if confirming {
                                    this.cancel_job(cancel_key.as_str(), cx);
                                } else {
                                    this.confirm_cancel = Some(cancel_key.clone());
                                    cx.notify();
                                }
                            }))
                            .into_any_element()]
                        }
                        None => {
                            let mut buttons: Vec<gpui::AnyElement> = Vec::new();
                            if installed {
                                let open_id = app_id.clone();
                                buttons.push(
                                    button("btn-open", Some("icons/play.svg"), "Open", Tone::Accent)
                                        .on_click(
                                            cx.listener(move |this, _e: &ClickEvent, _w, cx| {
                                                this.open(open_id.as_str(), cx);
                                            }),
                                        )
                                        .into_any_element(),
                                );
                                if self
                                    .installed
                                    .get(&app_id)
                                    .is_some_and(|info| update_available(&app, info))
                                {
                                    let update_id = app_id.clone();
                                    let update_confirming = self.confirm_update.as_deref()
                                        == Some(app_id.as_str());
                                    buttons.push(
                                        button(
                                            "btn-update",
                                            Some("icons/update.svg"),
                                            if update_confirming { "Sure?" } else { "Update" },
                                            if update_changed {
                                                Tone::Warn
                                            } else {
                                                Tone::Quiet
                                            },
                                        )
                                        .on_click(cx.listener(
                                            move |this, _e: &ClickEvent, _w, cx| {
                                                if update_confirming {
                                                    this.update(update_id.as_str(), cx);
                                                } else {
                                                    this.confirm_update =
                                                        Some(update_id.clone());
                                                    cx.notify();
                                                }
                                            },
                                        ))
                                        .into_any_element(),
                                    );
                                }
                                let remove_id = app_id.clone();
                                // two-press: the destructive action
                                // reads "Sure?" before it runs, same
                                // as the row's Remove
                                let remove_confirming = self.confirm_remove.as_deref()
                                    == Some(app_id.as_str());
                                buttons.push(
                                    button(
                                        "btn-remove",
                                        Some("icons/trash.svg"),
                                        if remove_confirming { "Sure?" } else { "Remove" },
                                        Tone::Danger,
                                    )
                                    .on_click(cx.listener(
                                        move |this, _e: &ClickEvent, _w, cx| {
                                            if remove_confirming {
                                                this.uninstall(remove_id.as_str(), cx);
                                            } else {
                                                this.confirm_remove = Some(remove_id.clone());
                                                cx.notify();
                                            }
                                        },
                                    ))
                                    .into_any_element(),
                                );
                            } else if app.version.is_none() && app.source == Source::Publisher {
                                // the honest note replaces an install
                                // that cannot work
                                buttons.push(
                                    badge(&no_build_note(&app), ui::text_dim()).into_any_element(),
                                );
                            } else {
                                let install_id = app_id.clone();
                                // two-press install: same idiom as the
                                // list rows; the label carries the
                                // publisher flavor until armed
                                let confirming = self.confirm_install.as_deref()
                                    == Some(app_id.as_str());
                                let label = if confirming {
                                    "Sure?"
                                } else if app.source == Source::Publisher {
                                    "Verify & install"
                                } else {
                                    "Install"
                                };
                                buttons.push(
                                    button("btn-install", Some("icons/plus.svg"), label, Tone::Accent)
                                        .on_click(cx.listener(
                                            move |this, _e: &ClickEvent, _w, cx| {
                                                if confirming {
                                                    this.install(install_id.as_str(), cx);
                                                } else {
                                                    this.confirm_install =
                                                        Some(install_id.clone());
                                                    cx.notify();
                                                }
                                            },
                                        ))
                                        .into_any_element(),
                                );
                            }
                            buttons
                        }
                    }),
            )
            .into_any_element()
    }
}

fn risk_label(risk: Risk) -> &'static str {
    match risk {
        Risk::Low => "low",
        Risk::Medium => "medium",
        Risk::High => "high",
    }
}

/// Flatpak's piped progress line, from the probe: a bare state line
/// ("Installing…"), then updates shaped "Installing… ▊▌   8%" (label,
/// ellipsis, a block-character bar, the percent; roughly one line a
/// second over a real download), finally "Installation complete.".
/// Everything else on stdout (the permission echo, the summary
/// table) parses to None. Returns the label without the ellipsis and
/// the announced percent.
fn parse_progress_line(line: &str) -> Option<(String, Option<u32>)> {
    let line = line.trim();
    let (label, rest) = line.split_once('…')?;
    // only flatpak's status lines: a short label, never a sentence
    // or a field with a colon
    let label = label.trim();
    if label.is_empty() || label.len() > 40 || label.contains(':') || label.contains('.') {
        return None;
    }
    let percent = rest
        .trim()
        .rsplit([' ', '\t'])
        .next()
        .and_then(|tok| tok.strip_suffix('%'))
        .and_then(|num| num.parse::<u32>().ok());
    Some((label.to_string(), percent))
}

/// The status bar's wording for a flatpak state label: the long pull
/// is "Pulling from <place>", the deploy is "Deploying", anything
/// unknown passes through as flatpak said it.
fn stage_text(label: &str, place: &str) -> String {
    if label.starts_with("Downloading") {
        if place.is_empty() {
            "Pulling".to_string()
        } else {
            format!("Pulling from {place}")
        }
    } else if label.starts_with("Installing") || label.starts_with("Deploying") {
        "Deploying".to_string()
    } else {
        label.to_string()
    }
}

/// "12s" then "1m07s": the universal fallback truth when the child
/// announces no percent.
fn elapsed_text(d: Duration) -> String {
    let s = d.as_secs();
    if s < 60 {
        format!("{s}s")
    } else {
        format!("{}m{:02}s", s / 60, s % 60)
    }
}

/// The medium-and-high grants as one line for the confirm moment,
/// with the worst risk present as the color: a low-risk app stays
/// silent. Capped at three names, then a count.
fn risk_echo(metadata: Option<&str>) -> Option<(gpui::Rgba, String)> {
    let meta = metadata?;
    let loud: Vec<String> = orchard_core::permissions::grants_from_metadata(meta)
        .into_iter()
        .filter(|grant| grant.risk != Risk::Low)
        .map(|grant| match grant.risk {
            Risk::High => format!("{} (high)", grant.summary),
            _ => grant.summary,
        })
        .collect();
    let worst = if loud
        .iter()
        .any(|line| line.contains("(high)")) { Risk::High } else { Risk::Medium };
    let named = loud.iter().take(3).cloned().collect::<Vec<_>>().join(", ");
    let names = if loud.len() > 3 {
        format!("{named} and {} more", loud.len() - 3)
    } else {
        named
    };
    (!loud.is_empty()).then(|| (ui::risk_color(worst), format!("This app asks for: {names}.")))
}

/// Fixed list row height: `uniform_list` needs uniform items, and a
/// pinned height keeps the layout work per frame trivial.
const ROW_HEIGHT: f32 = 56.0;

/// Canonical browse categories, display order: the chip row folds to
/// this set, anything absent from it never shows a chip.
const BROWSE_CATEGORIES: [&str; 10] = [
    "Games",
    "Utilities",
    "Audio & Video",
    "Development",
    "Network",
    "Office",
    "Graphics",
    "Science",
    "Education",
    "System",
];

/// The screenshot slot while its image fetch is in flight, or after it
/// failed.
fn image_placeholder(loading: bool) -> gpui::AnyElement {
    div()
        .w(px(176.))
        .h(px(108.))
        .flex_none()
        .flex()
        .items_center()
        .justify_center()
        .rounded_sm()
        .bg(ui::inset())
        .border_1()
        .border_color(ui::divider_soft())
        .text_size(px(ui::TEXT_SIZE_LABEL))
        .text_color(ui::text_dim())
        .child(if loading { "loading…" } else { "unavailable" })
        .into_any_element()
}

/// A hover tooltip: quiet chrome, one line of text. The new-key
/// sprout uses it so the glyph stays quiet and the meaning is on
/// demand.
struct TextTooltip(SharedString);

impl gpui::Render for TextTooltip {
    fn render(
        &mut self,
        _window: &mut gpui::Window,
        _cx: &mut gpui::Context<Self>,
    ) -> impl gpui::IntoElement {
        div()
            .px_2()
            .py_1()
            .rounded_sm()
            .border_1()
            .border_color(ui::divider())
            .bg(ui::panel())
            .text_size(px(ui::TEXT_SIZE_LABEL))
            .text_color(ui::text())
            .child(self.0.to_string())
    }
}

/// Is this review written by the app's own publisher?
fn is_publisher_review(review: &orchard_core::Review, publisher: Option<&str>) -> bool {
    publisher == Some(review.pubkey.as_str())
}

/// The collection page's subject. Both variants share the card
/// grid; they differ in what the header may claim: a Nostr key is
/// an identity (the proof and continuity facts attach to it), while
/// an AppStream developer name is a claim anyone can make, so the
/// developer header wears no trust furniture.
#[derive(Debug, Clone, PartialEq, Eq)]
enum CollectionPage {
    Publisher(String),
    Developer(String),
}

/// The publisher page's rows: every listing one key publishes, in
/// the same arrived-only recency rank the lists use. A page is
/// aggregation, not curation: a key with fifty apps shows fifty
/// cards.
fn publisher_rows(apps: &[StoreApp], key: &str, today: &str) -> Vec<usize> {
    let mut rows: Vec<usize> = apps
        .iter()
        .enumerate()
        .filter(|(_, app)| app.publisher.as_deref() == Some(key))
        .map(|(index, _)| index)
        .collect();
    rows.sort_by(|&a, &b| date_order(&apps[a], &apps[b], today));
    rows
}

/// The developer page's rows: every reviewed app whose AppStream
/// claims this developer name. The name is navigation, not identity.
fn developer_rows(apps: &[StoreApp], developer: &str, today: &str) -> Vec<usize> {
    let mut rows: Vec<usize> = apps
        .iter()
        .enumerate()
        .filter(|(_, app)| app.developer.as_deref() == Some(developer))
        .map(|(index, _)| index)
        .collect();
    rows.sort_by(|&a, &b| date_order(&apps[a], &apps[b], today));
    rows
}

/// The catalog's composition line: quiet facts about the current
/// view, never a judgment. Rows, distinct publishers, verified
/// listings, rows from followed publishers. A flood reads as
/// composition, not as rank.
fn composition_line(rows: usize, publishers: usize, verified: usize, circle: usize) -> String {    let plural = |n: usize| if n == 1 { "" } else { "s" };
    let mut head = format!("{} app{}", rows, plural(rows));
    if publishers > 0 {
        head.push_str(&format!(" from {} publisher{}", publishers, plural(publishers)));
    }
    let mut parts = vec![head];
    if verified > 0 {
        parts.push(format!("{} verified", verified));
    }
    if circle > 0 {
        parts.push(format!("{} in your circle", circle));
    }
    parts.join(" · ")
}

/// The review display rows. Everyone is one recency list, newest
/// first. The circle lens is a filter, not an ordering: only the
/// signed-in identity's own review and their follows' show, so the
/// lens reads as "your people", never as a rank of the rest.
fn review_order(
    reviews: &[orchard_core::Review],
    everyone: bool,
    me: Option<&str>,
    follows: &std::collections::HashSet<String>,
) -> Vec<usize> {
    let mut order: Vec<usize> = (0..reviews.len())
        .filter(|&i| {
            everyone
                || Some(reviews[i].pubkey.as_str()) == me
                || follows.contains(&reviews[i].pubkey)
        })
        .collect();
    order.sort_by(|&a, &b| {
        let (x, y) = (&reviews[a], &reviews[b]);
        y.created_at
            .cmp(&x.created_at)
            .then_with(|| x.pubkey.cmp(&y.pubkey))
    });
    order
}

impl StoreRoot {
    /// Reviewers two steps away: followed by someone the signed-in
    /// identity follows, not followed directly, never the identity
    /// itself. Personal interpretation of the public graph, computed
    /// from small sets.
    fn circle2(&self) -> std::collections::HashSet<String> {
        circle2_set(
            &self.follows,
            &self.graph,
            self.reviewer.as_ref().map(|r| r.signer_pubkey.as_str()),
        )
    }
}

/// The two-hop set: everything the identity's follows follow, minus
/// the follows themselves and the identity.
fn circle2_set(
    follows: &std::collections::HashSet<String>,
    graph: &std::collections::BTreeMap<String, Vec<String>>,
    me: Option<&str>,
) -> std::collections::HashSet<String> {
    let mut set = std::collections::HashSet::new();
    for followed in follows {
        for pk in graph.get(followed).into_iter().flatten() {
            if Some(pk.as_str()) != me && !follows.contains(pk) {
                set.insert(pk.clone());
            }
        }
    }
    set
}

/// Is this reviewer's key new to the feed (first seen under a week
/// ago)? Absence from the table means unknown age, never new.
fn is_new_key(
    first_seen: &std::collections::BTreeMap<String, u64>,
    pubkey: &str,
    now: i64,
) -> bool {
    const WEEK: i64 = 7 * 24 * 60 * 60;
    first_seen
        .get(pubkey)
        .is_some_and(|seen| (now - *seen as i64) < WEEK)
}

/// The reviews aggregate for one app. Publisher self-reviews are
/// excluded from the average and count (their rows stay, flagged);
/// reviews from new keys count fully and are only reported: a weak
/// heuristic never drops a vote. Circle reviews (the signed-in
/// identity's follows) are counted for the circle disclosure and its
/// own average, under the same publisher exclusion.
#[derive(Default, Debug, PartialEq)]
struct ReviewsAggregate {
    average: Option<f32>,
    counted: usize,
    new_keys: usize,
    publisher_reviews: usize,
    circle: usize,
    circle_average: Option<f32>,
    circle2: usize,
}

fn reviews_aggregate(
    reviews: &[orchard_core::Review],
    publisher: Option<&str>,
    first_seen: &std::collections::BTreeMap<String, u64>,
    now: i64,
    follows: &std::collections::HashSet<String>,
    circle2: &std::collections::HashSet<String>,
) -> ReviewsAggregate {
    let mut aggregate = ReviewsAggregate::default();
    let mut rated: Vec<f32> = Vec::new();
    let mut circle_rated: Vec<f32> = Vec::new();
    for review in reviews {
        if is_publisher_review(review, publisher) {
            aggregate.publisher_reviews += 1;
            continue;
        }
        if let Some(rating) = review.rating {
            rated.push(rating);
        }
        if is_new_key(first_seen, &review.pubkey, now) {
            aggregate.new_keys += 1;
        }
        if follows.contains(&review.pubkey) {
            aggregate.circle += 1;
            if let Some(rating) = review.rating {
                circle_rated.push(rating);
            }
        } else if circle2.contains(&review.pubkey) {
            aggregate.circle2 += 1;
        }
    }
    aggregate.average = (!rated.is_empty())
        .then(|| rated.iter().sum::<f32>() / rated.len() as f32);
    aggregate.circle_average = (!circle_rated.is_empty())
        .then(|| circle_rated.iter().sum::<f32>() / circle_rated.len() as f32);
    aggregate.counted = reviews.len() - aggregate.publisher_reviews;
    aggregate
}

impl StoreRoot {
    /// The reviews section of the app page: a quiet header that doubles
    /// as the expand/collapse toggle (the list is hidden until asked
    /// for), the review list flush with the other page fields, and the
    /// compose or sign-in form when expanded.
    fn render_reviews(
        &mut self,
        app_id: &str,
        publisher: Option<&str>,
        font: gpui::Font,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        // collapsed, the header only aggregates, and the aggregate can
        // borrow the map; the owned copy is for the expanded list,
        // whose row closures move content out. A collapsed section
        // therefore costs no content clones per frame.
        let expanded = self.reviews_expanded;
        let borrowed: &[orchard_core::Review] = self
            .reviews
            .get(app_id)
            .map_or(&[], Vec::as_slice);
        let reviews = if expanded {
            self.reviews.get(app_id).cloned().unwrap_or_default()
        } else {
            Vec::new()
        };

        // header: same shape as the other section headers, so the text
        // lines up; the score formats with one decimal, no float junk
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(i64::MAX);
        let circle2_set = self.circle2();
        let ReviewsAggregate {
            average,
            counted,
            new_keys,
            publisher_reviews,
            circle,
            circle_average,
            circle2,
        } = reviews_aggregate(
            borrowed,
            publisher,
            &self.first_seen,
            now,
            &self.follows,
            &circle2_set,
        );
        let label = if borrowed.is_empty() {
            "No reviews yet".to_string()
        } else {
            let score = average
                .map(|value| format!(" · {:.1} {}", value, stars(value)))
                .unwrap_or_default();
            format!("Reviews{} · {}", score, counted)
        };
        let mut section = div().flex().flex_col().gap_2().w_full().child(
            div()
                .id("reviews-toggle")
                .flex()
                .items_center()
                .gap_1p5()
                .cursor_pointer()
                .text_size(px(ui::TEXT_SIZE_EMPHASIS - 1.))
                .font_weight(gpui::FontWeight::MEDIUM)
                .text_color(ui::text())
                .hover(|style| style.text_color(ui::accent()))
                .on_click(cx.listener(|this, _e: &ClickEvent, _w, cx| {
                    this.reviews_expanded = !this.reviews_expanded;
                    cx.notify();
                }))
                .child(if self.reviews_expanded {
                    ui::icon("icons/chevron_down.svg", 13., ui::text())
                } else {
                    ui::icon("icons/chevron_right.svg", 13., ui::text())
                })
                .child(label),
        );

        // the reviews themselves: indented one step from the header,
        // the same inset the permissions box uses, so the page reads
        // as header, then its content
        if self.reviews_expanded {
            let mut body = div().flex().flex_col().gap_2().pl_3();
            // the lens: your circle is the signed-in default, everyone
            // is one click (or v) away. Hidden signed out: with no
            // follows the two lenses are the same list.
            if self.reviewer.is_some() && !reviews.is_empty() {
                body = body.child(
                    div()
                        .flex()
                        .items_center()
                        .gap_2()
                        .text_size(px(ui::TEXT_SIZE_LABEL))
                        .child(self.lens_chip(
                            "lens-circle",
                            "In your circle",
                            !self.reviews_everyone,
                            false,
                            cx,
                        ))
                        .child(div().text_color(ui::text_dim()).child("·"))
                        .child(self.lens_chip(
                            "lens-everyone",
                            "Everyone",
                            self.reviews_everyone,
                            true,
                            cx,
                        )),
                );
            }
            // the trust breakdown, disclosed rather than hidden: what
            // the aggregate left out and which reviews are young keys
            let mut notes: Vec<String> = Vec::new();
            if publisher_reviews > 0 {
                notes.push(format!(
                    "{publisher_reviews} publisher review{} not counted",
                    if publisher_reviews == 1 { "" } else { "s" }
                ));
            }
            if new_keys > 0 {
                notes.push(format!("{new_keys} of {counted} from new keys"));
            }
            if circle > 0 {
                let mut circle_note = format!("{circle} in your circle");
                if let Some(value) = circle_average {
                    circle_note.push_str(&format!(", avg {value:.1}"));
                }
                notes.push(circle_note);
            }
            if circle2 > 0 {
                notes.push(format!("{circle2} in your circle's circle"));
            }
            if !notes.is_empty() {
                body = body.child(
                    div()
                        .text_size(px(ui::TEXT_SIZE_LABEL))
                        .text_color(ui::text_dim())
                        .child(notes.join(" · ")),
                );
            }
            let mut avatar_urls: Vec<String> = Vec::new();
            let order = review_order(
                &reviews,
                self.reviews_everyone,
                self.reviewer
                    .as_ref()
                    .map(|r| r.signer_pubkey.as_str()),
                &self.follows,
            );
            // the circle lens with nothing to show says so, rather
            // than reading as an empty section
            if !self.reviews_everyone && order.is_empty() && !reviews.is_empty() {
                body = body.child(
                    div()
                        .text_size(px(ui::TEXT_SIZE))
                        .text_color(ui::text_dim())
                        .child("No reviews from people you follow yet."),
                );
            }
            for index in order {
                let review = &reviews[index];
                // the author's identity on the row: the profile name
                // when there is one, the truncated npub otherwise. A
                // NIP-05 claim never renders as a name; it renders as
                // the check beside one, and only when the domain
                // backed it.
                let author = review
                    .name
                    .clone()
                    .unwrap_or_else(|| short_npub(&review.pubkey));
                let nip05_note = self.verified_nip05(&review.pubkey).and_then(|claim| {
                    split_nip05(&claim)
                        .map(|(local, domain)| format!("{domain} lists {local} as this key"))
                });
                let when = relative_time(review.created_at);
                let flagged_publisher = is_publisher_review(review, publisher);
                let fresh = is_new_key(&self.first_seen, &review.pubkey, now);
                let in_circle = self.follows.contains(&review.pubkey);
                let in_circle2 = !in_circle && circle2_set.contains(&review.pubkey);
                // the author's picture when their profile carries one;
                // the glyph stands in until the fetch lands
                let picture = self
                    .profiles
                    .get(&review.pubkey)
                    .and_then(|p| p.picture.clone());
                if let Some(url) = &picture {
                    avatar_urls.push(url.clone());
                }
                let avatar_path = picture
                    .as_deref()
                    .and_then(|url| self.fetched.get(url))
                    .and_then(|f| f.as_deref());
                let mut row = div().flex().flex_col().gap_1().w_full().child(
                    div()
                        .flex()
                        .items_center()
                        .gap_2()
                        .text_size(px(ui::TEXT_SIZE_LABEL))
                        .text_color(ui::text_dim())
                        .child(avatar(avatar_path, 14., ui::text_dim()))
                        .child(div().child(author))
                        .children(nip05_note.map(|note| {
                            // the domain backed this author's claim;
                            // the words live on hover, same rules as
                            // the sprout
                            div()
                                .id(SharedString::from(format!("nip05-review-{index}")))
                                .flex_none()
                                .hover(|style| style.bg(gpui::transparent_black()))
                                .tooltip(move |_window, cx| {
                                    cx.new(|_| TextTooltip(note.clone().into())).into()
                                })
                                .child(ui::icon("icons/check.svg", 11., ui::accent()))
                        }))
                        .children(review.rating.map(|value| {
                            div().text_color(ui::accent()).child(stars(value))
                        }))
                        .child(div().child(when))
                        .children(flagged_publisher.then(|| {
                            div().text_color(ui::text()).child("publisher")
                        }))
                        .children(fresh.then(|| {
                            // a sprout: a key young in the feed; the
                            // words live on hover and in the line below.
                            // The transparent hover style is load-bearing:
                            // gpui registers its repaint-on-hover-change
                            // listener only for elements with a hover
                            // style, and a visible tooltip hides during
                            // prepaint, so without it the tooltip never
                            // sees the mouse leave. The id must be unique
                            // per row: the tooltip state lives under the
                            // element's global id, and duplicate ids
                            // collapse every row's tooltip onto one slot.
                            div()
                                .id(SharedString::from(format!("new-key-{index}")))
                                .flex_none()
                                .hover(|style| style.bg(gpui::transparent_black()))
                                .tooltip(|_window, cx| {
                                    cx.new(|_| {
                                        TextTooltip("New key: first seen on the relays under a week ago".into())
                                    })
                                    .into()
                                })
                                .child(ui::icon("icons/sprout.svg", 11., ui::accent()))
                        }))
                        .children(in_circle.then(|| {
                            // the circle glyph: a reviewer the signed-in
                            // identity follows. Same hover rules as the
                            // sprout (the style is the repaint trigger,
                            // and the id must be unique per row).
                            div()
                                .id(SharedString::from(format!("circle-{index}")))
                                .flex_none()
                                .hover(|style| style.bg(gpui::transparent_black()))
                                .tooltip(|_window, cx| {
                                    cx.new(|_| {
                                        TextTooltip("You follow this reviewer".into())
                                    })
                                    .into()
                                })
                                .child(ui::icon("icons/person.svg", 11., ui::accent()))
                        }))
                        .children(in_circle2.then(|| {
                            // the two-hop glyph: a reviewer someone in
                            // the circle follows. A second hue rather
                            // than a dimmed accent: the two distances
                            // read apart at a glance. Same hover rules.
                            div()
                                .id(SharedString::from(format!("circle2-{index}")))
                                .flex_none()
                                .hover(|style| style.bg(gpui::transparent_black()))
                                .tooltip(|_window, cx| {
                                    cx.new(|_| {
                                        TextTooltip("Followed by someone you follow".into())
                                    })
                                    .into()
                                })
                                .child(ui::icon("icons/person.svg", 11., ui::circle2()))
                        })),
                );
                if !review.content.is_empty() {
                    // second-hand trust reads dimmer: the two-hop rows
                    // only appear in the Everyone lens, and there they
                    // look what they are
                    row = row.child(
                        div()
                            .flex()
                            .flex_col()
                            .text_size(px(ui::TEXT_SIZE))
                            .text_color(if in_circle2 {
                                ui::text_dim()
                            } else {
                                ui::text()
                            })
                            .children(
                                review
                                    .content
                                    .lines()
                                    .map(|line| div().w_full().child(line.to_string())),
                            ),
                    );
                }
                body = body.child(row);
            }
            // the rows' avatar fetches: results notify and repaint
            self.ensure_fetched(avatar_urls, cx);

            // compose or invitation, depending on session state (the
            // sign-in modal is window-level, not part of the section)
            if self.composing {
                body = body.child(self.render_review_compose(app_id, font.clone(), cx));
            } else {
                let signed_in = self.reviewer.is_some();
                let invitation = div().flex().items_center().gap_2().child(
                    div()
                        .id("write-review")
                        .cursor_pointer()
                        .text_size(px(ui::TEXT_SIZE))
                        .text_color(ui::accent())
                        .hover(|style| style.text_color(ui::text()))
                        .on_click(cx.listener(|this, _e: &ClickEvent, _w, cx| {
                            this.start_review(cx);
                        }))
                        .child(if signed_in {
                            "Write a review (r)".to_string()
                        } else {
                            "Sign in to review".to_string()
                        }),
                );
                body = body.child(invitation);
            }
            section = section.child(body);
        }

        section.into_any_element()
    }

    /// One lens chip in the reviews section: accent when active,
    /// quiet otherwise; the click sets the lens.
    fn lens_chip(
        &self,
        id: &'static str,
        label: &'static str,
        active: bool,
        set_everyone: bool,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        div()
            .id(SharedString::from(id))
            .cursor_pointer()
            .text_size(px(ui::TEXT_SIZE_LABEL))
            .text_color(if active { ui::accent() } else { ui::text_dim() })
            .hover(|style| style.text_color(ui::text()))
            .on_click(cx.listener(move |this, _e: &ClickEvent, _w, cx| {
                this.reviews_everyone = set_everyone;
                cx.notify();
            }))
            .child(label.to_string())
            .into_any_element()
    }

    /// The collection page: one subject's apps as a card grid. The
    /// cards carry its apps in the same recency rank the lists use.
    /// Opened from an app page's "Published by" (a key) or
    /// "Developed by" (an AppStream claim) row, which stays mounted
    /// underneath.
    fn render_collection(&mut self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let Some(page) = self.collection_view.clone() else {
            return div().into_any_element();
        };
        let today = orchard_core::appstream::today_iso();
        let rows = match &page {
            CollectionPage::Publisher(key) => publisher_rows(&self.apps, key, &today),
            CollectionPage::Developer(label) => developer_rows(&self.apps, label, &today),
        };
        let plural = |n: usize| if n == 1 { "" } else { "s" };
        let count_line = match &page {
            CollectionPage::Publisher(_) => {
                format!("{} app{} on Orchard", rows.len(), plural(rows.len()))
            }
            // the claim only gathers Flathub listings; Orchard rows
            // group on the publisher's key instead
            CollectionPage::Developer(_) => {
                format!("{} app{} on Flathub", rows.len(), plural(rows.len()))
            }
        };

        // the cards' remote icons ride the same shared cache as the
        // list rows: a publisher app's icon exists only as a URL, and
        // this page wore initials because nothing here asked for the
        // fetch
        let card_icon_urls: Vec<String> = rows
            .iter()
            .filter_map(|&i| self.apps[i].icon_url.clone())
            .filter(|url| !self.fetched.contains_key(url))
            .collect();
        self.ensure_fetched(card_icon_urls, cx);

        // the header per subject: a publisher key is an identity, so
        // the profile name, the address check, and the avatar all
        // attach to it. A developer name is a claim anyone can make:
        // the label alone, no trust furniture. The truncated npub
        // rides along on publishers only, and a click copies the full
        // key: the identity page is where verification happens.
        let (name, nip05_note, avatar_path, npub_line) = match &page {
            CollectionPage::Publisher(key) => {
                let profile = self.profiles.get(key);
                let name = profile
                    .and_then(|p| p.name.clone())
                    .unwrap_or_else(|| short_npub(key));
                let nip05_note = self.verified_nip05(key).and_then(|claim| {
                    split_nip05(&claim)
                        .map(|(local, domain)| format!("{domain} lists {local} as this key"))
                });
                let picture = profile.and_then(|p| p.picture.clone());
                self.ensure_fetched(
                    picture.iter().cloned().collect::<Vec<String>>(),
                    cx,
                );
                let avatar_path = picture
                    .as_deref()
                    .and_then(|url| self.fetched.get(url))
                    .and_then(|f| f.as_deref());
                let npub_line = full_npub(key).map(|full| (short_npub(key), full));
                (name, nip05_note, avatar_path, npub_line)
            }
            CollectionPage::Developer(label) => (label.clone(), None, None, None),
        };

        // the cards: cloned strings first, the builder borrows after
        struct Card {
            app_id: String,
            publisher: Option<String>,
            name: String,
            summary: String,
            icon_path: Option<std::path::PathBuf>,
            date_note: Option<String>,
            repo_ok: bool,
        }
        let cards: Vec<Card> = rows
            .iter()
            .map(|&i| {
                let app = &self.apps[i];
                Card {
                    app_id: app.app_id.clone(),
                    publisher: app.publisher.clone(),
                    name: app.name.clone(),
                    summary: app.summary.clone(),
                    // the cached AppStream PNG, else the fetched
                    // remote icon once it lands
                    icon_path: app.icon_path.clone().or_else(|| {
                        let url = app.icon_url.as_deref()?;
                        self.fetched.get(url)?.as_deref().map(std::path::PathBuf::from)
                    }),
                    date_note: app.release_date.as_deref().map(|date| {
                        // the claim reads as "updated" only once true
                        if date > today.as_str() {
                            format!("dated {date}")
                        } else {
                            format!("updated {date}")
                        }
                    }),
                    repo_ok: self.row_verified(app),
                }
            })
            .collect();

        let mut grid = div().flex().flex_wrap().gap_3();
        for (index, card) in cards.iter().enumerate() {
            grid = grid.child(
                div()
                    .id(SharedString::from(format!("pub-card-{index}")))
                    .w(px(264.))
                    .flex()
                    .flex_col()
                    .gap_1p5()
                    .p_3()
                    .rounded_md()
                    .border_1()
                    .border_color(ui::divider_soft())
                    .bg(ui::panel())
                    .cursor_pointer()
                    .hover(|style| style.bg(ui::surface()))
                    .on_click({
                        let app_id = card.app_id.clone();
                        let publisher = card.publisher.clone();
                        cx.listener(move |this, _e: &ClickEvent, _w, cx| {
                            // the page is about THIS row of the twin pair
                            this.selected_publisher = publisher.clone();
                            this.collection_view = None;
                            this.open_page(&app_id, cx);
                        })
                    })
                    .child(
                        div()
                            .flex()
                            .items_start()
                            .gap_2p5()
                            .child(app_icon(
                                card.icon_path.as_deref(),
                                &card.name,
                                44.,
                            ))
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .flex()
                                    .flex_col()
                                    .gap_0p5()
                                    .child(
                                        div()
                                            .text_size(px(ui::TEXT_SIZE_EMPHASIS))
                                            .font_weight(gpui::FontWeight::MEDIUM)
                                            .text_color(ui::text())
                                            .overflow_hidden()
                                            .whitespace_nowrap()
                                            .child(card.name.clone()),
                                    )
                                    .child(
                                        div()
                                            .text_size(px(ui::TEXT_SIZE))
                                            .text_color(ui::text_dim())
                                            .overflow_hidden()
                                            .whitespace_nowrap()
                                            .child(card.summary.clone()),
                                    ),
                            ),
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_1p5()
                            .text_size(px(ui::TEXT_SIZE_LABEL))
                            .text_color(ui::text_dim())
                            .children(card.date_note.clone().map(|note| div().child(note)))
                            .children(card.repo_ok.then(|| {
                                ui::icon("icons/check.svg", 12., ui::accent())
                            })),
                    ),
            );
        }

        div()
            .flex_1()
            .min_h_0()
            .flex()
            .flex_col()
            .bg(ui::panel())
            .child(
                div()
                    .id("publisher-scroll")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .track_scroll(&self.collection_scroll)
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap_4()
                            .px_8()
                            .py_4()
                            .max_w(px(920.))
                            .mx_auto()
                            // back affordance, matching the app page
                            .child(
                                div()
                                    .id("publisher-back")
                                    .flex()
                                    .items_center()
                                    .gap_1p5()
                                    .cursor_pointer()
                                    .text_size(px(ui::TEXT_SIZE))
                                    .text_color(ui::accent())
                                    .hover(|style| style.text_color(ui::text()))
                                    .on_click(cx.listener(|this, _e: &ClickEvent, _w, cx| {
                                        this.collection_view = None;
                                        cx.notify();
                                    }))
                                    .child(ui::icon("icons/arrow_left.svg", 13., ui::accent()))
                                    .child(if self.page_open {
                                        "Back to the app"
                                    } else {
                                        "All apps"
                                    }),
                            )
                            .child(
                                // identity header
                                div()
                                    .flex()
                                    .items_center()
                                    .gap_4()
                                    .child(avatar(avatar_path, 48., ui::text_dim()))
                                    .child(
                                        div()
                                            .flex()
                                            .flex_col()
                                            .gap_1()
                                            .child(
                                                div()
                                                    .flex()
                                                    .items_center()
                                                    .gap_2()
                                                    .text_size(px(ui::TEXT_SIZE_EMPHASIS + 2.))
                                                    .font_weight(gpui::FontWeight::MEDIUM)
                                                    .text_color(ui::text())
                                                    .child(name)
                                                    .children(nip05_note.map(|note| {
                                                        div()
                                                            .id("publisher-nip05")
                                                            .flex_none()
                                                            .hover(|style| {
                                                                style.bg(
                                                                    gpui::transparent_black(),
                                                                )
                                                            })
                                                            .tooltip(move |_window, cx| {
                                                                cx.new(|_| {
                                                                    TextTooltip(note.clone().into())
                                                                })
                                                                    .into()
                                                            })
                                                            .child(ui::icon(
                                                                "icons/check.svg",
                                                                13.,
                                                                ui::accent(),
                                                            ))
                                                    })),
                                            )
                                            .children(npub_line.map(|(short, full)| {
                                                let copied = self
                                                    .npub_copied
                                                    .as_deref()
                                                    == Some(full.as_str());
                                                div()
                                                    .id("publisher-npub")
                                                    .cursor_pointer()
                                                    .text_size(px(ui::TEXT_SIZE_SMALL))
                                                    .text_color(if copied {
                                                        ui::accent()
                                                    } else {
                                                        ui::text()
                                                    })
                                                    .hover(|style| style.text_color(ui::accent()))
                                                    .tooltip(|_window, cx| {
                                                        cx.new(|_| {
                                                            TextTooltip("Copy npub".into())
                                                        })
                                                        .into()
                                                    })
                                                    .on_click({
                                                        let full = full.clone();
                                                        cx.listener(
                                                            move |this, _e: &ClickEvent, _w, cx| {
                                                                cx.write_to_clipboard(
                                                                    gpui::ClipboardItem::new_string(
                                                                        full.clone(),
                                                                    ),
                                                                );
                                                                this.npub_copied =
                                                                    Some(full.clone());
                                                                cx.notify();
                                                                // the word stays up for a
                                                                // beat, then the key comes
                                                                // back; only while the copy
                                                                // is still this one
                                                                let revert = full.clone();
                                                                cx.spawn(async move |this, cx| {
                                                                    cx.background_executor()
                                                                        .timer(
                                                                            std::time::Duration::from_millis(1600),
                                                                        )
                                                                        .await;
                                                                    let _ = this.update(
                                                                        cx,
                                                                        |this, cx| {
                                                                            if this.npub_copied.as_deref()
                                                                                == Some(revert.as_str())
                                                                            {
                                                                                this.npub_copied =
                                                                                    None;
                                                                                cx.notify();
                                                                            }
                                                                        },
                                                                    );
                                                                })
                                                                .detach();
                                                            },
                                                        )
                                                    })
                                                    .child(if copied {
                                                        "Copied.".to_string()
                                                    } else {
                                                        short
                                                    })
                                            }))
                                            .child(
                                                div()
                                                    .text_size(px(ui::TEXT_SIZE))
                                                    .text_color(ui::text())
                                                    .child(count_line),
                                            )
                                            .children(
                                                matches!(&page, CollectionPage::Developer(_))
                                                    .then(|| {
                                                        div()
                                                            .text_size(px(ui::TEXT_SIZE_SMALL))
                                                            .text_color(ui::text_dim())
                                                            .child(
                                                                "A developer name is a claim any listing can make; the publisher's key is the identity.",
                                                            )
                                                    }),
                                            ),
                                    ),
                            )
                            .child(grid),
                    ),
            )
            .into_any_element()
    }

    /// The sign-in modal body: the pairing code while it waits, the
    /// optional name question once a signer answered. The chrome
    /// (backdrop, centering) is around it in render_signin_modal.
    /// The corner X both sign-in phases carry: the same exit the
    /// backdrop and Escape take, visible instead of learned.
    fn signin_close(&self, cx: &Context<Self>) -> gpui::Stateful<gpui::Div> {
        div()
            .id("signin-modal-close")
            .absolute()
            .top_2()
            .right_2()
            .flex()
            .items_center()
            .justify_center()
            .w(px(22.))
            .h(px(22.))
            .rounded_sm()
            .cursor_pointer()
            .hover(|style| style.bg(ui::surface()))
            .on_click(cx.listener(|this, _e: &ClickEvent, _w, cx| {
                this.close_signin();
                cx.notify();
            }))
            .child(ui::icon("icons/close.svg", 12., ui::text_dim()))
    }

    fn render_signin_modal(&mut self, font: gpui::Font, cx: &mut Context<Self>) -> gpui::AnyElement {
        let body = if self.pending_reviewer.is_some() {
            self.render_name_phase(font, cx)
        } else if self.connect.is_some() {
            self.render_pairing_phase(cx)
        } else {
            div()
                .relative()
                .w(px(440.))
                .flex()
                .flex_col()
                .gap_3()
                .rounded_md()
                .border_1()
                .border_color(ui::divider())
                .bg(ui::panel())
                .p_4()
                .child(self.signin_close(cx))
                .child(
                    div()
                        .text_size(px(ui::TEXT_SIZE_EMPHASIS))
                        .text_color(ui::text())
                        .child("Sign in"),
                )
                .child(
                    div()
                        .text_size(px(ui::TEXT_SIZE))
                        .text_color(ui::text_dim())
                        .child("Preparing the pairing…"),
                )
                .into_any_element()
        };
        div()
            .absolute()
            .inset_0()
            .flex()
            .items_center()
            .justify_center()
            // the dimmed click-away is a FIRST sibling; the panel after
            // it wins every click that lands on it, and swallows the
            // mouse down so the backdrop's close never sees it
            .child(
                div()
                    .id("signin-modal-backdrop")
                    .absolute()
                    .inset_0()
                    .bg(gpui::rgba(0x000000B3))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, _e: &MouseDownEvent, _w, cx| {
                            this.close_signin();
                            // swallow the down: the Sign in button
                            // under the modal never sees it, so its
                            // click cannot reopen what this closed
                            cx.stop_propagation();
                            cx.notify();
                        }),
                    ),
            )
            .child(
                div().on_mouse_down(
                    MouseButton::Left,
                    |_e: &MouseDownEvent, _w, cx: &mut gpui::App| cx.stop_propagation(),
                ).child(body),
            )
            .into_any_element()
    }

    /// The pairing panel: the QR and the URI, both clickable (the
    /// signer app opens on click; the URI lands on the clipboard
    /// either way), a status line. Closing the modal and reopening
    /// starts a fresh pairing, so there is no retry button.
    fn render_pairing_phase(&mut self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let Some(session) = self.connect.as_ref() else {
            return div().into_any_element();
        };
        let qr = session.qr_path.clone();
        let uri = session.uri.clone();
        let uri_for_click = uri.clone();
        let uri_for_qr_click = uri.clone();
        let mut panel = div()
            .relative()
            .w(px(440.))
            .flex()
            .flex_col()
            .gap_3()
            .rounded_md()
            .border_1()
            .border_color(ui::divider())
            .bg(ui::panel())
            .p_4()
            .child(self.signin_close(cx))
            .child(
                div()
                    .text_size(px(ui::TEXT_SIZE_EMPHASIS))
                    .text_color(ui::text())
                    .child("Sign in"),
            )
            .child(
                div().flex().justify_center().child(
                    div()
                        .id("connect-qr")
                        .cursor_pointer()
                        .rounded_sm()
                        .on_click(cx.listener(move |_this, _e: &ClickEvent, _w, cx| {
                            cx.write_to_clipboard(gpui::ClipboardItem::new_string(
                                uri_for_qr_click.clone(),
                            ));
                            cx.open_url(&uri_for_qr_click);
                        }))
                        .child(gpui::img(qr.clone()).w(px(240.)).h(px(240.))),
                ),
            )
            .child(
                div()
                    .id("connect-uri")
                    .cursor_pointer()
                    .w_full()
                    .rounded_sm()
                    .border_1()
                    .border_color(ui::divider())
                    .bg(ui::surface())
                    .px_3()
                    .py_2()
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .text_size(px(ui::TEXT_SIZE_SMALL))
                    .text_color(ui::text_dim())
                    .hover(|style| {
                        style.text_color(ui::text()).border_color(ui::accent())
                    })
                    .on_click(cx.listener(move |_this, _e: &ClickEvent, _w, cx| {
                        cx.write_to_clipboard(gpui::ClipboardItem::new_string(
                            uri_for_click.clone(),
                        ));
                        cx.open_url(&uri_for_click);
                    }))
                    .child(uri.clone()),
            )
            .child(
                div()
                    .text_size(px(ui::TEXT_SIZE_LABEL))
                    .text_color(ui::text_dim())
                    .child("Click to pair, or scan with a signer app."),
            )
            .child(
                div()
                    .text_size(px(ui::TEXT_SIZE_LABEL))
                    .text_color(ui::text_dim())
                    .child("Waiting for approval…"),
            );
        if let Some(error) = &self.review_error {
            panel = panel.child(
                div()
                    .text_size(px(ui::TEXT_SIZE_LABEL))
                    .text_color(ui::risk_color(Risk::Medium))
                    .child(error.clone()),
            );
        }
        panel.into_any_element()
    }

    /// The name question after a successful pairing: one optional
    /// line, published as a merged kind 0 profile through the signer.
    fn render_name_phase(&mut self, font: gpui::Font, cx: &mut Context<Self>) -> gpui::AnyElement {
        let text = self.review_draft.1.clone();
        let input = self.render_text_input(
            "signin-input",
            &text,
            "Display name (optional)",
            font,
        );
        div()
            .relative()
            .w(px(440.))
            .flex()
            .flex_col()
            .gap_3()
            .rounded_md()
            .border_1()
            .border_color(ui::divider())
            .bg(ui::panel())
            .p_4()
            .child(self.signin_close(cx))
            .child(
                div()
                    .text_size(px(ui::TEXT_SIZE_EMPHASIS))
                    .text_color(ui::text())
                    .child("Paired"),
            )
            .child(input)
            .child(
                div().flex().items_center().gap_3().child(
                    button("btn-start-reviewing", None, "Start reviewing", Tone::Accent).on_click(
                        cx.listener(|this, _e: &ClickEvent, _w, cx| {
                            this.finish_sign_in(cx);
                        }),
                    ),
                ),
            )
            .child(
                div()
                    .text_size(px(ui::TEXT_SIZE_LABEL))
                    .text_color(ui::text_dim())
                    .child("Enter confirms. A name publishes a public profile."),
            )
            .children(self.review_error.clone().map(|error| {
                div()
                    .text_size(px(ui::TEXT_SIZE_LABEL))
                    .text_color(ui::risk_color(Risk::Medium))
                    .child(error)
            }))
            .into_any_element()
    }

    /// The shared single-line text input: StyledText with selection
    /// runs, a static caret, click-to-enter. Used by the review draft
    /// and the sign-in form (the search box keeps its own wiring in
    /// render, where the window's font style is at hand).
    fn render_text_input(
        &mut self,
        id: &'static str,
        text: &str,
        placeholder: &str,
        font: gpui::Font,
    ) -> gpui::Stateful<gpui::Div> {
        let empty = text.is_empty();
        let mut styled = StyledText::new(text.to_string());
        if !empty {
            let (start, end) = self.draft_selection();
            let plain: Hsla = ui::text().into();
            let mut highlighted: Hsla = ui::accent().into();
            highlighted.a = 0.35;
            let mut runs: Vec<TextRun> = Vec::new();
            let make_run = |len: usize, bg: Option<Hsla>| TextRun {
                len,
                font: font.clone(),
                color: plain,
                background_color: bg,
                underline: None,
                strikethrough: None,
            };
            if start > 0 {
                runs.push(make_run(start, None));
            }
            if end > start {
                runs.push(make_run(end - start, Some(highlighted)));
            }
            if end < text.len() {
                runs.push(make_run(text.len() - end, None));
            }
            styled = styled.with_runs(runs);
        }
        let live_layout = styled.layout().clone();
        let prev_layout = self.review_layout.clone();
        if !empty {
            self.review_layout = Some(live_layout);
        }
        let caret_x = if empty {
            Some(px(0.))
        } else {
            prev_layout.and_then(|layout| {
                layout
                    .position_for_index(self.review_cursor.min(text.len()))
                    .map(|pos| pos.x - layout.bounds().origin.x)
            })
        };

        let mut input = div()
            .id(SharedString::from(id))
            .cursor_pointer()
            .relative()
            .flex()
            .items_center()
            .min_h(px(22.))
            .text_size(px(ui::TEXT_SIZE))
            .text_color(if empty { ui::text_dim() } else { ui::text() })
            .when(empty, |slot| slot.child(placeholder.to_string()))
            // the StyledText itself must paint: measurement happens at
            // paint time, and the caret's next-frame position_for_index
            // panics on an unmeasured handle
            .when(!empty, |slot| slot.child(styled));

        if let Some(caret_x) = caret_x {
            input = input.child(
                div()
                    .absolute()
                    .top_0()
                    .bottom_0()
                    .left_0()
                    .flex()
                    .items_center()
                    .child(div().ml(caret_x).w(px(1.5)).h(px(16.)).bg(ui::text())),
            );
        }
        input
    }

    /// The live compose box: star row, draft input with a real caret,
    /// post button, error line.
    fn render_review_compose(&mut self, app_id: &str, font: gpui::Font, cx: &mut Context<Self>) -> gpui::AnyElement {
        let (draft_rating, draft_text) = (self.review_draft.0, self.review_draft.1.clone());

        // star row: five clicks, or keys 1 to 5
        let mut star_row = div().flex().items_center().gap_1();
        for value in 1u8..=5 {
            let filled = draft_rating.is_some_and(|r| r >= value as f32);
            star_row = star_row.child(
                div()
                    .id(SharedString::from(format!("star-{value}")))
                    .cursor_pointer()
                    .text_size(px(ui::TEXT_SIZE_EMPHASIS))
                    .text_color(if filled { ui::accent() } else { ui::text_dim() })
                    .hover(|style| style.text_color(ui::accent()))
                    .on_click(cx.listener(move |this, _e: &ClickEvent, _w, cx| {
                        this.review_draft.0 = Some(value as f32);
                        this.review_error = None;
                        cx.notify();
                    }))
                    .child("★"),
            );
        }

        let input = self.render_text_input(
            "review-input",
            &draft_text,
            "What did you think? Optional; Enter posts your review.",
            font,
        );
        let mut compose = div()
            .flex()
            .flex_col()
            .gap_2()
            .rounded_sm()
            .border_1()
            .border_color(ui::accent())
            .px_3()
            .py_2()
            .child(star_row)
            .child(input);

        compose = compose.child(
            div()
                .flex()
                .items_center()
                .justify_between()
                .child(
                    div()
                        .text_size(px(ui::TEXT_SIZE_LABEL))
                        .text_color(ui::text_dim())
                        .child(if self.review_posting {
                            "Posting…"
                        } else {
                            "Enter posts, Esc keeps editing, 1 to 5 rate"
                        }),
                )
                .child(
                    button("btn-post-review", None, "Post review", Tone::Accent).on_click(
                        cx.listener({
                            let app_id = app_id.to_string();
                            move |this, _e: &ClickEvent, _w, cx| {
                                this.post_review(&app_id, cx);
                            }
                        }),
                    ),
                ),
        );

        if let Some(error) = &self.review_error {
            compose = compose.child(
                div()
                    .text_size(px(ui::TEXT_SIZE_LABEL))
                    .text_color(ui::risk_color(Risk::Medium))
                    .child(error.clone()),
            );
        }

        compose.into_any_element()
    }
}

/// Five-star text: rounded to the nearest star, filled over dim.
fn stars(rating: f32) -> String {
    let filled = rating.round().clamp(0.0, 5.0) as usize;
    let mut out = String::new();
    for _ in 0..filled {
        out.push('★');
    }
    for _ in filled..5 {
        out.push('☆');
    }
    out
}

/// Human relative time for a review's created_at (unix seconds).
fn relative_time(secs: i64) -> String {
    let delta = (chrono_now() - secs).max(0);
    const MINUTE: i64 = 60;
    const HOUR: i64 = 60 * MINUTE;
    const DAY: i64 = 24 * HOUR;
    match delta {
        0..=MINUTE => "just now".into(),
        d if d < HOUR => format!("{} minutes ago", d / MINUTE),
        d if d < DAY => format!("{} hours ago", d / HOUR),
        d => format!("{} days ago", d / DAY),
    }
}

fn chrono_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn render_permissions_static(metadata: &str) -> gpui::AnyElement {
    // Borrow-free re-render of the permissions section (the method version
    // cannot be called from a non-self context).
    let grants = permissions::grants_from_metadata(metadata);
    div()
        .flex()
        .flex_col()
        .gap_2()
        .p_3()
        .rounded_sm()
        .bg(ui::inset())
        .border_1()
        .border_color(ui::divider_soft())
        .child(
            div()
                .text_size(px(ui::TEXT_SIZE_EMPHASIS))
                .font_weight(gpui::FontWeight::SEMIBOLD)
                .text_color(ui::text())
                .child("Permissions"),
        )
        .when(grants.is_empty(), |section| {
            section.child(
                div()
                    .text_size(px(ui::TEXT_SIZE))
                    .text_color(ui::text_dim())
                    .child("No sandbox grants declared (app runs with flatpak defaults)."),
            )
        })
        .children(grants.iter().map(|grant| {
            // the badge is fixed; the texts live in a shrinkable column
            // so long permission lines wrap inside the box instead of
            // painting past it
            div()
                .flex()
                .items_start()
                .gap_2()
                .text_size(px(ui::TEXT_SIZE))
                .child(
                    badge(risk_label(grant.risk), ui::risk_color(grant.risk)).flex_none(),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .flex()
                        .flex_col()
                        .gap_0p5()
                        .child(
                            div()
                                .w_full()
                                .text_color(ui::text())
                                .child(grant.summary.clone()),
                        )
                        .children((!grant.arg.is_empty()).then(|| {
                            div()
                                .w_full()
                                .text_color(ui::text_dim())
                                .child(grant.arg.clone())
                        })),
                )
        }))
        .into_any_element()
}

impl Render for StoreRoot {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let page_open = self.page_open;
        let selected = self.selected_app().cloned();
        let filter = self.filter;
        let total = self.apps.len();
        let installed_count = self.installed.len();
        let search_focused = self.focus_handle.is_focused(window);

        div()
            .key_context("StoreRoot")
            .track_focus(&self.focus_handle)
            .on_key_down(cx.listener(Self::key_down))
            .size_full()
            .relative()
            .flex()
            .flex_col()
            .bg(ui::inset())
            .text_color(ui::text())
            // the dropdown click-away overlays paint behind everything,
            // so their close handler runs after every content handler
            // in the bubble chain: a trigger's own toggle (which stops
            // propagation) wins, instead of losing to the close and
            // reopening
            .children(self.render_menu_overlays(cx))
            // top bar: the mark, the search centered across the full
            // window width, channel and account controls on the right
            .child(
                div()
                    .relative()
                    .h(px(46.))
                    .flex()
                    .items_center()
                    .gap_3()
                    .px_4()
                    .border_b_1()
                    .border_color(ui::divider())
                    .bg(ui::panel())
                    // the mark: an About popover trigger, not a dead
                    // decoration (version, license, source)
                    .child(
                        div()
                            .id("about-trigger")
                            .flex_none()
                            .px_1()
                            .py_0p5()
                            .rounded_sm()
                            .cursor_pointer()
                            .hover(|style| style.bg(ui::surface()))
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(|this, _event: &MouseDownEvent, _w, cx| {
                                    let open = this.about_modal;
                                    this.channel_menu = None;
                                    this.account_menu = None;
                                    this.sort_menu = None;
                                    this.about_modal = !open;
                                    cx.stop_propagation();
                                    cx.notify();
                                }),
                            )
                            .children(crate::cache::orchard_mark().map(|path| {
                                gpui::img(path).w(px(22.)).h(px(22.))
                            })),
                    )
                    // the search, centered on the window width: an
                    // absolute layer spanning the bar, content centered
                    .child(
                        div()
                            .absolute()
                            .left_0()
                            .right_0()
                            .top_0()
                            .bottom_0()
                            .flex()
                            .items_center()
                            .justify_center()
                            .child({
                        // The search input: a real caret and selection.
                        // StyledText carries the selection as a styled
                        // run; the layout handle maps byte indices to
                        // pixels (caret x, click position). The caret
                        // queries last frame's handle: one frame stale,
                        // invisible at 120fps.
                        let display: String = self.search.clone();
                        let placeholder = self.search.is_empty();
                        let font = window.text_style().font();
                        let mut styled = StyledText::new(display.clone());
                        if !self.search.is_empty() {
                            let (start, end) = self.selection();
                            let plain: Hsla = ui::text().into();
                            let mut highlighted: Hsla = ui::accent().into();
                            highlighted.a = 0.35;
                            let mut runs: Vec<TextRun> = Vec::new();
                            let make_run = |len: usize, bg: Option<Hsla>| TextRun {
                                len,
                                font: font.clone(),
                                color: plain,
                                background_color: bg,
                                underline: None,
                                strikethrough: None,
                            };
                            if start > 0 {
                                runs.push(make_run(start, None));
                            }
                            if end > start {
                                runs.push(make_run(end - start, Some(highlighted)));
                            }
                            if end < self.search.len() {
                                runs.push(make_run(self.search.len() - end, None));
                            }
                            styled = styled.with_runs(runs);
                        }
                        let prev_layout = self.search_layout.clone();
                        let live_layout = styled.layout().clone();
                        // only a painted text can be queried next frame:
                        // an unrendered layout panics on measurement
                        if !placeholder {
                            self.search_layout = Some(live_layout.clone());
                        }

                        // the caret queries last frame's handle; it has
                        // been measured by now. With the placeholder
                        // showing, the caret belongs at the slot's left
                        // edge without querying anything.
                        let caret_x = if placeholder {
                            Some(px(0.))
                        } else {
                            prev_layout.and_then(|layout| {
                                layout
                                    .position_for_index(self.cursor.min(self.search.len()))
                                    .map(|pos| pos.x - layout.bounds().origin.x)
                            })
                        };

                        div()
                            .id("search-box")
                            .max_w(px(460.))
                            .flex_1()
                            .flex()
                            .items_center()
                            .px_3()
                            .py_1()
                            .rounded_sm()
                            .bg(ui::inset())
                            .border_1()
                            .border_color(if search_focused {
                                ui::accent()
                            } else {
                                ui::divider()
                            })
                            .gap_2()
                            .text_size(px(ui::TEXT_SIZE))
                            .text_color(if self.search.is_empty() {
                                ui::text_dim()
                            } else {
                                ui::text()
                            })
                            .child(ui::icon("icons/search.svg", 13., ui::text_dim()))
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(move |this, event: &MouseDownEvent, _window, _cx| {
                                    // place the caret where clicked; one
                                    // frame of layout lag, invisible in
                                    // practice. Empty input: everything
                                    // lands at the slot's left edge
                                    if this.search.is_empty() {
                                        this.cursor = 0;
                                        this.anchor = None;
                                        return;
                                    }
                                    let local = event.position - live_layout.bounds().origin;
                                    let ix = match live_layout.index_for_position(local) {
                                        Ok(ix) | Err(ix) => ix,
                                    };
                                    let ix = ix.min(this.search.len());
                                    if event.modifiers.shift && this.anchor.is_some() {
                                        this.cursor = ix;
                                    } else {
                                        this.cursor = ix;
                                        this.anchor = None;
                                    }
                                }),
                            )
                            .child(
                                div()
                                    .relative()
                                    .flex_1()
                                    .min_w_0()
                                    .when(placeholder, |slot| {
                                        // the placeholder sits a step
                                        // right of the caret slot so the
                                        // caret does not touch the "T"
                                        slot.child(
                                            div()
                                                .pl(px(4.))
                                                .text_color(ui::text_dim())
                                                .child("Type to search…"),
                                        )
                                    })
                                    .when(!placeholder, |slot| slot.child(styled))
                                    .children(caret_x.map(|x| {
                                        div()
                                            .absolute()
                                            .left(x)
                                            .top(px(4.))
                                            .w(px(1.5))
                                            .h(px(15.))
                                            .bg(ui::accent())
                                    })),
                            )
                    }),
                    )
                    // the flex_1 keeps the right-hand controls at the
                    // edge; the absolute layer above centers the search
                    .child(div().flex_1())
                    .child(self.render_channel_trigger(filter, cx))
                    // signed out, settings needs a home: the gear (the
                    // signed-in menu carries it; never both at once)
                    .children((self.reviewer.is_none()).then(|| {
                        div()
                            .id("settings-trigger")
                            .px_2()
                            .py_1()
                            .rounded_sm()
                            .cursor_pointer()
                            .hover(|style| style.bg(ui::surface()))
                            .tooltip(|_window, cx| {
                                cx.new(|_| TextTooltip("Settings".into())).into()
                            })
                            .on_click(cx.listener(|this, _e: &ClickEvent, _w, cx| {
                                this.open_settings(cx);
                            }))
                            .child(ui::icon("icons/gear.svg", 15., ui::text_dim()))
                    }))
                    .child(self.render_account_trigger(cx)),
            )
            // body: settings, the full app page, or browse (category
            // chips + virtualized list)
            .child(if self.settings_open {
                self.render_settings(window.text_style().font(), cx)
            } else if self.collection_view.is_some() {
                self.render_collection(cx)
            } else if page_open {
                match selected {
                    Some(app) => self.render_page(app, window.text_style().font(), cx).into_any_element(),
                    None => self.render_browse(cx),
                }
            } else {
                self.render_browse(cx)
            })
            // status bar
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_3()
                    .px_4()
                    .py_1()
                    .border_t_1()
                    .border_color(ui::divider())
                    .bg(ui::panel())
                    .text_size(px(ui::TEXT_SIZE_SMALL))
                    .text_color(ui::text_dim())
                    .child(match self.flathub_loading {
                        true => format!("{total} apps (loading Flathub…) · {installed_count} installed"),
                        false => format!("{total} apps · {installed_count} installed"),
                    })
                    .child(div().flex_1())
                    // the running job, right-aligned: stage, percent,
                    // elapsed; logistics only, never a judgment
                    .children(self.jobs.values().next().map(|job| {
                        div()
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .child(job.status_text())
                    }))
                    // quiet non-error receipts: the cancel, the
                    // one-at-a-time refusal
                    .children(self.job_note.clone().map(|note| {
                        div()
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .child(note)
                    }))
                    .children(self.error.clone().map(|error| {
                        div()
                            .text_color(ui::urgent())
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .child(format!("⚠ {error}"))
                    })),
            )
            .when_some(self.lightbox.clone(), |root, url| {
                // the lightbox: a dimmed overlay over everything with
                // the fetched image contained in the viewport; any
                // click or Escape dismisses it
                root.child(
                    div()
                        .id("lightbox")
                        .absolute()
                        .top_0()
                        .left_0()
                        .w_full()
                        .h_full()
                        .bg(gpui::rgba(0x000000B3))
                        .flex()
                        .items_center()
                        .justify_center()
                        .p_6()
                        .cursor_pointer()
                        .on_click(cx.listener(|this, _e: &ClickEvent, _w, cx| {
                            this.lightbox = None;
                            cx.notify();
                        }))
                        .children(
                            self.fetched
                                .get(&url)
                                .and_then(|slot| slot.clone())
                                .map(|path| {
                                    gpui::img(path)
                                        .size_full()
                                        .object_fit(gpui::ObjectFit::Contain)
                                }),
                        ),
                )
            })
            // dropdown panels and the header sign-in panel paint after
            // everything (the lightbox included)
            .when(self.any_layer_open(), |root| {
                root.child(self.render_layers(window.text_style().font(), cx))
            })
            .id("store-root")
            .on_click(cx.listener(|this, _e: &ClickEvent, window, _cx| {
                // Keep the keyboard attached to the search context.
                window.focus(&this.focus_handle);
            }))
    }
}

impl StoreRoot {
    /// Is any window-level layer (channel dropdown, account dropdown,
    /// sign-in modal) showing?
    fn any_layer_open(&self) -> bool {
        self.channel_menu.is_some() || self.account_menu.is_some() || self.sort_menu.is_some()
            || self.about_modal
            || self.signin_modal
    }

    /// The window-level layers, if any: the dropdown panels and the
    /// sign-in modal, painted after everything (the lightbox
    /// included). The click-away overlays they pair with live behind
    /// the content instead (render_menu_overlays). The panel must
    /// swallow mouse down for exactly this reason: the behind-overlay's
    /// close would otherwise fire after the panel's own handlers and
    /// unmount the panel between the user's down and up, and a click
    /// whose element is gone before the up never fires.
    /// The modal paints last: it covers the dropdowns too.
    fn render_layers(&mut self, font: gpui::Font, cx: &mut Context<Self>) -> gpui::AnyElement {
        let mut layers = div().absolute().inset_0();
        if self.channel_menu.is_some() {
            layers = layers.child(self.render_channel_menu(cx));
        }
        if self.sort_menu.is_some() {
            layers = layers.child(self.render_sort_menu(cx));
        }
        if self.account_menu.is_some() {
            layers = layers.child(self.render_account_menu(cx));
        }
        if self.signin_modal {
            layers = layers.child(self.render_signin_modal(font, cx));
        }
        if self.about_modal {
            layers = layers.child(self.render_about_modal(cx));
        }
        layers.into_any_element()
    }

    /// The click-away overlay for the open dropdowns, painted behind
    /// everything: transparent, full-window, and last in the bubble
    /// chain. Any click the content does not stop lands here and
    /// closes the dropdowns.
    fn render_menu_overlays(&mut self, cx: &mut Context<Self>) -> Option<gpui::AnyElement> {
        if self.channel_menu.is_none()
            && self.account_menu.is_none()
            && self.sort_menu.is_none()
        {
            return None;
        }
        Some(
            div()
                .id("menu-overlay")
                .absolute()
                .inset_0()
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, _e: &MouseDownEvent, _w, cx| {
                        this.channel_menu = None;
                        this.account_menu = None;
                        this.sort_menu = None;
                        cx.notify();
                    }),
                )
                .into_any_element(),
        )
    }

    /// The sort dropdown: the list order options with the active one
    /// marked. Same machinery as the channel menu: the panel stops
    /// mouse down so its clicks survive the behind-everything overlay.
    fn render_sort_menu(&mut self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let anchor = self.sort_menu.unwrap_or_default();
        let left = (anchor.x - px(60.)).max(px(8.));
        div()
            .absolute()
            .left(left)
            .top(anchor.y + px(6.))
            .w(px(180.))
            .flex()
            .flex_col()
            .rounded_sm()
            .border_1()
            .border_color(ui::divider())
            .bg(ui::panel())
            .py_1()
            .on_mouse_down(
                MouseButton::Left,
                |_e: &MouseDownEvent, _w, cx: &mut gpui::App| cx.stop_propagation(),
            )
            .children(ListSort::all().iter().copied().map(|sort| {
                let active = sort == ListSort::from_settings(&self.settings.get().sort);
                div()
                    .id(SharedString::from(format!("sort-item-{}", sort.key())))
                    .flex()
                    .items_center()
                    .justify_between()
                    .px_3()
                    .py_1()
                    .cursor_pointer()
                    .text_size(px(ui::TEXT_SIZE))
                    .text_color(if active { ui::accent() } else { ui::text() })
                    .hover(|style| style.bg(ui::surface()))
                    .on_click(cx.listener(move |this, _e: &ClickEvent, _w, cx| {
                        let key = sort.key();
                        this.edit_settings(|settings| settings.sort = key.to_string(), cx);
                        this.sort_menu = None;
                        cx.notify();
                    }))
                    .child(sort.label().to_string())
                    .children(active.then(|| "✓".to_string()))
            }))
            .into_any_element()
    }

    /// The channel dropdown: filter options with the active one marked.
    /// The panel stops mouse down so its items' clicks survive to the
    /// up, out of reach of the behind-everything overlay.
    fn render_channel_menu(&mut self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let anchor = self.channel_menu.unwrap_or_default();
        let left = (anchor.x - px(60.)).max(px(8.));
        div()
            .absolute()
            .left(left)
            .top(anchor.y + px(6.))
            .w(px(180.))
            .flex()
            .flex_col()
            .rounded_sm()
            .border_1()
            .border_color(ui::divider())
            .bg(ui::panel())
            .py_1()
            .on_mouse_down(
                MouseButton::Left,
                |_e: &MouseDownEvent, _w, cx: &mut gpui::App| cx.stop_propagation(),
            )
            .children(
                // a hidden source loses its menu entry, and a single
                // visible source loses its own entry too: when one
                // source is hidden, All already is that channel
                [
                    Filter::All,
                    Filter::Reviewed,
                    Filter::Publisher,
                    Filter::Installed,
                ]
                .iter()
                .filter(|f| match f {
                    Filter::Reviewed | Filter::Publisher => {
                        self.settings.get().flathub_source && self.settings.get().orchard_source
                    }
                    _ => true,
                })
                .map(|f| {
                    let active = *f == self.filter;
                    div()
                        .id(SharedString::from(format!("channel-item-{}", f.label())))
                        .flex()
                        .items_center()
                        .justify_between()
                        .px_3()
                        .py_1()
                        .cursor_pointer()
                        .text_size(px(ui::TEXT_SIZE))
                        .text_color(if active {
                            ui::accent()
                        } else {
                            ui::text()
                        })
                        .hover(|style| style.bg(ui::surface()))
                        .on_click(cx.listener(move |this, _e: &ClickEvent, _w, cx| {
                            this.filter = *f;
                            this.channel_menu = None;
                            // a pending install confirm does not
                            // survive a channel switch
                            this.confirm_install = None;
                            this.confirm_remove = None;
                            this.confirm_update = None;
                            cx.notify();
                        }))
                        .child(f.label().to_string())
                        .children(active.then(|| "✓".to_string()))
                }),
            )
            .into_any_element()
    }

    /// The account dropdown: the signed-in session and sign-out. The
    /// panel stops mouse down, same as the channel menu: Sign out's
    /// click survives to the up, out of reach of the behind-everything
    /// overlay.
    fn render_account_menu(&mut self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let anchor = self.account_menu.unwrap_or_default();
        let left = (anchor.x - px(170.)).max(px(8.));
        let (display, signer) = match self.reviewer.as_ref() {
            Some(reviewer) => (reviewer.display(), Some(reviewer.signer_pubkey.clone())),
            None => (String::new(), None),
        };
        let note = signer
            .as_deref()
            .and_then(|pubkey| self.verified_nip05(pubkey))
            .and_then(|claim| {
                split_nip05(&claim)
                    .map(|(local, domain)| format!("{domain} lists {local} as this key"))
            });
        div()
            .absolute()
            .left(left)
            .top(anchor.y + px(6.))
            .w(px(220.))
            .flex()
            .flex_col()
            .rounded_sm()
            .border_1()
            .border_color(ui::divider())
            .bg(ui::panel())
            .py_1()
            .on_mouse_down(
                MouseButton::Left,
                |_e: &MouseDownEvent, _w, cx: &mut gpui::App| cx.stop_propagation(),
            )
            .child(
                div()
                    .px_3()
                    .py_1()
                    .flex()
                    .gap_1()
                    .items_center()
                    .text_size(px(ui::TEXT_SIZE))
                    .text_color(ui::text())
                    .child(format!("Signed in as {display}"))
                    .children(note.map(|note| {
                        // the domain backed this identity's claim;
                        // the words live on hover
                        div()
                            .id("account-nip05")
                            .flex_none()
                            .hover(|style| style.bg(gpui::transparent_black()))
                            .tooltip(move |_window, cx| {
                                cx.new(|_| TextTooltip(note.clone().into())).into()
                            })
                            .child(ui::icon("icons/check.svg", 11., ui::text()))
                    })),
            )
            .child(
                div()
                    .id("account-settings")
                    .px_3()
                    .py_1()
                    .cursor_pointer()
                    .text_size(px(ui::TEXT_SIZE))
                    .text_color(ui::text())
                    .hover(|style| style.bg(ui::surface()))
                    .on_click(cx.listener(|this, _e: &ClickEvent, _w, cx| {
                        this.open_settings(cx);
                    }))
                    .child("Settings"),
            )
            .child(
                div()
                    .id("account-sign-out")
                    .px_3()
                    .py_1()
                    .cursor_pointer()
                    .text_size(px(ui::TEXT_SIZE))
                    .text_color(ui::text())                    .hover(|style| style.bg(ui::surface()))
                    .on_click(cx.listener(|this, _e: &ClickEvent, _w, cx| {
                        this.account_menu = None;
                        this.sign_out(cx);
                    }))
                    .child("Sign out"),
            )
            .into_any_element()
    }

    // ----- settings page -----

    /// A section header: the page's card label, the detail page's
    /// pattern (icon, medium text).
    fn settings_section(
        &self,
        glyph: &'static str,
        title: &str,
        body: gpui::AnyElement,
    ) -> gpui::Div {
        div()
            .flex()
            .flex_col()
            .gap_2()
            .w_full()
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_1p5()
                    .text_size(px(ui::TEXT_SIZE_EMPHASIS - 1.))
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .text_color(ui::text())
                    .child(ui::icon(glyph, 13., ui::text()))
                    .child(title.to_string()),
            )
            .child(body)
    }

    /// A settings row: label and note left, control right.
    fn settings_row(
        &self,
        label: &str,
        note: &str,
        control: impl IntoElement,
    ) -> gpui::Div {
        div()
            .flex()
            .items_center()
            .justify_between()
            .gap_4()
            .px_3()
            .py_2()
            .rounded_sm()
            .bg(ui::panel())
            .border_1()
            .border_color(ui::divider())
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_0p5()
                    .min_w_0()
                    .child(
                        div()
                            .text_size(px(ui::TEXT_SIZE))
                            .text_color(ui::text())
                            .child(label.to_string()),
                    )
                    .child(
                        div()
                            .text_size(px(ui::TEXT_SIZE_SMALL))
                            .text_color(ui::text_dim())
                            .child(note.to_string()),
                    ),
            )
            .child(control)
    }

    /// A segmented control: the options as chips, the active one in
    /// accent, matching the filter chips.
    fn settings_segmented(
        &self,
        prefix: &'static str,
        options: &[(&'static str, &'static str)],
        active: &str,
        on_pick: impl Fn(&mut Self, &'static str, &mut Context<Self>) + 'static,
        cx: &mut Context<Self>,
    ) -> gpui::Div {
        let on_pick = std::sync::Arc::new(on_pick);
        div()
            .flex()
            .flex_none()
            .gap_1()
            .children(options.iter().map(|(name, label)| {
                let (name, label) = (*name, *label);
                let active = name == active;
                let id = SharedString::from(format!("settings-{prefix}-{name}"));
                let on_pick = std::sync::Arc::clone(&on_pick);
                div()
                    .id(id)
                    .px_2()
                    .py_0p5()
                    .rounded_sm()
                    .text_size(px(ui::TEXT_SIZE_SMALL))
                    .cursor_pointer()
                    .when(active, |chip| {
                        chip.bg(ui::accent())
                            .text_color(ui::accent_fg())
                            .font_weight(gpui::FontWeight::MEDIUM)
                    })
                    .when(!active, |chip| {
                        chip.bg(ui::surface())
                            .border_1()
                            .border_color(ui::divider())
                            .text_color(ui::text_dim())
                            .hover(|style| style.text_color(ui::text()).bg(ui::surface_hover()))
                    })
                    .on_click(cx.listener(move |this, _e: &ClickEvent, _w, cx| {
                        on_pick(this, name, cx);
                    }))
                    .child(label.to_string())
            }))
    }

    /// One relay group editor: the effective set as rows (the
    /// built-in rendezvous set until the file carries its own list),
    /// an add input, reset. Edits persist immediately and reach the
    /// relay threads within a second; no restart note needed, because
    /// there is no restart.
    fn render_relay_group(
        &mut self,
        target: RelayTarget,
        title: &str,
        note: &str,
        env: Option<(&'static str, String)>,
        font: gpui::Font,
        cx: &mut Context<Self>,
    ) -> gpui::Div {
        let settings = self.settings.get();
        let custom = self.relay_urls_for(target, &settings);
        let defaults = orchard_nostr::relay::relay_urls(None);
        // an empty file list means the built-in set: show it, so the
        // active relays are visible and individually removable
        let showing_defaults = custom.is_empty();
        let urls = if showing_defaults {
            defaults.clone()
        } else {
            custom.clone()
        };
        let focused = self.relay_target == Some(target);
        let slot = match target {
            RelayTarget::Events => 0,
            RelayTarget::Social => 1,
        };
        let group_id = format!("relay-group-{}", slot);

        // the input: draft when this group has the keyboard, the
        // placeholder otherwise. The caret queries last frame's layout
        // (measured by now); the fresh layout is stored for next frame.
        let show_draft = focused;
        let draft_text = if show_draft {
            self.relay_draft.clone()
        } else {
            String::new()
        };
        let placeholder = draft_text.is_empty();
        let mut styled = StyledText::new(if placeholder {
            "wss://relay.example".to_string()
        } else {
            draft_text.clone()
        });
        if placeholder {
            styled = styled.with_runs(vec![TextRun {
                len: "wss://relay.example".len(),
                font: font.clone(),
                color: ui::text_dim().into(),
                background_color: None,
                underline: None,
                strikethrough: None,
            }]);
        } else {
            let (start, end) = self.relay_selection();
            let plain: Hsla = ui::text().into();
            let mut highlighted: Hsla = ui::accent().into();
            highlighted.a = 0.35;
            let make_run = |len: usize, bg: Option<Hsla>| TextRun {
                len,
                font: font.clone(),
                color: plain,
                background_color: bg,
                underline: None,
                strikethrough: None,
            };
            let mut runs: Vec<TextRun> = Vec::new();
            if start > 0 {
                runs.push(make_run(start, None));
            }
            if end > start {
                runs.push(make_run(end - start, Some(highlighted)));
            }
            if end < draft_text.len() {
                runs.push(make_run(draft_text.len() - end, None));
            }
            if !runs.is_empty() {
                styled = styled.with_runs(runs);
            }
        }
        let live_layout = styled.layout().clone();
        let prev_layout = self.relay_layouts[slot].clone();
        if show_draft && !placeholder {
            self.relay_layouts[slot] = Some(live_layout.clone());
        }
        let caret_x = if !show_draft || placeholder {
            None
        } else {
            prev_layout.and_then(|layout| {
                layout
                    .position_for_index(self.relay_cursor.min(draft_text.len()))
                    .map(|pos| pos.x - layout.bounds().origin.x)
            })
        };

        let rows = div()
            .flex()
            .flex_col()
            .gap_1()
            .children(urls.iter().enumerate().map(|(index, url)| {
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap_2()
                    .px_2()
                    .py_1()
                    .rounded_sm()
                    .bg(ui::surface())
                    .border_1()
                    .border_color(ui::divider())
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .min_w_0()
                            .overflow_hidden()
                            .child(
                                div()
                                    .text_size(px(ui::TEXT_SIZE))
                                    .text_color(ui::text())
                                    .child(url.clone()),
                            )
                            .children(if url.as_str() == orchard_nostr::relay::REQUIRED_RELAY {
                                Some(ui::badge("required", ui::accent()).into_any_element())
                            } else {
                                showing_defaults
                                    .then(|| ui::badge("built-in", ui::text_dim()).into_any_element())
                            }),
                    )
                    .children((url.as_str() != orchard_nostr::relay::REQUIRED_RELAY).then(|| {
                        ui::small_button(
                            SharedString::from(format!("{group_id}-remove-{index}")),
                            Some("icons/trash.svg"),
                            "",
                            Tone::Danger,
                        )
                        .on_click(cx.listener(move |this, _e: &ClickEvent, _w, cx| {
                            this.remove_relay(target, index, cx);
                        }))
                        .into_any_element()
                    }))
            }));

        let input = div()
            .id(SharedString::from(format!("{group_id}-input")))
            .flex_1()
            .min_w_0()
            .relative()
            .px_2()
            .py_1()
            .rounded_sm()
            .bg(ui::surface())
            .border_1()
            .border_color(if focused {
                ui::accent()
            } else {
                ui::divider()
            })
            .text_size(px(ui::TEXT_SIZE))
            .cursor_pointer()
            .on_mouse_down(MouseButton::Left, cx.listener(move |this, _e: &MouseDownEvent, _w, cx| {
                if this.relay_target != Some(target) {
                    this.relay_target = Some(target);
                    this.relay_draft.clear();
                    this.relay_cursor = 0;
                    this.relay_anchor = None;
                    cx.notify();
                }
            }))
            .child(styled)
            .children(caret_x.map(|x| {
                div()
                    .absolute()
                    .top(px(2.))
                    .left(x)
                    .w(px(1.5))
                    .h(px(14.))
                    .bg(ui::accent())
            }));

        div()
            .flex()
            .flex_col()
            .gap_1p5()
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap_2()
                    .child(
                        div()
                            .text_size(px(ui::TEXT_SIZE))
                            .text_color(ui::text())
                            .child(title.to_string()),
                    )
                    .children((!showing_defaults).then(|| {
                        ui::small_button(
                            SharedString::from(format!("{group_id}-reset")),
                            None,
                            "Use built-in set",
                            Tone::Quiet,
                        )
                        .on_click(cx.listener(move |this, _e: &ClickEvent, _w, cx| {
                            this.reset_relays(target, cx);
                        }))
                        .into_any_element()
                    })),
            )
            .child(
                div()
                    .text_size(px(ui::TEXT_SIZE_SMALL))
                    .text_color(ui::text_dim())
                    .child(note.to_string()),
            )
            .child(rows)
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(input)
                    .child(
                        ui::small_button(
                            SharedString::from(format!("{group_id}-add")),
                            Some("icons/plus.svg"),
                            "Add",
                            Tone::Quiet,
                        )
                        .on_click(cx.listener(move |this, _e: &ClickEvent, _w, cx| {
                            this.relay_target = Some(target);
                            this.add_relay_from_draft(cx);
                        })),
                    ),
            )
            .children(env.map(|(variable, value)| {
                div()
                    .text_size(px(ui::TEXT_SIZE_SMALL))
                    .text_color(ui::text_dim())
                    .child(format!(
                        "The ORCHARD_{variable} environment variable ({value}) overrides this list while it is set."
                    ))
            }))
    }

    /// The settings page: sources, relays, update cadence, privacy.
    /// Every control writes through the shared handle: persistence is
    /// immediate, and the embedded indexd and the client pick changes
    /// up live, no restart.
    fn render_settings(&mut self, font: gpui::Font, cx: &mut Context<Self>) -> gpui::AnyElement {
        let settings = self.settings.get();
        let env_events = std::env::var("ORCHARD_RELAYS")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .map(|value| ("RELAYS", value));
        let env_social = std::env::var("ORCHARD_SOCIAL_RELAYS")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .map(|value| ("SOCIAL_RELAYS", value));
        let (image_bytes, event_bytes) = self.cache_sizes;
        // with one source hidden there is one channel, so the startup
        // choices collapse the same way the channel menu does
        let both_visible = settings.flathub_source && settings.orchard_source;
        let channel_options: &[(&'static str, &'static str)] = if both_visible {
            &[
                ("all", "All"),
                ("flathub", "Flathub"),
                ("orchard", "Orchard"),
                ("installed", "Installed"),
            ]
        } else {
            &[("all", "All"), ("installed", "Installed")]
        };

        let sources_body = div()
            .flex()
            .flex_col()
            .gap_1p5()
            .child(self.settings_row(
                "Flathub channel",
                "Reviewed, curated apps from flathub.org.",
                div()
                    .id("settings-source-flathub")
                    .cursor_pointer()
                    .on_click(cx.listener(|this, _e: &ClickEvent, _w, cx| {
                        let visible = !this.settings.get().flathub_source;
                        this.set_source_visible(true, visible, cx);
                    }))
                    .child(ui::toggle_pill(settings.flathub_source)),
            ))
            .child(self.settings_row(
                "Orchard channel",
                "Publisher-signed apps from the Nostr catalog.",
                div()
                    .id("settings-source-orchard")
                    .cursor_pointer()
                    .on_click(cx.listener(|this, _e: &ClickEvent, _w, cx| {
                        let visible = !this.settings.get().orchard_source;
                        this.set_source_visible(false, visible, cx);
                    }))
                    .child(ui::toggle_pill(settings.orchard_source)),
            ))
            .child(self.settings_row(
                "Opens on",
                "The channel the store starts on.",
                self.settings_segmented(
                    "default-channel",
                    channel_options,
                    &settings.default_channel,
                    |this, name, cx| {
                        this.edit_settings(|s| s.default_channel = name.to_string(), cx);
                    },
                    cx,
                ),
            ));

        let relays_body = div()
            .flex()
            .flex_col()
            .gap_3()
            .child(self.render_relay_group(
                RelayTarget::Events,
                "Catalog relays",
                "Where publisher listings, releases, and reviews are read from. The built-in rows are the rendezvous set. The required relay cannot be removed.",
                env_events,
                font.clone(),
                cx,
            ))
            .child(self.render_relay_group(
                RelayTarget::Social,
                "Profile relays",
                "Where reviewer names, avatars, and contact lists come from. The built-in rows are the rendezvous set. The required relay cannot be removed.",
                env_social,
                font.clone(),
                cx,
            ));

        let updates_body = div().flex().flex_col().gap_1p5().child(self.settings_row(
            "Check for updates",
            "How often installed apps are compared with their channel's newest version.",
            self.settings_segmented(
                "update-checks",
                &[
                    ("off", "Off"),
                    ("launch", "On launch"),
                    ("hourly", "Hourly"),
                    ("daily", "Daily"),
                ],
                &settings.update_checks,
                |this, name, cx| {
                    this.edit_settings(|s| s.update_checks = name.to_string(), cx);
                },
                cx,
            ),
        ));

        let privacy_body = div().flex().flex_col().gap_1p5().child(self.settings_row(
            "Load remote images",
            "Fetch publisher-hosted icons and screenshots. Off: placeholders only.",
            div()
                .id("settings-remote-images")
                .cursor_pointer()
                .on_click(cx.listener(|this, _e: &ClickEvent, _w, cx| {
                    let visible = !this.settings.get().remote_images;
                    this.edit_settings(|s| s.remote_images = visible, cx);
                    if !visible {
                        // already-rendered images drop to placeholders
                        this.fetched.clear();
                        this.fetching.clear();
                    }
                    cx.notify();
                }))
                .child(ui::toggle_pill(settings.remote_images)),
        ));

        let image_note = format!("{} on disk.", human_size(image_bytes));
        let event_note = format!(
            "{} on disk; the relays refill them.",
            human_size(event_bytes)
        );

        div()
            .id("settings-scroll")
            .flex_1()
            .min_h_0()
            .flex()
            .flex_col()
            .overflow_y_scroll()
            .track_scroll(&self.settings_scroll)
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_4()
                    // one definite width for every section: the
                    // content stretches to it, nothing shrink-wraps
                    // wider than another
                    .w_full()
                    .max_w(px(640.))
                    .mx_auto()
                    .py_4()
                    .px_4()
                    // the page header: title plus the way out, since
                    // the keyboard is the primary navigation here
                    .child(
                        div()
                            .id("settings-back")
                            .flex()
                            .items_center()
                            .gap_1p5()
                            .cursor_pointer()
                            .text_size(px(ui::TEXT_SIZE))
                            .text_color(ui::accent())
                            .hover(|style| style.text_color(ui::text()))
                            .on_click(cx.listener(|this, _e: &ClickEvent, _w, cx| {
                                this.close_settings(cx);
                            }))
                            .child(ui::icon("icons/arrow_left.svg", 13., ui::accent()))
                            .child("Back"),
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .justify_between()
                            .child(
                                div()
                                    .text_size(px(ui::TEXT_SIZE_EMPHASIS + 2.))
                                    .font_weight(gpui::FontWeight::MEDIUM)
                                    .text_color(ui::text())
                                    .child("Settings"),
                            )
                            .child(
                                div()
                                    .text_size(px(ui::TEXT_SIZE_SMALL))
                                    .text_color(ui::text_dim())
                                    .child("Escape backs out; every change applies now"),
                            ),
                    )
                    .child(self.settings_section(
                        "icons/sprout.svg",
                        "Sources",
                        sources_body.into_any_element(),
                    ))
                    .child(self.settings_section(
                        "icons/globe.svg",
                        "Relays",
                        relays_body.into_any_element(),
                    ))
                    .child(self.settings_section(
                        "icons/update.svg",
                        "Updates",
                        updates_body.into_any_element(),
                    ))
                    .child(self.settings_section(
                        "icons/shield.svg",
                        "Privacy",
                        privacy_body.into_any_element(),
                    ))
                    .child(self.settings_section(
                        "icons/trash.svg",
                        "Cached data",
                        // both clears are destructive on the first
                        // press, so they wear the two-press idiom
                        // every other destructive button wears
                        div()
                            .flex()
                            .flex_col()
                            .gap_1p5()
                            .child(self.settings_row(
                                "Clear cached images",
                                image_note.as_str(),
                                ui::small_button(
                                    "settings-clear-images",
                                    None,
                                    if self.confirm_clear.as_deref()
                                        == Some("settings-clear-images")
                                    {
                                        "Sure?"
                                    } else {
                                        "Clear"
                                    },
                                    Tone::Danger,
                                )
                                .on_click(cx.listener(|this, _e: &ClickEvent, _w, cx| {
                                    if this.confirm_clear.as_deref()
                                        == Some("settings-clear-images")
                                    {
                                        this.confirm_clear = None;
                                        this.clear_image_cache(cx);
                                    } else {
                                        this.confirm_clear =
                                            Some("settings-clear-images".into());
                                        cx.notify();
                                    }
                                })),
                            ))
                            .child(self.settings_row(
                                "Clear cached events",
                                event_note.as_str(),
                                ui::small_button(
                                    "settings-clear-events",
                                    None,
                                    if self.confirm_clear.as_deref()
                                        == Some("settings-clear-events")
                                    {
                                        "Sure?"
                                    } else {
                                        "Clear"
                                    },
                                    Tone::Danger,
                                )
                                .on_click(cx.listener(|this, _e: &ClickEvent, _w, cx| {
                                    if this.confirm_clear.as_deref()
                                        == Some("settings-clear-events")
                                    {
                                        this.confirm_clear = None;
                                        this.clear_event_cache(cx);
                                    } else {
                                        this.confirm_clear =
                                            Some("settings-clear-events".into());
                                        cx.notify();
                                    }
                                })),
                            ))
                            .into_any_element(),
                    )),
            )
            .into_any_element()
    }

    /// The channel dropdown trigger: the active filter with a chevron.
    /// Mouse down, not click: the toggle stops propagation, so the
    /// behind-everything overlay never closes what the trigger just
    /// opened, and a second press closes instead of reopening.
    fn render_channel_trigger(&mut self, filter: Filter, cx: &mut Context<Self>) -> gpui::AnyElement {
        div()
            .id("channel-trigger")
            .flex()
            .items_center()
            .gap_1()
            .px_2()
            .py_1()
            .rounded_sm()
            .cursor_pointer()
            .text_size(px(ui::TEXT_SIZE))
            .text_color(ui::text())
            .hover(|style| style.bg(ui::surface()))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, event: &MouseDownEvent, _w, cx| {
                    let open = this.channel_menu.is_some();
                    this.account_menu = None;
                    this.about_modal = false;
                    this.channel_menu = if open { None } else { Some(event.position) };
                    cx.stop_propagation();
                    cx.notify();
                }),
            )
            .child(filter.label().to_string())
            .child(ui::icon("icons/chevron_down.svg", 11., ui::text_dim()))
            .into_any_element()
    }

    /// The About modal: the mark's click target, the convention the
    /// desktop trains (Help, About). Mark, version, license, source;
    /// the dimmed backdrop closes it, so does Escape.
    fn render_about_modal(&mut self, cx: &mut Context<Self>) -> gpui::AnyElement {
        div()
            .absolute()
            .inset_0()
            .flex()
            .items_center()
            .justify_center()
            // the dimmed click-away is a FIRST sibling, the same shape
            // as the sign-in modal: the panel after it wins every
            // click that lands on it, and swallows the mouse down so
            // the backdrop's close never sees it
            .child(
                div()
                    .id("about-modal-backdrop")
                    .absolute()
                    .inset_0()
                    .bg(gpui::rgba(0x000000B3))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, _e: &MouseDownEvent, _w, cx| {
                            this.about_modal = false;
                            cx.stop_propagation();
                            cx.notify();
                        }),
                    ),
            )
            .child(
                div().on_mouse_down(
                    MouseButton::Left,
                    |_e: &MouseDownEvent, _w, cx: &mut gpui::App| cx.stop_propagation(),
                ).child(
                    div()
                        .relative()
                        .w(px(260.))
                        .flex()
                        .flex_col()
                        .items_center()
                        .gap_2()
                        .rounded_md()
                        .border_1()
                        .border_color(ui::divider())
                        .bg(ui::panel())
                        .p_4()
                        .child(
                            div()
                                .id("about-modal-close")
                                .absolute()
                                .top_2()
                                .right_2()
                                .flex()
                                .items_center()
                                .justify_center()
                                .w(px(22.))
                                .h(px(22.))
                                .rounded_sm()
                                .cursor_pointer()
                                .hover(|style| style.bg(ui::surface()))
                                .on_click(cx.listener(|this, _e: &ClickEvent, _w, cx| {
                                    this.about_modal = false;
                                    cx.notify();
                                }))
                                .child(ui::icon("icons/close.svg", 12., ui::text_dim())),
                        )
                        .children(crate::cache::orchard_mark().map(|path| {
                            gpui::img(path).w(px(40.)).h(px(40.))
                        }))
                        .child(
                            div()
                                .text_size(px(ui::TEXT_SIZE_EMPHASIS))
                                .font_weight(gpui::FontWeight::MEDIUM)
                                .text_color(ui::text())
                                .child("Orchard"),
                        )
                        .child(
                            div()
                                .text_size(px(ui::TEXT_SIZE))
                                .text_color(ui::text_dim())
                                .child(format!("Version {}", env!("CARGO_PKG_VERSION"))),
                        )
                        .child(
                            div()
                                .text_size(px(ui::TEXT_SIZE))
                                .text_color(ui::text_dim())
                                .child("MIT license"),
                        )
                        .child(
                            div()
                                .id("about-repo")
                                .mt_1()
                                .cursor_pointer()
                                .text_size(px(ui::TEXT_SIZE))
                                .text_color(ui::accent())
                                .hover(|style| style.underline())
                                .on_click(cx.listener(|this, _e: &ClickEvent, _w, cx| {
                                    this.about_modal = false;
                                    cx.open_url("https://github.com/Letdown2491/orchard");
                                }))
                                .child("Source repository"),
                        ),
                ),
            )
            .into_any_element()
    }

    /// The account control: the sign-in entry point when signed out,
    /// the session menu trigger when not.
    fn render_account_trigger(&mut self, cx: &mut Context<Self>) -> gpui::AnyElement {
        // the profile picture when the identity's kind 0 carries one;
        // fetched through the shared cache, glyph until it lands
        let picture = self
            .reviewer
            .as_ref()
            .and_then(|r| self.profiles.get(&r.signer_pubkey))
            .and_then(|p| p.picture.clone());
        if let Some(url) = &picture {
            self.ensure_fetched(vec![url.clone()], cx);
        }
        let path = picture
            .as_deref()
            .and_then(|url| self.fetched.get(url))
            .and_then(|f| f.as_deref());
        match self.reviewer.as_ref() {
            Some(_reviewer) => {
                div()
                    .id("account-trigger")
                    .flex()
                    .items_center()
                    .gap_1()
                    .px_2()
                    .py_1()
                    .rounded_sm()
                    .cursor_pointer()
                    .hover(|style| style.bg(ui::surface()))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, event: &MouseDownEvent, _w, cx| {
                            let open = this.account_menu.is_some();
                            this.channel_menu = None;
                            this.about_modal = false;
                            this.account_menu = if open {
                                None
                            } else {
                                Some(event.position)
                            };
                            cx.stop_propagation();
                            cx.notify();
                        }),
                    )
                    .child(avatar(path, 14., ui::text()))
                    .child(ui::icon("icons/chevron_down.svg", 11., ui::text_dim()))
                    .into_any_element()
            }
            None => div()
                .id("account-sign-in")
                .flex()
                .items_center()
                .px_2()
                .py_1()
                .rounded_sm()
                .cursor_pointer()
                .hover(|style| style.bg(ui::surface()))
                .tooltip(|_window, cx| cx.new(|_| TextTooltip("Sign in".into())).into())
                .on_click(cx.listener(|this, _e: &ClickEvent, _w, cx| {
                    this.open_signin_modal(false, cx);
                }))
                // the ghost avatar: the same slot the signed-in
                // avatar fills, so identity has one home in the bar
                .child(ui::icon("icons/person.svg", 15., ui::text_dim()))
                .into_any_element(),
        }
    }
}

// ----- catalog loaders (background) -----

/// Flathub channel: parse the local AppStream for the system install.
/// Session-stability fingerprint of the publisher rows: any change in
/// identity, version, pin, or listing text should re-render; anything
/// else is an idle refresh. Borrowing, not cloning: this runs every
/// refresh tick, and the rows' strings are the payload.
fn orchard_fingerprint<'a>(rows: impl IntoIterator<Item = &'a StoreApp>) -> String {
    use std::fmt::Write as _;
    let mut fingerprint = String::new();
    for app in rows {
        let _ = writeln!(
            fingerprint,
            "{}\u{1}{}\u{1}{}\u{1}{}\u{1}{}\u{1}{}\u{1}{}",
            app.app_id,
            app.publisher.as_deref().unwrap_or(""),
            app.version.as_deref().unwrap_or(""),
            app.pinned_digest.as_deref().unwrap_or(""),
            app.name,
            app.summary,
            app.successor_to.as_deref().unwrap_or(""),
        );
    }
    fingerprint
}

/// The per-publisher bookkeeping key: two publishers of one app id
/// keep separate digests, fetch states, and verify errors. Reviewed
/// apps have no publisher; their key is the bare id's shape.
fn app_key(app_id: &str, publisher: Option<&str>) -> String {
    format!("{app_id}\u{1}{}", publisher.unwrap_or(""))
}

/// Fold a catalog batch of `(app id, publisher key)` pairs into the
/// continuity record. Keys new for an app id met before are flagged
/// as a change (the fact sticks); an app id met for the first time
/// seeds all its keys unflagged: a fresh client has no history to
/// disclose. True when the record changed.
fn absorb_continuity(
    file: &mut orchard_core::ContinuityFile,
    batch: &[(String, String)],
) -> bool {
    let mut current: std::collections::BTreeMap<&str, Vec<&str>> = Default::default();
    for (app_id, pubkey) in batch {
        current.entry(app_id).or_default().push(pubkey);
    }
    let mut changed = false;
    for (app_id, keys) in &current {
        let had_history = file.apps.contains_key(*app_id);
        let entry = file.apps.entry((*app_id).to_string()).or_default();
        for key in keys {
            if entry.keys.iter().any(|known| known == key) {
                continue;
            }
            changed = true;
            if had_history {
                entry.flagged.push((*key).to_string());
            }
            entry.keys.push((*key).to_string());
        }
    }
    changed
}

/// The continuity record lives beside identity.json: private client
/// state, never published.
fn continuity_path() -> Option<std::path::PathBuf> {
    identity_path().map(|path| path.with_file_name("continuity.json"))
}

/// Load the continuity record. A missing or unreadable file is a
/// fresh client, not an error.
fn load_continuity() -> orchard_core::ContinuityFile {
    let Some(path) = continuity_path() else {
        return Default::default();
    };
    std::fs::read(&path)
        .map(|bytes| serde_json::from_slice(&bytes).unwrap_or_default())
        .unwrap_or_default()
}

fn save_continuity(file: &orchard_core::ContinuityFile) {
    let Some(path) = continuity_path() else {
        return;
    };
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(bytes) = serde_json::to_vec_pretty(file) {
        let _ = std::fs::write(&path, bytes);
    }
}

/// Human byte count, flatpak-style SI units: `9.8 MB`, `1.2 GB`.
/// ASCII-case-insensitive containment, allocation-free: the needle is
/// folded once, the haystack scanned byte-wise (safe on UTF-8:
/// multi-byte sequences are left intact, so an ASCII byte only ever
/// matches at a character boundary). A non-ASCII needle falls back to
/// the allocating Unicode fold.
fn ci_contains(haystack: &str, needle: &str) -> bool {
    if needle.is_empty() {
        return true;
    }
    if !needle.is_ascii() {
        return haystack
            .to_lowercase()
            .contains(&needle.to_lowercase());
    }
    let needle: Vec<u8> = needle
        .bytes()
        .map(|byte| byte.to_ascii_lowercase())
        .collect();
    haystack
        .as_bytes()
        .windows(needle.len())
        .any(|window| {
            window
                .iter()
                .zip(needle.iter())
                .all(|(byte, want)| byte.to_ascii_lowercase() == *want)
        })
}

/// ASCII-case-insensitive name order: no allocation per comparison,
/// unlike to_lowercase, which the sort comparator ran twice per pair.
/// Non-ASCII falls back to the Unicode fold.
fn ci_cmp(a: &str, b: &str) -> std::cmp::Ordering {
    if !a.is_ascii() || !b.is_ascii() {
        return a.to_lowercase().cmp(&b.to_lowercase());
    }
    a.bytes()
        .map(|byte| byte.to_ascii_lowercase())
        .cmp(b.bytes().map(|byte| byte.to_ascii_lowercase()))
}

/// The list's date order: rank on the newest release date that has
/// actually arrived. A future-dated release never lifts an app toward
/// the top (an upstream can pre-list one, so a raw sort rewards the
/// boldest claim), and an app whose dates are all in the future ranks
/// as undated: it sinks with the undated, name breaking ties, and
/// rises by itself the day its date arrives. `today` is an ISO date.
fn date_order(a: &StoreApp, b: &StoreApp, today: &str) -> std::cmp::Ordering {
    // the claim stays on the app (the page shows it as "dated"), it
    // just does not rank until it is true. A fn, not a closure: the
    // return lifetime is the app's, not the captured `today`'s.
    fn rank<'a>(app: &'a StoreApp, today: &str) -> Option<&'a str> {
        let date = app.release_date.as_deref()?;
        (date <= today).then_some(date)
    }
    match (rank(b, today), rank(a, today)) {
        (Some(x), Some(y)) => x
            .cmp(y)
            .then_with(|| ci_cmp(&a.name, &b.name)),
        // b dated, a undated: a sinks (undated apps sort last)
        (Some(_), None) => std::cmp::Ordering::Greater,
        // a dated, b undated: a first
        (None, Some(_)) => std::cmp::Ordering::Less,
        (None, None) => ci_cmp(&a.name, &b.name),
    }
}

/// The catalog list order, persisted in settings: recency by
/// default, alphabetical. Always a chosen option, never a hidden
/// reordering: the client curates nothing the user did not pick.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ListSort {
    Updated,
    Name,
}

impl ListSort {
    fn from_settings(sort: &str) -> Self {
        match sort {
            "name" => ListSort::Name,
            _ => ListSort::Updated,
        }
    }

    fn label(&self) -> &'static str {
        match self {
            ListSort::Updated => "Updated",
            ListSort::Name => "Name",
        }
    }

    /// The settings-file spelling of this sort.
    fn key(&self) -> &'static str {
        match self {
            ListSort::Updated => "updated",
            ListSort::Name => "name",
        }
    }

    fn all() -> [ListSort; 2] {
        [ListSort::Updated, ListSort::Name]
    }
}

/// One catalog list ordering. Recency is the arrived-only date rank;
/// name is alphabetical.
fn list_sort(a: &StoreApp, b: &StoreApp, sort: ListSort, today: &str) -> std::cmp::Ordering {
    match sort {
        ListSort::Updated => date_order(a, b, today),
        ListSort::Name => ci_cmp(&a.name, &b.name)
            .then_with(|| a.app_id.cmp(&b.app_id)),
    }
}

fn human_size(bytes: u64) -> String {
    const UNITS: [(&str, u64); 4] = [
        ("GB", 1_000_000_000),
        ("MB", 1_000_000),
        ("kB", 1_000),
        ("B", 1),
    ];
    for (unit, size) in UNITS {
        if bytes >= size {
            let value = bytes as f64 / size as f64;
            return format!("{value:.1} {unit}");
        }
    }
    format!("{bytes} B")
}

/// The honest state for an app whose chain exists but builds nothing
/// for this machine. This one line is the whole arch story: quiet
/// everywhere else, told in full exactly where it matters.
/// One clickable project link row: an icon, the URL, the host's
/// default handler on click. Homepage and repository share the shape;
/// `verified` adds the repo-verified check beside the link.
fn project_link(
    id: &'static str,
    icon: &'static str,
    url: String,
    verified: bool,
) -> impl IntoElement {
    let open = url.clone();
    div()
        .id(SharedString::from(format!("link-{id}")))
        .flex()
        .items_center()
        .gap_1p5()
        .cursor_pointer()
        .text_size(px(ui::TEXT_SIZE))
        .text_color(ui::accent())
        .hover(|style| style.text_color(ui::text()))
        .on_click(move |_e: &ClickEvent, _window, _cx| {
            // host default handler, fire and forget
            let _ = std::process::Command::new("xdg-open").arg(&open).spawn();
        })
        .child(ui::icon(icon, 13., ui::accent()))
        .child(url)
        .when(verified, |link| {
            link.child(ui::icon(
                "icons/check.svg",
                13.,
                ui::risk_color(orchard_core::Risk::Low),
            ))
        })
}

fn no_build_note(app: &StoreApp) -> String {
    let host = orchard_core::host_ref_arch();
    if app
        .platforms
        .iter()
        .any(|platform| platform == &format!("linux-{host}"))
    {
        "no installable build for this machine yet".to_string()
    } else {
        let published = app
            .platforms
            .iter()
            .filter_map(|platform| platform.strip_prefix("linux-"))
            .collect::<Vec<_>>()
            .join(", ");
        if published.is_empty() {
            format!("no {host} build yet")
        } else {
            format!("no {host} build yet, published for {published}")
        }
    }
}

fn load_flathub() -> Vec<StoreApp> {
    for path in
        orchard_core::appstream::appstream_paths(FLATHUB_REMOTE, &orchard_core::host_ref_arch())
    {
        if !path.exists() {
            continue;
        }
        let icons_root = path
            .parent()
            .map(|active| active.join("icons"))
            .unwrap_or_default();
        if let Ok(apps) = orchard_core::appstream::load(&path, &icons_root) {
            return apps
                .iter()
                .map(StoreApp::from_appstream)
                .collect();
        }
    }
    Vec::new()
}

/// Orchard channel: catalog over HTTP from indexd, file fallback.
fn load_orchard_catalog() -> Vec<StoreApp> {
    let agent = ureq::AgentBuilder::new()
        .timeout(std::time::Duration::from_secs(5))
        .build();
    let catalog: Option<CatalogFile> = agent
        .get(&format!("http://127.0.0.1:{INDEXD_PORT}/v1/catalog"))
        .call()
        .ok()
        .and_then(|response| response.into_json::<CatalogFile>().ok())
        .or_else(|| {
            for path in crate::catalog_file_paths() {
                if let Ok(bytes) = std::fs::read(&path)
                    && let Ok(catalog) = serde_json::from_slice::<CatalogFile>(&bytes)
                {
                    return Some(catalog);
                }
            }
            None
        });
    catalog
        .map(|catalog| catalog.apps.iter().map(StoreApp::from_catalog).collect())
        .unwrap_or_default()
}

/// The raw kind 0 metadata per pubkey from indexd, for merging a new
/// display name against the author's existing profile.
fn fetch_profile_raws() -> anyhow::Result<std::collections::BTreeMap<String, String>> {
    let agent = ureq::AgentBuilder::new()
        .timeout(std::time::Duration::from_secs(5))
        .build();
    let file: orchard_core::ProfilesFile = agent
        .get(&format!("http://127.0.0.1:{INDEXD_PORT}/v1/profiles"))
        .call()?
        .into_json()?;
    Ok(file.profiles)
}

/// Merge a new display name into the author's existing kind 0
/// metadata. Kind 0 is replaceable, so the merge must round-trip every
/// field the author ever published; a non-object payload (empty or
/// corrupt) starts a fresh object.
fn merge_profile_raw(existing: Option<&str>, name: &str) -> String {
    let mut value: serde_json::Value = existing
        .and_then(|raw| serde_json::from_str::<serde_json::Value>(raw).ok())
        .filter(|value| value.is_object())
        .unwrap_or_else(|| serde_json::json!({}));
    value["name"] = serde_json::Value::String(name.to_string());
    value.to_string()
}

/// Reviews from indexd, grouped by app id (newest first within an
/// app), plus the per-reviewer first-seen table.
type ReviewsIndex = (
    HashMap<String, Vec<orchard_core::Review>>,
    std::collections::BTreeMap<String, u64>,
);

fn load_reviews() -> anyhow::Result<ReviewsIndex> {
    let agent = ureq::AgentBuilder::new()
        .timeout(std::time::Duration::from_secs(5))
        .build();
    let file: orchard_core::ReviewsFile = agent
        .get(&format!("http://127.0.0.1:{INDEXD_PORT}/v1/reviews"))
        .call()?
        .into_json()?;
    let mut grouped: HashMap<String, Vec<orchard_core::Review>> = HashMap::new();
    for review in file.reviews {
        grouped
            .entry(review.app_id.clone())
            .or_default()
            .push(review);
    }
    Ok((grouped, file.first_seen))
}

/// Fetch the reviewer contact-list graph from indexd. Blocking: runs
/// on the background pool.
fn load_graph() -> anyhow::Result<std::collections::BTreeMap<String, Vec<String>>> {
    let agent = ureq::AgentBuilder::new()
        .timeout(std::time::Duration::from_secs(5))
        .build();
    let file: orchard_core::GraphFile = agent
        .get(&format!("http://127.0.0.1:{INDEXD_PORT}/v1/graph"))
        .call()?
        .into_json()?;
    Ok(file.graph)
}

/// What the UI needs from a kind 0 profile, parsed once on arrival.
#[derive(Default, Clone)]
struct ProfileBits {
    name: Option<String>,
    picture: Option<String>,
    /// The NIP-05 address the profile claims, `name@domain` or a bare
    /// domain. A claim is marketing until the domain backs it.
    nip05: Option<String>,
}

impl ProfileBits {
    /// Pull the display fields out of a raw kind 0 payload. Extra
    /// fields (and hostile JSON) are not our problem: wrong shapes
    /// parse as absent.
    fn parse(raw: &str) -> ProfileBits {
        let value: serde_json::Value = serde_json::from_str(raw).unwrap_or_default();
        ProfileBits {
            name: value["name"].as_str().map(str::to_string),
            picture: value["picture"].as_str().map(str::to_string),
            nip05: value["nip05"].as_str().map(str::to_string),
        }
    }
}

/// Fetch and parse every profile indexd holds. Blocking: runs on the
/// background pool.
fn load_profiles() -> anyhow::Result<std::collections::BTreeMap<String, ProfileBits>> {
    Ok(fetch_profile_raws()?
        .into_iter()
        .map(|(pubkey, raw)| (pubkey, ProfileBits::parse(&raw)))
        .collect())
}

/// The signed-in identity's own kind 0 from the social relays: the
/// first relay that answers. None when no relay has one. Blocking:
/// runs on the background pool.
fn load_own_profile(pubkey: &str) -> Option<ProfileBits> {
    let timeout = std::time::Duration::from_secs(5);
    for relay in social_relays() {
        match orchard_nostr::relay::fetch_profile_raw(&relay, pubkey, timeout) {
            Ok(Some(raw)) => return Some(ProfileBits::parse(&raw)),
            Ok(None) => {}
            Err(err) => eprintln!("own profile: {relay}: {err:#}"),
        }
    }
    None
}

/// A NIP-05 verdict: the claim that was checked and whether the
/// domain's well-known record named this key.
#[derive(Clone)]
struct Nip05Verdict {
    claim: String,
    verified: bool,
}

/// Split a NIP-05 claim into (local part, domain). A bare domain is
/// the `_` local part. Anything the URL would lie about (empty
/// pieces, a second `@`, a path) is not a claim.
fn split_nip05(claim: &str) -> Option<(String, String)> {
    let claim = claim.trim().to_ascii_lowercase();
    if claim.is_empty() {
        return None;
    }
    let (local, domain) = match claim.split_once('@') {
        Some((local, domain)) => (local, domain),
        None => ("_", claim.as_str()),
    };
    let ok = |piece: &str| {
        !piece.is_empty()
            && !piece.contains(['@', '/', '?', '#', ' '])
    };
    (ok(local) && ok(domain)).then(|| (local.to_string(), domain.to_string()))
}

/// The domain's NIP-05 record: the names it publishes, keyed by local
/// part. None when the fetch or the JSON fails; hostile payloads are
/// unverified, never a guess. `base` is the scheme plus host, so the
/// tests can point at a local server.
fn fetch_nip05_names(base: &str, local: &str) -> Option<std::collections::BTreeMap<String, String>> {
    let agent = ureq::AgentBuilder::new()
        .timeout(std::time::Duration::from_secs(5))
        .build();
    let record: serde_json::Value = agent
        .get(&format!("{base}/.well-known/nostr.json?name={local}"))
        .call()
        .ok()?
        .into_json()
        .ok()?;
    let names = record.get("names")?.as_object()?;
    Some(
        names
            .into_iter()
            .filter_map(|(local, key)| {
                let key = key.as_str()?.to_string();
                Some((local.to_string(), key))
            })
            .collect(),
    )
}

/// The claim checks out when the base url's well-known record names
/// this exact key. Case-insensitive, per the NIP. Blocking: runs on
/// the background pool.
fn claim_matches_record(base: &str, local: &str, pubkey: &str) -> bool {
    fetch_nip05_names(base, local)
        .and_then(|names| names.get(local).cloned())
        .is_some_and(|key| key.eq_ignore_ascii_case(pubkey))
}

/// NIP-05 verification: the domain behind the claim must name this
/// key. Any failure is unverified: the check is a fact earned, never
/// a guess. Blocking: runs on the background pool.
fn verify_nip05(claim: &str, pubkey: &str) -> bool {
    let Some((local, domain)) = split_nip05(claim) else {
        return false;
    };
    claim_matches_record(&format!("https://{domain}"), &local, pubkey)
}

/// The forge raw URL for a repository's proof file, derived from the
/// repository tag itself: claiming the repo means hosting the proof
/// at the forge's URL, so the tag stops being a free claim. GitHub
/// today (`HEAD` resolves the default branch); other forges are
/// unverified, never a guess.
fn proof_url(repository: &str) -> Option<String> {
    let rest = repository
        .strip_prefix("https://")
        .or_else(|| repository.strip_prefix("http://"))?
        .trim_start_matches("www.");
    let rest = rest.strip_prefix("github.com/")?;
    let rest = rest.trim_end_matches('/');
    let rest = rest.strip_suffix(".git").unwrap_or(rest);
    let mut parts = rest.split('/');
    let owner = parts.next()?.trim();
    let repo = parts.next()?.trim().trim_end_matches(".git");
    if owner.is_empty() || repo.is_empty() {
        return None;
    }
    Some(format!(
        "https://raw.githubusercontent.com/{owner}/{repo}/HEAD/orchard.txt"
    ))
}

/// Fetch the proof file and parse it through the shared format,
/// converting every token to the hex form events carry (npub lines
/// decode through the nostr codec; a broken bech32 string fails its
/// own checksum and drops out). None on any failure. Blocking: runs
/// on the background pool.
fn fetch_proof_keys(url: &str) -> Option<Vec<String>> {
    use nostr::nips::nip19::FromBech32 as _;
    let agent = ureq::AgentBuilder::new()
        .timeout(std::time::Duration::from_secs(5))
        .build();
    let body = agent.get(url).call().ok()?.into_string().ok()?;
    let keys: Vec<String> = orchard_core::proof::parse_proof_keys(&body)?
        .into_iter()
        .filter_map(|token| match token {
            orchard_core::proof::ProofKey::Hex(hex) => Some(hex),
            orchard_core::proof::ProofKey::Npub(npub) => nostr::prelude::PublicKey::from_bech32(
                &npub,
            )
            .ok()
            .map(|key| key.to_string()),
        })
        .collect();
    (!keys.is_empty()).then_some(keys)
}

/// Sign and send one review event through the signer, then publish it
/// to the given relays. Blocking: runs on the background pool.
fn post_review(
    reviewer: &Reviewer,
    app_id: &str,
    rating: f32,
    content: String,
    relays: &[String],
) -> anyhow::Result<()> {
    let mut tags: Vec<Vec<String>> = vec![vec!["d".into(), app_id.to_string()]];
    tags.push(vec!["rating".into(), format!("{rating:.1}")]);
    send_signed(
        reviewer,
        orchard_nostr::KIND_REVIEW as u16,
        tags,
        &content,
        relays,
    )?;
    Ok(())
}

/// Relays to post reviews to: resolved the way the embedded indexd
/// resolves its set (ORCHARD_RELAYS, else the settings list with the
/// required relay pinned in, else the rendezvous set), so a review
/// lands where the catalog is actually reading.
fn review_relays(settings: &orchard_core::SettingsHandle) -> Vec<String> {
    orchard_indexd::resolve_set(
        &settings.get().event_relays,
        std::env::var("ORCHARD_RELAYS").ok().as_deref(),
        &orchard_nostr::relay::relay_urls(None),
    )
}

/// Social relays to read the signed-in identity's own contact list
/// from (ORCHARD_SOCIAL_RELAYS or a default public set). Read-only,
/// anonymous: the fetch carries only the key, nothing else.
fn social_relays() -> Vec<String> {
    orchard_nostr::relay::social_relay_urls(
        std::env::var("ORCHARD_SOCIAL_RELAYS").ok().as_deref(),
    )
}

/// Read the identity's contact list from every social relay; the
/// union of the `p` tags each serves. Blocking: runs on the
/// background pool. Relays that fail shrink the union silently; the
/// caller keeps the old set when the union comes back empty.
fn load_follows(pubkey: &str) -> std::collections::HashSet<String> {
    let timeout = std::time::Duration::from_secs(5);
    let mut follows = std::collections::HashSet::new();
    for relay in social_relays() {
        match orchard_nostr::relay::fetch_contacts(&relay, pubkey, timeout) {
            Ok(contacts) => follows.extend(contacts),
            Err(err) => eprintln!("follows: {relay}: {err:#}"),
        }
    }
    follows
}

/// A live pairing session: the ephemeral client key, the QR image and
/// URI on screen, and the relay both ends must reach. The
/// anti-spoofing secret rides in the poll loop that waits for the
/// connect response.
struct ConnectSession {
    keys: nostr::prelude::Keys,
    qr_path: std::path::PathBuf,
    uri: String,
    relay: String,
}

/// The signed-in reviewer: a NIP-46 session, not a key. `keys` is the
/// ephemeral client keypair the connect channel encrypts with; the
/// identity reviews carry is the REMOTE signer's pubkey, and every
/// signature is a `sign_event` request the user approves on their
/// device. The client never sees the identity's secret key.
#[derive(Clone)]
pub struct Reviewer {
    keys: nostr::prelude::Keys,
    pub signer_pubkey: String,
    pub secret: String,
    pub relay: String,
    pub name: Option<String>,
}

impl Reviewer {
    /// What to call this reviewer in the UI: the published profile
    /// name, else a truncated npub (the format Nostr users expect,
    /// not raw hex).
    pub fn display(&self) -> String {
        self.name
            .clone()
            .unwrap_or_else(|| short_npub(&self.signer_pubkey))
    }
}

/// A round avatar from a cached image path; a person glyph when
/// there is none (or the fetch has not landed yet).
fn avatar(path: Option<&std::path::Path>, size: f32, fallback: gpui::Rgba) -> gpui::AnyElement {
    match path {
        // the radius must sit on the img itself: gpui does not clip
        // children to the parent's rounded corners, so a rounded
        // wrapper alone leaves a square picture
        Some(path) => div()
            .flex_none()
            .w(px(size))
            .h(px(size))
            .child(
                gpui::img(path)
                    .size_full()
                    .object_fit(gpui::ObjectFit::Cover)
                    .rounded_full(),
            )
            .into_any_element(),
        None => ui::icon("icons/person.svg", size * 0.85, fallback),
    }
}

/// The identity's fallback display: a truncated npub, head and tail
/// around an ellipsis, the format Nostr users expect, not raw hex.
/// The full npub is the identifier of record; a label is not the
/// place for 63 characters. Unparseable keys keep the hex fragment.
fn short_npub(pubkey: &str) -> String {
    match full_npub(pubkey) {
        Some(npub) if npub.len() > 19 => {
            format!("{}…{}", &npub[..14], &npub[npub.len() - 4..])
        }
        Some(npub) => npub,
        None => format!("{}…", &pubkey[..pubkey.len().min(12)]),
    }
}

/// The full bech32 npub for a hex key, when it parses. The identifier
/// of record: what a key-migration announcement names, and what a
/// click on the truncated form puts on the clipboard.
fn full_npub(pubkey: &str) -> Option<String> {
    use nostr::nips::nip19::ToBech32 as _;
    nostr::prelude::PublicKey::from_hex(pubkey)
        .ok()
        .and_then(|key| key.to_bech32().ok())
}

/// Load the saved NIP-46 session, if one is stored. The file holds the
/// ephemeral client key, the signer relationship, and the optional
/// locally chosen name; it survives sign-outs (the signer holds the
/// real key, so this file is not the identity).
fn load_reviewer() -> Option<Reviewer> {
    let path = identity_path()?;
    let raw = std::fs::read_to_string(path).ok()?;
    let saved: serde_json::Value = serde_json::from_str(&raw).ok()?;
    // files from the nsec-key experiment have no signer_pubkey: they
    // are ignored, the flow starts over with a pairing
    let keys = nostr::prelude::Keys::parse(saved["client_key"].as_str()?).ok()?;
    Some(Reviewer {
        signer_pubkey: saved["signer_pubkey"].as_str()?.to_string(),
        secret: saved["secret"].as_str()?.to_string(),
        relay: saved["relay"].as_str()?.to_string(),
        name: saved["name"].as_str().map(str::to_string),
        keys,
    })
}

/// Persist the NIP-46 session: the client's ephemeral key, the signer
/// relationship, and the optional published name.
fn save_reviewer(reviewer: &Reviewer) -> anyhow::Result<()> {
    let path = identity_path().ok_or_else(|| anyhow::anyhow!("XDG_DATA_HOME not set"))?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let saved = serde_json::json!({
        "client_key": reviewer.keys.secret_key().to_secret_hex(),
        "signer_pubkey": reviewer.signer_pubkey,
        "secret": reviewer.secret,
        "relay": reviewer.relay,
        "name": reviewer.name,
    });
    std::fs::write(&path, serde_json::to_vec_pretty(&saved)?)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    }
    Ok(())
}

/// Sign one event through the signer (NIP-46 `sign_event`) and publish
/// it to the given relays. Blocking: runs on the background pool.
fn send_signed(
    reviewer: &Reviewer,
    kind: u16,
    tags: Vec<Vec<String>>,
    content: &str,
    relays: &[String],
) -> anyhow::Result<()> {
    let unsigned = serde_json::json!({
        "pubkey": reviewer.signer_pubkey,
        "created_at": std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_secs(),
        "kind": kind,
        "tags": tags,
        "content": content,
    })
    .to_string();
    let event = orchard_nostr::connect::sign_event(
        &reviewer.relay,
        &reviewer.keys,
        &reviewer.signer_pubkey.parse()?,
        &unsigned,
    )?;
    for url in relays {
        orchard_nostr::relay::send(&url, std::slice::from_ref(&event))?;
    }
    Ok(())
}

fn identity_path() -> Option<std::path::PathBuf> {
    if let Some(data) = std::env::var_os("XDG_DATA_HOME") {
        return Some(std::path::PathBuf::from(data).join("orchard/identity.json"));
    }
    std::env::var_os("HOME").map(|home| {
        std::path::PathBuf::from(home).join(".local/share/orchard/identity.json")
    })
}

/// Where the pairing QR lands: the XDG cache when set, `$HOME/.cache`
/// otherwise, /tmp last. Unique names per call dodge the image cache.
fn qr_cache_dir() -> std::path::PathBuf {
    if let Some(cache) = std::env::var_os("XDG_CACHE_HOME") {
        return std::path::PathBuf::from(cache).join("orchard");
    }
    if let Some(home) = std::env::var_os("HOME") {
        return std::path::PathBuf::from(home).join(".cache/orchard");
    }
    std::env::temp_dir().join("orchard")
}

/// The digest verdict cache beside the other caches.
fn digest_cache_path() -> std::path::PathBuf {
    qr_cache_dir().join("digest-verdicts.json")
}

/// The verdict key: artifact location plus the pinned digest. The same
/// artifact shared by two app ids shares the verdict.
fn digest_cache_key(location: &str, pinned: &str) -> String {
    format!("{location}|{pinned}")
}

/// Load the digest verdict cache; a missing or unparseable file is an
/// empty cache, never a gate (the page-open check refills it).
fn load_digest_cache_at(
    path: &std::path::Path,
) -> std::collections::BTreeMap<String, String> {
    std::fs::read(path)
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

/// Persist the digest verdict cache: tmp plus rename, so a crash
/// mid-write cannot half-write it.
fn save_digest_cache_at(path: &std::path::Path, cache: &std::collections::BTreeMap<String, String>) {
    let Ok(json) = serde_json::to_vec(cache) else {
        return;
    };
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let tmp = path.with_extension("json.tmp");
    if std::fs::write(&tmp, json).is_ok() {
        let _ = std::fs::rename(&tmp, path);
    }
}

/// The relay the connect channel rides: a public NIP-46 relay, so a
/// signer app on a phone or another desktop can reach it too.
/// Override with ORCHARD_CONNECT_RELAY.
fn connect_relay() -> String {
    match std::env::var("ORCHARD_CONNECT_RELAY") {
        Ok(value) if !value.trim().is_empty() => value.trim().to_string(),
        _ => "wss://relay.nip46.com".to_string(),
    }
}

/// Parse `flatpak list --app --columns=application,version,origin,installation`.
fn list_installed() -> HashMap<String, InstalledInfo> {
    let Ok(output) = orchard_flatpak::list_installed().output() else {
        return HashMap::new();
    };
    let text = String::from_utf8_lossy(&output.stdout);
    text.lines()
        .filter_map(|line| {
            let mut fields = line.split('\t');
            let app_id = fields.next()?.trim().to_string();
            if app_id.is_empty() {
                return None;
            }
            let version = fields
                .next()
                .map(|f| f.trim().to_string())
                .filter(|version| !version.is_empty() && version != "—");
            let _origin = fields.next().map(|f| f.trim().to_string());
            // the installation column: "user" or "system"
            let user_scope = fields.next().is_some_and(|f| f.trim() == "user");
            Some((
                app_id.clone(),
                InstalledInfo {
                    version,
                    size: installed_size(&app_id, user_scope),
                    metadata: orchard_flatpak::installed_metadata(&app_id),
                    user_scope,
                },
            ))
        })
        .collect()
}

/// The "Installed Size" line of `flatpak info`, e.g. `1.0 GB`. None when
/// flatpak or the app is missing. Reads the scope the app deployed to.
fn installed_size(app_id: &str, user_scope: bool) -> Option<String> {
    let scope = if user_scope {
        orchard_flatpak::Scope::User
    } else {
        orchard_flatpak::Scope::System
    };
    let output = orchard_flatpak::info(scope, app_id).output().ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .find_map(|line| line.trim().strip_prefix("Installed Size:"))
        .map(str::trim)
        .filter(|size| !size.is_empty())
        .map(String::from)
}

/// True when the remote's version is strictly newer than the
/// installed one. Dot components compare numerically; anything
/// non-numeric is inconclusive and reads as up to date: flatpak's
/// deploy version `1.2.95.453.g0eeebbed` must not read as an update
/// against the appstream's `1.2.95.453`.
fn version_outdated(installed: &str, available: &str) -> bool {
    if installed == available {
        return false;
    }
    let mut installed = installed.split('.');
    let mut available = available.split('.');
    loop {
        match (installed.next(), available.next()) {
            (Some(a), Some(b)) => match (a.parse::<u64>(), b.parse::<u64>()) {
                (Ok(a), Ok(b)) if a != b => return b > a,
                // a non-numeric component on either side: different
                // versioning scheme, cannot judge, stay quiet
                (Err(_), _) | (Ok(_), Err(_)) => return false,
                _ => {}
            },
            // trailing numeric components mean the remote moved ahead
            (None, Some(b)) => return b.parse::<u64>().is_ok_and(|n| n > 0),
            (Some(_), None) | (None, None) => return false,
        }
    }
}

/// Update-available detection for one app: both versions must be
/// known and the remote's strictly newer by the numeric rule.
fn update_available(app: &StoreApp, installed: &InstalledInfo) -> bool {
    match (&app.version, &installed.version) {
        (Some(available), Some(installed)) => version_outdated(installed, available),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use orchard_core::{CatalogApp, Publisher};

    #[test]
    fn digest_cache_round_trips() {
        let path =
            std::env::temp_dir().join(format!("orchard-digests-{}.json", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let mut cache = load_digest_cache_at(&path);
        assert!(cache.is_empty());
        cache.insert(
            digest_cache_key("ghcr.io/a/b", "sha256:00"),
            "sha256:00".into(),
        );
        save_digest_cache_at(&path, &cache);
        let reloaded = load_digest_cache_at(&path);
        assert_eq!(
            reloaded
                .get("ghcr.io/a/b|sha256:00")
                .map(String::as_str),
            Some("sha256:00")
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn list_sort_orders_by_each_chosen_key() {
        use std::cmp::Ordering;
        let older = dated_app("Beta", Some("2026-01-01"));
        let newer = dated_app("Alpha", Some("2026-02-01"));
        // name: alphabetical wins, recency ignored
        assert_eq!(
            list_sort(&older, &newer, ListSort::Name, "2026-10-11"),
            Ordering::Greater
        );
        // updated: recency wins, name ignored; the newer app ranks first
        assert_eq!(
            list_sort(&older, &newer, ListSort::Updated, "2026-10-11"),
            Ordering::Greater
        );
    }

    #[test]
    fn risk_echo_speaks_only_when_it_matters() {
        // windowing alone is low risk: the confirm stays silent
        let quiet = "[Context]\nsockets=wayland\n";
        assert_eq!(risk_echo(Some(quiet)), None);
        // a home-directory grant is high risk: the line names it and
        // wears the high color
        let meta = "[Context]\nsockets=wayland\nfilesystems=home\n";
        let (color, text) = risk_echo(Some(meta)).expect("high grant speaks");
        assert_eq!(color, ui::risk_color(orchard_core::Risk::High));
        assert!(text.starts_with("This app asks for: "));
        assert!(text.contains("Home directory (read/write) (high)"));
    }


    /// A minimal publisher-channel row for order tests.
    fn dated_app(name: &str, date: Option<&str>) -> StoreApp {
        let mut app = StoreApp::from_catalog(&catalog_fixture(name, 0));
        app.release_date = date.map(Into::into);
        app
    }

    fn catalog_fixture(name: &str, published_at: i64) -> CatalogApp {
        CatalogApp {
            app_id: format!("com.example.{name}").into(),
            name: name.into(),
            summary: "A demo app".into(),
            description: None,
            icon_url: None,
            categories: Vec::new(),
            publisher: Publisher {
                pubkey: "k".into(),
                nip05: None,
            },
            version: "1.0.0".into(),
            platforms: vec!["linux-x86_64".into()],
            flatpak_ref: "app/com.example.Hello/x86_64/stable".into(),
            location: "oci:/srv/hello".into(),
            digest: Some("sha256:aa".into()),
            metadata: None,
            repository: None,
            homepage: None,
            notes: None,
            published_at,
            successor_to: None,
        }
    }

    #[test]
    fn publisher_rows_date_from_their_signed_release_time() {
        // the newest release's signed event time ranks a publisher row
        // beside the Flathub rows; the Hello fixture's stamp is today
        let app = StoreApp::from_catalog(&catalog_fixture("App", 1_791_711_263));
        assert_eq!(app.release_date.as_deref(), Some("2026-10-11"));
        // zero (no release) stays undated
        let app = StoreApp::from_catalog(&catalog_fixture("App", 0));
        assert_eq!(app.release_date, None);
    }

    #[test]
    fn date_order_ranks_only_arrived_dates() {
        let today = "2026-10-11";
        let past = dated_app("App", Some("2026-08-01"));
        let future = dated_app("App", Some("2026-11-11"));
        let undated = dated_app("App", None);
        // an arrived date outranks a future one, even though the
        // future date is numerically larger
        assert_eq!(
            date_order(&past, &future, today),
            std::cmp::Ordering::Less
        );
        // a future-only app ranks as undated: same name, so a tie
        assert_eq!(
            date_order(&future, &undated, today),
            std::cmp::Ordering::Equal
        );
        // the day the date arrives, the app ranks again
        let arrived = dated_app("App", Some("2026-10-11"));
        assert_eq!(
            date_order(&arrived, &future, today),
            std::cmp::Ordering::Less
        );
        // undated sinks below dated, name breaks ties among undated
        assert_eq!(
            date_order(&undated, &past, today),
            std::cmp::Ordering::Greater
        );
        assert_eq!(
            date_order(&dated_app("Aab", None), &undated, today),
            std::cmp::Ordering::Less
        );
    }

    #[test]
    fn version_outdated_numeric_compare() {        assert!(version_outdated("0.5.21", "0.5.22"));
        assert!(version_outdated("1.2.9", "1.2.10"));
        assert!(!version_outdated("0.5.22", "0.5.22"));
        assert!(!version_outdated("0.5.23", "0.5.22"));
        // flatpak deploy suffixes must not read as updates
        assert!(!version_outdated(
            "1.2.95.453.g0eeebbed",
            "1.2.95.453"
        ));
        assert!(!version_outdated("1.23.2b", "1.23.2b-x"));
        assert!(!version_outdated("1.0", "1.0.0"));
        // trailing numeric components mean the remote moved ahead
        assert!(version_outdated("1.2", "1.2.1"));
        assert!(!version_outdated("1.2.1", "1.2"));
    }

    #[test]
    fn profile_merge_keeps_published_fields() {
        // kind 0 is replaceable: the merge must round-trip everything
        // the author already published, only the name is ours
        let merged = merge_profile_raw(
            Some(r#"{"name":"Old","nip05":"me@example.com","picture":"https://x/y.png"}"#),
            "New",
        );
        let value: serde_json::Value = serde_json::from_str(&merged).unwrap();
        assert_eq!(value["name"], "New");
        assert_eq!(value["nip05"], "me@example.com");
        assert_eq!(value["picture"], "https://x/y.png");

        // no existing profile (fresh key): a fresh object
        let merged = merge_profile_raw(None, "Fresh");
        let value: serde_json::Value = serde_json::from_str(&merged).unwrap();
        assert_eq!(value["name"], "Fresh");
        assert!(value.get("nip05").is_none());

        // corrupt payload: start over rather than carry junk
        let merged = merge_profile_raw(Some("not json"), "New");
        let value: serde_json::Value = serde_json::from_str(&merged).unwrap();
        assert_eq!(value["name"], "New");
    }

    #[test]
    fn orchard_fingerprint_changes_with_updates() {
        let mut app = StoreApp::from_catalog(&CatalogApp {
            app_id: "com.example.Hello".into(),
            name: "Hello".into(),
            summary: "A demo app".into(),
            description: None,
            icon_url: None,
            categories: Vec::new(),
            publisher: Publisher {
                pubkey: "k".into(),
                nip05: None,
            },
            version: "1.0.0".into(),
            platforms: vec!["linux-x86_64".into()],
            flatpak_ref: "app/com.example.Hello/x86_64/stable".into(),
            location: "oci:/srv/hello".into(),
            digest: Some("sha256:aa".into()),
            metadata: None,
            repository: None,
            homepage: None,
            notes: None,
            published_at: 0,
            successor_to: None,
        });
        let same = orchard_fingerprint(std::slice::from_ref(&app));
        assert_eq!(same, orchard_fingerprint(std::slice::from_ref(&app)));
        // a new release changes the fingerprint (row + page must refresh)
        let mut updated = app.clone();
        updated.version = Some("1.1.0".into());
        assert_ne!(same, orchard_fingerprint(std::slice::from_ref(&updated)));
        // so does a listing edit
        app.summary = "Edited summary".into();
        assert_ne!(same, orchard_fingerprint(std::slice::from_ref(&app)));
    }

    #[test]
    fn composition_line_states_quiet_facts() {
        assert_eq!(composition_line(0, 0, 0, 0), "0 apps");
        assert_eq!(composition_line(1, 1, 0, 0), "1 app from 1 publisher");
        assert_eq!(
            composition_line(12, 3, 2, 1),
            "12 apps from 3 publishers · 2 verified · 1 in your circle"
        );
        // a Flathub-only view has no publisher keys to count
        assert_eq!(composition_line(2, 0, 0, 0), "2 apps");
    }

    #[test]
    fn publisher_rows_gather_one_keys_listings_in_recency_order() {
        let mut first = dated_app("Alpha", Some("2026-01-01"));
        first.publisher = Some("k".into());
        let mut second = dated_app("Beta", Some("2026-03-01"));
        second.publisher = Some("k".into());
        let mut other = dated_app("Gamma", Some("2026-02-01"));
        other.publisher = Some("other".into());
        let apps = vec![first, second, other];

        let rows = publisher_rows(&apps, "k", "2026-10-11");
        let names: Vec<&str> = rows.iter().map(|&i| apps[i].name.as_str()).collect();
        // only this key's apps, the newer release first
        assert_eq!(names, vec!["Beta", "Alpha"]);
    }

    #[test]
    fn developer_rows_group_the_appstream_claim() {
        let mut first = dated_app("Alpha", Some("2026-01-01"));
        first.developer = Some("Team".into());
        let mut second = dated_app("Beta", Some("2026-03-01"));
        second.developer = Some("Team".into());
        let mut other = dated_app("Gamma", Some("2026-02-01"));
        other.developer = Some("Other Team".into());
        let apps = vec![first, second, other];

        let rows = developer_rows(&apps, "Team", "2026-10-11");
        let names: Vec<&str> = rows.iter().map(|&i| apps[i].name.as_str()).collect();
        // only apps claiming this exact name, the newer release first
        assert_eq!(names, vec!["Beta", "Alpha"]);
    }

    #[test]
    fn ci_contains_matches_case_insensitively() {
        assert!(ci_contains("Orchard VPN", "vpn"));
        assert!(ci_contains("Orchard VPN", "ORCHARD"));
        assert!(!ci_contains("Orchard VPN", "proton"));
        assert!(ci_contains("anything", ""));
        // an ASCII needle matches past multi-byte characters
        assert!(ci_contains("Änderungen", "nder"));
        // a non-ASCII needle takes the Unicode fold
        assert!(ci_contains("Änderungen", "änderungen"));
        assert!(!ci_contains("Änderungen", "änderunx"));
    }

    #[test]
    fn ci_cmp_orders_names_case_insensitively() {
        assert_eq!(ci_cmp("alpha", "Beta"), std::cmp::Ordering::Less);
        assert_eq!(ci_cmp("beta", "ALPHA"), std::cmp::Ordering::Greater);
        assert_eq!(ci_cmp("same", "same"), std::cmp::Ordering::Equal);
        // a non-ASCII name takes the Unicode fold, which is byte
        // order, not locale collation: ä sorts past z
        assert_eq!(ci_cmp("Änder", "Zulu"), std::cmp::Ordering::Greater);
    }

    #[test]
    fn circle_lens_shows_only_your_people() {
        let mut followed = trust_review("1111", Some(4.0));
        followed.created_at = 100;
        let mut two_hops = trust_review("2222", Some(5.0));
        two_hops.created_at = 150;
        let mut crowd = trust_review("3333", Some(2.0));
        crowd.created_at = 300;
        let mut mine = trust_review("4444", Some(3.0));
        mine.created_at = 50;
        let reviews = vec![crowd, two_hops, followed, mine];
        let mut follows = std::collections::HashSet::new();
        follows.insert("1111".to_string());

        // the circle lens: your own review and your follows', newest
        // first; the two-hop key and the crowd stay out
        let order = review_order(&reviews, false, Some("4444"), &follows);
        let keys: Vec<&str> = order.iter().map(|&i| reviews[i].pubkey.as_str()).collect();
        assert_eq!(keys, vec!["1111", "4444"]);

        // everyone: one recency list, relationships ignored
        let order = review_order(&reviews, true, Some("4444"), &follows);
        let keys: Vec<&str> = order.iter().map(|&i| reviews[i].pubkey.as_str()).collect();
        assert_eq!(keys, vec!["3333", "2222", "1111", "4444"]);
    }

    fn trust_review(pubkey: &str, rating: Option<f32>) -> orchard_core::Review {
        orchard_core::Review {
            app_id: "com.orchard.Hello".into(),
            pubkey: pubkey.into(),
            name: None,
            nip05: None,
            rating,
            content: String::new(),
            created_at: 0,
        }
    }

    #[test]
    fn publisher_reviews_are_flagged_and_excluded() {
        let reviews = vec![
            trust_review("pub-a", Some(5.0)),
            trust_review("pub-b", Some(1.0)),
            trust_review("pub-a", None),
        ];
        let aggregate = reviews_aggregate(
            &reviews,
            Some("pub-a"),
            &Default::default(),
            0,
            &Default::default(),
            &Default::default(),
        );
        assert_eq!(aggregate.average, Some(1.0));
        assert_eq!(aggregate.counted, 1);
        assert_eq!(aggregate.new_keys, 0);
        assert_eq!(aggregate.publisher_reviews, 2);
    }

    #[test]
    fn new_keys_are_counted_but_never_excluded() {
        let reviews = vec![
            trust_review("fresh", Some(5.0)),
            trust_review("old", Some(1.0)),
        ];
        let mut first_seen = std::collections::BTreeMap::new();
        first_seen.insert("fresh".to_string(), 0);
        // the fresh key is first seen a minute before now; the other
        // key is absent from the table, which means unknown, not new
        let aggregate =
            reviews_aggregate(&reviews, None, &first_seen, 60, &Default::default(), &Default::default());
        assert_eq!(aggregate.average, Some(3.0));
        assert_eq!(aggregate.counted, 2);
        assert_eq!(aggregate.new_keys, 1);
        assert_eq!(aggregate.publisher_reviews, 0);
    }

    #[test]
    fn short_npub_truncates_and_falls_back() {
        // the demo publisher key, bech32-encoded, head…tail
        let npub = short_npub("ec2c39e7a8a4c38b9c8b9e3c8f3ab4b09bd0e3b98b6c1b3b0f2e3d4c5b6a7988");
        assert!(npub.starts_with("npub1"));
        // the ellipsis sits in the middle, not at the end
        let mid = npub.find('…').expect("ellipsis");
        assert_eq!(mid, 14);
        // head 14 + ellipsis + tail 4, in characters (bytes would
        // count the ellipsis as three)
        assert_eq!(npub.chars().count(), 19);
        // garbage keys keep the hex fragment
        assert_eq!(short_npub("not-hex"), "not-hex…");
    }

    #[test]
    fn circle_reviews_are_counted_and_averaged() {        let reviews = vec![
            trust_review("friend", Some(4.0)),
            trust_review("stranger", Some(1.0)),
            trust_review("mute", None),
        ];
        let mut follows = std::collections::HashSet::new();
        follows.insert("friend".to_string());
        follows.insert("mute".to_string());
        let mut first_seen = std::collections::BTreeMap::new();
        first_seen.insert("friend".to_string(), 0);
        let aggregate =
            reviews_aggregate(&reviews, None, &first_seen, 60, &follows, &Default::default());
        assert_eq!(aggregate.circle, 2);
        assert_eq!(aggregate.circle_average, Some(4.0));
        // the global aggregate is unchanged by the circle lens
        assert_eq!(aggregate.average, Some(2.5));
        assert_eq!(aggregate.counted, 3);
        // the followed key also shows its sprout
        assert_eq!(aggregate.new_keys, 1);
    }

    #[test]
    fn continuity_flags_only_changes_with_history() {
        let mut file = orchard_core::ContinuityFile::default();
        // first sight: two publishers of one app id seed unflagged (a
        // fresh client has no history to disclose); the record saves
        let batch = vec![
            ("com.x.A".to_string(), "k1".to_string()),
            ("com.x.A".to_string(), "k2".to_string()),
        ];
        assert!(absorb_continuity(&mut file, &batch));
        assert_eq!(file.apps["com.x.A"].flagged.len(), 0);
        // the same keys again: nothing changed
        assert!(!absorb_continuity(&mut file, &batch));
        // a third key joins a known app id: a change, flagged once
        let third = vec![("com.x.A".to_string(), "k3".to_string())];
        assert!(absorb_continuity(&mut file, &third));
        assert_eq!(file.apps["com.x.A"].flagged, vec!["k3".to_string()]);
        // and it stays flagged: the fact is not un-seen
        assert!(!absorb_continuity(&mut file, &third));
        assert_eq!(file.apps["com.x.A"].flagged, vec!["k3".to_string()]);
        // a second app id, first sight: unflagged even though the key
        // was seen elsewhere
        let other = vec![("com.x.B".to_string(), "k1".to_string())];
        assert!(absorb_continuity(&mut file, &other));
        assert_eq!(file.apps["com.x.B"].flagged.len(), 0);
        assert_eq!(file.apps["com.x.A"].keys.len(), 3);
        assert_eq!(file.apps["com.x.B"].keys, vec!["k1".to_string()]);
    }

    #[test]
    fn profile_bits_parses_display_fields() {
        let bits = ProfileBits::parse(r#"{"name":"Marta","picture":"https://x/m.png","nip05":"marta@x.org"}"#);
        assert_eq!(bits.name.as_deref(), Some("Marta"));
        assert_eq!(bits.picture.as_deref(), Some("https://x/m.png"));
        assert_eq!(bits.nip05.as_deref(), Some("marta@x.org"));
        // wrong shapes parse as absent, never crash
        let junk = ProfileBits::parse("not json");
        assert!(junk.name.is_none());
        assert!(junk.picture.is_none());
        assert!(junk.nip05.is_none());
        let empty = ProfileBits::parse(r#"{"about":""}"#);
        assert!(empty.name.is_none());
        assert!(empty.picture.is_none());
        assert!(empty.nip05.is_none());
    }

    #[test]
    fn nip05_claims_split_or_refuse() {
        let claim = |claim| split_nip05(claim);
        assert_eq!(
            claim("me@example.com"),
            Some(("me".into(), "example.com".into()))
        );
        // uppercase and stray spaces are presentation, not identity
        assert_eq!(
            claim("  ME@Example.COM "),
            Some(("me".into(), "example.com".into()))
        );
        // a bare domain claims the `_` local part
        assert_eq!(
            claim("example.com"),
            Some(("_".into(), "example.com".into()))
        );
        // anything the url would lie about refuses
        assert_eq!(claim(""), None);
        assert_eq!(claim("@example.com"), None);
        assert_eq!(claim("me@"), None);
        assert_eq!(claim("a@b@c"), None);
        assert_eq!(claim("a@example.com/x"), None);
    }

    /// The record check against a live local server: the domain's
    /// names decide, and everything else is unverified.
    #[test]
    fn nip05_check_against_a_local_record() {
        let record = r#"{"names": {"alice": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa", "bob": "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"}}"#;
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{record}",
            record.len()
        );
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            use std::io::Write as _;
            stream.write_all(response.as_bytes()).unwrap();
        });
        let base = format!("http://127.0.0.1:{port}");
        let alice = "A".repeat(64);
        // the record names alice with the same key, case-insensitively
        assert!(claim_matches_record(&base, "alice", &alice));
        // bob's claim against alice's key is not verified
        assert!(!claim_matches_record(&base, "bob", &alice));
        // a claim for a name the record does not carry: unverified
        assert!(!claim_matches_record(&base, "carol", &alice));
        // a malformed claim never checks out
        assert!(!verify_nip05("@", &alice));
    }

    #[test]
    fn circle2_is_two_hops_from_the_graph() {
        // the identity ("me") follows friend and mute; friend follows
        // twohop, the identity themself, and someone already followed
        let mut follows = std::collections::HashSet::new();
        follows.insert("friend".to_string());
        follows.insert("mute".to_string());
        let mut graph = std::collections::BTreeMap::new();
        graph.insert(
            "friend".to_string(),
            vec![
                "twohop".to_string(),
                "me".to_string(),
                "mute".to_string(),
            ],
        );
        let two = circle2_set(&follows, &graph, Some("me"));
        assert_eq!(two, std::collections::HashSet::from_iter(["twohop".to_string()]));

        // the aggregate counts a two-hop review in circle2, not circle
        let reviews = vec![trust_review("twohop", Some(3.0)), trust_review("stranger", Some(2.0))];
        let aggregate =
            reviews_aggregate(&reviews, None, &Default::default(), 0, &follows, &two);
        assert_eq!(aggregate.circle, 0);
        assert_eq!(aggregate.circle2, 1);
        assert_eq!(aggregate.counted, 2);
        // signed out there are no follows, so no two hops at all
        assert!(circle2_set(&Default::default(), &graph, None).is_empty());
    }

    // ----- job progress -----

    #[test]
    fn parse_reads_flatpak_state_and_percent_lines() {
        // bare state line: no percent yet
        assert_eq!(
            parse_progress_line("Installing…"),
            Some(("Installing".to_string(), None))
        );
        // the probe's shape: label, ellipsis, block bar, percent
        assert_eq!(
            parse_progress_line("Installing… ▊▌                     8%"),
            Some(("Installing".to_string(), Some(8)))
        );
        assert_eq!(
            parse_progress_line("Downloading metadata… 45%"),
            Some(("Downloading metadata".to_string(), Some(45)))
        );
        // everything else on stdout parses to nothing
        assert_eq!(parse_progress_line("Installation complete."), None);
        assert_eq!(
            parse_progress_line("com.orchard.Hello permissions:"),
            None
        );
        assert_eq!(
            parse_progress_line(" 1.\tcom.orchard.Hello\tstable\ti\thello-origin\t0 bytes"),
            None
        );
        assert_eq!(parse_progress_line(""), None);
    }

    #[test]
    fn stage_wording_maps_known_labels_and_keeps_unknown() {
        assert_eq!(stage_text("Downloading", "ghcr.io"), "Pulling from ghcr.io");
        assert_eq!(stage_text("Downloading metadata", "flathub"), "Pulling from flathub");
        assert_eq!(stage_text("Downloading", ""), "Pulling");
        assert_eq!(stage_text("Installing", "ghcr.io"), "Deploying");
        assert_eq!(stage_text("Deploying", "flathub"), "Deploying");
        assert_eq!(stage_text("Importing", ""), "Importing");
    }

    #[test]
    fn elapsed_reads_like_a_clock() {
        assert_eq!(elapsed_text(Duration::from_secs(12)), "12s");
        assert_eq!(elapsed_text(Duration::from_secs(59)), "59s");
        assert_eq!(elapsed_text(Duration::from_secs(60)), "1m00s");
        assert_eq!(elapsed_text(Duration::from_secs(754)), "12m34s");
    }

    #[test]
    fn status_text_prefers_the_finer_truth() {
        let mut job = Job::new(JobState::Installing);
        // no child word yet: the phase and the clock
        assert!(job.status_text().starts_with("Installing · "));
        // the child's stage replaces it, percent when announced
        job.progress = Some(JobProgress {
            stage: "Pulling from ghcr.io".to_string(),
            percent: Some(34),
        });
        let text = job.status_text();
        assert!(text.starts_with("Pulling from ghcr.io · 34% · "));
        // no percent announced: the stage and the clock only
        job.progress = Some(JobProgress {
            stage: "Deploying".to_string(),
            percent: None,
        });
        assert!(job.status_text().starts_with("Deploying · "));
    }

    #[test]
    fn job_nouns_name_the_running_thing() {
        assert_eq!(Job::new(JobState::Installing).state.noun(), "install");
        assert_eq!(Job::new(JobState::Updating).state.noun(), "update");
        assert_eq!(Job::new(JobState::Removing).state.noun(), "removal");
        assert_eq!(Job::new(JobState::Verifying).state.noun(), "install");
    }
}
