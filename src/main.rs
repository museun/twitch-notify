#![cfg_attr(debug_assertions, allow(dead_code, unused_variables,))]
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::{
    collections::HashSet,
    sync::Arc,
    time::{Duration, Instant},
};

use eframe::{epaint::ahash::HashMap, NativeOptions};
use egui::{
    collapsing_header::CollapsingState, vec2, Align, Button, CentralPanel, Color32, Frame, Grid,
    Key, Label, Layout, RichText, ScrollArea, Sense, TextEdit, TextStyle, TopBottomPanel, Vec2,
};
use tokio::sync::Mutex;
use tokio_stream::StreamExt;

mod runtime {
    use std::future::Future;

    use flume::Receiver;
    use once_cell::sync::OnceCell;
    use tokio::runtime::{Builder, Handle as TokioHandle};

    pub struct Handle {
        thread: std::thread::JoinHandle<()>,
    }

    impl Handle {
        pub fn wait_for_shutdown(self) {
            let _ = self.thread.join();
        }
    }

    pub fn start() -> (Handle, impl FnOnce() + Send + Sync + 'static) {
        let rt = Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("initialize tokio runtime");

        let handle = rt.handle().clone();

        let (tx, rx) = tokio::sync::oneshot::channel();

        let thread = std::thread::spawn(move || {
            rt.block_on(async move {
                tokio::select! {
                    _ = rx => {}
                    _ = std::future::pending::<()>() => {}
                }
            });
        });

        TOKIO_HANDLE.get_or_init(|| handle);

        (Handle { thread }, move || {
            let _ = tx.send(());
        })
    }

    pub fn enter_context(f: impl FnOnce()) {
        let _g = TOKIO_HANDLE.get().expect("initialization").enter();
        f()
    }

    #[must_use]
    pub fn spawn<T>(fut: impl Future<Output = T> + Send + Sync + 'static) -> Receiver<T>
    where
        T: Send + Sync + 'static,
    {
        let (tx, rx) = flume::bounded(1);
        enter_context(|| {
            tokio::task::spawn(async move {
                let res = fut.await;
                let _ = tx.send(res);
            });
        });
        rx
    }

    #[must_use]
    pub fn spawn_blocking<T>(func: impl FnOnce() -> T + Send + Sync + 'static) -> Receiver<T>
    where
        T: Send + Sync + 'static,
    {
        let (tx, rx) = flume::bounded(1);
        enter_context(|| {
            tokio::task::spawn_blocking(move || {
                let res = func();
                let _ = tx.send(res);
            });
        });
        rx
    }

    static TOKIO_HANDLE: OnceCell<TokioHandle> = OnceCell::new();
}

#[derive(
    Copy, Clone, Debug, PartialEq, PartialOrd, Eq, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
pub enum Either<L, R> {
    Left(L),
    Right(R),
}

mod helix {
    use std::sync::Arc;

    use tokio::sync::Mutex;

    use crate::{Channel, Either};

    pub const USER_AGENT: &str = concat!(env!("CARGO_PKG_NAME"), "/", env!("CARGO_PKG_VERSION"));

    #[derive(serde::Deserialize)]
    pub struct OAuth {
        access_token: String,
        refresh_token: Option<String>,
        expires_in: u64,

        token_type: String,

        #[serde(default)]
        client_id: String,

        #[serde(skip)]
        bearer_token: String,
    }

    impl OAuth {
        pub async fn fetch(client_id: &str, client_secret: &str) -> anyhow::Result<Self> {
            anyhow::ensure!(!client_id.is_empty(), "twitch client id cannot be empty");
            anyhow::ensure!(
                !client_secret.is_empty(),
                "twitch client secret cannot be empty"
            );

            #[derive(serde::Serialize)]
            struct Request<'a> {
                client_id: &'a str,
                client_secret: &'a str,
                grant_type: &'a str,
            }

            let mut this: Self = reqwest::Client::new()
                .post("https://id.twitch.tv/oauth2/token")
                .header("user-agent", USER_AGENT)
                .query(&Request {
                    client_id,
                    client_secret,
                    grant_type: "client_credentials",
                })
                .send()
                .await?
                .json()
                .await?;

            this.client_id = client_id.to_string();
            this.bearer_token = format!("Bearer {}", this.access_token);

            Ok(this)
        }

        async fn refresh(&mut self) -> anyhow::Result<()> {
            eprintln!("refresh token isn't implemented yet");
            Ok(())
        }
    }

    #[derive(Clone)]
    pub struct Client {
        client: reqwest::Client,
        recv: flume::Receiver<OAuth>,
        oauth: Arc<Mutex<Option<OAuth>>>,
    }

    impl Client {
        pub fn new(recv: flume::Receiver<OAuth>) -> Self {
            Self {
                client: reqwest::Client::new(),
                recv,
                oauth: Default::default(),
            }
        }

        pub async fn get_users<'a>(
            &self,
            iter: impl IntoIterator<Item = Either<&'a str, &'a str>> + Send + Sync,
        ) -> anyhow::Result<Vec<User>> {
            self.fetch(
                "users",
                Self::id_login_to_query(iter, |login| match login {
                    Either::Left(left) => ("id", left),
                    Either::Right(right) => ("login", Channel::strip_prefix(right)),
                }),
            )
            .await
        }

        pub async fn get_streams<'a>(
            &self,
            iter: impl IntoIterator<Item = Either<&'a str, &'a str>> + Send + Sync,
        ) -> anyhow::Result<Vec<Stream>> {
            self.fetch(
                "streams",
                Self::id_login_to_query(iter, |login| match login {
                    Either::Left(left) => ("user_id", left),
                    Either::Right(right) => ("user_login", Channel::strip_prefix(right)),
                }),
            )
            .await
        }

