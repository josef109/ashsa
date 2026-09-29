# ashsa – Dokumentation

**Alexa Smart Home Skill Adapter für Home Assistant**
Version 0.2.3 · Lizenz MIT · Sprache Rust (Edition 2024)

---

## 1. Überblick

`ashsa` ist ein AWS-Lambda-Adapter, der Alexa-Smart-Home-Anfragen entgegennimmt und unverändert an die Alexa-Schnittstelle von Home Assistant weiterleitet (`POST {BASE_URL}/api/alexa/smart_home`). Es ist eine Rust-Neuimplementierung des von Home Assistant empfohlenen Python-Lambda-Skills und wird als einzelne, statisch gelinkte Binärdatei (`bootstrap`) ausgeliefert.

**Kernidee:** ashsa ist ein reiner Durchleiter (Proxy). Es enthält keine Geräte- oder Skill-Logik, sondern validiert die Anfrage, wählt das Bearer-Token, leitet weiter und gibt die Antwort von Home Assistant zurück.

```
Alexa Cloud ──► AWS Lambda (ashsa) ──HTTPS──► Home Assistant
   Directive        validieren +               /api/alexa/smart_home
   (JSON)           Token wählen
                ◄── JSON-Antwort ◄──────────────
```

## 2. Projektstruktur

| Pfad | Zweck |
| --- | --- |
| `src/main.rs` | Einstiegspunkt: Logging, Konfiguration, HTTP-Client, Lambda-Runtime |
| `src/lib.rs` | Bibliotheks-Root (exportiert `client`, `config`, `handler`) |
| `src/config.rs` | Konfiguration aus Umgebungsvariablen (`Config`, `ConfigError`) |
| `src/client.rs` | Aufbau des gemeinsamen `reqwest::Client` |
| `src/handler.rs` | Kernlogik: Validierung, Token-Auswahl, Weiterleitung, Fehlerabbildung |
| `.github/workflows/` | CI, Lambda-Paketierung, Release |
| `Cargo.toml` / `Cargo.lock` | Abhängigkeiten und Metadaten |
| `cliff.toml`, `release-plz.toml` | Changelog- und Release-Automatisierung |
| `renovate.json` | Automatische Abhängigkeits-Updates |

### Abhängigkeiten

| Crate | Verwendung |
| --- | --- |
| `lambda_runtime` | Lambda-Laufzeit und Event-Schleife |
| `reqwest` (nur `json`, `rustls`) | HTTP-Client, TLS ohne OpenSSL |
| `serde`, `serde_json` | JSON-Verarbeitung (Events werden als `serde_json::Value` behandelt) |
| `tokio` | Asynchrone Laufzeit |
| `tracing`, `tracing-subscriber` | Strukturiertes Logging |
| `mockito` (dev) | Mock-HTTP-Server für Tests |

## 3. Ablauf einer Anfrage

Implementiert in `App::handle_event` / `handle_event_inner` (`src/handler.rs`):

1. **Start:** Beim Kaltstart liest `main` die Konfiguration, baut den HTTP-Client und legt eine `App` hinter einem `Arc` an. Die Lambda-Runtime ruft pro Event `handle_event` auf.
2. **Payload-Version prüfen** (`validate_payload_version`): `directive.header.payloadVersion` muss `"3"` sein.
3. **Scope ermitteln** (`validated_scope`), in dieser Reihenfolge:
   1. `directive.endpoint.scope` (normale Steuerbefehle)
   2. `directive.payload.grantee` (Account Linking / AcceptGrant)
   3. `directive.payload.scope` (Discovery)

   Der Scope-`type` muss `"BearerToken"` sein.
4. **Token wählen** (`extract_token`):
   1. `LONG_LIVED_ACCESS_TOKEN`, falls gesetzt (hat Vorrang)
   2. sonst `scope.token` aus dem Event
   3. sonst Fehler `MissingToken`
5. **Weiterleiten:** `POST {BASE_URL}/api/alexa/smart_home` mit `Authorization: Bearer <token>` und dem **unveränderten** Event als JSON-Body.
6. **Antwort auswerten:**

