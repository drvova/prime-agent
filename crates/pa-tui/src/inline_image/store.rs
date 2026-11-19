//! The image store: payloads registered by the transcript rows that show them, the rendered
//! (scaled) forms per geometry and protocol, and the one decode worker that produces them.
//! The paint path only ever takes this lock for map lookups: a miss queues a job and paints
//! the reserved blank cells, and the worker's completion wakes the event loop for a repaint.

use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::sync::{Arc, LazyLock, Mutex};

use base64::Engine;

use super::detect::ImageProtocol;
use super::raster::{self, Rgba};
use super::{encode, ImageGeometry};
use crate::Line;

/// Registered payload bytes (base64) the store keeps for re-renders; the least recently shown
/// leave first.
const SOURCE_BUDGET_BYTES: usize = 96 * 1024 * 1024;
/// Rendered forms (half-block rows plus protocol payloads) the store keeps.
const RENDERED_BUDGET_BYTES: usize = 64 * 1024 * 1024;
/// The largest payload the store decodes (base64 bytes); a larger one keeps its caption.
const MAX_PAYLOAD_BYTES: usize = 48 * 1024 * 1024;
/// The protocol payload's pixel box is capped on its longer side: the terminal scales it into
/// the reserved cells either way.
const MAX_PAYLOAD_SIDE: u32 = 2048;

/// One rendering of an image: its geometry, the protocol it was encoded for, and the cell
/// pixel size the protocol payload was scaled to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct RenderKey {
    pub(crate) image: u64,
    pub(crate) geometry: ImageGeometry,
    pub(crate) protocol: ImageProtocol,
    pub(crate) cell: (u16, u16),
}

/// The protocol bytes one placement writes.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Payload {
    /// The scaled PNG kitty transmits once under an id.
    KittyPng(Vec<u8>),
    /// A complete cell-bound sequence (the iTerm2 OSC, or the sixel DCS) written at the cursor.
    Sequence(String),
}

/// One finished rendering.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Rendered {
    /// `geometry.rows` rows of `geometry.cols` half-block cells.
    pub(crate) cells: Vec<Line>,
    /// The pixel protocol's payload (`None` for the half-block protocol).
    pub(crate) payload: Option<Payload>,
}

impl Rendered {
    fn bytes(&self) -> usize {
        let cells: usize = self
            .cells
            .iter()
            .flatten()
            .map(|span| span.content.len() + 16)
            .sum();
        let payload = match &self.payload {
            Some(Payload::KittyPng(bytes)) => bytes.len(),
            Some(Payload::Sequence(text)) => text.len(),
            None => 0,
        };
        cells + payload
    }
}

struct Source {
    data: Arc<str>,
    used: u64,
}

struct Entry {
    rendered: Arc<Rendered>,
    used: u64,
}

struct Job {
    key: RenderKey,
    data: Arc<str>,
}

#[derive(Default)]
struct Store {
    sources: HashMap<u64, Source>,
    source_bytes: usize,
    rendered: HashMap<RenderKey, Entry>,
    rendered_bytes: usize,
    in_flight: HashSet<RenderKey>,
    /// Renderings whose decode failed: never retried (their rows keep the blank cells).
    failed: HashSet<RenderKey>,
    tick: u64,
    worker: Option<std::sync::mpsc::Sender<Job>>,
    waker: Option<tokio::sync::mpsc::UnboundedSender<()>>,
    /// The images shown so far per protocol (the adoption counts).
    shown: HashSet<(u64, ImageProtocol)>,
}

static STORE: LazyLock<Mutex<Store>> = LazyLock::new(Mutex::default);

fn store() -> std::sync::MutexGuard<'static, Store> {
    STORE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// The content key of one payload.
pub(crate) fn image_key(data: &str) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    data.hash(&mut hasher);
    hasher.finish()
}

