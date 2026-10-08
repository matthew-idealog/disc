use crate::config::model::AppConfig;
use anyhow::{Context, Result};
use dirs::config_dir;
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct ConfigStore {
    path: PathBuf,
}

impl ConfigStore {
    pub fn load_default() -> Result<Self> {
        let base = config_dir().context("Could not determine platform config directory")?;
        let root = base.join("disc");
        let legacy_root = base.join("subsonic_tui");

        if !root.exists() && legacy_root.exists() {
            fs::create_dir_all(&root).context("Could not create DISC config directory")?;
            migrate_legacy_config(&legacy_root, &root)?;
        } else {
            fs::create_dir_all(&root).context("Could not create config directory")?;
        }

        Ok(Self {
            path: root.join("config.toml"),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn queue_dir(&self) -> PathBuf {
        self.path
            .parent()
            .map(|parent| parent.join("queues"))
            .unwrap_or_else(|| PathBuf::from("queues"))
    }

    pub fn session_path(&self) -> PathBuf {
        self.path
            .parent()
            .map(|parent| parent.join("last_session.toml"))
            .unwrap_or_else(|| PathBuf::from("last_session.toml"))
    }

    pub fn custom_styles_path(&self) -> PathBuf {
        self.path
            .parent()
            .map(|parent| parent.join("custom-styles.toml"))
            .unwrap_or_else(|| PathBuf::from("custom-styles.toml"))
    }

    pub fn load(&self) -> Result<AppConfig> {
        if !self.path.exists() {
            return Ok(AppConfig::default());
        }
        let text = fs::read_to_string(&self.path)
            .with_context(|| format!("Could not read config file {}", self.path.display()))?;
        let cfg: AppConfig = toml::from_str(&text)
            .with_context(|| format!("Could not parse config file {}", self.path.display()))?;
        Ok(cfg)
    }

    pub fn save(&self, config: &AppConfig) -> Result<()> {
        let text = toml::to_string_pretty(config).context("Could not serialize config")?;
        fs::write(&self.path, text)
            .with_context(|| format!("Could not write config file {}", self.path.display()))?;
        Ok(())
    }
}

fn migrate_legacy_config(legacy_root: &Path, root: &Path) -> Result<()> {
    for name in ["config.toml", "last_session.toml", "custom-styles.toml"] {
        let from = legacy_root.join(name);
        let to = root.join(name);
        if from.exists() && !to.exists() {
            fs::copy(&from, &to).with_context(|| {
                format!("Could not migrate {} to {}", from.display(), to.display())
            })?;
        }
    }

    let legacy_queues = legacy_root.join("queues");
    let queues = root.join("queues");
    if legacy_queues.is_dir() && !queues.exists() {
        copy_dir_recursive(&legacy_queues, &queues)?;
    }

    Ok(())
}

fn copy_dir_recursive(from: &Path, to: &Path) -> Result<()> {
    fs::create_dir_all(to)
        .with_context(|| format!("Could not create directory {}", to.display()))?;
    for entry in fs::read_dir(from)
        .with_context(|| format!("Could not read directory {}", from.display()))?
    {
        let entry = entry?;
        let source = entry.path();
        let target = to.join(entry.file_name());
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            copy_dir_recursive(&source, &target)?;
        } else if file_type.is_file() && !target.exists() {
            fs::copy(&source, &target).with_context(|| {
                format!("Could not migrate {} to {}", source.display(), target.display())
            })?;
        }
    }
    Ok(())
}
