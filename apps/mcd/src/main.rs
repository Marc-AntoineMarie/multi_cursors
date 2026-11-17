//! Démon MultiCursos.
//!
//! Deux rôles pour l'instant : `server` (la machine qui tient la souris) et `client` (celles qui
//! la reçoivent). Les sous-commandes `check` et `devices` sont des diagnostics sans effet de bord,
//! à lancer en premier sur une machine neuve.

use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use mc_node::config::{self, Config};
use mc_proto::DEFAULT_PORT;

#[derive(Parser)]
#[command(
    name = "mcd",
    about = "Une seule souris pour plusieurs ordinateurs",
    version
)]
struct Cli {
    /// Chemin du fichier de configuration.
    #[arg(long, short, global = true)]
    config: Option<PathBuf>,

    /// Verbosité : -v pour les détails, -vv pour tout.
    #[arg(long, short, global = true, action = clap::ArgAction::Count)]
    verbose: u8,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Vérifie les droits d'accès aux périphériques, sans rien modifier.
    Check,
    /// Liste les périphériques d'entrée détectés et leur classement.
    #[cfg(target_os = "linux")]
    Devices,
    /// Capture la souris et affiche ce qui est reçu, sans rien injecter.
    ///
    /// Sert à vérifier la couche capture seule : pavé tactile, plusieurs souris, molette, clics.
    /// Pendant la durée indiquée, le curseur local ne bouge plus — c'est normal, rien n'est
    /// réinjecté.
    #[cfg(target_os = "linux")]
    Monitor {
        /// Durée de la capture, en secondes.
        #[arg(long, default_value_t = 10)]
        seconds: u64,
    },
    /// Écrit une configuration de départ (géométrie détectée, jeton aléatoire).
    Init {
        /// Écrase un fichier existant.
        #[arg(long)]
        force: bool,
    },
    /// Vérifie que l'injection du curseur fonctionne : déplace le curseur en carré.
    ///
    /// Ne demande aucun droit de lecture des périphériques, donc utilisable avant la mise en place
    /// du groupe « input ». Aucun clic n'est émis.
    Selftest {
        /// Largeur du bureau en pixels (détectée si absente).
        #[arg(long)]
        width: Option<i32>,
        /// Hauteur du bureau en pixels (détectée si absente).
        #[arg(long)]
        height: Option<i32>,
        /// Attente avant le premier déplacement, en millisecondes.
        ///
        /// Laisse au compositeur le temps de découvrir le périphérique — et à l'utilisateur celui
        /// de regarder son écran.
        #[arg(long, default_value_t = 900)]
        delay_ms: u64,
    },
    /// Tient le rôle serveur : capture la souris locale et la distribue.
    ///
    /// Suppose de capturer les périphériques locaux, ce qui n'est implémenté que sous Linux pour
    /// l'instant (étape 5 de la feuille de route pour Windows et macOS).
    #[cfg(target_os = "linux")]
    Server,
    /// Tient le rôle client : reçoit le curseur d'un serveur.
    Client {
        /// Adresse du serveur, `hôte` ou `hôte:port`.
        #[arg(long, short = 'C')]
        connect: String,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    let level = match cli.verbose {
        0 => "info",
        1 => "debug",
        _ => "trace",
    };
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(level)),
        )
        .with_target(false)
        .init();

    let config_path = cli
        .config
        .clone()
        .unwrap_or_else(config::default_config_path);

    match cli.command {
        Command::Check => check(&config_path),
        #[cfg(target_os = "linux")]
        Command::Devices => devices(),
        Command::Init { force } => init(&config_path, force),
        Command::Selftest {
            width,
            height,
            delay_ms,
        } => selftest(width, height, delay_ms),
        #[cfg(target_os = "linux")]
        Command::Monitor { seconds } => run_async(monitor(&config_path, seconds)),
        #[cfg(target_os = "linux")]
        Command::Server => run_async(serve(&config_path)),
        Command::Client { connect } => run_async(connect_to(&config_path, connect)),
    }
}

/// Un exécuteur créé ici plutôt qu'avec `#[tokio::main]` : les sous-commandes de diagnostic sont
/// synchrones et n'ont aucune raison de démarrer un ordonnanceur.
fn run_async<F: std::future::Future<Output = Result<()>>>(future: F) -> Result<()> {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("démarrage de l'exécuteur asynchrone")?
        .block_on(future)
}