/// Keep `data` under `key` for later renders (a payload already held is only touched).
pub(crate) fn register(key: u64, data: &str) {
    if data.len() > MAX_PAYLOAD_BYTES {
        return;
    }
    let mut store = store();
    store.tick += 1;
    let tick = store.tick;
    if let Some(source) = store.sources.get_mut(&key) {
        source.used = tick;
        return;
    }
    store.source_bytes += data.len();
    store.sources.insert(
        key,
        Source {
            data: Arc::from(data),
            used: tick,
        },
    );
    while store.source_bytes > SOURCE_BUDGET_BYTES {
        let Some(oldest) = store
            .sources
            .iter()
            .filter(|(candidate, _)| **candidate != key)
            .min_by_key(|(_, source)| source.used)
            .map(|(candidate, _)| *candidate)
        else {
            break;
        };
        if let Some(source) = store.sources.remove(&oldest) {
            store.source_bytes -= source.data.len();
        }
    }
}

/// The event loop's repaint channel: a finished render sends one unit.
pub(crate) fn set_waker(waker: tokio::sync::mpsc::UnboundedSender<()>) {
    store().waker = Some(waker);
}

/// Ask the event loop for a repaint (a terminal answer changed how images draw).
pub(crate) fn wake() {
    if let Some(waker) = &store().waker {
        let _ = waker.send(());
    }
}

/// How [`lookup`] treats a rendering that is not ready.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OnMiss {
    /// Queue the render on the worker (the live frame).
    Render,
    /// Return nothing (the exit flush paints what is ready and never waits).
    Skip,
}

/// The finished rendering for `key`, queueing it on the worker on a miss when `on_miss` asks.
pub(crate) fn lookup(key: RenderKey, on_miss: OnMiss) -> Option<Arc<Rendered>> {
    let mut store = store();
    store.tick += 1;
    let tick = store.tick;
    if let Some(entry) = store.rendered.get_mut(&key) {
        entry.used = tick;
        return Some(entry.rendered.clone());
    }
    match on_miss {
        OnMiss::Render => {}
        // The half-block cells depend on the image and its box only: any protocol's rendering
        // of the same box serves a reader that never queues.
        OnMiss::Skip => {
            return store
                .rendered
                .iter()
                .find(|(candidate, _)| {
                    candidate.image == key.image && candidate.geometry == key.geometry
                })
                .map(|(_, entry)| entry.rendered.clone());
        }
    }
    if store.in_flight.contains(&key) || store.failed.contains(&key) {
        return None;
    }
    let data = store.sources.get(&key.image)?.data.clone();
    let worker = if let Some(worker) = store.worker.clone() {
        worker
    } else {
        let worker = spawn_worker()?;
        store.worker = Some(worker.clone());
        worker
    };
    if worker.send(Job { key, data }).is_ok() {
        store.in_flight.insert(key);
    } else {
        store.worker = None;
    }
    None
}

/// Count `image` as shown through `protocol` (once per image and protocol).
pub(crate) fn note_shown(image: u64, protocol: ImageProtocol) {
    store().shown.insert((image, protocol));
}

/// The images shown per protocol so far: `[kitty, iterm2, sixel, half_block]`.
pub(crate) fn shown_counts() -> [u64; 4] {
    let mut counts = [0u64; 4];
    for (_, protocol) in &store().shown {
        let slot = match protocol {
            ImageProtocol::Kitty => 0,
            ImageProtocol::Iterm2 => 1,
            ImageProtocol::Sixel => 2,
            ImageProtocol::HalfBlock => 3,
        };
        counts[slot] += 1;
    }
    counts
}

