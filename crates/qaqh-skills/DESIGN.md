# QAQ-Harness Skills Runtime v3

Status: **v3 target; P0 implemented, P1/P2 pending**. This replaces the v2
design that described the retired `skills` aggregate tool.

Last updated: 2026-10-27

Scope: `qaqh-skills`, the `skill_*` tools in `qaqh-workspace`, skill injection
in `qaqh-runtime`, and daemon-startup installation of bundled skills.

Implementation snapshot (2026-10-28): P0 is in the tree — `SkillOrigin`,
short-description/sanitization, creation-time catalog injection, `skill_release`
and the empty-envelope path, `~/.agents/skills/qaqh-*` manifest installation,
bundled pack, `qaqh-core` bootstrap, and neutral preview wording. P1/P2 remain
listed at the end of this document.

## 1. Why v3

The v2 runtime was written before the Codex skills implementation was studied.
The code has since moved on, but the design and several user-visible strings did
not. The following was the verified state when v3 work started; most items are
fixed by P0:

- The model-facing tools are `skill_activate`, `skill_list`, and
  `skill_resource`; there is no `skills` aggregate tool.
  (`crates/qaqh-workspace/src/skill.rs:152`, `:177`, `:202`)
- The catalog text still tells the model to call `skills` with
  `action=activate`. (`crates/qaqh-skills/src/lib.rs:621`)
- `SkillContextManager::initial_catalog_text()` is only referenced by a test.
  (`crates/qaqh-runtime/src/agent/state/agent.rs:1253`)
- Production creation paths push only the base system prompt.
  (`crates/qaqh-runtime/src/agent/state/lifecycle.rs:467`, `:512`, `:553`)
- `build_context` consumes `snapshot.requested_annotation`, not
  `snapshot.catalog`. (`crates/qaqh-runtime/src/agent/state/agent.rs:571-620`)
- `apply_ui_operation` rejects `release`; there is no release tool.
  (`crates/qaqh-skills/src/runtime.rs:203-205`)
- `SkillScope` has only `Project` and `User`; there is no bundled origin or
  managed install root.
  (`crates/qaqh-skills/src/lib.rs:41-47`)
- There is no bundled-skill installer, no manifest, and no managed `qaqh-*`
  install root.
- `sync_skill_injection` returns early when the active set is empty, so
  releasing the last skill cannot supersede the previous envelope.
  (`crates/qaqh-runtime/src/agent/state/agent.rs:789-800`)

The parts worth keeping are real: progressive disclosure, catalog/activate/
resource separation, typed activation effects, session-state persistence, body
hot reload, budget admission, and resource path containment.

v3 keeps those, and ports the missing Codex pieces selectively.

## 2. Goals and non-goals

### Goals

- The model automatically sees a compact skill catalog; bodies remain lazy.
- QAQH ships a versioned bundled skill pack that is installed and discovered
  without touching user skills.
- Project/user skills override bundled skills deterministically.
- Activation and release are explicit; releasing the last skill emits an
  explicit empty active set.
- The base prompt stays model- and credential-neutral. QAQH-specific tool usage
  lives in skills, not in provider/credential branches.
- The implementation stays synchronous `std::fs`; no Tokio, `ExecutorFileSystem`,
  `PathUri`, cloud, plugin, or extension API dependency.

### Non-goals for v3

- A full port of `codex-rs/ext/skills`.
- Cloud/executor/plugin skill sources, aliases, package authorities, dynamic
  selectors, or analytics.
- Per-skill generated tool schemas; the fixed `skill_*` surface remains.
- Skills granting permissions. `allowed-tools` is advisory only.
- Dumping every skill body into the system prompt.

## 3. Codex reference: port, adapt, skip