fn check(config_path: &PathBuf) -> Result<()> {
    let mut perms = mc_input::check_permissions();
    check_config(config_path, &mut perms);

    println!("Droits et capacités de cette machine\n");
    for item in &perms.items {
        println!("  [{}] {}", if item.ok { "ok " } else { "KO " }, item.label);
        if let Some(detail) = &item.detail {
            println!("        {detail}");
        }
    }

    if perms.ok() {
        println!("\nTout est en place.");
        if !perms.remediation.is_empty() {
            println!("\nÀ savoir :");
            for note in &perms.remediation {
                println!("  - {note}");
            }
        }
        return Ok(());
    }

    println!("\nÉtapes à suivre sur cette machine :");
    for (i, step) in perms.remediation.iter().enumerate() {
        println!("  {}. {step}", i + 1);
    }
    println!("\nL'injection du curseur, elle, peut déjà être testée : `mcd selftest`.");
    anyhow::bail!("droits insuffisants");
}

/// Ajoute au diagnostic l'état du fichier de configuration.
///
/// Une erreur de syntaxe ou un `name` qui ne correspond à aucun écran ne se voient qu'au
/// lancement, au pire moment. Autant les signaler ici, avec le reste.
fn check_config(path: &PathBuf, perms: &mut mc_input::Permissions) {
    if !path.exists() {
        perms.items.push(mc_input::CheckItem {
            label: "fichier de configuration".into(),
            ok: false,
            detail: Some(format!("absent : {} — lancer `mcd init`", path.display())),
        });
        perms
            .remediation
            .push("créer la configuration : `mcd init`".into());
        return;
    }

    match Config::load(path) {
        Ok(config) => {
            let noms: Vec<&str> = config.screens.iter().map(|s| s.name.as_str()).collect();
            let own = noms.contains(&config.name.as_str());
            perms.items.push(mc_input::CheckItem {
                label: "fichier de configuration".into(),
                ok: true,
                detail: Some(format!(
                    "{} — machine « {} », écrans : {}",
                    path.display(),
                    config.name,
                    noms.join(", ")
                )),
            });
            perms.items.push(mc_input::CheckItem {
                label: "cette machine figure dans la disposition".into(),
                ok: own,
                detail: (!own).then(|| {
                    format!(
                        "« {} » n'est aucun des écrans déclarés : le rôle serveur ne saurait pas \
                         où est son propre écran",
                        config.name
                    )
                }),
            });
            if !own {
                perms.remediation.push(format!(
                    "ajouter un bloc [[screen]] nommé « {} », ou corriger le champ `name`",
                    config.name
                ));
            }
        }
        Err(err) => {
            perms.items.push(mc_input::CheckItem {
                label: "fichier de configuration".into(),
                ok: false,
                detail: Some(format!("{err:#}")),
            });
            perms
                .remediation
                .push(format!("corriger {}", path.display()));
        }
    }
}

#[cfg(target_os = "linux")]
fn oui_non(v: bool) -> &'static str {
    if v {
        "oui"
    } else {
        "NON"
    }
}

#[cfg(target_os = "linux")]
fn devices() -> Result<()> {
    let all = mc_input::sys_linux::list_devices().context("énumération de /dev/input")?;
    println!("{:<22} {:<10} {:<9} {:<8} nom", "chemin", "souris", "pavé", "clavier");
    for d in &all {
        if !(d.relative_pointer || d.touchpad || d.keyboard) {
            continue;
        }
        println!(
            "{:<22} {:<10} {:<9} {:<8} {}",
            d.path.display(),
            oui_non(d.relative_pointer),
            oui_non(d.touchpad),
            oui_non(d.keyboard),
            d.name
        );
    }

    let capturable = mc_input::list_capturable()?;
    println!("\n{} périphérique(s) seront capturés en mode serveur.", capturable.len());
    if capturable.iter().all(|d| !d.relative_pointer) && !capturable.is_empty() {
        println!(
            "Aucune souris relative : seul un pavé tactile sera utilisé. Le déplacement, le clic,\n\
             la tape et le défilement à deux doigts fonctionnent ; les gestes plus riches non\n\
             (voir README, section « pavé tactile »)."
        );
    }
    Ok(())
}

