use std::{fs, io};
use std::cmp::{max, min};
use std::io::SeekFrom;
use tokio::fs::OpenOptions;
use std::path::{Path, PathBuf};
use tokio::io::{AsyncSeekExt, AsyncWriteExt};
use tokio_mpmc::{Receiver};
use anyhow::Result;
use crate::command::BroadcastCommand;
use crate::torrent::{Keys, TorrentFile};

pub struct FileManager {
    torrent_file: TorrentFile,
    download_directory: String,
    /// vector of full file paths and file lengths in bytes
    files : Vec<(PathBuf, usize)>,
    data_receiver: Receiver<(usize,Vec<u8>)>,
    broadcast_sender: tokio::sync::broadcast::Sender<BroadcastCommand>,
    downloaded_pieces: usize,
}

impl FileManager {
    pub fn new(torrent_file: TorrentFile,
               download_directory: String,
               data_receiver: Receiver<(usize,Vec<u8>)>,
               broadcast_sender: tokio::sync::broadcast::Sender<BroadcastCommand>) -> Self {

        let mut file_names = Vec::new();

        let root_path = Path::new(&download_directory).join(&torrent_file.info.name);
        match &torrent_file.info.keys{
            Keys::SingleFile { length } => {
                file_names.push((root_path.join(&torrent_file.info.name), length.clone()))
            }
            Keys::MultiFile { files } => {
                for f in files {
                    let mut full_path = PathBuf::from(&root_path);
                    for part in &f.path {
                        full_path.push(part);
                    }
                    file_names.push((full_path, f.length.clone()));
                }
            }
        }

        FileManager {
            torrent_file,
            download_directory,
            files: file_names,
            data_receiver,
            broadcast_sender,
            downloaded_pieces: 0,}
    }

    pub fn pre_allocate_files(&self) -> io::Result<()>{
        println!("Pre-allocating files");

        for file in &self.files {
            let parent = file.0.parent();
            if let Some(parent) = parent {
                fs::create_dir_all(parent)?;
            }
            let new_file = fs::File::create(&file.0)?;
            new_file.set_len(file.1 as u64)?;
        }
        Ok(())
    }

    //todo unit test this method
    pub async fn write_piece(&self ,index: usize, data: Vec<u8>){
        println!("Writing {} piece", index);
        let piece_start_offset = index * &self.torrent_file.info.plength;
        let piece_bytes_written = 0;
        let mut current_global_offset = 0;

        for file in &self.files {
            let file_start_offset = current_global_offset;
            let file_end_offset = file_start_offset + file.1;

            // Check if this piece overlaps with the current file at all
            if piece_start_offset + data.len() > file_start_offset && piece_start_offset < file_end_offset {
                let write_start = max(piece_start_offset, file_start_offset);
                let write_end = min(file_end_offset, piece_start_offset + data.len());
                let bytes_to_write = write_end - write_start;

                let source_offset = write_start - piece_start_offset;

                let file_offset = write_start - file_start_offset;

                let mut fs = OpenOptions::new().write(true).open(&file.0).await.expect(&format!("Failed to open file for writing: {:?}", file.0));

                fs.seek(SeekFrom::Start(file_offset as u64)).await.expect(&format!("Failed to seek for file: {:?}", file.0));
                fs.write_all(&data[source_offset..source_offset + bytes_to_write]).await.expect(&format!("Failed to write to file: {:?}", file.0));

            }

            current_global_offset += file.1;
            if piece_bytes_written >= data.len() { break; }
        }
    }
    pub async fn process(mut self) -> Result<()>{
        while let Ok(Some(data)) = self.data_receiver.recv().await{
            eprintln!("writing {} piece", data.0);
            self.write_piece(data.0, data.1).await;
            self.downloaded_pieces += 1;

            if self.torrent_file.info.pieces.0.len() == self.downloaded_pieces{
                eprintln!("Downloaded {} pieces", self.downloaded_pieces);
                eprintln!("shutting down");
                self.broadcast_sender.send(BroadcastCommand::Shutdown)?;
                break;
            }
        }
        Ok(())
    }
}