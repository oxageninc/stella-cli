//! Three-scope settings capture, overlay, trust restoration, and authority merge.

use super::authority::{apply_tool_ceiling, restore_project_prompts, restore_project_tools};
use super::managed::managed_settings_path;
use super::*;

/// Append `extra`'s matchers onto `base`, per event. `None + None` stays
/// `None` so a hook-free session carries no hooks handle at all.
fn concat_hooks(base: &mut Option<Hooks>, extra: &Hooks) {
    let target = base.get_or_insert_with(Hooks::default);
    let join = |dst: &mut Option<Vec<_>>, src: &Option<Vec<_>>| {
        if let Some(src) = src {
            dst.get_or_insert_with(Vec::new).extend(src.iter().cloned());
        }
    };
    join(&mut target.session_start, &extra.session_start);
    join(&mut target.pre_tool_use, &extra.pre_tool_use);
    join(&mut target.post_tool_use, &extra.post_tool_use);
}

impl Settings {
    /// Read and parse one scope exactly once. A missing file is represented by
    /// an empty captured snapshot; malformed content remains a named error.
    fn load_scope(path: &Path) -> Result<Self, String> {
        let contents = match std::fs::read_to_string(path) {
            Ok(contents) => contents,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self::default());
            }
            Err(error) => return Err(format!("cannot read {}: {error}", path.display())),
        };
        let scope: Settings = serde_json::from_str(&contents)
            .map_err(|error| format!("invalid settings file {}: {error}", path.display()))?;
        for (id, entry) in &scope.providers {
            if let Some(stated) = &entry.id
                && stated != id
            {
                return Err(format!(
                    "settings file {}: providers.{id} declares id `{stated}` — the \
                     entry's id must match its key",
                    path.display()
                ));
            }
        }
        Ok(scope)
    }

    /// Overlay one already-parsed scope without touching the filesystem.
    fn overlay_scope(&mut self, scope: &Settings) {
        for (id, entry) in &scope.providers {
            self.providers.entry(id.clone()).or_default().overlay(entry);
        }
        if let Some(hooks) = &scope.hooks {
            concat_hooks(&mut self.hooks, hooks);
        }
        if let Some(mcp) = &scope.mcp {
            let target = self.mcp.get_or_insert_with(McpSettings::default);
            if let Some(url) = &mcp.registry_url {
                target.registry_url = Some(url.clone());
            }
        }
        if let Some(tools) = &scope.tools {
            let target = self.tools.get_or_insert_with(ToolsSettings::default);
            if let Some(bash) = tools.bash {
                target.bash = Some(bash);
            }
            if let Some(web) = tools.web {
                target.web = Some(web);
            }
        }
        if let Some(engine) = &scope.agent_engine_config {
            self.agent_engine_config
                .get_or_insert_with(AgentEngineConfig::default)
                .overlay(engine);
        }
        if let Some(authority) = scope.managed_authority {
            self.managed_authority = Some(authority);
        }
    }

    fn merge_snapshots(scopes: &[&Settings]) -> Self {
        let mut merged = Self::default();
        for scope in scopes {
            merged.overlay_scope(scope);
        }
        merged
    }

    /// Resolve authority from three immutable snapshots captured by
    /// [`Settings::load`]. This function performs no I/O, so the bytes used to
    /// compute ceilings are the same bytes used to produce the merged config.
    pub(super) fn merge_captured_scopes(
        user: &Settings,
        managed: &Settings,
        project: &Settings,
        trust: ProjectTrust,
    ) -> Self {
        let trusted_only = Self::merge_snapshots(&[user, managed]);
        let mut merged = Self::merge_snapshots(&[user, managed, project]);
        let authority = AuthorityPolicy::compute(
            managed.managed_authority.as_ref(),
            managed.tools.as_ref(),
            trust.credentials,
        );

        if !trust.hooks && project.hooks.is_some() {
            merged.hooks = trusted_only.hooks.clone();
        }
        if !trust.credentials {
            for (id, project_entry) in &project.providers {
                let touches_credentials = project_entry.base_url.is_some()
                    || project_entry.api_key.is_some()
                    || project_entry.api_key_env.is_some();
                if !touches_credentials {
                    continue;
                }
                let trusted_entry = trusted_only.providers.get(id);
                if let Some(effective) = merged.providers.get_mut(id) {
                    effective.base_url = trusted_entry.and_then(|entry| entry.base_url.clone());
                    effective.api_key = trusted_entry.and_then(|entry| entry.api_key.clone());
                    effective.api_key_env =
                        trusted_entry.and_then(|entry| entry.api_key_env.clone());
                }
            }
            if project
                .mcp
                .as_ref()
                .and_then(|mcp| mcp.registry_url.as_ref())
                .is_some()
                && let Some(mcp) = merged.mcp.as_mut()
            {
                mcp.registry_url = trusted_only
                    .mcp
                    .as_ref()
                    .and_then(|trusted| trusted.registry_url.clone());
            }
            restore_project_tools(&mut merged, &trusted_only, project);
        }
        if !authority.project_prompts_allowed {
            restore_project_prompts(&mut merged, &trusted_only, project);
        }
        apply_tool_ceiling(&mut merged, authority);
        merged.managed_authority = managed.managed_authority;
        merged.enterprise_telemetry = managed.enterprise_telemetry.clone();
        merged.authority_policy = authority;
        merged
    }

    /// Load and merge the standard scope chain for `workspace_root`.
    /// Missing files are the common case and skipped silently; an existing
    /// file that fails to parse is a hard error naming the file.
    ///
    /// **The project scope is a trust boundary.** A cloned repo's
    /// `.stella/settings.json` is untrusted input, and two kinds of entry in
    /// it can act on your behalf without you asking:
    ///
    /// - **Hooks** run arbitrary shell commands automatically.
    /// - **Credential routing** — a provider entry's `base_url`, `api_key`,
    ///   or `api_key_env`, and the `mcp.registry_url` — decides *where your
    ///   API key is sent* and *where server configs are fetched from*.
    ///   Overriding a built-in provider's `base_url` (or repointing its
    ///   `api_key_env` at another env var) silently exfiltrates the real
    ///   key to an attacker-controlled host on the first model call. That
    ///   violates the "outbound traffic only to the user-chosen provider"
    ///   invariant just as surely as a phone-home would.
    ///
    /// The user and org-managed scopes always load. Project hooks and
    /// credential-routing fields load only when explicitly trusted; project
    /// tool switches and replacement prompts are likewise restored from the
    /// trusted scopes while untrusted. Managed denials remain ceilings even
    /// after explicit repository trust.
    pub fn load(workspace_root: &Path) -> Result<Self, String> {
        if filesystem_settings_disabled() {
            return Ok(Self::default());
        }

        let user = match user_settings_path() {
            Some(path) => Self::load_scope(&path)?,
            None => Self::default(),
        };
        let managed_path = managed_settings_path();
        let managed = Self::load_managed_scope(&managed_path)?;
        let project_path = project_settings_path(workspace_root);
        let project = Self::load_scope(&project_path)?;
        let trust = project_trust();
        let merged = Self::merge_captured_scopes(&user, &managed, &project, trust);

        if !trust.hooks && project.hooks.is_some() {
            eprintln!(
                "  ! project hooks in {} were NOT loaded — set STELLA_PROJECT_HOOKS=1 \
                 (or STELLA_TRUST_PROJECT=1) to trust this repo's hooks",
                project_path.display()
            );
        }

        if !trust.credentials {
            let mut redacted: Vec<String> = Vec::new();
            for (id, pentry) in &project.providers {
                let touches_credentials = pentry.base_url.is_some()
                    || pentry.api_key.is_some()
                    || pentry.api_key_env.is_some();
                if !touches_credentials {
                    continue;
                }
                // `id` is attacker-controlled repo text — escape it so it
                // can't smuggle terminal control sequences into stderr.
                redacted.push(format!("providers.{}", id.escape_debug()));
            }

            let project_registry = project.mcp.as_ref().and_then(|m| m.registry_url.as_ref());
            if project_registry.is_some() {
                redacted.push("mcp.registry_url".to_string());
            }

            if !redacted.is_empty() {
                eprintln!(
                    "  ! credential-routing fields in {} were IGNORED ({}) — set \
                     STELLA_TRUST_PROJECT=1 to let this repo redirect where your API key \
                     is sent",
                    project_path.display(),
                    redacted.join(", "),
                );
            }
        }
        Ok(merged)
    }

    /// Merge the files at `paths`, later paths taking precedence. Split out
    /// from [`Settings::load`] so tests can drive the merge over fixtures
    /// without touching `$HOME` or `/etc`.
    #[cfg(test)]
    pub fn load_from(paths: &[PathBuf]) -> Result<Self, String> {
        let mut merged = Settings::default();
        for path in paths {
            let scope = Self::load_scope(path)?;
            merged.overlay_scope(&scope);
        }
        Ok(merged)
    }
}
