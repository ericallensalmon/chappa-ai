//! What status shows but must not wait for, refreshed on a thread of its
//! own.
//!
//! - **Channel digests** for the `newer prebuilt` badge: a registry round
//!   trip (`manifest inspect`) can take over ten seconds per image, so the
//!   Environment payload only reads [`DigestCache`]; a reference it does not
//!   know yet is queued and shows no badge until the refresher has asked.
//!   Entries are refreshed after [`DIGEST_TTL`].
//! - **Engine facts** (version, OS mode, memory) for the facts section and
//!   the reachability alerts: `docker info` alone can take seconds, so the
//!   same refresher asks every [`FACTS_TTL`].
//!
//! - **Periodic work** registered with [`Background::add_periodic`]: the
//!   source's `every:` data-safety schedule looks here for due checks.
//!
//! Status registers what it wants on every call ([`Background::want_digest`],
//! [`Background::want_engine`]); the refresher picks it up within
//! [`TICK`]. Nothing here blocks a reader for longer than a lock held
//! around a map lookup.

use indexmap::IndexMap;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, Weak};
use std::thread;
use std::time::{Duration, Instant};

use stack_core::checks::{engine_facts, EngineFacts};
use stack_core::definition::{ContainerOs, EngineRef};
use stack_core::images::resolve_digest;
use stack_core::runner::PrebuiltDigests;

use crate::world::World;

/// How long a channel digest is trusted before it is asked again.
pub const DIGEST_TTL: Duration = Duration::from_secs(10 * 60);
/// How long engine facts are trusted before they are asked again.
pub const FACTS_TTL: Duration = Duration::from_secs(60);
/// How often the refresher looks for work.
pub const TICK: Duration = Duration::from_millis(500);

#[derive(Debug, Clone)]
struct DigestEntry {
    engine: EngineRef,
    os: Option<ContainerOs>,
    digest: Option<String>,
    fetched: Option<Instant>,
}

#[derive(Debug, Clone)]
struct FactsEntry {
    engine: EngineRef,
    facts: Option<EngineFacts>,
    fetched: Option<Instant>,
}

#[derive(Default)]
struct Caches {
    digests: HashMap<(String, String), DigestEntry>,
    facts: IndexMap<String, FactsEntry>,
}

/// Work the refresher does on every pass, after the caches. It must not
/// wait long: a slow task delays the next digest and facts refresh.
pub trait Periodic {
    fn tick(&self);
}

/// The shared caches and the refresher that fills them.
pub struct Background {
    caches: Mutex<Caches>,
    periodic: Mutex<Vec<Weak<dyn Periodic + Send + Sync>>>,
    wake: Condvar,
    stop: AtomicBool,
    digest_ttl: Duration,
    facts_ttl: Duration,
}

impl Background {
    pub fn new() -> Arc<Background> {
        Self::with_ttls(DIGEST_TTL, FACTS_TTL)
    }

    pub fn with_ttls(digest_ttl: Duration, facts_ttl: Duration) -> Arc<Background> {
        Arc::new(Background {
            caches: Mutex::new(Caches::default()),
            periodic: Mutex::new(Vec::new()),
            wake: Condvar::new(),
            stop: AtomicBool::new(false),
            digest_ttl,
            facts_ttl,
        })
    }

    fn caches(&self) -> std::sync::MutexGuard<'_, Caches> {
        self.caches.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Start the refresher thread; it ends after [`Background::shutdown`].
    pub fn start(self: &Arc<Self>, world: World) -> thread::JoinHandle<()> {
        let me = self.clone();
        thread::spawn(move || me.run(&world))
    }

    pub fn shutdown(&self) {
        self.stop.store(true, Ordering::SeqCst);
        self.wake.notify_all();
    }

    fn run(&self, world: &World) {
        while !self.stop.load(Ordering::SeqCst) {
            self.refresh_once(world);
            let guard = self.caches();
            let _ = self.wake.wait_timeout(guard, TICK);
        }
    }