| Area | Codex | v3 decision |
|---|---|---|
| Discovery roots | `$CODEX_HOME/skills`, `~/.agents/skills`, system `.system`, repo roots (`ext/skills/src/host_roots.rs:24-134`) | Adapt. Install the QAQH pack as `~/.agents/skills/qaqh-*`; keep project/user roots. |
| Frontmatter | `name`, `description`, `metadata.short-description`; single-line sanitization (`skills/src/parser.rs`) | Port the parser behavior. |
| Optional metadata | `agents/openai.yaml` with interface, policy, dependencies (`skills/src/metadata.rs`) | Port a small subset later: `short_description`, `policy.allow_implicit_invocation`. Skip dependencies/cloud. |
| Catalog | Compact list, budget, short descriptions, omission notice, aliases (`ext/skills/src/render.rs`, `catalog_prompt.rs`) | Port budget/short-description/omission; skip aliases and dedup groups. |
| Read/list tools | `skills.list` / `skills.read`, package authorities, pagination (`ext/skills/src/tools/`) | Keep QAQH's `skill_list` / `skill_resource`; add pagination later if needed. |
| Activation | Contextual fragments, explicit/implicit selection | Adapt to QAQH's trailing developer envelope and typed effects. |
| Bundled install | `CODEX_HOME/skills/.system`, marker fingerprint, replace dir (`skills/src/lib.rs`) | Adapt to `~/.agents/skills/qaqh-*` with a QAQH manifest/fingerprint; improve atomicity. |
| Scopes | user/repo/system/admin | Adapt to `Project` / `User`; bundled skills are a `SkillOrigin`, not a scope. |
| Required skills | Environment-scoped required names (`ext/skills/src/required.rs`) | Port a small host-side `qaqh-core` required list. |
| Implicit invocation / dynamic selection | Command detection and selectors | Skip for v3. |
| Security | symlink/path containment, trust boundaries | Port the principles, not the async filesystem types. |

Codex does **not** install system skills into `~/.agents/skills`; it installs
them into `$CODEX_HOME/skills/.system` (`codex-rs/skills/src/lib.rs`), while
`~/.agents/skills` is a user discovery root. QAQH intentionally chooses the
global `~/.agents/skills` (your call): it is the cross-agent skills root, so
other agents will see the `qaqh-*` descriptions in their catalogs. They will not
read the bodies unless they choose to. This is an accepted trade-off: simpler
ownership, less isolation, and a small catalog cost for other agents.

## 4. Target architecture

### 4.1 Scopes, origins, and roots

```rust
pub enum SkillScope {
    /// Workspace skills.
    Project,
    /// User home skills, including the shared `~/.agents/skills` root.
    User,
}

pub enum SkillOrigin {
    /// Workspace-authored.
    Project,
    /// User-authored.
    User,
    /// Installed from the QAQH bundled pack under `~/.agents/skills/qaqh-*`.
    Bundled,
}
```

| Scope | Roots | Priority |
|---|---|---|
| `Project` | `<workspace>/.qaqh/skills`, `<workspace>/.agents/skills`, `<workspace>/skills` | highest |
| `User` | `~/.qaqh/skills`, `~/.agents/skills` | lower |

The bundled pack is not a third scope. It lives under the shared
`~/.agents/skills` root as `qaqh-*` directories, so it is discovered as `User`
scope and identified as `SkillOrigin::Bundled` by the QAQH manifest.

Rules:

- Same-name skills are shadowed by the earlier root. Emit a warning diagnostic
  with both paths. Project roots are scanned before user roots; within the user
  roots, `~/.qaqh/skills` is scanned before `~/.agents/skills`.
- Ordering is deterministic: scope rank, then skill name, then path.
- `~/.agents/skills` is the global cross-agent root. QAQH writes only the
  `qaqh-*` directories listed in its manifest; it never deletes or rewrites
  another agent's or user's directories.
- The bundled `qaqh-*` directories are read-only to QAQH file tools. The
  `SkillOrigin::Bundled` marker is derived from the QAQH manifest, not from a
  name prefix alone, so a user-created `qaqh-custom` directory is not
  automatically protected or deleted.
