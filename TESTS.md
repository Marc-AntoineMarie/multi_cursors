# Comment vérifier que ça marche

Il n'y a pas d'interface graphique. La vérification repose donc sur deux choses : des
**diagnostics qui testent une seule couche à la fois**, et des **journaux qui disent exactement ce
qui se passe**.

Le principe : on monte niveau par niveau. Chaque niveau ne dépend que des précédents, donc le
premier qui échoue désigne la couche fautive. Inutile de deviner.

| Niveau | Question | Commande | Où |
|---|---|---|---|
| 0 | Le curseur peut-il être déplacé par programme ? | `mcd selftest` | chaque machine |
| 1 | Les droits et la configuration sont-ils bons ? | `mcd check` | chaque machine |
| 2 | La souris est-elle bien capturée ? | `mcd monitor` | serveur (Linux) |
| 3 | Les machines se parlent-elles ? | `mcd server` + `mcd client` | les deux |
| 4 | Le curseur traverse-t-il ? | pousser contre le bord | les deux |

---

## Niveau 0 — l'injection du curseur

**La question** : ce programme arrive-t-il à déplacer le curseur de cette machine ?

```bash
mcd selftest
```

Sur Windows : `mcd.exe selftest`.

**Signal attendu** : après environ une seconde, le curseur saute au centre de l'écran, parcourt un
carré (quatre positions à mi-chemin des coins) et revient au centre. Environ 3 secondes en tout.
Aucun clic n'est émis.

**Pourquoi ce test d'abord** : il ne demande aucun droit particulier et ne dépend ni du réseau ni
de la configuration. S'il fonctionne, la machine peut déjà tenir le rôle **client**.

**S'il ne se passe rien** :

| Cause | Vérification |
|---|---|
| Linux — le compositeur n'a pas pris le périphérique en compte | relancer avec `--delay-ms 3000` |
| Linux — `/dev/uinput` inaccessible | `mcd check` |
| Windows — fenêtre privilégiée au premier plan | cliquer sur le bureau, puis relancer |

Le message d'erreur est explicite dans tous les cas ; `-v` ajoute le détail.

---

## Niveau 1 — droits et configuration

```bash
mcd check
```

**Signal attendu** : chaque ligne commence par `[ok ]`, et le message final est `Tout est en place.`

Exemple sur une machine correctement préparée :

```
  [ok ] module uinput chargé (/dev/uinput)
  [ok ] /dev/uinput accessible en écriture
  [ok ] périphériques de pointage lisibles
        Logitech USB Mouse (/dev/input/event6), SYNA32AA Touchpad (/dev/input/event5)
  [ok ] fichier de configuration
        /home/…/config.toml — machine « portable », écrans : portable, bureau
  [ok ] cette machine figure dans la disposition
```

Chaque ligne `[KO ]` est suivie, en bas, de la commande exacte qui la corrige. Le cas le plus
courant :

```
  [KO ] périphériques de pointage lisibles
        aucun
```

→ il manque l'appartenance au groupe `input`. `sudo usermod -aG input $USER`, **puis fermer la
session et la réouvrir** : un groupe n'est relu qu'à la connexion. `id | grep input` confirme.

Voir aussi ce qui sera capturé :

```bash
mcd devices
```

```
chemin                 souris     pavé      clavier  nom
/dev/input/event5      NON        oui       NON      SYNA32AA:00 06CB:CE17 Touchpad
/dev/input/event6      oui        NON       NON      Logitech USB Receiver Mouse

2 périphérique(s) seront capturés en mode serveur.
```

---

## Niveau 2 — la capture

**La question** : quand je bouge la souris, le moteur reçoit-il quelque chose, et quoi ?

C'est le niveau que rien d'autre ne couvre, et celui où se trouvent les surprises — en particulier
sur un portable, où la conversion du pavé tactile est faite à la main.

```bash
mcd monitor --seconds 15
```

Pendant ces 15 secondes, **le curseur ne bouge plus à l'écran** : la souris est prise en
exclusivité et rien n'est réinjecté. C'est voulu. `Ctrl-C` interrompt (le clavier n'est pas
capturé).

