//! Goal of this library is to help crabs with web crawling.
//!
//!```rust
//!extern crate crabler;
//!
//!use crabler::*;
//!
//!#[derive(MutableWebScraper)]
//!#[on_response(response_handler)]
//!#[on_html("a[href]", print_handler)]
//!struct Scraper {}
//!
//!impl Scraper {
//!    async fn response_handler(&mut self, response: Response) -> Result<()> {
//!        println!("Status {}", response.status);
//!        Ok(())
//!    }
//!
//!    async fn print_handler(&mut self, response: Response, a: Element) -> Result<()> {
//!        if let Some(href) = a.attr("href") {
//!            println!("Found link {} on {}", href, response.url);
//!        }
//!
//!        Ok(())
//!    }
//!}
//!
//!#[async_std::main]
//!async fn main() -> Result<()> {
//!    let scraper = Scraper {};
//!
//!    scraper.run(Opts::new().with_urls(vec!["https://www.rust-lang.org/"])).await
//!}
//!```

mod opts;
use async_std::task::JoinHandle;
pub use opts::*;

mod errors;
pub use errors::*;

use async_std::channel::{unbounded, Receiver, RecvError, Sender};
use async_std::fs::File;
use async_std::prelude::*;
use async_std::sync::RwLock;
pub use crabquery::{Document, Element};
use log::{debug, error, info, warn};
use std::collections::HashSet;
use std::fmt::Debug;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

pub use async_trait::async_trait;
pub use crabler_derive::ImmutableWebScraper;
pub use crabler_derive::MutableWebScraper;

#[cfg(feature = "debug")]
fn enable_logging() {
    femme::with_level(femme::LevelFilter::Info);
}

#[cfg(not(feature = "debug"))]
fn enable_logging() {}

#[async_trait(?Send)]
pub trait MutableWebScraper {
    async fn dispatch_on_html(
        &mut self,
        selector: &str,
        response: Response,
        element: Element,
    ) -> Result<()>;
    async fn dispatch_on_response(&mut self, response: Response) -> Result<()>;
    fn all_html_selectors(&self) -> Vec<&str>;
    async fn run(&mut self, opts: Opts) -> Result<()>;
}

#[async_trait(?Send)]
pub trait ImmutableWebScraper {
    async fn dispatch_on_html(
        &self,
        selector: &str,
        response: Response,
        element: Element,
    ) -> Result<()>;
    async fn dispatch_on_response(&self, response: Response) -> Result<()>;
    fn all_html_selectors(&self) -> Vec<&str>;
    async fn run(&self, opts: Opts) -> Result<()>;
}

#[derive(Debug)]
enum WorkInput {
    Navigate(String),
    Download { url: String, destination: String },
    Exit,
}

pub struct Response {
    pub url: String,
    pub status: u16,
    pub download_destination: Option<String>,
    workinput_tx: Sender<WorkInput>,
    counter: Arc<AtomicUsize>,
}

impl Response {
    fn new(
        status: u16,
        url: String,
        download_destination: Option<String>,
        workinput_tx: Sender<WorkInput>,
        counter: Arc<AtomicUsize>,
    ) -> Self {
        Response {
            status,
            url,
            download_destination,
            workinput_tx,
            counter,
        }
    }

    /// Schedule scraper to visit given url,
    /// this will be executed on one of worker tasks
    pub async fn navigate(&mut self, url: String) -> Result<()> {
        debug!("Increasing counter by 1");
        self.counter.fetch_add(1, Ordering::SeqCst);
        self.workinput_tx.send(WorkInput::Navigate(url)).await?;

        Ok(())
    }

    /// Schedule scraper to download file from url into destination path
    pub async fn download_file(&mut self, url: String, destination: String) -> Result<()> {
        debug!("Increasing counter by 1");
        self.counter.fetch_add(1, Ordering::SeqCst);
        self.workinput_tx
            .send(WorkInput::Download { url, destination })
            .await?;

        Ok(())
    }
}

#[derive(Clone)]
struct Channels<T> {
    tx: Sender<T>,
    rx: Receiver<T>,
}

impl<T> Channels<T> {
    fn new() -> Self {
        let (tx, rx) = unbounded();

        Self { tx, rx }
    }
}

pub struct MutableCrabler<'a, T: MutableWebScraper> {
    visited_links: Arc<RwLock<HashSet<String>>>,
    workinput_ch: Channels<WorkInput>,
    workoutput_ch: Channels<WorkOutput>,
    scraper: &'a mut T,
    counter: Arc<AtomicUsize>,
    workers: Vec<async_std::task::JoinHandle<()>>,
}