- `skill_roots()` remains the existing project/user roots. The generic-read
  guard already treats every path under them as managed skill content; no
  separate managed root is added.
- Other agents will see `qaqh-*` entries in their skill catalogs. They will not
  read the bodies unless they choose to. This is accepted; if it becomes noisy,
  add an opt-out or a separate managed root later.
- No ancestor/project-root scan in v3. The workspace is already the project
  anchor; add multi-root ancestor discovery only if a real use case appears.

### 4.2 Metadata

```rust
pub struct SkillMetadata {
    pub name: String,
    pub description: String,
    pub short_description: Option<String>,
    pub license: Option<String>,
    pub compatibility: Option<String>,
    pub metadata: BTreeMap<String, String>,
    pub allowed_tools: Vec<String>,
    pub path: PathBuf,
    pub scope: SkillScope,
    pub origin: SkillOrigin,
    pub policy: SkillPolicy,
}

pub struct SkillPolicy {
    /// Advisory: the catalog may describe the skill, but activation is still
    /// explicit. Defaults to true.
    pub allow_implicit_invocation: bool,
}
```

Frontmatter remains Agent Skills-compatible:

```yaml
---
name: qaqh-apply-patch
description: Use when applying a Codex-style patch with apply_patch.
metadata:
  short-description: Apply Codex-style patches safely
allowed-tools: read apply_patch
---
```

Rules:

- `description` is required and must be non-empty.
- `metadata.short-description` is optional. The catalog prefers it over the
  full description.
- `name` and `description` are sanitized to a single line
  (`split_whitespace().join(" ")`). Control characters that could break the
  catalog envelope are rejected or replaced.
- `name` is validated against the Agent Skills grammar
  (`lowercase`, digits, single hyphens) as a warning during compatibility
  discovery; bundled skills must pass strictly in CI.
- `allowed-tools` is parsed into a list and shown in the activation envelope,
  but it never grants permission.
- `origin` is `Bundled` only when the skill directory is listed in
  `~/.agents/skills/.qaqh-bundled.json`; all other project/user skills are
  `Project` or `User`.
- `agents/openai.yaml` is optional P1. When supported, only
  `interface.short_description` and `policy.allow_implicit_invocation` are
  consumed. Dependencies, OAuth, products, and cloud skills are out of scope.
- The path is stored as an absolute path; symlinked `SKILL.md` files are not
  treated as valid skill files.

### 4.3 Catalog

Add:

```rust
pub const SKILL_CATALOG_MARKER: &str = "[QAQH_SKILL_CATALOG_V3]";
pub const SKILL_ACTIVATE_TOOL: &str = "skill_activate";
pub const SKILL_LIST_TOOL: &str = "skill_list";
pub const SKILL_RESOURCE_TOOL: &str = "skill_resource";
pub const SKILL_RELEASE_TOOL: &str = "skill_release";
```

Catalog text shape:

```text
[QAQH_SKILL_CATALOG_V3]
Skills use progressive disclosure. The catalog lists names and descriptions
only. To use a skill, call skill_activate with the exact name; the full body is
returned through the trailing skill context envelope. Use skill_list for
diagnostics and skill_resource for bundled files. Never infer a skill body from
its description, and never call a tool that is not in the current tool list.

<available_skills>
- qaqh-apply-patch: Apply Codex-style patches safely
- qaqh-exec: Run commands and manage background processes
</available_skills>
```

Rules:

- The catalog contains no bodies and no per-skill tool schemas.
- Prefer `short_description`; fall back to `description`.
- Sanitize newlines and control characters; cap each description at 600 chars.
- Sort by scope rank, name, and path. Do not depend on filesystem walk order.
- Total catalog budget is 8,000 characters by default. If a line does not fit,
  count it as omitted and emit one final omitted line. P1 may replace the
  character budget with a token/context-derived budget.
- The catalog must only name tools that actually exist. A unit test should
  assert that the rendered catalog does not contain `skills(` or
  `action=activate`.