**Signal attendu**, en bougeant la souris, en cliquant et en utilisant la molette :

```
   déplace    +12,   -4  ->  position simulée 972,536
   déplace     -3,  +18  ->  position simulée 969,554
      appui  Left
    relâché  Left
    molette  horizontal +0, vertical -2

38 trame(s) de déplacement, 1 clic(s), 2 cran(s) de molette.
La capture fonctionne : les déplacements arrivent au moteur.
```

**Ce que ça permet de valider séparément** :

- **une souris USB** — les déplacements doivent suivre fidèlement le geste ;
- **le pavé tactile** — un doigt déplace, deux doigts donnent des crans de `molette`, une tape brève
  produit un `appui`/`relâché` de `Left` ;
- **plusieurs périphériques** — brancher une souris pendant que `monitor` tourne : elle est prise en
  compte en moins de deux secondes, sans redémarrage, et les deux pilotent la même position ;
- **la sensibilité** — si le curseur paraîtra trop lent ou trop vif, c'est ici qu'on le voit, avant
  d'accuser le réseau. Régler `sensitivity` dans `[tuning]`.

**Si rien n'arrive** : un autre programme détient déjà la souris. Arrêter Deskflow, Synergy ou
Barrier — deux programmes ne peuvent pas détenir la même prise exclusive.

---

## Niveau 3 — la liaison entre machines

Deux terminaux, un par machine. `-v` est indispensable : c'est lui qui affiche les bascules et la
latence.

**Sur la machine qui tient la souris** (Linux) :

```bash
mcd server -v
```

**Signal attendu au démarrage** :

```
INFO pointeur virtuel « MultiCursos Virtual Pointer » créé (1920x1080)
INFO capture de « Logitech USB Receiver Mouse » (/dev/input/event6)
INFO serveur « portable » à l'écoute sur 0.0.0.0:24810 — écran local 1920x1080
INFO en attente de « bureau » (2560x1440 en 1920,0)
```

La ligne `en attente de` liste les machines de la disposition qui ne sont pas encore connectées.

**Sur l'autre machine** :

```bash
mcd.exe client --config config-windows.toml --connect 192.168.1.20 -v
```

**Signal attendu côté client** :

```
INFO injection Windows prête — bureau virtuel 2560x1440
INFO client « bureau » — bureau annoncé 2560x1440
INFO connectée à « portable » en tant qu'écran 1
```

**Et côté serveur, au même instant** — c'est la preuve que la liaison est établie :

```
INFO « bureau » connectée depuis Some(192.168.1.89:51324) — écran 1 (2560x1440)
DEBUG latence aller-retour : 2.31 ms
```

La latence s'affiche toutes les deux secondes. En dessous de 5 ms, la bascule sera imperceptible ;
au-delà de 20 ms, le curseur paraîtra mou — en Wi-Fi, c'est le moment d'essayer un câble.

### Si la connexion échoue

Le client dit toujours pourquoi, et la cause est dans le message :

| Message | Cause | Correction |
|---|---|---|
| `jeton refusé` | `token` différent entre les deux machines | copier le même jeton partout |
| `n'apparaît dans aucun écran` | le `name` du client n'est dans aucun `[[screen]]` du serveur | ajouter le bloc, ou corriger `name` |
| `versions de protocole incompatibles` | binaires de versions différentes | recopier le même binaire |
| `Connection refused` | serveur non lancé, ou mauvaise adresse | vérifier l'IP et que `mcd server` tourne |
| `Connection timed out` | pare-feu, ou machines sur des réseaux différents | ouvrir le port 24810 en entrée côté serveur |

Un avertissement mérite une attention particulière :

```
WARN « bureau » annonce 2560x1440 alors que la disposition prévoit 1920x1080 :
     le curseur n'entrera pas au bon endroit, corriger la configuration
```

Le client annonce sa **vraie** taille d'écran. Si elle ne correspond pas à la disposition, corriger
le bloc `[[screen]]` **sur les deux machines** : c'est une carte commune, elle doit être identique
partout.

