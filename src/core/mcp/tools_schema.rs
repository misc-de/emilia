//! The core MCP tool descriptors advertised by `tools/list` (the hand-written
//! `json!` registry literal). Split out of [`super::tools`] so the dispatch and
//! the schema list live in files of manageable size; the order of entries is
//! the order clients see.

use serde_json::{Value, json};

/// The core tool descriptors. Schemas are kept hand-written rather than derived.
pub(super) fn tool_list_core() -> Value {
    let obj = |props: Value, required: Value| json!({ "type": "object", "properties": props, "required": required });
    let empty = || obj(json!({}), json!([]));

    json!([
        {
            "name": "now_playing",
            "description": "Return the currently playing track and playback position.",
            "inputSchema": empty(),
        },
        {
            "name": "search_library",
            "description": "Search the local library (artists, albums, songs) by substring. A numeric query also matches an album release year.",
            "inputSchema": obj(
                json!({
                    "query": { "type": "string", "description": "Search text." },
                    "limit": { "type": "integer", "description": "Max hits per group (default 20).", "minimum": 1, "maximum": 200 },
                }),
                json!(["query"]),
            ),
        },
        {
            "name": "list_artists",
            "description": "List all distinct artists in the library. By default returns an array of names. Set `with_images` to true to instead return objects `{ name, has_image }`, where `has_image` is false for artists that have no photo yet — use this to find artists whose image is missing.",
            "inputSchema": obj(
                json!({
                    "with_images": { "type": "boolean", "description": "Return `{ name, has_image }` objects instead of plain names (default false)." },
                }),
                json!([]),
            ),
        },
        {
            "name": "list_albums",
            "description": "List albums, each with its release year (the earliest tagged track year). Optionally narrow by `artist` and/or an inclusive `year_from`/`year_to` range. Set `kind` to 'single' or 'compilation' (or 'album' for regular albums) to use the album-type classification — compilations are merged per name and carry a `tracks` count. Returns at most `limit` (default 100); the response carries the full `total` and a `truncated` flag.",
            "inputSchema": obj(
                json!({
                    "artist": { "type": "string", "description": "Restrict to this exact artist (optional; ignored when `kind` is set)." },
                    "kind": { "type": "string", "enum": ["album", "single", "compilation"], "description": "Album-type classification (optional)." },
                    "year_from": { "type": "integer", "description": "Earliest release year, inclusive (optional)." },
                    "year_to": { "type": "integer", "description": "Latest release year, inclusive (optional)." },
                    "limit": { "type": "integer", "description": "Max albums returned (default 100).", "minimum": 1, "maximum": 1000 },
                }),
                json!([]),
            ),
        },
        {
            "name": "list_tracks",
            "description": "List the tracks of an album (by album name).",
            "inputSchema": obj(
                json!({ "album": { "type": "string", "description": "Album name." } }),
                json!(["album"]),
            ),
        },
        {
            "name": "list_playlists",
            "description": "List the user's playlists with their track counts.",
            "inputSchema": empty(),
        },
        {
            "name": "get_stats",
            "description": "Aggregated listening statistics over the last N days (default 30).",
            "inputSchema": obj(
                json!({ "days": { "type": "integer", "description": "Look-back window in days.", "minimum": 1 } }),
                json!([]),
            ),
        },
        {
            "name": "library_overview",
            "description": "Whole-library inventory: counts and total runtime for tracks/artists/albums, playlists, podcasts & episodes, memos, and YouTube. Durations are given as raw `*_ms` and a human-readable string.",
            "inputSchema": empty(),
        },
        {
            "name": "artist_info",
            "description": "Tallies for one artist: number of albums and songs, plus total track runtime. Counts collaborations (\"feat.\") toward the named artist.",
            "inputSchema": obj(
                json!({ "name": { "type": "string", "description": "Artist name (case-insensitive)." } }),
                json!(["name"]),
            ),
        },
        {
            "name": "album_info",
            "description": "Tallies for one album: track count, total runtime, and release year. Pass `artist` to disambiguate an album name shared by several artists.",
            "inputSchema": obj(
                json!({
                    "album": { "type": "string", "description": "Album name (case-insensitive)." },
                    "artist": { "type": "string", "description": "Restrict to this artist (optional)." },
                }),
                json!(["album"]),
            ),
        },
        {
            "name": "get_top",
            "description": "Top-played rankings from the listening history over the last N days (default 30): most-played tracks (music library only), albums, artists, genres, radio stations (ranked by time heard), podcasts (by show), or YouTube items.",
            "inputSchema": obj(
                json!({
                    "kind": { "type": "string", "enum": ["tracks", "albums", "artists", "genres", "stations", "podcasts", "youtube"] },
                    "days": { "type": "integer", "description": "Look-back window in days (default 30).", "minimum": 1 },
                    "limit": { "type": "integer", "description": "Max entries (default 10).", "minimum": 1, "maximum": 100 },
                }),
                json!(["kind"]),
            ),
        },
        {
            "name": "list_podcasts",
            "description": "List subscribed podcasts with their episode counts. Use the returned `id` with `list_episodes`.",
            "inputSchema": empty(),
        },
        {
            "name": "list_episodes",
            "description": "List a podcast's episodes (newest first) by its `podcast_id` (from `list_podcasts`). Each carries a `url` usable to play it. Returns at most `limit` (default 50) with `total`/`truncated`.",
            "inputSchema": obj(
                json!({
                    "podcast_id": { "type": "integer", "description": "Podcast id from list_podcasts." },
                    "limit": { "type": "integer", "description": "Max episodes (default 50).", "minimum": 1, "maximum": 500 },
                }),
                json!(["podcast_id"]),
            ),
        },
        {
            "name": "list_memos",
            "description": "List voice memos / recordings with their playback length.",
            "inputSchema": empty(),
        },
        {
            "name": "list_youtube",
            "description": "List recently played YouTube videos and playlists (the library's YouTube 'Recently' list), with cached runtime.",
            "inputSchema": obj(
                json!({ "limit": { "type": "integer", "description": "Max entries (default 30).", "minimum": 1, "maximum": 200 } }),
                json!([]),
            ),
        },
        {
            "name": "playback_control",
            "description": "Control transport: play, pause, toggle, next, prev. With chapters (audiobook, podcast shownotes, YouTube), next/prev first jump between chapters; only past the last (or within the first) do they change the item.",
            "inputSchema": obj(
                json!({ "action": { "type": "string", "enum": ["play", "pause", "toggle", "next", "prev"] } }),
                json!(["action"]),
            ),
        },
        {
            "name": "seek",
            "description": "Seek to an absolute position in the current track.",
            "inputSchema": obj(
                json!({ "position_ms": { "type": "integer", "description": "Absolute position in milliseconds.", "minimum": 0 } }),
                json!(["position_ms"]),
            ),
        },
        {
            "name": "play_album",
            "description": "Play a whole album in track order.",
            "inputSchema": obj(
                json!({
                    "artist": { "type": "string" },
                    "album": { "type": "string" },
                }),
                json!(["artist", "album"]),
            ),
        },
        {
            "name": "play_artist",
            "description": "Play all tracks of an artist.",
            "inputSchema": obj(
                json!({ "name": { "type": "string" } }),
                json!(["name"]),
            ),
        },
        {
            "name": "play_track",
            "description": "Play a single track by its library path.",
            "inputSchema": obj(
                json!({ "path": { "type": "string", "description": "Track path as listed by the library." } }),
                json!(["path"]),
            ),
        },
        {
            "name": "play_episode",
            "description": "Play a podcast episode by its audio URL (from list_episodes); resumes at the remembered position.",
            "inputSchema": obj(
                json!({
                    "url": { "type": "string", "description": "Episode audio URL (the `url` field from list_episodes)." },
                    "title": { "type": "string", "description": "Display title (optional)." },
                }),
                json!(["url"]),
            ),
        },
        {
            "name": "play_memo",
            "description": "Play a voice memo / recording by its file path (from list_memos).",
            "inputSchema": obj(
                json!({ "path": { "type": "string", "description": "Memo file path (the `path` field from list_memos)." } }),
                json!(["path"]),
            ),
        },
        {
            "name": "play_youtube",
            "description": "Play a YouTube video by its id or watch URL (from list_youtube). Resolves a fresh audio stream.",
            "inputSchema": obj(
                json!({
                    "id": { "type": "string", "description": "YouTube video id or watch URL." },
                    "title": { "type": "string", "description": "Display title (optional; looked up if omitted)." },
                }),
                json!(["id"]),
            ),
        },
        {
            "name": "set_sleep_timer",
            "description": "Arm the sleep timer for N minutes; 0 turns it off.",
            "inputSchema": obj(
                json!({ "minutes": { "type": "integer", "minimum": 0, "maximum": 1440 } }),
                json!(["minutes"]),
            ),
        },
        {
            "name": "create_playlist",
            "description": "Create a new (empty) playlist and return its id.",
            "inputSchema": obj(
                json!({ "name": { "type": "string" } }),
                json!(["name"]),
            ),
        },
        {
            "name": "add_to_playlist",
            "description": "Append track paths to a playlist.",
            "inputSchema": obj(
                json!({
                    "playlist_id": { "type": "integer" },
                    "paths": { "type": "array", "items": { "type": "string" } },
                }),
                json!(["playlist_id", "paths"]),
            ),
        },
        {
            "name": "toggle_favorite",
            "description": "Toggle a favorite. scope ∈ {track, folder, album, artist}; key = path | artist\\u0001album | artist name.",
            "inputSchema": obj(
                json!({
                    "scope": { "type": "string", "enum": ["track", "folder", "album", "artist"] },
                    "key": { "type": "string" },
                    "title": { "type": "string", "description": "Display name (optional)." },
                }),
                json!(["scope", "key"]),
            ),
        },
        {
            "name": "play_playlist",
            "description": "Play a playlist by its id (from list_playlists), optionally shuffled.",
            "inputSchema": obj(
                json!({
                    "playlist_id": { "type": "integer", "description": "Playlist id." },
                    "shuffle": { "type": "boolean", "description": "Shuffle playback (default false)." },
                }),
                json!(["playlist_id"]),
            ),
        },
        {
            "name": "rename_playlist",
            "description": "Rename a playlist.",
            "inputSchema": obj(
                json!({
                    "playlist_id": { "type": "integer", "description": "Playlist id." },
                    "name": { "type": "string", "description": "New name." },
                }),
                json!(["playlist_id", "name"]),
            ),
        },
        {
            "name": "delete_playlist",
            "description": "Delete a playlist. Destructive: requires `confirm: true`.",
            "inputSchema": obj(
                json!({
                    "playlist_id": { "type": "integer", "description": "Playlist id." },
                    "confirm": { "type": "boolean", "description": "Must be true to actually delete." },
                }),
                json!(["playlist_id", "confirm"]),
            ),
        },
        {
            "name": "set_playlist_cover",
            "description": "Set a playlist's cover image from a local file path.",
            "inputSchema": obj(
                json!({
                    "playlist_id": { "type": "integer", "description": "Playlist id." },
                    "path": { "type": "string", "description": "Image file path." },
                }),
                json!(["playlist_id", "path"]),
            ),
        },
        {
            "name": "enqueue",
            "description": "Append tracks (by library path) to the play-next queue, without interrupting playback.",
            "inputSchema": obj(
                json!({
                    "paths": { "type": "array", "items": { "type": "string" }, "description": "Track paths to enqueue." },
                }),
                json!(["paths"]),
            ),
        },
        {
            "name": "toggle_episode",
            "description": "Toggle a podcast episode's listened/unlistened state (by its audio URL).",
            "inputSchema": obj(
                json!({
                    "url": { "type": "string", "description": "Episode audio URL." },
                    "title": { "type": "string", "description": "Display title (optional)." },
                }),
                json!(["url"]),
            ),
        },
        {
            "name": "delete_memo",
            "description": "Delete a voice memo by id (from list_memos). Destructive: requires `confirm: true`.",
            "inputSchema": obj(
                json!({
                    "memo_id": { "type": "integer", "description": "Memo id." },
                    "confirm": { "type": "boolean", "description": "Must be true to actually delete." },
                }),
                json!(["memo_id", "confirm"]),
            ),
        },
        {
            "name": "delete_recording",
            "description": "Delete a stream recording by id. Destructive: requires `confirm: true`.",
            "inputSchema": obj(
                json!({
                    "recording_id": { "type": "integer", "description": "Recording id." },
                    "confirm": { "type": "boolean", "description": "Must be true to actually delete." },
                }),
                json!(["recording_id", "confirm"]),
            ),
        },
        {
            "name": "set_album_cover",
            "description": "Set an album's cover image from a local file path.",
            "inputSchema": obj(
                json!({
                    "artist": { "type": "string", "description": "Album artist." },
                    "album": { "type": "string", "description": "Album name." },
                    "path": { "type": "string", "description": "Image file path." },
                }),
                json!(["artist", "album", "path"]),
            ),
        },
        {
            "name": "set_artist_image",
            "description": "Set an artist's photo from a local file path.",
            "inputSchema": obj(
                json!({
                    "name": { "type": "string", "description": "Artist name." },
                    "path": { "type": "string", "description": "Image file path." },
                }),
                json!(["name", "path"]),
            ),
        },
        {
            "name": "list_artist_image_candidates",
            "description": "Find additional photo candidates for an artist on fanart.tv (matched via MusicBrainz). Returns a list of image URLs to choose from — does not download or set them. Requires a configured fanart.tv API key.",
            "inputSchema": obj(
                json!({
                    "artist": { "type": "string", "description": "Artist name." },
                    "limit": { "type": "integer", "description": "Max candidates to return (default 5, max 8).", "minimum": 1, "maximum": 8 },
                }),
                json!(["artist"]),
            ),
        },
        {
            "name": "enrich_artist_images",
            "description": "Fetch an artist's photo gallery from fanart.tv (matched via MusicBrainz) and store it as the artist's image gallery on the detail view, replacing any previously fetched gallery. Returns how many images were added. Requires a configured fanart.tv API key. Use list_artist_image_candidates first if the user should preview the photos before saving.",
            "inputSchema": obj(
                json!({
                    "artist": { "type": "string", "description": "Artist name." },
                }),
                json!(["artist"]),
            ),
        },
        {
            "name": "set_properties",
            "description": "Set the library areas an item appears in (its properties). `scope` ∈ {track, album, artist}; `key` is the track path, the artist\\u0001album key, or the artist name. `areas` is the list of areas to show it in (from: filesystem, artists, albums, singles, compilations, concerts, audiobooks; singles/compilations apply to albums); an empty list hides it.",
            "inputSchema": obj(
                json!({
                    "scope": { "type": "string", "enum": ["track", "album", "artist"] },
                    "key": { "type": "string", "description": "Item key (path | artist\\u0001album | artist name)." },
                    "areas": { "type": "array", "items": { "type": "string", "enum": ["filesystem", "artists", "albums", "singles", "compilations", "concerts", "audiobooks"] } },
                }),
                json!(["scope", "key", "areas"]),
            ),
        },
        {
            "name": "set_album_kind",
            "description": "Override an album's classification as 'single', 'compilation' or 'album' (or 'auto' to revert to the heuristic). Matches by album name (case-insensitive); affects list_albums with `kind`.",
            "inputSchema": obj(
                json!({
                    "album": { "type": "string", "description": "Album name." },
                    "kind": { "type": "string", "enum": ["album", "single", "compilation", "auto"] },
                }),
                json!(["album", "kind"]),
            ),
        },
        {
            "name": "search_youtube",
            "description": "Search YouTube online via yt-dlp for videos (default), playlists or channels. Returns id/url/title/uploader/duration — use play_youtube with an id to play one. Network call; may take a few seconds.",
            "inputSchema": obj(
                json!({
                    "query": { "type": "string", "description": "Search text." },
                    "kind": { "type": "string", "enum": ["video", "playlist", "channel"], "description": "What to search for (default video)." },
                    "limit": { "type": "integer", "description": "Max results (default 15).", "minimum": 1, "maximum": 50 },
                }),
                json!(["query"]),
            ),
        },
        {
            "name": "search_podcasts",
            "description": "Search for podcasts online (iTunes directory) by name. Returns title/author/feed_url. Network call.",
            "inputSchema": obj(
                json!({
                    "query": { "type": "string", "description": "Podcast name or keywords." },
                    "limit": { "type": "integer", "description": "Max results (default 25).", "minimum": 1, "maximum": 50 },
                }),
                json!(["query"]),
            ),
        },
        {
            "name": "download_youtube",
            "description": "Download a YouTube video (by id or watch URL) into the music library (transcode to mp3, tag, index). Long-running: returns a `job_id` immediately; poll `list_jobs` for progress.",
            "inputSchema": obj(
                json!({ "id": { "type": "string", "description": "YouTube video id or watch URL." } }),
                json!(["id"]),
            ),
        },
        {
            "name": "download_episode",
            "description": "Download a podcast episode for offline playback (by its audio URL, from list_episodes). Long-running: returns a `job_id`; poll `list_jobs`.",
            "inputSchema": obj(
                json!({ "url": { "type": "string", "description": "Episode audio URL." } }),
                json!(["url"]),
            ),
        },
        {
            "name": "list_jobs",
            "description": "List background download jobs and their state (running / done / error), newest first.",
            "inputSchema": empty(),
        },
        // --- podcasts: subscriptions ---
        {
            "name": "subscribe_podcast",
            "description": "Subscribe to a podcast by its RSS feed URL (from search_podcasts, or any feed URL): fetches the feed, stores the show and its episodes. Re-subscribing an existing feed refreshes it. Network call.",
            "inputSchema": obj(
                json!({ "feed_url": { "type": "string", "description": "RSS feed URL (the `feed_url` field from search_podcasts)." } }),
                json!(["feed_url"]),
            ),
        },
        {
            "name": "unsubscribe_podcast",
            "description": "Remove a podcast subscription and its episode list (by `podcast_id` from list_podcasts). Destructive: requires `confirm: true`.",
            "inputSchema": obj(
                json!({
                    "podcast_id": { "type": "integer", "description": "Podcast id." },
                    "confirm": { "type": "boolean", "description": "Must be true to actually remove." },
                }),
                json!(["podcast_id", "confirm"]),
            ),
        },
        {
            "name": "refresh_podcasts",
            "description": "Re-fetch every subscribed feed for new episodes (the podcasts page's refresh-all). Runs in the background; list_episodes reflects the result once done.",
            "inputSchema": empty(),
        },
        {
            "name": "delete_episode_download",
            "description": "Delete the offline download of a podcast episode (by its audio URL); the episode itself stays and can still be streamed. Destructive: requires `confirm: true`.",
            "inputSchema": obj(
                json!({
                    "url": { "type": "string", "description": "Episode audio URL (from list_episodes)." },
                    "confirm": { "type": "boolean", "description": "Must be true to actually delete." },
                }),
                json!(["url", "confirm"]),
            ),
        },
        // --- radio stations (live streams) ---
        {
            "name": "list_stations",
            "description": "List the saved internet-radio stations (live streams) with their id, stream URL, genre tags and country. Use the `id` with play_station / rename_station / delete_station.",
            "inputSchema": empty(),
        },
        {
            "name": "search_stations",
            "description": "Search the Radio Browser directory for internet-radio stations by name, genre or country. Returns name/url/tags/country/codec/bitrate — pass a hit's fields to add_station to save it. Network call.",
            "inputSchema": obj(
                json!({
                    "query": { "type": "string", "description": "Station name, genre or country." },
                    "limit": { "type": "integer", "description": "Max results (default 15).", "minimum": 1, "maximum": 50 },
                }),
                json!(["query"]),
            ),
        },
        {
            "name": "add_station",
            "description": "Save an internet-radio station (live stream) by its http(s) stream URL. `name` defaults to a name derived from the URL; the optional fields are usually copied from a search_stations hit. Saving an already-saved URL updates that station.",
            "inputSchema": obj(
                json!({
                    "url": { "type": "string", "description": "Stream URL (http/https)." },
                    "name": { "type": "string", "description": "Display name (optional)." },
                    "favicon": { "type": "string", "description": "Logo URL (optional)." },
                    "tags": { "type": "string", "description": "Comma-separated genre tags (optional)." },
                    "country": { "type": "string", "description": "Country (optional)." },
                    "codec": { "type": "string", "description": "Codec, e.g. MP3/AAC (optional)." },
                    "bitrate": { "type": "integer", "description": "Bitrate in kbit/s (optional)." },
                }),
                json!(["url"]),
            ),
        },
        {
            "name": "rename_station",
            "description": "Rename a saved station.",
            "inputSchema": obj(
                json!({
                    "station_id": { "type": "integer", "description": "Station id (from list_stations)." },
                    "name": { "type": "string", "description": "New name." },
                }),
                json!(["station_id", "name"]),
            ),
        },
        {
            "name": "delete_station",
            "description": "Remove a saved station (stops it first if it is playing). Its recordings are kept. Destructive: requires `confirm: true`.",
            "inputSchema": obj(
                json!({
                    "station_id": { "type": "integer", "description": "Station id (from list_stations)." },
                    "confirm": { "type": "boolean", "description": "Must be true to actually remove." },
                }),
                json!(["station_id", "confirm"]),
            ),
        },
        {
            "name": "play_station",
            "description": "Start a saved internet-radio station (live stream) by its id. Use playback_control to pause/resume it afterwards.",
            "inputSchema": obj(
                json!({ "station_id": { "type": "integer", "description": "Station id (from list_stations)." } }),
                json!(["station_id"]),
            ),
        },
        {
            "name": "toggle_station_recording",
            "description": "Start or stop the timeshift recording of a station (the station's record button). Needs the recording buffer enabled in the app settings; recorded songs appear in list_memos-like recordings and can be removed with delete_recording.",
            "inputSchema": obj(
                json!({ "station_id": { "type": "integer", "description": "Station id (from list_stations)." } }),
                json!(["station_id"]),
            ),
        },
        // --- library: tag editing + deletion ---
        {
            "name": "set_track_tags",
            "description": "Edit the metadata tags of a local library track in the audio file itself, then re-index it. Only the passed fields change; an empty string clears a text tag, `0` clears a numeric one. Tracks on network sources are read-only.",
            "inputSchema": obj(
                json!({
                    "path": { "type": "string", "description": "Track path (from list_tracks/search_library)." },
                    "title": { "type": "string" },
                    "artist": { "type": "string" },
                    "album": { "type": "string" },
                    "album_artist": { "type": "string" },
                    "genre": { "type": "string" },
                    "year": { "type": "integer", "minimum": 0 },
                    "track_no": { "type": "integer", "minimum": 0 },
                    "disc_no": { "type": "integer", "minimum": 0 },
                }),
                json!(["path"]),
            ),
        },
        {
            "name": "delete_track",
            "description": "Move a local library track's audio file to the desktop trash and drop it from the library. Recoverable from the trash. Tracks on network sources cannot be deleted. Destructive: requires `confirm: true`.",
            "inputSchema": obj(
                json!({
                    "path": { "type": "string", "description": "Track path (from list_tracks/search_library)." },
                    "confirm": { "type": "boolean", "description": "Must be true to actually delete." },
                }),
                json!(["path", "confirm"]),
            ),
        },
        {
            "name": "delete_album",
            "description": "Move every local track of an album (exact artist + album) to the desktop trash and drop them from the library. Tracks on network sources are skipped and reported. Destructive: requires `confirm: true`.",
            "inputSchema": obj(
                json!({
                    "artist": { "type": "string", "description": "Album artist exactly as listed." },
                    "album": { "type": "string", "description": "Album name exactly as listed." },
                    "confirm": { "type": "boolean", "description": "Must be true to actually delete." },
                }),
                json!(["artist", "album", "confirm"]),
            ),
        },
        // --- sources + folder browsing ---
        {
            "name": "list_sources",
            "description": "List the music sources shown as tabs in Files: the primary music folder plus any extra local folders and network shares (Nextcloud/WebDAV, SMB, Google Drive). Credentials are never returned. Each carries a `root` usable with list_folder.",
            "inputSchema": empty(),
        },
        {
            "name": "list_folder",
            "description": "Browse the indexed library by folder, like the Files tabs: returns the immediate subfolders (with track counts) and the tracks directly in `path`. Defaults to the primary music folder; pass a `root` from list_sources or a folder `path` from a previous call to descend.",
            "inputSchema": obj(
                json!({
                    "path": { "type": "string", "description": "Folder path or source root (optional; default: the music folder)." },
                    "limit": { "type": "integer", "description": "Max tracks returned (default 200).", "minimum": 1, "maximum": 2000 },
                }),
                json!([]),
            ),
        },
        // --- device sync ---
        {
            "name": "sync_status",
            "description": "State of the device-sync connection: whether another device is paired (and its name), whether this device is offering a pairing code, the current flow phase, a pending incoming offer (what the peer wants to send: file counts, size, library data), transfer progress, and the last error.",
            "inputSchema": empty(),
        },
        {
            "name": "sync_start_server",
            "description": "Offer a device-sync connection: starts the pairing server and shows the QR code in the app; returns the pairing code (an emilia://pair URL) that the other device scans or pastes. The code expires after about two minutes. Poll sync_status until `connected` is true.",
            "inputSchema": empty(),
        },
        {
            "name": "sync_pair",
            "description": "Connect to another device that is offering a connection, by pasting its pairing code (the emilia://pair URL from its QR / sync_start_server). Waits a few seconds for the pairing to complete.",
            "inputSchema": obj(
                json!({ "code": { "type": "string", "description": "Pairing code (emilia://pair?…)." } }),
                json!(["code"]),
            ),
        },
        {
            "name": "sync_share",
            "description": "Send content to the paired device (requires `connected` in sync_status). Select what to share: artists, albums, track paths, playlists, podcasts, stations, memos, recordings, YouTube videos, or the whole library, plus optional library data (favorites, playlists, podcasts, EQ, categories). The offer is sent without a confirmation step; the other device still has to accept it. Poll sync_status for progress.",
            "inputSchema": obj(
                json!({
                    "artists": { "type": "array", "items": { "type": "string" }, "description": "Artist names (all their albums)." },
                    "albums": { "type": "array", "items": { "type": "object", "properties": { "artist": { "type": "string" }, "album": { "type": "string" } }, "required": ["artist", "album"] } },
                    "tracks": { "type": "array", "items": { "type": "string" }, "description": "Track paths." },
                    "playlist_ids": { "type": "array", "items": { "type": "integer" } },
                    "podcast_ids": { "type": "array", "items": { "type": "integer" } },
                    "station_ids": { "type": "array", "items": { "type": "integer" } },
                    "memo_ids": { "type": "array", "items": { "type": "integer" } },
                    "recording_ids": { "type": "array", "items": { "type": "integer" } },
                    "yt_videos": { "type": "array", "items": { "type": "string" }, "description": "YouTube video ids (only delivered when the peer has YouTube enabled)." },
                    "whole_library": { "type": "boolean" },
                    "audiobooks": { "type": "boolean", "description": "All audiobooks." },
                    "concerts": { "type": "boolean", "description": "All concerts." },
                    "include_metadata": { "type": "boolean", "description": "Send the covers/photos/years and area assignments of the shared music (default true)." },
                    "include_favorites": { "type": "boolean" },
                    "include_playlists": { "type": "boolean" },
                    "include_podcasts": { "type": "boolean", "description": "All podcast subscriptions." },
                    "include_eq": { "type": "boolean" },
                    "include_categories": { "type": "boolean" },
                }),
                json!([]),
            ),
        },
        {
            "name": "sync_respond",
            "description": "Answer the pending incoming offer shown in sync_status: `accept: true` takes what the review would pre-select (new files only — nothing is overwritten — plus the offered library data), `false` rejects everything.",
            "inputSchema": obj(
                json!({ "accept": { "type": "boolean", "description": "Accept (true) or reject (false) the offer." } }),
                json!(["accept"]),
            ),
        },
        {
            "name": "sync_disconnect",
            "description": "End the live device-sync pairing (or stop offering a pairing code).",
            "inputSchema": empty(),
        },
    ])
}
