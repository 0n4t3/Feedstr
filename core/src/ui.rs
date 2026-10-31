//! The desktop interface.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, mpsc};
use std::time::Duration;

use anyhow::Result;
use eframe::egui::{
    self, Align, Button, CentralPanel, Color32, ComboBox, CornerRadius, DragValue, FontId, Frame, Galley, Id, Key,
    Label, Layout, Margin, Modal, OpenUrl, Painter, Panel, Rect, Response, RichText, ScrollArea, Sense, Shape, Stroke,
    StrokeKind, TextEdit, TextStyle, Theme, ThemePreference, Ui, Vec2, ViewportCommand, pos2, text::LayoutJob, vec2,
};
use nostr_sdk::prelude::{Keys, ToBech32};
use tokio::sync::mpsc::UnboundedSender;

use crate::db::{self, Conn, FeedRow, ItemRow, ReportRow, Store, View, now_ms};
use crate::html::{self, Block, Style};
use crate::longform;
use crate::worker::{self, Cmd, FileKind, Login, Msg};

const ACCENT: Color32 = Color32::from_rgb(0x7c, 0x4d, 0xff);
const READER_WIDTH: f32 = 700.0;
/// Closing waits at most this long for changes to be saved to relays.
const EXIT_TIMEOUT: Duration = Duration::from_secs(30);
/// Interface sizes offered by the − and + buttons.
const ZOOM_STEPS: &[f32] = &[0.5, 0.67, 0.75, 0.8, 0.9, 1.0, 1.1, 1.25, 1.5, 1.75, 2.0];
const CARD_GAP: f32 = 12.0;
/// Articles listed at first; scrolling to the end shows this many more.
const ITEMS_PAGE: usize = 1000;
/// Narrower than this (a phone, say), one pane is shown at a time.
const NARROW_WIDTH: f32 = 640.0;

/// How the article list is shown.
#[derive(Clone, Copy, PartialEq)]
enum ListLayout {
    List,
    CompactList,
    Cards,
    CompactCards,
}

impl ListLayout {
    const ALL: [ListLayout; 4] = [Self::List, Self::CompactList, Self::Cards, Self::CompactCards];

    fn key(self) -> &'static str {
        match self {
            Self::List => "list",
            Self::CompactList => "compact-list",
            Self::Cards => "cards",
            Self::CompactCards => "compact-cards",
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::List => "List",
            Self::CompactList => "Compact list",
            Self::Cards => "Cards",
            Self::CompactCards => "Compact cards",
        }
    }

    fn from_key(key: &str) -> Self {
        Self::ALL.into_iter().find(|l| l.key() == key).unwrap_or(Self::List)
    }

    fn is_cards(self) -> bool {
        matches!(self, Self::Cards | Self::CompactCards)
    }

    /// Smallest width and the height of a card.
    fn card_size(self) -> Vec2 {
        if self == Self::CompactCards { vec2(220.0, 90.0) } else { vec2(270.0, 190.0) }
    }
}

#[derive(PartialEq)]
enum Screen {
    Login,
    Main,
}

/// `focus`: the dialog's text field still has to get the cursor (see [`focus_on_open`]).
enum Dialog {
    AddFeed {
        input: String,
        category: String,
        busy: bool,
        error: Option<String>,
        focus: bool,
    },
    RenameCategory {
        from: String,
        to: String,
    },
    NewCategory {
        url: String,
        name: String,
        focus: bool,
    },
    NewSyncKey {
        nsec: String,
    },
    ConfirmSignOut,
    /// Marking every unread article in a view (matching a search) as read.
    ConfirmMarkRead {
        view: View,
        search: String,
        count: usize,
    },
    ConfirmErase {
        understood: bool,
        busy: bool,
        error: Option<String>,
    },
}

#[derive(Clone, Copy, PartialEq)]
enum SettingsTab {
    Account,
    Sync,
    Filters,
    Report,
    Socials,
    Data,
    Appearance,
}

impl SettingsTab {
    const ALL: [SettingsTab; 7] =
        [Self::Account, Self::Sync, Self::Filters, Self::Report, Self::Socials, Self::Data, Self::Appearance];

    fn label(self) -> &'static str {
        match self {
            Self::Account => "Account",
            Self::Sync => "Sync",
            Self::Filters => "Filters",
            Self::Report => "Feed report",
            Self::Socials => "Socials",
            Self::Data => "Import & export",
            Self::Appearance => "Appearance",
        }
    }
}

/// Settings, shown full screen in place of the feeds.
struct SettingsPage {
    tab: SettingsTab,
    new_relay: String,
    show_key: bool,
    /// The word filter being added, and where it applies.
    new_filter: String,
    filter_scope: String,
    /// The feed report last made, and for how many days.
    report: Option<(u32, Vec<ReportRow>)>,
    /// On a phone: the chosen section is shown, not the list of sections.
    showing: bool,
}

/// Days without new posts after which the feed report lists a feed, unless changed.
const DEFAULT_REPORT_DAYS: u32 = 365;

#[derive(PartialEq)]
enum Exit {
    Running,
    /// Waiting for the worker to save changes, since this moment (ms).
    Saving(i64),
    Ready,
}

#[derive(Default)]
struct LoginForm {
    /// Something to tell the user, like that their data was deleted.
    notice: Option<String>,
    sync_key: String,
    bunker: String,
    busy: bool,
    error: Option<String>,
    /// On the web: don't keep anything in this browser after signing in
    /// ("Remember me" unticked).
    forget: bool,
    /// While signing in with a QR code: the `nostrconnect://` link it shows.
    connect_link: Option<String>,
}

pub struct App {
    db: Conn,
    tx: UnboundedSender<Cmd>,
    /// Stops a QR code sign-in (not offered on phones).
    #[cfg_attr(target_os = "android", allow(dead_code))]
    cancel_connect: Arc<tokio::sync::Notify>,
    platform: Arc<dyn worker::Platform>,
    /// A signer app is installed (Android), so signing in with it is offered.
    has_signer_app: bool,
    /// Tells the worker the app is closing.
    closing: tokio::sync::watch::Sender<bool>,
    rx: mpsc::Receiver<Msg>,
    screen: Screen,
    login: LoginForm,
    dialog: Option<Dialog>,
    settings: Option<SettingsPage>,
    status: String,
    /// Signing in again (at startup, or after changing relays) failed.
    sign_in_problem: Option<String>,
    npub: String,
    view: View,
    unread_only: bool,
    /// Only items whose title, author or text contain this.
    search: String,
    /// Categories folded up in the sidebar.
    collapsed: HashSet<String>,
    feeds: Vec<FeedRow>,
    favorites: i64,
    /// Every category in use.
    categories: Vec<String>,
    /// Categories this device doesn't sync.
    local_categories: Vec<String>,
    /// People in the follows feed, with unread counts, when it's shown as a group.
    follows_people: Vec<(String, i64)>,
    items: Vec<ItemRow>,
    /// How many articles are listed at most (grows as the list is scrolled to its end).
    limit: usize,
    selected: Option<ItemRow>,
    blocks: Vec<Block>,
    /// The article's title and text, for "Copy article".
    article_text: String,
    /// Heights of the article's blocks and of list rows, so off-screen ones
    /// aren't laid out on every frame (long articles and lists scrolled slowly).
    block_heights: RowHeights,
    row_heights: RefCell<RowHeights>,
    /// Set when the selection moved by keyboard, so the list follows it.
    scroll_to_selected: bool,
    layout: ListLayout,
    /// With cards or on a narrow screen, the selected article is shown instead of the list.
    reading: bool,
    /// The screen is too narrow for side-by-side panes.
    narrow: bool,
    /// On a narrow screen: the feeds are shown instead of the articles.
    show_feeds: bool,
    /// The interface scale last saved.
    zoom: f32,
    exit: Exit,
    /// An import or export is waiting for its file dialog or still running.
    file_busy: bool,
    /// What the last import or export did.
    file_result: Option<Result<String, String>>,
}

impl App {
    /// The app, keeping its data in `store`, with what differs between platforms in `platform`.
    pub fn new(cc: &eframe::CreationContext, store: Store, platform: Arc<dyn worker::Platform>) -> Result<Self> {
        let db = db::connect(&store)?;
        let worker::Handle { tx, rx, closing, cancel_connect } =
            worker::spawn(&store, cc.egui_ctx.clone(), platform.clone())?;
        setup_style(&cc.egui_ctx);
        let theme = match db.meta("theme")?.as_deref() {
            Some("light") => ThemePreference::Light,
            Some("dark") => ThemePreference::Dark,
            _ => ThemePreference::System,
        };
        cc.egui_ctx.set_theme(theme);
        cc.egui_ctx.add_plugin(SelectedText::default());
        // A notch of the mouse wheel moves the text further than egui does by default.
        cc.egui_ctx.options_mut(|o| o.input_options.line_scroll_speed *= 1.5);
        let zoom = db.meta("zoom")?.and_then(|z| z.parse().ok()).unwrap_or(1.0);
        cc.egui_ctx.set_zoom_factor(zoom);
        let layout = ListLayout::from_key(&db.meta("layout")?.unwrap_or_default());

        let signed_in = db.meta("login")?.is_some();
        if signed_in {
            // Show what we have right away; signing in again happens in the background.
            tx.send(Cmd::Login(Login::Saved))?;
        }
        let mut app = Self {
            db,
            tx,
            cancel_connect,
            has_signer_app: platform.signer_app().is_some(),
            platform,
            closing,
            rx,
            screen: if signed_in { Screen::Main } else { Screen::Login },
            login: LoginForm::default(),
            dialog: None,
            settings: None,
            status: "Connecting…".into(),
            sign_in_problem: None,
            npub: String::new(),
            view: View::All,
            unread_only: false,
            search: String::new(),
            collapsed: HashSet::new(),
            feeds: Vec::new(),
            favorites: 0,
            follows_people: Vec::new(),
            categories: Vec::new(),
            local_categories: Vec::new(),
            items: Vec::new(),
            limit: ITEMS_PAGE,
            selected: None,
            blocks: Vec::new(),
            article_text: String::new(),
            block_heights: RowHeights::default(),
            row_heights: RefCell::default(),
            scroll_to_selected: false,
            layout,
            reading: false,
            narrow: false,
            show_feeds: false,
            zoom,
            exit: Exit::Running,
            file_busy: false,
            file_result: None,
        };
        app.reload();
        Ok(app)
    }

    /// For telling the app about going to the background and back, from any thread.
    pub fn remote(&self) -> worker::Remote {
        worker::Remote(self.tx.clone())
    }

    /// Shown when signing in again failed: nothing syncs until it works.
    fn sign_in_problem_bar(&mut self, ui: &mut Ui) {
        let problem = self.sign_in_problem.clone().unwrap_or_default();
        ui.horizontal_wrapped(|ui| {
            let text = RichText::new(format!("Not signed in, so nothing syncs: {problem}"));
            ui.label(text.color(ui.visuals().warn_fg_color));
            if ui.button("Try again").clicked() {
                self.sign_in_problem = None;
                self.status = "Signing in…".into();
                self.send(Cmd::Login(Login::Saved));
            }
            if ui.button("Sign out").clicked() {
                self.send(Cmd::Logout { force: false });
            }
        });
    }

    /// Goes back one step (like Android's back button): closes a dialog, a
    /// settings section, the settings, an article or the feeds. Returns false
    /// if there was nothing to go back from.
    pub fn go_back(&mut self) -> bool {
        if self.dialog.take().is_some() {
            return true;
        }
        if let Some(page) = &mut self.settings {
            if self.narrow && page.showing {
                page.showing = false;
            } else {
                self.settings = None;
            }
            return true;
        }
        if self.reading && self.selected.is_some() && (self.narrow || self.layout.is_cards()) {
            self.reading = false;
            return true;
        }
        std::mem::take(&mut self.show_feeds)
    }

    fn send(&self, cmd: Cmd) {
        // Only fails if the worker thread died, which leaves nothing to do.
        let _ = self.tx.send(cmd);
    }