- `catalog_revision` is a fingerprint over metadata, paths, and `SKILL.md`
  bytes, not only the rendered text. This makes diagnostics and change
  detection accurate.
- `skill_list` returns the same effective catalog plus diagnostics and the
  revision. It may include `short_description`, `scope`, and source path.
- The catalog is not a security boundary. It is a model hint.

### 4.4 Tools

The fixed model-facing surface is:

| Tool | Args | Behavior |
|---|---|---|
| `skill_list` | none | List effective catalog and diagnostics. |
| `skill_activate` | `{name}` | Load exact skill, budget-check, emit typed activation. |
| `skill_resource` | `{name, path}` | Read a bundled text resource, contained in the skill dir. |
| `skill_release` | `{name}` | Remove an active skill and emit a new envelope. |

`skill_activate`:

- Exact catalog name; unknown -> recoverable `skill_not_available`.
- Already active with the same body hash -> idempotent success, no epoch bump.
- Changed body -> replace, bump epoch, emit update notice.
- Budget failure -> error, do not truncate.
- Return a short status; the full body arrives through the trailing envelope,
  not by re-embedding it in the tool result.

`skill_resource`:

- Catalog-only reads are allowed in v3: the tool resolves an exact catalog name
  and reads a contained resource. Activation remains the way to load
  instructions. An active-only restriction is a P1 option, not required for
  correct progressive disclosure.
- Reject absolute paths, `..`, root/prefix components, directories, symlinks,
  binary data, and files over `MAX_SKILL_BYTES`.
- Canonicalize the skill root and the candidate; require containment.
- P1: add cursor/pagination for large resources. v3 caps the response and
  returns a clear error rather than silently truncating instructions.

`skill_release`:

- New tool. Remove the active entry, increment `context_epoch`, and record a
  notice.
- Idempotent for an already-released skill.
- The runtime must emit an explicit empty active set when the last skill is
  released.
- UI action `release` maps to this operation; the old `apply_ui_operation`
  branch must stop rejecting it.

Keep the `skills` display projection for historical tool records, but do not
register a `skills` model tool.

### 4.5 Runtime state

States remain:

```text
Catalog -> Requested -> Active
                    \-> Unavailable
Active -> (release) -> absent / Catalog
```

Rules:

- Requested is annotation-only; `$name` does not inject a body.
- First ignored requested skill gets a final-system reminder; the second lap
  forces host activation with source `user_forced`.
- Active skills stay active until `skill_release` or budget reclamation. No
  turn-based auto-unload.
- Budget: 10% of effective input capacity, capped at 64K tokens; one skill
  capped at 32K. Bodies are never truncated. Reclaim oldest activation first.
- Hot reload: on each user turn, refresh catalog and reload active bodies.
  Missing/invalid bodies become `Unavailable`; changed bodies bump epoch and
  emit `skill_updated`/`skill_replaced`.
- `bootstrap_required()`: at session creation, auto-activate `qaqh-core` if it
  is present. Source is `bundled`; failure is a warning, not a fatal error.
  This is the only auto-activation in v3.
- `sync_skill_injection` must track whether the previous epoch had active
  skills. If the active set becomes empty, it must inject
  `<active_skills />` rather than returning early.

### 4.6 Context assembly

Per request:

1. persisted base system prompt;
2. persisted skill catalog system message (creation only);
3. conversation history;
4. current user-message clone with workspace and Requested annotations;
5. current-round tool results;
6. trailing developer `<skill_context_envelope>` when the active epoch changed.

Rules:

- The catalog is persisted once at session creation, after the base prompt. It is
  not re-injected every lap and is not a transient slot.
- `push_system` is creation-only. Resume must never front-insert a catalog into a
  non-empty history.
- If a resumed session has no catalog message (legacy session), inject the
  catalog once through the trailing path on the first model lap; accept a
  one-time prefix-cache miss. Do not mutate existing system messages.
