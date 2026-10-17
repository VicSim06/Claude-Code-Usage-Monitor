# CLAUDE.md — Usage widget

Dev « usage-widget » (Usage widget) du registre NOBRAIN. Il est rattaché à la branche « console-claude » du projet « Outils Claude » (casquette Perso).
- Documents et livrables : `C:\Users\vsimo\Desktop\CLAUDE\_OUTILS\CONSOLE CLAUDE`.
- Roadmap (tâches, phases, versions) : `C:\Users\vsimo\Desktop\CLAUDE\_OUTILS\NOBRAIN\suivi.yaml`. Une tâche se cite `t:<id>` dans le commit et la PR.
- Réponses en français ; code, commits et CHANGELOG en anglais, comme l'existant.

## Ce que fait le widget
Widget Rust de la barre des tâches Windows : quotas et heures de reset de Claude Code (et Codex, Antigravity, OpenCode Go, Cursor).
Ajouts du fork : CPU, RAM, réseau, état d'OmniRoute, de claude-mem et de NOBRAIN, crédits Firecrawl, consoles Claude Code ouvertes.
Les lignes live sont écrites dans `%APPDATA%\ClaudeCodeUsageMonitor\live-status.json` (lu par d'autres outils). Réglages : `settings.json` dans le même dossier.

## Architecture
- `src/main.rs` : point d'entrée ; `window/` : fenêtre dans la barre des tâches ; `dashboard.rs` + `studio_app/` : réglages et Theme Studio (egui).
- `poller/` : un module par fournisseur ; `local_services.rs` : lecteur du moteur NOBRAIN, en lecture seule (état des services par `sante`, consoles ouvertes par `jour`) ; `system_metrics/`, `firecrawl.rs`, `live_status.rs`.
- `theme_engine/` + `themes/*.json` : thèmes et bindings (`system.*`, `services.*`, `consoles.*`) ; `localization/locales/*.toml` : 14 langues.
- `build.rs` génère les locales (mêmes clés que `en.toml`, sinon le build casse), sous-ensembles de polices, icône et manifeste.
- `updater.rs` prend la source des mises à jour dans `package.repository` de `Cargo.toml`.

## Commandes
- Build : `cargo build --release` (exe : `target\release\claude-code-usage-monitor.exe`). Tests : `cargo test`. Diagnostic : `claude-code-usage-monitor --diagnose`.
- Le build release dure 9 à 11 min (LTO) et verrouille `target`. Lancer tests et clippy avec un autre `CARGO_TARGET_DIR`.
- Fermer le widget avant un build release : il tourne depuis `target\release\` (pas d'installation séparée) et verrouille l'exe.

## Release (fork, pas d'Actions)
Les GitHub Actions ne tournent pas sur le fork : pousser un tag `v*` ne publie rien.
1. PR qui monte la version dans `Cargo.toml`, `Cargo.lock` et `CHANGELOG.md`. Victor fait le merge, sauf demande explicite.
2. Tag sur `main` (tags légers), vérifier avec `git describe --tags --exact-match`, puis `cargo build --release`.
3. `gh release create vX --latest --title "vX — <thème>"` avec l'exe attaché. Sans l'exe, l'updater ne voit pas la mise à jour.
Avant tout `gh pr` ou `gh release` : `gh repo set-default --view` doit répondre `VicSim06/Claude-Code-Usage-Monitor`.

## Pièges connus
- Fork de `CodeZeno/Claude-Code-Usage-Monitor` (remote `upstream`) ; `origin` = `VicSim06`. Le hunk `package.repository` du `Cargo.toml` pointe sur le fork : le retirer de toute PR vers upstream.
- Deux tests `studio_app` échouent déjà sur `main` (Windows en français) : `version_text_and_trailing_icon_share_one_update_button` et `diagnostics_page_has_logging_controls_and_menu_version`. Ce ne sont pas des régressions.
- Services locaux : toujours `127.0.0.1`, jamais `localhost` (OmniRoute écoute en IPv4 seulement ; `localhost` coûte ~1,4 s).
- « Start with Windows » enregistre le dernier exe lancé (clé HKCU Run `ClaudeCodeUsageMonitor`). Après un test en `target\debug`, relancer la release et faire vérifier la clé par Victor.

## Règles
- Secrets : noms seulement, jamais de valeur (`FIRECRAWL_API_KEY`, `CURSOR_SESSION_TOKEN`, `WINGETCREATE_GITHUB_TOKEN`).
- Ne pas commiter `.claude/` ; `AGENTS.md`, `workflow.cmd` et `/manifests/` sont ignorés exprès.
- Toute nouvelle chaîne d'interface est traduite dans toutes les locales.
