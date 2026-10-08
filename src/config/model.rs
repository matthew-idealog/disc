use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ServerConfig {
    pub name: String,
    pub alias: String,
    pub base_url: String,
    pub username: String,
    pub password: String,
    #[serde(default = "default_search_timeout_seconds")]
    pub search_timeout_seconds: u64,
}

fn default_theme() -> String {
    // Style 4 is the preferred default for new installs: multi-soft.
    "multi-soft".to_string()
}

fn default_playlist_multicolour() -> bool {
    true
}

fn default_result_multicolour() -> bool {
    false
}

fn default_download_overwrite() -> bool {
    false
}

fn default_advanced_status() -> bool {
    false
}

fn default_queue_follow() -> bool {
    false
}

fn default_messages_visible() -> bool {
    true
}

fn default_gapless_playback() -> bool {
    false
}

fn default_media_keys_enabled() -> bool {
    false
}

pub fn default_search_timeout_seconds() -> u64 {
    60
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppConfig {
    pub primary_alias: Option<String>,
    pub servers: Vec<ServerConfig>,
    #[serde(default = "default_theme")]
    pub theme: String,
    #[serde(default = "default_playlist_multicolour")]
    pub playlist_multicolour: bool,
    #[serde(default = "default_result_multicolour")]
    pub result_multicolour: bool,
    #[serde(default)]
    pub download_dir: Option<String>,
    #[serde(default = "default_download_overwrite")]
    pub download_overwrite: bool,
    #[serde(default = "default_advanced_status")]
    pub advanced_status: bool,
    #[serde(default = "default_queue_follow")]
    pub queue_follow: bool,
    #[serde(default)]
    pub status_colour: Option<String>,
    #[serde(default = "default_messages_visible")]
    pub messages_visible: bool,
    #[serde(default = "default_gapless_playback")]
    pub gapless_playback: bool,
    #[serde(default = "default_media_keys_enabled")]
    pub media_keys_enabled: bool,
    #[serde(default = "default_search_timeout_seconds")]
    pub search_timeout_seconds: u64,
}


pub fn is_reserved_server_alias(alias: &str) -> bool {
    matches!(alias.trim().to_lowercase().as_str(), "h" | "k" | "kill" | "cancel" | "timeout" | "server-timeout" | "all" | "s" | "al" | "ar" | "art" | "tr" | "i" | "sh" | "so" | "unsh" | "v" | "qf" | "ss" | "rs" | "msg" | "pl" | "diag" | "doc" | "gap" | "gapless" | "mk" | "media" | "media-keys" | "mediakeys")
}

pub fn reserved_server_aliases_label() -> &'static str {
    "h, k, kill, cancel, timeout, server-timeout, all, s, al, ar, art, tr, i, sh, so, unsh, v, qf, ss, rs, msg, pl, diag, doc, gap, gapless, mk, media, media-keys"
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            primary_alias: None,
            servers: Vec::new(),
            theme: default_theme(),
            playlist_multicolour: default_playlist_multicolour(),
            result_multicolour: default_result_multicolour(),
            download_dir: None,
            download_overwrite: default_download_overwrite(),
            advanced_status: default_advanced_status(),
            queue_follow: default_queue_follow(),
            status_colour: None,
            messages_visible: default_messages_visible(),
            gapless_playback: default_gapless_playback(),
            media_keys_enabled: default_media_keys_enabled(),
            search_timeout_seconds: default_search_timeout_seconds(),
        }
    }
}

impl AppConfig {
    pub fn add_or_update_server(&mut self, new_server: ServerConfig) {
        let normalized_alias = new_server.alias.trim().to_lowercase();
        let normalized_name = new_server.name.trim().to_lowercase();

        if let Some(existing) = self.servers.iter_mut().find(|server| {
            server.alias.eq_ignore_ascii_case(&normalized_alias)
                || server.name.eq_ignore_ascii_case(&normalized_name)
        }) {
            *existing = ServerConfig {
                alias: normalized_alias,
                ..new_server
            };
        } else {
            self.servers.push(ServerConfig {
                alias: normalized_alias,
                ..new_server
            });
        }

        if self.primary_alias.is_none() && !self.servers.is_empty() {
            self.primary_alias = Some(self.servers[0].alias.clone());
        }
    }

    pub fn update_server_by_target(&mut self, target: &str, updated: ServerConfig) -> Result<()> {
        let normalized_alias = updated.alias.trim().to_lowercase();
        let idx = self
            .find_server_index(target)
            .ok_or_else(|| anyhow!("Server not found: {}", target))?;

        let old_alias = self.servers[idx].alias.clone();
        self.servers[idx] = ServerConfig {
            alias: normalized_alias.clone(),
            ..updated
        };

        if self
            .primary_alias
            .as_ref()
            .map(|alias| alias.eq_ignore_ascii_case(&old_alias))
            .unwrap_or(false)
        {
            self.primary_alias = Some(normalized_alias);
        }

        Ok(())
    }

    pub fn remove_server_by_target(&mut self, target: &str) -> Result<ServerConfig> {
        let idx = self
            .find_server_index(target)
            .ok_or_else(|| anyhow!("Server not found: {}", target))?;
        let removed = self.servers.remove(idx);

        if self
            .primary_alias
            .as_ref()
            .map(|alias| alias.eq_ignore_ascii_case(&removed.alias))
            .unwrap_or(false)
        {
            self.primary_alias = self.servers.first().map(|server| server.alias.clone());
        }

        Ok(removed)
    }

    pub fn set_primary_by_target(&mut self, target: &str) -> Result<()> {
        let server = self
            .find_server(target)
            .ok_or_else(|| anyhow!("Server not found: {}", target))?;
        self.primary_alias = Some(server.alias.clone());
        Ok(())
    }

    pub fn primary_server(&self) -> Option<&ServerConfig> {
        match &self.primary_alias {
            Some(alias) => self
                .servers
                .iter()
                .find(|server| server.alias.eq_ignore_ascii_case(alias)),
            None => self.servers.first(),
        }
    }

    pub fn is_primary(&self, alias: &str) -> bool {
        self.primary_alias
            .as_ref()
            .map(|primary| primary.eq_ignore_ascii_case(alias))
            .unwrap_or(false)
    }

    pub fn primary_display_name(&self) -> String {
        self.primary_server()
            .map(|server| format!("{} [{}]", server.name, server.alias))
            .unwrap_or_else(|| "none".to_string())
    }

    pub fn find_server(&self, target: &str) -> Option<&ServerConfig> {
        let needle = target.trim();
        self.servers.iter().find(|server| {
            server.alias.eq_ignore_ascii_case(needle) || server.name.eq_ignore_ascii_case(needle)
        })
    }

    pub fn find_server_index(&self, target: &str) -> Option<usize> {
        let needle = target.trim();
        self.servers.iter().position(|server| {
            server.alias.eq_ignore_ascii_case(needle) || server.name.eq_ignore_ascii_case(needle)
        })
    }
}
