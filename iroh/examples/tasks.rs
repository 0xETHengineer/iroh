//! Create a list of tasks that can be shared among devices
//!
//! By default a new peer id is created when starting the example. To reuse your identity,
//! set the `--private-key` CLI flag with the private key printed on a previous invocation.
//!
//! You can use this with a local DERP server. To do so, run
//! `cargo run --bin derper -- --dev`
//! and then set the `-d http://localhost:3340` flag on this example.

use std::{fmt, net::SocketAddr, path::PathBuf, str::FromStr, sync::Arc};

use anyhow::bail;
use bytes::Bytes;
use clap::{CommandFactory, FromArgMatches, Parser};
use comfy_table::{presets::UTF8_FULL, Cell, CellAlignment, Table};
use ed25519_dalek::SigningKey;
use iroh::sync::{BlobStore, Doc, DocStore, DownloadMode, LiveSync, PeerSource, SYNC_ALPN};
use iroh_gossip::{
    net::{GossipHandle, GOSSIP_ALPN},
    proto::TopicId,
};
use iroh_metrics::{
    core::{Counter, Metric},
    struct_iterable::Iterable,
};
use iroh_net::{
    defaults::{default_derp_map, DEFAULT_DERP_STUN_PORT},
    derp::{DerpMap, UseIpv4, UseIpv6},
    magic_endpoint::get_alpn,
    tls::Keypair,
    MagicEndpoint,
};
use iroh_sync::sync::{Author, Namespace, RecordIdentifier, SignedEntry};
use once_cell::sync::OnceCell;
use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, oneshot, Mutex};
use tracing_subscriber::{EnvFilter, Registry};
use url::Url;

use iroh_bytes_handlers::IrohBytesHandlers;

#[derive(Parser, Debug)]
struct Args {
    /// Private key to derive our peer id from
    #[clap(long)]
    private_key: Option<String>,
    /// Path to a data directory where blobs will be persisted
    #[clap(short, long)]
    storage_path: Option<PathBuf>,
    /// Set a custom DERP server. By default, the DERP server hosted by n0 will be used.
    #[clap(short, long)]
    derp: Option<Url>,
    /// Disable DERP completeley
    #[clap(long)]
    no_derp: bool,
    /// Set your nickname
    #[clap(short, long)]
    name: Option<String>,
    /// Set the bind port for our socket. By default, a random port will be used.
    #[clap(short, long, default_value = "0")]
    bind_port: u16,
    /// Bind address on which to serve Prometheus metrics
    #[clap(long)]
    metrics_addr: Option<SocketAddr>,
    #[clap(subcommand)]
    command: Command,
}

#[derive(Parser, Debug)]
enum Command {
    Open { doc_name: String },
    Join { ticket: String },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    run(args).await
}

pub fn init_metrics_collection(
    metrics_addr: Option<SocketAddr>,
) -> Option<tokio::task::JoinHandle<()>> {
    iroh_metrics::core::Core::init(|reg, metrics| {
        metrics.insert(iroh::sync::metrics::Metrics::new(reg));
        metrics.insert(iroh_gossip::metrics::Metrics::new(reg));
    });

    // doesn't start the server if the address is None
    if let Some(metrics_addr) = metrics_addr {
        return Some(tokio::spawn(async move {
            if let Err(e) = iroh_metrics::metrics::start_metrics_server(metrics_addr).await {
                eprintln!("Failed to start metrics server: {e}");
            }
        }));
    }
    tracing::info!("Metrics server not started, no address provided");
    None
}

