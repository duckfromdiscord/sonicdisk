use sunk::song::Song;
use sunk::{Album, Client, Streamable, ListType, search::ALL};

pub fn client(site: &str, username: &str, password: &str) -> Client {
    Client::new(site, username, password).unwrap()
}

#[derive(Clone, Debug)]
pub struct IdentifiedSong {
    pub song: String,
    pub song_id: String,
    pub streamable: Song,
    pub url: Option<String>,
}

impl PartialEq for IdentifiedSong {
    fn eq(&self, rhs: &IdentifiedSong) -> bool { 
        return self.song_id == rhs.song_id;
    }
}

impl IdentifiedSong {
    pub fn get_filename(&self) -> String {
        let extension = match self.streamable.content_type.clone().as_str() {
            "audio/x-flac" => {
                ".flac"
            },
            "audio/mpeg" => {
                ".mp3"
            },
            _ => {
                ""
            }
        };
        return self.song.clone() + extension;
    }
    pub fn get_size(&self) -> Option<u64> {
        match &self.url {
            Some(url) => {
                let httpclient = reqwest::blocking::Client::new();
                let data = httpclient.head(url.clone()).send().unwrap();
                let len = data.headers().get("content-length").unwrap().to_str().unwrap().parse().unwrap();
                Some(len)
            },
            None => {
                None
            },
        }
    }
}

#[derive(Clone, Debug)]
pub struct IdentifiedArtist {
    pub artist: String,
    pub artist_id: String,
}

impl PartialEq for IdentifiedArtist {
    fn eq(&self, rhs: &IdentifiedArtist) -> bool { 
        return self.artist_id == rhs.artist_id;
    }
}

#[derive(Clone, Debug)]
pub struct IdentifiedAlbum {
    pub album: String,
    pub album_id: String,
    pub songs: Vec<IdentifiedSong>,
}

impl PartialEq for IdentifiedAlbum {
    fn eq(&self, rhs: &IdentifiedAlbum) -> bool { 
        return self.album_id == rhs.album_id;
    }
}

fn get_albums(client: &Client) -> Result<Vec<Album>, sunk::Error> {
    let list = ListType::default();
    let all_results = Album::list(&client, list, ALL, 0);
    all_results
}

fn songvec_to_identifiedsongvec(songvec: Vec<Song>, client: Option<&Client>) -> Vec<IdentifiedSong> {
    let mut ret: Vec<IdentifiedSong> = Vec::new();
    for song in songvec {
        let url: Option<String> = match client {
            Some(client) => {
                Some(song.download_url(&client).unwrap())
            },
            None => {
                None
            },
        };
        
        ret.push(IdentifiedSong { song: song.title.clone(), song_id: song.id.to_string(), streamable: song, url, });
    }
    ret
}

fn artist_albums(client: Client) -> Vec<(IdentifiedArtist, IdentifiedAlbum)> { //Vec<(String, Vec<String>)> {
    let albums = get_albums(&client).unwrap();
    let mut pairs: Vec<(IdentifiedArtist, IdentifiedAlbum)> = Vec::new();
    for album in albums {
        let pair = (IdentifiedArtist {
            artist: album.artist.clone().unwrap(),
            artist_id: album.artist_id.clone().unwrap(),
        }, IdentifiedAlbum {
            album: album.clone().name.clone(),
            album_id: album.clone().id.clone(),
            songs: songvec_to_identifiedsongvec(album.songs(&client).unwrap(), Some(&client)),
        });
        pairs.push(pair);
    }
    pairs
}

fn desired_folders(client: Client) -> Vec<(IdentifiedArtist, Vec<IdentifiedAlbum>)> {

    let mut unique_artists: Vec<IdentifiedArtist> = Vec::new();
    let pairs = artist_albums(client);
    for pair in pairs.clone() {
        if !unique_artists.contains(&pair.0) {
            unique_artists.push(pair.0);
        }
    }

    let mut ret: Vec<(IdentifiedArtist, Vec<IdentifiedAlbum>)> = Vec::new();
    for artist in unique_artists {
        let mut albums_by_artist: Vec<IdentifiedAlbum> = Vec::new();
        for pair in pairs.clone() {
            if pair.0 == artist {
                albums_by_artist.push(pair.1);
            }
        }
        ret.push((artist, albums_by_artist));
    }

    ret
}

#[derive(Debug, Clone)]
pub struct SubsonicInfo {
    pub desired_folders: Vec<(IdentifiedArtist, Vec<IdentifiedAlbum>)>
}

impl SubsonicInfo {
    pub fn new(client: Client) -> Self {
        let desired_folders = desired_folders(client);
        Self {
            desired_folders,
        }
    }
}