| Antwort von Home Assistant | Ergebnis |
| --- | --- |
| 2xx mit gültigem JSON | JSON wird 1:1 an Alexa zurückgegeben |
| 2xx mit ungültigem JSON | `INTERNAL_ERROR` |
| 401 / 403 | `INVALID_AUTHORIZATION_CREDENTIAL` (Body als Meldung) |
| andere Statuscodes | `INTERNAL_ERROR` mit Statuscode und Body |
| Netzwerkfehler / Timeout | `INTERNAL_ERROR` („failed to call downstream endpoint …“) |
| Validierungsfehler der Anfrage | `INVALID_REQUEST` |

### Fehlerformat

Fehler werden immer als `Ok`-Antwort der Lambda zurückgegeben (die Funktion selbst schlägt nicht fehl):

```json
{
  "event": {
    "payload": {
      "type": "INTERNAL_ERROR",
      "message": "downstream endpoint returned 500: boom"
    }
  }
}
```

Mögliche `type`-Werte: `INVALID_REQUEST`, `INVALID_AUTHORIZATION_CREDENTIAL`, `INTERNAL_ERROR`.

## 4. Konfiguration

Alle Einstellungen kommen aus Umgebungsvariablen (`Config::from_env`).

| Variable | Pflicht | Standard | Beschreibung |
| --- | --- | --- | --- |
| `BASE_URL` | Ja | – | Basis-URL von Home Assistant. Ein abschließender `/` wird entfernt. Leer oder fehlend → Startfehler. |
| `LONG_LIVED_ACCESS_TOKEN` | Nein | – | Home-Assistant-Token; überschreibt das Token aus dem Alexa-Event. Leerer Wert wird ignoriert. |
| `DEBUG` | Nein | `false` | Schaltet das Debug-Logging des eingehenden Events ein (Tokens werden geschwärzt). |
| `NOT_VERIFY_SSL` | Nein | `false` | Deaktiviert die TLS-Zertifikatsprüfung. |
| `AWS_DEFAULT_REGION` | Nein | – | Wird von Lambda gesetzt und nur im User-Agent verwendet. |
| `RUST_LOG` | Nein | `info` | Standard-`EnvFilter` von `tracing-subscriber` für die Log-Stufe. |

**Boolesche Werte** (`DEBUG`, `NOT_VERIFY_SSL`): `1/true/yes/on` bzw. `0/false/no/off`, Groß-/Kleinschreibung egal. Alles andere führt zu einem Startfehler (`ConfigError::InvalidBool`).

Beispiel:

```text
BASE_URL=https://homeassistant.example.com
LONG_LIVED_ACCESS_TOKEN=<token>
DEBUG=false
NOT_VERIFY_SSL=false
```

## 5. HTTP-Client

Aufgebaut in `build_http_client` (`src/client.rs`), einmal pro Kaltstart und für alle Anfragen wiederverwendet:

- TLS über `rustls`
- Connect-Timeout 2 s, Gesamt-Timeout 10 s
- Idle-Timeout des Verbindungspools 90 s
- User-Agent: `Alexa Smart Home Skill Adapter - <region>` bzw. ohne Region-Suffix
- `NOT_VERIFY_SSL=true` → `danger_accept_invalid_certs(true)`

Hinweis: Alexa erwartet Antworten innerhalb weniger Sekunden. Das 10-s-Limit ist daher eher eine Obergrenze; Home Assistant sollte schnell erreichbar sein.

## 6. Logging

- Initialisiert in `init_tracing` (kompaktes Format).
- ANSI-Farben sind aus, sobald `AWS_LAMBDA_FUNCTION_NAME` gesetzt ist (also in Lambda).
- Standard-Stufe `INFO`; ausgegeben werden u. a. „processing Alexa request“ und welche Token-Quelle genutzt wird (`Using long lived token` / `Using token from event`).
- Bei `DEBUG=true` wird das Event geloggt. `sanitize_json` ersetzt rekursiv jeden Schlüssel `token` (ohne Beachtung der Groß-/Kleinschreibung) durch `<redacted>`.

## 7. Deployment auf AWS Lambda