macro_rules! scraper_new_impl {
    ( true,$identifier:ident ) => {
        MutableCrabler {
            visited_links: Arc::new(RwLock::new(HashSet::new())),
            workinput_ch: Channels::new(),
            workoutput_ch: Channels::new(),
            scraper: $identifier,
            counter: Arc::new(AtomicUsize::new(0)),
            workers: vec![],
        }
    };
    ( false,$identifier:ident ) => {
        ImmutableCrabler {
            visited_links: Arc::new(RwLock::new(HashSet::new())),
            workinput_ch: Channels::new(),
            workoutput_ch: Channels::new(),
            scraper: $identifier,
            counter: Arc::new(AtomicUsize::new(0)),
            workers: vec![],
        }
    };
}

macro_rules! scraper_run_impl {
    ( $identifier:ident ) => {{
        enable_logging();

        let ret = $identifier.event_loop().await;
        $identifier.shutdown().await?;
        ret
    }};
}

macro_rules! event_loop_impl {
    ( $identifier:ident ) => {
        loop {
            let output = $identifier.workoutput_ch.rx.recv().await?;
            let response_url;
            let response_status;
            let mut response_destination = None;

            match output {
                WorkOutput::Markup { text, url, status } => {
                    info!("Fetched markup from: {}", url);
                    let document = Document::from(text);
                    response_url = url.clone();
                    response_status = status;

                    let selectors = $identifier
                        .scraper
                        .all_html_selectors()
                        .iter()
                        .map(|s| s.to_string())
                        .collect::<Vec<_>>();

                    for selector in selectors {
                        for el in document.select(selector.as_str()) {
                            let response = Response::new(
                                status,
                                url.clone(),
                                None,
                                $identifier.workinput_ch.tx.clone(),
                                $identifier.counter.clone(),
                            );
                            $identifier
                                .scraper
                                .dispatch_on_html(selector.as_str(), response, el)
                                .await?;
                        }
                    }
                }
                WorkOutput::Download { url, destination } => {
                    info!("Downloaded: {} -> {}", url, destination);
                    response_url = url;
                    response_destination = Some(destination);
                    response_status = 200;
                }
                WorkOutput::Noop(url) => {
                    info!("Noop: {}", url);
                    response_url = url;
                    response_status = 304;
                }
                WorkOutput::Error(url, e) => {
                    error!("Error from {}: {}", url, e);
                    response_url = url;
                    response_status = 500;
                }
                WorkOutput::Exit => {
                    error!("Recieved exit output");
                    response_url = "".to_string();
                    response_status = 500;
                }
            }

            let response = Response::new(
                response_status,
                response_url,
                response_destination,
                $identifier.workinput_ch.tx.clone(),
                $identifier.counter.clone(),
            );
            $identifier.scraper.dispatch_on_response(response).await?;

            debug!("Decreasing counter by 1");
            $identifier.counter.fetch_sub(1, Ordering::SeqCst);

            debug!(
                "Done processing work output, counter is at {}",
                $identifier.counter.load(Ordering::SeqCst)
            );
            if $identifier.counter.load(Ordering::SeqCst) == 0 {
                return Ok(());
            }
        }
    };
}

macro_rules! start_worker_impl {
    ( $identifier:ident ) => {
        let visited_links = $identifier.visited_links.clone();
        let workinput_rx = $identifier.workinput_ch.rx.clone();
        let workoutput_tx = $identifier.workoutput_ch.tx.clone();

        let worker = Worker::new(visited_links, workinput_rx, workoutput_tx);

        let handle = async_std::task::spawn(async move {
            loop {
                info!("🐿️ Starting http worker");

                match worker.start().await {
                    Ok(()) => {
                        info!("Shutting down worker");
                        break;
                    }
                    Err(e) => warn!("❌ Restarting worker: {}", e),
                }
            }
        });

        $identifier.workers.push(handle);
    };
}

impl<'a, T> MutableCrabler<'a, T>
where
    T: MutableWebScraper,
{
    /// Create new MutableWebScraper out of given scraper struct
    pub fn new(scraper: &'a mut T) -> Self {
        scraper_new_impl!(true, scraper)
    }

    async fn shutdown(&self) -> Result<()> {
        scraper_shutdown(&self.workers, &self.workinput_ch, &self.workoutput_ch).await
    }

    /// Schedule scraper to visit given url,
    /// this will be executed on one of worker tasks
    pub async fn navigate(&self, url: &str) -> Result<()> {
        scraper_navigate(&self.counter, &self.workinput_ch, url).await
    }

    /// Run processing loop for the given MutableWebScraper
    pub async fn run(&mut self) -> Result<()> {
        scraper_run_impl!(self)
    }

    async fn event_loop(&mut self) -> Result<()> {
        event_loop_impl!(self)
    }

    /// Create and start new worker tasks.
    /// Worker task will automatically exit after scraper instance is freed.
    pub fn start_worker(&mut self) {
        start_worker_impl!(self);
    }
}

pub struct ImmutableCrabler<'a, T: ImmutableWebScraper> {
    visited_links: Arc<RwLock<HashSet<String>>>,
    workinput_ch: Channels<WorkInput>,
    workoutput_ch: Channels<WorkOutput>,
    scraper: &'a T,
    counter: Arc<AtomicUsize>,
    workers: Vec<async_std::task::JoinHandle<()>>,
}