- If the catalog changes mid-session, leave the stable prefix alone. The model
  can call `skill_list`; optionally emit a bounded trailing notice. A new
  session gets the new catalog.
- The activation envelope is a trailing developer message, injected once per
  epoch. An empty active set is an explicit envelope, not silence.
- The catalog and envelope are model-visible but are never used as the source of
  truth for skill state; `SkillSessionStateV2` is.

### 4.7 Persistence

- Keep `SkillSessionStateV2` version 2 for compatibility.
- Persist active names, activation order, source, and state; never bodies.
- Release is represented by absence; no tombstone is required.
- On resume, reload active bodies from disk and restore order. Missing bodies
  become `Unavailable`.
- `qaqh-core` is an ordinary active entry when bootstrapped; it is restored like
  any other skill.
- Optional P1: persist `catalog_revision` in session metadata for diagnostics.
- Legacy `[SKILLS]`/`Available skills` system messages are removed on resume;
  they are not used to reconstruct state.

### 4.8 Security

- Bundled skill directories (`~/.agents/skills/qaqh-*` listed in the QAQH
  manifest) are read-only to file tools. The permission engine must reject
  `write`/`edit`/`apply_patch`/`delete` targeting those directories unless an
  explicit maintenance mode is enabled.
- Generic `read`/`grep`/`glob` must reject managed skill files with
  `use_skills_tool`; `managed_skill_for_path` already covers both user roots,
  including the bundled directories under `~/.agents/skills`.
- Skip symlinked skill directories. A `SKILL.md` must be a regular file; do not
  follow a symlink for metadata or body reads.
- Resource reads canonicalize and require containment; no `..`, absolute paths,
  or symlink escapes.
- Sanitize names/descriptions to one line; cap sizes; reject binary resources.
- `allowed-tools` is advisory. Permissions and allowlists are authoritative.
- Skills are untrusted instructions. Bundled skills are trusted by QAQH, but
  still cannot execute code or grant permissions.
- Installation must not follow symlinks, must write through a temp directory,
  and must only replace directories listed in the QAQH manifest.

## 5. Bundled skill pack and installation

### 5.1 Pack layout

```text
crates/qaqh-skills/assets/bundled/
  qaqh-core/SKILL.md
  qaqh-apply-patch/SKILL.md
  qaqh-tools/SKILL.md
  qaqh-exec/SKILL.md
  qaqh-mcp/SKILL.md
  qaqh-subagent/SKILL.md
  qaqh-recovery/SKILL.md
  qaqh-manual/SKILL.md
```

Initial content:

- `qaqh-core`: short, required. Tool selection principles, inspect the current
  schema, never invent tools/parameters, permission/allowlist semantics, when to
  call `skill_list`/`skill_activate`/`skill_release`.
- `qaqh-apply-patch`: Codex patch format; first-hit matching; partial apply;
  dry-run only if the tool schema has it; preview/confirm only if present.
- `qaqh-tools`: decision tree for `read`/`glob`/`grep`/`write`/`edit`/
  `apply_patch`/`exec`/`spy`/`journal`.
- `qaqh-exec`: args/shell/background/process/output limits; do not use exec for
  typed file operations.
- `qaqh-mcp`: aggregate MCP tool/resources/prompts.
- `qaqh-subagent`: spawn/wait/message/task board; no polling loops.
- `qaqh-recovery`: spy/journal/undo/restore.
- `qaqh-manual`: long reference; activate only on demand. Its entrypoint routes
  to `references/apply-patch.md`, `references/tools.md`, and
  `references/runtime.md`; `agents/openai.yaml` carries UI metadata.

`qaqh-core` must stay short (roughly 1-2K tokens). The long manual belongs in
`qaqh-manual` and references, not in every request.

Installed layout:

