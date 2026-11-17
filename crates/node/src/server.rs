//! Rôle serveur : la machine qui possède la souris physique.
//!
//! Boucle de décision, pour chaque événement capturé :
//!
//! 1. appliquer le déplacement à la position globale (`mc-layout`) ;
//! 2. si l'écran actif change, prévenir l'ancien propriétaire (`Leave`) puis le nouveau (`Enter`) ;
//! 3. router l'événement vers le pointeur virtuel local ou vers la machine distante.
//!
//! Garde-fou important : seules les machines **connectées** font partie de la disposition active.
//! Sans ça, le curseur franchirait le bord vers une machine éteinte et deviendrait introuvable.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use mc_input::{Capture, PointerSink, RawEvent, UinputPointer};
use mc_layout::{Motion, Point, ScreenId, VirtualDesktop};
use mc_proto::codec::{read_msg, write_msg};
use mc_proto::{Msg, Refusal, PROTOCOL_VERSION};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc::{self, UnboundedSender};

use crate::config::Config;

/// Durée de maintien des deux boutons pour relâcher la souris (échappatoire matérielle).
const ESCAPE_HOLD: Duration = Duration::from_secs(2);
const PING_INTERVAL: Duration = Duration::from_secs(2);

/// Nom du pointeur virtuel. Le préfixe « MultiCursos » est ce qui évite qu'on se capture
/// soi-même : la capture ignore tout périphérique portant ce préfixe.
const SINK_NAME: &str = "MultiCursos Virtual Pointer";

struct ClientEntry {
    name: String,
    tx: UnboundedSender<Msg>,
    rtt_micros: Arc<AtomicU64>,
}

/// Annuaire des machines connectées.
///
/// `generation` s'incrémente à chaque connexion ou déconnexion : le moteur la relit à chaque
/// événement (une lecture atomique, négligeable) et ne recalcule la disposition active que
/// lorsqu'elle a changé.
struct Clients {
    inner: Mutex<HashMap<ScreenId, ClientEntry>>,
    generation: AtomicU64,
}

impl Clients {
    fn new() -> Self {
        Self {
            inner: Mutex::new(HashMap::new()),
            generation: AtomicU64::new(1),
        }
    }

    /// Enregistre une machine, sauf si son écran est déjà pris.
    ///
    /// Vérification et insertion sous le même verrou : deux machines annonçant le même nom en même
    /// temps ne peuvent pas se retrouver toutes les deux acceptées.
    fn try_insert(&self, id: ScreenId, entry: ClientEntry) -> bool {
        let mut guard = self.inner.lock().expect("annuaire empoisonné");
        if guard.contains_key(&id) {
            return false;
        }
        guard.insert(id, entry);
        drop(guard);
        self.generation.fetch_add(1, Ordering::SeqCst);
        true
    }

    fn remove(&self, id: ScreenId) {
        self.inner.lock().expect("annuaire empoisonné").remove(&id);
        self.generation.fetch_add(1, Ordering::SeqCst);
    }

    fn ids(&self) -> Vec<ScreenId> {
        self.inner.lock().expect("annuaire empoisonné").keys().copied().collect()
    }

    /// Nom et latence de chaque machine connectée. Journalisé périodiquement, et c'est aussi ce
    /// que l'interface de l'étape 2 affichera.
    fn status(&self) -> Vec<(String, f64)> {
        self.inner
            .lock()
            .expect("annuaire empoisonné")
            .values()
            .map(|c| {
                (
                    c.name.clone(),
                    c.rtt_micros.load(Ordering::Relaxed) as f64 / 1000.0,
                )
            })
            .collect()
    }

    fn send(&self, id: ScreenId, msg: Msg) {
        if let Some(entry) = self.inner.lock().expect("annuaire empoisonné").get(&id) {
            // Canal non borné : l'envoi n'attend jamais, donc la boucle de capture n'est jamais
            // ralentie par une machine distante lente ou injoignable.
            let _ = entry.tx.send(msg);
        }
    }

    fn generation(&self) -> u64 {
        self.generation.load(Ordering::SeqCst)
    }
}