/// Déplace le curseur selon un carré, pour vérifier que l'injection fonctionne vraiment.
///
/// Sous Linux, c'est le test de l'hypothèse centrale du projet : un périphérique uinput *absolu*
/// portant `INPUT_PROP_POINTER` est accepté comme souris, et le compositeur place le curseur
/// exactement aux coordonnées demandées — sous Wayland comme sous X11. Sous Windows, cela vérifie
/// que `SendInput` n'est pas filtré. À lancer sur chaque nouvelle machine avant tout le reste.
fn selftest(width: Option<i32>, height: Option<i32>, delay_ms: u64) -> Result<()> {
    let (w, h) = match (width, height) {
        (Some(w), Some(h)) => (w, h),
        _ => {
            let detected = mc_node::config::detect_geometry();
            println!(
                "Géométrie détectée : {}x{} (forçable avec --width/--height)",
                detected.0, detected.1
            );
            detected
        }
    };

    let mut sink = mc_input::create_pointer_sink("MultiCursos Selftest Pointer", w, h)
        .context("initialisation de l'injection du curseur")?;

    // Le compositeur doit d'abord découvrir le nouveau périphérique : les déplacements envoyés
    // avant cette découverte seraient perdus.
    println!("Injection prête. Démarrage dans {delay_ms} ms — regarder l'écran.");
    std::thread::sleep(std::time::Duration::from_millis(delay_ms));

    let (cx, cy) = (w / 2, h / 2);
    let (dx, dy) = (w / 4, h / 4);
    let parcours = [
        (cx, cy),
        (cx - dx, cy - dy),
        (cx + dx, cy - dy),
        (cx + dx, cy + dy),
        (cx - dx, cy + dy),
        (cx, cy),
    ];

    println!("Déplacement du curseur en carré — aucun clic n'est émis.");
    for (x, y) in parcours {
        sink.move_to(x, y).context("déplacement du curseur")?;
        println!("  → {x},{y}");
        std::thread::sleep(std::time::Duration::from_millis(450));
    }

    println!(
        "\nTerminé. Si le curseur a suivi le carré et fini au centre, l'injection fonctionne sur\n\
         cette machine : elle peut tenir le rôle client. Sinon, relancer avec `-v` et regarder le\n\
         journal."
    );
    Ok(())
}

fn init(path: &PathBuf, force: bool) -> Result<()> {
    if path.exists() && !force {
        anyhow::bail!(
            "{} existe déjà — relancer avec --force pour l'écraser",
            path.display()
        );
    }
    let config = Config::generate_default();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("création de {}", parent.display()))?;
    }
    std::fs::write(path, config.to_toml())
        .with_context(|| format!("écriture de {}", path.display()))?;

    println!("Configuration écrite dans {}", path.display());
    println!(
        "\nMachine : « {} », écran détecté {}x{}.\n\
         Jeton   : {}\n\
         \n\
         À faire maintenant :\n\
         \x20 1. copier ce jeton dans la configuration des autres machines ;\n\
         \x20 2. ajouter un bloc [[screen]] par machine, avec sa position en pixels ;\n\
         \x20 3. lancer `mcd check` sur chacune.",
        config.name,
        config.screens[0].width,
        config.screens[0].height,
        config.token
    );
    Ok(())
}