async fn run(args: Args) -> anyhow::Result<()> {
    // setup logging
    let log_filter = init_logging();

    let metrics_fut = init_metrics_collection(args.metrics_addr);

    // parse or generate our keypair
    let keypair = match args.private_key {
        None => Keypair::generate(),
        Some(key) => parse_keypair(&key)?,
    };
    println!("> our private key: {}", fmt_secret(&keypair));

    // configure our derp map
    let derp_map = match (args.no_derp, args.derp) {
        (false, None) => Some(default_derp_map()),
        (false, Some(url)) => Some(derp_map_from_url(url)?),
        (true, None) => None,
        (true, Some(_)) => bail!("You cannot set --no-derp and --derp at the same time"),
    };
    println!("> using DERP servers: {}", fmt_derp_map(&derp_map));

    // build our magic endpoint and the gossip protocol
    let (endpoint, gossip, initial_endpoints) = {
        // init a cell that will hold our gossip handle to be used in endpoint callbacks
        let gossip_cell: OnceCell<GossipHandle> = OnceCell::new();
        // init a channel that will emit once the initial endpoints of our local node are discovered
        let (initial_endpoints_tx, mut initial_endpoints_rx) = mpsc::channel(1);
        // build the magic endpoint
        let endpoint = MagicEndpoint::builder()
            .keypair(keypair.clone())
            .alpns(vec![
                GOSSIP_ALPN.to_vec(),
                SYNC_ALPN.to_vec(),
                iroh_bytes::protocol::ALPN.to_vec(),
            ])
            .derp_map(derp_map)
            .on_endpoints({
                let gossip_cell = gossip_cell.clone();
                Box::new(move |endpoints| {
                    // send our updated endpoints to the gossip protocol to be sent as PeerData to peers
                    if let Some(gossip) = gossip_cell.get() {
                        gossip.update_endpoints(endpoints).ok();
                    }
                    // trigger oneshot on the first endpoint update
                    initial_endpoints_tx.try_send(endpoints.to_vec()).ok();
                })
            })
            .bind(args.bind_port)
            .await?;

        // initialize the gossip protocol
        let gossip = GossipHandle::from_endpoint(endpoint.clone(), Default::default());
        // insert into the gossip cell to be used in the endpoint callbacks above
        gossip_cell.set(gossip.clone()).unwrap();

        // wait for a first endpoint update so that we know about at least one of our addrs
        let initial_endpoints = initial_endpoints_rx.recv().await.unwrap();
        // pass our initial endpoints to the gossip protocol so that they can be announced to peers
        gossip.update_endpoints(&initial_endpoints)?;
        (endpoint, gossip, initial_endpoints)
    };
    println!("> our peer id: {}", endpoint.peer_id());

    let (topic, peers) = match &args.command {
        Command::Open { doc_name } => {
            let topic: TopicId = blake3::hash(doc_name.as_bytes()).into();
            println!(
                "> opening document {doc_name} as namespace {} and waiting for peers to join us...",
                fmt_hash(topic.as_bytes())
            );
            (topic, vec![])
        }
        Command::Join { ticket } => {
            let Ticket { topic, peers } = Ticket::from_str(ticket)?;
            println!("> joining topic {topic} and connecting to {peers:?}",);
            (topic, peers)
        }
    };

    let our_ticket = {
        // add our local endpoints to the ticket and print it for others to join
        let addrs = initial_endpoints.iter().map(|ep| ep.addr).collect();
        let mut peers = peers.clone();
        peers.push(PeerSource {
            peer_id: endpoint.peer_id(),
            addrs,
            derp_region: endpoint.my_derp().await,
        });
        Ticket { peers, topic }
    };
    println!("> ticket to join us: {our_ticket}");

    // unwrap our storage path or default to temp
    let storage_path = args.storage_path.unwrap_or_else(|| {
        let name = format!("iroh-todo-{}", endpoint.peer_id());
        let dir = std::env::temp_dir().join(name);
        if !dir.exists() {
            std::fs::create_dir(&dir).expect("failed to create temp dir");
        }
        dir
    });
    println!("> storage directory: {storage_path:?}");

    // create a runtime that can spawn tasks on a local-thread executors (to support !Send futures)
    let rt = iroh_bytes::util::runtime::Handle::from_currrent(num_cpus::get())?;

    // create a blob store (with a iroh-bytes database inside)
    let blobs = BlobStore::new(rt.clone(), storage_path.join("blobs"), endpoint.clone()).await?;

    // create a doc store for the iroh-sync docs
    let author = Author::from(keypair.secret().clone());
    let docs = DocStore::new(blobs.clone(), author, storage_path.join("docs"));

    // create the live syncer
    let live_sync = LiveSync::spawn(endpoint.clone(), gossip.clone());

    // construct the state that is passed to the endpoint loop and from there cloned
    // into to the connection handler task for incoming connections.
    let state = Arc::new(State {
        gossip: gossip.clone(),
        docs: docs.clone(),
        bytes: IrohBytesHandlers::new(rt.clone(), blobs.db().clone()),
    });

    // spawn our endpoint loop that forwards incoming connections
    tokio::spawn(endpoint_loop(endpoint.clone(), state));

    // open our document and add to the live syncer
    let namespace = Namespace::from_bytes(topic.as_bytes());
    println!("> opening doc {}", fmt_hash(namespace.id().as_bytes()));
    let doc = docs.create_or_open(namespace, DownloadMode::Always).await?;
    live_sync.add(doc.replica().clone(), peers.clone()).await?;

    // spawn an repl thread that reads stdin and parses each line as a `Cmd` command
    let (cmd_tx, mut cmd_rx) = mpsc::channel(1);
    std::thread::spawn(move || repl_loop(cmd_tx).expect("input loop crashed"));
    // process commands in a loop
    println!("> ready to accept commands");
    println!("> type `help` for a list of commands");

    let (send, recv) = mpsc::channel(32);
    doc.on_insert(Box::new(move |_origin, entry| {
        send.try_send((entry.entry().id().to_owned(), entry))
            .expect("receiver dropped");
    }));
    let (mut tasks, mut update_errors) = Tasks::new(doc, recv).await?;

    loop {
        // wait for a command from the input repl thread
        let Some((cmd, to_repl_tx)) = cmd_rx.recv().await else {
            break;
        };
        // exit command: break early
        if let Cmd::Exit = cmd {
            to_repl_tx.send(ToRepl::Exit).ok();
            break;
        }

        // handle the command, but select against Ctrl-C signal so that commands can be aborted
        tokio::select! {
            biased;
            _ = tokio::signal::ctrl_c() => {
                println!("> aborted");
            }

            _ = &mut update_errors => {
                // TODO: when err is actual error print it
                println!("> error updating tasks");
                break;
            }
            res = handle_command(cmd, &mut tasks, &our_ticket, &log_filter) => if let Err(err) = res {
                println!("> error: {err}");
            },
        };
        // notify to the repl that we want to get the next command
        to_repl_tx.send(ToRepl::Continue).ok();
    }

    // exit: cancel the sync and store blob database and document
    if let Err(err) = live_sync.cancel().await {
        println!("> syncer closed with error: {err:?}");
    }
    println!("> persisting tasks {storage_path:?}");
    blobs.save().await?;
    tasks.save(&docs).await?;

    if let Some(metrics_fut) = metrics_fut {
        metrics_fut.abort();
        drop(metrics_fut);
    }

    tasks.handle.abort();

    Ok(())
}