1. Paket für die gewünschte Architektur beziehen (empfohlen: arm64) aus den GitHub-Releases:
   `ashsa-lambda-arm64.zip` oder `ashsa-lambda-x86_64.zip`.
2. In der Lambda-Konsole eine neue Funktion anlegen:
   - „Author from scratch“
   - Runtime: `Provide your own bootstrap on Amazon Linux 2023` (`provided.al2023`)
   - Architektur passend zum ZIP
   - Ausführungsrolle mit mindestens CloudWatch-Logs-Rechten
3. ZIP als Code hochladen (falls verlangt: Handler `bootstrap`).
4. Umgebungsvariablen setzen (siehe Abschnitt 4).
5. Die Lambda-ARN als Endpunkt des Alexa-Smart-Home-Skills eintragen.

Voraussetzung auf der Home-Assistant-Seite: Die Alexa-Smart-Home-Integration muss konfiguriert sein (`alexa:` mit `smart_home:`) und `BASE_URL` muss aus dem Internet erreichbar sein.

## 8. Entwicklung

```bash
cargo fmt --check      # Formatierung prüfen
cargo test --locked    # Tests ausführen
```

Die Tests sind als Unit-Tests direkt in `config.rs` und `handler.rs` enthalten. Sie decken ab:

- Konfiguration: fehlende `BASE_URL`, Bool-Parsing, optionale Region, Token-Laden
- Payload-Version, fehlendes `directive`
- Token-Extraktion aus allen drei Scope-Orten, Vorrang des Long-Lived-Tokens, ungültiger Scope-Typ
- Weiterleitung inkl. Header (Authorization, Content-Type, User-Agent) gegen einen `mockito`-Server
- Fehlerabbildung: 401, 500, ungültiges JSON, Transportfehler

## 9. CI/CD und Release

| Workflow | Auslöser | Aufgabe |
| --- | --- | --- |
| `ci.yml` | Push auf `main`, Pull Requests | `cargo fmt --check`, `cargo test --locked` |
| `lambda-package.yml` | Pull Requests, manuell | Baut Lambda-ZIPs (x86_64 und arm64, musl) mit `cargo-lambda` und Zig, erzeugt CycloneDX-SBOM |
| `release.yml` | Push auf `main`, manuell | `release-plz` (Release und Release-PR), baut Release-Artefakte, erstellt Provenance- und SBOM-Attestierungen, hängt ZIPs und SBOMs ans GitHub-Release |

Weitere Punkte:

- Changelog wird mit `git-cliff` (`cliff.toml`) aus Commits erzeugt; Tags haben das Format `v<version>`.
- Renovate aktualisiert Cargo-Abhängigkeiten und GitHub Actions (gruppiert) sowie die in den Workflows fest verdrahteten Tool-Versionen (`cargo-lambda`, `cargo-cyclonedx`, Zig, `release-plz`).
- GitHub Actions sind auf Commit-Hashes gepinnt (Supply-Chain-Schutz).

## 10. Sicherheitshinweise

- **Long-Lived-Token:** Wenn gesetzt, wird es für *jede* Anfrage verwendet, unabhängig davon, welches Token Alexa mitschickt. Wer die Lambda aufrufen darf, hat damit die Rechte dieses Tokens. Lambda-Zugriff (Alexa-Skill-Trigger mit Skill-ID) daher restriktiv halten. Bei Account Linking über OAuth ist das Token aus dem Event vorzuziehen.
- **Token-Speicherung:** Umgebungsvariablen sind in der Lambda-Konsole sichtbar. Für höheren Schutz Verschlüsselung mit KMS bzw. Secrets Manager erwägen (nicht Teil von ashsa).
- **`NOT_VERIFY_SSL`:** Nur für kontrollierte Umgebungen (z. B. selbstsigniertes Zertifikat im Test). In Produktion ausgeschaltet lassen.
- **Log-Schwärzung:** Nur Schlüssel mit dem Namen `token` werden geschwärzt.
- **Fehlerweitergabe:** Bei 401/403 und sonstigen Fehlern wird der Response-Body von Home Assistant in die Alexa-Fehlermeldung übernommen.

## 11. Migration vom Python-Skript (`lambda_function.py`)