    fn reload(&mut self) {
        let result = (|| -> Result<()> {
            self.feeds = self.db.feeds()?;
            self.favorites = self.db.favorites_count()?;
            self.follows_people = self.db.follows_people()?;
            self.categories = self.db.categories()?;
            self.local_categories = self.db.local_categories()?;
            self.items = self.db.items_matching(&self.view, self.unread_only, &self.search, self.limit)?;
            Ok(())
        })();
        if let Err(e) = result {
            self.status = format!("Database error: {e:#}");
        }
    }

    fn handle_messages(&mut self, ctx: &egui::Context) {
        while let Ok(msg) = self.rx.try_recv() {
            match msg {
                Msg::LoggedIn { npub } => {
                    self.npub = npub;
                    self.sign_in_problem = None;
                    self.screen = Screen::Main;
                    self.login = LoginForm::default();
                    self.reload();
                }
                Msg::ConnectLink(link) => self.login.connect_link = Some(link),
                Msg::LoginFailed(error) if self.screen == Screen::Login => {
                    self.login.busy = false;
                    // Cancelling a QR code sign-in isn't a problem worth showing.
                    let cancelled = self.login.connect_link.take().is_some() && error == "cancelled";
                    self.login.error = (!cancelled).then_some(error);
                }
                Msg::LoginFailed(error) => {
                    self.status = "Not signed in".into();
                    self.sign_in_problem = Some(error);
                }
                Msg::LoggedOut => {
                    self.screen = Screen::Login;
                    self.dialog = None;
                    self.settings = None;
                    self.selected = None;
                    self.view = View::All;
                    self.reload();
                }
                Msg::LogoutBlocked => {
                    self.status = "Couldn't save to relays".into();
                    self.dialog = Some(Dialog::ConfirmSignOut);
                }
                Msg::Status(status) => self.status = status,
                Msg::FeedAdded(Ok(url)) => {
                    self.dialog = None;
                    self.view = View::Feed(url);
                    self.reload();
                }
                Msg::FeedAdded(Err(error)) => {
                    if let Some(Dialog::AddFeed { busy, error: e, .. }) = &mut self.dialog {
                        *busy = false;
                        *e = Some(error);
                    }
                }
                Msg::Changed => {
                    if let Some(page) = &mut self.settings {
                        page.report = None;
                    }
                    self.reload();
                }
                Msg::ExitReady => self.exit = Exit::Ready,
                Msg::Erased(Ok(done)) => {
                    // Every setting is gone too, so look like a fresh install.
                    ctx.set_theme(ThemePreference::System);
                    ctx.set_zoom_factor(1.0);
                    self.zoom = 1.0;
                    self.layout = ListLayout::List;
                    self.screen = Screen::Login;
                    self.dialog = None;
                    self.settings = None;
                    self.selected = None;
                    self.view = View::All;
                    self.search.clear();
                    self.npub.clear();
                    self.login = LoginForm { notice: Some(done), ..Default::default() };
                    self.reload();
                }
                Msg::Erased(Err(error)) => {
                    if let Some(Dialog::ConfirmErase { busy, error: e, .. }) = &mut self.dialog {
                        *busy = false;
                        *e = Some(error);
                    }
                }
                Msg::FileDone(result) => {
                    self.file_busy = false;
                    if result.is_some() {
                        self.file_result = result;
                    }
                }
            }
        }
    }

    fn select(&mut self, item: ItemRow) {
        self.blocks = html::to_blocks(&self.db.item_content(&item.key).unwrap_or_default());
        self.block_heights = RowHeights::default();
        let texts = self.blocks.iter().map(Block::text).filter(|text| !text.is_empty());
        self.article_text = std::iter::once(item_title(&item.title)).chain(texts).collect::<Vec<_>>().join("\n\n");
        if item.unread {
            self.set_read(&item.key, true);
        }
        self.selected = self.items.iter().find(|i| i.key == item.key).cloned().or(Some(item));
        self.reading = true;
    }

    fn set_layout(&mut self, layout: ListLayout) {
        self.layout = layout;
        self.reading = false;
        let _ = self.db.set_meta("layout", layout.key());
    }

    fn set_read(&mut self, key: &str, read: bool) {
        if self.db.set_read(&[key.to_string()], read).is_ok() {
            self.update_item(key, |i| i.unread = !read);
        }
    }

    fn set_fav(&mut self, key: &str, fav: bool) {
        if self.db.set_fav(key, fav).is_ok() {
            self.update_item(key, |i| i.fav = fav);
        }
    }

    /// Updates an item on screen without reloading the list, so it doesn't jump around.
    fn update_item(&mut self, key: &str, change: impl Fn(&mut ItemRow)) {
        self.items.iter_mut().chain(self.selected.as_mut()).filter(|i| i.key == key).for_each(change);
        self.feeds = self.db.feeds().unwrap_or_default();
        self.favorites = self.db.favorites_count().unwrap_or_default();
        self.follows_people = self.db.follows_people().unwrap_or_default();
    }

    fn set_view(&mut self, view: View) {
        self.view = view;
        self.reading = false;
        self.show_feeds = false;
        self.limit = ITEMS_PAGE;
        self.items = self.db.items_matching(&self.view, self.unread_only, &self.search, self.limit).unwrap_or_default();
    }

    /// Asks before marking every unread article in `view` (matching `search`) as read.
    fn confirm_mark_read(&mut self, view: View, search: &str) {
        let count = self.db.unread_keys(&view, search).map_or(0, |keys| keys.len());
        if count > 0 {
            self.dialog = Some(Dialog::ConfirmMarkRead { view, search: search.to_string(), count });
        }
    }

    fn view_title(&self, view: &View) -> String {
        match view {
            View::All => "All items".to_string(),
            View::Favorites => "Favorites".to_string(),
            View::Feed(url) => self.feeds.iter().find(|f| &f.url == url).map_or(url.clone(), |f| f.title.clone()),
            View::Category(name) | View::Person(name) => name.clone(),
        }
    }

    // ---- Login ----

    fn sign_in(&mut self, login: Login) {
        self.login.busy = true;
        self.login.error = None;
        // Read by the web version before it keeps anything in the browser.
        let _ = self.db.set_meta("remember", if self.login.forget { "0" } else { "1" });
        self.send(Cmd::Login(login));
    }