async fn handle_command(
    cmd: Cmd,
    task: &mut Tasks,
    ticket: &Ticket,
    log_filter: &LogLevelReload,
) -> anyhow::Result<()> {
    match cmd {
        Cmd::Add { description } => task.new_task(description).await?,
        Cmd::Done { index } => task.mark_done(index).await?,
        Cmd::Delete { index } => task.delete(index).await?,
        Cmd::Archive => task.clear().await?,
        Cmd::Ls => task.list().await,
        Cmd::Ticket => {
            println!("Ticket: {ticket}");
        }
        Cmd::Log { directive } => {
            let next_filter = EnvFilter::from_str(&directive)?;
            log_filter.modify(|layer| *layer = next_filter)?;
        }
        Cmd::Stats => get_stats(),
        Cmd::Exit => {}
    }

    Ok(())
}

#[derive(Clone, Serialize, Deserialize)]
/// Task in a list of tasks
struct Task {
    /// Description of the task
    /// Limited to 2000 characters
    description: String,
    /// Record creation timestamp. Counted as micros since the Unix epoch.
    created: u64,
    /// Whether or not the task has been completed. Done tasks will show up in the task list until
    /// they are archived.
    done: bool,
    /// Archive indicates whether we should display the task
    archived: bool,
}

const MAX_TASK_SIZE: usize = 2 * 1024;
const MAX_DESCRIPTION_LEN: usize = 2 * 1000;

impl Task {
    fn from_bytes(bytes: Bytes) -> anyhow::Result<Self> {
        let task = postcard::from_bytes(&bytes)?;
        Ok(task)
    }

    fn as_bytes(self) -> anyhow::Result<Bytes> {
        let mut buf = bytes::BytesMut::zeroed(MAX_TASK_SIZE);
        postcard::to_slice(&self, &mut buf)?;
        Ok(buf.freeze())
    }

    fn missing_task() -> Self {
        Self {
            description: String::from("Missing Content"),
            created: 0,
            done: false,
            archived: false,
        }
    }
}

