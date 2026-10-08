//! Project-orchestration seed/persist cluster for [`App`] — one cluster
//! extracted from `app.rs` (NORM slice 22-B).
//!
//! This module owns the global-only orchestration lifecycle from the `App`
//! side: the first-open roster/blueprint auto-seed
//! ([`App::ensure_orchestration_seeded`]), the Studio's single-arm Save
//! ([`App::persist_orchestration`] plus its guarded include write
//! [`App::persist_include_blueprint`]), the global-only enforcement banner
//! ([`App::orchestration_import_banner_view`]) and its explicit import
//! action ([`App::run_project_orchestration_import`]). The bodies moved
//! verbatim from `app.rs`; the only edits are the `pub(super)` annotations
//! (the fns were module-private to `app.rs` before the move, so effective
//! visibility is unchanged) — call sites, message routing, and the
//! orchestration tests in `app.rs`'s `mod tests` stay put untouched. The
//! free projection `orchestration_hides_relationships` deliberately stays in
//! `app.rs` (its unit tests build plain `AppConfig` values there) and is in
//! scope here through `use super::*;`.

use super::*;

impl App {
    /// ADR-58/59 (rewritten) Slice 2, AMENDED (global-only orchestration,
    /// smoke follow-up round 2 2026-09): orchestration is persisted to the
    /// GLOBAL config only, and the auto-seed never creates a project config
    /// file — creating a file is an explicit user save, never a side effect
    /// of opening the Studio. Global-only enforcement 2026-09 AMENDED again:
    /// the load path now IGNORES project-layer orchestration keys, so when
    /// the project file declares any of them the seed is skipped entirely —
    /// the Studio banner + the explicit import action own moving those keys
    /// project→global.
    ///
    /// Two independent fills run ONLY against the global config file: the
    /// roster seed (only when its roster was never materialized) and the
    /// blueprint-selection fill (only when `[orchestration]` exists with no
    /// selector). Three global-file shapes for the roster seed:
    ///
    /// 1. **Key present** (`roster_materialized` on the global file) →
    ///    strict no-op for the ROSTER: whether the array is empty (all agents
    ///    deleted) or populated, the global config owns its roster and the
    ///    seed is skipped. The blueprint fill below still runs — roster
    ///    ownership and selection ownership are independent.
    /// 2. Orphan shape — `[orchestration]` present in the global file but
    ///    the roster key never materialized: `seed_agent_roster_only`
    ///    writes ONLY `[multi_agent.custom_agents]` into the global file,
    ///    preserving the existing — possibly user-edited — `[orchestration]`
    ///    table byte-for-byte.
    /// 3. Fresh — no global orchestration at all: the full
    ///    `seed_orchestration_roster` writes `[orchestration]`
    ///    standard-inline + the five agents, unchanged first-run bootstrap.
    ///
    /// Independently of the roster, `ensure_default_blueprint` fills a TOTAL
    /// absence of `name`/`include`/`inline` under an existing
    /// `[orchestration]` with the default standard selection, so the Studio
    /// surface can always activate; any declared selector is never touched.
    ///
    /// A broken global file makes `roster_materialized` report owned so the
    /// seed is never attempted over it (`config_broken` already surfaces
    /// dirty config elsewhere). After either fill, state re-derives from disk
    /// so the first render already resolves the seeded blueprint.
    pub(super) fn ensure_orchestration_seeded(&mut self) {
        // Global-only orchestration (2026-09): when the project file declares
        // ANY load-ignored orchestration key — its old `[orchestration]`
        // table, roster, or model pins — the seed stays silent and the banner
        // + explicit import action own the migration project→global. Seeding
        // the global layer while declared (even though load-ignored now)
        // would race the user's file edits for no benefit. Note the
        // `roster_materialized` (custom_agents key) check alone is no longer
        // sufficient — a project declaring ONLY `[orchestration]` must skip
        // too, and the broader key set covers that shape.
        let project_config =
            self.project_dir.join(concerto_config::legacy::NEW_PROJECT_CONFIG_FILE);
        if !concerto_config::declared_project_orchestration_keys(&project_config).is_empty() {
            return;
        }
        let Some(config_path) = concerto_config::default_config_path() else {
            return;
        };
        // Raw-file ownership test on the global layer: the `custom_agents`
        // key exists in the TOML (even `[]` = every agent deleted). "Key
        // present" means owned — deletions stick and nothing is ever written.
        // The roster seed is skipped then, but the blueprint fill below still
        // runs: roster ownership and blueprint-selection ownership are
        // independent, and a config may own a roster while never having
        // selected a blueprint (the old early return left the Studio in the
        // degraded fallback forever).
        let mut wrote = false;
        if !concerto_config::roster_materialized(&config_path) {
            // Orphan shape (global `[orchestration]` present, roster never
            // materialized) seeds ONLY the agents so the searchable library
            // matches the blueprint's staffing; the existing (possibly
            // user-edited) `[orchestration]` table is preserved byte-for-byte.
            // The raw presence signal avoids re-parsing the merged config —
            // the orphan decision must key on the GLOBAL file only. A failed
            // seed leaves the previous file at the target intact.
            let global_has_orchestration = concerto_config::orchestration_declared(&config_path);
            let seeded = if global_has_orchestration {
                concerto_config::seed_agent_roster_only(&config_path)
            } else {
                concerto_config::seed_orchestration_roster(&config_path)
            };
            if seeded.is_err() {
                // A failed seed leaves the previous file at the target intact;
                // nothing to reconcile then. Broken config is surfaced
                // elsewhere.
                return;
            }
            wrote = true;
        }
        // Blueprint content fill (global-only, roster-independent): when
        // `[orchestration]` exists with NO selector (`name`/`include`/`inline`
        // all absent) write the default standard selection so the Studio
        // surface can always activate. Only total absence is filled — any
        // declared selector is left untouched, so the exactly-one load
        // invariant is safe by construction. A failure (broken/unreadable
        // file) is ignored here; broken config is surfaced via `config_broken`.
        if matches!(concerto_config::ensure_default_blueprint(&config_path), Ok(true)) {
            wrote = true;
        }
        // Per-agent config files (single source of truth): materialize the
        // agents directory once. When it is absent the roster is exported from
        // the just-seeded inline roster (or an existing one), or seeded from
        // the builtin defaults. If it already exists this is a strict no-op —
        // files rule and deletions stick. A failure is logged and ignored: the
        // inline roster stays authoritative for this session (readable, never
        // dropped).
        match concerto_config::ensure_agent_files(&config_path, Some(&project_config)) {
            Ok(outcome) => {
                if outcome.dir_created {
                    wrote = true;
                }
            }
            Err(error) => {
                tracing::error!(
                    %error,
                    "failed to materialize per-agent config files; the inline roster remains authoritative"
                );
            }
        }
        if wrote {
            self.reconcile_config_from_reload();
        }
    }