        pub async fn fetch<T>(
            &self,
            ep: &str,
            query: impl serde::Serialize + Send + Sync,
        ) -> anyhow::Result<Vec<T>>
        where
            for<'de> T: serde::Deserialize<'de>,
        {
            let mut lock = self.oauth.lock().await;
            if lock.is_none() {
                let oauth = self.recv.recv_async().await.expect("oauth initialization");
                lock.replace(oauth);
            }

            let oauth = lock.as_ref().unwrap();

            let req = self
                .client
                .get(&format!("https://api.twitch.tv/helix/{ep}"))
                .header("client-id", &oauth.client_id)
                .header("authorization", &oauth.bearer_token)
                .query(&query);
            let req = req.build()?;

            eprintln!("getting: {}", req.url());

            #[derive(serde::Deserialize)]
            struct Response<T> {
                data: Vec<T>,
            }

            let resp: Response<T> = self.client.execute(req).await?.json().await?;
            Ok(resp.data)
        }

        fn id_login_to_query<'a>(
            iter: impl IntoIterator<Item = Either<&'a str, &'a str>>,
            map: fn(Either<&'a str, &'a str>) -> (&'static str, &'a str),
        ) -> impl serde::Serialize + 'a {
            iter.into_iter().map(map).collect::<Vec<_>>()
        }
    }

    #[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
    pub struct Stream {
        pub game_id: String,
        pub game_name: String,
        pub id: String,
        pub is_mature: bool,
        pub language: String,
        pub started_at: String,
        pub tag_ids: Vec<String>,
        pub thumbnail_url: String,
        pub title: String,
        #[serde(rename = "type")]
        pub type_: String,
        pub user_id: String,
        pub user_login: String,
        pub user_name: String,
        pub viewer_count: i64,
    }

    #[derive(Debug, serde::Serialize, serde::Deserialize)]
    pub struct User {
        pub login: String,
        pub display_name: String,
        pub id: String,
        pub created_at: String,
        pub description: String,
        pub profile_image_url: String,
    }
}

#[derive(Default, serde::Serialize, serde::Deserialize)]
struct Users {
    list: Vec<helix::User>,
}

impl Users {
    fn append(&mut self, user: helix::User) {
        if let Some(old) = self.list.iter_mut().find(|c| c.id == user.id) {
            *old = user;
            return;
        }
        self.list.push(user);
    }
}

#[derive(Default, Debug, PartialEq)]
enum Status {
    #[default]
    Offline,
    Online(Box<crate::helix::Stream>),
}

impl PartialOrd for Status {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        match (
            matches!(self, Self::Online { .. }),
            matches!(other, Self::Online { .. }),
        ) {
            (true, true) | (false, false) => Some(std::cmp::Ordering::Equal),
            (true, false) => Some(std::cmp::Ordering::Less),
            (false, true) => Some(std::cmp::Ordering::Greater),
        }
    }
}

#[derive(Default)]
struct StatusMap {
    map: HashMap<String, Status>,
}

impl StatusMap {
    fn get(&self, login: &str) -> &Status {
        self.map.get(login).unwrap()
    }
}

#[derive(Default)]
struct State {
    users: Users,
    status: StatusMap,
    shown: HashMap<String, bool>,
}

impl State {
    fn remove(&mut self, index: usize, images: &mut Images) {
        if index >= self.users.list.len() {
            return;
        }
        let user = self.users.list.remove(index);
        self.status.map.remove(&user.login);

        images.static_images.remove(&user.profile_image_url);

        images.preview_images.remove(&format!(
            "https://static-cdn.jtvnw.net/previews-ttv/live_user_{login}-{w}x{h}.jpg",
            login = user.login,
            w = OnlineEntry::PREVIEW_IMAGE_SIZE.x,
            h = OnlineEntry::PREVIEW_IMAGE_SIZE.y,
        ));
    }