    /// One pass: ask for every entry that is missing or past its TTL. The
    /// questions are asked with no lock held.
    pub fn refresh_once(&self, world: &World) {
        let now = Instant::now();
        let (digests, engines) = {
            let c = self.caches();
            let digests: Vec<((String, String), DigestEntry)> = c
                .digests
                .iter()
                .filter(|(_, e)| e.fetched.is_none_or(|t| now - t >= self.digest_ttl))
                .map(|(k, e)| (k.clone(), e.clone()))
                .collect();
            let engines: Vec<FactsEntry> = c
                .facts
                .values()
                .filter(|e| e.fetched.is_none_or(|t| now - t >= self.facts_ttl))
                .cloned()
                .collect();
            (digests, engines)
        };
        for (key, entry) in digests {
            if self.stop.load(Ordering::SeqCst) {
                return;
            }
            let digest =
                resolve_digest(&*world.registry, &entry.engine, &key.1, entry.os).unwrap_or(None);
            if let Some(e) = self.caches().digests.get_mut(&key) {
                e.digest = digest;
                e.fetched = Some(Instant::now());
            }
        }
        for entry in engines {
            if self.stop.load(Ordering::SeqCst) {
                return;
            }
            let facts = engine_facts(&*world.cli, &entry.engine);
            if let Some(e) = self.caches().facts.get_mut(&entry.engine.name) {
                e.facts = Some(facts);
                e.fetched = Some(Instant::now());
            }
        }
        let tasks: Vec<Arc<dyn Periodic + Send + Sync>> = {
            let mut list = self.periodic.lock().unwrap_or_else(|p| p.into_inner());
            list.retain(|w| w.strong_count() > 0);
            list.iter().filter_map(Weak::upgrade).collect()
        };
        for t in tasks {
            if self.stop.load(Ordering::SeqCst) {
                return;
            }
            t.tick();
        }
    }

    /// Run `task` on every pass while it lives (held weakly, so a dropped
    /// source stops being ticked).
    pub fn add_periodic(&self, task: Weak<dyn Periodic + Send + Sync>) {
        self.periodic
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .push(task);
    }

    /// Queue a channel reference for the refresher (no-op when known).
    pub fn want_digest(&self, engine: &EngineRef, reference: &str, os: Option<ContainerOs>) {
        let mut c = self.caches();
        let key = (engine.name.clone(), reference.to_string());
        if let std::collections::hash_map::Entry::Vacant(slot) = c.digests.entry(key) {
            slot.insert(DigestEntry {
                engine: engine.clone(),
                os,
                digest: None,
                fetched: None,
            });
            drop(c);
            self.wake.notify_all();
        }
    }

    pub fn want_engine(&self, engine: &EngineRef) {
        let mut c = self.caches();
        if !c.facts.contains_key(&engine.name) {
            c.facts.insert(
                engine.name.clone(),
                FactsEntry {
                    engine: engine.clone(),
                    facts: None,
                    fetched: None,
                },
            );
            drop(c);
            self.wake.notify_all();
        }
    }

    /// The facts known for an engine; `None` until first asked.
    pub fn engine_facts(&self, engine: &str) -> Option<EngineFacts> {
        self.caches()
            .facts
            .get(engine)
            .and_then(|e| e.facts.clone())
    }

    /// Forget the facts so the next pass asks again (after a job, which
    /// may have started or stopped an engine's containers).
    pub fn invalidate_facts(&self) {
        for e in self.caches().facts.values_mut() {
            e.fetched = None;
        }
        self.wake.notify_all();
    }
}

impl PrebuiltDigests for Background {
    /// Never waits: the cached digest, or `None` (and queued) when unknown.
    fn current_digest(
        &self,
        engine: &EngineRef,
        reference: &str,
        os: Option<ContainerOs>,
    ) -> Option<String> {
        let known = self
            .caches()
            .digests
            .get(&(engine.name.clone(), reference.to_string()))
            .map(|e| e.digest.clone());
        match known {
            Some(d) => d,
            None => {
                self.want_digest(engine, reference, os);
                None
            }
        }
    }
}