`ashsa` ersetzt das Python-Skript aus der Home-Assistant-Dokumentation (Apache-2.0, Copyright 2019 Jason Hu, überarbeitet von Matthew Hilton). Beide leiten dieselben Alexa-Directives an denselben Endpunkt weiter, verhalten sich aber in Konfiguration und Fehlerfällen unterschiedlich.

### 11.1 Gemeinsames Verhalten

- Nur `payloadVersion "3"` wird akzeptiert.
- Scope-Suche in derselben Reihenfolge: `endpoint.scope`, `payload.grantee`, `payload.scope`; der Typ muss `BearerToken` sein.
- `POST {BASE_URL}/api/alexa/smart_home` mit `Authorization: Bearer <token>` und dem unveränderten Event als Body.
- Timeouts: 2 s Connect, 10 s Read/Gesamt.
- Fehler werden als `{"event": {"payload": {"type": ..., "message": ...}}}` zurückgegeben; 401/403 ergeben `INVALID_AUTHORIZATION_CREDENTIAL`.
- Umgebungsvariablen `BASE_URL`, `DEBUG`, `NOT_VERIFY_SSL`, `LONG_LIVED_ACCESS_TOKEN` haben dieselben Namen.

### 11.2 Unterschiede

| Punkt | Python-Skript | Rust (`ashsa`) |
| --- | --- | --- |
| Token-Vorrang | Event-Token zuerst. `LONG_LIVED_ACCESS_TOKEN` wird nur genutzt, wenn im Event kein Token steht **und** `DEBUG` gesetzt ist. | `LONG_LIVED_ACCESS_TOKEN` hat immer Vorrang, unabhängig von `DEBUG`. |
| Boolesche Variablen | `bool(os.environ.get(...))`: jeder nicht leere String ist „wahr“, auch `false` und `0`. | Strenges Parsing (`1/true/yes/on`, `0/false/no/off`); ungültige Werte verhindern den Start. |
| Debug-Logging | `DEBUG` setzt das Log-Level auf DEBUG. Das Event wird samt Token geloggt. | Zusätzlich `RUST_LOG=debug` nötig. Tokens werden als `<redacted>` geloggt. |
| Fehlende `BASE_URL` | Fehler erst bei der Anfrage (`INVALID_REQUEST`). | Fehler beim Kaltstart; leere `BASE_URL` wird ebenfalls abgelehnt. |
| Statuscodes der HA-Antwort | Ab 400 Fehler; sonst wird der Body als JSON gelesen. | Alles außer 2xx ist ein Fehler. |
| Ungültiges JSON von HA | `INVALID_REQUEST` | `INTERNAL_ERROR` |
| Netzwerkfehler / Timeout | `INTERNAL_ERROR`, feste Meldung „An unexpected error occurred“. | `INTERNAL_ERROR` mit Detailmeldung. |
| HTTP-Client | Neuer `PoolManager` pro Aufruf. | Gemeinsamer Client mit Connection-Pooling. |
| Wiederholungen | `urllib3` nutzt standardmäßig `Retry(total=3)`. Bei POST werden aber nur Verbindungsfehler wiederholt (Connect, DNS), keine Read-Timeouts und keine Fehlerstatus. | Standardmäßig keine Retries. Optional nachrüstbar, siehe Abschnitt 12. |
| Timeout-Semantik | `read=10` pro Lesevorgang. | 10 s für die gesamte Anfrage. |
| User-Agent | urllib3-Standard | `Alexa Smart Home Skill Adapter - <region>` |

### 11.3 Wichtigste Stolperfallen

1. **`NOT_VERIFY_SSL=false`:** Im Python-Skript schaltet auch dieser Wert die Zertifikatsprüfung *ab*, weil der String nicht leer ist. In `ashsa` bleibt sie aktiv. Wer bisher unbemerkt ohne Prüfung lief (z. B. mit selbstsigniertem Zertifikat), bekommt nach dem Umstieg TLS-Fehler. Lösung: gültiges Zertifikat einsetzen oder bewusst `NOT_VERIFY_SSL=true` setzen.
2. **Long-Lived-Token gewinnt immer:** Ein gesetztes `LONG_LIVED_ACCESS_TOKEN` überschreibt das Token aus dem Account Linking. Bei OAuth-basiertem Linking die Variable weglassen.
3. **Strenges Bool-Parsing:** Werte wie `DEBUG=enabled` oder `NOT_VERIFY_SSL=` mit anderem Inhalt führen zum Startfehler der Lambda. Die Fehlermeldung steht in CloudWatch.
4. **Debug-Logs:** Für ausführliche Logs zusätzlich `RUST_LOG=debug` setzen.