    fn login_screen(&mut self, ui: &mut Ui) {
        // Scrolls on short screens: a phone held sideways, a large interface size.
        CentralPanel::default().show(ui, |ui| {
            ScrollArea::vertical().auto_shrink(false).show(ui, |ui| {
                ui.vertical_centered(|ui| {
                    ui.add_space(((ui.available_height() - 520.0) / 2.0).max(24.0));
                    ui.set_max_width(400.0);
                    ui.label(RichText::new("Feedstr").size(36.0).strong());
                    ui.label(RichText::new("Your feeds, synced privately over Nostr").weak());
                    ui.add_space(28.0);
                    if let Some(notice) = &self.login.notice {
                        card(ui, |ui| {
                            ui.add(Label::new(notice.as_str()).wrap());
                        });
                        ui.add_space(12.0);
                    }

                    let enabled = !self.login.busy;
                    #[cfg(target_arch = "wasm32")]
                    {
                        card(ui, |ui| {
                            ui.label(RichText::new("Browser extension").strong());
                            let hint = "Alby, nos2x or another Nostr extension (NIP-07) that supports NIP-44";
                            ui.label(RichText::new(hint).small().weak());
                            if ui.add_enabled(enabled, wide(primary_button("Use extension"), ui)).clicked() {
                                self.sign_in(Login::Extension);
                            }
                        });
                        ui.add_space(12.0);
                    }
                    if self.has_signer_app {
                        card(ui, |ui| {
                            ui.label(RichText::new("Signer app").strong());
                            let hint = "Amber or another Nostr signer app (NIP-55) keeps your key";
                            ui.label(RichText::new(hint).small().weak());
                            if ui.add_enabled(enabled, wide(primary_button("Use signer app"), ui)).clicked() {
                                self.sign_in(Login::SignerApp);
                            }
                        });
                        ui.add_space(12.0);
                    }
                    card(ui, |ui| {
                        ui.label(RichText::new("Sync Key").strong());
                        let edit = TextEdit::singleline(&mut self.login.sync_key).password(true).hint_text("nsec1…");
                        let response = ui.add_enabled(enabled, edit.desired_width(f32::INFINITY));
                        let submit = response.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter));
                        if ui.add_enabled(enabled, wide(primary_button("Sign in"), ui)).clicked() || submit {
                            self.sign_in(Login::SyncKey(self.login.sync_key.clone()));
                        }
                    });
                    ui.add_space(12.0);
                    card(ui, |ui| {
                        ui.label(RichText::new("Remote signer").strong());
                        let edit = TextEdit::singleline(&mut self.login.bunker).hint_text("bunker://…");
                        ui.add_enabled(enabled, edit.desired_width(f32::INFINITY));
                        if ui.add_enabled(enabled, wide(primary_button("Connect"), ui)).clicked() {
                            self.sign_in(Login::Bunker(self.login.bunker.clone()));
                        }
                        // On a computer, the signer app on a phone can scan a code instead.
                        #[cfg(not(target_os = "android"))]
                        match self.login.connect_link.clone() {
                            Some(link) if self.login.busy => {
                                ui.add_space(8.0);
                                let hint = "Scan this with your Nostr signer app (Amber, nsec.app, …)";
                                ui.label(RichText::new(hint).small());
                                qr_code(ui, &link);
                                ui.horizontal(|ui| {
                                    if ui.button("Copy link").clicked() {
                                        ui.ctx().copy_text(link.clone());
                                    }
                                    if ui.button("Cancel").clicked() {
                                        self.cancel_connect.notify_waiters();
                                    }
                                });
                            }
                            _ => {
                                if ui.add_enabled(enabled, wide(Button::new("Sign in with a QR code"), ui)).clicked() {
                                    self.sign_in(Login::QrCode);
                                }
                            }
                        }
                    });
                    #[cfg(target_arch = "wasm32")]
                    {
                        ui.add_space(12.0);
                        let mut remember = !self.login.forget;
                        ui.checkbox(&mut remember, "Remember me on this browser");
                        self.login.forget = !remember;
                        let hint = if remember {
                            "Your sign-in and feeds are kept in this browser until you sign out."
                        } else {
                            "Nothing is kept in this browser: you sign in again next time."
                        };
                        ui.label(RichText::new(hint).small().weak());
                    }
                    ui.add_space(16.0);
                    if self.login.busy {
                        ui.horizontal(|ui| {
                            ui.spinner();
                            ui.label(&self.status);
                        });
                    } else if let Some(error) = &self.login.error {
                        ui.colored_label(ui.visuals().error_fg_color, error);
                    }
                    ui.add_space(16.0);
                    ui.label(RichText::new("New here?").weak());
                    if ui.add_enabled(enabled, Button::new("Create a Sync Key")).clicked() {
                        let nsec = Keys::generate().secret_key().to_bech32().expect("bech32 encoding can't fail");
                        self.dialog = Some(Dialog::NewSyncKey { nsec });
                    }
                    ui.add_space(24.0);
                })
            });
        });
    }

    // ---- Main screen ----

    fn top_bar(&mut self, ui: &mut Ui) {
        ui.horizontal(|ui| {
            if self.narrow {
                // The feeds take turns with the articles.
                if ui.add(Button::new(if self.show_feeds { "✖" } else { "☰" }).frame(false)).clicked() {
                    self.show_feeds = !self.show_feeds;
                    self.reading = false;
                }
            } else {
                ui.label(RichText::new("Feedstr").size(18.0).strong());
                ui.add_space(12.0);
            }
            let hint = "Download changes from relays, fetch all feeds, then upload changes (R)";
            if ui.button(if self.narrow { "⟳" } else { "⟳  Sync" }).on_hover_text(hint).clicked() {
                self.send(Cmd::SyncNow);
            }
            if ui.button(if self.narrow { "✚" } else { "✚  Add feed" }).clicked() {
                let category = match &self.view {
                    View::Category(name) => name.clone(),
                    _ => String::new(),
                };
                self.dialog =
                    Some(Dialog::AddFeed { input: String::new(), category, busy: false, error: None, focus: true });
            }
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if ui.button("⚙").on_hover_text("Settings").clicked() {
                    self.settings = Some(SettingsPage {
                        tab: SettingsTab::Account,
                        new_relay: String::new(),
                        show_key: false,
                        new_filter: String::new(),
                        filter_scope: String::new(),
                        report: None,
                        showing: false,
                    });
                }
                ui.add(Label::new(RichText::new(&self.status).small().weak()).truncate()).on_hover_text(&self.status);
            });
        });
    }

    fn sidebar(&mut self, ui: &mut Ui) {
        let total: i64 = self.feeds.iter().map(|f| f.unread).sum();
        let categories = self.categories.clone();
        let mut action = None;
        ScrollArea::vertical().auto_shrink(false).show(ui, |ui| {
            ui.add_space(4.0);
            if nav_button(ui, "All items", total, self.view == View::All).clicked() {
                action = Some(FeedAction::Show(View::All));
            }
            if nav_button(ui, "★  Favorites", self.favorites, self.view == View::Favorites).clicked() {
                action = Some(FeedAction::Show(View::Favorites));
            }
            ui.add_space(14.0);
            ui.label(RichText::new("FEEDS").small().weak());
            ui.add_space(2.0);
            if self.feeds.is_empty() {
                ui.label(RichText::new("No feeds yet. Add one with “✚ Add feed”.").weak());
            }
            // Feeds come sorted by category, with uncategorized ones last.
            for group in self.feeds.chunk_by(|a, b| a.category == b.category) {
                let category = &group[0].category;
                if category.is_empty() {
                    for feed in group {
                        if feed.url == longform::FOLLOWS_URL {
                            let open = !self.collapsed.contains(&feed.url);
                            follows_group(ui, feed, &self.follows_people, &self.view, open, &mut action);
                        } else {
                            feed_row(ui, feed, feed.local, &self.view, &categories, &mut action);
                        }
                    }
                    continue;
                }
                let open = !self.collapsed.contains(category);
                let local = group_is_local(category, group.iter(), &self.local_categories);
                ui.horizontal(|ui| {
                    let arrow = Button::new(if open { "⏷" } else { "⏵" }).frame(false);
                    if ui.add(arrow).clicked() {
                        action = Some(FeedAction::Fold(category.clone()));
                    }
                    let view = View::Category(category.clone());
                    let unread = group.iter().map(|f| f.unread).sum();
                    let title = if local { format!("{category}  · Local Only") } else { category.clone() };
                    let mut response = nav_button(ui, &title, unread, self.view == view);
                    if local {
                        response =
                            response.on_hover_text("Local Only: these feeds aren't synced to your other devices");
                    }
                    if response.clicked() {
                        action = Some(FeedAction::Show(view.clone()));
                    }
                    response.context_menu(|ui| {
                        if ui.button("Mark all as read").clicked() {
                            action = Some(FeedAction::MarkRead(view));
                        }
                        let toggle = if local { "Sync this group" } else { "Make Local Only" };
                        let hint = if local {
                            "Its feeds go back to your other devices"
                        } else {
                            "Keeps its feeds on this device only; your other devices drop them"
                        };
                        if ui.button(toggle).on_hover_text(hint).clicked() {
                            action = Some(FeedAction::SetSynced(category.clone(), local));
                        }
                        if ui.button("Rename…").clicked() {
                            action = Some(FeedAction::Rename(category.clone()));
                        }
                        if ui.button("Remove category").on_hover_text("Its feeds are kept").clicked() {
                            action = Some(FeedAction::RemoveCategory(category.clone()));
                        }
                    });
                });
                if open {
                    ui.indent(category, |ui| {
                        for feed in group {
                            // A group can be partly local (a feed moved in by another device
                            // stays synced), and then each local feed says so itself.
                            let marked = feed.local && !local;
                            feed_row(ui, feed, marked, &self.view, &categories, &mut action);
                        }
                    });
                }
            }
        });
        match action {
            Some(FeedAction::Show(view)) => self.set_view(view),
            Some(FeedAction::MarkRead(view)) => self.confirm_mark_read(view, ""),
            Some(FeedAction::Fold(category)) => {
                if !self.collapsed.remove(&category) {
                    self.collapsed.insert(category);
                }
            }
            Some(FeedAction::SetCategory(url, category)) if self.db.set_category(&url, &category).is_ok() => {
                self.reload();
            }
            Some(FeedAction::NewCategory(url)) => {
                self.dialog = Some(Dialog::NewCategory { url, name: String::new(), focus: true })
            }
            Some(FeedAction::Rename(from)) => self.dialog = Some(Dialog::RenameCategory { to: from.clone(), from }),
            Some(FeedAction::RemoveCategory(name)) => self.rename_category(&name, ""),
            Some(FeedAction::SetSynced(name, synced)) if self.db.set_category_synced(&name, synced).is_ok() => {
                self.reload();
            }
            Some(FeedAction::Unsubscribe(url)) if self.db.set_subscribed(&url, false).is_ok() => {
                if self.view == View::Feed(url) {
                    self.view = View::All;
                }
                self.reload();
            }
            _ => {}
        }
    }

    fn rename_category(&mut self, from: &str, to: &str) {
        if self.db.rename_category(from, to).is_ok() {
            if self.view == View::Category(from.to_string()) {
                self.view = if to.trim().is_empty() { View::All } else { View::Category(to.trim().to_string()) };
            }
            self.reload();
        }
    }

    fn item_list(&mut self, ui: &mut Ui) {
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            let title = self.view_title(&self.view);
            ui.add(Label::new(RichText::new(title).size(16.0).strong()).truncate());
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                let any_unread = self.items.iter().any(|i| i.unread);
                let mark = ui.add_enabled(any_unread, Button::new("✔").frame_when_inactive(false));
                // Every unread article in the view (matching the search), not just those listed.
                if mark.on_hover_text("Mark all as read").clicked() {
                    self.confirm_mark_read(self.view.clone(), &self.search.clone());
                }
                if ui.toggle_value(&mut self.unread_only, "Unread").changed() {
                    self.limit = ITEMS_PAGE;
                    self.reload();
                }
                let mut chosen = None;
                ui.menu_button("⊞", |ui| {
                    for layout in ListLayout::ALL {
                        if ui.add(Button::selectable(self.layout == layout, layout.label())).clicked() {
                            chosen = Some(layout);
                            ui.close();
                        }
                    }
                })
                .response
                .on_hover_text("Layout");
                if let Some(layout) = chosen {
                    self.set_layout(layout);
                }
            });
        });
        ui.add_space(2.0);
        // Right to left, so the text field takes exactly the space left by the button.
        let row = vec2(ui.available_width(), ui.spacing().interact_size.y);
        ui.allocate_ui_with_layout(row, Layout::right_to_left(Align::Center), |ui| {
            let mut changed = false;
            if !self.search.is_empty() && ui.small_button("✖").on_hover_text("Clear search").clicked() {
                self.search.clear();
                changed = true;
            }
            // A fixed id keeps the focus when the clear button appears.
            let edit = TextEdit::singleline(&mut self.search).id_salt("search").hint_text("Search titles and text");
            changed |= ui.add(edit.desired_width(f32::INFINITY)).changed();
            if changed {
                self.limit = ITEMS_PAGE;
                self.reload();
            }
        });
        ui.add_space(4.0);
        ui.separator();
        if self.items.is_empty() {
            ui.add_space(24.0);
            // The web version shows other feeds only as your other devices cached them.
            let waiting = cfg!(target_arch = "wasm32")
                && matches!(&self.view, View::Feed(url) if !longform::is_nostr(url))
                && !self.unread_only
                && self.search.is_empty();
            let empty = if waiting {
                "Nothing here yet: your other devices fetch this feed, and its articles show up after they sync."
            } else {
                "Nothing here"
            };
            ui.vertical_centered(|ui| ui.add(Label::new(RichText::new(empty).weak()).wrap()));
        }
        ui.style_mut().interaction.selectable_labels = false;
        let (clicked, at_end) = match self.layout {
            ListLayout::List => self.standard_list(ui),
            ListLayout::CompactList => self.compact_list(ui),
            ListLayout::Cards | ListLayout::CompactCards => self.card_grid(ui),
        };
        self.scroll_to_selected = false;
        // Scrolled to the end of a full page: list older articles too.
        if at_end && self.items.len() >= self.limit {
            self.limit += ITEMS_PAGE;
            self.reload();
        }
        if let Some(item) = clicked {
            self.select(item);
        }
    }

    fn is_selected(&self, item: &ItemRow) -> bool {
        self.selected.as_ref().is_some_and(|s| s.key == item.key)
    }

    /// Rows of varying height (titles wrap); only the visible ones are laid out.
    /// Returns the clicked article, and whether the end of the list is in view.
    fn standard_list(&self, ui: &mut Ui) -> (Option<ItemRow>, bool) {
        let (mut clicked, mut at_end) = (None, false);
        let heights = &mut *self.row_heights.borrow_mut();
        ScrollArea::vertical().auto_shrink(false).show(ui, |ui| {
            heights.check_width(ui.available_width());
            for item in &self.items {
                let selected = self.is_selected(item);
                let follow = selected && self.scroll_to_selected;
                at_end = heights.row(ui, &item.key, 72.0, follow, |ui| {
                    let response = item_row(ui, item, selected);
                    if follow {
                        response.scroll_to_me(None);
                    }
                    if response.clicked() {
                        clicked = Some(item.clone());
                    }
                });
            }
        });
        (clicked, at_end)
    }

    /// Rows of equal height, so only the visible ones are laid out.
    fn compact_list(&self, ui: &mut Ui) -> (Option<ItemRow>, bool) {
        let height = compact_row_height(ui);
        let (mut clicked, mut at_end) = (None, false);
        ui.spacing_mut().item_spacing.y = 0.0;
        let selected_row = self.selected.as_ref().and_then(|s| self.items.iter().position(|i| i.key == s.key));
        let mut scroll = ScrollArea::vertical().auto_shrink(false);
        if let Some(row) = selected_row.filter(|_| self.scroll_to_selected) {
            scroll = scroll.vertical_scroll_offset((row as f32 * height - ui.available_height() / 2.0).max(0.0));
        }
        scroll.show_rows(ui, height, self.items.len(), |ui, rows| {
            at_end = rows.end == self.items.len();
            for item in &self.items[rows] {
                if compact_row(ui, item, self.is_selected(item)).clicked() {
                    clicked = Some(item.clone());
                }
            }
        });
        (clicked, at_end)
    }

    /// Cards in rows filling the width; only the visible rows are laid out.
    fn card_grid(&self, ui: &mut Ui) -> (Option<ItemRow>, bool) {
        let min = self.layout.card_size();
        let width = ui.available_width() - 16.0;
        let columns = (((width + CARD_GAP) / (min.x + CARD_GAP)) as usize).max(1);
        let card = vec2((width - CARD_GAP * (columns - 1) as f32) / columns as f32, min.y);
        let compact = self.layout == ListLayout::CompactCards;
        let (mut clicked, mut at_end) = (None, false);
        ui.spacing_mut().item_spacing = vec2(CARD_GAP, CARD_GAP);
        let selected_row =
            self.selected.as_ref().and_then(|s| self.items.iter().position(|i| i.key == s.key)).map(|i| i / columns);
        let mut scroll = ScrollArea::vertical().auto_shrink(false);
        if let Some(row) = selected_row.filter(|_| self.scroll_to_selected) {
            let offset = row as f32 * (card.y + CARD_GAP) - (ui.available_height() - card.y) / 2.0;
            scroll = scroll.vertical_scroll_offset(offset.max(0.0));
        }
        let rows = self.items.len().div_ceil(columns);
        let total = rows;
        scroll.show_rows(ui, card.y, rows, |ui, rows| {
            at_end = rows.end == total;
            ui.add_space(4.0);
            for row in rows {
                ui.horizontal(|ui| {
                    for item in self.items.iter().skip(row * columns).take(columns) {
                        if item_card(ui, item, self.is_selected(item), card, compact).clicked() {
                            clicked = Some(item.clone());
                        }
                    }
                });
            }
        });
        (clicked, at_end)
    }

    fn reader(&mut self, ui: &mut Ui) {
        let Some(item) = self.selected.clone() else {
            ui.centered_and_justified(|ui| ui.label(RichText::new("Select an article to read it").weak()));
            return;
        };
        let back = self.layout.is_cards() || self.narrow;
        ScrollArea::vertical().auto_shrink(false).id_salt(&item.key).show(ui, |ui| {
            let width = (ui.available_width() - 64.0).min(READER_WIDTH);
            let margin = (ui.available_width() - width) / 2.0;
            ui.horizontal_top(|ui| {
                ui.add_space(margin);
                ui.vertical(|ui| {
                    ui.set_width(width - 8.0);
                    ui.add_space(if back { 12.0 } else { 28.0 });
                    if back && ui.button("‹  Back").on_hover_text("Esc").clicked() {
                        self.reading = false;
                    }
                    let mut meta = vec![item.feed_title.clone()];
                    meta.extend(
                        (!item.author.is_empty() && item.author != item.feed_title).then(|| item.author.clone()),
                    );
                    meta.push(format_date(item.published, true));
                    ui.label(RichText::new(meta.join("  ·  ")).weak());
                    ui.add_space(6.0);
                    let title = item_title(&item.title);
                    ui.add(Label::new(RichText::new(&title).size(26.0).strong()).wrap())
                        .context_menu(|ui| copy_menu(ui, &title, &self.article_text, None));
                    ui.add_space(10.0);
                    // Wrapped, so the buttons fit on a phone too.
                    ui.horizontal_wrapped(|ui| {
                        let star = if item.fav { "★  Favorite" } else { "☆  Favorite" };
                        if ui.add(Button::new(star).selected(item.fav)).on_hover_text("S").clicked() {
                            self.set_fav(&item.key, !item.fav);
                        }
                        let read = if item.unread { "Mark read" } else { "Mark unread" };
                        if ui.button(read).on_hover_text("M").clicked() {
                            self.set_read(&item.key, item.unread);
                        }
                        // A Nostr post's web link shows it on Ditto; it can also go to a Nostr app.
                        let nostr = longform::nostr_link(&item.link);
                        let web = if nostr.is_some() { "Open in Ditto  ↗" } else { "Open in browser  ↗" };
                        if html::is_web_link(&item.link) && ui.button(web).on_hover_text("O").clicked() {
                            ui.ctx().open_url(OpenUrl::new_tab(&item.link));
                        }
                        if let Some(link) = nostr.filter(|_| self.platform.opens_nostr_links())
                            && ui.button("Open in Nostr app  ↗").clicked()
                            && let Err(e) = self.platform.open_nostr_link(&link)
                        {
                            self.status = format!("{e:#}");
                        }
                    });
                    ui.add_space(14.0);
                    ui.separator();
                    ui.add_space(14.0);
                    self.block_heights.check_width(ui.available_width());
                    for (i, block) in self.blocks.iter().enumerate() {
                        let article = &self.article_text;
                        self.block_heights.row(ui, &i.to_string(), 40.0, false, |ui| show_block(ui, block, article));
                    }
                    ui.add_space(40.0);
                });
            });
        });
    }

    fn shortcuts(&mut self, ctx: &egui::Context) {
        if ctx.text_edit_focused() || self.dialog.is_some() {
            return;
        }
        let pressed = |key| ctx.input(|i| i.key_pressed(key));
        let index = self.selected.as_ref().and_then(|s| self.items.iter().position(|i| i.key == s.key));
        let next = if pressed(Key::J) || pressed(Key::ArrowDown) {
            Some(index.map_or(0, |i| (i + 1).min(self.items.len().saturating_sub(1))))
        } else if pressed(Key::K) || pressed(Key::ArrowUp) {
            Some(index.map_or(0, |i| i.saturating_sub(1)))
        } else {
            None
        };
        if let Some(item) = next.and_then(|i| self.items.get(i)).cloned() {
            self.select(item);
            self.scroll_to_selected = true;
        }
        if let Some(item) = self.selected.clone() {
            if pressed(Key::S) {
                self.set_fav(&item.key, !item.fav);
            }
            if pressed(Key::M) {
                self.set_read(&item.key, item.unread);
            }
            if pressed(Key::O) && html::is_web_link(&item.link) {
                ctx.open_url(OpenUrl::new_tab(&item.link));
            }
        }
        if pressed(Key::R) {
            self.send(Cmd::SyncNow);
        }
        if pressed(Key::Escape) {
            self.reading = false;
        }
    }

    // ---- Dialogs ----

    fn dialogs(&mut self, ctx: &egui::Context) {
        let Some(mut dialog) = self.dialog.take() else { return };
        let mut open = true;
        let frame = Frame::popup(&ctx.global_style()).inner_margin(20).corner_radius(12);
        let modal = Modal::new(Id::unique("dialog")).frame(frame).show(ctx, |ui| {
            ui.set_width(440.0_f32.min(ctx.content_rect().width() - 72.0));
            match &mut dialog {
                Dialog::AddFeed { input, category, busy, error, focus } => {
                    ui.label(RichText::new("Add a feed").size(18.0).strong());
                    let hint = "Paste a feed address or a website that has one. For a Nostr writer's long-form posts, \
                                paste their npub or NIP-05 address (like name@example.com).";
                    ui.add(Label::new(RichText::new(hint).weak()).wrap());
                    ui.add_space(8.0);
                    let edit =
                        TextEdit::singleline(input).hint_text("https://example.com").desired_width(f32::INFINITY);
                    let response = ui.add_enabled(!*busy, edit);
                    focus_on_open(&response, focus);
                    let enter = response.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter));
                    ui.add_space(4.0);
                    let edit =
                        TextEdit::singleline(category).hint_text("Category (optional)").desired_width(f32::INFINITY);
                    ui.add_enabled(!*busy, edit);
                    let categories = self.categories.clone();
                    if !categories.is_empty() {
                        ui.horizontal_wrapped(|ui| {
                            for name in categories {
                                if ui.add(Button::selectable(*category == name, name.as_str()).small()).clicked() {
                                    *category = name;
                                }
                            }
                        });
                    }
                    if let Some(error) = error {
                        ui.colored_label(ui.visuals().error_fg_color, error.as_str());
                    }
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        let add = ui.add_enabled(!*busy && !input.trim().is_empty(), primary_button("Add"));
                        if (add.clicked() || enter) && !input.trim().is_empty() {
                            *busy = true;
                            *error = None;
                            self.send(Cmd::AddFeed { input: input.clone(), category: category.clone() });
                        }
                        if ui.button("Cancel").clicked() {
                            open = false;
                        }
                        if *busy {
                            ui.spinner();
                        }
                    });
                }
                Dialog::NewCategory { url, name, focus } => {
                    ui.label(RichText::new("New category").size(18.0).strong());
                    ui.add_space(8.0);
                    let response = ui.add(TextEdit::singleline(name).desired_width(f32::INFINITY));
                    focus_on_open(&response, focus);
                    let enter = response.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter));
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        let valid = !name.trim().is_empty();
                        if (ui.add_enabled(valid, primary_button("Create")).clicked() || enter)
                            && valid
                            && self.db.set_category(url, name).is_ok()
                        {
                            self.reload();
                            open = false;
                        }
                        if ui.button("Cancel").clicked() {
                            open = false;
                        }
                    });
                }
                Dialog::RenameCategory { from, to } => {
                    ui.label(RichText::new(format!("Rename “{from}”")).size(18.0).strong());
                    ui.add_space(8.0);
                    let response = ui.add(TextEdit::singleline(to).desired_width(f32::INFINITY));
                    let enter = response.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter));
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        if (ui.add_enabled(!to.trim().is_empty(), primary_button("Rename")).clicked() || enter)
                            && !to.trim().is_empty()
                        {
                            self.rename_category(from, to);
                            open = false;
                        }
                        if ui.button("Cancel").clicked() {
                            open = false;
                        }
                    });
                }
                Dialog::ConfirmErase { understood, busy, error } => {
                    ui.label(RichText::new("Delete all Feedstr data?").size(18.0).strong());
                    ui.add(
                        Label::new(
                            "This deletes your feeds, categories, stored articles, read/unread state, favorites and \
                             settings from your relays and from this device, and signs you out. Your other devices \
                             erase their copies the next time they sync. This can't be undone.",
                        )
                        .wrap(),
                    );
                    ui.add_space(4.0);
                    let note = "Only Feedstr's data is deleted: your Nostr profile and posts are not affected. If you \
                                sign in with a Sync Key, make sure you have saved it somewhere else.";
                    ui.add(Label::new(RichText::new(note).weak()).wrap());
                    ui.add_space(8.0);
                    ui.add_enabled(!*busy, egui::Checkbox::new(understood, "I understand this can't be undone"));
                    if let Some(error) = error {
                        ui.colored_label(ui.visuals().error_fg_color, error.as_str());
                    }
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        let enabled = *understood && !*busy;
                        // Red only once it can be pressed, so it doesn't look ready before.
                        let delete = if enabled {
                            Button::new(RichText::new("Delete everything").color(Color32::WHITE))
                                .fill(ui.visuals().error_fg_color)
                        } else {
                            Button::new("Delete everything")
                        };
                        if ui.add_enabled(enabled, delete).clicked() {
                            *busy = true;
                            *error = None;
                            self.send(Cmd::EraseAll);
                        }
                        if ui.add_enabled(!*busy, Button::new("Cancel")).clicked() {
                            open = false;
                        }
                        if *busy {
                            ui.spinner();
                            ui.label(RichText::new(&self.status).weak());
                        }
                    });
                }
                Dialog::ConfirmMarkRead { view, search, count } => {
                    let articles = if *count == 1 { "1 article".to_string() } else { format!("{count} articles") };
                    ui.label(RichText::new(format!("Mark {articles} as read?")).size(18.0).strong());
                    let mut text = format!("Every unread article in “{}”", self.view_title(view));
                    if !search.trim().is_empty() {
                        text.push_str(&format!(" matching “{}”", search.trim()));
                    }
                    text.push_str(" is marked as read.");
                    ui.add(Label::new(text).wrap());
                    ui.add_space(8.0);
                    ui.horizontal_wrapped(|ui| {
                        if ui.add(primary_button("Mark as read")).clicked() {
                            let keys = self.db.unread_keys(view, search).unwrap_or_default();
                            if self.db.set_read(&keys, true).is_ok() {
                                self.reload();
                            }
                            open = false;
                        }
                        if ui.button("Cancel").clicked() {
                            open = false;
                        }
                    });
                }
                Dialog::ConfirmSignOut => {
                    ui.label(RichText::new("Sign out anyway?").size(18.0).strong());
                    ui.label(
                        "Feedstr couldn't confirm that your latest changes are saved on a relay. If you sign out \
                         now, any changes made on this device since the last successful sync are lost.",
                    );
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        if ui.add(primary_button("Stay signed in")).clicked() {
                            open = false;
                        }
                        let sign_out = Button::new(RichText::new("Sign out anyway").color(ui.visuals().error_fg_color));
                        if ui.add(sign_out).clicked() {
                            self.send(Cmd::Logout { force: true });
                            open = false;
                        }
                    });
                }
                Dialog::NewSyncKey { nsec } => {
                    ui.label(RichText::new("Your new Sync Key").size(18.0).strong());
                    ui.label(
                        "This key is your account. Save it in a password manager: you need it to sign in on other \
                         devices, and anyone who has it can read and change your feeds.",
                    );
                    ui.add_space(8.0);
                    ui.add(Label::new(RichText::new(nsec.as_str()).monospace()).wrap());
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        if ui.button("Copy").clicked() {
                            ui.ctx().copy_text(nsec.clone());
                        }
                        if ui.add(primary_button("I saved it, continue")).clicked() {
                            self.sign_in(Login::SyncKey(nsec.clone()));
                            open = false;
                        }
                        if ui.button("Cancel").clicked() {
                            open = false;
                        }
                    });
                }
            }
        });
        let busy = matches!(dialog, Dialog::AddFeed { busy: true, .. } | Dialog::ConfirmErase { busy: true, .. });
        // Keep the dialog open, unless it was closed or replaced by another one.
        if open && !(modal.should_close() && !busy) && self.dialog.is_none() {
            self.dialog = Some(dialog);
        }
    }

    /// The settings page: sections on the left, the chosen one on the right.
    fn settings_page(&mut self, ui: &mut Ui) {
        let Some(mut page) = self.settings.take() else { return };
        let mut open = true;
        // On a phone the list of sections and the chosen one take turns.
        let narrow = self.narrow;
        let (list_shown, section_shown) = (!narrow || !page.showing, !narrow || page.showing);
        let nav = Frame::side_top_panel(ui.style()).inner_margin(Margin::symmetric(12, 12));
        let sections = |ui: &mut Ui| {
            if ui.button("‹  Back").on_hover_text("Esc").clicked() {
                open = false;
            }
            ui.add_space(16.0);
            ui.label(RichText::new("Settings").size(20.0).strong());
            ui.add_space(8.0);
            for tab in SettingsTab::ALL {
                if nav_button(ui, tab.label(), 0, page.tab == tab && !narrow).clicked() {
                    page.tab = tab;
                    page.report = None;
                    page.showing = true;
                }
            }
        };
        if !narrow {
            Panel::left("settings").frame(nav).resizable(false).exact_size(220.0).show(ui, sections);
        } else if list_shown {
            CentralPanel::default().frame(nav).show(ui, sections);
        }
        if section_shown {
            CentralPanel::default().frame(Frame::central_panel(ui.style()).inner_margin(0)).show(ui, |ui| {
                ScrollArea::vertical().auto_shrink(false).id_salt(page.tab.label()).show(ui, |ui| {
                    let margin = if narrow { 16.0 } else { 32.0 };
                    let width = (ui.available_width() - 2.0 * margin).min(680.0);
                    ui.horizontal_top(|ui| {
                        ui.add_space(margin);
                        ui.vertical(|ui| {
                            ui.set_width(width);
                            ui.add_space(if narrow { 12.0 } else { 24.0 });
                            if narrow && ui.button("‹  Settings").clicked() {
                                page.showing = false;
                            }
                            ui.label(RichText::new(page.tab.label()).size(22.0).strong());
                            ui.add_space(12.0);
                            match page.tab {
                                SettingsTab::Account => self.account_settings(ui, &mut page.show_key),
                                SettingsTab::Sync => self.sync_settings(ui, &mut page.new_relay),
                                SettingsTab::Filters => self.filter_settings(ui, &mut page),
                                SettingsTab::Report => self.feed_report(ui, &mut page.report),
                                SettingsTab::Socials => self.socials_settings(ui),
                                SettingsTab::Data => self.data_settings(ui),
                                SettingsTab::Appearance => self.appearance_settings(ui),
                            }
                            ui.add_space(32.0);
                        });
                    });
                });
            });
        }
        let escape = ui.ctx().input(|i| i.key_pressed(Key::Escape));
        if escape && self.dialog.is_none() && !ui.ctx().text_edit_focused() {
            if narrow && page.showing { page.showing = false } else { open = false }
        }
        // Signing out or deleting everything leaves the settings too.
        if open && self.screen == Screen::Main {
            self.settings = Some(page);
        }
    }

    fn appearance_settings(&mut self, ui: &mut Ui) {
        self.size_settings(ui);
        ui.add_space(16.0);
        ui.label(RichText::new("Article list").strong());
        ui.horizontal_wrapped(|ui| {
            for layout in ListLayout::ALL {
                if ui.add(Button::selectable(self.layout == layout, layout.label())).clicked() {
                    self.set_layout(layout);
                }
            }
        });
        ui.add_space(16.0);
        ui.label(RichText::new("Theme").strong());
        ui.horizontal_wrapped(|ui| {
            let current = ui.ctx().options(|o| o.theme_preference);
            for (preference, label, name) in [
                (ThemePreference::System, "System", "system"),
                (ThemePreference::Light, "Light", "light"),
                (ThemePreference::Dark, "Dark", "dark"),
            ] {
                if ui.add(Button::selectable(current == preference, label)).clicked() {
                    ui.ctx().set_theme(preference);
                    let _ = self.db.set_meta("theme", name);
                }
            }
        });
    }

    fn filter_settings(&mut self, ui: &mut Ui, page: &mut SettingsPage) {
        let hint = "Hide articles that contain a word or phrase, in all feeds, a category or a single feed. \
                    Matching ignores case and only matches whole words. Favorites are never hidden. Filters are \
                    synced to your other devices.";
        ui.add(Label::new(RichText::new(hint).weak()).wrap());
        ui.add_space(12.0);
        // (scope, label) for every place a filter can apply.
        let mut scopes = vec![(String::new(), "All feeds".to_string())];
        let categories = self.categories.clone();
        scopes.extend(categories.into_iter().map(|c| (format!("category:{c}"), format!("Category: {c}"))));
        scopes.extend(self.feeds.iter().map(|f| (format!("feed:{}", f.url), format!("Feed: {}", f.title))));
        let scope_label = |scope: &str| {
            scopes
                .iter()
                .find(|(s, _)| s == scope)
                .map(|(_, label)| label.clone())
                .unwrap_or_else(|| scope.replacen("category:", "Category: ", 1).replacen("feed:", "Feed: ", 1))
        };
        ui.horizontal_wrapped(|ui| {
            let width = ui.available_width().min(220.0);
            let edit = TextEdit::singleline(&mut page.new_filter).hint_text("Word or phrase").desired_width(width);
            let response = ui.add(edit);
            let enter = response.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter));
            ComboBox::from_id_salt("filter scope").selected_text(scope_label(&page.filter_scope)).width(width).show_ui(
                ui,
                |ui| {
                    for (scope, _) in &scopes {
                        ui.selectable_value(&mut page.filter_scope, scope.clone(), scope_label(scope));
                    }
                },
            );
            let valid = !page.new_filter.trim().is_empty();
            if (ui.add_enabled(valid, primary_button("Add")).clicked() || enter)
                && valid
                && self.db.add_filter(&page.new_filter, &page.filter_scope).is_ok()
            {
                page.new_filter.clear();
                self.reload();
            }
        });
        ui.add_space(12.0);
        let filters = self.db.filters().unwrap_or_default();
        if filters.is_empty() {
            ui.label(RichText::new("No filters yet.").weak());
        }
        for filter in filters {
            ui.horizontal_wrapped(|ui| {
                if ui.small_button("✖").on_hover_text("Remove").clicked() && self.db.remove_filter(&filter.id).is_ok()
                {
                    self.reload();
                }
                ui.label(RichText::new(&filter.phrase).strong());
                ui.label(RichText::new(scope_label(&filter.scope)).weak());
            });
        }
    }

    fn socials_settings(&mut self, ui: &mut Ui) {
        let hint = "Feedstr can follow long-form posts (NIP-23) from Nostr. To follow one writer, use “Add feed” \
                    with their npub or NIP-05 address (like name@example.com); you don't need to follow them on \
                    Nostr.";
        ui.add(Label::new(RichText::new(hint).weak()).wrap());
        ui.add_space(16.0);
        ui.label(RichText::new("Everyone you follow").strong());
        let follows = self.feeds.iter().find(|f| f.url == longform::FOLLOWS_URL);
        let mut on = follows.is_some();
        let category = follows.map(|f| f.category.clone()).unwrap_or_default();
        if ui.checkbox(&mut on, "Show long-form posts from everyone you follow").changed()
            && self.db.set_subscribed(longform::FOLLOWS_URL, on).is_ok()
        {
            if on {
                let _ = self.db.set_title_if_empty(longform::FOLLOWS_URL, longform::FOLLOWS_TITLE);
                self.send(Cmd::FetchFeed(longform::FOLLOWS_URL.to_string()));
            }
            self.reload();
        }
        let hint = "Uses the follow list from your other Nostr apps.";
        ui.add(Label::new(RichText::new(hint).small().weak()).wrap());
        if on {
            ui.add_space(8.0);
            ui.horizontal_wrapped(|ui| {
                ui.label("Show it as");
                let label = |category: &str| {
                    if category.is_empty() {
                        "A group in the sidebar, with an entry per person".to_string()
                    } else {
                        format!("A single feed in “{category}”")
                    }
                };
                let mut chosen = category.clone();
                let combo = ComboBox::from_id_salt("follows placement").selected_text(label(&chosen));
                combo.width(ui.available_width().min(380.0)).show_ui(ui, |ui| {
                    ui.selectable_value(&mut chosen, String::new(), label(""));
                    for name in self.categories.clone() {
                        ui.selectable_value(&mut chosen, name.clone(), label(&name));
                    }
                });
                if chosen != category && self.db.set_category(longform::FOLLOWS_URL, &chosen).is_ok() {
                    self.reload();
                }
            });
        }
    }

    fn feed_report(&mut self, ui: &mut Ui, report: &mut Option<(u32, Vec<ReportRow>)>) {
        let current = self.db.meta("report_days").ok().flatten().and_then(|d| d.parse().ok());
        let current = current.unwrap_or(DEFAULT_REPORT_DAYS);
        let mut days = current;
        ui.horizontal_wrapped(|ui| {
            ui.label("Feeds that failed to update, or have had no new posts for");
            ui.add(DragValue::new(&mut days).range(1..=3650));
            ui.label("days.");
        });
        if days != current {
            let _ = self.db.set_meta("report_days", &days.to_string());
        }
        if cfg!(target_arch = "wasm32") {
            let note = "The web version only sees what your other devices cached on relays (by default the last \
                        30 days), and doesn't fetch feeds itself: check the report in the app for the full picture.";
            ui.add(Label::new(RichText::new(note).small().weak()).wrap());
        }
        ui.add_space(12.0);
        // Made once, not on every frame: it looks at every feed's items.
        if report.as_ref().is_none_or(|(made_for, _)| *made_for != days) {
            *report = Some((days, self.db.feed_report(days).unwrap_or_default()));
        }
        let rows = report.as_ref().map(|(_, rows)| rows.as_slice()).unwrap_or_default();
        if rows.is_empty() {
            ui.label(RichText::new("All your feeds look fine.").weak());
        }
        let mut unsubscribe = None;
        for row in rows {
            Frame::new().fill(ui.visuals().faint_bg_color).corner_radius(8).inner_margin(12).show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.add(Label::new(RichText::new(&row.title).strong()).truncate());
                ui.add(Label::new(RichText::new(&row.url).small().weak()).truncate());
                let problem = match (&row.error, row.latest) {
                    (Some(error), _) => {
                        RichText::new(format!("Update failed: {error}")).color(ui.visuals().error_fg_color)
                    }
                    (None, None) => RichText::new("No posts"),
                    (None, Some(latest)) => RichText::new(format!("Newest post: {}", format_date(latest, false))),
                };
                ui.add(Label::new(problem.small()).wrap());
                ui.horizontal_wrapped(|ui| {
                    if ui.button("Copy address").clicked() {
                        ui.ctx().copy_text(row.url.clone());
                    }
                    if ui.button("Unsubscribe").clicked() {
                        unsubscribe = Some(row.url.clone());
                    }
                });
            });
            ui.add_space(6.0);
        }
        if let Some(url) = unsubscribe
            && self.db.set_subscribed(&url, false).is_ok()
        {
            *report = None;
            if self.view == View::Feed(url) {
                self.view = View::All;
            }
            self.reload();
        }
    }

    fn size_settings(&mut self, ui: &mut Ui) {
        ui.label(RichText::new("Interface size").strong());
        ui.horizontal_wrapped(|ui| {
            let zoom = ui.ctx().zoom_factor();
            let smaller = ZOOM_STEPS.iter().rev().find(|&&z| z < zoom - 0.001);
            let larger = ZOOM_STEPS.iter().find(|&&z| z > zoom + 0.001);
            let mut wanted = None;
            if ui.add_enabled(smaller.is_some(), Button::new("−")).clicked() {
                wanted = smaller.copied();
            }
            ui.label(format!("{:.0}%", zoom * 100.0));
            if ui.add_enabled(larger.is_some(), Button::new("+")).clicked() {
                wanted = larger.copied();
            }
            if ui.add_enabled((zoom - 1.0).abs() > 0.001, Button::new("Reset")).clicked() {
                wanted = Some(1.0);
            }
            if let Some(zoom) = wanted {
                ui.ctx().set_zoom_factor(zoom);
            }
        });
        ui.add(Label::new(RichText::new("Ctrl + and Ctrl − change it too; Ctrl 0 resets.").small().weak()).wrap());
    }

    fn account_settings(&mut self, ui: &mut Ui, show_key: &mut bool) {
        ui.add(Label::new(RichText::new(&self.npub).monospace().small()).truncate());
        ui.horizontal_wrapped(|ui| {
            if ui.button("Copy public key").clicked() {
                ui.ctx().copy_text(self.npub.clone());
            }
            let sync_key = self.db.meta("sync_key").ok().flatten();
            if let Some(nsec) = &sync_key {
                ui.toggle_value(show_key, "Show Sync Key");
                if *show_key && ui.button("Copy Sync Key").clicked() {
                    ui.ctx().copy_text(nsec.clone());
                }
            } else {
                let signer = match self.db.meta("login").ok().flatten().as_deref() {
                    Some("signer_app") => "Signed in with a signer app",
                    Some("extension") => "Signed in with a browser extension",
                    _ => "Signed in with a remote signer",
                };
                ui.label(RichText::new(signer).weak());
            }
        });
        if *show_key && let Some(nsec) = self.db.meta("sync_key").ok().flatten() {
            ui.add(Label::new(RichText::new(nsec).monospace().small()).wrap());
        }
        ui.add_space(16.0);
        ui.separator();
        ui.add_space(8.0);
        let sign_out = Button::new(RichText::new("Sign out").color(ui.visuals().error_fg_color));
        if ui.add(sign_out).on_hover_text("Removes your feeds from this device").clicked() {
            self.send(Cmd::Logout { force: false });
        }
        let delete = Button::new(RichText::new("Delete all Feedstr data…").color(ui.visuals().error_fg_color));
        let hint = "Deletes your Feedstr data from your relays, this device and your other devices";
        if ui.add(delete).on_hover_text(hint).clicked() {
            self.dialog = Some(Dialog::ConfirmErase { understood: false, busy: false, error: None });
        }
    }

    fn data_settings(&mut self, ui: &mut Ui) {
        let hint = |ui: &mut Ui, text: &str| ui.add(Label::new(RichText::new(text).small().weak()).wrap());
        let sections = [
            ("Feeds (OPML)", FileKind::Opml, "Your subscriptions, to move them to or from another feed reader."),
            (
                "Full backup",
                FileKind::Backup,
                "Feeds, stored items, read/unread and favorites. Importing merges with what you have: newer changes \
                 are kept and nothing is deleted. Your Sync Key is not included.",
            ),
        ];
        for (title, kind, text) in sections {
            ui.label(RichText::new(title).strong());
            ui.horizontal_wrapped(|ui| {
                ui.add_enabled_ui(!self.file_busy, |ui| {
                    if ui.button("Import…").clicked() {
                        self.file_busy = true;
                        self.file_result = None;
                        self.send(Cmd::Import(kind));
                    }
                    if ui.button("Export…").clicked() {
                        self.file_busy = true;
                        self.file_result = None;
                        self.send(Cmd::Export(kind));
                    }
                });
            });
            hint(ui, text);
            ui.add_space(12.0);
        }
        if self.file_busy {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label("Working…");
            });
        }
        match &self.file_result {
            Some(Ok(done)) => {
                ui.add(Label::new(done.as_str()).wrap());
            }
            Some(Err(error)) => {
                ui.add(Label::new(RichText::new(error).color(ui.visuals().error_fg_color)).wrap());
            }
            None => {}
        }
    }

    fn sync_settings(&mut self, ui: &mut Ui, new_relay: &mut String) {
        let hint = |ui: &mut Ui, text: &str| ui.add(Label::new(RichText::new(text).small().weak()).wrap());

        ui.horizontal_wrapped(|ui| {
            ui.label("Sync automatically");
            let current = worker::sync_minutes(&self.db);
            let mut minutes = current;
            ComboBox::from_id_salt("schedule").selected_text(schedule_label(current)).show_ui(ui, |ui| {
                for &choice in worker::SYNC_SCHEDULES {
                    ui.selectable_value(&mut minutes, choice, schedule_label(choice));
                }
            });
            if minutes != current {
                let _ = self.db.set_meta("sync_minutes", &minutes.to_string());
                self.send(Cmd::ScheduleChanged);
            }
        });
        hint(
            ui,
            if cfg!(target_arch = "wasm32") {
                "Feedstr syncs when the page opens, and saves your changes to relays a few seconds after you make them."
            } else {
                "Feedstr always syncs when it starts, and saves your changes to relays when you close it."
            },
        );

        ui.add_space(12.0);
        ui.label(RichText::new("Relays").strong());
        let mut relays = worker::relays(&self.db);
        let mut changed = false;
        relays.retain(|relay| {
            let mut keep = true;
            ui.horizontal_wrapped(|ui| {
                if ui.small_button("✖").on_hover_text("Remove").clicked() {
                    keep = false;
                    changed = true;
                }
                ui.label(relay);
            });
            keep
        });
        ui.horizontal_wrapped(|ui| {
            let width = (ui.available_width() - 60.0).clamp(120.0, 300.0);
            let edit = ui.add(TextEdit::singleline(new_relay).hint_text("wss://…").desired_width(width));
            let enter = edit.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter));
            let relay = worker::parse_relay(new_relay).filter(|relay| !relays.contains(relay));
            if (ui.add_enabled(relay.is_some(), Button::new("Add")).clicked() || enter)
                && let Some(relay) = relay
            {
                relays.push(relay);
                new_relay.clear();
                changed = true;
            }
        });
        if worker::custom_relays(&self.db) && ui.button("Use the default relays").clicked() {
            relays = worker::DEFAULT_RELAYS.iter().map(|r| r.to_string()).collect();
            changed = true;
        }
        if changed {
            let _ = worker::set_relays(&self.db, &relays);
            self.send(Cmd::RelaysChanged);
        }
        hint(ui, "Unless you change them, Feedstr uses its default relays, and updates to Feedstr can change those.");

        ui.add_space(12.0);
        let categories = self.categories.clone();
        if !categories.is_empty() {
            ui.label(RichText::new("Groups").strong());
            for name in categories {
                let group = self.feeds.iter().filter(|f| f.category == name);
                let mut synced = !group_is_local(&name, group, &self.local_categories);
                if ui.checkbox(&mut synced, format!("Sync “{name}”")).changed()
                    && self.db.set_category_synced(&name, synced).is_ok()
                {
                    self.reload();
                }
            }
            hint(
                ui,
                "A Local Only group stays on this device: your other devices remove its feeds, its articles \
                 aren't cached on relays, and what you read or star in it isn't synced.",
            );
            ui.add_space(12.0);
        }
        ui.label(RichText::new("Cached on relays").strong());
        let current = self.db.cache_limits().ok().flatten().map(|l| l.0).unwrap_or_default();
        let mut limits = current;
        ui.horizontal_wrapped(|ui| {
            ui.label("Up to");
            ui.add(DragValue::new(&mut limits.items).range(0..=500));
            ui.label("items per feed, from the last");
            ui.add(DragValue::new(&mut limits.days).range(1..=3650));
            ui.label("days");
        });
        if limits != current {
            let _ = self.db.set_cache_limits(limits);
        }
        hint(
            ui,
            "Shared by all your devices. Clients that can't fetch feeds themselves, like the web version, only see \
             this cache. Favorites are always included.",
        );

        ui.add_space(12.0);
        let here = if cfg!(target_arch = "wasm32") { "Stored in this browser" } else { "Stored on this device" };
        ui.label(RichText::new(here).strong());
        let current = self.db.keep_days().unwrap_or(crate::db::DEFAULT_KEEP_DAYS);
        let mut forever = current == 0;
        let mut days = if forever { crate::db::SUGGESTED_KEEP_DAYS } else { current };
        ui.horizontal_wrapped(|ui| {
            ui.checkbox(&mut forever, "Keep everything");
            if !forever {
                ui.label("or delete items after");
                ui.add(DragValue::new(&mut days).range(1..=3650));
                ui.label("days");
            }
        });
        let wanted = if forever { 0 } else { days };
        if wanted != current {
            let _ = self.db.set_meta("keep_days", &wanted.to_string());
        }
        hint(ui, "Favorites, items still in their feed, and items cached on relays are always kept.");
    }

    /// Shown while closing waits for changes to be saved to relays.
    fn exit_dialog(&mut self, ctx: &egui::Context, since: i64) {
        let frame = Frame::popup(&ctx.global_style()).inner_margin(20).corner_radius(12);
        Modal::new(Id::unique("exit")).frame(frame).show(ctx, |ui| {
            ui.set_width(360.0);
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label(RichText::new("Saving your changes to relays…").strong());
            });
            ui.add_space(8.0);
            let waited = Duration::from_millis((now_ms() - since).max(0) as u64);
            if ui.button("Quit without saving").clicked() || waited > EXIT_TIMEOUT {
                self.exit = Exit::Ready;
            }
        });
        ctx.request_repaint_after(Duration::from_millis(500));
    }
}