pub async fn run(config: Config) -> Result<()> {
    let desktop = config.virtual_desktop();
    let local = desktop
        .by_name(&config.name)
        .with_context(|| {
            format!(
                "aucun [[screen]] ne porte le nom de cette machine (« {} ») : \
                 le serveur ne sait pas où se trouve son propre écran",
                config.name
            )
        })?
        .clone();

    anyhow::ensure!(
        desktop.screens().len() >= 2,
        "la disposition ne contient qu'un écran : ajouter au moins une autre machine \
         dans la configuration"
    );

    let mut sink = UinputPointer::new(SINK_NAME, local.width, local.height)
        .context("création du pointeur virtuel local")?;
    let capture = Capture::start(config.tuning.to_tuning())?;
    let control = capture.control();

    let clients = Arc::new(Clients::new());
    let listener = TcpListener::bind(&config.listen)
        .await
        .with_context(|| format!("écoute sur {}", config.listen))?;
    tracing::info!(
        "serveur « {} » à l'écoute sur {} — écran local {}x{}",
        config.name,
        config.listen,
        local.width,
        local.height
    );
    for screen in desktop.screens() {
        if screen.id != local.id {
            tracing::info!(
                "en attente de « {} » ({}x{} en {},{})",
                screen.name,
                screen.width,
                screen.height,
                screen.origin.x,
                screen.origin.y
            );
        }
    }

    {
        let clients = Arc::clone(&clients);
        let desktop = desktop.clone();
        let config = config.clone();
        tokio::spawn(async move {
            loop {
                match listener.accept().await {
                    Ok((stream, addr)) => {
                        let clients = Arc::clone(&clients);
                        let desktop = desktop.clone();
                        let config = config.clone();
                        tokio::spawn(async move {
                            if let Err(err) = handle_client(stream, &config, &desktop, &clients).await {
                                tracing::info!("connexion {addr} terminée : {err:#}");
                            }
                        });
                    }
                    Err(err) => {
                        tracing::error!("accept impossible : {err}");
                        tokio::time::sleep(Duration::from_millis(500)).await;
                    }
                }
            }
        });
    }

    // Position de départ : au centre de l'écran local, et on y place réellement le curseur. C'est
    // le point de synchronisation entre notre position virtuelle et celle du compositeur.
    let mut engine = Engine {
        local_id: local.id,
        current: local.id,
        pos: Point::new(local.width / 2, local.height / 2),
        active: VirtualDesktop::new(vec![local.clone()]),
        full: desktop,
        seen_generation: 0,
        clients: Arc::clone(&clients),
        left_down: false,
        right_down: false,
        both_since: None,
        bypassed: false,
    };
    sink.move_to(engine.pos.x, engine.pos.y)
        .context("placement initial du curseur")?;

    engine.run(capture, sink, control).await
}

struct Engine {
    local_id: ScreenId,
    current: ScreenId,
    /// Position du curseur dans le plan global.
    pos: Point,
    /// Disposition restreinte aux machines réellement joignables.
    active: VirtualDesktop,
    full: VirtualDesktop,
    seen_generation: u64,
    clients: Arc<Clients>,
    left_down: bool,
    right_down: bool,
    both_since: Option<Instant>,
    bypassed: bool,
}

impl Engine {
    async fn run(
        &mut self,
        mut capture: Capture,
        mut sink: UinputPointer,
        control: Arc<mc_input::CaptureControl>,
    ) -> Result<()> {
        let mut ticks: u64 = 0;
        let mut ticker = tokio::time::interval(Duration::from_millis(200));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

        loop {
            tokio::select! {
                event = capture.recv() => {
                    let Some(event) = event else {
                        anyhow::bail!("la capture s'est arrêtée");
                    };
                    self.refresh_active(&mut sink)?;
                    self.handle(event, &mut sink, &control)?;
                }
                _ = ticker.tick() => {
                    // Le maintien des deux boutons se mesure dans le temps : il faut un réveil
                    // régulier, l'absence d'événement ne doit pas empêcher l'échappatoire.
                    self.check_escape(&mut sink, &control)?;

                    ticks = ticks.wrapping_add(1);
                    if ticks % 50 == 0 {
                        for (name, rtt) in self.clients.status() {
                            tracing::debug!("« {name} » : {rtt:.2} ms");
                        }
                    }
                }
            }
        }
    }

    /// Recalcule la disposition active si l'ensemble des machines connectées a changé.
    fn refresh_active(&mut self, sink: &mut UinputPointer) -> Result<()> {
        let generation = self.clients.generation();
        if generation == self.seen_generation {
            return Ok(());
        }
        self.seen_generation = generation;

        let connected = self.clients.ids();
        let screens: Vec<_> = self
            .full
            .screens()
            .iter()
            .filter(|s| s.id == self.local_id || connected.contains(&s.id))
            .cloned()
            .collect();
        self.active = VirtualDesktop::new(screens);

        if self.current != self.local_id && self.active.get(self.current).is_none() {
            // La machine qui tenait le curseur a disparu : on le ramène ici plutôt que de le
            // laisser dans le vide.
            tracing::warn!("la machine active s'est déconnectée, retour au curseur local");
            self.current = self.local_id;
            let local = self.active.get(self.local_id).expect("écran local absent");
            self.pos = local.clamp(self.pos);
            let local_pos = local.to_local(self.pos);
            sink.move_to(local_pos.x, local_pos.y)?;
        }
        Ok(())
    }

