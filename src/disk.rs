use crate::NetworkGraph;
use bitcoin::BlockHash;
use chrono::Utc;
use lightning::routing::scoring::{ProbabilisticScorer, ProbabilisticScoringParameters};
use lightning::util::logger::{Logger, Record};
use lightning::util::ser::{ReadableArgs, Writer};
use std::fs;
use std::fs::File;
use std::io::BufReader;
use std::path::Path;
use std::sync::Arc;

pub(crate) struct FilesystemLogger {
    data_dir: String,
}
impl FilesystemLogger {
    pub(crate) fn new(data_dir: String) -> Self {
        let logs_path = format!("{data_dir}/logs");
        fs::create_dir_all(logs_path.clone()).unwrap();
        Self {
            data_dir: logs_path,
        }
    }
}
impl Logger for FilesystemLogger {
    fn log(&self, record: &Record) {
        let raw_log = record.args.to_string();
        let log = format!(
            "{} {:<5} [{}:{}] {}\n",
            // Note that a "real" lightning node almost certainly does *not* want subsecond
            // precision for message-receipt information as it makes log entries a target for
            // deanonymization attacks. For testing, however, its quite useful.
            Utc::now().format("%Y-%m-%d %H:%M:%S%.3f"),
            record.level.to_string(),
            record.module_path,
            record.line,
            raw_log
        );
        let logs_file_path = format!("{}/logs.txt", self.data_dir.clone());
        fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(logs_file_path)
            .unwrap()
            .write_all(log.as_bytes())
            .unwrap();
    }
}

pub(crate) fn read_network(
    path: &Path,
    genesis_hash: BlockHash,
    logger: Arc<FilesystemLogger>,
) -> NetworkGraph<Arc<FilesystemLogger>> {
    if let Ok(file) = File::open(path) {
        if let Ok(graph) = NetworkGraph::read(&mut BufReader::new(file), logger.clone()) {
            return graph;
        }
    }
    NetworkGraph::new(genesis_hash, logger)
}

pub(crate) fn read_scorer(
    path: &Path,
    graph: Arc<NetworkGraph<Arc<FilesystemLogger>>>,
    logger: Arc<FilesystemLogger>,
) -> ProbabilisticScorer<Arc<NetworkGraph<Arc<FilesystemLogger>>>, Arc<FilesystemLogger>> {
    let params = ProbabilisticScoringParameters::default();
    if let Ok(file) = File::open(path) {
        let args = (params.clone(), Arc::clone(&graph), Arc::clone(&logger));
        if let Ok(scorer) = ProbabilisticScorer::read(&mut BufReader::new(file), args) {
            return scorer;
        }
    }
    ProbabilisticScorer::new(params, graph, logger)
}