    /// ADR-58/59 (rewritten) Slice 2 (single-arm Save), AMENDED (global-only
    /// orchestration, smoke follow-up round 2 2026-09): the Studio persists
    /// the blueprint and the agent roster to the GLOBAL config file
    /// (`default_config_path()`), never `<project>/.concerto.toml`. Creating
    /// that file is an explicit user save, and orchestration is global-only
    /// going forward; a project `.concerto.toml` declaring orchestration
    /// keys is ignored at load, and the explicit banner import owns moving
    /// those keys project→global (a Save is refused while they remain).
    ///
    /// While a project config still DECLARES `[orchestration]`, the inline
    /// save is refused with the draft kept: writing the fresher selection to
    /// the global file would leave the stale project-layer selection
    /// merged on top of it (figment layers project over global) and the
    /// exactly-one load seam would reject the mixed selection. The include
    /// path is unaffected — it writes the include file the selection already
    /// loads from, so no project-layer conflict can arise.
    ///
    /// The generic guards run before routing — the draft is kept and nothing
    /// is written when either fails:
    ///
    /// 1. **Validation** — the UI already disables Save while the draft is
    ///    invalid; this belt-and-braces check guards stale queued messages.
    /// 2. **No editable blueprint** — nothing to write (defensive).
    /// 3. **Project-layer conflict** — a project `.concerto.toml` carrying
    ///    `[orchestration]` blocks the inline save (see above).
    ///
    /// Then, by selection source (exactly one of name/include/inline is
    /// guaranteed by `BlueprintSelection`):
    ///
    /// - **include** → the guarded include write (`persist_include_blueprint`,
    ///   target-shadow + unparseable guards), the only path that touches a
    ///   blueprint file.
    /// - **name** (materialize), **inline**, or a defensively-absent
    ///   `[orchestration]` → write the blueprint inline into the global
    ///   config's `[orchestration].blueprint.inline` (`save_inline_blueprint`,
    ///   merge-aware, atomic).
    ///
    /// The agent roster always goes to the global config (`save_agent_roster`
    /// after the blueprint write succeeds). Never navigates and never
    /// switches the surface (Slice 2).
    pub(super) fn persist_orchestration(&mut self) -> Result<(), String> {
        let Some(blueprint) = self.orchestration_studio.blueprint() else {
            return Err("no editable blueprint loaded; nothing was written".to_string());
        };
        if !self.orchestration_studio.validation().ok {
            return Err(
                "blueprint has validation issue(s); the draft is kept, nothing was written"
                    .to_string(),
            );
        }
        let selection = self
            .config
            .as_ref()
            .and_then(|config| config.orchestration.as_ref())
            .map(|orchestration| &orchestration.blueprint);

        // Conflict guard: a project config that still declares
        // `[orchestration]` owns its selection until the user moves or
        // removes it — we never silently delete project data on save.
        let project_config =
            self.project_dir.clone().join(concerto_config::legacy::NEW_PROJECT_CONFIG_FILE);
        if !matches!(selection, Some(selection) if selection.include.is_some())
            && concerto_config::orchestration_declared(&project_config)
        {
            return Err(format!(
                "the project config '{}' still declares an [orchestration] selection; \
                 orchestration is saved to the global config only — move that section \
                 into the global config or remove it from the project file first",
                project_config.display()
            ));
        }
        let config_path = concerto_config::default_config_path()
            .ok_or_else(|| "no global config path available; nothing was written".to_string())?;
        match selection {
            // The blueprint lives in the include file the selection
            // references: the guarded include write, then reload.
            Some(selection) if selection.include.is_some() => {
                self.persist_include_blueprint(&blueprint)?;
            }
            // Inline, bare name (materialize), or defensively-absent
            // `[orchestration]`: the config owns the blueprint — write it
            // back into `[orchestration].blueprint.inline`.
            _ => {
                concerto_config::save_inline_blueprint(&config_path, &blueprint)
                    .map_err(|error| error.to_string())?;
            }
        }

        // ADR-58/59 (rewritten) Slice 3, per-agent files: the agent roster is
        // written to its own files (`<global-config-dir>/agents/<id>.toml`) —
        // the single source of truth. Additions create files; deletions remove
        // them (deletion sticks: the directory stays initialized). The write is
        // atomic per file. The roster has no rulebook of its own, so it is
        // gated on the same blueprint validation that ran up front (a failed
        // blueprint never reaches the files).
        let (roster, _, _) = self.orchestration_studio.persisted_parts();
        let agents_dir = concerto_config::agents_dir_for_config(&config_path)
            .map_err(|error| error.to_string())?;
        concerto_config::save_agent_roster_files(&agents_dir, &roster)
            .map_err(|error| error.to_string())?;

        self.reconcile_config_from_reload();
        Ok(())
    }