    fn serialize(&self) -> String {
        #[derive(serde::Serialize)]
        struct PersistState<'a> {
            following: &'a Users,
        }

        serde_json::to_string(&PersistState {
            following: &self.users,
        })
        .expect("valid json")
    }

    fn load(input: &str) -> Self {
        #[derive(serde::Deserialize)]
        struct PersistState {
            following: Users,
        }

        let mut this = serde_json::from_str(input)
            .map(|p: PersistState| Self {
                users: p.following,
                ..Default::default()
            })
            .unwrap_or_else(|err| {
                eprintln!("defaulting state. cannot load: {err}");
                Self::default()
            });

        for (k, v) in this
            .users
            .list
            .iter()
            .map(|u| (u.login.clone(), Status::default()))
        {
            this.status.map.insert(k.clone(), v);
            this.shown.insert(k, false);
        }

        this
    }
}

#[derive(Copy, Clone, Default)]
enum ListAction {
    #[default]
    Nothing,
    ScrollToBottom,
    Highlight(usize),
}

#[derive(Default)]
struct Input {
    buffer: String,
    validity: Validity,
    result: Option<flume::Receiver<Either<helix::User, String>>>,
}

impl Input {
    fn display(
        &mut self,
        client: &helix::Client,
        list_action: &mut ListAction,
        status: &StatusChecker,
        state: &mut State,
        ui: &mut egui::Ui,
    ) -> egui::Response {
        let mut resp = ui.add_sized(
            ui.available_size(),
            TextEdit::singleline(&mut self.buffer)
                .text_color(
                    matches!(self.validity, Validity::Empty | Validity::Valid)
                        .then_some(ui.style().visuals.text_color())
                        .unwrap_or(ui.style().visuals.error_fg_color),
                )
                .hint_text("enter a twitch channel name")
                .frame(false)
                .lock_focus(true),
        );

        if let Some(recv) = self.result.as_ref() {
            if let Ok(result) = recv.try_recv() {
                match result {
                    Either::Left(user) => {
                        status.queue(user.login.clone());
                        state
                            .status
                            .map
                            .insert(user.login.clone(), Status::default());
                        state.shown.insert(user.login.clone(), false);
                        state.users.append(user);
                        *list_action = ListAction::ScrollToBottom;
                    }
                    Either::Right(invalid) => {
                        let _ = std::mem::replace(&mut self.buffer, invalid);
                        self.validity = Validity::Unknown;
                    }
                }
                self.result.take();
            }
        }

        if resp.changed() {
            self.validity = self.check_validity(state);
        }

        match self.validity {
            Validity::Empty => {}
            Validity::Valid => {
                if resp.lost_focus() && ui.ctx().input().key_pressed(Key::Enter) {
                    let result = self.try_add_channel(client.clone(), state, ui);
                    self.result.replace(result);
                    *list_action = ListAction::ScrollToBottom;
                }
            }
            Validity::Duplicate(index) => {
                *list_action = ListAction::Highlight(index);
            }
            Validity::ContainsSpaces => {
                resp = resp.on_hover_ui(|ui| {
                    ui.colored_label(
                        ui.style().visuals.warn_fg_color,
                        "The input cannot contain spaces",
                    );
                })
            }
            Validity::Unknown => {
                resp = resp.on_hover_ui(|ui| {
                    ui.colored_label(ui.style().visuals.warn_fg_color, "Unknown Twitch channel");
                });
            }
        }

        ui.ctx().request_repaint();
        resp
    }

    fn try_add_channel(
        &mut self,
        client: helix::Client,
        state: &mut State,
        ui: &mut egui::Ui,
    ) -> flume::Receiver<Either<helix::User, String>> {
        let s = std::mem::take(&mut self.buffer);
        crate::runtime::spawn(async move {
            match client.get_users([Either::Right(&*s)]).await {
                Ok(users) if users.is_empty() => Either::Right(s),
                Err(_) => Either::Right(s),
                Ok(mut users) => Either::Left(users.remove(0)),
            }
        })
    }

    fn check_validity(&self, state: &State) -> Validity {
        if self.buffer.is_empty() {
            return Validity::Empty;
        }

        if self.buffer.chars().any(char::is_whitespace) {
            return Validity::ContainsSpaces;
        }

        if let Some(index) = state
            .users
            .list
            .iter()
            .position(|c| Channel::is_eq(&self.buffer, &c.login))
        {
            return Validity::Duplicate(index);
        }

        Validity::Valid
    }
}

struct Channel;

impl Channel {
    fn is_eq(left: &str, right: &str) -> bool {
        Self::strip_prefix(left).eq_ignore_ascii_case(Self::strip_prefix(right))
    }

