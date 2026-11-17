# MultiCursos

Une seule souris pour plusieurs ordinateurs. Le curseur franchit le bord d'un écran et continue sur
la machine voisine, comme s'il n'y avait qu'un seul ordinateur à plusieurs écrans.

**État : étape 1 sur 6, plus le client Windows.** Souris en ligne de commande, serveur Linux,
clients Linux et Windows. L'interface graphique, le client macOS, le clavier et le presse-papier
suivent (voir *Feuille de route*).

## Pourquoi pas Deskflow / Synergy / Barrier

Ces outils dépendent, sous Wayland, des portails `InputCapture` et `RemoteDesktop` : autorisations
à accorder, comportements variables selon le compositeur, latence inégale.

MultiCursos ne les utilise pas. Il prend les périphériques réels en exclusivité au niveau du noyau
(`EVIOCGRAB`) et recrée un pointeur **absolu** via `uinput`. Le compositeur ne voit plus qu'un seul
périphérique de pointage, dont nous fixons la position au pixel près. Conséquences :

* comportement identique sous Wayland et sous X11, et sous n'importe quel compositeur ;
* aucune fenêtre d'autorisation, aucun portail, aucune dépendance à GNOME ;
* la position du curseur est **connue** au lieu d'être devinée — la bascule entre écrans n'est plus
  qu'un calcul géométrique.

Le prix à payer : un accès aux périphériques d'entrée (groupe `input`), et la capture du pavé
tactile est pour l'instant plus rudimentaire que celle de libinput (voir plus bas).

## Comment ça marche

```
   souris physique                                   machine distante
        │                                                   ▲
        │ EVIOCGRAB (exclusif)                              │ TCP, positions absolues
        ▼                                                   │
   ┌────────────────┐     position globale    ┌──────────────┴───────┐
   │  mc-input      │────────────────────────▶│  mc-layout           │
   │  capture       │                         │  quel écran, où      │
   └────────────────┘                         └──────────┬───────────┘
                                                         │ écran local
                                              ┌──────────▼───────────┐
                                              │  uinput, pointeur    │
                                              │  absolu (ABS_X/Y)    │
                                              └──────────────────────┘
```

Le serveur est seul maître de la position du curseur. Sur l'écran local, il l'écrit dans son
pointeur virtuel ; sur un écran distant, il envoie la position au client concerné. Les messages
portent toujours des **positions absolues**, jamais des déplacements : une trame perdue n'a donc
aucun effet durable.

### Organisation du code

| Dossier | Rôle |
|---|---|
| `crates/layout` | Géométrie du bureau virtuel, traversée des bords. Pur, entièrement testé. |
| `crates/proto` | Messages réseau, cadrage longueur + `postcard`. |
| `crates/input` | `PointerSink` par plateforme (uinput, `SendInput`) et la capture evdev. |
| `crates/node` | Configuration, rôle serveur, rôle client. |
| `apps/mcd` | Binaire en ligne de commande. |
| `packaging` | Règle udev, unités systemd, script de droits. |

Les interfaces système sont déclarées à la main, sans crate de liaisons, des deux côtés :

* sous Linux, il faut `UI_SET_PROPBIT` pour déclarer `INPUT_PROP_POINTER`, que la crate `evdev`
  n'expose pas. Sans ce bit, udev classe notre périphérique en écran tactile et le curseur
  n'atterrit pas où on le demande ;
* sous Windows, `SendInput` et `GetSystemMetrics` suffisent : trois déclarations contre un gros
  paquet de bindings à suivre en version.

Les tailles des structures d'ABI sont vérifiées **à la compilation** (`const _: () = assert!(…)`).
Une erreur de disposition mémoire ne produirait aucun message à l'exécution, juste un curseur qui
ne bouge pas : mieux vaut que la compilation refuse.

## Installation

### Compiler sans toolchain locale

Aucun Rust n'est nécessaire sur la machine : l'image de `packaging/Dockerfile.cross` contient tout,
pour Linux comme pour Windows.

```bash
docker build -f packaging/Dockerfile.cross -t multicursos-cross .

compiler() {
  docker run --rm --user "$(id -u):$(id -g)" \
    -e CARGO_HOME=/cargo -e CARGO_TARGET_DIR=/cargo/target \
    -v "$PWD/.cargo-cross":/cargo -v "$PWD":/w -w /w multicursos-cross "$@"
}

compiler cargo test                                            # 32 tests
compiler cargo build --release                                 # Linux
compiler cargo build --release --target x86_64-pc-windows-gnu  # Windows
```