    /// The global-only orchestration enforcement banner (2026-09): rendered
    /// above the Studio while the project file declares load-ignored
    /// orchestration keys. Explicit import + session-scoped dismiss; the same
    /// palette/toast idiom as the settings feedback for the danger-family
    /// notice (no hardcoded colors — palette colors, computed alpha only).
    pub(super) fn orchestration_import_banner_view(&self) -> Element<'_, Message> {
        let ts = &self.current_theme.type_scale;
        let sp = &self.current_theme.spacing;
        let palette = &self.current_theme.palette;
        row![
            text(format!(
                "Project config declares orchestration settings ignored at load: {}. \
                 Orchestration is global-only.",
                self.project_orchestration_keys.join(", ")
            ))
            .size(ts.body)
            .color(palette.danger)
            .width(Length::Fill),
            button("Import to global")
                .style(crate::ui::button::primary)
                .on_press(Message::ImportProjectOrchestration),
            button("Dismiss")
                .style(crate::ui::button::secondary)
                .on_press(Message::DismissOrchestrationBanner),
        ]
        .spacing(sp.md)
        .align_y(iced::Alignment::Center)
        .padding(sp.md)
        .into()
    }

    /// The explicit import action for the global-only orchestration
    /// enforcement (2026-09): one user action, started by the Studio's
    /// import banner ([`Message::ImportProjectOrchestration`]). The raw-file
    /// relocation lives in
    /// [`concerto_config::import_project_orchestration_to_global`] (atomic on
    /// both sides, refuses on global-conflict); this handler adds the
    /// user-visible side effects: logging, toast, and a config reload so the
    /// global keys take effect immediately. On conflict nothing moves and
    /// the refusal is toasted with the colliding keys named — user global
    /// data is never silently overwritten, and the stale project keys stay
    /// load-ignored while the user resolves it manually.
    pub(super) fn run_project_orchestration_import(&mut self) {
        let project_config =
            self.project_dir.join(concerto_config::legacy::NEW_PROJECT_CONFIG_FILE);
        let Some(global_path) = concerto_config::default_config_path() else {
            self.toasts
                .push(ToastLevel::Error, "No global config path available; import failed".into());
            return;
        };
        match concerto_config::import_project_orchestration_to_global(&project_config, &global_path)
        {
            Ok(concerto_config::ImportOrchestrationOutcome::Imported { imported }) => {
                tracing::info!(
                    keys = %imported.join(", "),
                    project = %project_config.display(),
                    global = %global_path.display(),
                    "project orchestration keys imported into the global config at user request"
                );
                self.orchestration_banner_dismissed = false;
                self.toasts.push(
                    ToastLevel::Success,
                    format!(
                        "Imported project orchestration keys into the global config: {}",
                        imported.join(", ")
                    ),
                );
                self.reconcile_config_from_reload();
            }
            Ok(concerto_config::ImportOrchestrationOutcome::Conflict { keys }) => {
                tracing::warn!(
                    keys = %keys.join(", "),
                    project = %project_config.display(),
                    "project-orchestration import refused: the global config already declares them"
                );
                self.toasts.push(
                    ToastLevel::Error,
                    format!(
                        "Import refused: the global config already declares {}; orchestration \
                         keys cannot be overwritten — remove them from the project file \
                         manually if intended",
                        keys.join(", ")
                    ),
                );
            }
            Ok(concerto_config::ImportOrchestrationOutcome::NothingToImport) => {
                self.toasts.push(
                    ToastLevel::Info,
                    "Nothing to import — the project config declares no orchestration keys".into(),
                );
            }
            Err(error) => {
                tracing::warn!(%error, "project-orchestration import failed");
                self.toasts.push(ToastLevel::Error, format!("Import failed: {error}"));
            }
        }
    }

    /// The include-file half of [`Self::persist_orchestration`]: write the
    /// edited [`Blueprint`] to the file the active include selection
    /// references, guarded against (a) target shadowing and (b) an
    /// unparseable on-disk file, then reload. Ported from the pre-Slice-2
    /// `persist_blueprint` guards (ADR-59 Decision 3):
    ///
    /// 1. **Target shadowing** — the write target must be the path the config
    ///    would actually load (project dir first, global second, bare-name
    ///    cwd last, mirroring `load_config`'s resolution order). Saving
    ///    anywhere else would silently write a file a later load never reads.
    ///    The project file, the global-dir file (global-first of the same
    ///    resolution), and the bare fallback are allowed; anything else is a
    ///    shadow and refuses.
    /// 2. **Unparseable include** — `save_blueprint` serializes from the
    ///    in-memory model, so a round-trip would silently DROP unknown keys
    ///    the on-disk file carries (`deny_unknown_fields`). The file must
    ///    parse as a valid [`Blueprint`] before any write.
    ///
    /// On failure nothing is written and the draft is kept.
    pub(super) fn persist_include_blueprint(
        &mut self,
        blueprint: &concerto_config::Blueprint,
    ) -> Result<(), String> {
        let include_name = self
            .config
            .as_ref()
            .and_then(|config| config.orchestration.as_ref())
            .and_then(|orchestration| orchestration.blueprint.include.clone())
            .unwrap_or_else(|| concerto_config::BLUEPRINT_INCLUDE_FILE.to_string());

        // Build the same config-dir order `load_config` uses (lib.rs) so
        // `include_write_target` mirrors load-time resolution — project root
        // first, then the global config file's directory.
        let mut config_dirs = vec![self.project_dir.clone()];
        if let Some(path) = concerto_config::default_config_path() {
            if let Some(parent) = path.parent() {
                config_dirs.push(parent.to_path_buf());
            }
        }
        let target = concerto_config::include_write_target(&config_dirs, &include_name);
        let bare_fallback = std::path::Path::new(include_name.as_str()).to_path_buf();
        // A path that is not the project file is only the bare fallback
        // (no candidate exists anywhere, nothing to shadow) or the global-dir
        // include file — which IS a load-time candidate (global-first of the
        // same resolution order). Global-only orchestration 2026-09: with
        // include-file orchestration selectable only from the global layer,
        // the global-dir include file is the common Save target and must be
        // allowed; anything else would shadow the file the config loads.
        let global_dir_target = config_dirs.get(1).map(|dir| dir.join(&include_name));
        let shadowed = target != self.project_dir.join(&include_name)
            && target != bare_fallback
            && Some(&target) != global_dir_target.as_ref();
        if shadowed {
            return Err(format!(
                "the blueprint was loaded from {}; saving would write a file a later load \
                 never reads — save over the loaded file instead",
                target.display()
            ));
        }

        // A round-trip through `save_blueprint` would silently dump unknown
        // keys, so the on-disk file must parse before any write.
        concerto_config::parse_blueprint_file(&target)
            .map_err(|error| format!("{error} — the draft is kept and nothing was written"))?;

        concerto_config::save_blueprint(blueprint, &target).map_err(|error| error.to_string())?;
        Ok(())
    }
}
