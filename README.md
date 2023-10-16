# SonicDisk

SonicDisk allows you to mount a Subsonic server of your choice as a *drive* in Windows, with the power of Dokany and Rust.

I apologize for the messy code, I am building off of the Rust Dokan example and slowly learning how everything works.

Note that any write operations will never be supported by SonicDisk. You cannot write to music files on Subsonic servers, so there will be no implementation for modifying or creating files on SonicDisk.

## Goals:
- [x] **Display artists as top level folders, and display albums inside each**
- [x] **Display song files with their respective formats as songs inside album folders**
- [x] **Display song file sizes**
- [x] **Load and play song files**
- [ ] **Optional scrobbling**
- [ ] **Set a bitrate and format for all songs.** This will require some way to predict the file size as well.