# Remote Duel: Rust-Klon vs. C++ Tentris auf Hetzner Cloud

Diese Infrastruktur provisioniert einen x86_64-Server bei Hetzner Cloud, baut dort unseren Rust-Klon und die Forschungsversion von Tentris (`dice-group/tentris`) aus Quellen und führt `final_duel.py` aus.

## Warum Remote?

Das originale Tentris ist offiziell nur für **Linux x86_64** mit AVX2-Unterstützung verfügbar. Auf einem Apple-Silicon-Mac lässt es sich daher nur schwer oder gar nicht bauen. Mit Hetzner Cloud bekommen wir für wenige Cent pro Stunde einen passenden x86_64-Server.

## Voraussetzungen lokal

- [Terraform](https://developer.hashicorp.com/terraform/downloads)
- [Ansible](https://docs.ansible.com/ansible/latest/installation_guide/index.html)
- Ein Hetzner Cloud Account + API-Token

Das Skript erzeugt automatisch ein dediziertes SSH-Key-Paar unter `infra/terraform/duel_key`.

## Schnellstart

```bash
# 1. API-Token setzen
export HCLOUD_TOKEN="dein-hetzner-cloud-api-token"

# 2. Server provisionieren, Rust-Klon bauen, Tentris bauen und Duell ausführen
./run_remote_duel.sh

# 3. Ergebnisse ansehen
cat duel_output.log

# 4. Server wieder löschen (vergisst das nicht!)
./infra/terraform/destroy.sh
```

## Was passiert im Hintergrund?

1. **Terraform** erzeugt einen `cpx42` (oder konfigurierbaren) Hetzner-Server mit Ubuntu 22.04 und öffnet SSH.
2. **Ansible** installiert Build-Tools, Rust, Conan, CMake etc.
3. **Ansible** kopiert den Rust-Klon, baut ihn im Release-Modus und generiert `synthetic_1m.nt`.
4. **Ansible** klont die Forschungsversion von Tentris, fügt den DICE-Conan-Remote hinzu und baut sie.
5. **Ansible** führt `final_duel.py` aus und speichert die Ausgabe in `/opt/trillian/duel_output.log`.
6. Das Wrapper-Skript holt die Log-Datei zurück.

`final_duel.py` erkennt automatisch, ob die Forschungsversion oder das kommerzielle `tentris/tentris`-Binary vorhanden ist. Die Forschungsversion wird bevorzugt, da die kommerzielle Beta eine Lizenzdatei benötigt.

## Kosten

Ein `cpx42` Server kostet ca. 0,238 €/Stunde. Das komplette Deployment + Build + Duel dauert typischerweise 20–40 Minuten, also etwa **0,12–0,16 € pro Durchlauf**.

## Fehlerbehebung

### Terraform sagt "invalid token"

`HCLOUD_TOKEN` ist nicht gesetzt oder falsch.

### Ansible kann nicht per SSH verbinden

`run_remote_duel.sh` pollt jetzt selbst bis zu 120 Sekunden auf SSH-Erreichbarkeit. Falls es trotzdem fehlschlägt:

1. Stelle sicher, dass der dedizierte Key existiert: `infra/terraform/duel_key` und `infra/terraform/duel_key.pub`.
2. Wenn du gerade vom alten Verhalten (Nutzer-SSH-Key) umgestiegen bist, lösche den alten Server mit `./infra/terraform/destroy.sh`, damit Terraform den neuen Key injizieren kann.
3. Prüfe im Hetzner-Cloud-Webinterface, ob der Server läuft und eine IPv4 hat.

### Tentris-Build bricht ab

Tentris ist ein komplexes C++-Projekt mit vielen Conan-Abhängigkeiten. Falls der Build fehlschlägt:

```bash
# Per SSH auf den Server einloggen und manuell debuggen
ssh -i infra/terraform/duel_key root@$(cd infra/terraform && terraform output -raw server_ip)
```

Dann im Verzeichnis `/opt/trillian/third_party/tentris` die Build-Schritte manuell ausführen.

## Konfiguration

Server-Typ und Standort lassen sich in `infra/terraform/variables.tf` oder via Kommandozeile anpassen:

```bash
cd infra/terraform
terraform apply -var="server_type=cpx42" -var="location=fsn1"
```