    fn strip_prefix(input: &str) -> &str {
        input.strip_prefix('#').unwrap_or(input)
    }
}

#[derive(Debug, Copy, Clone, Default)]
enum Validity {
    #[default]
    Empty,
    Valid,
    Duplicate(usize),
    ContainsSpaces,
    Unknown,
}

trait RequestRepaint: Send + Sync + 'static + Clone {
    fn request_repaint(&self) {}
}

impl RequestRepaint for egui::Context {
    fn request_repaint(&self) {
        Self::request_repaint(self)
    }
}

struct PreviewImage {
    last: Instant,
    image: egui_extras::RetainedImage,
}

impl PreviewImage {
    const PREVIEW_INTERVAL: Duration = Duration::from_secs(60);
    fn should_update(&self) -> bool {
        self.last.elapsed() >= Self::PREVIEW_INTERVAL
    }
}

#[derive(Default)]
struct Images {
    static_images: HashMap<String, egui_extras::RetainedImage>,
    preview_images: HashMap<String, PreviewImage>,
}

impl Images {
    fn get_static(&self, url: &str, fetcher: &ImageFetcher) -> Option<&egui_extras::RetainedImage> {
        match self.static_images.get(url) {
            Some(img) => Some(img),
            _ => {
                fetcher.fetch(ImageKind::Static, url);
                None
            }
        }
    }

    fn get_preview(
        &self,
        url: &str,
        fetcher: &ImageFetcher,
    ) -> Option<&egui_extras::RetainedImage> {
        match self.preview_images.get(url) {
            Some(img) if img.should_update() => {
                fetcher.fetch(ImageKind::Refresh, url);
                Some(&img.image)
            }
            Some(img) => Some(&img.image),
            None => {
                fetcher.fetch(ImageKind::Preview, url);
                None
            }
        }
    }
}

impl Images {
    fn add_static(&mut self, key: impl Into<String>, img: egui_extras::RetainedImage) {
        self.static_images.insert(key.into(), img);
    }

    fn add_preview(&mut self, key: impl Into<String>, img: egui_extras::RetainedImage) {
        let key = key.into();
        self.preview_images.insert(
            key,
            PreviewImage {
                last: Instant::now(),
                image: img,
            },
        );
    }
}

struct ImageFetcher {
    send: flume::Sender<(String, ImageKind)>,
    ready: flume::Receiver<(String, ImageKind, Vec<u8>)>,
}

impl ImageFetcher {
    fn spawn(repaint: impl RequestRepaint) -> Self {
        let (tx, rx) = flume::unbounded::<(String, ImageKind)>();
        let (out_tx, out_rx) = flume::unbounded::<(String, ImageKind, Vec<u8>)>();

        let _ = crate::runtime::spawn(async move {
            async fn fetch(client: reqwest::Client, url: &str) -> anyhow::Result<Vec<u8>> {
                eprintln!("getting image: {url}");
                let data = client.get(url).send().await?.bytes().await?;
                Ok(data.to_vec())
            }

            let client = reqwest::Client::new();
            let mut stream = rx.into_stream();

            let seen = Arc::new(Mutex::new(HashSet::new()));

            while let Some((url, kind)) = stream.next().await {
                let resp = out_tx.clone();
                let repaint = repaint.clone();
                let client = client.clone();

                if !seen.lock().await.insert(url.clone()) {
                    continue;
                }

                let seen = seen.clone();
                let _ = crate::runtime::spawn(async move {
                    match fetch(client, &url).await {
                        Ok(data) => {
                            if matches!(kind, ImageKind::Preview | ImageKind::Refresh) {
                                seen.lock().await.remove(&url);
                            }
                            let _ = resp.send((url, kind, data));
                            repaint.request_repaint();
                        }
                        Err(err) => {
                            eprintln!("cannot fetch ({kind:?}): {url}: {err}")
                        }
                    }
                });
            }
        });

        Self {
            send: tx,
            ready: out_rx,
        }
    }

    fn poll(&mut self, loader: &ImageLoader) {
        for (url, kind, data) in self.ready.try_iter() {
            loader.load(url, kind, data)
        }
    }

    fn fetch(&self, kind: ImageKind, url: impl Into<String>) {
        let url = url.into();
        if url.is_empty() {
            return;
        }

        let _ = self.send.send((url, kind));
    }
}

#[derive(Debug)]
enum ImageKind {
    Static,
    Preview,
    Refresh,
}

struct ImageLoader {
    send: flume::Sender<(String, ImageKind, Vec<u8>)>,
    ready: flume::Receiver<(String, ImageKind, egui_extras::RetainedImage)>,
}