/// Whether a whole group is Local Only: marked so, and none of its feeds still
/// synced (one another device moved into it is, until the group is set again).
fn group_is_local<'a>(category: &str, mut group: impl Iterator<Item = &'a FeedRow>, local: &[String]) -> bool {
    local.iter().any(|c| c == category) && group.all(|f| f.local)
}

fn schedule_label(minutes: u32) -> String {
    match minutes {
        // The web version can't count on being closed properly, so it syncs while open.
        0 if cfg!(target_arch = "wasm32") => format!("Every {} minutes (default)", worker::WEB_SYNC_MINUTES),
        0 => "Only at start and exit".into(),
        60 => "Every hour".into(),
        m if m % 60 == 0 => format!("Every {} hours", m / 60),
        m => format!("Every {m} minutes"),
    }
}

enum FeedAction {
    Show(View),
    MarkRead(View),
    /// Folds or unfolds a category in the sidebar.
    Fold(String),
    SetCategory(String, String),
    /// Asks for a new category name for a feed.
    NewCategory(String),
    Rename(String),
    RemoveCategory(String),
    /// Syncs a category, or keeps it on this device only.
    SetSynced(String, bool),
    Unsubscribe(String),
}

/// The follows feed at the top level: a group with an entry per person.
fn follows_group(
    ui: &mut Ui,
    feed: &FeedRow,
    people: &[(String, i64)],
    current: &View,
    open: bool,
    action: &mut Option<FeedAction>,
) {
    ui.horizontal(|ui| {
        let arrow = Button::new(if open { "⏷" } else { "⏵" }).frame(false);
        if ui.add(arrow).clicked() {
            *action = Some(FeedAction::Fold(feed.url.clone()));
        }
        let view = View::Feed(feed.url.clone());
        let title = if feed.error.is_some() { format!("⚠ {}", feed.title) } else { feed.title.clone() };
        let mut response = nav_button(ui, &title, feed.unread, *current == view);
        if let Some(error) = &feed.error {
            response = response.on_hover_text(format!("Last update failed: {error}"));
        }
        if response.clicked() {
            *action = Some(FeedAction::Show(view.clone()));
        }
        response.context_menu(|ui| {
            if ui.button("Mark all as read").clicked() {
                *action = Some(FeedAction::MarkRead(view));
            }
            if ui.button("Turn off").on_hover_text("Settings › Socials turns it on again").clicked() {
                *action = Some(FeedAction::Unsubscribe(feed.url.clone()));
            }
        });
    });
    if open {
        ui.indent(&feed.url, |ui| {
            for (name, unread) in people {
                let view = View::Person(name.clone());
                if nav_button(ui, name, *unread, *current == view).clicked() {
                    *action = Some(FeedAction::Show(view));
                }
            }
        });
    }
}