/// List of tasks, including completed tasks that have not been archived
struct Tasks {
    inner: Arc<Mutex<InnerTasks>>,
    handle: tokio::task::JoinHandle<()>,
}

struct InnerTasks {
    tasks: Vec<(RecordIdentifier, Task)>,
    doc: Doc,
}

/// TODO: make actual error
enum UpdateError {
    NoMoreUpdates,
    DeserializeTask,
    AddingTask,
}

impl Tasks {
    async fn new(
        doc: Doc,
        mut updates: mpsc::Receiver<(RecordIdentifier, SignedEntry)>,
    ) -> anyhow::Result<(Self, oneshot::Receiver<UpdateError>)> {
        let entries = doc.replica().all();
        let mut tasks = vec![];
        for (id, entry) in entries.into_iter() {
            match doc.get_content_bytes(&entry).await {
                None => tasks.push((id, Task::missing_task())),
                Some(content) => {
                    let task = Task::from_bytes(content)?;
                    if !task.archived {
                        tasks.push((id, task))
                    }
                }
            }
        }
        tasks.sort_by_key(|(_, task)| task.created);
        let inner = Arc::new(Mutex::new(InnerTasks { doc, tasks }));
        let inner_clone = Arc::clone(&inner);
        let (sender, receiver) = oneshot::channel();
        let handle = tokio::spawn(async move {
            loop {
                tokio::select! {
                    biased;
                    _ = tokio::signal::ctrl_c() => {
                        return;
                    }
                    res = updates.recv() => {
                        match res {
                            Some((id, entry)) => {
                                let mut inner = inner_clone.lock().await;
                                let doc = &inner.doc;
                                let content = doc.get_content_bytes(&entry).await;
                                let task = match content {
                                    Some(content) => {
                                        match Task::from_bytes(content) {
                                            Ok(task) => task,
                                            Err(_) => {
                                                    let _ = sender.send(UpdateError::DeserializeTask);
                                                    return;
                                            }
                                        }
                                    },
                                    None => Task::missing_task(),
                                };
                                match inner.insert_task(id, task) {
                                    Ok(_) => {},
                                    Err(_) => {
                                        let _ = sender.send(UpdateError::AddingTask);
                                        return;
                                    }
                                }

                                let table = fmt_tasks(&inner.tasks);
                                println!("{table}");
                            },
                            None => {
                                let _ = sender.send(UpdateError::NoMoreUpdates);
                                return;
                            }
                        }
                    }
                }
            }
        });
        Ok((Self { inner, handle }, receiver))
    }

    async fn save(&self, store: &DocStore) -> anyhow::Result<()> {
        let inner = self.inner.lock().await;
        inner.save(store).await
    }

    async fn new_task(&mut self, description: String) -> anyhow::Result<()> {
        if description.len() > MAX_DESCRIPTION_LEN {
            bail!("The task description must be under {MAX_DESCRIPTION_LEN} characters");
        }
        let id = nanoid::nanoid!();
        let created = std::time::SystemTime::now()
            .duration_since(std::time::SystemTime::UNIX_EPOCH)
            .expect("time drift")
            .as_secs();
        let task = Task {
            description,
            created,
            done: false,
            archived: false,
        };
        self.insert_bytes(id.as_bytes(), task.as_bytes()?).await
    }

    async fn insert_bytes(&self, key: impl AsRef<[u8]>, content: Bytes) -> anyhow::Result<()> {
        let inner = self.inner.lock().await;
        inner.doc.insert_bytes(key, content).await?;
        Ok(())
    }

    async fn update_task(&mut self, key: impl AsRef<[u8]>, task: Task) -> anyhow::Result<()> {
        let content = task.as_bytes()?;
        self.insert_bytes(key, content).await
    }

    async fn mark_done(&mut self, index: usize) -> anyhow::Result<()> {
        let (id, mut task) = {
            let inner = self.inner.lock().await;
            inner.get_task(index)?
        };
        task.done = true;
        self.update_task(id.key(), task).await
    }

    async fn delete(&mut self, index: usize) -> anyhow::Result<()> {
        let (id, mut task) = {
            let inner = self.inner.lock().await;
            inner.get_task(index)?
        };
        task.archived = true;
        self.update_task(id.key(), task).await
    }