impl ImageLoader {
    fn spawn(repaint: impl RequestRepaint) -> Self {
        let (tx, rx) = flume::unbounded::<(String, ImageKind, Vec<u8>)>();
        let (out_tx, out_rx) =
            flume::unbounded::<(String, ImageKind, egui_extras::RetainedImage)>();

        let _ = crate::runtime::spawn(async move {
            let mut stream = rx.into_stream();
            while let Some((url, kind, data)) = stream.next().await {
                let resp = out_tx.clone();
                let repaint = repaint.clone();
                let _ = crate::runtime::spawn_blocking(move || {
                    if let Err(err) = Self::load_image(url, kind, data, resp) {
                        eprintln!("image loader: {err}");
                        return;
                    }
                    repaint.request_repaint();
                });
            }
        });

        Self {
            send: tx,
            ready: out_rx,
        }
    }

    fn poll(&mut self, images: &mut Images) {
        for (url, kind, img) in self.ready.try_iter() {
            match kind {
                ImageKind::Static => images.add_static(url, img),
                ImageKind::Preview | ImageKind::Refresh => images.add_preview(url, img),
            }
        }
    }

    fn load(&self, url: impl Into<String>, kind: ImageKind, data: impl Into<Vec<u8>>) {
        let _ = self.send.send((url.into(), kind, data.into()));
    }

    fn load_image(
        url: String,
        kind: ImageKind,
        data: Vec<u8>,
        resp: flume::Sender<(String, ImageKind, egui_extras::RetainedImage)>,
    ) -> anyhow::Result<()> {
        assert!(!data.is_empty(), "data cannot be empty");

        let img = match image::guess_format(&data[..data.len().min(128)])
            .map_err(|err| anyhow::anyhow!("unknown format for {url}: {err}"))?
        {
            image::ImageFormat::Png | image::ImageFormat::Jpeg => {
                Self::load_retained_image(&url, &data)?
            }
            fmt => {
                anyhow::bail!("unsupported format for {url}: {fmt:?}")
            }
        };

        let _ = resp.send((url, kind, img));
        Ok(())
    }

    fn load_retained_image(url: &str, data: &[u8]) -> anyhow::Result<egui_extras::RetainedImage> {
        egui_extras::RetainedImage::from_image_bytes(url, data)
            .map_err(|err| anyhow::anyhow!("cannot load '{url}': {err}"))
    }
}

struct StatusChecker {
    queue: flume::Sender<String>, // NOTE: this is `login`
    output: flume::Receiver<(String, Status)>,
}

impl StatusChecker {
    const DELAY: Duration = Duration::from_secs(30);

    fn spawn(client: helix::Client, repaint: impl RequestRepaint) -> Self {
        let (tx, rx) = flume::unbounded::<String>();
        let (out_tx, out_rx) = flume::unbounded::<(String, Status)>();

        let _ = crate::runtime::spawn(async move {
            let mut stream = rx.into_stream();
            let mut seen = HashSet::<String>::default();

            while let Some(login) = stream.next().await {
                if !seen.insert(login.clone()) {
                    continue;
                }

                let client = client.clone();
                let repaint = repaint.clone();
                let out = out_tx.clone();

                // TODO get the initial status

                // TODO this should coalesce the statuses into a single fetch (sans 100-limit chunking)
                let _ = crate::runtime::spawn(async move {
                    let stream = tokio::time::interval(Self::DELAY);
                    let mut stream = tokio_stream::wrappers::IntervalStream::new(stream);

                    while let Some(..) = stream.next().await {
                        let status = match client
                            .get_streams([Either::Right(&*login)])
                            .await
                            .ok()
                            .and_then(|mut c| (!c.is_empty()).then(|| c.remove(0)))
                        {
                            Some(mut stream) => Status::Online({
                                stream.thumbnail_url = format!(
                                    "https://static-cdn.jtvnw.net/previews-ttv/live_user_{login}-{w}x{h}.jpg",
                                    login = stream.user_login,
                                    w = OnlineEntry::PREVIEW_IMAGE_SIZE.x,
                                    h = OnlineEntry::PREVIEW_IMAGE_SIZE.y,
                                );
                                Box::new(stream)
                            }),
                            _ => Status::Offline,
                        };

                        eprintln!("{login} -> {status:?}");

                        let _ = out.send((login.clone(), status));
                        repaint.request_repaint();
                    }

                    eprintln!("end of status check for: {login}");
                });
            }
        });

        Self {
            queue: tx,
            output: out_rx,
        }
    }

    fn poll(&self, map: &mut StatusMap) {
        for (login, status) in self.output.try_iter() {
            map.map.insert(login.clone(), status);
        }
    }

    // TODO this should take a priority ('now' or 'later')
    // TODO is this user-login or user-id?
    fn queue(&self, id: impl Into<String>) {
        let _ = self.queue.send(id.into());
    }
}

