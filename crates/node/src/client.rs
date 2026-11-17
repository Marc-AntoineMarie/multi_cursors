//! Rôle client : la machine qui reçoit le curseur.
//!
//! Elle ne connaît ni le plan global ni les autres machines. Elle reçoit des positions absolues
//! déjà exprimées dans ses propres pixels et les injecte. Toute l'intelligence de bascule reste
//! chez le serveur, ce qui rend ce rôle trivial à porter sur Windows et macOS : il suffit d'une
//! autre implémentation de `PointerSink`.

use std::time::Duration;

use anyhow::{Context, Result};
use mc_input::PointerSink;
use mc_proto::codec::{read_msg, write_msg};
use mc_proto::{Hello, Msg, Os, Refusal, PROTOCOL_VERSION};
use tokio::net::TcpStream;

use crate::config::Config;

const SINK_NAME: &str = "MultiCursos Client Pointer";
const RETRY_MIN: Duration = Duration::from_millis(500);
const RETRY_MAX: Duration = Duration::from_secs(10);

/// Ce qui a mis fin à une session.
#[derive(Debug)]
enum Flow {
    /// Fin normale ou coupure réseau : on retentera.
    Ended,
    /// Refus définitif : réessayer ne servirait à rien tant que la configuration n'est pas
    /// corrigée, autant le dire clairement et s'arrêter.
    Fatal(Refusal),
}

pub async fn run(config: Config, server: String) -> Result<()> {
    // On annonce la taille **réelle** du bureau quand la plateforme sait la donner, et non celle
    // écrite dans la configuration. Deux bénéfices : l'injection est à la bonne échelle, et le
    // serveur détecte de lui-même une disposition mal déclarée (il compare les deux et le dit).
    // Repli sur la configuration pour une machine sans écran, par exemple un NAS.
    let declared = config.own_geometry();
    let geometry = match mc_input::desktop_geometry() {
        Some((width, height)) => {
            if width != declared.width || height != declared.height {
                tracing::warn!(
                    "la configuration annonce {}x{} mais le bureau mesure {width}x{height} : \
                     c'est cette dernière valeur qui est transmise au serveur, et c'est la \
                     configuration des DEUX machines qu'il faut corriger",
                    declared.width,
                    declared.height
                );
            }
            mc_proto::Geometry { width, height }
        }
        None => {
            tracing::info!("taille du bureau non détectable, valeur de la configuration utilisée");
            declared
        }
    };
    tracing::info!(
        "client « {} » — bureau annoncé {}x{}",
        config.name,
        geometry.width,
        geometry.height
    );

    // Une seule sortie pointeur pour toute la durée de vie du processus : la recréer à chaque
    // reconnexion ferait réapparaître un nouveau périphérique côté compositeur à chaque fois.
    let mut sink = mc_input::create_pointer_sink(SINK_NAME, geometry.width, geometry.height)
        .context("initialisation de l'injection du curseur")?;

    let mut delay = RETRY_MIN;
    loop {
        match session(&config, &server, geometry, sink.as_mut()).await {
            Ok(Flow::Fatal(refusal)) => {
                anyhow::bail!("le serveur a refusé la connexion : {refusal}");
            }
            Ok(Flow::Ended) => {
                tracing::info!("session terminée, reconnexion dans {delay:?}");
            }
            Err(err) => {
                tracing::warn!("connexion à {server} impossible : {err:#}");
            }
        }
        // Tout bouton resté enfoncé au moment de la coupure doit être relâché, sinon la machine
        // garde un clic bloqué jusqu'au retour du serveur.
        let _ = sink.release_all();

        tokio::time::sleep(delay).await;
        delay = (delay * 2).min(RETRY_MAX);
    }
}