---

## Niveau 4 — la traversée

**Le signal physique, celui qui compte** : pousser le curseur contre le bord de l'écran du serveur
qui touche l'autre machine dans la disposition (le bord **droit** si l'autre écran est à droite).

Ce que l'on doit observer, dans l'ordre :

1. le curseur s'immobilise contre le bord de l'écran du serveur — il est « garé », c'est normal ;
2. il **apparaît sur l'écran de l'autre machine**, sur le bord opposé, à la même hauteur ;
3. il y répond à la souris comme s'il s'agissait d'un seul grand bureau ;
4. les clics et la molette agissent sur cette machine.

Et dans le journal du serveur, au moment du franchissement :

```
DEBUG bascule de l'écran 0 vers « bureau »
```

Côté client :

```
DEBUG curseur entrant en 0,540
```

Pousser ensuite contre le bord opposé de la seconde machine : le curseur doit revenir, et le
journal afficher `curseur sortant` puis une bascule en sens inverse.

### Vérifications de détail, une fois la traversée acquise

| À tester | Attendu |
|---|---|
| cliquer sur la machine distante | le clic agit là-bas, pas ici |
| molette sur la machine distante | défilement là-bas |
| maintenir un clic et traverser | aucun bouton ne reste bloqué (un `Leave` relâche tout) |
| traverser très vite | le curseur traverse quand même, sans rester collé au bord |
| couper le client (`Ctrl-C`) alors qu'il tient le curseur | le curseur revient sur le serveur, journal : `la machine active s'est déconnectée` |
| rallumer le client | il se reconnecte seul, et la traversée remarche |

---

## Sortie de secours

La souris est prise en exclusivité : tant que le serveur tourne, elle lui appartient. Trois
sorties, par ordre de préférence :

1. **maintenir les boutons gauche et droit ensemble 2 secondes** — rend la souris au système local
   sans arrêter le démon ; le même geste reprend la capture. Fonctionne depuis n'importe quel
   écran ;
2. `Ctrl-C` dans le terminal du serveur ;
3. tuer le processus — le noyau relâche la prise à la fermeture des descripteurs, y compris après un
   plantage.

**Au premier essai, garder une session SSH ouverte depuis une autre machine.** C'est la porte de
sortie si quelque chose se comporte mal.

---

## Test en boucle locale, sans deuxième machine

À quoi il sert : faire tourner le serveur **et** le client sur la même machine, pour séparer deux
causes de panne. Si ça échoue là, le défaut est dans MultiCursos. Si ça marche, tout problème
restant vient du réseau ou de la configuration entre machines. C'est un confort de diagnostic, pas
une étape obligatoire.

Le client se présente sous le nom de l'écran voisin, donc le serveur lui envoie le curseur au
franchissement du bord — alors que les deux programmes tournent côte à côte :

```bash
# terminal 1
mcd server -v

# terminal 2
mcd client --config ~/.config/multicursos/loopback-client.toml --connect 127.0.0.1 -v
```

Le curseur saute alors d'un bord à l'autre du même écran : c'est attendu, les deux « écrans » sont
physiquement le même. Ce que le test prouve, c'est que la bascule se déclenche et que le client
injecte.

---

## Tests automatiques

```bash
cargo test     # 32 tests, moins d'une seconde
```

Ils couvrent ce qui peut l'être sans matériel : la géométrie de la traversée (bords extérieurs,
trous entre écrans, coins, mouvements rapides qui enjambent un écran entier), le cadrage et la
sérialisation du protocole, la lecture de la configuration, et une session réseau complète rejouée
contre un faux serveur et une fausse sortie pointeur.

Les tailles des structures d'ABI système sont vérifiées **à la compilation**, y compris en
compilation croisée : un binaire Windows dont la structure `INPUT` serait mal dimensionnée ne peut
pas être produit.

Ce qu'aucun test automatique ne couvre, et qui demande donc les niveaux ci-dessus : la capture
réelle, la conversion du pavé tactile, et l'injection sur chaque plateforme.