    fn handle(
        &mut self,
        event: RawEvent,
        sink: &mut UinputPointer,
        control: &mc_input::CaptureControl,
    ) -> Result<()> {
        match event {
            RawEvent::Button { button, pressed } => {
                self.track_escape_gesture(button, pressed);
                self.check_escape(sink, control)?;
                if self.bypassed {
                    return Ok(());
                }
                if self.current == self.local_id {
                    sink.button(button, pressed)?;
                } else {
                    self.clients
                        .send(self.current, Msg::Button { button, pressed });
                }
            }
            _ if self.bypassed => {}
            RawEvent::Motion { dx, dy } => self.motion(dx, dy, sink)?,
            RawEvent::Scroll { x, y } => {
                if self.current == self.local_id {
                    sink.scroll(x, y)?;
                } else {
                    self.clients.send(self.current, Msg::Scroll { x, y });
                }
            }
        }
        Ok(())
    }

    fn motion(&mut self, dx: i32, dy: i32, sink: &mut UinputPointer) -> Result<()> {
        let motion = self.active.apply(self.current, self.pos, dx, dy);
        self.pos = motion.pos();

        if let Motion::Cross { from, to, .. } = motion {
            let target_name = self
                .active
                .get(to)
                .map(|s| s.name.clone())
                .unwrap_or_default();
            tracing::debug!("bascule de l'écran {from} vers « {target_name} »");

            // On quitte l'écran précédent : relâcher ses boutons évite un clic resté enfoncé.
            if from == self.local_id {
                sink.release_all()?;
            } else {
                self.clients.send(from, Msg::Leave);
            }

            self.current = to;
            let entry = self
                .active
                .get(to)
                .map(|s| s.to_local(self.pos))
                .unwrap_or(self.pos);
            if to == self.local_id {
                sink.move_to(entry.x, entry.y)?;
            } else {
                self.clients.send(to, Msg::Enter { x: entry.x, y: entry.y });
            }
            return Ok(());
        }

        let local_pos = self
            .active
            .get(self.current)
            .map(|s| s.to_local(self.pos))
            .unwrap_or(self.pos);
        if self.current == self.local_id {
            sink.move_to(local_pos.x, local_pos.y)?;
        } else {
            self.clients.send(
                self.current,
                Msg::MoveTo {
                    x: local_pos.x,
                    y: local_pos.y,
                },
            );
        }
        Ok(())
    }

    fn track_escape_gesture(&mut self, button: mc_proto::Button, pressed: bool) {
        match button {
            mc_proto::Button::Left => self.left_down = pressed,
            mc_proto::Button::Right => self.right_down = pressed,
            _ => {}
        }
        if self.left_down && self.right_down {
            self.both_since.get_or_insert_with(Instant::now);
        } else {
            self.both_since = None;
        }
    }

    /// Bascule entre capture et relâchement quand les deux boutons sont maintenus.
    ///
    /// C'est le filet de secours utilisable sans écran ni clavier : si la bascule d'écrans se
    /// comporte mal, maintenir les deux boutons rend la souris au système d'exploitation local.
    fn check_escape(
        &mut self,
        sink: &mut UinputPointer,
        control: &mc_input::CaptureControl,
    ) -> Result<()> {
        let Some(since) = self.both_since else {
            return Ok(());
        };
        if since.elapsed() < ESCAPE_HOLD {
            return Ok(());
        }
        self.both_since = None;
        self.bypassed = !self.bypassed;

        if self.bypassed {
            tracing::warn!(
                "échappatoire : souris rendue au système local. \
                 Maintenir à nouveau les deux boutons pour reprendre la capture"
            );
            if self.current != self.local_id {
                self.clients.send(self.current, Msg::Leave);
                self.current = self.local_id;
            }
            sink.release_all()?;
            control.set_grabbed(false);
        } else {
            tracing::info!("capture reprise");
            control.set_grabbed(true);
            if let Some(local) = self.active.get(self.local_id) {
                self.pos = local.clamp(self.pos);
                let local_pos = local.to_local(self.pos);
                sink.move_to(local_pos.x, local_pos.y)?;
            }
        }
        Ok(())
    }
}