/// Une session, de la connexion à sa fin.
///
/// Prend un `&mut dyn PointerSink` et non la sortie concrète : c'est ce qui permet de tester tout
/// le dialogue réseau avec une fausse sortie, sans périphérique ni privilège.
async fn session(
    config: &Config,
    server: &str,
    geometry: mc_proto::Geometry,
    sink: &mut dyn PointerSink,
) -> Result<Flow> {
    let stream = TcpStream::connect(server)
        .await
        .with_context(|| format!("connexion à {server}"))?;
    stream.set_nodelay(true).context("TCP_NODELAY")?;
    let (mut reader, mut writer) = stream.into_split();

    write_msg(
        &mut writer,
        &Msg::Hello(Hello {
            protocol: PROTOCOL_VERSION,
            name: config.name.clone(),
            os: Os::current(),
            geometry,
            token: config.token.clone(),
        }),
    )
    .await?;

    match read_msg(&mut reader).await? {
        Msg::Welcome {
            screen,
            server_name,
        } => {
            tracing::info!("connectée à « {server_name} » en tant qu'écran {screen}");
        }
        Msg::Refused(refusal) => {
            return Ok(match refusal {
                Refusal::AlreadyConnected { .. } => {
                    // Une ancienne connexion traîne encore côté serveur : elle finira par tomber.
                    tracing::warn!("{refusal}");
                    Flow::Ended
                }
                other => Flow::Fatal(other),
            });
        }
        other => anyhow::bail!("réponse inattendue à Hello : {other:?}"),
    }

    loop {
        match read_msg(&mut reader).await? {
            Msg::Enter { x, y } => {
                tracing::debug!("curseur entrant en {x},{y}");
                sink.move_to(x, y)?;
            }
            Msg::MoveTo { x, y } => sink.move_to(x, y)?,
            Msg::Button { button, pressed } => sink.button(button, pressed)?,
            Msg::Scroll { x, y } => sink.scroll(x, y)?,
            Msg::Leave => {
                tracing::debug!("curseur sortant");
                sink.release_all()?;
            }
            Msg::Ping { nonce } => write_msg(&mut writer, &Msg::Pong { nonce }).await?,
            Msg::Bye => {
                sink.release_all()?;
                return Ok(Flow::Ended);
            }
            other => tracing::debug!("message serveur ignoré : {other:?}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{ScreenConfig, TuningConfig};
    use mc_proto::{Button, Geometry};
    use tokio::net::TcpListener;

    /// Sortie pointeur d'essai : enregistre ce qu'on lui demande, au lieu de bouger un curseur.
    #[derive(Default)]
    struct FakeSink {
        events: Vec<String>,
    }

    impl PointerSink for FakeSink {
        fn move_to(&mut self, x: i32, y: i32) -> Result<()> {
            self.events.push(format!("move {x},{y}"));
            Ok(())
        }
        fn button(&mut self, button: Button, pressed: bool) -> Result<()> {
            self.events
                .push(format!("{} {button:?}", if pressed { "press" } else { "relâche" }));
            Ok(())
        }
        fn scroll(&mut self, x: i32, y: i32) -> Result<()> {
            self.events.push(format!("scroll {x},{y}"));
            Ok(())
        }
        fn release_all(&mut self) -> Result<()> {
            self.events.push("tout relâché".into());
            Ok(())
        }
    }

    fn config_test() -> Config {
        Config {
            name: "client-essai".into(),
            token: "jeton-partagé".into(),
            listen: "0.0.0.0:0".into(),
            tuning: TuningConfig::default(),
            screens: vec![ScreenConfig {
                name: "client-essai".into(),
                x: 0,
                y: 0,
                width: 1280,
                height: 720,
            }],
        }
    }

    #[tokio::test]
    async fn dialogue_complet_avec_le_serveur() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let serveur = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let (mut r, mut w) = stream.into_split();

            let hello = match read_msg(&mut r).await.unwrap() {
                Msg::Hello(hello) => hello,
                other => panic!("premier message inattendu : {other:?}"),
            };

            write_msg(
                &mut w,
                &Msg::Welcome {
                    screen: 1,
                    server_name: "serveur-essai".into(),
                },
            )
            .await
            .unwrap();

            for msg in [
                Msg::Enter { x: 10, y: 20 },
                Msg::MoveTo { x: 11, y: 21 },
                Msg::Button {
                    button: Button::Left,
                    pressed: true,
                },
                Msg::Scroll { x: 0, y: -2 },
                Msg::Ping { nonce: 42 },
            ] {
                write_msg(&mut w, &msg).await.unwrap();
            }

            // Le client doit répondre au Ping sans qu'on lui demande rien d'autre.
            assert_eq!(
                read_msg(&mut r).await.unwrap(),
                Msg::Pong { nonce: 42 }
            );

            write_msg(&mut w, &Msg::Leave).await.unwrap();
            write_msg(&mut w, &Msg::Bye).await.unwrap();
            hello
        });

        let config = config_test();
        let geometry = Geometry {
            width: 1280,
            height: 720,
        };
        let mut sink = FakeSink::default();
        let flow = session(&config, &addr.to_string(), geometry, &mut sink)
            .await
            .expect("la session doit se terminer proprement");
        assert!(matches!(flow, Flow::Ended));

        assert_eq!(
            sink.events,
            vec![
                "move 10,20",
                "move 11,21",
                "press Left",
                "scroll 0,-2",
                // `Leave` puis `Bye` relâchent chacun : jamais de bouton resté enfoncé.
                "tout relâché",
                "tout relâché",
            ]
        );

        let hello = serveur.await.unwrap();
        assert_eq!(hello.name, "client-essai");
        assert_eq!(hello.token, "jeton-partagé");
        assert_eq!(hello.protocol, PROTOCOL_VERSION);
        assert_eq!(hello.geometry, geometry);
    }

    #[tokio::test]
    async fn refus_definitif_ne_doit_pas_etre_retente() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let (mut r, mut w) = stream.into_split();
            let _ = read_msg(&mut r).await.unwrap();
            write_msg(&mut w, &Msg::Refused(Refusal::BadToken))
                .await
                .unwrap();
        });

        let mut sink = FakeSink::default();
        let flow = session(
            &config_test(),
            &addr.to_string(),
            Geometry {
                width: 1280,
                height: 720,
            },
            &mut sink,
        )
        .await
        .unwrap();
        match flow {
            Flow::Fatal(Refusal::BadToken) => {}
            other => panic!("un jeton refusé doit être définitif, obtenu : {other:?}"),
        }
    }
}