struct App {
    state: State,
    input: Input,
    images: Images,
    loader: ImageLoader,
    fetcher: ImageFetcher,
    status: StatusChecker,
    client: helix::Client,
    shutdown: Option<Box<dyn FnOnce() + Send + Sync + 'static>>,
}

impl App {
    const SAVE_KEY: &'static str = concat!(env!("CARGO_PKG_NAME"), "_", "state");

    fn new(
        state: State,
        client: helix::Client,
        repaint: impl RequestRepaint,
        shutdown: impl FnOnce() + Send + Sync + 'static,
    ) -> Self {
        let status = StatusChecker::spawn(client.clone(), repaint.clone());

        for user in &state.users.list {
            status.queue(&user.login)
        }

        Self {
            state,
            images: Images::default(),
            loader: ImageLoader::spawn(repaint.clone()),
            fetcher: ImageFetcher::spawn(repaint),
            status,
            input: Input::default(),
            client,
            shutdown: Some(Box::new(shutdown)),
        }
    }

    fn poll(&mut self) {
        self.fetcher.poll(&self.loader);
        self.loader.poll(&mut self.images);
        self.status.poll(&mut self.state.status)
    }
}

fn format_seconds(mut secs: i64) -> String {
    const TABLE: [(&str, i64); 4] = [
        ("days", 24 * 60 * 60),
        ("hours", 60 * 60),
        ("minutes", 60),
        ("seconds", 1),
    ];

    fn pluralize(s: &str, n: i64) -> String {
        format!("{n} {}", if n > 1 { s } else { &s[..s.len() - 1] })
    }

    let mut time = vec![];
    for (name, d) in &TABLE {
        let div = secs / d;
        if div > 0 {
            time.push(pluralize(name, div));
            secs -= d * div
        }
    }

    let len = time.len();
    if len > 1 {
        if len > 2 {
            for segment in time.iter_mut().take(len - 2) {
                segment.push(',')
            }
        }
        time.insert(len - 1, "and".into());
    }

    time.into_iter().fold(String::new(), |mut a, c| {
        if !a.is_empty() {
            a.push(' ');
        }
        a.push_str(&c);
        a
    })
}

fn hover_button(
    label: impl Into<String>,
    color: impl Into<egui::Color32>,
    id: egui::Id,
    ui: &mut egui::Ui,
) -> bool {
    let mut label = RichText::new(label);
    if *ui.memory().data.get_temp_mut_or_insert_with(id, || false) {
        label = label.color(color);
    }
    let resp = ui.add(Button::new(label).small());

    let hovered = ui.rect_contains_pointer(resp.rect);
    ui.memory().data.insert_temp(id, hovered);

    resp.clicked()
}

const REMOVE_BUTTON_LABEL: &str = "✖";
const INFO_BUTTON_LABEL: &str = "ℹ";

struct OnlineEntry<'a> {
    entry: Entry<'a>,
    stream: &'a helix::Stream,
}

impl<'a> OnlineEntry<'a> {
    const PREVIEW_IMAGE_SIZE: Vec2 = vec2(250.0, 141.0);

