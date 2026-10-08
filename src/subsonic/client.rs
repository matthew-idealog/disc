use crate::config::model::ServerConfig;
use anyhow::{anyhow, Context, Result};
use rand::{distributions::Alphanumeric, Rng};
use reqwest::{Client, header};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashSet;
use std::time::Duration;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct QueueTrack {
    pub id: String,
    pub title: String,
    pub artist: String,
    pub album: String,
    pub server_alias: String,
    #[serde(default)]
    pub suffix: Option<String>,
    #[serde(default)]
    pub content_type: Option<String>,
    #[serde(default)]
    pub duration_seconds: Option<u64>,
    #[serde(default)]
    pub track_number: Option<u64>,
    #[serde(default)]
    pub year: Option<u64>,
    #[serde(default)]
    pub genre: Option<String>,
    #[serde(default)]
    pub bit_rate_kbps: Option<u64>,
}

impl QueueTrack {
    pub fn label(&self) -> String {
        format!("{} — {} • {} [{}]", self.title, self.artist, self.album, self.server_alias)
    }
}

#[derive(Clone, Debug)]
pub struct DownloadBinary {
    pub bytes: Vec<u8>,
    pub content_type: Option<String>,
    pub content_disposition: Option<String>,
}

impl DownloadBinary {
    pub fn is_zip(&self) -> bool {
        self.content_type
            .as_deref()
            .map(|content_type| content_type.to_ascii_lowercase().contains("zip"))
            .unwrap_or(false)
            || self.bytes.starts_with(b"PK\x03\x04")
            || self.bytes.starts_with(b"PK\x05\x06")
    }
}

#[derive(Clone, Debug)]
pub struct SearchResultItem {
    pub kind: ResultKind,
    pub title: String,
    pub subtitle: String,
    pub server_alias: String,
    pub playable: bool,
    pub target_id: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResultKind {
    Track,
    Album,
    Artist,
    Playlist,
    Genre,
    Section,
}

#[derive(Clone)]
pub struct SubsonicClient {
    server: ServerConfig,
    http: Client,
}

impl SubsonicClient {
    pub fn new(server: ServerConfig) -> Self {
        let timeout_seconds = server.search_timeout_seconds.clamp(5, 600);
        let http = Client::builder()
            .connect_timeout(Duration::from_secs(5))
            .timeout(Duration::from_secs(timeout_seconds))
            .build()
            .unwrap_or_else(|_| Client::new());

        Self { server, http }
    }

    pub fn alias(&self) -> &str {
        &self.server.alias
    }

    pub fn stream_url(&self, track_id: &str) -> String {
        self.authed_url(
            "/rest/stream.view",
            &[("id", track_id.to_string())],
            false,
        )
    }