    async fn clear(&mut self) -> anyhow::Result<()> {
        let tasks = {
            let inner = self.inner.lock().await;
            inner.get_done_tasks()
        };
        for (id, mut task) in tasks {
            task.archived = true;
            self.update_task(id.key(), task).await?;
        }
        Ok(())
    }

    async fn list(&self) {
        let inner = self.inner.lock().await;
        let table = fmt_tasks(&inner.tasks);
        println!("{table}");
    }
}

impl InnerTasks {
    fn insert_task(&mut self, id: RecordIdentifier, task: Task) -> anyhow::Result<()> {
        if let Some(index) = self.tasks.iter().position(|(tid, _)| &id == tid) {
            if task.archived {
                self.tasks.remove(index);
            } else {
                self.tasks.insert(index, (id, task));
            }
        } else {
            if !task.archived {
                self.tasks.push((id, task));
            }
        }

        self.tasks.sort_by_key(|(_, task)| task.created);
        Ok(())
    }

    fn get_task(&self, index: usize) -> anyhow::Result<(RecordIdentifier, Task)> {
        match self.tasks.get(index) {
            Some((id, task)) => Ok((id.to_owned(), task.clone())),
            None => bail!("No task exists at index {index}"),
        }
    }

    fn get_done_tasks(&self) -> Vec<(RecordIdentifier, Task)> {
        self.tasks
            .iter()
            .filter(|(_, t)| t.done)
            .map(|(id, task)| (id.to_owned(), task.clone()))
            .collect()
    }

    async fn save(&self, store: &DocStore) -> anyhow::Result<()> {
        store.save(&self.doc).await
    }
}

fn fmt_tasks(tasks: &Vec<(RecordIdentifier, Task)>) -> String {
    let mut table = Table::new();
    table
        .load_preset(UTF8_FULL)
        .set_width(1024)
        .set_header(vec!["Num", "Done", "Task"]);
    for (num, (_, task)) in tasks.iter().enumerate() {
        let num = num.to_string();
        let done = if task.done { "✓" } else { "" };
        table.add_row(vec![
            Cell::new(num).set_alignment(CellAlignment::Center),
            Cell::new(done).set_alignment(CellAlignment::Center),
            Cell::new(task.description.clone()).set_alignment(CellAlignment::Left),
        ]);
    }
    table.to_string()
}

#[derive(Parser, Debug)]
pub enum Cmd {
    /// Add a task
    Add {
        /// the content of the actual task
        description: String,
    },
    /// Mark a task as finished
    Done {
        /// The index of the task
        index: usize,
    },
    /// Remove a task
    Delete {
        /// The index of the task
        index: usize,
    },
    /// Archives finished tasks
    Archive,
    /// List entries.
    Ls,

    /// Print the ticket with which other peers can join our document.
    Ticket,
    /// Change the log level
    Log {
        /// The log level or log filtering directive
        ///
        /// Valid log levels are: "trace", "debug", "info", "warn", "error"
        ///
        /// You can also set one or more filtering directives to enable more fine-grained log
        /// filtering. The supported filtering directives and their semantics are documented here:
        /// https://docs.rs/tracing-subscriber/latest/tracing_subscriber/filter/struct.EnvFilter.html#directives
        ///
        /// To disable logging completely, set to the empty string (via empty double quotes: "").
        #[clap(verbatim_doc_comment)]
        directive: String,
    },
    /// Show stats about the current session
    Stats,
    /// Quit
    Exit,
}

impl FromStr for Cmd {
    type Err = anyhow::Error;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let args = shell_words::split(s)?;
        let matches = Cmd::command()
            .multicall(true)
            .subcommand_required(true)
            .try_get_matches_from(args)?;
        let cmd = Cmd::from_arg_matches(&matches)?;
        Ok(cmd)
    }
}

#[derive(Debug)]
struct State {
    gossip: GossipHandle,
    docs: DocStore,
    bytes: IrohBytesHandlers,
}

async fn endpoint_loop(endpoint: MagicEndpoint, state: Arc<State>) -> anyhow::Result<()> {
    while let Some(conn) = endpoint.accept().await {
        let state = state.clone();
        tokio::spawn(async move {
            if let Err(err) = handle_connection(conn, state).await {
                println!("> connection closed, reason: {err}");
            }
        });
    }
    Ok(())
}