/// Affiche en direct ce que la capture reçoit, sans rien injecter.
///
/// C'est le maillon qu'aucun autre diagnostic ne couvre : `selftest` valide l'injection, `check`
/// les droits, mais rien ne disait si les déplacements arrivent vraiment, ni s'ils arrivent
/// correctement — ce qui compte surtout pour la conversion du pavé tactile, écrite à la main.
#[cfg(target_os = "linux")]
async fn monitor(path: &PathBuf, seconds: u64) -> Result<()> {
    use mc_input::RawEvent;

    // La configuration ne sert ici qu'aux réglages de sensibilité ; son absence n'est pas un
    // obstacle sur une machine qu'on est en train de mettre en route.
    let tuning = match Config::load(path) {
        Ok(config) => config.tuning.to_tuning(),
        Err(_) => {
            println!("(configuration absente ou illisible : réglages par défaut)");
            mc_input::Tuning::default()
        }
    };

    let (width, height) = mc_node::config::detect_geometry();
    let mut capture = mc_input::Capture::start(tuning)?;

    println!(
        "\nCapture active pendant {seconds} s.\n\
         Bouge la souris, clique, utilise la molette, essaie le pavé tactile.\n\
         Le curseur ne bougera pas à l'écran : rien n'est réinjecté, c'est voulu.\n\
         Ctrl-C pour arrêter plus tôt.\n"
    );

    let started = std::time::Instant::now();
    let limit = std::time::Duration::from_secs(seconds);
    let mut ticker = tokio::time::interval(std::time::Duration::from_millis(100));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    // Position simulée, uniquement pour rendre les déplacements lisibles.
    let (mut x, mut y) = (width / 2, height / 2);
    let (mut acc_x, mut acc_y) = (0i32, 0i32);
    let (mut moves, mut clicks, mut scrolls) = (0u64, 0u64, 0u64);

    loop {
        tokio::select! {
            event = capture.recv() => {
                match event {
                    Some(RawEvent::Motion { dx, dy }) => {
                        acc_x += dx;
                        acc_y += dy;
                        x = (x + dx).clamp(0, width - 1);
                        y = (y + dy).clamp(0, height - 1);
                        moves += 1;
                    }
                    Some(RawEvent::Button { button, pressed }) => {
                        clicks += u64::from(pressed);
                        println!(
                            "  {:>8}  {button:?}",
                            if pressed { "appui" } else { "relâché" }
                        );
                    }
                    Some(RawEvent::Scroll { x: sx, y: sy }) => {
                        scrolls += 1;
                        println!("  {:>8}  horizontal {sx:+}, vertical {sy:+}", "molette");
                    }
                    None => anyhow::bail!("la capture s'est arrêtée"),
                }
            }
            _ = ticker.tick() => {
                if acc_x != 0 || acc_y != 0 {
                    println!(
                        "  {:>8}  {acc_x:+5},{acc_y:+5}  ->  position simulée {x},{y}",
                        "déplace"
                    );
                    acc_x = 0;
                    acc_y = 0;
                }
                if started.elapsed() >= limit {
                    break;
                }
            }
            _ = tokio::signal::ctrl_c() => break,
        }
    }

    println!(
        "\n{moves} trame(s) de déplacement, {clicks} clic(s), {scrolls} cran(s) de molette.\n"
    );
    if moves == 0 && clicks == 0 {
        println!(
            "Rien n'est arrivé. Pistes, dans l'ordre :\n\
             \x20 - un autre programme détient-il déjà la souris (Deskflow, Synergy, Barrier) ?\n\
             \x20 - `mcd devices` liste-t-il le bon périphérique ?\n\
             \x20 - sur un portable sans souris branchée, seul le pavé tactile est en jeu, et sa\n\
             \x20   conversion est la partie la plus fragile : relancer avec -v."
        );
    } else {
        println!("La capture fonctionne : les déplacements arrivent au moteur.");
    }
    Ok(())
}

#[cfg(target_os = "linux")]
async fn serve(path: &PathBuf) -> Result<()> {
    let config = Config::load(path).with_context(|| {
        format!(
            "configuration illisible ({}). Lancer `mcd init` pour en créer une",
            path.display()
        )
    })?;

    tokio::select! {
        result = mc_node::server::run(config) => result,
        _ = tokio::signal::ctrl_c() => {
            // En quittant, la capture est libérée par le noyau à la fermeture des descripteurs :
            // la souris revient au système même si quelque chose s'est mal passé.
            tracing::info!("arrêt demandé, périphériques relâchés");
            Ok(())
        }
    }
}

async fn connect_to(path: &PathBuf, connect: String) -> Result<()> {
    let config = Config::load(path).with_context(|| {
        format!(
            "configuration illisible ({}). Lancer `mcd init` pour en créer une",
            path.display()
        )
    })?;
    let server = with_default_port(&connect);

    tokio::select! {
        result = mc_node::client::run(config, server) => result,
        _ = tokio::signal::ctrl_c() => {
            tracing::info!("arrêt demandé");
            Ok(())
        }
    }
}

/// Complète une adresse sans port. Les adresses IPv6 littérales s'écrivent `[::1]:24810`.
fn with_default_port(address: &str) -> String {
    let has_port = if address.starts_with('[') {
        address.rsplit_once(']').is_some_and(|(_, rest)| rest.starts_with(':'))
    } else {
        address.matches(':').count() == 1
    };
    if has_port {
        address.to_string()
    } else {
        format!("{address}:{DEFAULT_PORT}")
    }
}

#[cfg(test)]
mod tests {
    use super::with_default_port;

    #[test]
    fn port_par_defaut_ajoute() {
        assert_eq!(with_default_port("192.0.2.10"), "192.0.2.10:24810");
        assert_eq!(with_default_port("portable"), "portable:24810");
    }

    #[test]
    fn port_explicite_conserve() {
        assert_eq!(with_default_port("192.0.2.10:9999"), "192.0.2.10:9999");
        assert_eq!(with_default_port("[::1]:9999"), "[::1]:9999");
    }

    #[test]
    fn ipv6_sans_port() {
        // Plusieurs deux-points sans crochets : c'est une IPv6 nue, il faut ajouter le port.
        assert_eq!(with_default_port("[fe80::1]"), "[fe80::1]:24810");
    }
}