### 11.4 Umstiegs-Checkliste

1. Neue Lambda-Funktion mit Runtime `provided.al2023` anlegen (bzw. bestehende Funktion umstellen: Runtime und Code ersetzen, Handler `bootstrap`).
2. Architektur wählen (arm64 empfohlen) und passendes ZIP hochladen.
3. Umgebungsvariablen übernehmen und prüfen:
   - `BASE_URL` unverändert
   - `NOT_VERIFY_SSL`: bei Wert `false`/`0` die Variable **entfernen** oder bewusst entscheiden (siehe 11.3)
   - `LONG_LIVED_ACCESS_TOKEN`: nur behalten, wenn es für alle Anfragen gelten soll
   - `DEBUG`: nur noch bei Bedarf, dazu `RUST_LOG=debug`
4. Test-Event aus der Alexa-Developer-Console oder ein Discovery-Test aus der Alexa-App auslösen.
5. CloudWatch-Logs auf Startfehler (Konfiguration) und TLS-Fehler prüfen.
6. Skill-Endpunkt auf die ARN der neuen Funktion umstellen; die alte Funktion erst nach erfolgreichem Test löschen.

## 12. Optionale Erweiterung: Retries bei Verbindungsfehlern

Das Python-Skript verhält sich über `urllib3` so, dass Verbindungsfehler bei POST-Anfragen automatisch wiederholt werden. `ashsa` sendet dagegen genau einmal. Der mitgelieferte Patch `connect-retry.patch` rüstet dieses Verhalten nach. Er ist **nicht Teil des Originalprojekts**, sondern eine Ergänzung zu Version 0.2.3.

### 12.1 Verhalten

- Wiederholt werden nur Fehler beim **Verbindungsaufbau** (`reqwest::Error::is_connect()`), etwa „Connection refused“ oder DNS-Fehler.
- **Nie** wiederholt werden Anfragen, die Home Assistant möglicherweise schon erreicht haben (Read-Timeout, Statuscodes wie 500 oder 401, ungültiges JSON). So wird ein Befehl wie „Licht an“ nicht doppelt ausgeführt.
- Standardwerte: 2 Versuche insgesamt, 200 ms Pause dazwischen.
- Jeder Wiederholungsversuch wird als `WARN` mit Versuchsnummer und Fehler geloggt.
- `max_attempts = 0` wird wie 1 behandelt (es wird immer mindestens einmal gesendet).

### 12.2 Änderungen im Code

| Datei | Änderung |
| --- | --- |
| `src/handler.rs` | Neue Struktur `RetryPolicy { max_attempts, delay }` mit `Default`; neues Feld `retry` in `App`; `App::with_retry(policy)`; neue Methode `send_with_retry`, die die bisherige Sendelogik ersetzt |
| `Cargo.toml` | Tokio-Feature `time` (für `tokio::time::sleep`); Dev-Abhängigkeit `tokio` mit `io-util`, `net`, `time` für die Tests |

`App::new(config, client)` bleibt unverändert nutzbar und verwendet die Standardwerte. `main.rs` muss deshalb nicht geändert werden. Wer andere Werte will, ruft `App::new(...).with_retry(RetryPolicy { max_attempts: 3, delay: Duration::from_millis(100) })` auf.

### 12.3 Zeitbudget

Im ungünstigsten Fall dauert ein Aufruf bei den Standardwerten etwa 2 s (Connect-Timeout) + 0,2 s + 2 s = rund 4,2 s, bevor der Fehler an Alexa geht. Alexa wartet in der Praxis nur wenige Sekunden, außerdem muss das Lambda-Timeout dazu passen. Bei Bedarf `max_attempts` auf 1 setzen oder den Connect-Timeout in `client.rs` senken.