/// A feed in the sidebar; `marked` adds the Local Only label.
fn feed_row(
    ui: &mut Ui,
    feed: &FeedRow,
    marked: bool,
    current: &View,
    categories: &[String],
    action: &mut Option<FeedAction>,
) {
    let mut title = if feed.error.is_some() { format!("⚠ {}", feed.title) } else { feed.title.clone() };
    if marked {
        title += "  · Local Only";
    }
    let view = View::Feed(feed.url.clone());
    let mut response = nav_button(ui, &title, feed.unread, *current == view);
    if marked {
        response = response.on_hover_text("Local Only: this feed isn't synced to your other devices");
    }
    if let Some(error) = &feed.error {
        response = response.on_hover_text(format!("Last update failed: {error}"));
    }
    if response.clicked() {
        *action = Some(FeedAction::Show(view.clone()));
    }
    response.context_menu(|ui| {
        if ui.button("Mark all as read").clicked() {
            *action = Some(FeedAction::MarkRead(view));
        }
        ui.menu_button("Move to category", |ui| {
            for name in categories.iter().filter(|c| **c != feed.category) {
                if ui.button(name).clicked() {
                    *action = Some(FeedAction::SetCategory(feed.url.clone(), name.clone()));
                }
            }
            if !feed.category.is_empty() && ui.button("No category").clicked() {
                *action = Some(FeedAction::SetCategory(feed.url.clone(), String::new()));
            }
            ui.separator();
            if ui.button("New category…").clicked() {
                *action = Some(FeedAction::NewCategory(feed.url.clone()));
            }
        });
        if ui.button("Copy feed address").clicked() {
            ui.ctx().copy_text(feed.url.clone());
        }
        if ui.button("Unsubscribe").clicked() {
            *action = Some(FeedAction::Unsubscribe(feed.url.clone()));
        }
    });
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut Ui, _frame: &mut eframe::Frame) {
        self.handle_messages(ui.ctx());
        let ctx = ui.ctx().clone();
        self.narrow = ctx.content_rect().width() < NARROW_WIDTH;
        match self.screen {
            Screen::Login => self.login_screen(ui),
            Screen::Main if self.settings.is_some() => self.settings_page(ui),
            Screen::Main => {
                let bar = Frame::side_top_panel(ui.style()).inner_margin(Margin::symmetric(12, 8));
                Panel::top("top").frame(bar).show(ui, |ui| self.top_bar(ui));
                if self.sign_in_problem.is_some() {
                    let warning = bar.fill(ui.visuals().warn_fg_color.gamma_multiply(0.15));
                    Panel::top("sign-in problem").frame(warning).show(ui, |ui| self.sign_in_problem_bar(ui));
                }
                let side = Frame::side_top_panel(ui.style()).inner_margin(Margin::symmetric(8, 8));
                let list = Frame::central_panel(ui.style()).inner_margin(Margin::symmetric(6, 4));
                let whole = Frame::central_panel(ui.style()).inner_margin(0);
                if self.narrow {
                    // One pane at a time: the article, the feeds, or the list.
                    if self.reading && self.selected.is_some() {
                        CentralPanel::default().frame(whole).show(ui, |ui| self.reader(ui));
                    } else if self.show_feeds {
                        CentralPanel::default().frame(side).show(ui, |ui| self.sidebar(ui));
                    } else {
                        CentralPanel::default().frame(list).show(ui, |ui| self.item_list(ui));
                    }
                } else {
                    Panel::left("feeds")
                        .frame(side)
                        .resizable(true)
                        .default_size(240.0)
                        .show(ui, |ui| self.sidebar(ui));
                    if !self.layout.is_cards() {
                        Panel::left("items")
                            .frame(list)
                            .resizable(true)
                            .default_size(380.0)
                            .show(ui, |ui| self.item_list(ui));
                        CentralPanel::default().frame(whole).show(ui, |ui| self.reader(ui));
                    } else if self.reading && self.selected.is_some() {
                        CentralPanel::default().frame(whole).show(ui, |ui| self.reader(ui));
                    } else {
                        let grid = Frame::central_panel(ui.style()).inner_margin(Margin::symmetric(12, 4));
                        CentralPanel::default().frame(grid).show(ui, |ui| self.item_list(ui));
                    }
                }
                self.shortcuts(&ctx);
                // Remember the interface size, however it was changed.
                let zoom = ctx.zoom_factor();
                if (zoom - self.zoom).abs() > 0.001 {
                    self.zoom = zoom;
                    let _ = self.db.set_meta("zoom", &zoom.to_string());
                }
            }
        }
        self.dialogs(&ctx);
        self.handle_exit(&ctx);
    }
}

