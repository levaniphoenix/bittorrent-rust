use std::cmp::{max, min};
use std::io::SeekFrom;
use std::path::{Path, PathBuf};
use tokio::fs::OpenOptions;
use tokio::io::{AsyncSeekExt, AsyncWriteExt};
use tokio_mpmc::Receiver;
use anyhow::{Context, Result};
use crate::command::BroadcastCommand;
use crate::torrent::{Keys, TorrentFile};

pub struct FileManager {
    torrent_file: TorrentFile,
    files: Vec<(PathBuf, usize)>,
    data_receiver: Receiver<(usize, Vec<u8>)>,
    broadcast_sender: tokio::sync::broadcast::Sender<BroadcastCommand>,
    downloaded_pieces: usize,
}

impl FileManager {
    pub fn new(
        torrent_file: TorrentFile,
        download_directory: String,
        data_receiver: Receiver<(usize, Vec<u8>)>,
        broadcast_sender: tokio::sync::broadcast::Sender<BroadcastCommand>,
    ) -> Self {
        let mut file_names = Vec::new();

        let root_path = Path::new(&download_directory).join(&torrent_file.info.name);
        match &torrent_file.info.keys {
            Keys::SingleFile { length } => {
                file_names.push((root_path.join(&torrent_file.info.name), *length))
            }
            Keys::MultiFile { files } => {
                for f in files {
                    let mut full_path = PathBuf::from(&root_path);
                    for part in &f.path {
                        full_path.push(part);
                    }
                    file_names.push((full_path, f.length));
                }
            }
        }

        FileManager {
            torrent_file,
            files: file_names,
            data_receiver,
            broadcast_sender,
            downloaded_pieces: 0,
        }
    }

    pub fn pre_allocate_files(&self) -> std::io::Result<()> {
        eprintln!("Pre-allocating files");

        for file in &self.files {
            if let Some(parent) = file.0.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let new_file = std::fs::File::create(&file.0)?;
            new_file.set_len(file.1 as u64)?;
        }
        Ok(())
    }

    pub async fn write_piece(&self, index: usize, data: Vec<u8>) -> Result<()> {
        eprintln!(
            "Writing piece {}/{}",
            index,
            self.torrent_file.info.pieces.0.len()
        );
        let piece_start_offset = index * self.torrent_file.info.plength;
        let mut piece_bytes_written = 0;
        let mut current_global_offset = 0;

        for file in &self.files {
            let file_start_offset = current_global_offset;
            let file_end_offset = file_start_offset + file.1;

            if piece_start_offset + data.len() > file_start_offset
                && piece_start_offset < file_end_offset
            {
                let write_start = max(piece_start_offset, file_start_offset);
                let write_end = min(file_end_offset, piece_start_offset + data.len());
                let bytes_to_write = write_end - write_start;

                let source_offset = write_start - piece_start_offset;
                let file_offset = write_start - file_start_offset;

                let mut fs = OpenOptions::new()
                    .write(true)
                    .open(&file.0)
                    .await
                    .with_context(|| format!("failed to open file: {:?}", file.0))?;

                fs.seek(SeekFrom::Start(file_offset as u64))
                    .await
                    .with_context(|| format!("failed to seek in file: {:?}", file.0))?;

                fs.write_all(&data[source_offset..source_offset + bytes_to_write])
                    .await
                    .with_context(|| format!("failed to write to file: {:?}", file.0))?;

                piece_bytes_written += bytes_to_write;
            }

            current_global_offset += file.1;
            if piece_bytes_written >= data.len() {
                break;
            }
        }
        Ok(())
    }

    pub async fn process(mut self) -> Result<()> {
        while let Ok(Some(data)) = self.data_receiver.recv().await {
            self.write_piece(data.0, data.1).await?;
            self.downloaded_pieces += 1;

            if self.torrent_file.info.pieces.0.len() == self.downloaded_pieces {
                eprintln!("Downloaded all {} pieces", self.downloaded_pieces);
                eprintln!("Shutting down");
                self.broadcast_sender.send(BroadcastCommand::Shutdown)?;
                break;
            }
        }
        Ok(())
    }
}