    fn display(mut self, images: &Images, fetcher: &ImageFetcher, ui: &mut egui::Ui) {
        let id = ui.make_persistent_id(&self.entry.user.id);

        ui.push_id(id.with("header"), |ui| {
            let mut state = CollapsingState::load_with_default_open(ui.ctx(), id, false);

            ui.horizontal(|ui| {
                let mut resp = self
                    .entry
                    .display_picture_name(ui.style().visuals.text_color(), ui);

                if ui.ctx().input().modifiers.shift_only() {
                    if let Some(img) = images.get_preview(&self.stream.thumbnail_url, fetcher) {
                        resp = resp.on_hover_ui_at_pointer(|ui| {
                            img.show_size(ui, Self::PREVIEW_IMAGE_SIZE);
                        })
                    }
                }

                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    ui.horizontal(|ui| {
                        let id = egui::Id::new(&self.entry.user.id);

                        if hover_button(
                            REMOVE_BUTTON_LABEL,
                            ui.style().visuals.error_fg_color,
                            id.with("status"),
                            ui,
                        ) {
                            self.entry.to_remove.replace(self.entry.index);
                        }

                        let mut label = RichText::new(INFO_BUTTON_LABEL);
                        if *ui
                            .memory()
                            .data
                            .get_temp_mut_or_insert_with(id.with("info_button"), || false)
                        {
                            label = label.color(ui.visuals().hyperlink_color);
                        }
                        let resp = ui.toggle_value(self.entry.shown, label);

                        let hovered = resp.hovered();
                        ui.memory()
                            .data
                            .insert_temp(id.with("info_button"), hovered);

                        if resp.changed() {
                            state.toggle(ui);
                        }
                    });
                });

                self.entry.handle_double_click(resp, ui);
            });

            ui.vertical(|ui| {
                state.show_body_unindented(ui, |ui| {
                    ui.indent(id.with("left-pad"), |ui| {
                        Grid::new(id.with("grid")).num_columns(2).show(ui, |ui| {
                            self.display_info(ui);
                        });
                    });
                });
            });
        });
    }

    fn display_info(&self, ui: &mut egui::Ui) {
        const LOCAL_TIMESTAMP: &[time::format_description::FormatItem<'static>] = time::macros::format_description!(
            "[year]-[month]-[day] [hour]:[minute]:[second] [offset_hour][offset_minute]"
        );

        // 2022-10-11T02:20:39Z
        const TWITCH_TIMESTAMP: &[time::format_description::FormatItem<'static>] = time::macros::format_description!(
            "[year]-[month]-[day]T[hour]:[minute]:[second] [offset_hour]"
        );

        ui.monospace("Title:");
        ui.add(Label::new(RichText::new(&self.stream.title)).wrap(true));
        ui.end_row();

        ui.monospace("Game:");
        ui.add(Label::new(RichText::new(&self.stream.game_name)).wrap(true))
            .on_hover_text_at_pointer(format!("game id: {}", self.stream.game_id));
        ui.end_row();

        let dt = time::OffsetDateTime::parse(
            &self.stream.started_at.replace('Z', " +00"),
            TWITCH_TIMESTAMP,
        )
        .expect("valid time")
        .to_offset(time::UtcOffset::current_local_offset().expect("system should have a locale"));

        ui.monospace("Started at:");
        ui.label(&dt.format(&LOCAL_TIMESTAMP).expect("valid time"))
            .on_hover_text_at_pointer({
                let uptime =
                    time::OffsetDateTime::now_local().expect("system should have a clock") - dt;
                format!("{} ago", format_seconds(uptime.whole_seconds()))
            });
        ui.end_row();

        ui.monospace("Viewers:");
        ui.label(format!(
            "{viewers} viewer{plural}",
            viewers = self.stream.viewer_count,
            plural = (self.stream.viewer_count > 1)
                .then_some('s')
                .unwrap_or_default()
        ));
        ui.end_row();

        ui.monospace("Mature?");
        ui.label(if self.stream.is_mature { "Yes" } else { "No" });
        ui.end_row();
    }
}

struct OfflineEntry<'a> {
    entry: Entry<'a>,
}

impl<'a> OfflineEntry<'a> {
    fn display(mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            let resp = self
                .entry
                .display_picture_name(ui.style().visuals.weak_text_color(), ui);

            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                ui.horizontal(|ui| {
                    let id = egui::Id::new(&self.entry.user.id).with("status");

                    if hover_button(
                        REMOVE_BUTTON_LABEL,
                        ui.style().visuals.error_fg_color,
                        id,
                        ui,
                    ) {
                        self.entry.to_remove.replace(self.entry.index);
                    }
                });
            });

            self.entry.handle_double_click(resp, ui);
        });
    }
}

struct Entry<'a> {
    images: &'a Images,
    fetcher: &'a ImageFetcher,

    to_remove: &'a mut Option<usize>,
    action: ListAction,

    index: usize,
    user: &'a helix::User,

    shown: &'a mut bool,
}

impl<'a> Entry<'a> {
    const IMAGE_SIZE: Vec2 = vec2(16.0, 16.0);

    const fn is_highlighted(&self) -> bool {
        matches!(self.action, ListAction::Highlight(d) if d == self.index)
    }

    fn display_picture_name(
        &mut self,
        color: impl Into<Color32>,
        ui: &mut egui::Ui,
    ) -> egui::Response {
        let resp = ui
            .scope(|ui| {
                let width = ui
                    .fonts()
                    .glyph_width(&TextStyle::Body.resolve(ui.style()), ' ');
                ui.spacing_mut().item_spacing.x = width;

                ui.horizontal(|ui| {
                    if let Some(img) = self
                        .images
                        .get_static(&self.user.profile_image_url, self.fetcher)
                    {
                        img.show_size(ui, Self::IMAGE_SIZE);
                    }

                    ui.colored_label(
                        self.is_highlighted()
                            .then_some(ui.style().visuals.warn_fg_color)
                            .unwrap_or_else(|| color.into()),
                        &self.user.display_name,
                    )
                })
            })
            .inner
            .inner;

        if ui.ctx().input().modifiers.command_only() {
            return resp.on_hover_text_at_pointer(format!("user id: {}", &self.user.id));
        }

        resp
    }

