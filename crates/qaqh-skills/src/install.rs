//! Installation of the QAQH bundled skill pack.
//!
//! The pack lives in the shared cross-agent root:
//!
//! ```text
//! ~/.agents/skills/qaqh-*/
//! ~/.agents/skills/.qaqh-bundled.json
//! ```
//!
//! The manifest is the ownership boundary. We only replace directories that
//! QAQH listed there; a user-created `qaqh-*` directory is skipped.

use std::fs;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use include_dir::{Dir, DirEntry, include_dir};

use crate::{
    BUNDLED_MANIFEST_FILENAME, BundledManifest, bundled_manifest_path, content_hash,
    is_safe_managed_name,
};

static BUNDLED: Dir<'_> = include_dir!("$CARGO_MANIFEST_DIR/assets/bundled");

pub const PACK_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstallReport {
    pub changed: bool,
    pub installed: Vec<String>,
    pub skipped: Vec<String>,
    pub warnings: Vec<String>,
    pub fingerprint: String,
}

/// Install or repair the embedded pack under `<home>/.agents/skills`.
///
/// `home` is explicit so tests can use a real temporary directory rather than
/// mocking the filesystem.
pub fn ensure_bundled_skills(home: &Path) -> Result<InstallReport, String> {
    let skills_root = home.join(".agents/skills");
    if fs::symlink_metadata(&skills_root)
        .map(|metadata| metadata.file_type().is_symlink())
        .unwrap_or(false)
    {
        return Err(format!(
            "refusing to install through symlinked skills root: {}",
            skills_root.display()
        ));
    }
    fs::create_dir_all(&skills_root)
        .map_err(|error| format!("create {}: {error}", skills_root.display()))?;

    let manifest_path = bundled_manifest_path(home);
    let old = BundledManifest::load(home);
    let fingerprint = pack_fingerprint();
    let names = bundled_names();

    if old.version == PACK_VERSION
        && old.fingerprint == fingerprint
        && old.skipped.is_empty()
        && old.managed.iter().all(|name| skills_root.join(name).is_dir())
    {
        return Ok(InstallReport {
            changed: false,
            installed: old.managed,
            skipped: Vec::new(),
            warnings: Vec::new(),
            fingerprint,
        });
    }

    let mut installed = Vec::new();
    let mut skipped = Vec::new();
    let mut warnings = Vec::new();
    let temp_root = skills_root.join(format!(
        ".qaqh-tmp-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |duration| duration.as_nanos())
    ));
    fs::create_dir_all(&temp_root)
        .map_err(|error| format!("create {}: {error}", temp_root.display()))?;

    for dir in BUNDLED.entries() {
        let DirEntry::Dir(skill) = dir else {
            continue;
        };
        let Some(name) = skill.path().file_name().and_then(|name| name.to_str()) else {
            warnings.push(format!("ignore bundled entry without a name: {}", skill.path().display()));
            continue;
        };
        if name.starts_with('.') || name.contains('/') || name.contains('\\') {
            warnings.push(format!("ignore unsafe bundled entry: {name}"));
            continue;
        }
        let target = skills_root.join(name);
        if let Ok(metadata) = fs::symlink_metadata(&target) {
            if metadata.file_type().is_symlink() {
                warnings.push(format!("skip symlink target: {}", target.display()));
                skipped.push(name.to_string());
                continue;
            }
            if !old.is_managed(name) {
                warnings.push(format!(
                    "skip {}: existing directory is not owned by QAQH",
                    target.display()
                ));
                skipped.push(name.to_string());
                continue;
            }
            if target.is_dir() {
                fs::remove_dir_all(&target)
                    .map_err(|error| format!("remove old {}: {error}", target.display()))?;
            } else {
                fs::remove_file(&target)
                    .map_err(|error| format!("remove old {}: {error}", target.display()))?;
            }
        }

        let temp_target = temp_root.join(name);
        write_dir(skill, &temp_target)?;
        fs::rename(&temp_target, &target).map_err(|error| {
            format!(
                "rename {} -> {}: {error}",
                temp_target.display(),
                target.display()
            )
        })?;
        installed.push(name.to_string());
    }

    // Remove QAQH-owned directories that disappeared from the pack. Never
    // touch an unlisted directory, even if its name starts with qaqh-.
    for old_name in &old.managed {
        if names.iter().any(|name| name == old_name) {
            continue;
        }
        if !is_safe_managed_name(old_name) {
            warnings.push(format!("ignore unsafe managed name in manifest: {old_name:?}"));
            continue;
        }
        let target = skills_root.join(old_name);
        if fs::symlink_metadata(&target)
            .map(|metadata| metadata.is_dir() && !metadata.file_type().is_symlink())
            .unwrap_or(false)
        {
            if let Err(error) = fs::remove_dir_all(&target) {
                warnings.push(format!("remove stale {}: {error}", target.display()));
            }
        }
    }

    let manifest = BundledManifest {
        version: PACK_VERSION,
        fingerprint: fingerprint.clone(),
        managed: installed.clone(),
        skipped: skipped.clone(),
    };
    write_manifest(&manifest_path, &manifest)?;
    let _ = fs::remove_dir_all(&temp_root);

    Ok(InstallReport {
        changed: true,
        installed,
        skipped,
        warnings,
        fingerprint,
    })
}