Les binaires se trouvent alors dans `.cargo-cross/target/release/mcd` et
`.cargo-cross/target/x86_64-pc-windows-gnu/release/mcd.exe`. Le dossier `dist/` contient les
derniers binaires construits, prêts à être copiés.

Avec une toolchain locale (`rustup`), `cargo build --release` suffit.

### Linux

```bash
# 1. Compilation (voir ci-dessus si cargo n'est pas installé)
cargo build --release
install -Dm755 target/release/mcd ~/.local/bin/mcd

# 2. Vérifier que l'injection du curseur fonctionne (aucun droit particulier requis)
mcd selftest

# 3. Droits de lecture des périphériques, pour le rôle serveur uniquement
sudo usermod -aG input $USER
#    puis fermer la session et la réouvrir — `id` doit afficher « input »

# 4. Vérification
mcd check
```

Sur une session de bureau classique, `/dev/uinput` est **déjà** accessible : logind y pose une ACL
pour l'utilisateur de la session. La règle udev de `packaging/` n'est utile que si ce n'est pas le
cas (serveur sans session graphique, machine sans logind) — `mcd check` le dit explicitement et
n'affiche que les étapes réellement manquantes.

Un rôle **client** n'a besoin d'aucun droit supplémentaire : il n'injecte, il ne capture pas.

### Windows

Rien à installer sur la machine Windows : copier `dist/mcd.exe` (ou le construire comme ci-dessus),
puis :

```
mcd.exe selftest
mcd.exe init
mcd.exe client --connect 192.168.1.20
```

La configuration se trouve dans `%APPDATA%\multicursos\config.toml`. Deux points à connaître :

* Windows filtre l'injection vers les fenêtres plus privilégiées (UIPI). Au-dessus d'une invite
  UAC ou d'une application lancée en administrateur, le curseur ne bougera pas, sauf si le client
  lui-même est lancé en administrateur.
* autoriser `mcd.exe` dans le pare-feu, ou ouvrir le port 24810 en entrée côté serveur.

## Utilisation

```bash
# Sur chaque machine, une configuration de départ
mcd init
```

Puis décrire la disposition dans `~/.config/multicursos/config.toml`, **identique sur toutes les
machines** pour la partie `[[screen]]`, et avec le même `token` :

```toml
name  = "portable"        # le nom de CETTE machine
token = "le-même-partout"

[[screen]]
name   = "portable"
width  = 1920
height = 1080

[[screen]]
name   = "bureau"
x      = 1920             # commence là où le précédent finit : les bords se touchent
y      = 0
width  = 2560
height = 1440
```

Lancer ensuite :

```bash
# sur la machine qui tient la souris
mcd server

# sur chaque autre machine
mcd client --connect 192.168.1.20
```

Le curseur traverse maintenant le bord droit du portable pour arriver sur le bureau.

### Test en boucle locale, sans deuxième machine

Toute la chaîne — capture, réseau, injection — se vérifie sur une seule machine. Le client se
présente sous le nom de l'écran voisin, donc le serveur lui envoie le curseur au franchissement du
bord droit :

```bash
# terminal 1
mcd server -v

# terminal 2
mcd client --config ~/.config/multicursos/loopback-client.toml --connect 127.0.0.1 -v
```

Le curseur saute alors d'un bord à l'autre du même écran : c'est attendu, les deux « écrans » sont
physiquement le même. Ce que le test prouve, c'est que la bascule se déclenche et que le client
injecte. Le journal du serveur affiche `bascule de l'écran 0 vers « windows »`.

### Diagnostics

```bash
mcd selftest  # déplace le curseur en carré : valide l'injection, sans aucun droit particulier
mcd devices   # ce qui sera capturé, et comment chaque périphérique est classé
mcd check     # droits d'accès, avec la marche à suivre en cas de manque
mcd server -v # journal détaillé : bascules, latence aller-retour
```

`mcd selftest` est la première commande à lancer sur toute nouvelle machine : si le curseur suit le
carré, la machine peut déjà tenir le rôle client. Elle n'émet aucun clic.

## Sécurité d'usage

La souris est prise en exclusivité : tant que le démon tourne, elle lui appartient. Trois sorties,
par ordre de préférence :

1. **maintenir les boutons gauche et droit ensemble 2 secondes** — rend la souris au système local
   sans arrêter le démon ; le même geste reprend la capture ;
2. `Ctrl-C` dans le terminal du démon ;
3. tuer le processus — le noyau relâche la prise à la fermeture des descripteurs, y compris après
   un plantage.