impl App {
    /// Closing first saves changes to relays (or gives up after a while).
    fn handle_exit(&mut self, ctx: &egui::Context) {
        if ctx.input(|i| i.viewport().close_requested()) && self.exit != Exit::Ready {
            ctx.send_viewport_cmd(ViewportCommand::CancelClose);
            if self.exit == Exit::Running {
                if self.screen == Screen::Main {
                    self.send(Cmd::Exit);
                    let _ = self.closing.send(true);
                    self.exit = Exit::Saving(now_ms());
                } else {
                    self.exit = Exit::Ready;
                }
            }
        }
        match self.exit {
            Exit::Saving(since) => self.exit_dialog(ctx, since),
            Exit::Ready => ctx.send_viewport_cmd(ViewportCommand::Close),
            Exit::Running => {}
        }
    }
}

fn setup_style(ctx: &egui::Context) {
    ctx.all_styles_mut(|style| {
        style.spacing.item_spacing = vec2(8.0, 6.0);
        style.spacing.button_padding = vec2(10.0, 5.0);
        style.spacing.interact_size.y = 26.0;
        style.visuals.hyperlink_color = ACCENT;
        style.visuals.selection.bg_fill = ACCENT.gamma_multiply(0.35);
        for text in style.text_styles.values_mut() {
            text.size *= 1.08;
        }
    });
    ctx.style_mut_of(Theme::Light, |style| {
        style.visuals.panel_fill = Color32::from_rgb(0xfb, 0xfb, 0xfc);
        style.visuals.window_fill = Color32::WHITE;
        style.visuals.faint_bg_color = Color32::from_rgb(0xf1, 0xf0, 0xf5);
        style.visuals.extreme_bg_color = Color32::from_rgb(0xf3, 0xf3, 0xf6);
    });
    ctx.style_mut_of(Theme::Dark, |style| {
        style.visuals.panel_fill = Color32::from_rgb(0x1b, 0x1b, 0x1f);
        style.visuals.window_fill = Color32::from_rgb(0x24, 0x24, 0x29);
        style.visuals.faint_bg_color = Color32::from_rgb(0x26, 0x26, 0x2c);
        style.visuals.extreme_bg_color = Color32::from_rgb(0x14, 0x14, 0x17);
    });
}