    fn handle_double_click(&self, resp: egui::Response, ui: &mut egui::Ui) {
        if ui
            .interact(resp.rect, resp.id, Sense::click())
            .double_clicked()
        {
            let url = format!("https://twitch.tv/{}", self.user.login);
            ui.ctx().output().open_url(url)
        }

        if self.is_highlighted() {
            resp.on_hover_text_at_pointer("This channel has already been added")
                .scroll_to_me(None);
        }
    }
}

impl eframe::App for App {
    fn save(&mut self, storage: &mut dyn eframe::Storage) {
        storage.set_string(Self::SAVE_KEY, self.state.serialize());
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        if let Some(signal) = self.shutdown.take() {
            (signal)()
        }
    }

    fn persist_egui_memory(&self) -> bool {
        false
    }

    fn update(&mut self, ctx: &egui::Context, frame: &mut eframe::Frame) {
        const IMAGE_SIZE: f32 = 16.0;

        self.poll();

        if ctx.input().key_pressed(Key::F12) {
            ctx.set_debug_on_hover(!ctx.debug_on_hover())
        }

        let mut action = ListAction::default();
        let resp = TopBottomPanel::bottom("input")
            .resizable(false)
            .frame(Frame::none().fill(ctx.style().visuals.faint_bg_color))
            .show(ctx, |ui| {
                self.input
                    .display(&self.client, &mut action, &self.status, &mut self.state, ui)
            })
            .inner;

        let mut remove = None;

        CentralPanel::default().show(ctx, |ui| {
            ScrollArea::vertical()
                .auto_shrink([false, true])
                .stick_to_bottom(!matches!(action, ListAction::Highlight(..)))
                .show(ui, |ui| {
                    macro_rules! entry {
                        ($index:expr, $user:expr) => {
                            Entry {
                                images: &self.images,
                                fetcher: &self.fetcher,
                                to_remove: &mut remove,
                                action,
                                index: $index,
                                user: $user,
                                shown: self
                                    .state
                                    .shown
                                    .get_mut(&$user.login)
                                    .expect("synchronization of map"),
                            }
                        };
                    }

                    let mut list = self
                        .state
                        .users
                        .list
                        .iter()
                        .map(|user| (user, self.state.status.get(&user.login)))
                        .enumerate()
                        .collect::<Vec<_>>();

                    list.sort_unstable_by(|(_, (_, l)), (_, (_, r))| {
                        l.partial_cmp(r).expect("ordering between statuses")
                    });

                    let pp = list.partition_point(|(_, (_, status))| {
                        matches!(status, Status::Online { .. })
                    });

                    let (online, offline) = list.split_at(pp);

                    for (index, (user, stream)) in
                        online.iter().map(|(index, (user, status))| match status {
                            Status::Online(stream) => (*index, (user, stream)),
                            _ => unreachable!(),
                        })
                    {
                        OnlineEntry {
                            entry: entry!(index, user),
                            stream,
                        }
                        .display(&self.images, &self.fetcher, ui);
                    }

                    if !offline.is_empty() {
                        ui.separator();
                    }

                    for (index, (user, status)) in offline.iter() {
                        OfflineEntry {
                            entry: entry!(*index, user),
                        }
                        .display(ui);
                    }
                });

            if let Some(index) = remove {
                self.state.remove(index, &mut self.images);
            }

            resp.request_focus();
        });
    }
}

struct Configuration {
    client_id: String,
    client_secret: String,
}

impl Configuration {
    fn from_env() -> anyhow::Result<Self> {
        fn env_var(key: &str) -> anyhow::Result<String> {
            std::env::var(key).map_err(|_| anyhow::anyhow!("cannot find '{key}' in env"))
        }

        Ok(Self {
            client_id: env_var("TWITCH_CLIENT_ID")?,
            client_secret: env_var("TWITCH_CLIENT_SECRET")?,
        })
    }
}

fn main() {
    simple_env_load::load_env_from([".dev.env", ".secrets.env"]);

    // TODO default this, provide a GUI to set it and persist it
    let config = Configuration::from_env().expect("valid configuration");

    let (handle, shutdown) = runtime::start();

    let oauth = crate::runtime::spawn(async move {
        helix::OAuth::fetch(&config.client_id, &config.client_secret)
            .await
            .expect("get oauth token")
    });

    eframe::run_native(
        env!("CARGO_PKG_NAME"),
        NativeOptions::default(),
        Box::new(|cc| {
            let state = cc
                .storage
                .and_then(|storage| storage.get_string(App::SAVE_KEY))
                .map(|s| State::load(&s))
                .unwrap_or_default();

            cc.egui_ctx.set_pixels_per_point(1.8);
            Box::new(App::new(
                state,
                helix::Client::new(oauth),
                cc.egui_ctx.clone(),
                shutdown,
            ))
        }),
    );

    handle.wait_for_shutdown();
}