impl<'a, T> ImmutableCrabler<'a, T>
where
    T: ImmutableWebScraper,
{
    /// Create new ImmutableWebScraper out of given scraper struct
    pub fn new(scraper: &'a T) -> Self {
        scraper_new_impl!(false, scraper)
    }

    async fn shutdown(&self) -> Result<()> {
        scraper_shutdown(&self.workers, &self.workinput_ch, &self.workoutput_ch).await
    }

    /// Schedule scraper to visit given url,
    /// this will be executed on one of worker tasks
    pub async fn navigate(&self, url: &str) -> Result<()> {
        scraper_navigate(&self.counter, &self.workinput_ch, url).await
    }

    /// Run processing loop for the given MutableWebScraper
    pub async fn run(&self) -> Result<()> {
        scraper_run_impl!(self)
    }

    async fn event_loop(&self) -> Result<()> {
        event_loop_impl!(self)
    }

    /// Create and start new worker tasks.
    /// Worker task will automatically exit after scraper instance is freed.
    pub fn start_worker(&mut self) {
        start_worker_impl!(self);
    }
}

async fn scraper_shutdown(
    workers: &Vec<JoinHandle<()>>,
    input: &Channels<WorkInput>,
    output: &Channels<WorkOutput>,
) -> Result<()> {
    for _ in workers.iter() {
        input.tx.send(WorkInput::Exit).await?;
    }
    input.tx.close();
    input.rx.close();
    output.tx.close();
    output.rx.close();
    Ok(())
}

async fn scraper_navigate(
    counter: &Arc<AtomicUsize>,
    input: &Channels<WorkInput>,
    url: &str,
) -> Result<()> {
    debug!("Increasing counter by 1");
    counter.fetch_add(1, Ordering::SeqCst);

    Ok(input.tx.send(WorkInput::Navigate(url.to_string())).await?)
}

struct Worker {
    visited_links: Arc<RwLock<HashSet<String>>>,
    workinput_rx: Receiver<WorkInput>,
    workoutput_tx: Sender<WorkOutput>,
}

impl Worker {
    fn new(
        visited_links: Arc<RwLock<HashSet<String>>>,
        workinput_rx: Receiver<WorkInput>,
        workoutput_tx: Sender<WorkOutput>,
    ) -> Self {
        Worker {
            visited_links,
            workinput_rx,
            workoutput_tx,
        }
    }

    async fn start(&self) -> Result<()> {
        let workoutput_tx = self.workoutput_tx.clone();

        loop {
            let workinput = self.workinput_rx.recv().await;
            if let Err(RecvError) = workinput {
                continue;
            }

            let workinput = workinput?;
            let payload = self.process_message(workinput).await;

            match payload {
                Ok(WorkOutput::Exit) => return Ok(()),
                _ => workoutput_tx.send(payload?).await?,
            }
        }
    }

    async fn process_message(&self, workinput: WorkInput) -> Result<WorkOutput> {
        match workinput {
            WorkInput::Navigate(url) => {
                let workoutput = self.navigate(url.clone()).await;

                if let Err(e) = workoutput {
                    Ok(WorkOutput::Error(url, e))
                } else {
                    workoutput
                }
            }
            WorkInput::Download { url, destination } => {
                let workoutput = self.download(url.clone(), destination).await;

                if let Err(e) = workoutput {
                    Ok(WorkOutput::Error(url, e))
                } else {
                    workoutput
                }
            }
            WorkInput::Exit => Ok(WorkOutput::Exit),
        }
    }

    async fn navigate(&self, url: String) -> Result<WorkOutput> {
        let contains = self.visited_links.read().await.contains(&url.clone());

        if !contains {
            self.visited_links.write().await.insert(url.clone());
            let response = surf::get(&url).await?;

            workoutput_from_response(response, url.clone()).await
        } else {
            Ok(WorkOutput::Noop(url))
        }
    }

    async fn download(&self, url: String, destination: String) -> Result<WorkOutput> {
        let contains = self.visited_links.read().await.contains(&url.clone());

        if !contains {
            // need to notify parent about work being done
            let response = surf::get(&*url).await?.body_bytes().await?;
            let mut dest = File::create(destination.clone()).await?;
            dest.write_all(&response).await?;

            Ok(WorkOutput::Download { url, destination })
        } else {
            Ok(WorkOutput::Noop(url))
        }
    }
}

#[derive(Debug)]
enum WorkOutput {
    Markup {
        url: String,
        text: String,
        status: u16,
    },
    Download {
        url: String,
        destination: String,
    },
    Noop(String),
    Error(String, CrablerError),
    Exit,
}

async fn workoutput_from_response(mut response: surf::Response, url: String) -> Result<WorkOutput> {
    let status = response.status().into();
    let text = response.body_string().await?;

    if text.len() == 0 {
        error!("body length is 0")
    }

    Ok(WorkOutput::Markup { status, url, text })
}
