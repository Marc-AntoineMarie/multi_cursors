#!/usr/bin/env bash
# Met en place les droits nécessaires sur une machine Linux. À lancer une seule fois, par machine.
set -euo pipefail

dossier="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

echo "== Règle udev pour /dev/uinput =="
sudo install -m 644 "$dossier/60-multicursos.rules" /etc/udev/rules.d/

echo "== Chargement du module uinput, maintenant et au démarrage =="
echo uinput | sudo tee /etc/modules-load.d/uinput.conf >/dev/null
sudo modprobe uinput

echo "== Ajout de $USER au groupe input =="
sudo usermod -aG input "$USER"

echo "== Application des règles =="
sudo udevadm control --reload-rules
sudo udevadm trigger

cat <<'FIN'

Terminé. Il reste une étape manuelle : fermer la session et la réouvrir.

L'appartenance à un groupe n'est relue qu'à la connexion ; tant que `id` n'affiche pas « input »,
le démon ne pourra pas lire les périphériques. Vérifier ensuite avec :

    mcd check
FIN