fn spawn_worker() -> Option<std::sync::mpsc::Sender<Job>> {
    let (sender, receiver) = std::sync::mpsc::channel::<Job>();
    std::thread::Builder::new()
        .name("tui-image-decode".to_string())
        .spawn(move || {
            // The last decoded source: a resize re-renders the same images at a new geometry.
            let mut last: Option<(u64, Arc<Rgba>)> = None;
            while let Ok(job) = receiver.recv() {
                let source = match &last {
                    Some((image, pixels)) if *image == job.key.image => Ok(pixels.clone()),
                    _ => decode_source(&job.data).map(Arc::new),
                };
                let rendered = source.and_then(|pixels| {
                    last = Some((job.key.image, pixels.clone()));
                    render(&pixels, job.key)
                });
                finish(job.key, rendered);
            }
        })
        .ok()?;
    Some(sender)
}

fn decode_source(data: &str) -> Result<Rgba, String> {
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(data.trim())
        .map_err(|error| error.to_string())?;
    raster::decode(&bytes)
}

/// Render one key from decoded pixels: the half-block cells, plus the protocol payload.
pub(crate) fn render(source: &Rgba, key: RenderKey) -> Result<Rendered, String> {
    let ImageGeometry { cols, rows } = key.geometry;
    let cells = raster::half_block_rows(source, cols, rows);
    let (cell_width, cell_height) = (u32::from(key.cell.0), u32::from(key.cell.1));
    let (box_width, box_height) = (
        u32::from(cols) * cell_width.max(1),
        u32::from(rows) * cell_height.max(1),
    );
    let payload = match key.protocol {
        ImageProtocol::HalfBlock => None,
        ImageProtocol::Sixel => Some(Payload::Sequence(encode::sixel(&raster::resample(
            source, box_width, box_height,
        )))),
        ImageProtocol::Kitty | ImageProtocol::Iterm2 => {
            // Never upscale (the terminal scales into the cells), never past the side cap.
            let scale = (f64::from(source.width) / f64::from(box_width))
                .max(f64::from(source.height) / f64::from(box_height))
                .max(f64::from(source.width.max(source.height)) / f64::from(MAX_PAYLOAD_SIDE));
            let (width, height) = if scale > 1.0 {
                (
                    (f64::from(source.width) / scale).round() as u32,
                    (f64::from(source.height) / scale).round() as u32,
                )
            } else {
                (source.width, source.height)
            };
            let png = raster::encode_png(&raster::resample(source, width, height))?;
            Some(match key.protocol {
                ImageProtocol::Kitty => Payload::KittyPng(png),
                ImageProtocol::Iterm2 | ImageProtocol::Sixel | ImageProtocol::HalfBlock => {
                    Payload::Sequence(encode::iterm2_inline(&png, cols, rows))
                }
            })
        }
    };
    Ok(Rendered { cells, payload })
}

fn finish(key: RenderKey, rendered: Result<Rendered, String>) {
    let mut store = store();
    store.in_flight.remove(&key);
    match rendered {
        Ok(rendered) => {
            store.tick += 1;
            let used = store.tick;
            store.rendered_bytes += rendered.bytes();
            store.rendered.insert(
                key,
                Entry {
                    rendered: Arc::new(rendered),
                    used,
                },
            );
            while store.rendered_bytes > RENDERED_BUDGET_BYTES && store.rendered.len() > 1 {
                let Some(oldest) = store
                    .rendered
                    .iter()
                    .filter(|(candidate, _)| **candidate != key)
                    .min_by_key(|(_, entry)| entry.used)
                    .map(|(candidate, _)| *candidate)
                else {
                    break;
                };
                if let Some(entry) = store.rendered.remove(&oldest) {
                    store.rendered_bytes -= entry.rendered.bytes();
                }
            }
        }
        Err(_) => {
            store.failed.insert(key);
        }
    }
    if let Some(waker) = &store.waker {
        let _ = waker.send(());
    }
}

/// Block until no render is in flight (tests drive the worker to completion).
#[cfg(test)]
pub(crate) fn wait_idle() {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while !store().in_flight.is_empty() {
        assert!(
            std::time::Instant::now() < deadline,
            "the decode worker stalled"
        );
        std::thread::yield_now();
    }
}
