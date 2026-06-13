use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::Instant;

pub struct DownloadProgress {
    bytes_downloaded: AtomicU64,
    pieces_completed: AtomicUsize,
    connected_peers: AtomicUsize,
    total_pieces: usize,
    start_time: Instant,
}

impl DownloadProgress {
    pub fn new(total_pieces: usize) -> Self {
        Self {
            bytes_downloaded: AtomicU64::new(0),
            pieces_completed: AtomicUsize::new(0),
            connected_peers: AtomicUsize::new(0),
            total_pieces,
            start_time: Instant::now(),
        }
    }

    pub fn connect_peer(&self) {
        self.connected_peers.fetch_add(1, Ordering::Relaxed);
    }

    pub fn disconnect_peer(&self) {
        self.connected_peers.fetch_sub(1, Ordering::Relaxed);
    }

    pub fn add_bytes(&self, bytes: u64) {
        self.bytes_downloaded.fetch_add(bytes, Ordering::Relaxed);
    }

    pub fn complete_piece(&self) {
        self.pieces_completed.fetch_add(1, Ordering::Relaxed);
    }

    pub fn display(&self) {
        let bytes = self.bytes_downloaded.load(Ordering::Relaxed);
        let pieces = self.pieces_completed.load(Ordering::Relaxed);
        let peers = self.connected_peers.load(Ordering::Relaxed);
        let elapsed = self.start_time.elapsed().as_secs_f64();
        
        let speed = if elapsed > 0.0 {
            bytes as f64 / elapsed
        } else {
            0.0
        };

        let progress_pct = if self.total_pieces > 0 {
            (pieces as f64 / self.total_pieces as f64) * 100.0
        } else {
            0.0
        };

        let speed_str = format_bytes(speed);
        let downloaded_str = format_bytes(bytes as f64);

        eprint!(
            "\r[Progress: {}/{} ({:.1}%) | Peers: {} | Speed: {}/s | Downloaded: {} | Time: {:.0}s]    ",
            pieces,
            self.total_pieces,
            progress_pct,
            peers,
            speed_str,
            downloaded_str,
            elapsed
        );
    }
}

fn format_bytes(bytes: f64) -> String {
    const KB: f64 = 1024.0;
    const MB: f64 = KB * 1024.0;
    const GB: f64 = MB * 1024.0;

    if bytes >= GB {
        format!("{:.2} GB", bytes / GB)
    } else if bytes >= MB {
        format!("{:.2} MB", bytes / MB)
    } else if bytes >= KB {
        format!("{:.2} KB", bytes / KB)
    } else {
        format!("{:.0} B", bytes)
    }
}