```text
~/.agents/skills/
  .qaqh-bundled.json          # manifest: version, fingerprint, managed names
  qaqh-core/SKILL.md
  qaqh-apply-patch/SKILL.md
  qaqh-tools/SKILL.md
  qaqh-exec/SKILL.md
  qaqh-mcp/SKILL.md
  qaqh-subagent/SKILL.md
  qaqh-recovery/SKILL.md
  qaqh-manual/SKILL.md
  qaqh-manual/agents/openai.yaml
  qaqh-manual/references/apply-patch.md
  qaqh-manual/references/tools.md
  qaqh-manual/references/runtime.md
```

### 5.2 Install algorithm

```text
install_bundled_skills(home):
  skills_root = home/.agents/skills
  manifest_path = skills_root/.qaqh-bundled.json
  create_dir_all(skills_root)
  fingerprint = stable_hash(pack_version + embedded file paths + bytes)
  manifest = read_manifest(manifest_path) or empty
  if manifest.fingerprint == fingerprint
     and manifest.skipped is empty
     and all manifest.managed dirs exist:
      return
  installed = []
  skipped = []
  for each bundled skill:
      name = qaqh-*
      temp = skills_root/.qaqh-tmp-<pid>-<nonce>/<name>
      write embedded skill dir to temp
      fsync files and temp dir
      if skills_root/<name> exists:
          if name not in manifest.managed:
              warn and skip this skill   # do not overwrite a user dir
              skipped.push(name)
          else:
              remove_dir_all(skills_root/<name>)
      rename(temp, skills_root/<name>)
      installed.push(name)
  for old in manifest.managed - bundled_names:
      remove_dir_all(skills_root/<old>)   # QAQH-owned, no longer in the pack
  write manifest atomically:
      { fingerprint, managed: installed, skipped }
```

Rules:

- Use `include_dir` (as Codex does) or an equivalent compile-time embed.
- Use a stable fingerprint (FNV/SHA over path+bytes), not `DefaultHasher`,
  because `DefaultHasher` is not a stable cross-version contract.
- Create `~/.agents/skills` if it does not exist; this is the requested global
  install root.
- Do not delete the whole `~/.agents/skills` directory. Only replace the
  `qaqh-*` directories listed in the QAQH manifest.
- If a target name exists but is not in the manifest, skip it and warn; never
  overwrite a user- or other-agent-owned directory.
- `manifest.managed` contains only directories QAQH actually owns. A skipped
  collision is recorded in `manifest.skipped` and is retried (with a warning)
  on the next startup; it is never silently claimed.
- On failure, leave the previous directories intact and return a warning. The
  manifest is written last, so a partial install is retried next startup.
- Do not follow symlinks; create directories with user-only permissions where
  the platform supports it.
- Windows directory rename-over-existing needs a remove/rename fallback; guard
  against partial installs with the temp directory and manifest.
- The manifest is a hidden file at the root; discovery scans directories, so it
  does not become a skill. It is the authoritative list for updates and cleanup.

### 5.3 Trigger and ordering

- Daemon startup: `ensure_bundled_skills()` before any `SkillContextManager`
  discovery or session creation.
- CLI/installer: call the same function; it is idempotent.
- Failure: log a warning and continue; the base prompt and user skills still
  work.
- Do not install on every turn. The manifest fingerprint makes startup cheap.

### 5.4 Versioning and user edits

- Bundled skills are QAQH-owned but live in the shared global root. A pack
  update replaces only the manifest's `qaqh-*` directories.
- Users who want to customize should copy a skill to `~/.qaqh/skills` under a
  different name. `~/.qaqh/skills` is scanned before `~/.agents/skills`, so a
  same-name user skill shadows the bundled copy.
- Document that edits inside managed `qaqh-*` directories are overwritten on
  upgrade.
- If a user creates a `qaqh-*` directory outside the manifest, QAQH leaves it
  alone and warns on install.
- Add a pack version constant and a changelog entry.

## 6. Prompt strategy

### 6.1 Base prompt

Keep the base prompt neutral and universal:

- identity, safety, workspace, tool-result semantics;
- "the current tool list and schema are authoritative; do not invent tools or
  parameters";
- "use `skill_list`/`skill_activate` for QAQH-specific recipes";
- no `dry_run` / `apply_patch_preview` / `confirm_apply` specifics;
- no OAuth/credential/provider branches.

Remove the current legacy apply_patch preview paragraph from
`backend_prompt.md`. The detailed forms belong in `qaqh-apply-patch`.

### 6.2 `qaqh-core`

Auto-activated as a required skill. It should say, in effect:

- Inspect the current tool schema before calling; use only tools present.
- Prefer typed file tools over `exec` for file operations.
- `skill_list` is the catalog; `skill_activate` loads a body; `skill_release`
  removes it; `skill_resource` reads bundled files.
- `allowed-tools` is advisory; permissions still apply.
- If a skill and a tool schema disagree, the schema wins.

Keep it short. It is not the full manual.

### 6.3 `qaqh-apply-patch`

Use a conditional table rather than a credential branch:

```text
Inspect the current apply_patch declaration.
- custom/freeform: send raw patch text; do not JSON-wrap; no dry_run.
- function/JSON: use {patch, dry_run} only if the schema has dry_run.
- Use apply_patch_preview/confirm_apply only if those tools are in the current
  tool list. Never invent them.
```

This covers OAuth/Responses and ApiKey/Chat without a runtime special case. The
tool schema still has to be accurate; a skill cannot prevent an invalid
parameter.

### 6.4 Division of responsibility

| Layer | Responsibility |
|---|---|
| Tool schema/description | Exact parameters and tool contract. |
| Base prompt | Universal behavior and safety. |
| `qaqh-core` | QAQH mental model and tool selection. |
| `qaqh-apply-patch` etc. | Recipes and conditional usage. |
| Permission/allowlist | Enforcement. |
| Skills | Guidance; never enforcement. |

## 7. Migration from v2

- Replace every model-visible `skills` action reference with the actual
  `skill_*` names. Add constants and tests.
- Rewrite stale comments in `qaqh-skills/src/lib.rs`, `runtime.rs`, and the
  `SkillEffect` docs.
- Add `SKILL_CATALOG_MARKER`; keep the old `"Available skills"` removal for
  legacy sessions only.
- Inject the catalog in all three creation paths; do not inject it in
  `build_context` on every turn.
- Add `skill_release` and the empty-envelope path; update UI operations.
- Add `SkillOrigin::Bundled` and the installer. Existing `~/.agents/skills` and
  `~/.qaqh/skills` keep working.
- Keep `SkillSessionStateV2`; old sessions restore as before. Sessions without a
  catalog get a trailing catalog once.
- Keep the `skills` display projection for historical records; do not register
  the old model tool.
- Update `backend_prompt.md` to the neutral version and add the bundled skill
  pack.

## 8. Implementation plan

### P0: correctness and baseline

1. **Types and sanitization**
   - Add `SkillOrigin::Bundled`, `SkillPolicy`, `short_description`.
   - Sanitize `name`/`description`; reject control characters; add tests.
   - Add tool-name and catalog-marker constants.

2. **Catalog injection**
   - Push `initial_catalog_text()` in `create_session`,
     `create_session_with_session`, and the resume/new-session path.
   - Resume: keep an existing catalog; if missing, inject once as trailing.
   - Do not front-insert into non-empty history.
   - Add integration tests for create/resume.

3. **Release and empty envelope**
   - Add `skill_release`, `SkillEffect::Release`, `SkillContextManager::release`.
   - Update `apply_ui_operation` to accept `release`.
   - Change `sync_skill_injection` to inject `<active_skills />` when the epoch
     changes to an empty active set.
   - Tests: release one, release last, idempotent release, old envelope
     superseded.