async fn handle_connection(mut conn: quinn::Connecting, state: Arc<State>) -> anyhow::Result<()> {
    let alpn = get_alpn(&mut conn).await?;
    println!("> incoming connection with alpn {alpn}");
    match alpn.as_bytes() {
        GOSSIP_ALPN => state.gossip.handle_connection(conn.await?).await,
        SYNC_ALPN => state.docs.handle_connection(conn).await,
        alpn if alpn == iroh_bytes::protocol::ALPN => state.bytes.handle_connection(conn).await,
        _ => bail!("ignoring connection: unsupported ALPN protocol"),
    }
}

#[derive(Debug)]
enum ToRepl {
    Continue,
    Exit,
}

fn repl_loop(cmd_tx: mpsc::Sender<(Cmd, oneshot::Sender<ToRepl>)>) -> anyhow::Result<()> {
    use rustyline::{error::ReadlineError, Config, DefaultEditor};
    let mut rl = DefaultEditor::with_config(Config::builder().check_cursor_position(true).build())?;
    loop {
        // prepare a channel to receive a signal from the main thread when a command completed
        let (to_repl_tx, to_repl_rx) = oneshot::channel();
        let readline = rl.readline(">> ");
        match readline {
            Ok(line) if line.is_empty() => continue,
            Ok(line) => {
                rl.add_history_entry(line.as_str())?;
                match Cmd::from_str(&line) {
                    Ok(cmd) => cmd_tx.blocking_send((cmd, to_repl_tx))?,
                    Err(err) => {
                        println!("{err}");
                        continue;
                    }
                };
            }
            Err(ReadlineError::Interrupted | ReadlineError::Eof) => {
                cmd_tx.blocking_send((Cmd::Exit, to_repl_tx))?;
            }
            Err(ReadlineError::WindowResized) => continue,
            Err(err) => return Err(err.into()),
        }
        // wait for reply from main thread
        match to_repl_rx.blocking_recv()? {
            ToRepl::Continue => continue,
            ToRepl::Exit => break,
        }
    }
    Ok(())
}

fn get_stats() {
    let core = iroh_metrics::core::Core::get().expect("Metrics core not initialized");
    println!("# sync");
    let metrics = core
        .get_collector::<iroh::sync::metrics::Metrics>()
        .unwrap();
    fmt_metrics(metrics);
    println!("# gossip");
    let metrics = core
        .get_collector::<iroh_gossip::metrics::Metrics>()
        .unwrap();
    fmt_metrics(metrics);
}