fn card(ui: &mut Ui, contents: impl FnOnce(&mut Ui)) {
    Frame::group(ui.style()).fill(ui.visuals().window_fill).corner_radius(10).inner_margin(16).show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.spacing_mut().text_edit_width = ui.available_width();
        contents(ui);
    });
}

fn primary_button(text: &str) -> Button<'_> {
    Button::new(RichText::new(text).color(Color32::WHITE)).fill(ACCENT)
}

fn wide<'a>(button: Button<'a>, ui: &Ui) -> Button<'a> {
    button.min_size(vec2(ui.available_width(), 32.0))
}

fn nav_button(ui: &mut Ui, title: &str, count: i64, selected: bool) -> egui::Response {
    let count = if count > 0 { count.to_string() } else { String::new() };
    let button = Button::selectable(selected, title).right_text(RichText::new(count).weak());
    ui.add(button.min_size(vec2(ui.available_width(), 30.0)).truncate())
}

fn item_row(ui: &mut Ui, item: &ItemRow, selected: bool) -> egui::Response {
    let background = ui.painter().add(Shape::Noop);
    let inner = Frame::new().inner_margin(Margin::symmetric(10, 8)).show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.horizontal(|ui| {
            if item.unread {
                ui.label(RichText::new("●").small().color(ACCENT));
            }
            ui.add(Label::new(RichText::new(&item.feed_title).small().weak()).truncate());
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                ui.label(RichText::new(format_date(item.published, false)).small().weak());
                if item.fav {
                    ui.label(RichText::new("★").small().color(ACCENT));
                }
            });
        });
        let title = RichText::new(item_title(&item.title));
        let title = if item.unread {
            title.strong()
        } else if selected {
            title
        } else {
            title.weak()
        };
        ui.add(Label::new(title).wrap());
    });
    let rect = inner.response.rect;
    let response = ui.interact(rect, Id::unique(("item", &item.key)), Sense::click());
    let visuals = ui.visuals();
    let fill = if selected {
        visuals.selection.bg_fill
    } else if response.hovered() {
        visuals.faint_bg_color
    } else {
        Color32::TRANSPARENT
    };
    ui.painter().set(background, Shape::rect_filled(rect, CornerRadius::same(6), fill));
    response
}

fn compact_row_height(ui: &Ui) -> f32 {
    ui.text_style_height(&TextStyle::Body) + 14.0
}

fn paint_background(ui: &Ui, rect: Rect, selected: bool, hovered: bool) {
    let visuals = ui.visuals();
    let fill = if selected {
        visuals.selection.bg_fill
    } else if hovered {
        visuals.faint_bg_color
    } else {
        Color32::TRANSPARENT
    };
    ui.painter().rect_filled(rect, CornerRadius::same(6), fill);
}

/// Text cut to `rows` lines of `width`, ending in "…" if it doesn't fit.
fn elided(painter: &Painter, text: &str, font: FontId, color: Color32, width: f32, rows: usize) -> Arc<Galley> {
    let mut job = LayoutJob::simple(text.to_owned(), font, color, width.max(1.0));
    job.wrap.max_rows = rows;
    job.wrap.overflow_character = Some('…');
    // A single line is cut anywhere; longer text keeps whole words.
    job.wrap.break_anywhere = rows == 1;
    painter.layout_job(job)
}

fn title_color(ui: &Ui, item: &ItemRow, selected: bool) -> Color32 {
    let visuals = ui.visuals();
    if item.unread {
        visuals.strong_text_color()
    } else if selected {
        visuals.text_color()
    } else {
        visuals.weak_text_color()
    }
}

/// A one-line row: unread dot, title, feed, star, date.
fn compact_row(ui: &mut Ui, item: &ItemRow, selected: bool) -> Response {
    let (rect, response) = ui.allocate_exact_size(vec2(ui.available_width(), compact_row_height(ui)), Sense::click());
    paint_background(ui, rect, selected, response.hovered());
    let painter = ui.painter_at(rect);
    let (small, body) = (TextStyle::Small.resolve(ui.style()), TextStyle::Body.resolve(ui.style()));
    let weak = ui.visuals().weak_text_color();
    let y = rect.center().y;
    let date = painter.layout_no_wrap(format_date(item.published, false), small.clone(), weak);
    let mut right = rect.right() - 10.0 - date.size().x;
    painter.galley(pos2(right, y - date.size().y / 2.0), date, weak);
    if item.fav {
        let star = painter.layout_no_wrap("★".into(), small.clone(), ACCENT);
        right -= star.size().x + 8.0;
        painter.galley(pos2(right, y - star.size().y / 2.0), star, ACCENT);
    }
    // The feed name gets a column of its own, so titles all end at the same place.
    let column = (rect.width() * 0.25).min(140.0);
    let feed = elided(&painter, &item.feed_title, small, weak, column, 1);
    right -= 12.0;
    painter.galley(pos2(right - feed.size().x, y - feed.size().y / 2.0), feed, weak);
    right -= column;
    if item.unread {
        painter.circle_filled(pos2(rect.left() + 13.0, y), 3.5, ACCENT);
    }
    let left = rect.left() + 26.0;
    let color = title_color(ui, item, selected);
    let title = elided(&painter, &item_title(&item.title), body, color, right - 12.0 - left, 1);
    painter.galley(pos2(left, y - title.size().y / 2.0), title, color);
    response
}