async fn handle_client(
    stream: TcpStream,
    config: &Config,
    desktop: &VirtualDesktop,
    clients: &Arc<Clients>,
) -> Result<()> {
    // Sans `TCP_NODELAY`, Nagle regrouperait les déplacements : jusqu'à 40 ms de retard sur un
    // curseur, immédiatement perceptible.
    stream.set_nodelay(true).context("TCP_NODELAY")?;
    let peer = stream.peer_addr().ok();
    let (mut reader, mut writer) = stream.into_split();

    let hello = match read_msg(&mut reader).await? {
        Msg::Hello(hello) => hello,
        other => anyhow::bail!("premier message inattendu : {other:?}"),
    };

    let refusal = if hello.protocol != PROTOCOL_VERSION {
        Some(Refusal::ProtocolMismatch {
            server: PROTOCOL_VERSION,
        })
    } else if hello.token != config.token {
        Some(Refusal::BadToken)
    } else {
        match desktop.by_name(&hello.name) {
            None => Some(Refusal::UnknownMachine {
                name: hello.name.clone(),
            }),
            Some(_) => None,
        }
    };

    if let Some(refusal) = refusal {
        tracing::warn!("connexion refusée ({peer:?}) : {refusal}");
        let _ = write_msg(&mut writer, &Msg::Refused(refusal.clone())).await;
        anyhow::bail!("{refusal}");
    }

    let screen = desktop.by_name(&hello.name).expect("écran validé plus haut");
    if screen.width != hello.geometry.width || screen.height != hello.geometry.height {
        tracing::warn!(
            "« {} » annonce {}x{} alors que la disposition prévoit {}x{} : \
             le curseur n'entrera pas au bon endroit, corriger la configuration",
            hello.name,
            hello.geometry.width,
            hello.geometry.height,
            screen.width,
            screen.height
        );
    }

    let (tx, mut rx) = mpsc::unbounded_channel::<Msg>();
    let rtt_micros = Arc::new(AtomicU64::new(0));
    if !clients.try_insert(
        screen.id,
        ClientEntry {
            name: hello.name.clone(),
            tx: tx.clone(),
            rtt_micros: Arc::clone(&rtt_micros),
        },
    ) {
        let refusal = Refusal::AlreadyConnected {
            name: hello.name.clone(),
        };
        tracing::warn!("connexion refusée ({peer:?}) : {refusal}");
        let _ = write_msg(&mut writer, &Msg::Refused(refusal.clone())).await;
        anyhow::bail!("{refusal}");
    }
    tracing::info!(
        "« {} » connectée depuis {peer:?} — écran {} ({}x{})",
        hello.name,
        screen.id,
        screen.width,
        screen.height
    );

    write_msg(
        &mut writer,
        &Msg::Welcome {
            screen: screen.id,
            server_name: config.name.clone(),
        },
    )
    .await?;

    // Fil d'écriture : la boucle de capture dépose dans le canal et repart aussitôt.
    let writer_task = tokio::spawn(async move {
        while let Some(msg) = rx.recv().await {
            if write_msg(&mut writer, &msg).await.is_err() {
                break;
            }
        }
    });

    // Sonde de latence : une tâche à part, pas une branche de `select!`, pour ne jamais
    // interrompre une lecture en cours (voir `client_read_loop`).
    let started = Instant::now();
    let ping_tx = tx.clone();
    let ping_task = tokio::spawn(async move {
        let mut ticker = tokio::time::interval(PING_INTERVAL);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            ticker.tick().await;
            let nonce = started.elapsed().as_micros() as u64;
            if ping_tx.send(Msg::Ping { nonce }).is_err() {
                break;
            }
        }
    });

    let result = client_read_loop(&mut reader, &rtt_micros, started).await;

    clients.remove(screen.id);
    ping_task.abort();
    writer_task.abort();
    tracing::info!("« {} » déconnectée", hello.name);
    result
}

/// Lecture des messages d'un client.
///
/// Volontairement **sans `select!`** : `read_msg` lit d'abord 4 octets de longueur puis la charge
/// utile, et n'est donc pas sûr à l'annulation. Si une autre branche gagnait la course au milieu
/// d'un message, les octets déjà lus seraient perdus et le flux resterait désynchronisé pour
/// toujours. L'émission périodique des `Ping` passe par le canal d'écriture, depuis une tâche
/// séparée.
async fn client_read_loop(
    reader: &mut tokio::net::tcp::OwnedReadHalf,
    rtt_micros: &AtomicU64,
    started: Instant,
) -> Result<()> {
    loop {
        match read_msg(reader).await? {
            Msg::Pong { nonce } => {
                let now = started.elapsed().as_micros() as u64;
                let rtt = now.saturating_sub(nonce);
                rtt_micros.store(rtt, Ordering::Relaxed);
                tracing::debug!("latence aller-retour : {:.2} ms", rtt as f64 / 1000.0);
            }
            Msg::Bye => return Ok(()),
            other => tracing::debug!("message client ignoré : {other:?}"),
        }
    }
}