**Au premier essai, garder une session SSH ouverte depuis une autre machine.** C'est la porte de
sortie si le clavier et la souris deviennent inutilisables.

Arrêter Deskflow, Synergy ou Barrier avant de lancer `mcd server` : deux programmes ne peuvent pas
détenir la même prise exclusive.

## Plusieurs périphériques en même temps

Le serveur capture **tous** les périphériques de pointage de la machine et fusionne leurs
déplacements dans une seule position virtuelle. Conséquences voulues :

* souris USB et pavé tactile fonctionnent indifféremment, y compris alternativement dans la même
  seconde ;
* plusieurs souris branchées se partagent le même curseur ;
* un branchement ou un débranchement est pris en compte en moins de deux secondes, sans
  redémarrage (`/dev/input` est rebalayé périodiquement) ;
* nos propres périphériques virtuels sont reconnus par leur nom (préfixe `MultiCursos`) et jamais
  capturés — sans quoi le pointeur s'alimenterait en boucle.

`mcd devices` montre ce qui sera capturé.

### Logiciels de prise en main à distance

AnyDesk, TeamViewer et équivalents injectent des événements de pointage, et deux cas se présentent :

* ils créent un périphérique virtuel au niveau du noyau → nous le capturons comme n'importe quelle
  souris, et le curseur distant traverse les écrans comme le vôtre. C'est le comportement
  souhaitable ;
* ils déplacent le curseur par-dessus le système (XTEST sous X11, portail sous Wayland) → notre
  position de référence ne le voit pas et se désynchronise du curseur réel.

Le second cas n'est pas traité pour l'instant. La solution prévue est une liste d'exclusion dans la
configuration (`[capture] ignore = [...]`) doublée d'une resynchronisation lorsqu'un écart est
détecté. À faire quand le besoin se présentera concrètement.

## Pavé tactile

Un pavé tactile « Precision » moderne n'émet rien sur son nœud souris : tout passe par le nœud
multipoint, en coordonnées absolues. MultiCursos le convertit lui-même en déplacements, et prend en
charge le déplacement à un doigt, le clic physique, la tape pour cliquer et le défilement à deux
doigts.

Ce qui n'est **pas** pris en charge : gestes à trois doigts ou plus, pincement, rejet de paume fin,
accélération identique à celle du système. Ces comportements viennent de libinput, qu'on ne peut pas
interroger sur un périphérique qu'on détient en exclusivité sans lui passer nos propres
descripteurs — c'est prévu, mais plus tard. Avec une souris USB, rien de tout cela ne s'applique.

Réglages dans `[tuning]` : `sensitivity`, `touchpad_pixels_per_mm`, `touchpad_mm_per_scroll`,
`tap_to_click`.

## Feuille de route

| Étape | Contenu | État |
|---|---|---|
| 1 | Souris Linux → Linux, ligne de commande | **fait**, test réel à mener |
| 2 | Interface graphique : disposition en glisser-déposer, découverte mDNS, latence | à venir |
| 3 | Client Windows (`SendInput`) | **fait** |
| 3 | Client macOS (`CGEventPost`) | à venir |
| 4 | Clavier et presse-papier | à venir |
| 5 | N'importe quelle machine peut tenir le rôle serveur | à venir |
| 6 | TLS + appariement par code, raccourcis de bascule directe | à venir |

Les traits `PointerSink` et la capture sont déjà séparés du moteur : les étapes 3 et 5 consistent à
ajouter des implémentations, pas à retoucher la logique de bascule.

## Tests

```bash
cargo test                  # 32 tests : géométrie, protocole, configuration, dialogue client
cargo test -p mc-layout     # uniquement la traversée des bords
```

Deux points sont couverts de près :

* **la géométrie** — pure, donc testée exhaustivement : bords extérieurs, trous entre écrans,
  coins, et mouvements rapides qui enjambent un écran entier (à 1000 Hz, un coup de poignet
  produit un delta de plusieurs milliers de pixels) ;
* **le dialogue réseau** — un faux serveur et une fausse sortie pointeur rejouent une session
  complète, sans périphérique ni privilège.

Les tailles des structures d'ABI système sont vérifiées à la compilation, y compris en compilation
croisée : un binaire Windows faux ne peut pas être produit.

### Ce qui n'est pas encore vérifié sur matériel

* la **capture** (serveur) — elle demande l'appartenance au groupe `input` ;
* la conversion **pavé tactile** → déplacements, écrite mais jamais exécutée ;
* le **client Windows** — compilé et vérifié statiquement, jamais lancé.