4. **Bundled origin and installer**
   - Add a manifest-backed `SkillOrigin::Bundled` for `~/.agents/skills/qaqh-*`.
   - Add `install.rs` with `include_dir`, `~/.agents/skills/.qaqh-bundled.json`,
     fingerprint, and per-directory temp/rename.
   - Add `assets/bundled/` with at least `qaqh-core` and
     `qaqh-apply-patch`.
   - Call install before sessions in the daemon startup path.
   - Tests: fresh, idempotent, upgrade, failure, preserve user skills, symlink,
     unknown `qaqh-*` directory is skipped.

5. **Fix model-visible strings**
   - Catalog, requested annotation, tool summaries, `DESIGN.md`, and code
     comments.
   - Add a test that the catalog references only registered tools.

### P1: quality and Codex parity

1. `agents/openai.yaml` subset: `short_description`,
   `policy.allow_implicit_invocation`; no dependencies/cloud.
2. Catalog budget: prefer short descriptions; optional token/context budget;
   round-robin description allocation; bounded list output.
3. Required core skill: `bootstrap_required()` and session creation ordering.
4. Neutral base prompt and `qaqh-core`/`qaqh-apply-patch`/`qaqh-tools` content.
5. `skill_resource` pagination/cursor and optional active-only enforcement.
6. Diagnostics/UI: omitted count, catalog revision, install warnings, scope.
7. Bundled-directory write protection and generic-read guard tests.

### P2: optional parity

- Dynamic selection/aliases, dedup groups, remote/cloud/plugin sources.
- Project-root ancestor discovery.
- Visibility controls / opt-out for `qaqh-*` entries in other agents' catalogs.
- Telemetry/analytics.
- Rich `agents/openai.yaml` dependencies.

## 9. Test plan

Unit:

- parser: missing frontmatter, invalid YAML, missing description, short
  description, name sanitization, control characters, CRLF.
- discovery: project > user, bundled-origin detection, shadow diagnostics,
  missing roots, symlinked dirs/files, depth and count limits.
- catalog: short description preference, deterministic ordering, 600-char cap,
  8K budget, omitted line, no stale tool names.
- resource: `..`, absolute, directory, symlink, binary, oversize, containment.
- install: fresh, manifest match, update, atomic failure, user-skill
  preservation, unknown `qaqh-*` skip, no symlink follow.
- runtime: activate idempotence, budget rejection, reclaim, release, empty
  envelope, hot reload, force-after-ignored, restore.

Integration:

- New session contains base + catalog system messages; no envelope until
  activation.
- Activate -> trailing envelope; release -> empty envelope; release last ->
  empty envelope supersedes.
- Resume with catalog -> no duplicate; resume without catalog -> one trailing
  catalog.
- Workspace change -> catalog refresh and skill state restore.
- OAuth-like tool surface: `qaqh-apply-patch` content does not mention
  preview/confirm as available; catalog/activation do not add tools.

Security:

- Writes to manifest-listed `qaqh-*` directories are rejected.
- Generic `read` on bundled/user skill paths returns `use_skills_tool`.
- Symlink `SKILL.md` and symlink resources are rejected.

## 10. Defaults and open decisions

Defaults chosen by this design:

- Bundled install root: `~/.agents/skills/qaqh-*`; `~/.agents/skills` is the
  global cross-agent root, and other agents may see the descriptions.
- Bundled skills are `User` scope with `SkillOrigin::Bundled`; they are
  read-only to QAQH file tools.
- `qaqh-core` is auto-activated; `qaqh-manual` is lazy.
- `skill_resource` may read a catalog-resolved skill without an active
  activation; it never reads outside the skill directory.
- Keep `skill_*` names; no aggregate `skills` model tool.
- Catalog is a creation-time system message; resume uses the persisted message
  or a one-time trailing catalog.

Open for implementation review:

- Whether to move to token/context-derived catalog budgets in P1.
- Whether `agents/openai.yaml` policy should be consumed in P1 or P2.
- Whether bundled directories should be physically read-only or guarded by the
  permission engine only.
