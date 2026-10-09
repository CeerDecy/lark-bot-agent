//! ACP 原始消息落盘（FR-OPS-5）。
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::Path;
use std::sync::Mutex;

use agent_client_protocol::LineDirection;

pub struct AcpTrace {
    file: Mutex<File>,
}

impl AcpTrace {
    pub fn open(dir: &Path) -> std::io::Result<Self> {
        std::fs::create_dir_all(dir)?;
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let path = dir.join(format!("acp-{ts}-{}.jsonl", std::process::id()));
        let file = OpenOptions::new().create(true).append(true).open(path)?;
        Ok(Self { file: Mutex::new(file) })
    }

    pub fn record(&self, line: &str, direction: LineDirection) {
        let dir = match direction {
            LineDirection::Stdin => "stdin",
            LineDirection::Stdout => "stdout",
            LineDirection::Stderr => "stderr",
        };
        let entry = serde_json::json!({
            "ts_ms": std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0),
            "dir": dir,
            "line": line,
        });
        if let Ok(mut f) = self.file.lock() {
            let _ = writeln!(f, "{entry}");
        }
    }
}