    fn authed_query<'a>(&self, extra_params: &'a [(&'a str, String)], json_response: bool) -> Vec<(&'a str, String)> {
        let salt: String = rand::thread_rng()
            .sample_iter(&Alphanumeric)
            .take(8)
            .map(char::from)
            .collect();

        let token = format!("{:x}", md5::compute(format!("{}{}", self.server.password, salt)));

        let mut query: Vec<(&'a str, String)> = vec![
            ("u", self.server.username.clone()),
            ("t", token),
            ("s", salt),
            ("v", "1.16.1".to_string()),
            ("c", "disc".to_string()),
        ];
        if json_response {
            query.push(("f", "json".to_string()));
        }
        query.extend(extra_params.iter().map(|(k, v)| (*k, v.clone())));
        query
    }

    fn authed_url(&self, path: &str, extra_params: &[(&str, String)], json_response: bool) -> String {
        let url = format!("{}/{}", self.server.base_url.trim_end_matches('/'), path.trim_start_matches('/'));
        let query = self.authed_query(extra_params, json_response);
        let encoded = query
            .into_iter()
            .map(|(k, v)| format!("{}={}", k, urlencoding::encode(&v)))
            .collect::<Vec<_>>()
            .join("&");
        format!("{}?{}", url, encoded)
    }

    async fn request_json(&self, path: &str, extra_params: &[(&str, String)]) -> Result<Value> {
        let url = format!("{}/{}", self.server.base_url.trim_end_matches('/'), path.trim_start_matches('/'));
        let query = self.authed_query(extra_params, true);

        let response = self
            .http
            .get(url)
            .query(&query)
            .send()
            .await
            .with_context(|| format!("Could not reach {} [{}]", self.server.name, self.server.alias))?;

        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        if !status.is_success() {
            return Err(anyhow!(
                "Request failed for {} [{}] with HTTP {}: {}",
                self.server.name,
                self.server.alias,
                status,
                body
            ));
        }

        let value: Value = serde_json::from_str(&body)
            .with_context(|| format!("Server returned invalid JSON for {} [{}]", self.server.name, self.server.alias))?;

        let payload = value
            .get("subsonic-response")
            .cloned()
            .ok_or_else(|| anyhow!("Malformed Subsonic response from {} [{}]", self.server.name, self.server.alias))?;

        if payload.get("status").and_then(|v| v.as_str()) != Some("ok") {
            let message = payload
                .get("error")
                .and_then(|err| err.get("message"))
                .and_then(|msg| msg.as_str())
                .unwrap_or("Unknown Subsonic error");
            return Err(anyhow!("Subsonic error for {} [{}]: {}", self.server.name, self.server.alias, message));
        }

        Ok(payload)
    }


    pub async fn download_media(&self, media_id: &str) -> Result<DownloadBinary> {
        let url = format!("{}/{}", self.server.base_url.trim_end_matches('/'), "rest/download.view");
        let params = [("id", media_id.to_string())];
        let query = self.authed_query(&params, false);
        let response = self
            .http
            .get(url)
            .query(&query)
            .send()
            .await
            .with_context(|| format!("Could not reach {} [{}] for download", self.server.name, self.server.alias))?;

        let status = response.status();
        let content_type = response
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .map(|value| value.to_string());
        let content_disposition = response
            .headers()
            .get(header::CONTENT_DISPOSITION)
            .and_then(|value| value.to_str().ok())
            .map(|value| value.to_string());
        let bytes = response
            .bytes()
            .await
            .context("Could not read download response")?
            .to_vec();

        if !status.is_success() {
            let snippet = String::from_utf8_lossy(&bytes);
            return Err(anyhow!(
                "Download request failed for {} [{}] with HTTP {}: {}",
                self.server.name,
                self.server.alias,
                status,
                snippet.chars().take(300).collect::<String>()
            ));
        }

        if content_type
            .as_deref()
            .map(|value| value.to_ascii_lowercase().starts_with("text/xml"))
            .unwrap_or(false)
            || looks_like_xml_error(&bytes)
        {
            let snippet = String::from_utf8_lossy(&bytes);
            return Err(anyhow!(
                "Subsonic download error for {} [{}]: {}",
                self.server.name,
                self.server.alias,
                snippet.chars().take(300).collect::<String>()
            ));
        }

        Ok(DownloadBinary {
            bytes,
            content_type,
            content_disposition,
        })
    }

    pub async fn ping(&self) -> Result<String> {
        let _ = self.request_json("/rest/ping.view", &[]).await?;
        Ok(format!("{} [{}] responded", self.server.name, self.server.alias))
    }


    pub async fn get_starred2(&self) -> Result<Vec<SearchResultItem>> {
        let payload = self.request_json("/rest/getStarred2.view", &[]).await?;
        let starred = payload.get("starred2").cloned().unwrap_or(Value::Null);
        let mut out = Vec::new();

        for artist in starred.get("artist").map(normalize_array).unwrap_or_default() {
            out.push(SearchResultItem {
                kind: ResultKind::Artist,
                title: string_field(&artist, "name"),
                subtitle: format!("starred artist • {}", self.server.alias),
                server_alias: self.server.alias.clone(),
                playable: false,
                target_id: Some(string_field(&artist, "id")),
            });
        }

        for album in starred.get("album").map(normalize_array).unwrap_or_default() {
            out.push(SearchResultItem {
                kind: ResultKind::Album,
                title: string_field(&album, "name"),
                subtitle: format!(
                    "{} • starred album • {}",
                    fallback_string(&album, "artist", "Unknown artist"),
                    self.server.alias
                ),
                server_alias: self.server.alias.clone(),
                playable: true,
                target_id: Some(string_field(&album, "id")),
            });
        }

        for song in starred.get("song").map(normalize_array).unwrap_or_default() {
            out.push(search_song_to_item(&song, &self.server.alias));
        }

        Ok(out)
    }

    pub async fn star_track(&self, track_id: &str) -> Result<()> {
        self.request_json("/rest/star.view", &[("id", track_id.to_string())]).await?;
        Ok(())
    }

    pub async fn unstar_track(&self, track_id: &str) -> Result<()> {
        self.request_json("/rest/unstar.view", &[("id", track_id.to_string())]).await?;
        Ok(())
    }

    pub async fn star_album(&self, album_id: &str) -> Result<()> {
        self.request_json("/rest/star.view", &[("albumId", album_id.to_string())]).await?;
        Ok(())
    }

    pub async fn unstar_album(&self, album_id: &str) -> Result<()> {
        self.request_json("/rest/unstar.view", &[("albumId", album_id.to_string())]).await?;
        Ok(())
    }

    pub async fn star_artist(&self, artist_id: &str) -> Result<()> {
        self.request_json("/rest/star.view", &[("artistId", artist_id.to_string())]).await?;
        Ok(())
    }

    pub async fn unstar_artist(&self, artist_id: &str) -> Result<()> {
        self.request_json("/rest/unstar.view", &[("artistId", artist_id.to_string())]).await?;
        Ok(())
    }

    pub async fn get_recent_albums(&self, count: usize) -> Result<Vec<SearchResultItem>> {
        let payload = self
            .request_json(
                "/rest/getAlbumList2.view",
                &[
                    ("type", "newest".to_string()),
                    ("size", count.to_string()),
                ],
            )
            .await?;

        let items = payload
            .get("albumList2")
            .and_then(|v| v.get("album"))
            .map(normalize_array)
            .unwrap_or_default();

        Ok(items
            .into_iter()
            .map(|album| self.album_json_to_result(&album))
            .collect())
    }

    fn album_json_to_result(&self, album: &Value) -> SearchResultItem {
        SearchResultItem {
            kind: ResultKind::Album,
            title: string_field(album, "name"),
            subtitle: fallback_string(album, "artist", "Unknown artist"),
            server_alias: self.server.alias.clone(),
            playable: true,
            target_id: Some(string_field(album, "id")),
        }
    }

    pub async fn get_random_albums(&self, count: usize) -> Result<Vec<SearchResultItem>> {
        let payload = self
            .request_json(
                "/rest/getAlbumList2.view",
                &[
                    ("type", "random".to_string()),
                    ("size", count.to_string()),
                ],
            )
            .await?;

        let items = payload
            .get("albumList2")
            .and_then(|v| v.get("album"))
            .map(normalize_array)
            .unwrap_or_default();

        Ok(items
            .into_iter()
            .map(|album| self.album_json_to_result(&album))
            .collect())
    }

    pub async fn get_all_albums_for_search(&self, max_count: usize) -> Result<Vec<SearchResultItem>> {
        let max_count = max_count.max(1);
        let page_size = max_count.min(500);
        let mut offset = 0usize;
        let mut out = Vec::new();

        while out.len() < max_count {
            let payload = self
                .request_json(
                    "/rest/getAlbumList2.view",
                    &[
                        ("type", "alphabeticalByName".to_string()),
                        ("size", page_size.to_string()),
                        ("offset", offset.to_string()),
                    ],
                )
                .await?;

            let items = payload
                .get("albumList2")
                .and_then(|v| v.get("album"))
                .map(normalize_array)
                .unwrap_or_default();

            if items.is_empty() {
                break;
            }

            let fetched = items.len();
            out.extend(items.into_iter().map(|album| self.album_json_to_result(&album)));
            if fetched < page_size {
                break;
            }
            offset += fetched;
        }

        if out.len() > max_count {
            out.truncate(max_count);
        }
        Ok(out)
    }

    pub async fn get_random_tracks(&self, count: usize) -> Result<Vec<SearchResultItem>> {
        let payload = self
            .request_json(
                "/rest/getRandomSongs.view",
                &[("size", count.to_string())],
            )
            .await?;

        let items = payload
            .get("randomSongs")
            .and_then(|v| v.get("song"))
            .map(normalize_array)
            .unwrap_or_default();

        Ok(items
            .into_iter()
            .map(|song| search_song_to_item(&song, &self.server.alias))
            .collect())
    }

    pub async fn search_genre(&self, query: &str, count: usize) -> Result<Vec<SearchResultItem>> {
        let payload = self.request_json("/rest/getGenres.view", &[]).await?;

        let items = payload
            .get("genres")
            .and_then(|v| v.get("genre"))
            .map(normalize_array)
            .unwrap_or_default();

        let q = query.trim().to_lowercase();
        let mut out = Vec::new();
        for genre in items {
            let value = fallback_string(&genre, "value", "");
            if value.trim().is_empty() {
                continue;
            }
            if !q.is_empty() && !value.to_lowercase().contains(&q) {
                continue;
            }

            let album_count = display_field(&genre, "albumCount");
            let song_count = display_field(&genre, "songCount");
            let mut details = Vec::new();
            if !album_count.is_empty() {
                details.push(format!("{} albums", album_count));
            }
            if !song_count.is_empty() {
                details.push(format!("{} songs", song_count));
            }
            let count_text = if details.is_empty() {
                if q.is_empty() { "genre".to_string() } else { "genre match".to_string() }
            } else {
                details.join(" • ")
            };
            out.push(SearchResultItem {
                kind: ResultKind::Genre,
                title: value.clone(),
                subtitle: format!("{} • {}", count_text, self.server.alias),
                server_alias: self.server.alias.clone(),
                playable: false,
                target_id: Some(value.clone()),
            });
        }

        out.sort_by(|a, b| a.title.to_lowercase().cmp(&b.title.to_lowercase()));
        if out.len() > count {
            out.truncate(count);
        }
        Ok(out)
    }

    pub async fn get_all_artists(&self, count: usize) -> Result<Vec<SearchResultItem>> {
        let payload = self.request_json("/rest/getArtists.view", &[]).await?;
        let indexes = payload
            .get("artists")
            .and_then(|v| v.get("index"))
            .map(normalize_array)
            .unwrap_or_default();

        let mut out = Vec::new();
        for index in indexes {
            for artist in index.get("artist").map(normalize_array).unwrap_or_default() {
                let name = string_field(&artist, "name");
                if name.trim().is_empty() {
                    continue;
                }
                let album_count = display_field(&artist, "albumCount");
                let subtitle = if album_count.is_empty() {
                    format!("artist • {}", self.server.alias)
                } else {
                    format!("{} albums • artist • {}", album_count, self.server.alias)
                };
                out.push(SearchResultItem {
                    kind: ResultKind::Artist,
                    title: name,
                    subtitle,
                    server_alias: self.server.alias.clone(),
                    playable: false,
                    target_id: Some(string_field(&artist, "id")),
                });
            }
        }

        out.sort_by(|a, b| a.title.to_lowercase().cmp(&b.title.to_lowercase()));
        if out.len() > count {
            out.truncate(count);
        }
        Ok(out)
    }

    pub async fn search3(&self, query: &str, count: usize) -> Result<Vec<SearchResultItem>> {
        let payload = self
            .request_json(
                "/rest/search3.view",
                &[
                    ("query", query.to_string()),
                    ("artistCount", count.to_string()),
                    ("albumCount", count.to_string()),
                    ("songCount", count.to_string()),
                ],
            )
            .await?;

        let result = payload.get("searchResult3").cloned().unwrap_or(Value::Null);
        let mut out = Vec::new();

        for artist in result.get("artist").map(normalize_array).unwrap_or_default() {
            out.push(SearchResultItem {
                kind: ResultKind::Artist,
                title: string_field(&artist, "name"),
                subtitle: format!("artist • {}", self.server.alias),
                server_alias: self.server.alias.clone(),
                playable: false,
                target_id: Some(string_field(&artist, "id")),
            });
        }
        for album in result.get("album").map(normalize_array).unwrap_or_default() {
            out.push(SearchResultItem {
                kind: ResultKind::Album,
                title: string_field(&album, "name"),
                subtitle: format!(
                    "{} • album • {}",
                    fallback_string(&album, "artist", "Unknown artist"),
                    self.server.alias
                ),
                server_alias: self.server.alias.clone(),
                playable: true,
                target_id: Some(string_field(&album, "id")),
            });
        }
        for song in result.get("song").map(normalize_array).unwrap_or_default() {
            out.push(search_song_to_item(&song, &self.server.alias));
        }

        Ok(out)
    }

    pub async fn search_playlists(&self, query: &str, count: usize) -> Result<Vec<SearchResultItem>> {
        let payload = self.request_json("/rest/getPlaylists.view", &[]).await?;
        let items = payload
            .get("playlists")
            .and_then(|v| v.get("playlist"))
            .map(normalize_array)
            .unwrap_or_default();

        let q = query.trim().to_lowercase();
        let mut out = Vec::new();
        for playlist in items {
            let name = string_field(&playlist, "name");
            if !q.is_empty() && !name.to_lowercase().contains(&q) {
                continue;
            }
            let song_count = display_field(&playlist, "songCount");
            let count_text = if song_count.is_empty() {
                "playlist".to_string()
            } else {
                format!("{} tracks • playlist", song_count)
            };
            out.push(SearchResultItem {
                kind: ResultKind::Playlist,
                title: name,
                subtitle: format!("{} • {}", count_text, self.server.alias),
                server_alias: self.server.alias.clone(),
                playable: true,
                target_id: Some(string_field(&playlist, "id")),
            });
            if out.len() >= count {
                break;
            }
        }
        Ok(out)
    }

    pub async fn get_playlist_tracks(&self, playlist_id: &str) -> Result<Vec<SearchResultItem>> {
        let payload = self
            .request_json("/rest/getPlaylist.view", &[("id", playlist_id.to_string())])
            .await?;

        let items = payload
            .get("playlist")
            .and_then(|playlist| playlist.get("entry"))
            .map(normalize_array)
            .unwrap_or_default();

        Ok(items
            .into_iter()
            .map(|song| search_song_to_item(&song, &self.server.alias))
            .collect())
    }

    pub async fn get_playlist_queue_tracks(&self, playlist_id: &str) -> Result<Vec<QueueTrack>> {
        let payload = self
            .request_json("/rest/getPlaylist.view", &[("id", playlist_id.to_string())])
            .await?;

        let items = payload
            .get("playlist")
            .and_then(|playlist| playlist.get("entry"))
            .map(normalize_array)
            .unwrap_or_default();

        Ok(items
            .into_iter()
            .map(|song| queue_track_from_song(&song, &self.server.alias))
            .collect())
    }

    pub async fn get_album_tracks(&self, album_id: &str) -> Result<Vec<SearchResultItem>> {
        let payload = self
            .request_json("/rest/getAlbum.view", &[("id", album_id.to_string())])
            .await?;

        let album = payload.get("album").cloned().unwrap_or(Value::Null);
        let album_name = fallback_string(&album, "name", "Unknown album");
        let artist_name = fallback_string(&album, "artist", "Unknown artist");

        let items = album.get("song").map(normalize_array).unwrap_or_default();
        Ok(items
            .into_iter()
            .map(|song| SearchResultItem {
                kind: ResultKind::Track,
                title: string_field(&song, "title"),
                subtitle: format!("{} • {}", artist_name, album_name),
                server_alias: self.server.alias.clone(),
                playable: true,
                target_id: Some(string_field(&song, "id")),
            })
            .collect())
    }

    pub async fn get_album_queue_tracks(&self, album_id: &str) -> Result<Vec<QueueTrack>> {
        let payload = self
            .request_json("/rest/getAlbum.view", &[("id", album_id.to_string())])
            .await?;

        let items = payload
            .get("album")
            .and_then(|album| album.get("song"))
            .map(normalize_array)
            .unwrap_or_default();

        Ok(items
            .into_iter()
            .map(|song| queue_track_from_song(&song, &self.server.alias))
            .collect())
    }

    pub async fn get_track_by_id(&self, track_id: &str) -> Result<QueueTrack> {
        let payload = self
            .request_json("/rest/getSong.view", &[("id", track_id.to_string())])
            .await?;

        let song = payload.get("song").cloned().unwrap_or(Value::Null);
        Ok(queue_track_from_song(&song, &self.server.alias))
    }

    pub async fn get_artist_albums(&self, artist_id: &str) -> Result<Vec<SearchResultItem>> {
        let payload = self
            .request_json("/rest/getArtist.view", &[("id", artist_id.to_string())])
            .await?;

        let artist = payload.get("artist").cloned().unwrap_or(Value::Null);
        let artist_name = fallback_string(&artist, "name", "Unknown artist");
        let items = artist.get("album").map(normalize_array).unwrap_or_default();

        Ok(items
            .into_iter()
            .map(|album| SearchResultItem {
                kind: ResultKind::Album,
                title: string_field(&album, "name"),
                subtitle: format!("{} • album • {}", artist_name, self.server.alias),
                server_alias: self.server.alias.clone(),
                playable: true,
                target_id: Some(string_field(&album, "id")),
            })
            .collect())
    }
    pub async fn get_artist_queue_tracks(&self, artist_id: &str) -> Result<Vec<QueueTrack>> {
        let albums = self.get_artist_albums(artist_id).await?;
        let mut out = Vec::new();
        for album in albums {
            if let Some(id) = album.target_id {
                if id.trim().is_empty() {
                    continue;
                }
                out.extend(self.get_album_queue_tracks(&id).await?);
            }
        }
        Ok(out)
    }

    pub async fn get_genre_albums(&self, genre: &str) -> Result<Vec<SearchResultItem>> {
        let songs = self.get_songs_by_genre(genre).await?;
        let mut seen = HashSet::new();
        let mut albums = Vec::new();

        for song in songs {
            let album_name = fallback_string(&song, "album", "");
            if album_name.trim().is_empty() {
                continue;
            }
            let album_id = fallback_string(&song, "albumId", "");
            let artist_name = fallback_string(&song, "artist", "Unknown artist");
            let key = if album_id.trim().is_empty() {
                format!("name:{}|artist:{}", album_name.to_lowercase(), artist_name.to_lowercase())
            } else {
                format!("id:{}", album_id)
            };
            if !seen.insert(key) {
                continue;
            }

            albums.push(SearchResultItem {
                kind: ResultKind::Album,
                title: album_name,
                subtitle: format!("{} • genre '{}' • {}", artist_name, genre, self.server.alias),
                server_alias: self.server.alias.clone(),
                playable: true,
                target_id: if album_id.trim().is_empty() { None } else { Some(album_id) },
            });
        }

        albums.sort_by(|a, b| {
            let artist_cmp = a.subtitle.to_lowercase().cmp(&b.subtitle.to_lowercase());
            if artist_cmp == std::cmp::Ordering::Equal {
                a.title.to_lowercase().cmp(&b.title.to_lowercase())
            } else {
                artist_cmp
            }
        });

        Ok(albums)
    }

    pub async fn get_genre_queue_tracks(&self, genre: &str) -> Result<Vec<QueueTrack>> {
        let songs = self.get_songs_by_genre(genre).await?;
        Ok(songs
            .into_iter()
            .map(|song| queue_track_from_song(&song, &self.server.alias))
            .collect())
    }



    pub async fn create_playlist(&self, name: &str, song_ids: &[String]) -> Result<()> {
        let mut params: Vec<(&str, String)> = vec![("name", name.to_string())];
        for song_id in song_ids {
            params.push(("songId", song_id.clone()));
        }
        self.request_json("/rest/createPlaylist.view", &params).await?;
        Ok(())
    }

    pub async fn replace_playlist(&self, playlist_id: &str, song_ids: &[String]) -> Result<()> {
        let mut params: Vec<(&str, String)> = vec![("playlistId", playlist_id.to_string())];
        for song_id in song_ids {
            params.push(("songId", song_id.clone()));
        }
        self.request_json("/rest/createPlaylist.view", &params).await?;
        Ok(())
    }

    pub async fn append_to_playlist(&self, playlist_id: &str, song_ids: &[String]) -> Result<()> {
        let mut params: Vec<(&str, String)> = vec![("playlistId", playlist_id.to_string())];
        for song_id in song_ids {
            params.push(("songIdToAdd", song_id.clone()));
        }
        self.request_json("/rest/updatePlaylist.view", &params).await?;
        Ok(())
    }

    pub async fn rename_playlist(&self, playlist_id: &str, new_name: &str) -> Result<()> {
        self.request_json(
            "/rest/updatePlaylist.view",
            &[
                ("playlistId", playlist_id.to_string()),
                ("name", new_name.to_string()),
            ],
        )
        .await?;
        Ok(())
    }

    pub async fn delete_playlist(&self, playlist_id: &str) -> Result<()> {
        self.request_json("/rest/deletePlaylist.view", &[("id", playlist_id.to_string())]).await?;
        Ok(())
    }

    async fn get_songs_by_genre(&self, genre: &str) -> Result<Vec<Value>> {
        let mut out = Vec::new();
        let mut seen_ids = HashSet::new();
        let page_size = 500usize;
        let mut offset = 0usize;

        loop {
            let payload = self
                .request_json(
                    "/rest/getSongsByGenre.view",
                    &[
                        ("genre", genre.to_string()),
                        ("count", page_size.to_string()),
                        ("offset", offset.to_string()),
                    ],
                )
                .await?;

            let batch = payload
                .get("songsByGenre")
                .and_then(|v| v.get("song"))
                .map(normalize_array)
                .unwrap_or_default();
            let fetched = batch.len();

            for song in batch {
                let song_id = string_field(&song, "id");
                if song_id.is_empty() || seen_ids.insert(song_id) {
                    out.push(song);
                }
            }

            if fetched < page_size {
                break;
            }
            offset += fetched;
        }

        Ok(out)
    }
}

fn search_song_to_item(song: &Value, server_alias: &str) -> SearchResultItem {
    SearchResultItem {
        kind: ResultKind::Track,
        title: string_field(song, "title"),
        subtitle: format!(
            "{} • {}",
            fallback_string(song, "artist", "Unknown artist"),
            fallback_string(song, "album", "Unknown album")
        ),
        server_alias: server_alias.to_string(),
        playable: true,
        target_id: Some(string_field(song, "id")),
    }
}

fn queue_track_from_song(song: &Value, server_alias: &str) -> QueueTrack {
    QueueTrack {
        id: string_field(song, "id"),
        title: string_field(song, "title"),
        artist: fallback_string(song, "artist", "Unknown artist"),
        album: fallback_string(song, "album", "Unknown album"),
        server_alias: server_alias.to_string(),
        suffix: optional_string_field(song, "suffix"),
        content_type: optional_string_field(song, "contentType"),
        duration_seconds: optional_u64_field(song, "duration"),
        track_number: optional_u64_field(song, "track"),
        year: optional_u64_field(song, "year"),
        genre: optional_string_field(song, "genre"),
        bit_rate_kbps: optional_u64_field(song, "bitRate"),
    }
}

fn optional_string_field(value: &Value, key: &str) -> Option<String> {
    let text = string_field(value, key);
    if text.trim().is_empty() { None } else { Some(text) }
}

fn optional_u64_field(value: &Value, key: &str) -> Option<u64> {
    match value.get(key) {
        Some(Value::Number(number)) => number.as_u64(),
        Some(Value::String(text)) => text.trim().parse::<u64>().ok(),
        _ => None,
    }
}

fn normalize_array(value: &Value) -> Vec<Value> {
    match value {
        Value::Array(arr) => arr.clone(),
        Value::Null => Vec::new(),
        other => vec![other.clone()],
    }
}

fn string_field(value: &Value, key: &str) -> String {
    value.get(key)
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string()
}

fn display_field(value: &Value, key: &str) -> String {
    match value.get(key) {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Number(n)) => n.to_string(),
        Some(Value::Bool(b)) => b.to_string(),
        _ => String::new(),
    }
}

fn fallback_string(value: &Value, key: &str, fallback: &str) -> String {
    value.get(key)
        .and_then(|v| v.as_str())
        .unwrap_or(fallback)
        .to_string()
}

fn looks_like_xml_error(bytes: &[u8]) -> bool {
    let trimmed = bytes.iter().copied().skip_while(|b| (*b).is_ascii_whitespace()).take(64).collect::<Vec<_>>();
    trimmed.starts_with(b"<") && String::from_utf8_lossy(&trimmed).to_ascii_lowercase().contains("subsonic")
}