pub fn ensure_bundled_skills_default() -> Result<InstallReport, String> {
    let home = qaqh_types::platform::home_dir();
    ensure_bundled_skills(&home)
}

fn bundled_names() -> Vec<String> {
    BUNDLED
        .entries()
        .iter()
        .filter_map(|entry| match entry {
            DirEntry::Dir(dir) => dir
                .path()
                .file_name()
                .and_then(|name| name.to_str())
                .map(str::to_string),
            DirEntry::File(_) => None,
        })
        .collect()
}

fn pack_fingerprint() -> String {
    let mut items = Vec::new();
    collect_fingerprint(&BUNDLED, &mut items);
    items.sort_by(|a, b| a.0.cmp(&b.0));
    let mut input = format!("v{PACK_VERSION}\n");
    for (path, hash) in items {
        input.push_str(&path);
        input.push('\0');
        if let Some(hash) = hash {
            input.push_str(&hash);
        }
        input.push('\n');
    }
    content_hash(&input)
}

fn collect_fingerprint(dir: &Dir<'_>, items: &mut Vec<(String, Option<String>)>) {
    for entry in dir.entries() {
        match entry {
            DirEntry::Dir(subdir) => {
                items.push((subdir.path().to_string_lossy().into_owned(), None));
                collect_fingerprint(subdir, items);
            }
            DirEntry::File(file) => {
                let hash = content_hash(&String::from_utf8_lossy(file.contents()));
                items.push((file.path().to_string_lossy().into_owned(), Some(hash)));
            }
        }
    }
}

fn write_dir(dir: &Dir<'_>, dest: &Path) -> Result<(), String> {
    fs::create_dir_all(dest).map_err(|error| format!("create {}: {error}", dest.display()))?;
    for entry in dir.entries() {
        let relative = match entry {
            DirEntry::Dir(subdir) => subdir.path().strip_prefix(dir.path()).unwrap_or(subdir.path()),
            DirEntry::File(file) => file.path().strip_prefix(dir.path()).unwrap_or(file.path()),
        };
        let target = dest.join(relative);
        match entry {
            DirEntry::Dir(subdir) => write_dir(subdir, &target)?,
            DirEntry::File(file) => {
                if let Some(parent) = target.parent() {
                    fs::create_dir_all(parent)
                        .map_err(|error| format!("create {}: {error}", parent.display()))?;
                }
                fs::write(&target, file.contents())
                    .map_err(|error| format!("write {}: {error}", target.display()))?;
                if let Ok(file) = fs::File::open(&target) {
                    let _ = file.sync_all();
                }
            }
        }
    }
    Ok(())
}

fn write_manifest(path: &Path, manifest: &BundledManifest) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|error| format!("create {}: {error}", parent.display()))?;
    }
    let text = serde_json::to_string_pretty(manifest)
        .map_err(|error| format!("serialize skill manifest: {error}"))?;
    let temp = path.with_extension(format!(
        "tmp-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |duration| duration.as_nanos())
    ));
    fs::write(&temp, format!("{text}\n"))
        .map_err(|error| format!("write {}: {error}", temp.display()))?;
    if path.exists() {
        fs::remove_file(path)
            .map_err(|error| format!("replace {}: {error}", path.display()))?;
    }
    fs::rename(&temp, path).map_err(|error| {
        format!(
            "rename {} -> {}: {error}",
            temp.display(),
            path.display()
        )
    })
}