### 12.4 Patch anwenden

```bash
cd ashsa
git apply connect-retry.patch      # alternativ: patch -p1 < connect-retry.patch
cargo fmt --check
cargo test --locked
```

### 12.5 Neue Tests

| Test | Prüft |
| --- | --- |
| `connect_failure_is_retried_until_server_is_up` | Der Server startet erst nach 150 ms; der Aufruf gelingt trotzdem durch Wiederholung |
| `connect_failure_gives_up_after_max_attempts` | Nach 3 Versuchen (mit 2 Pausen) kommt `INTERNAL_ERROR` |
| `single_attempt_policy_does_not_retry` | Bei `max_attempts = 1` gibt es keine Pause und keine Wiederholung |
| `zero_max_attempts_still_sends_once` | `max_attempts = 0` sendet trotzdem genau einmal |
| `error_status_from_downstream_is_not_retried` | Ein 500er von Home Assistant wird genau einmal gesendet |

### 12.6 Verifikation und Grenzen

- Geprüft mit Rust 1.91.1: `cargo fmt --check` und `cargo test --locked` laufen durch (24 Tests, davon 5 neu; die 19 bisherigen bestehen unverändert). Die beiden zeitabhängigen Retry-Tests liefen sechs Mal hintereinander stabil.
- Der Test mit dem spät startenden Server reserviert kurz einen freien Port und gibt ihn wieder frei. Theoretisch könnte ein fremder Prozess den Port in dieser Zeit belegen; das ist sehr unwahrscheinlich.
- Nicht getestet ist, ob ein **Connect-Timeout** von `is_connect()` erfasst wird. Nach meiner Kenntnis von `reqwest`/`hyper-util` ist das in der Regel der Fall, dann würde auch ein Timeout beim Verbindungsaufbau wiederholt (Zeitbudget siehe 12.3). Das sollte im echten Betrieb geprüft werden.
- Die Werte sind fest im Code hinterlegt und nicht per Umgebungsvariable einstellbar.
- Alternative: Bibliotheken wie `reqwest-middleware` mit `reqwest-retry` (exponentielles Backoff). Die passende API zu `reqwest` 0.13.4 wurde nicht geprüft.

## 13. Beobachtungen und mögliche Verbesserungen

Aus der Code-Analyse (keine Fehler im Sinne von Abstürzen, aber Punkte, die man kennen sollte):

1. **`DEBUG` allein reicht nicht für sichtbare Debug-Logs.** Der Filter startet mit Standard-Stufe `INFO`. Die `debug!`-Ausgabe erscheint daher nur, wenn zusätzlich `RUST_LOG=debug` (oder ähnlich) gesetzt ist. Die README erwähnt das nicht.
2. **`RUST_LOG` und `AWS_DEFAULT_REGION` sind in der README nicht dokumentiert**, obwohl beide vom Code ausgewertet werden.
3. **Fehlerantworten enthalten keinen Alexa-`header`** (nur `event.payload`). Alexa akzeptiert das in der Regel, ein vollständiges Error-Event mit `header` und `endpoint` wäre aber protokollkonformer.
4. **Timeout:** 10 s Gesamt-Timeout liegt über dem, was Alexa in der Praxis abwartet. Ein niedrigerer Wert würde saubere Fehlermeldungen statt Alexa-Timeouts liefern.
5. **Token-Feld leer/fehlend:** Ist `scope.token` vorhanden, aber leer, wird es unverändert verwendet; erst Home Assistant lehnt es ab.

## 14. Glossar

| Begriff | Bedeutung |
| --- | --- |
| Directive | JSON-Befehl von Alexa (z. B. Discovery, TurnOn) |
| Scope | Abschnitt der Directive mit Authentifizierungsdaten |
| Bearer-Token | Token im `Authorization`-Header |
| Long-Lived Access Token | In Home Assistant erzeugtes, langlebiges Token |
| SBOM | Software Bill of Materials (Abhängigkeitsliste, hier CycloneDX) |
| `provided.al2023` | Lambda-Runtime für eigene Binärdateien auf Amazon Linux 2023 |