fn fmt_metrics(metrics: &impl Iterable) {
    for (name, counter) in metrics.iter() {
        if let Some(counter) = counter.downcast_ref::<Counter>() {
            let value = counter.get();
            println!("{name:23} : {value:>6}    ({})", counter.description);
        } else {
            println!("{name:23} : unsupported metric kind");
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct Ticket {
    topic: TopicId,
    peers: Vec<PeerSource>,
}
impl Ticket {
    /// Deserializes from bytes.
    fn from_bytes(bytes: &[u8]) -> anyhow::Result<Self> {
        postcard::from_bytes(bytes).map_err(Into::into)
    }
    /// Serializes to bytes.
    pub fn to_bytes(&self) -> Vec<u8> {
        postcard::to_stdvec(self).expect("postcard::to_stdvec is infallible")
    }
}

/// Serializes to base32.
impl fmt::Display for Ticket {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let encoded = self.to_bytes();
        let mut text = data_encoding::BASE32_NOPAD.encode(&encoded);
        text.make_ascii_lowercase();
        write!(f, "{text}")
    }
}

/// Deserializes from base32.
impl FromStr for Ticket {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let bytes = data_encoding::BASE32_NOPAD.decode(s.to_ascii_uppercase().as_bytes())?;
        let slf = Self::from_bytes(&bytes)?;
        Ok(slf)
    }
}

type LogLevelReload = tracing_subscriber::reload::Handle<EnvFilter, Registry>;
fn init_logging() -> LogLevelReload {
    use tracing_subscriber::{filter, fmt, prelude::*, reload};
    let filter = filter::EnvFilter::from_default_env();
    let (filter, reload_handle) = reload::Layer::new(filter);
    tracing_subscriber::registry()
        .with(filter)
        .with(fmt::Layer::default())
        .init();
    reload_handle
}

// helpers
fn fmt_hash(hash: impl AsRef<[u8]>) -> String {
    let mut text = data_encoding::BASE32_NOPAD.encode(hash.as_ref());
    text.make_ascii_lowercase();
    format!("{}…{}", &text[..5], &text[(text.len() - 2)..])
}
fn fmt_secret(keypair: &Keypair) -> String {
    let mut text = data_encoding::BASE32_NOPAD.encode(&keypair.secret().to_bytes());
    text.make_ascii_lowercase();
    text
}
fn parse_keypair(secret: &str) -> anyhow::Result<Keypair> {
    let bytes: [u8; 32] = data_encoding::BASE32_NOPAD
        .decode(secret.to_ascii_uppercase().as_bytes())?
        .try_into()
        .map_err(|_| anyhow::anyhow!("Invalid secret"))?;
    let key = SigningKey::from_bytes(&bytes);
    Ok(key.into())
}
fn fmt_derp_map(derp_map: &Option<DerpMap>) -> String {
    match derp_map {
        None => "None".to_string(),
        Some(map) => {
            let regions = map.regions.iter().map(|(id, region)| {
                let nodes = region.nodes.iter().map(|node| node.url.to_string());
                (*id, nodes.collect::<Vec<_>>())
            });
            format!("{:?}", regions.collect::<Vec<_>>())
        }
    }
}
fn derp_map_from_url(url: Url) -> anyhow::Result<DerpMap> {
    Ok(DerpMap::default_from_node(
        url,
        DEFAULT_DERP_STUN_PORT,
        UseIpv4::TryDns,
        UseIpv6::TryDns,
        0,
    ))
}

/// handlers for iroh_bytes connections
mod iroh_bytes_handlers {
    use std::sync::Arc;

    use bytes::Bytes;
    use futures::{future::BoxFuture, FutureExt};
    use iroh_bytes::{
        protocol::{GetRequest, RequestToken},
        provider::{CustomGetHandler, EventSender, RequestAuthorizationHandler},
    };

    use iroh::{collection::IrohCollectionParser, database::flat::Database};

    #[derive(Debug, Clone)]
    pub struct IrohBytesHandlers {
        db: Database,
        rt: iroh_bytes::util::runtime::Handle,
        event_sender: NoopEventSender,
        get_handler: Arc<NoopCustomGetHandler>,
        auth_handler: Arc<NoopRequestAuthorizationHandler>,
    }
    impl IrohBytesHandlers {
        pub fn new(rt: iroh_bytes::util::runtime::Handle, db: Database) -> Self {
            Self {
                db,
                rt,
                event_sender: NoopEventSender,
                get_handler: Arc::new(NoopCustomGetHandler),
                auth_handler: Arc::new(NoopRequestAuthorizationHandler),
            }
        }
        pub async fn handle_connection(&self, conn: quinn::Connecting) -> anyhow::Result<()> {
            iroh_bytes::provider::handle_connection(
                conn,
                self.db.clone(),
                self.event_sender.clone(),
                IrohCollectionParser,
                self.get_handler.clone(),
                self.auth_handler.clone(),
                self.rt.clone(),
            )
            .await;
            Ok(())
        }
    }

    #[derive(Debug, Clone)]
    struct NoopEventSender;
    impl EventSender for NoopEventSender {
        fn send(&self, _event: iroh_bytes::provider::Event) -> BoxFuture<()> {
            async {}.boxed()
        }
    }
    #[derive(Debug)]
    struct NoopCustomGetHandler;
    impl CustomGetHandler for NoopCustomGetHandler {
        fn handle(
            &self,
            _token: Option<RequestToken>,
            _request: Bytes,
        ) -> BoxFuture<'static, anyhow::Result<GetRequest>> {
            async move { Err(anyhow::anyhow!("no custom get handler defined")) }.boxed()
        }
    }
    #[derive(Debug)]
    struct NoopRequestAuthorizationHandler;
    impl RequestAuthorizationHandler for NoopRequestAuthorizationHandler {
        fn authorize(
            &self,
            token: Option<RequestToken>,
            _request: &iroh_bytes::protocol::Request,
        ) -> BoxFuture<'static, anyhow::Result<()>> {
            async move {
                if let Some(token) = token {
                    anyhow::bail!(
                        "no authorization handler defined, but token was provided: {:?}",
                        token
                    );
                }
                Ok(())
            }
            .boxed()
        }
    }
}