/// A card: feed and date, title, and (unless compact) the start of the text.
fn item_card(ui: &mut Ui, item: &ItemRow, selected: bool, size: Vec2, compact: bool) -> Response {
    let (rect, response) = ui.allocate_exact_size(size, Sense::click());
    let visuals = ui.visuals();
    let fill = if selected {
        visuals.selection.bg_fill
    } else if response.hovered() {
        visuals.faint_bg_color
    } else {
        visuals.window_fill
    };
    let stroke = Stroke::new(1.0, visuals.widgets.noninteractive.bg_stroke.color);
    ui.painter().rect(rect, CornerRadius::same(10), fill, stroke, StrokeKind::Inside);
    let painter = ui.painter_at(rect);
    let inner = rect.shrink(14.0);
    let small = TextStyle::Small.resolve(ui.style());
    let weak = visuals.weak_text_color();

    // Feed and date, with the unread dot and favorite star.
    let mut left = inner.left();
    let line = ui.text_style_height(&TextStyle::Small);
    if item.unread {
        painter.circle_filled(pos2(left + 3.5, inner.top() + line / 2.0), 3.5, ACCENT);
        left += 14.0;
    }
    let date = painter.layout_no_wrap(format_date(item.published, false), small.clone(), weak);
    let mut right = inner.right() - date.size().x;
    painter.galley(pos2(right, inner.top()), date, weak);
    if item.fav {
        let star = painter.layout_no_wrap("★".into(), small.clone(), ACCENT);
        right -= star.size().x + 6.0;
        painter.galley(pos2(right, inner.top()), star, ACCENT);
    }
    let feed = elided(&painter, &item.feed_title, small.clone(), weak, right - 10.0 - left, 1);
    painter.galley(pos2(left, inner.top()), feed, weak);

    let mut y = inner.top() + line + 8.0;
    let title_font = if compact { TextStyle::Body.resolve(ui.style()) } else { FontId::proportional(17.0) };
    let color = title_color(ui, item, selected);
    let rows = if compact { 2 } else { 3 };
    let title = elided(&painter, &item_title(&item.title), title_font, color, inner.width(), rows);
    let title_height = title.size().y;
    painter.galley(pos2(inner.left(), y), title, color);
    y += title_height + 8.0;
    if !compact && !item.snippet.is_empty() {
        let rows = ((inner.bottom() - y) / line).floor() as usize;
        if rows > 0 {
            let text = elided(&painter, &item.snippet, small, weak, inner.width(), rows);
            painter.galley(pos2(inner.left(), y), text, weak);
        }
    }
    response
}

fn show_block(ui: &mut Ui, block: &Block, article: &str) {
    let text = block.text();
    let size = match block.style {
        Style::Heading(1) => 23.0,
        Style::Heading(2) => 20.0,
        Style::Heading(_) => 18.0,
        _ => 16.0,
    };
    if matches!(block.style, Style::Heading(_)) {
        ui.add_space(6.0);
    }
    let row = |ui: &mut Ui| {
        ui.horizontal_top(|ui| {
            // List items: indented by depth, with the marker in a column of its own.
            if block.depth > 0 {
                ui.add_space(20.0 * f32::from(block.depth - 1));
                let start = ui.cursor().left();
                ui.label(RichText::new(block.marker.as_deref().unwrap_or("")).size(size));
                ui.add_space((22.0 - (ui.cursor().left() - start)).max(0.0));
            }
            ui.vertical(|ui| match block.style {
                Style::Rule => {
                    ui.separator();
                }
                Style::Code => {
                    ui.add(Label::new(RichText::new(&text).monospace()).wrap())
                        .context_menu(|ui| copy_menu(ui, &text, article, None));
                }
                _ => show_spans(ui, &block.spans, size, block.style, |ui, link| copy_menu(ui, &text, article, link)),
            });
        });
    };
    // Quotes and code sit on a background, list markers included.
    let background = match block.style {
        Style::Quote => Some(ui.visuals().faint_bg_color),
        Style::Code => Some(ui.visuals().extreme_bg_color),
        _ => None,
    };
    match background {
        Some(fill) => {
            Frame::new().fill(fill).corner_radius(6).inner_margin(12).show(ui, |ui| {
                ui.set_width(ui.available_width());
                row(ui);
            });
        }
        None => row(ui),
    }
    ui.add_space(if block.depth > 0 { 4.0 } else { 10.0 });
}

/// Styled runs of text flowing as one paragraph; links open in the browser.
/// Headings are strong throughout, quotes in italics. `menu` is shown on a
/// right-click or long press, with the link pressed, if any.
fn show_spans(ui: &mut Ui, spans: &[html::Span], size: f32, style: Style, menu: impl Fn(&mut Ui, Option<&str>)) {
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing.x = 0.0;
        for span in spans {
            let mut text = RichText::new(&span.text).size(size);
            if span.bold || matches!(style, Style::Heading(_)) {
                text = text.strong();
            }
            if span.italic || style == Style::Quote {
                text = text.italics();
            }
            if span.code {
                text = text.code();
            }
            if span.strike {
                text = text.strikethrough();
            }
            let response = match &span.link {
                Some(url) => {
                    let response = ui.link(text).on_hover_text(url);
                    if response.clicked() {
                        ui.ctx().open_url(OpenUrl::new_tab(url));
                    }
                    response
                }
                None => ui.add(Label::new(text).wrap()),
            };
            response.context_menu(|ui| menu(ui, span.link.as_deref()));
        }
    });
}

/// Puts the cursor in a dialog's text field when it opens, asking until it has
/// it (a new dialog's first frame is laid out twice). Only until then: every
/// request also restarts the on-screen keyboard's typing, so asking on every
/// frame made phone keyboards flicker and drop what was typed, and grabbing
/// focus back after Enter would hide that Enter was pressed.
fn focus_on_open(response: &Response, focus: &mut bool) {
    if !*focus {
        return;
    }
    if response.has_focus() {
        *focus = false;
    } else {
        response.request_focus();
    }
}

/// A QR code of `text`, in black on white whatever the theme, as scanners expect.
#[cfg(not(target_os = "android"))]
fn qr_code(ui: &mut Ui, text: &str) {
    use qrcodegen::{QrCode, QrCodeEcc};
    let Ok(qr) = QrCode::encode_text(text, QrCodeEcc::Medium) else {
        ui.label("The link is too long for a QR code: use Copy link.");
        return;
    };
    // Modules of whole pixels, and the blank border scanners need around them.
    const BORDER: i32 = 4;
    let modules = qr.size() + 2 * BORDER;
    let ppp = ui.ctx().pixels_per_point();
    let module = ((240.0_f32.min(ui.available_width()) * ppp / modules as f32).floor() / ppp).max(1.0 / ppp);
    let (rect, _) = ui.allocate_exact_size(Vec2::splat(module * modules as f32), Sense::hover());
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, 0, Color32::WHITE);
    for y in 0..qr.size() {
        for x in 0..qr.size() {
            if qr.get_module(x, y) {
                let min = rect.min + vec2((x + BORDER) as f32, (y + BORDER) as f32) * module;
                painter.rect_filled(Rect::from_min_size(min, Vec2::splat(module)), 0, Color32::BLACK);
            }
        }
    }
}

/// The menu on article text, opened with a right-click (or a long press on a
/// touch screen, where text can't be selected by dragging).
fn copy_menu(ui: &mut Ui, paragraph: &str, article: &str, link: Option<&str>) {
    let selection = ui.ctx().plugin::<SelectedText>().lock().at_menu.clone();
    let mut copy = None;
    if let Some(selection) = selection
        && ui.button("Copy").clicked()
    {
        copy = Some(selection);
    }
    if ui.button("Copy paragraph").clicked() {
        copy = Some(paragraph.to_string());
    }
    if ui.button("Copy article").clicked() {
        copy = Some(article.to_string());
    }
    if let Some(link) = link {
        if ui.button("Copy link address").clicked() {
            copy = Some(link.to_string());
        }
        if ui.button("Open link").clicked() {
            ui.ctx().open_url(OpenUrl::new_tab(link));
        }
    }
    if let Some(text) = copy {
        ui.ctx().copy_text(text);
    }
}

/// Keeps the text selected in an article when its menu is opened with a
/// right-click: egui drops the selection as soon as a menu item is pressed.
/// Registered after egui's own text selection, so it sees what it reported.
#[derive(Default)]
struct SelectedText {
    /// The selection as egui last reported it.
    settled: String,
    /// What was selected when the menu was opened, if anything.
    at_menu: Option<String>,
}

impl egui::Plugin for SelectedText {
    fn debug_name(&self) -> &'static str {
        "SelectedText"
    }

    fn on_end_pass(&mut self, ui: &mut Ui) {
        let ctx = ui.ctx();
        ctx.output(|o| {
            for command in &o.commands {
                if let egui::OutputCommand::TextSelectionSettled(text) = command {
                    self.settled = text.clone();
                }
            }
        });
        if ctx.input(|i| i.pointer.button_pressed(egui::PointerButton::Secondary)) {
            let selected = ctx.plugin::<egui::text_selection::LabelSelectionState>().lock().has_selection();
            self.at_menu = selected.then(|| self.settled.clone()).filter(|text| !text.is_empty());
        }
    }
}

/// Heights of rows last laid out (each with the spacing after it), so rows
/// out of view can be skipped: they're just empty space of that height.
#[derive(Default)]
struct RowHeights {
    width: f32,
    heights: HashMap<String, f32>,
}

impl RowHeights {
    /// Heights are only right for the width they were measured at.
    fn check_width(&mut self, width: f32) {
        if (width - self.width).abs() > 0.5 {
            self.width = width;
            self.heights.clear();
        }
    }

    /// Shows a row with `show` if it's in view (or `always`), otherwise
    /// leaves room for it: its measured height, or `estimate` if it was never
    /// shown. Returns whether it's in view.
    fn row(&mut self, ui: &mut Ui, key: &str, estimate: f32, always: bool, show: impl FnOnce(&mut Ui)) -> bool {
        let top = ui.cursor().top();
        let known = self.heights.get(key).copied();
        let rect = Rect::from_min_size(
            egui::pos2(ui.cursor().left(), top),
            vec2(ui.available_width(), known.unwrap_or(estimate)),
        );
        let visible = ui.is_rect_visible(rect);
        if !visible && !always {
            ui.add_space(rect.height());
            return false;
        }
        show(ui);
        self.heights.insert(key.to_string(), ui.cursor().top() - top);
        visible
    }
}

/// An item's title as shown, with a stand-in for items that have none.
fn item_title(title: &str) -> String {
    let title = html::decode_entities(title);
    if title.trim().is_empty() { "Untitled Entry".into() } else { title }
}

fn format_date(seconds: i64, long: bool) -> String {
    use chrono::{Local, TimeZone};
    let Some(date) = Local.timestamp_opt(seconds, 0).single() else { return String::new() };
    let now = Local::now();
    if long {
        date.format("%A, %B %-d, %Y at %H:%M").to_string()
    } else if date.date_naive() == now.date_naive() {
        date.format("%H:%M").to_string()
    } else if date.format("%Y").to_string() == now.format("%Y").to_string() {
        date.format("%b %-d").to_string()
    } else {
        date.format("%b %-d, %Y").to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Db;

    #[test]
    fn blank_titles_get_a_stand_in() {
        assert_eq!(item_title(""), "Untitled Entry");
        assert_eq!(item_title("  \n "), "Untitled Entry");
        assert_eq!(item_title("&nbsp;"), "Untitled Entry");
        assert_eq!(item_title("Fish &amp; chips"), "Fish & chips");
    }

    #[test]
    fn partly_local_groups_mark_each_local_feed() {
        let db = Db::open_memory().unwrap();
        let (mine, theirs) = ("https://a.example/feed", "https://b.example/feed");
        for (url, category) in [(mine, "Mine"), (theirs, "News")] {
            db.set_subscribed(url, true).unwrap();
            db.set_category(url, category).unwrap();
        }
        db.set_category_synced("Mine", false).unwrap();
        let local = db.local_categories().unwrap();
        assert!(group_is_local("Mine", db.feeds().unwrap().iter().filter(|f| f.category == "Mine"), &local));
        // Renaming the Local Only group into a synced one: the group isn't all local any more.
        db.rename_category("Mine", "News").unwrap();
        let (feeds, local) = (db.feeds().unwrap(), db.local_categories().unwrap());
        assert!(!group_is_local("News", feeds.iter(), &local));
        let flags: Vec<(&str, bool)> = feeds.iter().map(|f| (f.url.as_str(), f.local)).collect();
        assert_eq!(flags, [(mine, true), (theirs, false)]);
    }
}