/// Test helper: the manifest filename is part of the public contract.
pub fn manifest_filename() -> &'static str {
    BUNDLED_MANIFEST_FILENAME
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::discover_with_home;
    use crate::SkillOrigin;
    use std::path::PathBuf;

    fn write_user_skill(home: &Path, name: &str, body: &str) -> PathBuf {
        let dir = home.join(".agents/skills").join(name);
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("SKILL.md"),
            format!("---\nname: {name}\ndescription: user skill\n---\n\n{body}"),
        )
        .unwrap();
        dir
    }

    #[test]
    fn installs_into_agents_and_discovers_as_bundled() {
        let home = tempfile::tempdir().unwrap();
        let report = ensure_bundled_skills(home.path()).unwrap();
        assert!(report.changed);
        assert!(report.installed.iter().any(|name| name == "qaqh-core"));
        assert!(home.path().join(".agents/skills/qaqh-core/SKILL.md").is_file());
        assert!(bundled_manifest_path(home.path()).is_file());

        let workspace = home.path().join("workspace");
        fs::create_dir_all(&workspace).unwrap();
        let catalog = discover_with_home(&workspace, home.path());
        let core = catalog
            .skills
            .iter()
            .find(|skill| skill.name == "qaqh-core")
            .expect("installed qaqh-core");
        assert_eq!(core.scope, crate::SkillScope::User);
        assert_eq!(core.origin, SkillOrigin::Bundled);
    }

    #[test]
    fn every_bundled_skill_parses_and_is_marked_bundled() {
        let home = tempfile::tempdir().unwrap();
        ensure_bundled_skills(home.path()).unwrap();
        let workspace = home.path().join("workspace");
        fs::create_dir_all(&workspace).unwrap();
        let catalog = discover_with_home(&workspace, home.path());
        let names = catalog
            .skills
            .iter()
            .map(|skill| skill.name.as_str())
            .collect::<Vec<_>>();
        for expected in [
            "qaqh-core",
            "qaqh-apply-patch",
            "qaqh-ask",
            "qaqh-tools",
            "qaqh-exec",
            "qaqh-mcp",
            "qaqh-subagent",
            "qaqh-recovery",
            "qaqh-manual",
        ] {
            assert!(names.contains(&expected), "missing bundled skill {expected}");
        }
        assert!(
            catalog
                .skills
                .iter()
                .all(|skill| skill.origin == SkillOrigin::Bundled)
        );
        assert!(
            catalog
                .diagnostics
                .iter()
                .all(|diagnostic| diagnostic.severity != crate::DiagnosticSeverity::Error)
        );
    }

    #[test]
    fn qaqh_manual_installs_previewable_references() {
        let home = tempfile::tempdir().unwrap();
        ensure_bundled_skills(home.path()).unwrap();
        let manual = home.path().join(".agents/skills/qaqh-manual");
        assert!(manual.join("agents/openai.yaml").is_file());
        assert!(manual.join("references/apply-patch.md").is_file());
        assert!(manual.join("references/tools.md").is_file());
        assert!(manual.join("references/runtime.md").is_file());

        let activation = crate::load_named(home.path(), "qaqh-manual").unwrap();
        assert!(activation.body.contains("Provider-neutral apply_patch"));
        assert!(
            activation
                .resources
                .iter()
                .any(|path| path.ends_with("references/apply-patch.md"))
        );
        let patch_reference = crate::read_resource(
            home.path(),
            "qaqh-manual",
            std::path::Path::new("references/apply-patch.md"),
        )
        .unwrap();
        assert!(patch_reference.content.contains("*** Begin Patch"));
        assert!(patch_reference.content.contains("custom / freeform"));
    }

    #[test]
    fn second_install_is_idempotent() {
        let home = tempfile::tempdir().unwrap();
        let first = ensure_bundled_skills(home.path()).unwrap();
        assert!(first.changed);
        let second = ensure_bundled_skills(home.path()).unwrap();
        assert!(!second.changed);
        assert_eq!(second.installed, first.installed);
    }

    #[test]
    fn qaqh_home_skill_is_user_origin_and_shadows_bundled() {
        let home = tempfile::tempdir().unwrap();
        ensure_bundled_skills(home.path()).unwrap();
        let user = write_user_skill(home.path(), "qaqh-core", "USER_OVERRIDE");
        // Move the user override into ~/.qaqh/skills, which is scanned before
        // ~/.agents/skills.
        let qaqh_skill = home.path().join(".qaqh/skills/qaqh-core");
        std::fs::create_dir_all(qaqh_skill.parent().unwrap()).unwrap();
        std::fs::rename(&user, &qaqh_skill).unwrap();

        let workspace = home.path().join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();
        let catalog = discover_with_home(&workspace, home.path());
        let core = catalog
            .skills
            .iter()
            .find(|skill| skill.name == "qaqh-core")
            .expect("qaqh-core");
        assert_eq!(core.origin, SkillOrigin::User);
        assert_eq!(core.scope, crate::SkillScope::User);
        assert!(core.path.starts_with(home.path().join(".qaqh/skills")));
    }

    #[test]
    fn unknown_qaqh_directory_is_preserved_and_skipped() {
        let home = tempfile::tempdir().unwrap();
        let user = write_user_skill(home.path(), "qaqh-custom", "USER_BODY");
        let report = ensure_bundled_skills(home.path()).unwrap();
        assert!(report.skipped.is_empty());
        assert!(user.join("SKILL.md").is_file());
        assert_eq!(
            fs::read_to_string(user.join("SKILL.md")).unwrap(),
            "---\nname: qaqh-custom\ndescription: user skill\n---\n\nUSER_BODY"
        );
        let manifest = BundledManifest::load(home.path());
        assert!(!manifest.is_managed("qaqh-custom"));
    }

    #[test]
    fn managed_name_collision_is_not_overwritten() {
        let home = tempfile::tempdir().unwrap();
        let user = write_user_skill(home.path(), "qaqh-core", "USER_CORE");
        let report = ensure_bundled_skills(home.path()).unwrap();
        assert!(report.skipped.iter().any(|name| name == "qaqh-core"));
        assert!(user.join("SKILL.md").is_file());
        assert_eq!(
            fs::read_to_string(user.join("SKILL.md")).unwrap(),
            "---\nname: qaqh-core\ndescription: user skill\n---\n\nUSER_CORE"
        );
        let manifest = BundledManifest::load(home.path());
        assert!(!manifest.is_managed("qaqh-core"));
        assert!(manifest.skipped.iter().any(|name| name == "qaqh-core"));
    }

    #[test]
    fn stale_managed_directory_is_removed() {
        let home = tempfile::tempdir().unwrap();
        let manifest = BundledManifest {
            version: 0,
            fingerprint: "old".into(),
            managed: vec!["qaqh-old".into()],
            skipped: Vec::new(),
        };
        write_manifest(&bundled_manifest_path(home.path()), &manifest).unwrap();
        let stale = home.path().join(".agents/skills/qaqh-old");
        fs::create_dir_all(&stale).unwrap();
        fs::write(stale.join("SKILL.md"), "stale").unwrap();

        ensure_bundled_skills(home.path()).unwrap();
        assert!(!stale.exists());
    }

    #[test]
    fn unsafe_manifest_name_cannot_delete_outside_skills_root() {
        let home = tempfile::tempdir().unwrap();
        let outside = home.path().join("outside");
        fs::create_dir_all(&outside).unwrap();
        fs::write(outside.join("keep.txt"), "keep").unwrap();
        let manifest = BundledManifest {
            version: 0,
            fingerprint: "old".into(),
            managed: vec!["../outside".into()],
            skipped: Vec::new(),
        };
        write_manifest(&bundled_manifest_path(home.path()), &manifest).unwrap();

        let report = ensure_bundled_skills(home.path()).unwrap();
        assert!(outside.join("keep.txt").is_file());
        assert!(
            report
                .warnings
                .iter()
                .any(|warning| warning.contains("unsafe managed name"))
        );
    }

    #[test]
    fn manifest_filename_is_stable() {
        assert_eq!(manifest_filename(), ".qaqh-bundled.json");
    }
}
